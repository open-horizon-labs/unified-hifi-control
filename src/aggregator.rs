//! ZoneAggregator - Single source of truth for zone state

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::bus::{BusEvent, NowPlaying, ProviderAccount, SharedBus, Zone};
use crate::mcp::observation_history::PlaybackObservationHistory;

fn observation_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// ZoneAggregator maintains unified zone state from all adapters.
/// - Subscribes to bus events
/// - Maintains HashMap of zones by zone_id
/// - Flushes zones when adapter stops
/// - Provides query interface for API layer
pub struct ZoneAggregator {
    zones: Arc<RwLock<HashMap<String, Zone>>>,
    provider_accounts: Arc<RwLock<HashMap<String, ProviderAccount>>>,
    adapter_errors: Arc<RwLock<HashMap<String, String>>>,
    observed_playback: PlaybackObservationHistory,
    bus: SharedBus,
}

impl ZoneAggregator {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            zones: Arc::new(RwLock::new(HashMap::new())),
            provider_accounts: Arc::new(RwLock::new(HashMap::new())),
            adapter_errors: Arc::new(RwLock::new(HashMap::new())),
            observed_playback: PlaybackObservationHistory::from_config(),
            bus,
        }
    }

    pub fn new_with_observation_history(
        bus: SharedBus,
        observed_playback: PlaybackObservationHistory,
    ) -> Self {
        Self {
            zones: Arc::new(RwLock::new(HashMap::new())),
            provider_accounts: Arc::new(RwLock::new(HashMap::new())),
            adapter_errors: Arc::new(RwLock::new(HashMap::new())),
            observed_playback,
            bus,
        }
    }

    pub async fn observed_playback_history(
        &self,
        zone_id: &str,
        limit: usize,
    ) -> Vec<crate::mcp::observation_history::PlaybackObservation> {
        self.observed_playback.recent(zone_id, limit).await
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
                    self.observed_playback
                        .record_zone(&zone, observation_now())
                        .await;
                    self.zones.write().await.insert(zone.zone_id.clone(), zone);
                }

                BusEvent::ZoneUpdated {
                    zone_id,
                    display_name,
                    state,
                } => {
                    debug!("Zone updated: {}", zone_id);
                    self.observed_playback
                        .record_state(zone_id.as_str(), state.as_str().into(), observation_now())
                        .await;
                    if let Some(zone) = self.zones.write().await.get_mut(zone_id.as_str()) {
                        zone.zone_name = display_name;
                        zone.state = state.as_str().into();
                    }
                }

                BusEvent::ZoneRemoved { zone_id } => {
                    debug!("Zone removed: {}", zone_id);
                    self.zones.write().await.remove(zone_id.as_str());
                }

                BusEvent::ProviderAccountUpdated { provider, account } => {
                    debug!("Provider account updated: {}", provider);
                    let mut accounts = self.provider_accounts.write().await;
                    if let Some(account) = account {
                        accounts.insert(provider, account);
                    } else {
                        accounts.remove(&provider);
                    }
                }

                BusEvent::AdapterError { adapter, error } => {
                    debug!("Adapter error: {}: {}", adapter, error);
                    self.adapter_errors.write().await.insert(adapter, error);
                }

                BusEvent::AdapterConnected { adapter, .. } => {
                    self.adapter_errors.write().await.remove(&adapter);
                }

                BusEvent::NowPlayingChanged {
                    zone_id,
                    title,
                    artist,
                    album,
                    image_key,
                } => {
                    debug!("Now playing changed: {}", zone_id);
                    let state = {
                        let mut zones = self.zones.write().await;
                        if let Some(zone) = zones.get_mut(zone_id.as_str()) {
                            // Preserve seek_position and duration from existing now_playing
                            let (seek_position, duration) = zone
                                .now_playing
                                .as_ref()
                                .map(|np| (np.seek_position, np.duration))
                                .unwrap_or((None, None));

                            let (repeat_mode, shuffle) = zone
                                .now_playing
                                .as_ref()
                                .map(|np| (np.repeat_mode, np.shuffle))
                                .unwrap_or((None, None));
                            zone.now_playing = Some(NowPlaying {
                                title: title.unwrap_or_default(),
                                artist: artist.unwrap_or_default(),
                                album: album.unwrap_or_default(),
                                image_key,
                                seek_position,
                                duration,
                                metadata: None,
                                repeat_mode,
                                shuffle,
                            });
                            (zone.state, zone.now_playing.clone())
                        } else {
                            (crate::bus::PlaybackState::Unknown, None)
                        }
                    };
                    self.observed_playback
                        .record_now_playing(
                            zone_id.as_str(),
                            state.0,
                            state.1.as_ref(),
                            observation_now(),
                        )
                        .await;
                }

                BusEvent::PlaybackModesChanged {
                    zone_id,
                    repeat_mode,
                    shuffle,
                } => {
                    debug!("Playback modes changed: {}", zone_id);
                    if let Some(zone) = self.zones.write().await.get_mut(zone_id.as_str()) {
                        if let Some(ref mut now_playing) = zone.now_playing {
                            now_playing.repeat_mode = repeat_mode;
                            now_playing.shuffle = shuffle;
                        }
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

                    self.provider_accounts.write().await.remove(&adapter);
                    self.adapter_errors.write().await.remove(&adapter);
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

    /// Get the last non-secret account identity reported by a provider.
    pub async fn get_provider_account(&self, provider: &str) -> Option<ProviderAccount> {
        self.provider_accounts.read().await.get(provider).cloned()
    }

    /// Get the last backend error reported by an adapter, if any.
    pub async fn get_adapter_error(&self, adapter: &str) -> Option<String> {
        self.adapter_errors.read().await.get(adapter).cloned()
    }
}
