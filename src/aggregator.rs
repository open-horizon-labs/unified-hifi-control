//! ZoneAggregator - Single source of truth for zone state

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, info};

use anyhow::{anyhow, Result};

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

    /// Spawn the event loop and wait until its bus receiver exists.
    ///
    /// A `broadcast` sender drops events when no receiver is attached. Startup
    /// must therefore await this barrier before any adapter can publish its
    /// initial `ZoneDiscovered` snapshot.
    pub async fn start(self: Arc<Self>) -> Result<()> {
        let (ready_tx, ready_rx) = oneshot::channel();
        tokio::spawn(async move {
            self.run_with_ready(Some(ready_tx)).await;
        });
        ready_rx
            .await
            .map_err(|_| anyhow!("ZoneAggregator task exited before subscribing to the bus"))
    }

    /// Start the aggregator's event processing loop.
    ///
    /// Kept for existing test harnesses that deliberately own the task. New
    /// production startup must use [`Self::start`] so publication cannot race
    /// the subscription.
    pub async fn run(&self) {
        self.run_with_ready(None).await;
    }

    async fn run_with_ready(&self, ready: Option<oneshot::Sender<()>>) {
        let mut rx = self.bus.subscribe();

        if let Some(ready) = ready {
            let _ = ready.send(());
        }

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
                    // All adapters must use prefixed output_ids (e.g., "lms:xx:xx:xx", "roon:output-id")
                    // The lint test `bus_events_use_prefixed_output_ids` enforces this.
                    let mut zones = self.zones.write().await;
                    for zone in zones.values_mut() {
                        let matches = zone
                            .volume_control
                            .as_ref()
                            .and_then(|vc| vc.output_id.as_ref())
                            .map(|oid| oid == &output_id)
                            .unwrap_or(false);

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::create_bus;

    #[tokio::test]
    async fn start_returns_only_after_the_bus_subscription_exists() {
        let bus = create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));

        assert!(aggregator.start().await.is_ok());

        assert_eq!(
            bus.subscriber_count(),
            1,
            "a producer may publish as soon as start() returns"
        );
        bus.publish(BusEvent::ShuttingDown { reason: None });
    }
}
