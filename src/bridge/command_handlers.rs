//! Qobuz command handlers.
//!
//! Handles commands from the Qobuz app that require a response:
//! SetState (play/pause/seek/skip), SetActive, SetVolume, Heartbeat.

use crate::sonos::PlayState;
use qonductor::msg::cmd::{SetState, SetVolume};
use qonductor::{ActivationState, BufferState, Command, PlayingState, Responder};
use tracing::{debug, info, warn};

use super::state::{
    build_renderer_state, extract_track_id, find_queue_item_id, map_sonos_state, BridgeState,
    CommandedState,
};

impl BridgeState {
    /// Dispatch a qonductor command.
    pub async fn handle_command(&mut self, cmd: Command) {
        match cmd {
            Command::SetState { cmd, respond } => {
                self.handle_set_state(cmd, respond).await;
            }
            Command::SetActive { cmd: _, respond } => {
                self.handle_set_active(respond).await;
            }
            Command::SetVolume { cmd, respond } => {
                self.handle_set_volume(cmd, respond).await;
            }
            Command::Heartbeat { respond } => {
                self.handle_heartbeat(respond).await;
            }
        }
    }

    /// Handle SetState command (play/pause/seek/skip).
    async fn handle_set_state(
        &mut self,
        cmd: SetState,
        respond: Responder<qonductor::msg::QueueRendererState>,
    ) {
        use qonductor::msg::SetStateExt;

        info!(
            "[{}] Playback command: state={:?} position={:?}",
            self.name,
            cmd.state(),
            cmd.current_position
        );

        // If this is the initial SetState (before cloud queue loaded) and contains
        // current_queue_item, store it for use when loading the cloud queue
        if self.session_id.is_none()
            && let Some(ref item) = cmd.current_queue_item
        {
            let queue_item_id = item.queue_item_id;
            let position = cmd.current_position.unwrap_or(0);
            debug!(
                "[{}] Storing initial state: queue_item={}, position={}ms",
                self.name, queue_item_id, position
            );
            self.pending_initial_item = Some((queue_item_id, position));
        }

        // Check if Qobuz is requesting a track change
        let new_queue_item = cmd
            .current_queue_item
            .as_ref()
            .map(|item| item.queue_item_id as i32);

        let did_seek = cmd.current_position.is_some()
            && (new_queue_item.is_none()
                || new_queue_item == self.current_queue_item_id);
        let did_skip = new_queue_item.is_some()
            && new_queue_item != self.current_queue_item_id;

        // On skip, save old id for direction lookup, then update
        let pre_skip_queue_item_id = self.current_queue_item_id;
        if did_skip {
            self.current_queue_item_id = new_queue_item;
            self.current_duration_ms = None;
        }

        // Always respond with the commanded state — Sonos state is
        // stale pre-command. Position is fine to query from Sonos
        // when the command doesn't include one (plain play/pause).
        let report_state = cmd.state().unwrap_or(PlayingState::Playing);
        let report_position = match cmd.current_position {
            Some(pos) => pos,
            None => self
                .player
                .get_playback_status(&self.group_id)
                .await
                .map(|s| s.position_millis.unwrap_or(0) as u32)
                .unwrap_or(0),
        };

        respond.send(build_renderer_state(
            report_state,
            BufferState::Ok,
            report_position,
            self.current_duration_ms,
            self.current_queue_item_id,
            None,
        ));

        // Now forward the command to Sonos (after responding to Qobuz)
        if self.session_id.is_some() {
            if did_skip {
                if let (Some(new_id), Some(old_id)) =
                    (new_queue_item, pre_skip_queue_item_id)
                {
                    let direction = self
                        .queue_store
                        .get(&self.bridge_id)
                        .await
                        .and_then(|state| {
                            let cur_idx =
                                state.get_item_index(&old_id.to_string())?;
                            let new_idx =
                                state.get_item_index(&new_id.to_string())?;
                            Some(new_idx.cmp(&cur_idx))
                        });

                    match direction {
                        Some(std::cmp::Ordering::Greater) => {
                            info!(
                                "[{}] Skip next: {} -> {}",
                                self.name, old_id, new_id
                            );
                            if let Err(e) =
                                self.player.skip_next(&self.group_id).await
                            {
                                warn!(
                                    "[{}] Failed to skip next: {}",
                                    self.name, e
                                );
                            }
                        }
                        Some(std::cmp::Ordering::Less) => {
                            info!(
                                "[{}] Skip prev: {} -> {}",
                                self.name, old_id, new_id
                            );
                            if let Err(e) =
                                self.player.skip_prev(&self.group_id).await
                            {
                                warn!(
                                    "[{}] Failed to skip prev: {}",
                                    self.name, e
                                );
                            }
                        }
                        _ => {
                            warn!(
                                "[{}] Track change {} -> {} but couldn't determine direction",
                                self.name, old_id, new_id
                            );
                        }
                    }
                }
            } else {
                if let Some(state) = cmd.state() {
                    let result = match state {
                        PlayingState::Playing => {
                            self.player.play(&self.group_id).await
                        }
                        PlayingState::Paused => {
                            self.player.pause(&self.group_id).await
                        }
                        PlayingState::Stopped => {
                            self.player.stop(&self.group_id).await
                        }
                        PlayingState::Unknown => Ok(()),
                    };
                    if let Err(e) = result {
                        warn!(
                            "[{}] Failed to send playback command: {}",
                            self.name, e
                        );
                    }
                }

                if did_seek
                    && let Some(position) = cmd.current_position
                {
                    info!("[{}] Seek to {}ms", self.name, position);
                    if let Err(e) =
                        self.player.seek(&self.group_id, position).await
                    {
                        warn!("[{}] Failed to seek: {}", self.name, e);
                    }
                }
            }

            // Set commanded state override so we report the target
            // state until Sonos confirms it caught up
            self.commanded =
                Some(CommandedState::new(report_state, cmd.current_position));
            debug!(
                "[{}] Set commanded state: {:?} position={:?}",
                self.name, report_state, cmd.current_position
            );
        }
    }

    /// Handle SetActive command (device activation).
    async fn handle_set_active(
        &mut self,
        respond: qonductor::Responder<ActivationState>,
    ) {
        info!("[{}] Device activated", self.name);

        // Get initial state from Sonos
        let (volume, muted) = match self
            .player
            .get_volume(&self.group_id)
            .await
        {
            Ok(vol) => (vol.volume as u32, vol.muted),
            Err(e) => {
                warn!("[{}] Failed to get volume: {}", self.name, e);
                (50, false)
            }
        };

        // Use restored state from previous renderer if available,
        // otherwise fall back to Sonos's actual state
        let (state, position) = if let Some(rs) = self.restored_state.take() {
            let pos = self.restored_position.take().unwrap_or(0);
            info!(
                "[{}] Using restored state: {:?} position={}ms",
                self.name, rs, pos
            );
            (rs, pos)
        } else {
            match self
                .player
                .get_playback_status(&self.group_id)
                .await
            {
                Ok(status) => {
                    let fresh_position =
                        status.position_millis.unwrap_or(0) as u32;
                    let state = match status.state {
                        PlayState::Playing => PlayingState::Playing,
                        PlayState::Paused => PlayingState::Paused,
                        PlayState::Idle | PlayState::Buffering => {
                            PlayingState::Stopped
                        }
                    };
                    (state, fresh_position)
                }
                Err(e) => {
                    warn!(
                        "[{}] Failed to get playback status: {}",
                        self.name, e
                    );
                    (PlayingState::Stopped, 0)
                }
            }
        };

        // Get initial metadata to track current item
        if let Ok(metadata) = self.player.get_metadata(&self.group_id).await
            && let Some(current_item) = &metadata.current_item
            && let Some(track) = &current_item.track
        {
            self.current_duration_ms =
                track.duration_millis.map(|d| d as u32);
            if let Some(id) = &track.id
                && let Some(tid) = extract_track_id(&id.object_id)
            {
                self.current_track_id = Some(tid);
                self.current_queue_item_id = find_queue_item_id(
                    &self.queue_store,
                    &self.bridge_id,
                    tid,
                )
                .await;
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
                self.current_duration_ms,
                self.current_queue_item_id,
                None,
            ),
        });
    }

    /// Handle SetVolume command.
    async fn handle_set_volume(
        &mut self,
        cmd: SetVolume,
        respond: Responder<qonductor::msg::report::VolumeChanged>,
    ) {
        let new_volume = cmd.volume.unwrap_or(0).min(100) as u8;
        info!("[{}] Volume command: {}", self.name, new_volume);

        if let Err(e) = self
            .player
            .set_volume(&self.group_id, new_volume)
            .await
        {
            warn!("[{}] Failed to set volume: {}", self.name, e);
        }

        respond.send(qonductor::msg::report::VolumeChanged {
            volume: Some(new_volume as u32),
        });
    }

    /// Handle Heartbeat command.
    async fn handle_heartbeat(
        &mut self,
        respond: qonductor::Responder<Option<qonductor::msg::QueueRendererState>>,
    ) {
        match self
            .player
            .get_playback_status(&self.group_id)
            .await
        {
            Ok(status) => {
                let sonos_state = map_sonos_state(status.state);
                let fresh_position =
                    status.position_millis.unwrap_or(0) as u32;

                let report_state =
                    self.resolve_commanded_state(sonos_state);
                let report_position =
                    self.resolve_report_position(fresh_position);

                debug!(
                    "[{}] Heartbeat: sonos={:?} reporting={:?} @ {}ms",
                    self.name, sonos_state, report_state, report_position
                );
                respond.send(Some(build_renderer_state(
                    report_state,
                    BufferState::Ok,
                    report_position,
                    self.current_duration_ms,
                    self.current_queue_item_id,
                    None,
                )));
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to get playback status for heartbeat: {}",
                    self.name, e
                );
                respond.send(None);
            }
        }
    }
}
