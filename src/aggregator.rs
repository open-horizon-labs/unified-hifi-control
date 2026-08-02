//! ZoneAggregator - Single source of truth for zone state

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, info};

use crate::adapters::hqplayer::{
    HqpAdvancedOptionsSnapshot, HqpNativeObservation, HqpNativeObservationSink, HqpProfile,
};
use crate::bus::{BusEvent, NowPlaying, SharedBus, Zone};

/// Whether an HQPlayer snapshot is current or retained across a recoverable outage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HqpSnapshotPresence {
    Live,
    LastKnown,
}

/// One coherent HQPlayer observation owned by the same aggregator as ordinary zones.
#[derive(Debug, Clone)]
pub struct HqpSnapshot {
    pub observation: HqpNativeObservation,
    pub advanced: Option<HqpAdvancedOptionsSnapshot>,
    pub profiles: Option<Vec<HqpProfile>>,
    pub profiles_error: Option<String>,
    pub presence: HqpSnapshotPresence,
    pub revision: u64,
    pub last_failure_at: Option<std::time::SystemTime>,
}

/// ZoneAggregator maintains unified zone state from all adapters.
/// - Subscribes to bus events
/// - Maintains HashMap of zones by zone_id
/// - Flushes zones when adapter stops
/// - Provides query interface for API layer
pub struct ZoneAggregator {
    zones: Arc<RwLock<HashMap<String, Zone>>>,
    hqplayer_snapshots: Arc<RwLock<HashMap<String, HqpSnapshot>>>,
    bus: SharedBus,
}

impl ZoneAggregator {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            zones: Arc::new(RwLock::new(HashMap::new())),
            hqplayer_snapshots: Arc::new(RwLock::new(HashMap::new())),
            bus,
        }
    }

    /// Start the aggregator's event processing loop
    /// Should be spawned as a task
    pub async fn run(&self) {
        self.run_inner(None).await;
    }

    /// Start the event loop and acknowledge after the broadcast subscription exists.
    ///
    /// The bus does not replay. Merely spawning `run()` before producers is insufficient because the
    /// spawned future may not be polled until after an adapter publishes its initial snapshot.
    pub async fn run_with_ready(&self, ready: oneshot::Sender<()>) {
        self.run_inner(Some(ready)).await;
    }

    async fn run_inner(&self, ready: Option<oneshot::Sender<()>>) {
        let mut rx = self.bus.subscribe();
        if let Some(ready) = ready {
            if ready.send(()).is_err() {
                debug!("ZoneAggregator readiness receiver was dropped before startup completed");
            }
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

    /// Return the aggregator-owned state for one configured HQPlayer instance.
    pub async fn get_hqplayer_snapshot(&self, instance_name: &str) -> Option<HqpSnapshot> {
        self.hqplayer_snapshots
            .read()
            .await
            .get(instance_name)
            .cloned()
    }

    /// Return every aggregator-owned HQPlayer instance snapshot.
    pub async fn get_hqplayer_snapshots(&self) -> Vec<HqpSnapshot> {
        self.hqplayer_snapshots
            .read()
            .await
            .values()
            .cloned()
            .collect()
    }

    /// Attach a coherent advanced-control read to the exact native snapshot it extends.
    pub async fn publish_hqplayer_advanced(
        &self,
        instance_name: &str,
        advanced: HqpAdvancedOptionsSnapshot,
    ) -> bool {
        let mut snapshots = self.hqplayer_snapshots.write().await;
        let Some(snapshot) = snapshots.get_mut(instance_name) else {
            return false;
        };
        if snapshot.observation.execution_target != advanced.execution_target {
            return false;
        }
        snapshot.observation.pipeline = advanced.pipeline.clone();
        snapshot.advanced = Some(advanced);
        snapshot.revision = snapshot.revision.saturating_add(1);
        true
    }

    /// Publish the browser lane's named-profile inventory or its explicit failure.
    pub async fn publish_hqplayer_profiles(
        &self,
        instance_name: &str,
        result: Result<Vec<HqpProfile>, String>,
    ) -> bool {
        let mut snapshots = self.hqplayer_snapshots.write().await;
        let Some(snapshot) = snapshots.get_mut(instance_name) else {
            return false;
        };
        match result {
            Ok(profiles) => {
                snapshot.profiles = Some(profiles);
                snapshot.profiles_error = None;
            }
            Err(error) => {
                snapshot.profiles_error = Some(error);
            }
        }
        snapshot.revision = snapshot.revision.saturating_add(1);
        true
    }
}

#[async_trait::async_trait]
impl HqpNativeObservationSink for ZoneAggregator {
    async fn manager_started(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn observed(&self, observation: HqpNativeObservation) -> anyhow::Result<()> {
        let mut snapshots = self.hqplayer_snapshots.write().await;
        if snapshots
            .get(&observation.instance_name)
            .is_some_and(|current| current.observation.producer_epoch > observation.producer_epoch)
        {
            return Ok(());
        }
        let previous = snapshots.get(&observation.instance_name);
        let revision = previous.map_or(1, |current| current.revision.saturating_add(1));
        let same_native_session = previous.is_some_and(|current| {
            current.observation.execution_target == observation.execution_target
        });
        let same_endpoint = previous.is_some_and(|current| {
            current.observation.execution_target.instance_name
                == observation.execution_target.instance_name
                && current.observation.execution_target.endpoint_generation
                    == observation.execution_target.endpoint_generation
        });
        let advanced = same_native_session
            .then(|| previous.and_then(|current| current.advanced.clone()))
            .flatten();
        let profiles = same_endpoint
            .then(|| previous.and_then(|current| current.profiles.clone()))
            .flatten();
        let profiles_error = same_endpoint
            .then(|| previous.and_then(|current| current.profiles_error.clone()))
            .flatten();
        snapshots.insert(
            observation.instance_name.clone(),
            HqpSnapshot {
                observation,
                advanced,
                profiles,
                profiles_error,
                presence: HqpSnapshotPresence::Live,
                revision,
                last_failure_at: None,
            },
        );
        Ok(())
    }

    async fn advanced_observed(
        &self,
        instance_name: &str,
        snapshot: HqpAdvancedOptionsSnapshot,
    ) -> anyhow::Result<()> {
        if self
            .publish_hqplayer_advanced(instance_name, snapshot)
            .await
        {
            Ok(())
        } else {
            anyhow::bail!("HQPlayer instance {instance_name:?} changed before advanced publication")
        }
    }

    async fn profiles_observed(
        &self,
        instance_name: &str,
        result: Result<Vec<HqpProfile>, String>,
    ) -> anyhow::Result<()> {
        if self.publish_hqplayer_profiles(instance_name, result).await {
            Ok(())
        } else {
            anyhow::bail!("HQPlayer instance {instance_name:?} changed before profile publication")
        }
    }

    async fn transient_failure(
        &self,
        instance_name: &str,
        observed_at: std::time::SystemTime,
    ) -> anyhow::Result<()> {
        if let Some(snapshot) = self.hqplayer_snapshots.write().await.get_mut(instance_name) {
            snapshot.presence = HqpSnapshotPresence::LastKnown;
            snapshot.revision = snapshot.revision.saturating_add(1);
            snapshot.last_failure_at = Some(observed_at);
        }
        Ok(())
    }

    async fn instance_removed(
        &self,
        instance_name: &str,
        producer_epoch: u64,
    ) -> anyhow::Result<()> {
        let mut snapshots = self.hqplayer_snapshots.write().await;
        if snapshots
            .get(instance_name)
            .is_some_and(|snapshot| snapshot.observation.producer_epoch == producer_epoch)
        {
            snapshots.remove(instance_name);
        }
        Ok(())
    }

    async fn manager_stopped(&self) -> anyhow::Result<()> {
        for snapshot in self.hqplayer_snapshots.write().await.values_mut() {
            snapshot.presence = HqpSnapshotPresence::LastKnown;
            snapshot.revision = snapshot.revision.saturating_add(1);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::hqplayer::{
        HqpNativeExecutionTarget, HqpNativeMetadata, HqpNativeObservation,
        HqpNativeObservationSink, HqpNativeSelection, HqpNativeTransportState, HqpNativeVolume,
    };
    use std::time::{Duration, SystemTime};

    fn observation(epoch: u64, selected_mode: &str) -> HqpNativeObservation {
        HqpNativeObservation {
            instance_name: "main".to_string(),
            instance_label: Some("Listening room".to_string()),
            product_version: Some("6.0.4".to_string()),
            producer_epoch: epoch,
            execution_target: HqpNativeExecutionTarget {
                instance_name: "main".to_string(),
                producer_epoch: epoch,
                endpoint_generation: 1,
                transport_generation: epoch,
            },
            observed_at: SystemTime::UNIX_EPOCH + Duration::from_secs(epoch),
            connection: crate::adapters::hqplayer::HqpConnectionStatus {
                connected: true,
                host: Some("127.0.0.1".to_string()),
                port: 4321,
                web_port: 8088,
                info: None,
            },
            pipeline: crate::adapters::hqplayer::PipelineStatus::default(),
            transport: HqpNativeTransportState::Stopped,
            metadata: HqpNativeMetadata {
                track_id: None,
                title: None,
                artist: None,
                album: None,
            },
            volume: HqpNativeVolume {
                value_db: -18.5,
                min_db: -60.0,
                max_db: 0.0,
                step_db: Some(0.5),
                enabled: true,
                adaptive: false,
            },
            mode_is_source: selected_mode == "[source]",
            mode: HqpNativeSelection {
                selected: selected_mode.to_string(),
                choices: vec![
                    "[source]".to_string(),
                    "PCM".to_string(),
                    "SDM (DSD)".to_string(),
                ],
            },
            filter_1x: HqpNativeSelection {
                selected: "poly-sinc-gauss-long".to_string(),
                choices: vec!["poly-sinc-gauss-long".to_string()],
            },
            filter_nx: HqpNativeSelection {
                selected: "poly-sinc-gauss-hires-lp".to_string(),
                choices: vec!["poly-sinc-gauss-hires-lp".to_string()],
            },
            shaper: HqpNativeSelection {
                selected: "ASDM7EC-fast".to_string(),
                choices: vec!["ASDM7EC-fast".to_string()],
            },
            rate: HqpNativeSelection {
                selected: "0".to_string(),
                choices: vec!["0".to_string(), "11289600".to_string()],
            },
        }
    }

    #[tokio::test]
    async fn hqplayer_observations_are_owned_by_the_zone_aggregator() {
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        HqpNativeObservationSink::manager_started(&aggregator)
            .await
            .unwrap();
        HqpNativeObservationSink::observed(&aggregator, observation(7, "PCM"))
            .await
            .unwrap();

        let snapshot = aggregator
            .get_hqplayer_snapshot("main")
            .await
            .expect("published HQPlayer snapshot");
        assert_eq!(snapshot.observation.mode.selected, "PCM");
        assert_eq!(snapshot.presence, HqpSnapshotPresence::Live);
        assert_eq!(snapshot.revision, 1);
    }

    #[tokio::test]
    async fn transient_failure_retains_last_known_hqplayer_truth() {
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        HqpNativeObservationSink::manager_started(&aggregator)
            .await
            .unwrap();
        HqpNativeObservationSink::observed(&aggregator, observation(7, "PCM"))
            .await
            .unwrap();
        HqpNativeObservationSink::transient_failure(
            &aggregator,
            "main",
            SystemTime::UNIX_EPOCH + Duration::from_secs(8),
        )
        .await
        .unwrap();

        let snapshot = aggregator
            .get_hqplayer_snapshot("main")
            .await
            .expect("last-known HQPlayer snapshot");
        assert_eq!(snapshot.observation.mode.selected, "PCM");
        assert_eq!(snapshot.presence, HqpSnapshotPresence::LastKnown);
        assert_eq!(snapshot.revision, 2);
    }

    #[tokio::test]
    async fn stale_retirement_cannot_remove_a_replacement_hqplayer_epoch() {
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        HqpNativeObservationSink::manager_started(&aggregator)
            .await
            .unwrap();
        HqpNativeObservationSink::observed(&aggregator, observation(8, "SDM (DSD)"))
            .await
            .unwrap();
        HqpNativeObservationSink::instance_removed(&aggregator, "main", 7)
            .await
            .unwrap();

        assert_eq!(
            aggregator
                .get_hqplayer_snapshot("main")
                .await
                .expect("replacement snapshot")
                .observation
                .producer_epoch,
            8
        );

        HqpNativeObservationSink::instance_removed(&aggregator, "main", 8)
            .await
            .unwrap();
        assert!(aggregator.get_hqplayer_snapshot("main").await.is_none());
    }
}
