//! Bridge between Sonos players and qonductor devices.
//!
//! Each `SonosBridge` pairs a Sonos `Player` with a qonductor `DeviceSession`,
//! handling event translation between them in isolation.

mod command_handlers;
mod notification_handlers;
mod sonos_handlers;
mod state;

use crate::cloud_queue::QueueStore;
use crate::sonos::{GroupId, Player};
use qonductor::{DeviceSession, SessionEvent};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use state::BridgeState;

/// Configuration for cloud queue integration.
#[derive(Clone)]
pub struct CloudQueueConfig {
    /// The base URL for the cloud queue server (e.g., "http://192.168.1.100:9443")
    pub base_url: String,
}

/// A bridge between a Sonos player/group and a Qobuz Connect device session.
///
/// Each bridge owns its Player connection and DeviceSession, handling all
/// event translation in a spawned task.
pub struct SonosBridge {
    device_uuid: [u8; 16],
    task: JoinHandle<()>,
}

impl SonosBridge {
    /// Start a new bridge for a Sonos group.
    ///
    /// This will:
    /// 1. Connect to the Sonos player
    /// 2. Subscribe to playback/volume events
    /// 3. Spawn an event loop task
    pub async fn start(
        player: Player,
        group_id: GroupId,
        device_uuid: [u8; 16],
        session: DeviceSession,
        app_id: String,
        queue_store: QueueStore,
        cloud_queue_config: CloudQueueConfig,
    ) -> Result<Self, sonos_websocket::Error> {
        let name = player.name().to_string();

        // Generate a unique bridge ID from the device UUID
        let bridge_id = format!(
            "{:02x}{:02x}{:02x}{:02x}",
            device_uuid[0], device_uuid[1], device_uuid[2], device_uuid[3]
        );

        // Connect to Sonos player
        player.connect().await?;
        info!(
            "[{}] Connected to Sonos player (bridge_id={})",
            name, bridge_id
        );

        // Subscribe to Sonos events (for bidirectional sync)
        if let Err(e) = player.subscribe_playback(&group_id).await {
            warn!("[{}] Failed to subscribe to playback: {}", name, e);
        }
        if let Err(e) = player.subscribe_volume(&group_id).await {
            warn!("[{}] Failed to subscribe to volume: {}", name, e);
        }
        if let Err(e) = player.subscribe_metadata(&group_id).await {
            warn!("[{}] Failed to subscribe to metadata: {}", name, e);
        }

        // Get Sonos player events before moving player into state
        let mut player_events = player.events();

        // Build state bundle
        let mut state = BridgeState {
            name,
            bridge_id,
            app_id,
            player,
            group_id,
            session,
            queue_store,
            cloud_queue_config,
            service_account_number: None,
            session_id: None,
            qobuz_client: None,
            current_queue_item_id: None,
            current_track_id: None,
            current_duration_ms: None,
            pending_initial_item: None,
            commanded: None,
            restored_state: None,
            restored_position: None,
        };

        // Spawn event loop task
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    Ok(player_event) = player_events.recv() => {
                        state.handle_player_event(player_event).await;
                    }
                    Some(event) = state.session.recv() => {
                        match event {
                            SessionEvent::Command(cmd) => {
                                state.handle_command(cmd).await;
                            }
                            SessionEvent::Notification(n) => {
                                state.handle_notification(n).await;
                            }
                        }
                    }
                    else => break,
                }
            }

            info!("[{}] Session ended", state.name);
        });

        Ok(Self {
            device_uuid,
            task,
        })
    }

    /// Get the device UUID for this bridge.
    pub fn device_uuid(&self) -> [u8; 16] {
        self.device_uuid
    }

    /// Shutdown the bridge, aborting its task.
    pub fn shutdown(self) {
        self.task.abort();
    }
}
