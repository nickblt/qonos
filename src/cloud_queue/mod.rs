//! Cloud Queue HTTP server for Sonos.
//!
//! Implements the Sonos Cloud Queue protocol (v2.3) to serve queue data
//! to Sonos speakers. The server exposes endpoints that Sonos calls to
//! fetch queue metadata and track windows.

mod handlers;
mod metadata_cache;
mod server;
mod state;
mod types;

pub use server::CloudQueueServer;
pub use state::{cloud_queue_item, QueueState, QueueStore};
