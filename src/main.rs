//! Qonos - Sonos to Qobuz Connect bridge.
//!
//! Exposes Sonos speaker groups as Qobuz Connect devices.

mod bridge;
mod cloud_queue;
mod sonos;

use qonos::qobuz_api;

use bridge::{CloudQueueConfig, SonosBridge};
use cloud_queue::CloudQueueServer;
use qonductor::{DeviceConfig, SessionManager};
use sonos::{GroupId, PlayerId, SonosSystem, SystemEvent};
use std::collections::HashMap;
use std::env;
use std::net::UdpSocket;
use std::time::Duration;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// Get the local IP address that can reach the given target IP.
///
/// Uses UDP socket trick to determine which local interface would be used.
fn get_local_ip_for(target_ip: &str) -> Option<String> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect(format!("{}:1443", target_ip)).ok()?;
    let local_addr = socket.local_addr().ok()?;
    Some(local_addr.ip().to_string())
}

const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(3);
const HTTP_PORT: u16 = 7864;
const DEFAULT_CLOUD_QUEUE_PORT: u16 = 9444;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize tracing
    // Use RUST_LOG env var, falling back to qonos=info if not set
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("qonos=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Read app_id from environment
    let app_id = env::var("QOBUZ_APP_ID").expect("QOBUZ_APP_ID environment variable required");
    info!("Starting qonos bridge");

    // Start Sonos system (discovers speakers via mDNS)
    info!("Discovering Sonos speakers...");
    let system = SonosSystem::start(DISCOVERY_TIMEOUT).await?;
    let mut system_events = system.events();

    // Wait for initial topology
    info!("Waiting for topology...");
    let initial_topology = loop {
        match system_events.recv().await {
            Ok(SystemEvent::TopologyUpdated(topology)) => {
                info!(
                    "Got topology: {} players, {} groups",
                    topology.players.len(),
                    topology.groups.len()
                );
                break topology;
            }
            Ok(_) => continue,
            Err(e) => {
                error!("Failed to receive topology: {}", e);
                return Ok(());
            }
        }
    };

    if initial_topology.groups.is_empty() {
        error!("No Sonos groups found!");
        return Ok(());
    }

    // Start qonductor session manager
    let mut manager = SessionManager::start(HTTP_PORT, &app_id).await?;
    info!("qonductor listening on port {}", HTTP_PORT);

    // Start cloud queue HTTP server
    let cloud_queue_port = env::var("QONOS_CLOUD_QUEUE_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(DEFAULT_CLOUD_QUEUE_PORT);
    let cloud_queue_server = CloudQueueServer::new(cloud_queue_port);
    cloud_queue_server.start().await?;

    // Determine local IP from first player's websocket URL
    let local_ip = initial_topology
        .players
        .iter()
        .filter_map(|p| p.websocket_url.as_ref())
        .next()
        .and_then(|url| {
            // Extract IP from websocket URL like "wss://192.168.1.100:1443/..."
            let ip = url
                .strip_prefix("wss://")
                .or_else(|| url.strip_prefix("ws://"))
                .and_then(|s| s.split(':').next());
            ip.and_then(get_local_ip_for)
        })
        .unwrap_or_else(|| "127.0.0.1".to_string());

    info!("Cloud queue base URL: http://{}:{}", local_ip, cloud_queue_port);

    let queue_store = cloud_queue_server.store().clone();
    let cloud_queue_config = CloudQueueConfig {
        base_url: format!("http://{}:{}", local_ip, cloud_queue_port),
    };

    // Track bridges by group_id, store coordinator_id to detect changes
    let mut bridges: HashMap<GroupId, (PlayerId, SonosBridge)> = HashMap::new();

    // Process initial topology
    for group in &initial_topology.groups {
        info!("Creating bridge for group: {} ({})", group.name, group.id);

        match system.get_coordinator(group).await {
            Some(player) => {
                let config = DeviceConfig::new(&group.name);
                let device_uuid = config.device_uuid;

                match manager.add_device(config).await {
                    Ok(session) => {
                        match SonosBridge::start(
                            player,
                            group.id.clone(),
                            device_uuid,
                            session,
                            app_id.clone(),
                            queue_store.clone(),
                            cloud_queue_config.clone(),
                        )
                        .await
                        {
                            Ok(bridge) => {
                                bridges
                                    .insert(group.id.clone(), (group.coordinator_id.clone(), bridge));
                            }
                            Err(e) => {
                                warn!("Failed to start bridge for {}: {}", group.name, e);
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to add device {}: {}", group.name, e);
                    }
                }
            }
            None => {
                warn!("Coordinator {} not found for group {}", group.coordinator_id, group.id);
            }
        }
    }

    // Main event loop
    loop {
        tokio::select! {
            // Handle topology changes from Sonos
            event = system_events.recv() => {
                match event {
                    Ok(SystemEvent::TopologyUpdated(topology)) => {
                        info!(
                            "Topology updated: {} players, {} groups",
                            topology.players.len(),
                            topology.groups.len()
                        );

                        // Build map of current groups with their coordinators
                        let current: HashMap<_, _> = topology.groups.iter()
                            .map(|g| (g.id.clone(), g.coordinator_id.clone()))
                            .collect();

                        // Find stale or changed bridges (removed groups or coordinator changed)
                        let to_remove: Vec<_> = bridges.iter()
                            .filter(|(id, (coord, _))| {
                                current.get(*id).map(|c| c != coord).unwrap_or(true)
                            })
                            .map(|(id, _)| id.clone())
                            .collect();

                        // Remove stale bridges
                        for id in to_remove {
                            if let Some((_, bridge)) = bridges.remove(&id) {
                                info!("Removing bridge for group: {}", id);
                                if let Err(e) = manager.remove_device(&bridge.device_uuid()).await {
                                    warn!("Failed to remove device: {}", e);
                                }
                                bridge.shutdown();
                            }
                        }

                        // Add new bridges
                        for group in &topology.groups {
                            if bridges.contains_key(&group.id) {
                                continue;
                            }

                            info!("Creating bridge for group: {} ({})", group.name, group.id);

                            if let Some(player) = system.get_coordinator(group).await {
                                let config = DeviceConfig::new(&group.name);
                                let device_uuid = config.device_uuid;

                                match manager.add_device(config).await {
                                    Ok(session) => {
                                        match SonosBridge::start(
                                            player,
                                            group.id.clone(),
                                            device_uuid,
                                            session,
                                            app_id.clone(),
                                            queue_store.clone(),
                                            cloud_queue_config.clone(),
                                        )
                                        .await
                                        {
                                            Ok(bridge) => {
                                                bridges.insert(
                                                    group.id.clone(),
                                                    (group.coordinator_id.clone(), bridge),
                                                );
                                            }
                                            Err(e) => {
                                                warn!("Failed to start bridge for {}: {}", group.name, e);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to add device {}: {}", group.name, e);
                                    }
                                }
                            } else {
                                warn!(
                                    "Coordinator {} not found for group {}",
                                    group.coordinator_id, group.id
                                );
                            }
                        }
                    }
                    Ok(SystemEvent::PlayerAdded(player)) => {
                        info!("Player added: {} ({})", player.name, player.id);
                    }
                    Ok(SystemEvent::PlayerRemoved(id)) => {
                        info!("Player removed: {}", id);
                    }
                    Err(e) => {
                        warn!("System event error: {}", e);
                    }
                }
            }

            // Run qonductor manager
            result = manager.run() => {
                if let Err(e) = result {
                    warn!("Manager error: {}", e);
                }
                break;
            }

            // Handle shutdown
            _ = tokio::signal::ctrl_c() => {
                info!("Shutting down...");
                break;
            }
        }
    }

    // Cleanup bridges
    for (_, (_, bridge)) in bridges {
        bridge.shutdown();
    }

    Ok(())
}
