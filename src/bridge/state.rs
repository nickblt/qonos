//! Bridge state and shared helpers.
//!
//! Contains the mutable state bundle for a single bridge and helper methods
//! used across multiple event handlers.

use crate::cloud_queue::QueueStore;
use crate::qobuz_api::QobuzClient;
use crate::sonos::{GroupId, PlayState, Player};
use qonductor::{
    msg::{Position, PositionExt, QueueRendererState},
    BufferState, DeviceSession, PlayingState,
};
use sonos_websocket::{
    Artist, CreateSessionRequest, LoadCloudQueueRequest, MusicObjectId, Track,
};
use tokio::time::Instant;
use tracing::{debug, info, warn};

use super::CloudQueueConfig;

/// Safety timeout for commanded state override.
const COMMANDED_STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Minimum age before we'll clear a commanded override on state match.
/// Prevents stale events from the old track clearing the override prematurely.
const COMMANDED_MIN_AGE: std::time::Duration = std::time::Duration::from_millis(500);

/// Tracks what we commanded Sonos to do, so we can report the target state
/// to Qobuz until Sonos confirms it caught up. This prevents transient states
/// (Idle, Buffering) from causing UI flicker.
pub(crate) struct CommandedState {
    pub state: PlayingState,
    pub position_ms: Option<u32>,
    set_at: Instant,
}

impl CommandedState {
    pub fn new(state: PlayingState, position_ms: Option<u32>) -> Self {
        Self {
            state,
            position_ms,
            set_at: Instant::now(),
        }
    }

    /// Whether this commanded state has expired.
    pub fn is_expired(&self) -> bool {
        self.set_at.elapsed() > COMMANDED_STATE_TIMEOUT
    }

    /// Whether this override is old enough that a matching state is a real
    /// confirmation, not a stale event from before the command.
    pub fn is_confirmable(&self) -> bool {
        self.set_at.elapsed() >= COMMANDED_MIN_AGE
    }
}

/// All mutable state needed by the bridge event loop.
pub(crate) struct BridgeState {
    // Identity
    pub name: String,
    pub bridge_id: String,
    pub app_id: String,

    // Sonos
    pub player: Player,
    pub group_id: GroupId,

    // Qobuz session
    pub session: DeviceSession,

    // Cloud queue
    pub queue_store: QueueStore,
    pub cloud_queue_config: CloudQueueConfig,

    // Mutable state
    pub service_account_number: Option<u32>,
    pub session_id: Option<String>,
    pub qobuz_client: Option<QobuzClient>,
    pub current_queue_item_id: Option<i32>,
    pub current_track_id: Option<u64>,
    pub current_duration_ms: Option<u32>,
    pub pending_initial_item: Option<(u64, u32)>,
    pub commanded: Option<CommandedState>,
    pub restored_state: Option<PlayingState>,
    pub restored_position: Option<u32>,
}

impl BridgeState {
    /// Resolve the reported state, applying commanded state override logic.
    ///
    /// If we recently commanded Sonos to a state and it hasn't caught up yet,
    /// report the commanded state instead of the actual Sonos state.
    /// Returns the state to report to Qobuz.
    pub fn resolve_commanded_state(&mut self, sonos_state: PlayingState) -> PlayingState {
        match &self.commanded {
            Some(cmd) if cmd.state == sonos_state && cmd.is_confirmable() => {
                // Sonos caught up to commanded state — clear override
                debug!(
                    "[{}] Sonos confirmed commanded {:?}, clearing override",
                    self.name, sonos_state
                );
                self.commanded = None;
                sonos_state
            }
            Some(cmd) if !cmd.is_expired() => {
                // Sonos hasn't caught up yet — report what we commanded
                debug!(
                    "[{}] Overriding Sonos {:?} with commanded {:?}",
                    self.name, sonos_state, cmd.state
                );
                cmd.state
            }
            Some(cmd) => {
                // Timed out — clear and report Sonos state
                debug!(
                    "[{}] Commanded {:?} timed out, reporting Sonos {:?}",
                    self.name, cmd.state, sonos_state
                );
                self.commanded = None;
                sonos_state
            }
            None => sonos_state,
        }
    }

    /// Get the report position, applying commanded override if present.
    pub fn resolve_report_position(&self, fresh_position: u32) -> u32 {
        self.commanded
            .as_ref()
            .and_then(|c| c.position_ms)
            .unwrap_or(fresh_position)
    }

    /// Fetch track metadata from the Qobuz API and build a Sonos Track.
    pub async fn fetch_track_metadata(&self, track_id: u64, sn: u32) -> Option<Track> {
        let client = self.qobuz_client.as_ref()?;
        match client.get_tracks(&[track_id]).await {
            Ok(metadata) if !metadata.is_empty() => {
                let m = &metadata[0];
                debug!(
                    "[{}] Fetched track metadata: {} - {}",
                    self.name, m.artist_name, m.title
                );
                Some(Track {
                    name: Some(m.title.clone()),
                    artist: Some(Artist {
                        name: m.artist_name.clone(),
                        id: None,
                    }),
                    album: Some(sonos_websocket::Album {
                        name: m.album_title.clone(),
                        artist: None,
                        id: None,
                    }),
                    duration_millis: Some((m.duration_secs * 1000) as i32),
                    image_url: m.album_image_url.clone(),
                    id: Some(MusicObjectId {
                        object_id: format!("track:{}", track_id),
                        service_id: Some("31".to_string()),
                        account_id: Some(format!("sn_{}", sn)),
                    }),
                    ..Default::default()
                })
            }
            Ok(_) => {
                debug!("[{}] No metadata returned for track {}", self.name, track_id);
                None
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to fetch track metadata: {}",
                    self.name, e
                );
                None
            }
        }
    }

    /// Load the cloud queue on Sonos, creating a session if needed.
    ///
    /// This handles the common pattern of creating a playback session and loading
    /// the cloud queue at a specific item/position.
    pub async fn load_cloud_queue_at(
        &mut self,
        queue_item_id: u64,
        track_id: u64,
        position_ms: u32,
        sn: u32,
    ) {
        let start_item_id = queue_item_id.to_string();

        // Fetch metadata for the target track
        let track_metadata = self.fetch_track_metadata(track_id, sn).await;

        if let Some(sid) = &self.session_id {
            // Active session — reload cloud queue at the target track
            let queue_url = format!(
                "{}/queues/{}/v2.3/",
                self.cloud_queue_config.base_url, self.bridge_id
            );

            match self
                .player
                .load_cloud_queue(
                    sid,
                    LoadCloudQueueRequest {
                        queue_base_url: queue_url,
                        item_id: Some(start_item_id.clone()),
                        play_on_completion: Some(true),
                        track_metadata,
                        position_millis: if position_ms > 0 {
                            Some(position_ms as i32)
                        } else {
                            None
                        },
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(()) => {
                    info!(
                        "[{}] Loaded cloud queue at item {} (track {}, position {}ms)",
                        self.name, start_item_id, track_id, position_ms
                    );
                    self.commanded =
                        Some(CommandedState::new(PlayingState::Playing, None));
                }
                Err(e) => {
                    warn!("[{}] Failed to load cloud queue: {}", self.name, e);
                }
            }
        } else {
            // No session yet — need to create one first
            info!(
                "[{}] Creating playback session and loading cloud queue",
                self.name
            );

            match self
                .player
                .create_playback_session(
                    &self.group_id,
                    CreateSessionRequest {
                        app_id: self.app_id.clone(),
                        app_context: self.bridge_id.clone(),
                        account_id: self
                            .service_account_number
                            .map(|sn| format!("sn_{}", sn)),
                        ..Default::default()
                    },
                )
                .await
            {
                Ok(playback_session) => {
                    info!(
                        "[{}] Created playback session: {}",
                        self.name, playback_session.session_id
                    );

                    let queue_url = format!(
                        "{}/queues/{}/v2.3/",
                        self.cloud_queue_config.base_url, self.bridge_id
                    );

                    match self
                        .player
                        .load_cloud_queue(
                            &playback_session.session_id,
                            LoadCloudQueueRequest {
                                queue_base_url: queue_url.clone(),
                                item_id: Some(start_item_id.clone()),
                                play_on_completion: Some(true),
                                track_metadata,
                                position_millis: if position_ms > 0 {
                                    Some(position_ms as i32)
                                } else {
                                    None
                                },
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        Ok(()) => {
                            info!(
                                "[{}] Loaded cloud queue: {} (starting at item {}, position {}ms)",
                                self.name, queue_url, start_item_id, position_ms
                            );
                            self.session_id = Some(playback_session.session_id);
                        }
                        Err(e) => {
                            warn!("[{}] Failed to load cloud queue: {}", self.name, e);
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "[{}] Failed to create playback session: {}",
                        self.name, e
                    );
                }
            }
        }
    }
}

// ============================================================================
// Free helper functions
// ============================================================================

/// Build a QueueRendererState from playback parameters.
pub(crate) fn build_renderer_state(
    state: PlayingState,
    buffer_state: BufferState,
    position_ms: u32,
    duration_ms: Option<u32>,
    queue_item_id: Option<i32>,
    next_queue_item_id: Option<i32>,
) -> QueueRendererState {
    QueueRendererState {
        playing_state: Some(state.into()),
        buffer_state: Some(buffer_state.into()),
        current_position: Some(Position::now(position_ms)),
        duration: duration_ms,
        current_queue_item_id: queue_item_id,
        next_queue_item_id,
        ..Default::default()
    }
}

/// Extract track_id from Sonos MusicObjectId (e.g., "track:363336060" -> 363336060)
pub(crate) fn extract_track_id(object_id: &str) -> Option<u64> {
    object_id.strip_prefix("track:").and_then(|s| s.parse().ok())
}

/// Look up queue_item_id from QueueState by track_id.
pub(crate) async fn find_queue_item_id(
    queue_store: &QueueStore,
    bridge_id: &str,
    track_id: u64,
) -> Option<i32> {
    let state = queue_store.get(bridge_id).await?;
    state
        .items
        .iter()
        .find(|item| item.track_id == track_id)
        .and_then(|item| item.id.parse::<i32>().ok())
}

/// Map Sonos PlayState to Qobuz PlayingState.
pub(crate) fn map_sonos_state(state: PlayState) -> PlayingState {
    match state {
        PlayState::Playing => PlayingState::Playing,
        PlayState::Paused => PlayingState::Paused,
        PlayState::Idle | PlayState::Buffering => PlayingState::Stopped,
    }
}
