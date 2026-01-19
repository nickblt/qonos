//! Shared queue state management.
//!
//! Maintains queue state for each bridge, allowing the HTTP server
//! to serve queue data to Sonos speakers.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use super::types::{CloudQueueItem, ItemPolicies, qobuz_track};
use crate::qobuz_api::QobuzClient;

/// Create a CloudQueueItem from track_id and queue_item_id.
pub fn cloud_queue_item(track_id: u64, queue_item_id: u64, service_account_number: u32) -> CloudQueueItem {
    CloudQueueItem {
        id: queue_item_id.to_string(),
        track: qobuz_track(track_id, service_account_number),
        track_id,
        policies: ItemPolicies::default(),
    }
}

/// Queue state for a single bridge.
#[derive(Debug, Clone)]
pub struct QueueState {
    /// The queue items in order
    pub items: Vec<CloudQueueItem>,

    /// Queue version as "major.minor" string
    pub version: String,

    /// Service account number for this user's Qobuz account on Sonos
    pub service_account_number: u32,

    /// Qobuz API client for fetching track metadata (lazy fetch)
    pub client: Option<QobuzClient>,
}

impl QueueState {
    /// Create a new empty queue state.
    #[allow(dead_code)]
    pub fn new(service_account_number: u32) -> Self {
        Self {
            items: Vec::new(),
            version: "0.0".to_string(),
            service_account_number,
            client: None,
        }
    }

    /// Create queue state from qonductor tracks.
    pub fn from_tracks(
        tracks: &[(u64, u64)], // (track_id, queue_item_id)
        version: (u64, i32),
        service_account_number: u32,
        client: Option<QobuzClient>,
    ) -> Self {
        let items = tracks
            .iter()
            .map(|(track_id, queue_item_id)| CloudQueueItem {
                id: queue_item_id.to_string(),
                track: qobuz_track(*track_id, service_account_number),
                track_id: *track_id,
                policies: ItemPolicies::default(),
            })
            .collect();

        Self {
            items,
            version: format!("{}.{}", version.0, version.1),
            service_account_number,
            client,
        }
    }

    /// Get the index of an item by its ID.
    pub fn get_item_index(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.id == id)
    }

    /// Get a window of items centered on a specific item.
    #[allow(dead_code)]
    pub fn get_window(&self, item_id: Option<&str>, up_before: usize, up_after: usize) -> Vec<CloudQueueItem> {
        let (items, _, _) = self.get_window_with_bounds(item_id, up_before, up_after);
        items
    }

    /// Get a window of items with boundary indicators.
    /// Returns (items, includes_beginning, includes_end).
    pub fn get_window_with_bounds(
        &self,
        item_id: Option<&str>,
        up_before: usize,
        up_after: usize,
    ) -> (Vec<CloudQueueItem>, bool, bool) {
        if self.items.is_empty() {
            return (Vec::new(), true, true);
        }

        let center_idx = item_id
            .and_then(|id| self.get_item_index(id))
            .unwrap_or(0);

        let start = center_idx.saturating_sub(up_before);
        let end = (center_idx + up_after + 1).min(self.items.len());

        let includes_beginning = start == 0;
        let includes_end = end == self.items.len();

        (self.items[start..end].to_vec(), includes_beginning, includes_end)
    }
}

/// Thread-safe store for queue states, keyed by bridge ID.
#[derive(Debug, Clone, Default)]
pub struct QueueStore {
    inner: Arc<RwLock<HashMap<String, QueueState>>>,
}

impl QueueStore {
    /// Create a new empty queue store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Update the queue for a specific bridge.
    pub async fn update(&self, bridge_id: &str, state: QueueState) {
        self.inner.write().await.insert(bridge_id.to_string(), state);
    }

    /// Get the queue state for a specific bridge.
    pub async fn get(&self, bridge_id: &str) -> Option<QueueState> {
        self.inner.read().await.get(bridge_id).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_queue_state_from_tracks() {
        let tracks = vec![(123456, 1), (789012, 2), (345678, 3)];
        let state = QueueState::from_tracks(&tracks, (1, 5), 8, None);

        assert_eq!(state.items.len(), 3);
        assert_eq!(state.version, "1.5");
        assert_eq!(state.items[0].id, "1");
        assert_eq!(state.items[1].id, "2");
        assert_eq!(state.items[2].id, "3");
    }

    #[test]
    fn test_get_window_empty() {
        let state = QueueState::new(8);
        let window = state.get_window(None, 5, 5);
        assert!(window.is_empty());
    }

    #[test]
    fn test_get_window_from_start() {
        let tracks = vec![(1, 1), (2, 2), (3, 3), (4, 4), (5, 5)];
        let state = QueueState::from_tracks(&tracks, (1, 0), 8, None);

        let window = state.get_window(Some("1"), 2, 2);
        assert_eq!(window.len(), 3); // items 1, 2, 3 (can't go before 1)
        assert_eq!(window[0].id, "1");
    }

    #[test]
    fn test_get_window_centered() {
        let tracks = vec![(1, 1), (2, 2), (3, 3), (4, 4), (5, 5)];
        let state = QueueState::from_tracks(&tracks, (1, 0), 8, None);

        let window = state.get_window(Some("3"), 1, 1);
        assert_eq!(window.len(), 3); // items 2, 3, 4
        assert_eq!(window[0].id, "2");
        assert_eq!(window[1].id, "3");
        assert_eq!(window[2].id, "4");
    }

    #[test]
    fn test_get_window_at_end() {
        let tracks = vec![(1, 1), (2, 2), (3, 3), (4, 4), (5, 5)];
        let state = QueueState::from_tracks(&tracks, (1, 0), 8, None);

        let window = state.get_window(Some("5"), 2, 2);
        assert_eq!(window.len(), 3); // items 3, 4, 5 (can't go after 5)
        assert_eq!(window[2].id, "5");
    }

    #[tokio::test]
    async fn test_queue_store_update_and_get() {
        let store = QueueStore::new();
        let state = QueueState::new(8);

        // Initially empty
        assert!(store.get("bridge1").await.is_none());

        // Add
        store.update("bridge1", state.clone()).await;
        assert!(store.get("bridge1").await.is_some());

        // Update overwrites
        let state2 = QueueState::from_tracks(&[(1, 1)], (1, 0), 8, None);
        store.update("bridge1", state2).await;
        let retrieved = store.get("bridge1").await.unwrap();
        assert_eq!(retrieved.items.len(), 1);
    }
}
