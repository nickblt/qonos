//! Cloud Queue protocol types.
//!
//! These types match the Sonos Cloud Queue API v2.3 specification.
//! Track metadata uses the `sonos_websocket::Track` type for consistency.

use serde::{Deserialize, Serialize};
use sonos_websocket::{MusicObjectId, Track};

/// Create a Track with Qobuz MusicObjectId (no metadata).
pub fn qobuz_track(track_id: u64, service_account_number: u32) -> Track {
    Track {
        id: Some(MusicObjectId {
            object_id: format!("track:{}", track_id),
            service_id: Some("31".to_string()),
            account_id: Some(format!("sn_{}", service_account_number)),
        }),
        ..Default::default()
    }
}

/// A single item in the cloud queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CloudQueueItem {
    /// Unique ID for this queue item (string, sequential from "1")
    pub id: String,

    /// The track information (uses sonos_websocket::Track for consistency)
    pub track: Track,

    /// Qobuz track ID (not serialized, used for metadata lookup)
    #[serde(skip)]
    pub track_id: u64,

    /// Playback policies for this item
    #[serde(default)]
    pub policies: ItemPolicies,
}

/// Playback policies for a queue item.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemPolicies {
    // Empty for now - can add skip limits, seek restrictions, etc.
}

/// Response for GET /context endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextResponse {
    /// Context version string
    pub context_version: String,

    /// Queue version string
    pub queue_version: String,

    /// Container metadata
    pub container: ContainerInfo,

    /// Reporting configuration
    pub reports: ReportingConfig,

    /// Playback policies
    pub playback_policies: PlaybackPolicies,
}

/// Reporting configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportingConfig {
    /// Milliseconds after which to send update
    #[serde(default)]
    pub send_update_after_millis: u32,

    /// Whether to send playback actions
    #[serde(default = "default_true")]
    pub send_playback_actions: bool,
}

impl Default for ReportingConfig {
    fn default() -> Self {
        Self {
            send_update_after_millis: 0,
            send_playback_actions: true,
        }
    }
}

/// Playback policies for the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlaybackPolicies {
    #[serde(default = "default_true")]
    pub can_skip: bool,

    #[serde(default = "default_true")]
    pub limited_skips: bool,

    #[serde(default = "default_true")]
    pub can_skip_to_item: bool,

    #[serde(default = "default_true")]
    pub can_skip_back: bool,

    #[serde(default = "default_true")]
    pub can_seek: bool,

    #[serde(default = "default_true")]
    pub can_repeat: bool,

    #[serde(default = "default_true")]
    pub can_repeat_one: bool,

    #[serde(default = "default_true")]
    pub can_crossfade: bool,

    #[serde(default = "default_true")]
    pub can_shuffle: bool,

    #[serde(default)]
    pub show_n_next_tracks: u32,

    #[serde(default)]
    pub show_n_previous_tracks: u32,
}

impl Default for PlaybackPolicies {
    fn default() -> Self {
        Self {
            can_skip: true,
            limited_skips: true,
            can_skip_to_item: true,
            can_skip_back: true,
            can_seek: true,
            can_repeat: true,
            can_repeat_one: true,
            can_crossfade: true,
            can_shuffle: true,
            show_n_next_tracks: 0,
            show_n_previous_tracks: 0,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Container metadata in context response.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerInfo {
    /// Container name
    pub name: String,

    /// Container type
    #[serde(rename = "type")]
    pub container_type: String,

    /// Container ID
    pub id: ContainerId,

    /// Service information
    pub service: ServiceInfo,
}

/// Service information.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceInfo {
    /// Service name
    pub name: String,

    /// Service ID
    pub id: String,
}

/// Container identifier.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContainerId {
    /// Service ID (31 for Qobuz)
    pub service_id: String,

    /// Object ID for the container
    pub object_id: String,

    /// Account ID
    pub account_id: String,
}

/// Response for GET /itemWindow endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ItemWindowResponse {
    /// Whether the items include the beginning of the queue
    pub includes_beginning_of_queue: bool,

    /// Whether the items include the end of the queue
    pub includes_end_of_queue: bool,

    /// Context version string
    pub context_version: String,

    /// Current queue version
    pub queue_version: String,

    /// The requested items
    pub items: Vec<CloudQueueItem>,
}

/// Response for GET /version endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionResponse {
    /// Current queue version
    pub version: String,
}

/// Query parameters for GET /itemWindow.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ItemWindowQuery {
    /// The reason a player is requesting a new item window.
    /// Values like "refresh", "load", "queueCompleted", or multiple joined with "+".
    #[serde(default = "default_reason")]
    pub reason: String,

    /// The item ID to center the window on.
    /// If omitted or empty, returns the first item in the queue.
    pub item_id: Option<String>,

    /// Maximum number of tracks before the itemId to return.
    pub previous_window_size: Option<usize>,

    /// Maximum number of tracks after the itemId to return.
    pub upcoming_window_size: Option<usize>,

    /// The last cloud queue version cached by the player.
    pub queue_version: Option<String>,

    /// If true, indicates user performed a specific action (pressed play, skipped, etc).
    /// If false/omitted, the player requested the track automatically.
    #[serde(default)]
    pub is_explicit: bool,
}

fn default_reason() -> String {
    "refresh".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonos_websocket::{Album, Artist};

    #[test]
    fn test_qobuz_track_serialization() {
        let track = qobuz_track(363336060, 8);
        let json = serde_json::to_string(&track).unwrap();

        assert!(json.contains(r#""serviceId":"31""#));
        assert!(json.contains(r#""objectId":"track:363336060""#));
        assert!(json.contains(r#""accountId":"sn_8""#));
    }

    #[test]
    fn test_cloud_queue_item_serialization() {
        let item = CloudQueueItem {
            id: "1".to_string(),
            track: qobuz_track(363336060, 8),
            track_id: 363336060,
            policies: ItemPolicies::default(),
        };
        let json = serde_json::to_string(&item).unwrap();

        assert!(json.contains(r#""id":"1""#));
        assert!(json.contains(r#""track""#));
        // track_id should not be serialized
        assert!(!json.contains("track_id"));
        assert!(!json.contains("trackId"));
        assert!(json.contains(r#""objectId":"track:363336060""#));
    }

    #[test]
    fn test_track_with_metadata() {
        let track = Track {
            id: Some(MusicObjectId {
                object_id: "track:363336060".to_string(),
                service_id: Some("31".to_string()),
                account_id: Some("sn_8".to_string()),
            }),
            name: Some("Zebra".to_string()),
            artist: Some(Artist {
                name: "Beach House".to_string(),
                id: None,
            }),
            album: Some(Album {
                name: "Teen Dream".to_string(),
                artist: None,
                id: None,
            }),
            duration_millis: Some(290000),
            image_url: Some("https://example.com/cover.jpg".to_string()),
            content_type: Some("audio/flac".to_string()),
            ..Default::default()
        };
        let json = serde_json::to_string(&track).unwrap();

        assert!(json.contains(r#""name":"Zebra""#));
        assert!(json.contains(r#""durationMillis":290000"#));
        assert!(json.contains(r#""imageUrl":"https://example.com/cover.jpg""#));
        assert!(json.contains(r#""contentType":"audio/flac""#));
    }
}
