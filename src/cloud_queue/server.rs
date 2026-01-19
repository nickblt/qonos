//! Cloud Queue HTTP server.

use axum::{Router, routing::{get, post}};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

use super::handlers;
use super::metadata_cache::MetadataCache;
use super::state::QueueStore;

/// Shared application state for cloud queue handlers.
#[derive(Clone)]
pub struct AppState {
    pub store: QueueStore,
    pub metadata_cache: Arc<MetadataCache>,
}

/// Cloud Queue HTTP server.
///
/// Serves the Sonos Cloud Queue API endpoints for all bridges.
#[derive(Clone)]
pub struct CloudQueueServer {
    state: AppState,
    port: u16,
}

impl CloudQueueServer {
    /// Create a new CloudQueueServer.
    pub fn new(port: u16) -> Self {
        Self {
            state: AppState {
                store: QueueStore::new(),
                metadata_cache: Arc::new(MetadataCache::new()),
            },
            port,
        }
    }

    /// Get the queue store for updating queue state.
    pub fn store(&self) -> &QueueStore {
        &self.state.store
    }

    /// Start the HTTP server.
    ///
    /// This spawns a background task and returns immediately.
    pub async fn start(&self) -> Result<(), std::io::Error> {
        let app = self.build_router();
        let addr = SocketAddr::from(([0, 0, 0, 0], self.port));

        let listener = TcpListener::bind(addr).await?;
        info!("Cloud Queue server listening on {}", addr);

        let app = app.clone();
        tokio::spawn(async move {
            if let Err(e) = axum::serve(listener, app).await {
                tracing::error!("Cloud Queue server error: {}", e);
            }
        });

        Ok(())
    }

    /// Build the axum router with all endpoints.
    fn build_router(&self) -> Router {
        Router::new()
            .route(
                "/queues/{bridge_id}/v2.3/context",
                get(handlers::get_context),
            )
            .route(
                "/queues/{bridge_id}/v2.3/itemWindow",
                get(handlers::get_item_window),
            )
            .route(
                "/queues/{bridge_id}/v2.3/version",
                get(handlers::get_version),
            )
            .route(
                "/queues/{bridge_id}/v2.3/timePlayed",
                post(handlers::post_time_played),
            )
            .with_state(self.state.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud_queue::state::QueueState;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::util::ServiceExt;

    #[tokio::test]
    async fn test_get_context_not_found() {
        let server = CloudQueueServer::new(9443);
        let app = server.build_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/queues/unknown/v2.3/context")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_get_context_success() {
        let server = CloudQueueServer::new(9443);
        let state = QueueState::new(8);
        server.store().update("test-bridge", state).await;

        let app = server.build_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/queues/test-bridge/v2.3/context")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_item_window_success() {
        let server = CloudQueueServer::new(9443);
        let tracks = vec![(123, 1), (456, 2), (789, 3)];
        let state = QueueState::from_tracks(&tracks, (1, 0), 8, None);
        server.store().update("test-bridge", state).await;

        let app = server.build_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/queues/test-bridge/v2.3/itemWindow?itemId=1&upBefore=0&upAfter=10")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn test_get_version_success() {
        let server = CloudQueueServer::new(9443);
        let state = QueueState::new(8);
        server.store().update("test-bridge", state).await;

        let app = server.build_router();

        let response = app
            .oneshot(
                Request::builder()
                    .uri("/queues/test-bridge/v2.3/version")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), StatusCode::OK);
    }
}
