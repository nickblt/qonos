//! Qobuz notification handlers.
//!
//! Handles informational events from the Qobuz session: queue updates,
//! track additions/removals/reordering, device registration, etc.

use crate::cloud_queue::{cloud_queue_item, QueueState};
use crate::qobuz_api::QobuzClient;
use qonductor::msg::notify;
use qonductor::{Notification, PlayingState};
use std::collections::HashSet;
use tracing::{debug, info, warn};

use super::state::BridgeState;

impl BridgeState {
    /// Dispatch a qonductor notification.
    pub async fn handle_notification(&mut self, n: Notification) {
        match n {
            Notification::Deactivated => {
                info!("[{}] Device deactivated", self.name);
            }
            Notification::QueueState(queue) => {
                self.handle_queue_state(queue).await;
            }
            Notification::QueueLoadTracks(msg) => {
                self.handle_queue_load_tracks(msg).await;
            }
            Notification::LoopModeSet(msg) => {
                info!("[{}] Loop mode changed: {:?}", self.name, msg.mode);
                // TODO: Sync to Sonos play modes
            }
            Notification::ShuffleModeSet(msg) => {
                info!(
                    "[{}] Shuffle mode changed: {:?}",
                    self.name, msg.shuffle_on
                );
                // TODO: Sync to Sonos play modes
            }
            Notification::RestoreState(state) => {
                self.handle_restore_state(state);
            }
            Notification::QueueTracksAdded(msg) => {
                self.handle_queue_tracks_added(msg).await;
            }
            Notification::QueueTracksInserted(msg) => {
                self.handle_queue_tracks_inserted(msg).await;
            }
            Notification::QueueTracksRemoved(msg) => {
                self.handle_queue_tracks_removed(msg).await;
            }
            Notification::QueueTracksReordered(msg) => {
                self.handle_queue_tracks_reordered(msg).await;
            }
            Notification::Connected => {
                info!("[{}] Connected to Qobuz", self.name);
            }
            Notification::Disconnected { reason, .. } => {
                warn!(
                    "[{}] Disconnected from Qobuz: {:?}",
                    self.name, reason
                );
            }
            Notification::DeviceRegistered {
                device_uuid,
                renderer_id,
                api_jwt,
            } => {
                self.handle_device_registered(device_uuid, renderer_id, api_jwt)
                    .await;
            }
            Notification::SessionClosed { .. } => {
                info!("[{}] Session closed", self.name);
            }
            // Ignore other notifications (broadcasts from other renderers, etc.)
            _ => {}
        }
    }

    /// Handle full queue state update.
    async fn handle_queue_state(
        &mut self,
        queue: notify::QueueState,
    ) {
        use super::state::find_queue_item_id;

        let version = queue.queue_version.as_ref();
        let major = version.and_then(|v| v.major).unwrap_or(0);
        let minor = version.and_then(|v| v.minor).unwrap_or(0);
        info!(
            "[{}] Queue updated: {} tracks (version {}.{})",
            self.name,
            queue.tracks.len(),
            major,
            minor
        );

        // Need service account number to build queue
        let Some(sn) = self.service_account_number else {
            warn!(
                "[{}] Cannot update queue: no service account number yet",
                self.name
            );
            return;
        };

        // Convert tracks to (track_id, queue_item_id) tuples
        let track_tuples: Vec<(u64, u64)> = queue
            .tracks
            .iter()
            .filter_map(|t| Some((t.track_id? as u64, t.queue_item_id)))
            .collect();

        // Update the queue store (include QobuzClient for lazy metadata fetch)
        let queue_state = QueueState::from_tracks(
            &track_tuples,
            (major, minor),
            sn,
            self.qobuz_client.clone(),
        );
        self.queue_store.update(&self.bridge_id, queue_state).await;
        debug!("[{}] Updated cloud queue store", self.name);

        // Refresh current_queue_item_id if we're tracking a track
        if let Some(tid) = self.current_track_id {
            self.current_queue_item_id =
                find_queue_item_id(&self.queue_store, &self.bridge_id, tid)
                    .await;
        }

        // If this is the first queue update, create session and load cloud queue
        if self.session_id.is_none() && !queue.tracks.is_empty() {
            // Determine which track to start from:
            // - Use pending_initial_item if we got current track info from SetState
            // - Otherwise fall back to first track in queue
            let (start_queue_item_id, start_track_id, start_position) =
                if let Some((queue_item_id, position)) =
                    self.pending_initial_item.take()
                {
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
                                self.name, queue_item_id
                            );
                            queue
                                .tracks
                                .first()
                                .and_then(|t| t.track_id)
                                .unwrap_or(0)
                                as u64
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
                self.name, start_queue_item_id, start_track_id, start_position
            );

            self.load_cloud_queue_at(
                start_queue_item_id,
                start_track_id,
                start_position,
                sn,
            )
            .await;
        }
    }

    /// Handle queue load tracks (replace entire queue + start playback).
    async fn handle_queue_load_tracks(
        &mut self,
        msg: notify::QueueLoadTracks,
    ) {
        let version = msg.queue_version.as_ref();
        let major = version.and_then(|v| v.major).unwrap_or(0);
        let minor = version.and_then(|v| v.minor).unwrap_or(0);
        let position = msg.queue_position.unwrap_or(0) as usize;
        info!(
            "[{}] Queue load tracks: {} tracks, position {} (version {}.{})",
            self.name,
            msg.tracks.len(),
            position,
            major,
            minor
        );

        let Some(sn) = self.service_account_number else {
            warn!(
                "[{}] Cannot load tracks: no service account number yet",
                self.name
            );
            return;
        };

        // Update the queue store
        let track_tuples: Vec<(u64, u64)> = msg
            .tracks
            .iter()
            .filter_map(|t| Some((t.track_id? as u64, t.queue_item_id)))
            .collect();

        let queue_state = QueueState::from_tracks(
            &track_tuples,
            (major, minor),
            sn,
            self.qobuz_client.clone(),
        );
        self.queue_store.update(&self.bridge_id, queue_state).await;

        // Determine the target track from queue_position
        let target = msg.tracks.get(position).or_else(|| msg.tracks.first());
        let Some(target) = target else {
            warn!(
                "[{}] QueueLoadTracks with empty track list",
                self.name
            );
            return;
        };

        let target_queue_item_id = target.queue_item_id;
        let target_track_id = target.track_id.unwrap_or(0) as u64;

        // Update current track state
        self.current_queue_item_id = Some(target_queue_item_id as i32);
        self.current_track_id = Some(target_track_id);

        // Fetch duration for the target track
        if let Some(client) = &self.qobuz_client {
            match client.get_tracks(&[target_track_id]).await {
                Ok(metadata) if !metadata.is_empty() => {
                    self.current_duration_ms =
                        Some(metadata[0].duration_secs * 1000);
                }
                _ => {}
            }
        }

        if self.session_id.is_some() {
            self.load_cloud_queue_at(
                target_queue_item_id,
                target_track_id,
                0,
                sn,
            )
            .await;
        } else {
            // No session yet — store as pending so initial load picks it up
            info!(
                "[{}] No active session, storing target track as pending",
                self.name
            );
            self.pending_initial_item = Some((target_queue_item_id, 0));
        }
    }

    /// Handle restore state from previous renderer.
    fn handle_restore_state(
        &mut self,
        state: notify::RendererStateUpdated,
    ) {
        let position = state
            .state
            .as_ref()
            .and_then(|s| s.current_position.as_ref())
            .and_then(|p| p.value);
        let queue_index = state
            .state
            .as_ref()
            .and_then(|s| s.current_queue_index);
        let playing_state = state
            .state
            .as_ref()
            .and_then(|s| s.playing_state)
            .and_then(|s| PlayingState::try_from(s).ok());
        info!(
            "[{}] Restore state: state={:?} position={:?}ms queue_idx={:?}",
            self.name, playing_state, position, queue_index
        );
        // Save for use in SetActive response
        self.restored_state = playing_state;
        self.restored_position = position;
    }

    /// Handle tracks being added to the end of the queue.
    async fn handle_queue_tracks_added(
        &mut self,
        msg: notify::QueueTracksAdded,
    ) {
        let version = msg.queue_version.as_ref();
        let major = version.and_then(|v| v.major).unwrap_or(0);
        let minor = version.and_then(|v| v.minor).unwrap_or(0);
        info!(
            "[{}] Queue tracks added: {} tracks (version {}.{})",
            self.name,
            msg.tracks.len(),
            major,
            minor
        );

        let Some(sn) = self.service_account_number else {
            warn!(
                "[{}] Cannot add tracks: no service account number",
                self.name
            );
            return;
        };

        if let Some(mut state) =
            self.queue_store.get(&self.bridge_id).await
        {
            for t in &msg.tracks {
                if let Some(track_id) = t.track_id {
                    state.items.push(cloud_queue_item(
                        track_id as u64,
                        t.queue_item_id,
                        sn,
                    ));
                }
            }
            state.version = format!("{}.{}", major, minor);
            self.queue_store
                .update(&self.bridge_id, state)
                .await;
            debug!(
                "[{}] Updated queue with {} new tracks",
                self.name,
                msg.tracks.len()
            );
        }
    }

    /// Handle tracks being inserted at a specific position.
    async fn handle_queue_tracks_inserted(
        &mut self,
        msg: notify::QueueTracksInserted,
    ) {
        let version = msg.queue_version.as_ref();
        let major = version.and_then(|v| v.major).unwrap_or(0);
        let minor = version.and_then(|v| v.minor).unwrap_or(0);
        info!(
            "[{}] Queue tracks inserted: {} tracks after {:?} (version {}.{})",
            self.name,
            msg.tracks.len(),
            msg.insert_after,
            major,
            minor
        );

        let Some(sn) = self.service_account_number else {
            warn!(
                "[{}] Cannot insert tracks: no service account number",
                self.name
            );
            return;
        };

        if let Some(mut state) =
            self.queue_store.get(&self.bridge_id).await
        {
            let insert_idx = msg
                .insert_after
                .and_then(|id| state.get_item_index(&id.to_string()))
                .map(|i| i + 1)
                .unwrap_or(0);

            for (i, t) in msg.tracks.iter().enumerate() {
                if let Some(track_id) = t.track_id {
                    state.items.insert(
                        insert_idx + i,
                        cloud_queue_item(
                            track_id as u64,
                            t.queue_item_id,
                            sn,
                        ),
                    );
                }
            }
            state.version = format!("{}.{}", major, minor);
            self.queue_store
                .update(&self.bridge_id, state)
                .await;
            debug!(
                "[{}] Inserted {} tracks at position {}",
                self.name,
                msg.tracks.len(),
                insert_idx
            );
        }
    }

    /// Handle tracks being removed from the queue.
    async fn handle_queue_tracks_removed(
        &mut self,
        msg: notify::QueueTracksRemoved,
    ) {
        let version = msg.queue_version.as_ref();
        let major = version.and_then(|v| v.major).unwrap_or(0);
        let minor = version.and_then(|v| v.minor).unwrap_or(0);
        info!(
            "[{}] Queue tracks removed: {} tracks (version {}.{})",
            self.name,
            msg.queue_item_ids.len(),
            major,
            minor
        );

        if let Some(mut state) =
            self.queue_store.get(&self.bridge_id).await
        {
            let ids: HashSet<String> = msg
                .queue_item_ids
                .iter()
                .map(|id| id.to_string())
                .collect();
            let before_len = state.items.len();
            state.items.retain(|item| !ids.contains(&item.id));
            let removed = before_len - state.items.len();
            state.version = format!("{}.{}", major, minor);
            self.queue_store
                .update(&self.bridge_id, state)
                .await;
            debug!(
                "[{}] Removed {} tracks from queue",
                self.name, removed
            );
        }
    }

    /// Handle tracks being reordered within the queue.
    async fn handle_queue_tracks_reordered(
        &mut self,
        msg: notify::QueueTracksReordered,
    ) {
        let version = msg.queue_version.as_ref();
        let major = version.and_then(|v| v.major).unwrap_or(0);
        let minor = version.and_then(|v| v.minor).unwrap_or(0);
        info!(
            "[{}] Queue tracks reordered: {} tracks to after {:?} (version {}.{})",
            self.name,
            msg.queue_item_ids.len(),
            msg.insert_after,
            major,
            minor
        );

        if let Some(mut state) =
            self.queue_store.get(&self.bridge_id).await
        {
            let ids: HashSet<String> = msg
                .queue_item_ids
                .iter()
                .map(|id| id.to_string())
                .collect();

            // Extract items to move
            let moved: Vec<_> = state
                .items
                .iter()
                .filter(|item| ids.contains(&item.id))
                .cloned()
                .collect();

            // Remove from current positions
            state.items.retain(|item| !ids.contains(&item.id));

            // Find insertion point and insert
            let insert_idx = msg
                .insert_after
                .and_then(|id| state.get_item_index(&id.to_string()))
                .map(|i| i + 1)
                .unwrap_or(0);

            for (i, item) in moved.into_iter().enumerate() {
                state.items.insert(insert_idx + i, item);
            }
            state.version = format!("{}.{}", major, minor);
            self.queue_store
                .update(&self.bridge_id, state)
                .await;
            debug!(
                "[{}] Reordered {} tracks to position {}",
                self.name,
                msg.queue_item_ids.len(),
                insert_idx
            );
        }
    }

    /// Handle device registration (provides JWT and renderer ID).
    async fn handle_device_registered(
        &mut self,
        device_uuid: [u8; 16],
        renderer_id: u64,
        api_jwt: String,
    ) {
        info!(
            "[{}] Device registered: {:02x?} -> renderer {}",
            self.name,
            &device_uuid[..4],
            renderer_id
        );

        // Create and store Qobuz API client for metadata fetching
        let client = QobuzClient::new(&api_jwt, &self.app_id);

        // Get user's public_id from Qobuz REST API
        match client.get_user().await {
            Ok(user) => {
                info!(
                    "[{}] Qobuz user: {} ({})",
                    self.name,
                    user.display_name.as_deref().unwrap_or("unknown"),
                    user.public_id
                );

                // Match with Sonos music service account
                match self
                    .player
                    .match_music_service_account(
                        &user.public_id,
                        "31",
                        "Qobuz",
                    )
                    .await
                {
                    Ok(account) => {
                        let sn = account.service_account_number();
                        info!(
                            "[{}] Matched Sonos account: sn={:?}",
                            self.name, sn
                        );
                        self.service_account_number = sn;
                    }
                    Err(e) => {
                        warn!(
                            "[{}] Failed to match Sonos account: {}",
                            self.name, e
                        );
                    }
                }
            }
            Err(e) => {
                warn!(
                    "[{}] Failed to get Qobuz user: {}",
                    self.name, e
                );
            }
        }

        // Store client for later use (metadata fetching)
        self.qobuz_client = Some(client);
    }
}
