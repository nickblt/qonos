//! HTTP request handlers for the Cloud Queue API.

use axum::{
    Json,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use sonos_websocket::{Album, Artist, Track};
use tracing::{info, warn, debug};

use super::server::AppState;
use super::types::{
    ContainerId, ContainerInfo, ContextResponse, ItemWindowQuery, ItemWindowResponse,
    PlaybackPolicies, ReportingConfig, ServiceInfo, VersionResponse,
};
use crate::qobuz_api::TrackMetadata;

/// Query parameters for GET /context.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct ContextQuery {
    pub queue_version: Option<String>,
    pub context_version: Option<String>,
}

/// GET /queues/{bridge_id}/v2.3/context
///
/// Returns container metadata and current queue version.
pub async fn get_context(
    State(state): State<AppState>,
    Path(bridge_id): Path<String>,
    Query(query): Query<ContextQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    info!("GET /context for bridge {}", bridge_id);
    debug!("  Query: {:?}", query);
    debug!("  Headers: {:?}", headers);

    match state.store.get(&bridge_id).await {
        Some(state) => {
            let response = ContextResponse {
                context_version: "1".to_string(),
                queue_version: state.version.clone(),
                container: ContainerInfo {
                    name: "Qobuz Queue".to_string(),
                    container_type: "playlist".to_string(),
                    id: ContainerId {
                        service_id: "31".to_string(),
                        object_id: format!("queue:{}", bridge_id),
                        account_id: format!("sn_{}", state.service_account_number),
                    },
                    service: ServiceInfo {
                        name: "Qobuz".to_string(),
                        id: "31".to_string(),
                    },
                },
                reports: ReportingConfig {
                    send_update_after_millis: 0,
                    send_playback_actions: false,
                },
                playback_policies: PlaybackPolicies::default(),
            };
            info!("  -> returning context with version {}", state.version);
            debug!("  Response: {:?}", response);
            (StatusCode::OK, Json(response)).into_response()
        }
        None => {
            warn!("  -> No queue found for bridge {}", bridge_id);
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// GET /queues/{bridge_id}/v2.3/itemWindow
///
/// Returns a window of queue items centered on a specific item.
/// Implements lazy-fetch: fetches metadata from Qobuz API for tracks not in cache.
pub async fn get_item_window(
    State(state): State<AppState>,
    Path(bridge_id): Path<String>,
    Query(query): Query<ItemWindowQuery>,
    headers: HeaderMap,
) -> impl IntoResponse {
    info!(
        "GET /itemWindow for bridge {} (reason={}, itemId={:?}, prev={:?}, upcoming={:?}, explicit={})",
        bridge_id, query.reason, query.item_id, query.previous_window_size, query.upcoming_window_size, query.is_explicit
    );
    debug!("  Query: {:?}", query);
    debug!("  Headers: {:?}", headers);

    match state.store.get(&bridge_id).await {
        Some(queue_state) => {
            let previous_size = query.previous_window_size.unwrap_or(0);
            let upcoming_size = query.upcoming_window_size.unwrap_or(10);

            let (mut items, includes_beginning, includes_end) =
                queue_state.get_window_with_bounds(query.item_id.as_deref(), previous_size, upcoming_size);

            // Extract track IDs from items
            let track_ids: Vec<u64> = items.iter().map(|item| item.track_id).collect();

            if !track_ids.is_empty() {
                // Check cache for missing metadata
                let missing_ids = state.metadata_cache.missing(&track_ids).await;

                // Fetch missing from Qobuz API if we have a client
                if !missing_ids.is_empty() {
                    if let Some(client) = &queue_state.client {
                        debug!("  Fetching metadata for {} tracks", missing_ids.len());
                        match client.get_tracks(&missing_ids).await {
                            Ok(metadata) => {
                                debug!("  Fetched {} track metadata", metadata.len());
                                state.metadata_cache.insert_many(metadata).await;
                            }
                            Err(e) => {
                                warn!("  Failed to fetch track metadata: {}", e);
                            }
                        }
                    } else {
                        debug!("  No Qobuz client available, skipping metadata fetch");
                    }
                }

                // Apply cached metadata to items
                let cached = state.metadata_cache.get_many(&track_ids).await;
                for item in &mut items {
                    if let Some(metadata) = cached.get(&item.track_id) {
                        apply_metadata(&mut item.track, metadata);
                    }
                }
            }

            let response = ItemWindowResponse {
                includes_beginning_of_queue: includes_beginning,
                includes_end_of_queue: includes_end,
                context_version: "1".to_string(),
                queue_version: queue_state.version.clone(),
                items,
            };
            info!("  -> returning {} items (version {})", response.items.len(), queue_state.version);
            debug!("  Response: {}", serde_json::to_string_pretty(&response).unwrap_or_default());
            (StatusCode::OK, Json(response)).into_response()
        }
        None => {
            warn!("  -> No queue found for bridge {}", bridge_id);
            StatusCode::NOT_FOUND.into_response()
        }
    }
}

/// Apply Qobuz metadata to a Track.
fn apply_metadata(track: &mut Track, metadata: &TrackMetadata) {
    track.name = Some(metadata.title.clone());
    track.artist = Some(Artist {
        name: metadata.artist_name.clone(),
        id: None,
    });
    track.album = Some(Album {
        name: metadata.album_title.clone(),
        artist: None,
        id: None,
    });
    track.duration_millis = Some((metadata.duration_secs * 1000) as i32);
    track.image_url = metadata.album_image_url.clone();
    track.content_type = Some("audio/flac".to_string());
}

/// POST /queues/{bridge_id}/v2.3/timePlayed
///
/// Receives playback action reports from Sonos.
pub async fn post_time_played(
    Path(bridge_id): Path<String>,
    Json(body): Json<serde_json::Value>,
) -> impl IntoResponse {
    info!("POST /timePlayed for bridge {}: {}", bridge_id, body);

    StatusCode::OK
}

/// GET /queues/{bridge_id}/v2.3/version
///
/// Returns just the current queue version.
pub async fn get_version(
    State(state): State<AppState>,
    Path(bridge_id): Path<String>,
) -> impl IntoResponse {
    info!("GET /version for bridge {}", bridge_id);

    match state.store.get(&bridge_id).await {
        Some(queue_state) => {
            info!("  -> returning version {}", queue_state.version);
            let response = VersionResponse {
                version: queue_state.version.clone(),
            };
            (StatusCode::OK, Json(response)).into_response()
        }
        None => {
            warn!("  -> No queue found for bridge {}", bridge_id);
            StatusCode::NOT_FOUND.into_response()
        }
    }
}
