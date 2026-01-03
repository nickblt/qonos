//! Bridge between Sonos players and qonductor devices.
//!
//! Each `SonosBridge` pairs a Sonos `Player` with a qonductor `DeviceSession`,
//! handling event translation between them in isolation.

use crate::qobuz_api;
use crate::sonos::{GroupId, PlayState, Player};
use qonductor::{
    ActivationState, BufferState, CommandEvent, DeviceEvent, DeviceSession, PlaybackResponse,
    PlayingState, SessionEvent, SystemEvent,
};
use tokio::task::JoinHandle;
use tracing::{info, warn};

/// A bridge between a Sonos player/group and a Qobuz Connect device session.
///
/// Each bridge owns its Player connection and DeviceSession, handling all
/// event translation in a spawned task.
pub struct SonosBridge {
    group_id: GroupId,
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
    ) -> Result<Self, sonos_websocket::Error> {
        let name = player.name().to_string();

        // Connect to Sonos player
        player.connect().await?;
        info!("[{}] Connected to Sonos player", name);

        // Subscribe to Sonos events (for future bidirectional sync)
        if let Err(e) = player.subscribe_playback(&group_id).await {
            warn!("[{}] Failed to subscribe to playback: {}", name, e);
        }
        if let Err(e) = player.subscribe_volume(&group_id).await {
            warn!("[{}] Failed to subscribe to volume: {}", name, e);
        }

        // Spawn event loop task
        let task = tokio::spawn(Self::run_event_loop(
            name,
            player,
            group_id.clone(),
            session,
            app_id,
        ));

        Ok(Self {
            group_id,
            device_uuid,
            task,
        })
    }

    /// Run the event loop, translating between Qobuz and Sonos.
    async fn run_event_loop(
        name: String,
        player: Player,
        group_id: GroupId,
        mut session: DeviceSession,
        app_id: String,
    ) {
        // Track the Sonos service account number for this user (used for queue URIs)
        let mut _service_account_number: Option<u32> = None;

        // TODO: Add Sonos → Qobuz sync via select! with player.events()

        while let Some(event) = session.recv().await {
            match event {
                // === Commands (require response) ===
                SessionEvent::Command(cmd) => match cmd {
                    CommandEvent::PlaybackCommand {
                        renderer_id,
                        cmd,
                        respond,
                    } => {
                        info!(
                            "[{}] Playback command: renderer={} state={:?} position={:?}",
                            name, renderer_id, cmd.state, cmd.position_ms
                        );

                        // Forward state changes to Sonos
                        // TODO: Re-enable once queue handling is implemented
                        // if let Some(state) = cmd.state {
                        //     let result = match state {
                        //         PlayingState::Playing => player.play(&group_id).await,
                        //         PlayingState::Paused => player.pause(&group_id).await,
                        //         PlayingState::Stopped => player.stop(&group_id).await,
                        //         PlayingState::Unknown => Ok(()),
                        //     };
                        //     if let Err(e) = result {
                        //         warn!("[{}] Failed to send playback command: {}", name, e);
                        //     }
                        // }
                        if let Some(state) = cmd.state {
                            info!("[{}] Playback command ignored (disabled): {:?}", name, state);
                        }

                        // TODO: Handle seek (position_ms)
                        if cmd.position_ms.is_some() {
                            warn!("[{}] Seek not yet implemented", name);
                        }

                        // Get current state from Sonos for response
                        let (sonos_state, position) =
                            match player.get_playback_status(&group_id).await {
                                Ok(status) => {
                                    let state = match status.state {
                                        PlayState::Playing => PlayingState::Playing,
                                        PlayState::Paused => PlayingState::Paused,
                                        PlayState::Idle | PlayState::Buffering => {
                                            PlayingState::Stopped
                                        }
                                    };
                                    (state, status.position_millis.unwrap_or(0) as u32)
                                }
                                Err(e) => {
                                    warn!("[{}] Failed to get playback status: {}", name, e);
                                    (cmd.state.unwrap_or(PlayingState::Stopped), 0)
                                }
                            };

                        respond.send(PlaybackResponse {
                            state: sonos_state,
                            buffer_state: BufferState::Ok,
                            position_ms: position,
                            duration_ms: None,
                            queue_item_id: None,
                            next_queue_item_id: None,
                        });
                    }

                    CommandEvent::Activate { renderer_id, respond } => {
                        info!("[{}] Device activated: renderer_id={}", name, renderer_id);

                        // Get initial state from Sonos
                        let (volume, muted) = match player.get_volume(&group_id).await {
                            Ok(vol) => (vol.volume as u32, vol.muted),
                            Err(e) => {
                                warn!("[{}] Failed to get volume: {}", name, e);
                                (50, false)
                            }
                        };

                        let (state, position) = match player.get_playback_status(&group_id).await {
                            Ok(status) => {
                                let state = match status.state {
                                    PlayState::Playing => PlayingState::Playing,
                                    PlayState::Paused => PlayingState::Paused,
                                    PlayState::Idle | PlayState::Buffering => PlayingState::Stopped,
                                };
                                (state, status.position_millis.unwrap_or(0) as u32)
                            }
                            Err(e) => {
                                warn!("[{}] Failed to get playback status: {}", name, e);
                                (PlayingState::Stopped, 0)
                            }
                        };

                        respond.send(ActivationState {
                            muted,
                            volume,
                            max_quality: 4, // HiRes 192kHz
                            playback: PlaybackResponse {
                                state,
                                buffer_state: BufferState::Ok,
                                position_ms: position,
                                duration_ms: None,
                                queue_item_id: None,
                                next_queue_item_id: None,
                            },
                        });
                    }

                    CommandEvent::Heartbeat { respond, .. } => {
                        respond.send(None);
                    }
                },

                // === Device events (no response needed) ===
                SessionEvent::Device(dev) => match dev {
                    DeviceEvent::Deactivated { renderer_id } => {
                        info!("[{}] Device deactivated: renderer_id={}", name, renderer_id);
                    }

                    DeviceEvent::QueueUpdated { tracks, version } => {
                        info!(
                            "[{}] Queue updated: {} tracks (version {}.{})",
                            name,
                            tracks.len(),
                            version.0,
                            version.1
                        );
                        // TODO: Sync queue to Sonos
                    }

                    DeviceEvent::LoopModeChanged { mode } => {
                        info!("[{}] Loop mode changed: {:?}", name, mode);
                        // TODO: Sync to Sonos play modes
                    }

                    DeviceEvent::ShuffleModeChanged { enabled } => {
                        info!("[{}] Shuffle mode changed: {}", name, enabled);
                        // TODO: Sync to Sonos play modes
                    }

                    DeviceEvent::RestoreState {
                        position_ms,
                        queue_index,
                    } => {
                        info!(
                            "[{}] Restore state: position={}ms queue_idx={:?}",
                            name, position_ms, queue_index
                        );
                        // TODO: Restore playback position on Sonos
                    }
                },

                // === System events ===
                SessionEvent::System(sys) => match sys {
                    SystemEvent::Connected => {
                        info!("[{}] Connected to Qobuz", name);
                    }

                    SystemEvent::Disconnected { reason, .. } => {
                        warn!("[{}] Disconnected from Qobuz: {:?}", name, reason);
                    }

                    SystemEvent::DeviceRegistered {
                        device_uuid,
                        renderer_id,
                        api_jwt,
                    } => {
                        info!(
                            "[{}] Device registered: {:02x?} -> renderer {}",
                            name, &device_uuid[..4], renderer_id
                        );

                        // Get user's public_id from Qobuz REST API
                        match qobuz_api::get_user(&api_jwt, &app_id).await {
                            Ok(user) => {
                                info!(
                                    "[{}] Qobuz user: {} ({})",
                                    name,
                                    user.display_name.as_deref().unwrap_or("unknown"),
                                    user.public_id
                                );

                                // Match with Sonos music service account
                                match player
                                    .match_music_service_account(&user.public_id, "31", "Qobuz")
                                    .await
                                {
                                    Ok(account) => {
                                        let sn = account.service_account_number();
                                        info!(
                                            "[{}] Matched Sonos account: sn={:?}",
                                            name, sn
                                        );
                                        _service_account_number = sn;
                                    }
                                    Err(e) => {
                                        warn!(
                                            "[{}] Failed to match Sonos account: {}",
                                            name, e
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("[{}] Failed to get Qobuz user: {}", name, e);
                            }
                        }
                    }

                    SystemEvent::SessionClosed { .. } => {
                        info!("[{}] Session closed", name);
                    }
                },

                // === Broadcasts (from other renderers) ===
                SessionEvent::Broadcast(_) => {
                    // Broadcasts from other renderers are not handled in the bridge
                }
            }
        }

        info!("[{}] Session ended", name);
    }

    /// Get the device UUID for this bridge.
    pub fn device_uuid(&self) -> [u8; 16] {
        self.device_uuid
    }

    /// Get the group ID for this bridge.
    pub fn group_id(&self) -> &GroupId {
        &self.group_id
    }

    /// Shutdown the bridge, aborting its task.
    pub fn shutdown(self) {
        self.task.abort();
    }
}
