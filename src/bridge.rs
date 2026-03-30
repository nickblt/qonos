//! Bridge between Sonos players and qonductor devices.
//!
//! Each `SonosBridge` pairs a Sonos `Player` with a qonductor `DeviceSession`,
//! handling event translation between them in isolation.

use crate::cloud_queue::{cloud_queue_item, QueueState, QueueStore};
use std::collections::HashSet;
use crate::qobuz_api::QobuzClient;
use crate::sonos::{GroupId, PlayState, Player, PlayerEvent};
use qonductor::{
    msg::{Position, PositionExt, QueueRendererState, SetStateExt},
    ActivationState, BufferState, Command, DeviceSession, Notification, PlayingState, SessionEvent,
};
use sonos_websocket::{
    Artist, CreateSessionRequest, LoadCloudQueueRequest, MusicObjectId, Track,
};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, error, info, warn};

// ============================================================================
// Helper Functions
// ============================================================================

/// Build a QueueRendererState from playback parameters.
fn build_renderer_state(
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
fn extract_track_id(object_id: &str) -> Option<u64> {
    object_id.strip_prefix("track:").and_then(|s| s.parse().ok())
}

/// Look up queue_item_id from QueueState by track_id
async fn find_queue_item_id(queue_store: &QueueStore, bridge_id: &str, track_id: u64) -> Option<i32> {
    let state = queue_store.get(bridge_id).await?;
    state
        .items
        .iter()
        .find(|item| item.track_id == track_id)
        .and_then(|item| item.id.parse::<i32>().ok())
}

/// Map Sonos PlayState to Qobuz PlayingState.
fn map_sonos_state(state: PlayState) -> PlayingState {
    match state {
        PlayState::Playing => PlayingState::Playing,
        PlayState::Paused => PlayingState::Paused,
        PlayState::Idle | PlayState::Buffering => PlayingState::Stopped,
    }
}

/// Safety timeout for commanded state override.
const COMMANDED_STATE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Minimum age before we'll clear a commanded override on state match.
/// Prevents stale events from the old track clearing the override prematurely.
const COMMANDED_MIN_AGE: std::time::Duration = std::time::Duration::from_millis(500);

/// Tracks what we commanded Sonos to do, so we can report the target state
/// to Qobuz until Sonos confirms it caught up. This prevents transient states
/// (Idle, Buffering) from causing UI flicker.
struct CommandedState {
    state: PlayingState,
    position_ms: Option<u32>,
    set_at: Instant,
}

impl CommandedState {
    fn new(state: PlayingState, position_ms: Option<u32>) -> Self {
        Self {
            state,
            position_ms,
            set_at: Instant::now(),
        }
    }

    /// Whether this commanded state has expired.
    fn is_expired(&self) -> bool {
        self.set_at.elapsed() > COMMANDED_STATE_TIMEOUT
    }

    /// Whether this override is old enough that a matching state is a real
    /// confirmation, not a stale event from before the command.
    fn is_confirmable(&self) -> bool {
        self.set_at.elapsed() >= COMMANDED_MIN_AGE
    }
}

// ============================================================================
// Bridge Types
// ============================================================================

/// Configuration for cloud queue integration.
#[derive(Clone)]
pub struct CloudQueueConfig {
    /// The base URL for the cloud queue server (e.g., "http://192.168.1.100:9443")
    pub base_url: String,
}

/// All context needed by the event loop, bundled to avoid too many arguments.
struct EventLoopContext {
    name: String,
    player: Player,
    group_id: GroupId,
    bridge_id: String,
    session: DeviceSession,
    app_id: String,
    queue_store: QueueStore,
    cloud_queue_config: CloudQueueConfig,
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

        // Spawn event loop task
        let ctx = EventLoopContext {
            name,
            player,
            group_id: group_id.clone(),
            bridge_id: bridge_id.clone(),
            session,
            app_id,
            queue_store,
            cloud_queue_config,
        };
        let task = tokio::spawn(Self::run_event_loop(ctx));

        Ok(Self {
            device_uuid,
            task,
        })
    }

    /// Run the event loop, translating between Qobuz and Sonos.
    async fn run_event_loop(ctx: EventLoopContext) {
        let EventLoopContext {
            name,
            player,
            group_id,
            bridge_id,
            mut session,
            app_id,
            queue_store,
            cloud_queue_config,
        } = ctx;
        // Track the Sonos service account number for this user (used for queue URIs)
        let mut service_account_number: Option<u32> = None;

        // Track the Sonos playback session ID
        let mut session_id: Option<String> = None;

        // Qobuz API client (created after DeviceRegistered provides JWT)
        let mut qobuz_client: Option<QobuzClient> = None;

        // State for bidirectional sync - track current playback for Qobuz reporting
        let mut current_queue_item_id: Option<i32> = None;
        let mut current_track_id: Option<u64> = None;
        let mut current_duration_ms: Option<u32> = None;

        // Pending initial state from SetState (before queue is loaded)
        // Stores (queue_item_id, position_ms) to use when loading cloud queue
        let mut pending_initial_item: Option<(u64, u32)> = None;

        // After commanding Sonos, trust what we commanded until Sonos confirms
        // it caught up. Prevents transient states from causing UI flicker.
        let mut commanded: Option<CommandedState> = None;

        // State from previous renderer (received via RestoreState before SetActive)
        let mut restored_state: Option<PlayingState> = None;
        let mut restored_position: Option<u32> = None;

        // Get Sonos player events for bidirectional sync
        let mut player_events = player.events();

        loop {
            tokio::select! {
                // Handle Sonos player events (bidirectional sync to Qobuz)
                Ok(player_event) = player_events.recv() => {
                    match player_event {
                        PlayerEvent::PlaybackError(err) => {
                            error!(
                                "[{}] Sonos playback error: {} (item={:?}, http_status={:?}, reason={:?})",
                                name, err.error_code, err.item_id, err.http_status, err.reason
                            );
                        }

                        PlayerEvent::PlaybackChanged(status) => {
                            let sonos_state = map_sonos_state(status.state);

                            // Query fresh position from Sonos (event position may be stale)
                            let fresh_position = match player.get_playback_status(&group_id).await {
                                Ok(fresh) => fresh.position_millis.unwrap_or(0) as u32,
                                Err(_) => status.position_millis.unwrap_or(0) as u32,
                            };

                            // Determine reported state using commanded override
                            let report_state = match &commanded {
                                Some(cmd) if cmd.state == sonos_state && cmd.is_confirmable() => {
                                    // Sonos caught up to commanded state — clear override
                                    debug!(
                                        "[{}] Sonos confirmed commanded {:?}, clearing override",
                                        name, sonos_state
                                    );
                                    commanded = None;
                                    sonos_state
                                }
                                Some(cmd) if !cmd.is_expired() => {
                                    // Sonos hasn't caught up yet — report what we commanded
                                    debug!(
                                        "[{}] Overriding Sonos {:?} with commanded {:?}",
                                        name, sonos_state, cmd.state
                                    );
                                    cmd.state
                                }
                                Some(cmd) => {
                                    // Timed out — clear and report Sonos state
                                    debug!(
                                        "[{}] Commanded {:?} timed out, reporting Sonos {:?}",
                                        name, cmd.state, sonos_state
                                    );
                                    commanded = None;
                                    sonos_state
                                }
                                None => sonos_state,
                            };

                            let report_position = commanded
                                .as_ref()
                                .and_then(|c| c.position_ms)
                                .unwrap_or(fresh_position);

                            debug!(
                                "[{}] Sonos playback changed: {:?} -> reporting {:?} @ {}ms",
                                name, status.state, report_state, report_position
                            );

                            let buffer = match status.state {
                                PlayState::Buffering => BufferState::Buffering,
                                _ => BufferState::Ok,
                            };
                            if let Err(e) = session.report_state(build_renderer_state(
                                report_state,
                                buffer,
                                report_position,
                                current_duration_ms,
                                current_queue_item_id,
                                None,
                            )).await {
                                warn!("[{}] Failed to report playback state to Qobuz: {}", name, e);
                            }
                        }

                        PlayerEvent::MetadataChanged(metadata) => {
                            debug!("[{}] Sonos metadata changed: track={:?}", name, metadata.track_name());

                            // Extract track_id from current item's MusicObjectId
                            if let Some(current_item) = &metadata.current_item
                                && let Some(track) = &current_item.track
                            {
                                // Update duration
                                current_duration_ms = track.duration_millis.map(|d| d as u32);

                                if let Some(id) = &track.id
                                    && let Some(track_id) = extract_track_id(&id.object_id)
                                {
                                    // Check if track changed
                                    let track_changed = current_track_id != Some(track_id);
                                    current_track_id = Some(track_id);

                                    // Look up queue_item_id from our queue state
                                    let new_queue_item_id =
                                        find_queue_item_id(&queue_store, &bridge_id, track_id).await;

                                    if track_changed || current_queue_item_id != new_queue_item_id {
                                        info!(
                                            "[{}] Track changed: queue_item {:?} -> {:?}",
                                            name, current_queue_item_id, new_queue_item_id
                                        );
                                        current_queue_item_id = new_queue_item_id;

                                        // Get fresh position from Sonos
                                        if let Ok(status) = player.get_playback_status(&group_id).await {
                                            let sonos_state = map_sonos_state(status.state);
                                            let fresh_position = status.position_millis.unwrap_or(0) as u32;

                                            // Apply commanded state override
                                            let report_state = match &commanded {
                                                Some(cmd) if cmd.state == sonos_state && cmd.is_confirmable() => {
                                                    commanded = None;
                                                    sonos_state
                                                }
                                                Some(cmd) if !cmd.is_expired() => cmd.state,
                                                Some(_) => {
                                                    commanded = None;
                                                    sonos_state
                                                }
                                                None => sonos_state,
                                            };

                                            let report_position = commanded
                                                .as_ref()
                                                .and_then(|c| c.position_ms)
                                                .unwrap_or(fresh_position);

                                            if let Err(e) = session.report_state(build_renderer_state(
                                                report_state,
                                                BufferState::Ok,
                                                report_position,
                                                current_duration_ms,
                                                current_queue_item_id,
                                                None,
                                            )).await {
                                                warn!("[{}] Failed to report track change to Qobuz: {}", name, e);
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        PlayerEvent::VolumeChanged(volume) => {
                            debug!(
                                "[{}] Sonos volume changed: {} (muted={})",
                                name, volume.volume, volume.muted
                            );

                            // Report volume to Qobuz
                            if let Err(e) = session.report_volume(volume.volume as u32).await {
                                warn!("[{}] Failed to report volume to Qobuz: {}", name, e);
                            }

                            // Report mute state to Qobuz
                            if let Err(e) = session.report_muted(volume.muted).await {
                                warn!("[{}] Failed to report mute state to Qobuz: {}", name, e);
                            }
                        }

                        PlayerEvent::Disconnected => {
                            warn!("[{}] Sonos player disconnected", name);
                        }

                        _ => {}
                    }
                }

                // Handle qonductor session events
                Some(event) = session.recv() => {
            match event {
                // === Commands (require response) ===
                SessionEvent::Command(cmd) => match cmd {
                    Command::SetState { cmd, respond } => {
                        info!(
                            "[{}] Playback command: state={:?} position={:?}",
                            name, cmd.state(), cmd.current_position
                        );

                        // If this is the initial SetState (before cloud queue loaded) and contains
                        // current_queue_item, store it for use when loading the cloud queue
                        if session_id.is_none()
                            && let Some(ref item) = cmd.current_queue_item
                        {
                            let queue_item_id = item.queue_item_id;
                            let position = cmd.current_position.unwrap_or(0);
                            debug!(
                                "[{}] Storing initial state: queue_item={}, position={}ms",
                                name, queue_item_id, position
                            );
                            pending_initial_item = Some((queue_item_id, position));
                        }

                        // Check if Qobuz is requesting a track change
                        let new_queue_item = cmd.current_queue_item.as_ref()
                            .map(|item| item.queue_item_id as i32);

                        let did_seek = cmd.current_position.is_some()
                            && (new_queue_item.is_none() || new_queue_item == current_queue_item_id);
                        let did_skip = new_queue_item.is_some()
                            && new_queue_item != current_queue_item_id;

                        // On skip, save old id for direction lookup, then update
                        let pre_skip_queue_item_id = current_queue_item_id;
                        if did_skip {
                            current_queue_item_id = new_queue_item;
                            current_duration_ms = None;
                        }

                        // Always respond with the commanded state — Sonos state is
                        // stale pre-command. Position is fine to query from Sonos
                        // when the command doesn't include one (plain play/pause).
                        let report_state = cmd.state().unwrap_or(PlayingState::Playing);
                        let report_position = match cmd.current_position {
                            Some(pos) => pos,
                            None => player.get_playback_status(&group_id).await
                                .map(|s| s.position_millis.unwrap_or(0) as u32)
                                .unwrap_or(0),
                        };

                        respond.send(build_renderer_state(
                            report_state,
                            BufferState::Ok,
                            report_position,
                            current_duration_ms,
                            current_queue_item_id,
                            None,
                        ));

                        // Now forward the command to Sonos (after responding to Qobuz)
                        if session_id.is_some() {
                            if did_skip {
                                if let (Some(new_id), Some(old_id)) = (new_queue_item, pre_skip_queue_item_id) {
                                    let direction = queue_store.get(&bridge_id).await
                                        .and_then(|state| {
                                            let cur_idx = state.get_item_index(&old_id.to_string())?;
                                            let new_idx = state.get_item_index(&new_id.to_string())?;
                                            Some(new_idx.cmp(&cur_idx))
                                        });

                                    match direction {
                                        Some(std::cmp::Ordering::Greater) => {
                                            info!("[{}] Skip next: {} -> {}", name, old_id, new_id);
                                            if let Err(e) = player.skip_next(&group_id).await {
                                                warn!("[{}] Failed to skip next: {}", name, e);
                                            }
                                        }
                                        Some(std::cmp::Ordering::Less) => {
                                            info!("[{}] Skip prev: {} -> {}", name, old_id, new_id);
                                            if let Err(e) = player.skip_prev(&group_id).await {
                                                warn!("[{}] Failed to skip prev: {}", name, e);
                                            }
                                        }
                                        _ => {
                                            warn!("[{}] Track change {} -> {} but couldn't determine direction", name, old_id, new_id);
                                        }
                                    }
                                }
                            } else {
                                if let Some(state) = cmd.state() {
                                    let result = match state {
                                        PlayingState::Playing => player.play(&group_id).await,
                                        PlayingState::Paused => player.pause(&group_id).await,
                                        PlayingState::Stopped => player.stop(&group_id).await,
                                        PlayingState::Unknown => Ok(()),
                                    };
                                    if let Err(e) = result {
                                        warn!("[{}] Failed to send playback command: {}", name, e);
                                    }
                                }

                                if did_seek
                                    && let Some(position) = cmd.current_position
                                {
                                    info!("[{}] Seek to {}ms", name, position);
                                    if let Err(e) = player.seek(&group_id, position).await {
                                        warn!("[{}] Failed to seek: {}", name, e);
                                    }
                                }
                            }

                            // Set commanded state override so we report the target
                            // state until Sonos confirms it caught up
                            commanded = Some(CommandedState::new(
                                report_state,
                                cmd.current_position,
                            ));
                            debug!(
                                "[{}] Set commanded state: {:?} position={:?}",
                                name, report_state, cmd.current_position
                            );
                        }
                    }

                    Command::SetActive { cmd: _, respond } => {
                        info!("[{}] Device activated", name);

                        // Get initial state from Sonos
                        let (volume, muted) = match player.get_volume(&group_id).await {
                            Ok(vol) => (vol.volume as u32, vol.muted),
                            Err(e) => {
                                warn!("[{}] Failed to get volume: {}", name, e);
                                (50, false)
                            }
                        };

                        // Use restored state from previous renderer if available,
                        // otherwise fall back to Sonos's actual state
                        let (state, position) = if let Some(rs) = restored_state.take() {
                            let pos = restored_position.take().unwrap_or(0);
                            info!("[{}] Using restored state: {:?} position={}ms", name, rs, pos);
                            (rs, pos)
                        } else {
                            match player.get_playback_status(&group_id).await {
                                Ok(status) => {
                                    let fresh_position = status.position_millis.unwrap_or(0) as u32;
                                    let state = match status.state {
                                        PlayState::Playing => PlayingState::Playing,
                                        PlayState::Paused => PlayingState::Paused,
                                        PlayState::Idle | PlayState::Buffering => PlayingState::Stopped,
                                    };
                                    (state, fresh_position)
                                }
                                Err(e) => {
                                    warn!("[{}] Failed to get playback status: {}", name, e);
                                    (PlayingState::Stopped, 0)
                                }
                            }
                        };

                        // Get initial metadata to track current item
                        if let Ok(metadata) = player.get_metadata(&group_id).await
                            && let Some(current_item) = &metadata.current_item
                            && let Some(track) = &current_item.track
                        {
                            current_duration_ms = track.duration_millis.map(|d| d as u32);
                            if let Some(id) = &track.id
                                && let Some(tid) = extract_track_id(&id.object_id)
                            {
                                current_track_id = Some(tid);
                                current_queue_item_id =
                                    find_queue_item_id(&queue_store, &bridge_id, tid).await;
                            }
                        }

                        respond.send(ActivationState {
                            muted,
                            volume,
                            max_quality: 4, // HiRes 192kHz
                            playback: build_renderer_state(
                                state,
                                BufferState::Ok,
                                position,
                                current_duration_ms,
                                current_queue_item_id,
                                None,
                            ),
                        });
                    }

                    Command::SetVolume { cmd, respond } => {
                        let new_volume = cmd.volume.unwrap_or(0).min(100) as u8;
                        info!("[{}] Volume command: {}", name, new_volume);

                        match player.set_volume(&group_id, new_volume).await {
                            Ok(()) => {
                                respond.send(qonductor::msg::report::VolumeChanged {
                                    volume: Some(new_volume as u32),
                                });
                            }
                            Err(e) => {
                                warn!("[{}] Failed to set volume: {}", name, e);
                                respond.send(qonductor::msg::report::VolumeChanged {
                                    volume: Some(new_volume as u32),
                                });
                            }
                        }
                    }

                    Command::Heartbeat { respond } => {
                        match player.get_playback_status(&group_id).await {
                            Ok(status) => {
                                let sonos_state = map_sonos_state(status.state);
                                let fresh_position = status.position_millis.unwrap_or(0) as u32;

                                // Apply commanded state override
                                let report_state = match &commanded {
                                    Some(cmd) if cmd.state == sonos_state && cmd.is_confirmable() => {
                                        commanded = None;
                                        sonos_state
                                    }
                                    Some(cmd) if !cmd.is_expired() => cmd.state,
                                    Some(_) => {
                                        commanded = None;
                                        sonos_state
                                    }
                                    None => sonos_state,
                                };

                                let report_position = commanded
                                    .as_ref()
                                    .and_then(|c| c.position_ms)
                                    .unwrap_or(fresh_position);

                                debug!(
                                    "[{}] Heartbeat: sonos={:?} reporting={:?} @ {}ms",
                                    name, sonos_state, report_state, report_position
                                );
                                respond.send(Some(build_renderer_state(
                                    report_state,
                                    BufferState::Ok,
                                    report_position,
                                    current_duration_ms,
                                    current_queue_item_id,
                                    None,
                                )));
                            }
                            Err(e) => {
                                warn!("[{}] Failed to get playback status for heartbeat: {}", name, e);
                                respond.send(None);
                            }
                        }
                    }
                },

                // === Notifications (informational events) ===
                SessionEvent::Notification(n) => match n {
                    Notification::Deactivated => {
                        info!("[{}] Device deactivated", name);
                    }

                    Notification::QueueState(queue) => {
                        let version = queue.queue_version.as_ref();
                        let major = version.and_then(|v| v.major).unwrap_or(0);
                        let minor = version.and_then(|v| v.minor).unwrap_or(0);
                        info!(
                            "[{}] Queue updated: {} tracks (version {}.{})",
                            name,
                            queue.tracks.len(),
                            major,
                            minor
                        );

                        // Need service account number to build queue
                        let Some(sn) = service_account_number else {
                            warn!("[{}] Cannot update queue: no service account number yet", name);
                            continue;
                        };

                        // Convert tracks to (track_id, queue_item_id) tuples
                        let track_tuples: Vec<(u64, u64)> = queue.tracks
                            .iter()
                            .filter_map(|t| {
                                Some((t.track_id? as u64, t.queue_item_id))
                            })
                            .collect();

                        // Update the queue store (include QobuzClient for lazy metadata fetch)
                        let queue_state = QueueState::from_tracks(
                            &track_tuples,
                            (major, minor),
                            sn,
                            qobuz_client.clone(),
                        );
                        queue_store.update(&bridge_id, queue_state).await;
                        debug!("[{}] Updated cloud queue store", name);

                        // Refresh current_queue_item_id if we're tracking a track
                        if let Some(tid) = current_track_id {
                            current_queue_item_id =
                                find_queue_item_id(&queue_store, &bridge_id, tid).await;
                        }

                        // If this is the first queue update, create session and load cloud queue
                        if session_id.is_none() && !queue.tracks.is_empty() {
                            info!("[{}] Creating playback session and loading cloud queue", name);

                            // Determine which track to start from:
                            // - Use pending_initial_item if we got current track info from SetState
                            // - Otherwise fall back to first track in queue
                            let (start_queue_item_id, start_track_id, start_position) =
                                if let Some((queue_item_id, position)) = pending_initial_item.take() {
                                    // Find the track_id for this queue_item_id
                                    let track_id = queue
                                        .tracks
                                        .iter()
                                        .find(|t| t.queue_item_id == queue_item_id)
                                        .and_then(|t| t.track_id)
                                        .map(|id| id as u64)
                                        .unwrap_or_else(|| {
                                            warn!(
                                                "[{}] Could not find track for queue_item_id {}, using first track",
                                                name, queue_item_id
                                            );
                                            queue.tracks.first().and_then(|t| t.track_id).unwrap_or(0) as u64
                                        });
                                    (queue_item_id, track_id, position)
                                } else {
                                    // No initial state from SetState, use first track
                                    let first = queue.tracks.first().unwrap();
                                    (
                                        first.queue_item_id,
                                        first.track_id.unwrap_or(0) as u64,
                                        0,
                                    )
                                };

                            debug!(
                                "[{}] Starting from queue_item={}, track={}, position={}ms",
                                name, start_queue_item_id, start_track_id, start_position
                            );

                            // Fetch metadata for the starting track
                            let track_metadata = if let Some(client) = &qobuz_client {
                                match client.get_tracks(&[start_track_id]).await {
                                    Ok(metadata) if !metadata.is_empty() => {
                                        let m = &metadata[0];
                                        debug!(
                                            "[{}] Fetched starting track metadata: {} - {}",
                                            name, m.artist_name, m.title
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
                                                object_id: format!("track:{}", start_track_id),
                                                service_id: Some("31".to_string()),
                                                account_id: Some(format!("sn_{}", sn)),
                                            }),
                                            ..Default::default()
                                        })
                                    }
                                    Ok(_) => {
                                        debug!("[{}] No metadata returned for starting track", name);
                                        None
                                    }
                                    Err(e) => {
                                        warn!("[{}] Failed to fetch starting track metadata: {}", name, e);
                                        None
                                    }
                                }
                            } else {
                                debug!("[{}] No Qobuz client available for metadata fetch", name);
                                None
                            };

                            // Create playback session
                            match player
                                .create_playback_session(
                                    &group_id,
                                    CreateSessionRequest {
                                        app_id: app_id.clone(),
                                        app_context: bridge_id.clone(),
                                        account_id: service_account_number
                                            .map(|sn| format!("sn_{}", sn)),
                                        ..Default::default()
                                    },
                                )
                                .await
                            {
                                Ok(playback_session) => {
                                    info!(
                                        "[{}] Created playback session: {}",
                                        name, playback_session.session_id
                                    );

                                    // Build cloud queue URL
                                    let queue_url = format!(
                                        "{}/queues/{}/v2.3/",
                                        cloud_queue_config.base_url, bridge_id
                                    );

                                    // Load the cloud queue starting at the current item
                                    let start_item_id = start_queue_item_id.to_string();

                                    match player
                                        .load_cloud_queue(
                                            &playback_session.session_id,
                                            LoadCloudQueueRequest {
                                                queue_base_url: queue_url.clone(),
                                                item_id: Some(start_item_id.clone()),
                                                play_on_completion: Some(true),
                                                track_metadata,
                                                position_millis: if start_position > 0 {
                                                    Some(start_position as i32)
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
                                                name, queue_url, start_item_id, start_position
                                            );
                                            session_id = Some(playback_session.session_id);
                                        }
                                        Err(e) => {
                                            warn!("[{}] Failed to load cloud queue: {}", name, e);
                                        }
                                    }
                                }
                                Err(e) => {
                                    warn!("[{}] Failed to create playback session: {}", name, e);
                                }
                            }
                        }
                    }

                    Notification::QueueLoadTracks(msg) => {
                        let version = msg.queue_version.as_ref();
                        let major = version.and_then(|v| v.major).unwrap_or(0);
                        let minor = version.and_then(|v| v.minor).unwrap_or(0);
                        let position = msg.queue_position.unwrap_or(0) as usize;
                        info!(
                            "[{}] Queue load tracks: {} tracks, position {} (version {}.{})",
                            name, msg.tracks.len(), position, major, minor
                        );

                        let Some(sn) = service_account_number else {
                            warn!("[{}] Cannot load tracks: no service account number yet", name);
                            continue;
                        };

                        // Update the queue store (same as QueueState)
                        let track_tuples: Vec<(u64, u64)> = msg.tracks
                            .iter()
                            .filter_map(|t| Some((t.track_id? as u64, t.queue_item_id)))
                            .collect();

                        let queue_state = QueueState::from_tracks(
                            &track_tuples,
                            (major, minor),
                            sn,
                            qobuz_client.clone(),
                        );
                        queue_store.update(&bridge_id, queue_state).await;

                        // Determine the target track from queue_position
                        let target = msg.tracks.get(position).or_else(|| msg.tracks.first());
                        let Some(target) = target else {
                            warn!("[{}] QueueLoadTracks with empty track list", name);
                            continue;
                        };

                        let target_queue_item_id = target.queue_item_id;
                        let target_track_id = target.track_id.unwrap_or(0) as u64;

                        // Update current track state
                        current_queue_item_id = Some(target_queue_item_id as i32);
                        current_track_id = Some(target_track_id);

                        // Fetch duration for the target track
                        if let Some(client) = &qobuz_client {
                            match client.get_tracks(&[target_track_id]).await {
                                Ok(metadata) if !metadata.is_empty() => {
                                    current_duration_ms = Some(metadata[0].duration_secs * 1000);
                                }
                                _ => {}
                            }
                        }

                        if let Some(sid) = &session_id {
                            // Active session — reload cloud queue at the target track
                            let queue_url = format!(
                                "{}/queues/{}/v2.3/",
                                cloud_queue_config.base_url, bridge_id
                            );
                            let start_item_id = target_queue_item_id.to_string();

                            // Fetch metadata for the target track
                            let track_metadata = if let Some(client) = &qobuz_client {
                                match client.get_tracks(&[target_track_id]).await {
                                    Ok(metadata) if !metadata.is_empty() => {
                                        let m = &metadata[0];
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
                                                object_id: format!("track:{}", target_track_id),
                                                service_id: Some("31".to_string()),
                                                account_id: Some(format!("sn_{}", sn)),
                                            }),
                                            ..Default::default()
                                        })
                                    }
                                    _ => None,
                                }
                            } else {
                                None
                            };

                            match player
                                .load_cloud_queue(
                                    sid,
                                    LoadCloudQueueRequest {
                                        queue_base_url: queue_url,
                                        item_id: Some(start_item_id.clone()),
                                        play_on_completion: Some(true),
                                        track_metadata,
                                        ..Default::default()
                                    },
                                )
                                .await
                            {
                                Ok(()) => {
                                    info!(
                                        "[{}] Loaded cloud queue at item {} (track {})",
                                        name, start_item_id, target_track_id
                                    );
                                    commanded = Some(CommandedState::new(PlayingState::Playing, None));
                                }
                                Err(e) => {
                                    warn!("[{}] Failed to load cloud queue: {}", name, e);
                                }
                            }
                        } else {
                            // No session yet — store as pending so initial load picks it up
                            info!(
                                "[{}] No active session, storing target track as pending",
                                name
                            );
                            pending_initial_item = Some((target_queue_item_id, 0));
                        }
                    }

                    Notification::LoopModeSet(msg) => {
                        info!("[{}] Loop mode changed: {:?}", name, msg.mode);
                        // TODO: Sync to Sonos play modes
                    }

                    Notification::ShuffleModeSet(msg) => {
                        info!("[{}] Shuffle mode changed: {:?}", name, msg.shuffle_on);
                        // TODO: Sync to Sonos play modes
                    }

                    Notification::RestoreState(state) => {
                        let position = state.state.as_ref()
                            .and_then(|s| s.current_position.as_ref())
                            .and_then(|p| p.value);
                        let queue_index = state.state.as_ref()
                            .and_then(|s| s.current_queue_index);
                        let playing_state = state.state.as_ref()
                            .and_then(|s| s.playing_state)
                            .and_then(|s| PlayingState::try_from(s).ok());
                        info!(
                            "[{}] Restore state: state={:?} position={:?}ms queue_idx={:?}",
                            name, playing_state, position, queue_index
                        );
                        // Save for use in SetActive response
                        restored_state = playing_state;
                        restored_position = position;
                    }

                    Notification::QueueTracksAdded(msg) => {
                        let version = msg.queue_version.as_ref();
                        let major = version.and_then(|v| v.major).unwrap_or(0);
                        let minor = version.and_then(|v| v.minor).unwrap_or(0);
                        info!(
                            "[{}] Queue tracks added: {} tracks (version {}.{})",
                            name, msg.tracks.len(), major, minor
                        );

                        let Some(sn) = service_account_number else {
                            warn!("[{}] Cannot add tracks: no service account number", name);
                            continue;
                        };

                        if let Some(mut state) = queue_store.get(&bridge_id).await {
                            for t in &msg.tracks {
                                if let Some(track_id) = t.track_id {
                                    state.items.push(cloud_queue_item(track_id as u64, t.queue_item_id, sn));
                                }
                            }
                            state.version = format!("{}.{}", major, minor);
                            queue_store.update(&bridge_id, state).await;
                            debug!("[{}] Updated queue with {} new tracks", name, msg.tracks.len());
                        }
                    }

                    Notification::QueueTracksInserted(msg) => {
                        let version = msg.queue_version.as_ref();
                        let major = version.and_then(|v| v.major).unwrap_or(0);
                        let minor = version.and_then(|v| v.minor).unwrap_or(0);
                        info!(
                            "[{}] Queue tracks inserted: {} tracks after {:?} (version {}.{})",
                            name, msg.tracks.len(), msg.insert_after, major, minor
                        );

                        let Some(sn) = service_account_number else {
                            warn!("[{}] Cannot insert tracks: no service account number", name);
                            continue;
                        };

                        if let Some(mut state) = queue_store.get(&bridge_id).await {
                            let insert_idx = msg.insert_after
                                .and_then(|id| state.get_item_index(&id.to_string()))
                                .map(|i| i + 1)
                                .unwrap_or(0);

                            for (i, t) in msg.tracks.iter().enumerate() {
                                if let Some(track_id) = t.track_id {
                                    state.items.insert(insert_idx + i, cloud_queue_item(track_id as u64, t.queue_item_id, sn));
                                }
                            }
                            state.version = format!("{}.{}", major, minor);
                            queue_store.update(&bridge_id, state).await;
                            debug!("[{}] Inserted {} tracks at position {}", name, msg.tracks.len(), insert_idx);
                        }
                    }

                    Notification::QueueTracksRemoved(msg) => {
                        let version = msg.queue_version.as_ref();
                        let major = version.and_then(|v| v.major).unwrap_or(0);
                        let minor = version.and_then(|v| v.minor).unwrap_or(0);
                        info!(
                            "[{}] Queue tracks removed: {} tracks (version {}.{})",
                            name, msg.queue_item_ids.len(), major, minor
                        );

                        if let Some(mut state) = queue_store.get(&bridge_id).await {
                            let ids: HashSet<String> = msg.queue_item_ids.iter().map(|id| id.to_string()).collect();
                            let before_len = state.items.len();
                            state.items.retain(|item| !ids.contains(&item.id));
                            let removed = before_len - state.items.len();
                            state.version = format!("{}.{}", major, minor);
                            queue_store.update(&bridge_id, state).await;
                            debug!("[{}] Removed {} tracks from queue", name, removed);
                        }
                    }

                    Notification::QueueTracksReordered(msg) => {
                        let version = msg.queue_version.as_ref();
                        let major = version.and_then(|v| v.major).unwrap_or(0);
                        let minor = version.and_then(|v| v.minor).unwrap_or(0);
                        info!(
                            "[{}] Queue tracks reordered: {} tracks to after {:?} (version {}.{})",
                            name, msg.queue_item_ids.len(), msg.insert_after, major, minor
                        );

                        if let Some(mut state) = queue_store.get(&bridge_id).await {
                            let ids: HashSet<String> = msg.queue_item_ids.iter().map(|id| id.to_string()).collect();

                            // Extract items to move
                            let moved: Vec<_> = state.items.iter()
                                .filter(|item| ids.contains(&item.id))
                                .cloned()
                                .collect();

                            // Remove from current positions
                            state.items.retain(|item| !ids.contains(&item.id));

                            // Find insertion point and insert
                            let insert_idx = msg.insert_after
                                .and_then(|id| state.get_item_index(&id.to_string()))
                                .map(|i| i + 1)
                                .unwrap_or(0);

                            for (i, item) in moved.into_iter().enumerate() {
                                state.items.insert(insert_idx + i, item);
                            }
                            state.version = format!("{}.{}", major, minor);
                            queue_store.update(&bridge_id, state).await;
                            debug!("[{}] Reordered {} tracks to position {}", name, msg.queue_item_ids.len(), insert_idx);
                        }
                    }

                    Notification::Connected => {
                        info!("[{}] Connected to Qobuz", name);
                    }

                    Notification::Disconnected { reason, .. } => {
                        warn!("[{}] Disconnected from Qobuz: {:?}", name, reason);
                    }

                    Notification::DeviceRegistered {
                        device_uuid,
                        renderer_id,
                        api_jwt,
                    } => {
                        info!(
                            "[{}] Device registered: {:02x?} -> renderer {}",
                            name, &device_uuid[..4], renderer_id
                        );

                        // Create and store Qobuz API client for metadata fetching
                        let client = QobuzClient::new(&api_jwt, &app_id);

                        // Get user's public_id from Qobuz REST API
                        match client.get_user().await {
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
                                        service_account_number = sn;
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

                        // Store client for later use (metadata fetching)
                        qobuz_client = Some(client);
                    }

                    Notification::SessionClosed { .. } => {
                        info!("[{}] Session closed", name);
                    }

                    // Ignore other notifications (broadcasts from other renderers, etc.)
                    _ => {}
                }
            }
                }

                // Session closed
                else => {
                    break;
                }
            }
        }

        info!("[{}] Session ended", name);
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
