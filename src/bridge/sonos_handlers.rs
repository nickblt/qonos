//! Sonos player event handlers.
//!
//! Handles bidirectional sync from Sonos back to Qobuz — playback state,
//! metadata changes, and volume updates.

use crate::sonos::{PlayState, PlayerEvent};
use qonductor::BufferState;
use tracing::{debug, error, info, warn};

use super::state::{
    build_renderer_state, extract_track_id, find_queue_item_id, map_sonos_state, BridgeState,
};

impl BridgeState {
    /// Dispatch a Sonos player event.
    pub async fn handle_player_event(&mut self, event: PlayerEvent) {
        match event {
            PlayerEvent::PlaybackError(err) => {
                error!(
                    "[{}] Sonos playback error: {} (item={:?}, http_status={:?}, reason={:?})",
                    self.name, err.error_code, err.item_id, err.http_status, err.reason
                );
            }
            PlayerEvent::PlaybackChanged(status) => {
                self.handle_playback_changed(status).await;
            }
            PlayerEvent::MetadataChanged(metadata) => {
                self.handle_metadata_changed(metadata).await;
            }
            PlayerEvent::VolumeChanged(volume) => {
                self.handle_volume_changed(volume).await;
            }
            PlayerEvent::Disconnected => {
                warn!("[{}] Sonos player disconnected", self.name);
            }
            _ => {}
        }
    }

    /// Handle playback state changes from Sonos.
    async fn handle_playback_changed(
        &mut self,
        status: sonos_websocket::PlaybackStatus,
    ) {
        let sonos_state = map_sonos_state(status.state);

        // Query fresh position from Sonos (event position may be stale)
        let fresh_position = match self
            .player
            .get_playback_status(&self.group_id)
            .await
        {
            Ok(fresh) => fresh.position_millis.unwrap_or(0) as u32,
            Err(_) => status.position_millis.unwrap_or(0) as u32,
        };

        let report_state = self.resolve_commanded_state(sonos_state);
        let report_position = self.resolve_report_position(fresh_position);

        debug!(
            "[{}] Sonos playback changed: {:?} -> reporting {:?} @ {}ms",
            self.name, status.state, report_state, report_position
        );

        let buffer = match status.state {
            PlayState::Buffering => BufferState::Buffering,
            _ => BufferState::Ok,
        };
        if let Err(e) = self
            .session
            .report_state(build_renderer_state(
                report_state,
                buffer,
                report_position,
                self.current_duration_ms,
                self.current_queue_item_id,
                None,
            ))
            .await
        {
            warn!(
                "[{}] Failed to report playback state to Qobuz: {}",
                self.name, e
            );
        }
    }

    /// Handle metadata changes from Sonos (track changes).
    async fn handle_metadata_changed(
        &mut self,
        metadata: sonos_websocket::MetadataStatus,
    ) {
        debug!(
            "[{}] Sonos metadata changed: track={:?}",
            self.name,
            metadata.track_name()
        );

        // Extract track_id from current item's MusicObjectId
        if let Some(current_item) = &metadata.current_item
            && let Some(track) = &current_item.track
        {
            // Update duration
            self.current_duration_ms = track.duration_millis.map(|d| d as u32);

            if let Some(id) = &track.id
                && let Some(track_id) = extract_track_id(&id.object_id)
            {
                // Check if track changed
                let track_changed = self.current_track_id != Some(track_id);
                self.current_track_id = Some(track_id);

                // Look up queue_item_id from our queue state
                let new_queue_item_id =
                    find_queue_item_id(&self.queue_store, &self.bridge_id, track_id).await;

                if track_changed || self.current_queue_item_id != new_queue_item_id {
                    info!(
                        "[{}] Track changed: queue_item {:?} -> {:?}",
                        self.name, self.current_queue_item_id, new_queue_item_id
                    );
                    self.current_queue_item_id = new_queue_item_id;

                    // Get fresh position from Sonos
                    if let Ok(status) =
                        self.player.get_playback_status(&self.group_id).await
                    {
                        let sonos_state = map_sonos_state(status.state);
                        let fresh_position =
                            status.position_millis.unwrap_or(0) as u32;

                        let report_state =
                            self.resolve_commanded_state(sonos_state);
                        let report_position =
                            self.resolve_report_position(fresh_position);

                        if let Err(e) = self
                            .session
                            .report_state(build_renderer_state(
                                report_state,
                                BufferState::Ok,
                                report_position,
                                self.current_duration_ms,
                                self.current_queue_item_id,
                                None,
                            ))
                            .await
                        {
                            warn!(
                                "[{}] Failed to report track change to Qobuz: {}",
                                self.name, e
                            );
                        }
                    }
                }
            }
        }
    }

    /// Handle volume changes from Sonos.
    async fn handle_volume_changed(&mut self, volume: sonos_websocket::GroupVolume) {
        debug!(
            "[{}] Sonos volume changed: {} (muted={})",
            self.name, volume.volume, volume.muted
        );

        if let Err(e) = self.session.report_volume(volume.volume as u32).await {
            warn!(
                "[{}] Failed to report volume to Qobuz: {}",
                self.name, e
            );
        }

        if let Err(e) = self.session.report_muted(volume.muted).await {
            warn!(
                "[{}] Failed to report mute state to Qobuz: {}",
                self.name, e
            );
        }
    }
}
