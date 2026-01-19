//! Qobuz REST API client.
//!
//! Fetches user info and track metadata from the Qobuz API.

use reqwest::{
    Client, Response,
    header::{HeaderMap, HeaderValue},
};
use serde::{Deserialize, de::DeserializeOwned};
use thiserror::Error;
use tracing::warn;

/// Qobuz API base URL.
const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";

/// Errors from Qobuz API calls.
#[derive(Debug, Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Qobuz API error: {0}")]
    Api(String),
}

pub type Result<T> = std::result::Result<T, Error>;

/// Check response status and deserialize JSON body.
async fn json_response<T: DeserializeOwned>(response: Response) -> Result<T> {
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(Error::Api(format!("{status}: {text}")));
    }
    Ok(response.json().await?)
}

/// Qobuz REST API client.
#[derive(Debug, Clone)]
pub struct QobuzClient {
    client: Client,
}

impl QobuzClient {
    /// Create a new Qobuz API client.
    pub fn new(jwt: impl Into<String>, app_id: impl Into<String>) -> Self {
        let mut headers = HeaderMap::new();
        headers.insert(
            "Authorization",
            HeaderValue::from_str(&format!("Bearer {}", jwt.into())).unwrap(),
        );
        headers.insert("X-App-Id", HeaderValue::from_str(&app_id.into()).unwrap());

        let client = Client::builder()
            .default_headers(headers)
            .build()
            .expect("failed to build HTTP client");

        Self { client }
    }

    /// Get user info from Qobuz REST API.
    ///
    /// Fetches the user's `publicId`, needed for `match_music_service_account` on Sonos.
    pub async fn get_user(&self) -> Result<QobuzUser> {
        let response = self.client.get(format!("{API_BASE}/user/get")).send().await?;
        let data: UserResponse = json_response(response).await?;

        Ok(QobuzUser {
            id: data.id,
            public_id: data.public_id,
            display_name: data.display_name,
        })
    }

    /// Fetch metadata for multiple tracks in a single request.
    ///
    /// Uses `POST /track/getList` which is more efficient than individual requests.
    /// Returns partial results on failure (graceful degradation).
    pub async fn get_tracks(&self, track_ids: &[u64]) -> Result<Vec<TrackMetadata>> {
        if track_ids.is_empty() {
            return Ok(Vec::new());
        }

        let body = serde_json::json!({ "tracks_id": track_ids });
        let response = self
            .client
            .post(format!("{API_BASE}/track/getList"))
            .body(body.to_string())
            .send()
            .await?;

        let data: TrackListResponse = json_response(response).await?;

        // Convert to TrackMetadata, filtering out tracks with missing required fields
        let mut results = Vec::with_capacity(data.tracks.items.len());
        for item in data.tracks.items {
            let Some(performer) = &item.performer else {
                warn!(track_id = item.id, "Track missing performer, skipping");
                continue;
            };
            let Some(album) = &item.album else {
                warn!(track_id = item.id, "Track missing album, skipping");
                continue;
            };

            results.push(TrackMetadata {
                id: item.id,
                title: item.title,
                duration_secs: item.duration,
                artist_name: performer.name.clone(),
                album_title: album.title.clone(),
                album_image_url: album.image.as_ref().and_then(|i| i.large.clone()),
            });
        }

        Ok(results)
    }
}

/// Qobuz user info from REST API.
#[derive(Debug, Clone)]
pub struct QobuzUser {
    /// Numeric user ID.
    pub id: u64,
    /// Public ID (e.g., `qobuz:user:HwyJtEEf1kazy`).
    pub public_id: String,
    /// Display name.
    pub display_name: Option<String>,
}

/// API response structure.
#[derive(Debug, Deserialize)]
struct UserResponse {
    id: u64,
    #[serde(rename = "publicId")]
    public_id: String,
    display_name: Option<String>,
}

// ============================================================================
// Track Metadata
// ============================================================================

/// Track metadata from Qobuz API.
#[derive(Debug, Clone)]
pub struct TrackMetadata {
    /// Qobuz track ID.
    pub id: u64,
    pub title: String,
    /// Duration in seconds.
    pub duration_secs: u32,
    pub artist_name: String,
    pub album_title: String,
    /// Album cover image URL (large size).
    pub album_image_url: Option<String>,
}

/// Response from POST /track/getList.
#[derive(Debug, Deserialize)]
struct TrackListResponse {
    tracks: TracksContainer,
}

#[derive(Debug, Deserialize)]
struct TracksContainer {
    items: Vec<TrackItem>,
}

#[derive(Debug, Deserialize)]
struct TrackItem {
    id: u64,
    title: String,
    #[serde(default)]
    duration: u32,
    performer: Option<Performer>,
    album: Option<Album>,
}

#[derive(Debug, Deserialize)]
struct Performer {
    name: String,
}

#[derive(Debug, Deserialize)]
struct Album {
    title: String,
    image: Option<AlbumImage>,
}

#[derive(Debug, Deserialize)]
struct AlbumImage {
    large: Option<String>,
}
