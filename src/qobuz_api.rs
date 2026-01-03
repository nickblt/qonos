//! Qobuz REST API client.
//!
//! Fetches user info from Qobuz to get the `publicId` needed for Sonos account matching.

use reqwest::Client;
use serde::Deserialize;
use thiserror::Error;
use tracing::debug;

/// Qobuz API base URL.
const API_BASE: &str = "https://www.qobuz.com/api.json/0.2";

/// Errors from Qobuz API calls.
#[derive(Debug, Error)]
pub enum Error {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),

    #[error("Qobuz API error: {0}")]
    Api(String),

    #[error("Failed to parse response: {0}")]
    Parse(String),
}

pub type Result<T> = std::result::Result<T, Error>;

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

/// Get user info from Qobuz REST API.
///
/// Uses the JWT from QConnect to authenticate and fetch the user's `publicId`,
/// which is needed for `match_music_service_account` on Sonos.
pub async fn get_user(jwt: &str, app_id: &str) -> Result<QobuzUser> {
    let url = format!("{API_BASE}/user/get");

    let client = Client::new();
    let response = client
        .get(&url)
        .header("Authorization", format!("Bearer {jwt}"))
        .header("X-App-Id", app_id)
        .send()
        .await?;

    let status = response.status();
    let text = response.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(Error::Api(format!("{status}: {text}")));
    }

    debug!(response = %text, "user/get response");

    let data: UserResponse =
        serde_json::from_str(&text).map_err(|e| Error::Parse(e.to_string()))?;

    Ok(QobuzUser {
        id: data.id,
        public_id: data.public_id,
        display_name: data.display_name,
    })
}
