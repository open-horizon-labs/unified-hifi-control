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
    bus: SharedBus,
}

impl ZoneAggregator {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            zones: Arc::new(RwLock::new(HashMap::new())),
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
                    if let Some(zone) = self.zones.write().await.get_mut(zone_id.as_str()) {
                        zone.zone_name = display_name;
                        zone.state = state.as_str().into();
                    }
                }

                BusEvent::ZoneRemoved { zone_id } => {
                    debug!("Zone removed: {}", zone_id);
                    self.zones.write().await.remove(zone_id.as_str());
                }

                BusEvent::NowPlayingChanged {
                    zone_id,
                    title,
                    artist,
                    album,
                    image_key,
                } => {
                    debug!("Now playing changed: {}", zone_id);
                    if let Some(zone) = self.zones.write().await.get_mut(zone_id.as_str()) {
                        // Preserve seek_position and duration from existing now_playing
                        let (seek_position, duration) = zone
                            .now_playing
                            .as_ref()
                            .map(|np| (np.seek_position, np.duration))
                            .unwrap_or((None, None));

                        zone.now_playing = Some(NowPlaying {
                            title: title.unwrap_or_default(),
                            artist: artist.unwrap_or_default(),
                            album: album.unwrap_or_default(),
                            image_key,
                            seek_position,
                            duration,
                            metadata: None,
                        });
                    }
                }

                BusEvent::VolumeChanged {
                    output_id,
                    value,
                    is_muted,
                } => {
                    debug!(
                        "Volume changed: {} = {} (muted: {})",
                        output_id, value, is_muted
                    );
                    // Find zone containing this output and update volume_control
                    let mut zones = self.zones.write().await;
                    for zone in zones.values_mut() {
                        // Match by volume_control.output_id (works for Roon where output_id != zone_id)
                        // Fall back to zone_id suffix match for LMS (output_id is player MAC)
                        let matches = zone
                            .volume_control
                            .as_ref()
                            .and_then(|vc| vc.output_id.as_ref())
                            .map(|oid| oid == &output_id)
                            .unwrap_or_else(|| {
                                // Fallback: check if zone_id ends with output_id (LMS style)
                                zone.zone_id.ends_with(&output_id)
                            });

                        if matches {
                            if let Some(ref mut vc) = zone.volume_control {
                                vc.value = value;
                                vc.is_muted = is_muted;
                            }
                            break;
                        }
                    }
                }

                BusEvent::SeekPositionChanged { zone_id, position } => {
                    debug!("Seek position changed: {} = {}", zone_id, position);
                    if let Some(zone) = self.zones.write().await.get_mut(zone_id.as_str()) {
                        if let Some(ref mut np) = zone.now_playing {
                            np.seek_position = Some(position as f64);
                        }
                    }
                }

                BusEvent::AdapterStopping { adapter, .. } => {
                    info!("Flushing zones for adapter: {}", adapter);
                    let prefix = format!("{}:", adapter);

                    // Remove all zones with this prefix
                    let mut zones = self.zones.write().await;

                    let zone_ids: Vec<String> = zones
                        .keys()
                        .filter(|k| k.starts_with(&prefix))
                        .cloned()
                        .collect();

                    for zone_id in &zone_ids {
                        zones.remove(zone_id);
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
        self.zones
            .read()
            .await
            .get(zone_id)
            .and_then(|z| z.now_playing.clone())
    }

    /// Get zone count
    pub async fn zone_count(&self) -> usize {
        self.zones.read().await.len()
    }
}
