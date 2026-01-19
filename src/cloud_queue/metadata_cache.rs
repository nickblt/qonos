//! Cache for track metadata fetched from Qobuz API.

use std::collections::HashMap;
use tokio::sync::RwLock;

use crate::qobuz_api::TrackMetadata;

/// Cache for track metadata fetched from Qobuz API.
///
/// Shared across all bridges to avoid duplicate fetches.
/// Thread-safe via interior mutability with `RwLock`.
#[derive(Debug, Default)]
pub struct MetadataCache {
    inner: RwLock<HashMap<u64, TrackMetadata>>,
}

impl MetadataCache {
    /// Create a new empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Get metadata for a single track.
    #[allow(dead_code)]
    pub async fn get(&self, track_id: u64) -> Option<TrackMetadata> {
        self.inner.read().await.get(&track_id).cloned()
    }

    /// Get metadata for multiple tracks.
    ///
    /// Returns a map of track_id -> metadata for all tracks found in cache.
    pub async fn get_many(&self, track_ids: &[u64]) -> HashMap<u64, TrackMetadata> {
        let cache = self.inner.read().await;
        track_ids
            .iter()
            .filter_map(|id| cache.get(id).map(|m| (*id, m.clone())))
            .collect()
    }

    /// Insert metadata for a single track.
    #[allow(dead_code)]
    pub async fn insert(&self, metadata: TrackMetadata) {
        self.inner.write().await.insert(metadata.id, metadata);
    }

    /// Insert metadata for multiple tracks.
    pub async fn insert_many(&self, metadata: Vec<TrackMetadata>) {
        let mut cache = self.inner.write().await;
        for m in metadata {
            cache.insert(m.id, m);
        }
    }

    /// Find track IDs that are not in the cache.
    pub async fn missing(&self, track_ids: &[u64]) -> Vec<u64> {
        let cache = self.inner.read().await;
        track_ids
            .iter()
            .filter(|id| !cache.contains_key(id))
            .copied()
            .collect()
    }

    /// Get the number of cached entries.
    #[allow(dead_code)]
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_metadata(id: u64) -> TrackMetadata {
        TrackMetadata {
            id,
            title: format!("Track {id}"),
            duration_secs: 180,
            artist_name: "Artist".to_string(),
            album_title: "Album".to_string(),
            album_image_url: None,
        }
    }

    #[tokio::test]
    async fn test_insert_and_get() {
        let cache = MetadataCache::new();
        let metadata = sample_metadata(123);

        cache.insert(metadata.clone()).await;

        let retrieved = cache.get(123).await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().title, "Track 123");
    }

    #[tokio::test]
    async fn test_get_missing() {
        let cache = MetadataCache::new();
        assert!(cache.get(999).await.is_none());
    }

    #[tokio::test]
    async fn test_insert_many_and_get_many() {
        let cache = MetadataCache::new();
        let metadata = vec![sample_metadata(1), sample_metadata(2), sample_metadata(3)];

        cache.insert_many(metadata).await;

        let retrieved = cache.get_many(&[1, 2, 4]).await;
        assert_eq!(retrieved.len(), 2);
        assert!(retrieved.contains_key(&1));
        assert!(retrieved.contains_key(&2));
        assert!(!retrieved.contains_key(&4));
    }

    #[tokio::test]
    async fn test_missing() {
        let cache = MetadataCache::new();
        cache.insert_many(vec![sample_metadata(1), sample_metadata(3)]).await;

        let missing = cache.missing(&[1, 2, 3, 4]).await;
        assert_eq!(missing, vec![2, 4]);
    }
}
