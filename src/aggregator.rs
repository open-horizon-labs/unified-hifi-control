//! ZoneAggregator - Single source of truth for zone state

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::bus::{BusEvent, NowPlaying, SharedBus, Zone};

/// ZoneAggregator maintains unified zone state from all adapters.
/// - Subscribes to bus events
/// - Maintains HashMap of zones by zone_id
/// - Flushes zones when adapter stops
/// - Provides query interface for API layer
pub struct ZoneAggregator {
    zones: Arc<RwLock<HashMap<String, Zone>>>,
    now_playing: Arc<RwLock<HashMap<String, NowPlaying>>>,
    bus: SharedBus,
}

impl ZoneAggregator {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            zones: Arc::new(RwLock::new(HashMap::new())),
            now_playing: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Start the aggregator's event processing loop
    /// Should be spawned as a task
    pub async fn run(&self) {
        let mut rx = self.bus.subscribe();

        info!("ZoneAggregator started");

        while let Ok(event) = rx.recv().await {
            match event {
                BusEvent::ZoneDiscovered { zone } => {
                    debug!("Zone discovered: {}", zone.zone_id);
                    self.zones.write().await.insert(zone.zone_id.clone(), zone);
                }

                BusEvent::ZoneUpdated {
                    zone_id,
                    display_name,
                    state,
                } => {
                    debug!("Zone updated: {}", zone_id);
                    if let Some(zone) = self.zones.write().await.get_mut(&zone_id) {
                        zone.zone_name = display_name;
                        zone.state = state.as_str().into();
                    }
                }

                BusEvent::ZoneRemoved { zone_id } => {
                    debug!("Zone removed: {}", zone_id);
                    self.zones.write().await.remove(&zone_id);
                    self.now_playing.write().await.remove(&zone_id);
                }

                BusEvent::NowPlayingChanged {
                    zone_id,
                    title,
                    artist,
                    album,
                    image_key,
                } => {
                    debug!("Now playing changed: {}", zone_id);
                    let np = NowPlaying {
                        title: title.unwrap_or_default(),
                        artist: artist.unwrap_or_default(),
                        album: album.unwrap_or_default(),
                        image_key,
                        seek_position: None,
                        duration: None,
                        metadata: None,
                    };
                    self.now_playing.write().await.insert(zone_id, np);
                }

                BusEvent::AdapterStopping { adapter, .. } => {
                    info!("Flushing zones for adapter: {}", adapter);
                    let prefix = format!("{}:", adapter);

                    // Remove all zones with this prefix
                    let mut zones = self.zones.write().await;
                    let mut np = self.now_playing.write().await;

                    let zone_ids: Vec<String> = zones
                        .keys()
                        .filter(|k| k.starts_with(&prefix))
                        .cloned()
                        .collect();

                    for zone_id in &zone_ids {
                        zones.remove(zone_id);
                        np.remove(zone_id);
                    }

                    // Publish flush acknowledgment
                    self.bus.publish(BusEvent::ZonesFlushed {
                        adapter: adapter.clone(),
                        zone_ids,
                    });
                }

                BusEvent::ShuttingDown { .. } => {
                    info!("ZoneAggregator shutting down");
                    break;
                }

                _ => {
                    // Ignore other events
                }
            }
        }

        info!("ZoneAggregator stopped");
    }

    /// Get all zones
    pub async fn get_zones(&self) -> Vec<Zone> {
        self.zones.read().await.values().cloned().collect()
    }

    /// Get zones for a specific adapter
    pub async fn get_zones_by_adapter(&self, adapter: &str) -> Vec<Zone> {
        let prefix = format!("{}:", adapter);
        self.zones
            .read()
            .await
            .values()
            .filter(|z| z.zone_id.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// Get a specific zone
    pub async fn get_zone(&self, zone_id: &str) -> Option<Zone> {
        self.zones.read().await.get(zone_id).cloned()
    }

    /// Get now playing for a zone
    pub async fn get_now_playing(&self, zone_id: &str) -> Option<NowPlaying> {
        self.now_playing.read().await.get(zone_id).cloned()
    }

    /// Get zone count
    pub async fn zone_count(&self) -> usize {
        self.zones.read().await.len()
    }
}
