//! ZoneAggregator - Single source of truth for zone state

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio::sync::{oneshot, RwLock};
use tracing::{debug, info, warn};

use crate::adapters::hqplayer::{
    HqpAdvancedOptionsSnapshot, HqpNativeObservation, HqpNativeObservationSink, HqpProfile,
};
#[cfg(test)]
use crate::bus::runtime::ProjectionSource;
use crate::bus::runtime::{
    ProjectionCommit, ProjectionCommitter, ProjectionEntry, ProjectionFreshness, ProjectionKind,
    ProjectionPayload, ProjectionUpdate,
};
use crate::bus::{BusEvent, NowPlaying, PrefixedZoneId, ProviderAccount, SharedBus, Zone};
use crate::mcp::observation_history::PlaybackObservationHistory;

fn observation_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

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
    state: Arc<RwLock<AggregateState>>,
    provider_accounts: Arc<RwLock<HashMap<String, ProviderAccount>>>,
    adapter_errors: Arc<RwLock<HashMap<String, String>>>,
    observed_playback: PlaybackObservationHistory,
    bus: SharedBus,
}

/// Every canonical projection value shares one lock and revision domain. This lets a coherent
/// provider observation update a direct zone and its provider-native state without exposing a
/// torn pair to readers.
#[derive(Default)]
struct AggregateState {
    zones: HashMap<String, Zone>,
    hqplayer_snapshots: HashMap<String, HqpSnapshot>,
    projection_revision: u64,
    source_cursors: HashMap<String, ProjectionCursor>,
    projection_entries: BTreeMap<String, (u64, ProjectionPayload)>,
}

#[derive(Clone, Copy)]
struct ProjectionCursor {
    epoch: u64,
    sequence: u64,
    freshness: ProjectionFreshness,
}

impl ZoneAggregator {
    pub fn new(bus: SharedBus) -> Self {
        Self {
            state: Arc::new(RwLock::new(AggregateState::default())),
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
            state: Arc::new(RwLock::new(AggregateState::default())),
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

        loop {
            let event = match rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // `broadcast` is a lossy notification channel. A lagged receiver is still
                    // subscribed and can admit the next complete adapter observation; stopping
                    // here would turn one burst into a permanently stale aggregate projection.
                    warn!(
                        skipped,
                        "ZoneAggregator lagged behind the event bus; waiting for a recovery snapshot"
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    info!("ZoneAggregator event bus closed");
                    break;
                }
            };
            match event {
                BusEvent::ZoneDiscovered { zone } => {
                    debug!("Zone discovered: {}", zone.zone_id);
                    self.observed_playback
                        .record_zone(&zone, observation_now())
                        .await;
                    self.state
                        .write()
                        .await
                        .zones
                        .insert(zone.zone_id.clone(), zone);
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
                    if let Some(zone) = self.state.write().await.zones.get_mut(zone_id.as_str()) {
                        zone.zone_name = display_name;
                        zone.state = state.as_str().into();
                    }
                }

                BusEvent::ZoneRemoved { zone_id } => {
                    debug!("Zone removed: {}", zone_id);
                    self.observed_playback.clear_zone(zone_id.as_str()).await;
                    self.state.write().await.zones.remove(zone_id.as_str());
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
                        let mut aggregate = self.state.write().await;
                        if let Some(zone) = aggregate.zones.get_mut(zone_id.as_str()) {
                            // All fields absent is the explicit clear sentinel emitted by
                            // adapters whose complete snapshot reports no current track. Do
                            // not turn it into an empty `Some(NowPlaying)`: that would retain a
                            // phantom track in every API projection until the next song starts.
                            if title.is_none()
                                && artist.is_none()
                                && album.is_none()
                                && image_key.is_none()
                            {
                                zone.now_playing = None;
                                (zone.state, None)
                            } else {
                                // Preserve fields this compatibility event cannot carry when it describes
                                // the same track as the canonical snapshot. A changed identity drops
                                // metadata so details from the previous track cannot leak forward.
                                let same_track = zone.now_playing.as_ref().is_some_and(|np| {
                                    title.as_deref().unwrap_or_default() == np.title
                                        && artist.as_deref().unwrap_or_default() == np.artist
                                        && album.as_deref().unwrap_or_default() == np.album
                                });
                                let (seek_position, duration, metadata, repeat_mode, shuffle) =
                                    zone.now_playing
                                        .as_ref()
                                        .map(|np| {
                                            (
                                                np.seek_position,
                                                np.duration,
                                                same_track.then(|| np.metadata.clone()).flatten(),
                                                np.repeat_mode,
                                                np.shuffle,
                                            )
                                        })
                                        .unwrap_or((None, None, None, None, None));
                                zone.now_playing = Some(NowPlaying {
                                    title: title.unwrap_or_default(),
                                    artist: artist.unwrap_or_default(),
                                    album: album.unwrap_or_default(),
                                    image_key,
                                    seek_position,
                                    duration,
                                    metadata,
                                    repeat_mode,
                                    shuffle,
                                });
                                (zone.state, zone.now_playing.clone())
                            }
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
                    if let Some(zone) = self.state.write().await.zones.get_mut(zone_id.as_str()) {
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
                    let mut aggregate = self.state.write().await;
                    for zone in aggregate.zones.values_mut() {
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
                    if let Some(zone) = self.state.write().await.zones.get_mut(zone_id.as_str()) {
                        if let Some(ref mut np) = zone.now_playing {
                            np.seek_position = Some(position as f64);
                        }
                    }
                }

                BusEvent::AdapterStopping { adapter, .. } => {
                    info!("Flushing zones for adapter: {}", adapter);
                    let prefix = format!("{}:", adapter);

                    // Remove all zones with this prefix
                    let zone_ids = {
                        let mut aggregate = self.state.write().await;
                        let zone_ids: Vec<String> = aggregate
                            .zones
                            .keys()
                            .filter(|k| k.starts_with(&prefix))
                            .cloned()
                            .collect();

                        for zone_id in &zone_ids {
                            aggregate.zones.remove(zone_id);
                        }
                        zone_ids
                    };

                    if adapter == "applemusic" {
                        for zone_id in &zone_ids {
                            self.observed_playback.clear_zone(zone_id).await;
                        }
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
        self.state.read().await.zones.values().cloned().collect()
    }

    /// Get zones for a specific adapter
    pub async fn get_zones_by_adapter(&self, adapter: &str) -> Vec<Zone> {
        let prefix = format!("{}:", adapter);
        self.state
            .read()
            .await
            .zones
            .values()
            .filter(|z| z.zone_id.starts_with(&prefix))
            .cloned()
            .collect()
    }

    /// Get a specific zone
    pub async fn get_zone(&self, zone_id: &str) -> Option<Zone> {
        self.state.read().await.zones.get(zone_id).cloned()
    }

    /// Get now playing for a zone
    pub async fn get_now_playing(&self, zone_id: &str) -> Option<NowPlaying> {
        self.state
            .read()
            .await
            .zones
            .get(zone_id)
            .and_then(|z| z.now_playing.clone())
    }

    /// Get zone count
    pub async fn zone_count(&self) -> usize {
        self.state.read().await.zones.len()
    }

    /// Get the last non-secret account identity reported by a provider.
    pub async fn get_provider_account(&self, provider: &str) -> Option<ProviderAccount> {
        self.provider_accounts.read().await.get(provider).cloned()
    }

    /// Get the last backend error reported by an adapter, if any.
    pub async fn get_adapter_error(&self, adapter: &str) -> Option<String> {
        self.adapter_errors.read().await.get(adapter).cloned()
    }

    /// Return the aggregator-owned state for one configured HQPlayer instance.
    pub async fn get_hqplayer_snapshot(&self, instance_name: &str) -> Option<HqpSnapshot> {
        self.state
            .read()
            .await
            .hqplayer_snapshots
            .get(instance_name)
            .cloned()
    }

    /// Return every aggregator-owned HQPlayer instance snapshot.
    pub async fn get_hqplayer_snapshots(&self) -> Vec<HqpSnapshot> {
        self.state
            .read()
            .await
            .hqplayer_snapshots
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
        let mut snapshots = self.state.write().await;
        let Some(snapshot) = snapshots.hqplayer_snapshots.get_mut(instance_name) else {
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
        let mut snapshots = self.state.write().await;
        let Some(snapshot) = snapshots.hqplayer_snapshots.get_mut(instance_name) else {
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

    /// The reliable projection actor commits through this method; it is deliberately the only
    /// owner of projection revisions, cursors, and entries. The runtime is only ingress plus a
    /// command ledger, so it cannot race this aggregate with a second authoritative view.
    async fn commit_reliable_projection(&self, update: ProjectionUpdate) -> ProjectionCommit {
        let source_identity = update.source.identity();
        let mut aggregate = self.state.write().await;

        if let Some(cursor) = aggregate.source_cursors.get_mut(&source_identity) {
            if update.source.epoch < cursor.epoch
                || (update.source.epoch == cursor.epoch && update.sequence <= cursor.sequence)
            {
                return ProjectionCommit::StaleIgnored {
                    current_epoch: cursor.epoch,
                    current_sequence: cursor.sequence,
                };
            }
            if update.source.epoch == cursor.epoch
                && update.sequence > cursor.sequence.saturating_add(1)
                && update.kind == ProjectionKind::Delta
            {
                let expected_sequence = cursor.sequence.saturating_add(1);
                cursor.freshness = ProjectionFreshness::Reconciling;
                return ProjectionCommit::GapDetected {
                    expected_sequence,
                    received_sequence: update.sequence,
                };
            }
        }

        aggregate.projection_revision = aggregate.projection_revision.saturating_add(1);
        let revision = aggregate.projection_revision;
        aggregate.source_cursors.insert(
            source_identity,
            ProjectionCursor {
                epoch: update.source.epoch,
                sequence: update.sequence,
                freshness: ProjectionFreshness::Fresh,
            },
        );
        let mut changed_zones = Vec::new();
        let mut removed_zones = Vec::new();
        for ProjectionEntry { key, payload } in update.entries {
            match &payload {
                ProjectionPayload::Zone(zone) => {
                    aggregate
                        .zones
                        .insert(zone.zone_id.clone(), (**zone).clone());
                    changed_zones.push((**zone).clone());
                }
                ProjectionPayload::ZoneRemoved { zone_id } => {
                    if aggregate.zones.remove(&zone_id.to_string()).is_some() {
                        removed_zones.push(zone_id.clone());
                    }
                }
                ProjectionPayload::HqpObservation(observation) => {
                    let snapshots = &mut aggregate.hqplayer_snapshots;
                    if snapshots
                        .get(&observation.instance_name)
                        .is_none_or(|current| {
                            current.observation.producer_epoch <= observation.producer_epoch
                        })
                    {
                        let previous = snapshots.get(&observation.instance_name);
                        let snapshot_revision =
                            previous.map_or(1, |current| current.revision.saturating_add(1));
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
                                observation: (**observation).clone(),
                                advanced,
                                profiles,
                                profiles_error,
                                presence: HqpSnapshotPresence::Live,
                                revision: snapshot_revision,
                                last_failure_at: None,
                            },
                        );
                    }
                }
                ProjectionPayload::HqpAdvanced {
                    instance_name,
                    snapshot,
                } => {
                    if let Some(current) = aggregate.hqplayer_snapshots.get_mut(instance_name) {
                        if current.observation.execution_target == snapshot.execution_target {
                            current.observation.pipeline = snapshot.pipeline.clone();
                            current.advanced = Some((**snapshot).clone());
                            current.revision = current.revision.saturating_add(1);
                        }
                    }
                }
                ProjectionPayload::HqpProfiles {
                    instance_name,
                    result,
                } => {
                    if let Some(current) = aggregate.hqplayer_snapshots.get_mut(instance_name) {
                        match result {
                            Ok(profiles) => {
                                current.profiles = Some(profiles.clone());
                                current.profiles_error = None;
                            }
                            Err(error) => current.profiles_error = Some(error.clone()),
                        }
                        current.revision = current.revision.saturating_add(1);
                    }
                }
                ProjectionPayload::HqpTransientFailure {
                    instance_name,
                    observed_at,
                } => {
                    if let Some(current) = aggregate.hqplayer_snapshots.get_mut(instance_name) {
                        current.presence = HqpSnapshotPresence::LastKnown;
                        current.revision = current.revision.saturating_add(1);
                        current.last_failure_at = Some(*observed_at);
                    }
                }
                ProjectionPayload::HqpRemoved {
                    instance_name,
                    producer_epoch,
                } => {
                    if aggregate
                        .hqplayer_snapshots
                        .get(instance_name)
                        .is_some_and(|snapshot| {
                            snapshot.observation.producer_epoch == *producer_epoch
                        })
                    {
                        aggregate.hqplayer_snapshots.remove(instance_name);
                        let zone_id = format!("hqplayer:{instance_name}");
                        if aggregate.zones.remove(&zone_id).is_some() {
                            removed_zones.push(PrefixedZoneId::hqplayer(instance_name));
                        }
                    }
                }
                ProjectionPayload::HqpManagerStopped => {
                    for snapshot in aggregate.hqplayer_snapshots.values_mut() {
                        snapshot.presence = HqpSnapshotPresence::LastKnown;
                        snapshot.revision = snapshot.revision.saturating_add(1);
                    }
                    aggregate
                        .zones
                        .retain(|zone_id, _| !zone_id.starts_with("hqplayer:"));
                }
                ProjectionPayload::Marker(_) => {}
            }
            aggregate
                .projection_entries
                .insert(key, (revision, payload));
        }
        drop(aggregate);
        // Reliable projection is the canonical mutation lane. Existing in-app bus consumers still
        // receive notifications, but only after the aggregator has committed the complete state
        // they subsequently re-read. Its own subscriber sees idempotent partial updates; it is
        // never asked to reconstruct canonical state from these compatibility hints.
        for zone in changed_zones {
            let Some(zone_id) = PrefixedZoneId::parse(&zone.zone_id) else {
                tracing::error!(zone_id = %zone.zone_id, "reliable projection contained an invalid zone id");
                continue;
            };
            // The full lifecycle hint invalidates MCP's zone-list resource; the granular hints
            // below drive the existing browser/SSE refresh predicates.
            self.bus
                .publish(BusEvent::ZoneDiscovered { zone: zone.clone() });
            self.bus.publish(BusEvent::ZoneUpdated {
                zone_id: zone_id.clone(),
                display_name: zone.zone_name.clone(),
                state: zone.state.to_string(),
            });
            if let Some(now_playing) = &zone.now_playing {
                self.bus.publish(BusEvent::NowPlayingChanged {
                    zone_id: zone_id.clone(),
                    title: Some(now_playing.title.clone()),
                    artist: Some(now_playing.artist.clone()),
                    album: Some(now_playing.album.clone()),
                    image_key: now_playing.image_key.clone(),
                });
                if let Some(position) = now_playing.seek_position {
                    self.bus.publish(BusEvent::SeekPositionChanged {
                        zone_id: zone_id.clone(),
                        position: position.round() as i64,
                    });
                }
            }
            if let Some(volume) = &zone.volume_control {
                self.bus.publish(BusEvent::VolumeChanged {
                    output_id: volume
                        .output_id
                        .clone()
                        .unwrap_or_else(|| zone.zone_id.clone()),
                    value: volume.value,
                    is_muted: volume.is_muted,
                });
            }
        }
        for zone_id in removed_zones {
            self.bus.publish(BusEvent::ZoneRemoved { zone_id });
        }
        ProjectionCommit::Committed { revision }
    }

    #[cfg(test)]
    async fn projection_revision(&self) -> u64 {
        self.state.read().await.projection_revision
    }

    #[cfg(test)]
    async fn projection_entry(&self, key: &str) -> Option<(u64, ProjectionPayload)> {
        self.state.read().await.projection_entries.get(key).cloned()
    }

    #[cfg(test)]
    async fn projection_freshness(&self, source: &ProjectionSource) -> Option<ProjectionFreshness> {
        self.state
            .read()
            .await
            .source_cursors
            .get(&source.identity())
            .map(|cursor| cursor.freshness)
    }
}

#[async_trait::async_trait]
impl ProjectionCommitter for ZoneAggregator {
    async fn commit_projection(&self, update: ProjectionUpdate) -> ProjectionCommit {
        self.commit_reliable_projection(update).await
    }
}

#[async_trait::async_trait]
impl HqpNativeObservationSink for ZoneAggregator {
    async fn manager_started(&self) -> anyhow::Result<()> {
        Ok(())
    }

    async fn observed(&self, observation: HqpNativeObservation) -> anyhow::Result<()> {
        let mut aggregate = self.state.write().await;
        let snapshots = &mut aggregate.hqplayer_snapshots;
        if snapshots
            .get(&observation.instance_name)
            .is_some_and(|current| current.observation.producer_epoch > observation.producer_epoch)
        {
            return Ok(());
        }
        aggregate
            .zones
            .insert(observation.zone.zone_id.clone(), observation.zone.clone());
        let snapshots = &mut aggregate.hqplayer_snapshots;
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
        if let Some(snapshot) = self
            .state
            .write()
            .await
            .hqplayer_snapshots
            .get_mut(instance_name)
        {
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
        let mut aggregate = self.state.write().await;
        let snapshots = &mut aggregate.hqplayer_snapshots;
        if snapshots
            .get(instance_name)
            .is_some_and(|snapshot| snapshot.observation.producer_epoch == producer_epoch)
        {
            snapshots.remove(instance_name);
        }
        Ok(())
    }

    async fn manager_stopped(&self) -> anyhow::Result<()> {
        for snapshot in self.state.write().await.hqplayer_snapshots.values_mut() {
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
            zone: projected_zone("hqplayer:main"),
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

    fn projected_zone(zone_id: &str) -> Zone {
        Zone {
            zone_id: zone_id.to_string(),
            zone_name: zone_id.to_string(),
            state: crate::bus::PlaybackState::Stopped,
            volume_control: None,
            now_playing: None,
            source: "hqplayer".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }

    fn source(epoch: u64) -> ProjectionSource {
        ProjectionSource {
            adapter: "hqplayer".to_string(),
            instance: Some("main".to_string()),
            epoch,
        }
    }

    fn projection(
        source: ProjectionSource,
        sequence: u64,
        kind: ProjectionKind,
        entries: Vec<ProjectionEntry>,
    ) -> ProjectionUpdate {
        ProjectionUpdate {
            source,
            sequence,
            kind,
            caused_by: None,
            entries,
        }
    }

    #[tokio::test]
    async fn reliable_projection_commits_zones_and_entries_in_one_aggregate_revision() {
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        let update = projection(
            source(1),
            1,
            ProjectionKind::Snapshot,
            vec![
                ProjectionEntry {
                    key: "zone:hqplayer:main".to_string(),
                    payload: ProjectionPayload::Zone(Box::new(projected_zone("hqplayer:main"))),
                },
                ProjectionEntry {
                    key: "provider:hqplayer:main".to_string(),
                    payload: ProjectionPayload::Marker("native snapshot".to_string()),
                },
            ],
        );

        assert_eq!(
            ProjectionCommitter::commit_projection(&aggregator, update).await,
            ProjectionCommit::Committed { revision: 1 }
        );
        assert_eq!(aggregator.projection_revision().await, 1);
        assert_eq!(
            aggregator
                .projection_entry("zone:hqplayer:main")
                .await
                .map(|(revision, _)| revision),
            Some(1)
        );
        assert_eq!(
            aggregator
                .projection_entry("provider:hqplayer:main")
                .await
                .map(|(revision, _)| revision),
            Some(1)
        );
        assert!(aggregator.get_zone("hqplayer:main").await.is_some());
    }

    #[tokio::test]
    async fn reliable_zone_projection_notifies_consumers_only_after_canonical_commit() {
        let bus = crate::bus::create_bus();
        let mut notifications = bus.subscribe();
        let aggregator = ZoneAggregator::new(bus);
        let mut zone = projected_zone("hqplayer:main");
        zone.state = crate::bus::PlaybackState::Playing;
        zone.now_playing = Some(crate::bus::NowPlaying {
            title: "Observed title".to_string(),
            artist: "Observed artist".to_string(),
            album: "Observed album".to_string(),
            image_key: Some("observed-image".to_string()),
            seek_position: Some(12.0),
            duration: Some(120.0),
            metadata: None,
            repeat_mode: None,
            shuffle: None,
        });
        zone.volume_control = Some(crate::bus::VolumeControl {
            output_id: Some("hqplayer:main".to_string()),
            value: -9.0,
            min: -60.0,
            max: 0.0,
            step: 0.5,
            is_muted: false,
            scale: crate::bus::VolumeScale::Decibel,
        });

        assert_eq!(
            ProjectionCommitter::commit_projection(
                &aggregator,
                projection(
                    source(1),
                    1,
                    ProjectionKind::Snapshot,
                    vec![ProjectionEntry {
                        key: "zone:hqplayer:main".to_string(),
                        payload: ProjectionPayload::Zone(Box::new(zone.clone())),
                    }],
                ),
            )
            .await,
            ProjectionCommit::Committed { revision: 1 }
        );

        let committed = aggregator
            .get_zone("hqplayer:main")
            .await
            .expect("zone is canonical before notification");
        assert_eq!(committed.state, crate::bus::PlaybackState::Playing);

        let mut event_names = Vec::new();
        for _ in 0..5 {
            let event = tokio::time::timeout(Duration::from_millis(100), notifications.recv())
                .await
                .expect("post-commit notification")
                .expect("notification bus remains open");
            event_names.push(event.event_type());
        }
        assert_eq!(
            event_names,
            vec![
                "zone_discovered",
                "zone_updated",
                "now_playing_changed",
                "seek_position_changed",
                "volume_changed"
            ]
        );
    }

    #[tokio::test]
    async fn reliable_projection_rejects_stale_data_and_requires_snapshot_after_delta_gap() {
        let aggregator = ZoneAggregator::new(crate::bus::create_bus());
        let current = source(3);
        assert_eq!(
            ProjectionCommitter::commit_projection(
                &aggregator,
                projection(
                    current.clone(),
                    2,
                    ProjectionKind::Snapshot,
                    vec![ProjectionEntry {
                        key: "provider:hqplayer:main".to_string(),
                        payload: ProjectionPayload::Marker("seq2".to_string()),
                    }],
                ),
            )
            .await,
            ProjectionCommit::Committed { revision: 1 }
        );
        assert_eq!(
            ProjectionCommitter::commit_projection(
                &aggregator,
                projection(current.clone(), 1, ProjectionKind::Delta, Vec::new()),
            )
            .await,
            ProjectionCommit::StaleIgnored {
                current_epoch: 3,
                current_sequence: 2,
            }
        );
        assert_eq!(
            ProjectionCommitter::commit_projection(
                &aggregator,
                projection(current.clone(), 4, ProjectionKind::Delta, Vec::new()),
            )
            .await,
            ProjectionCommit::GapDetected {
                expected_sequence: 3,
                received_sequence: 4,
            }
        );
        assert_eq!(
            aggregator.projection_freshness(&current).await,
            Some(ProjectionFreshness::Reconciling)
        );
        assert_eq!(aggregator.projection_revision().await, 1);

        assert_eq!(
            ProjectionCommitter::commit_projection(
                &aggregator,
                projection(current.clone(), 4, ProjectionKind::Snapshot, Vec::new()),
            )
            .await,
            ProjectionCommit::Committed { revision: 2 }
        );
        assert_eq!(
            aggregator.projection_freshness(&current).await,
            Some(ProjectionFreshness::Fresh)
        );
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

    /// A broadcast receiver reports `Lagged` before the next retained message; it has not closed.
    /// The aggregator must keep receiving so a subsequent complete adapter snapshot can repair the
    /// lossy notification gap instead of leaving every future zone update permanently invisible.
    #[tokio::test]
    async fn lagged_event_receiver_recovers_and_admits_the_next_zone_snapshot() {
        let bus = std::sync::Arc::new(crate::bus::EventBus::new(1));
        let aggregator = std::sync::Arc::new(ZoneAggregator::new(bus.clone()));
        let (ready_tx, ready_rx) = oneshot::channel();
        let running = {
            let aggregator = aggregator.clone();
            tokio::spawn(async move { aggregator.run_with_ready(ready_tx).await })
        };
        ready_rx
            .await
            .expect("aggregator subscribed before publication");

        // These synchronous sends run before this task yields, so the capacity-one receiver must
        // observe `Lagged` before it can consume the final, retained discovery snapshot.
        bus.publish(BusEvent::HealthCheck { timestamp: 1 });
        bus.publish(BusEvent::HealthCheck { timestamp: 2 });
        bus.publish(BusEvent::ZoneDiscovered {
            zone: Zone {
                zone_id: "lms:after-lag".to_string(),
                zone_name: "Recovered after lag".to_string(),
                state: crate::bus::PlaybackState::Stopped,
                volume_control: None,
                now_playing: None,
                source: "lms".to_string(),
                is_controllable: true,
                is_seekable: false,
                last_updated: 0,
                is_play_allowed: true,
                is_pause_allowed: false,
                is_next_allowed: false,
                is_previous_allowed: false,
            },
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if aggregator.get_zone("lms:after-lag").await.is_some() {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("aggregator must process the retained event after a lag notification");

        bus.publish(BusEvent::ShuttingDown { reason: None });
        tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("aggregator exits when the bus is explicitly shut down")
            .expect("aggregator task did not panic");
    }

    #[tokio::test]
    async fn playback_mode_event_updates_the_aggregated_now_playing_state() {
        let bus = crate::bus::create_bus();
        let aggregator = std::sync::Arc::new(ZoneAggregator::new(bus.clone()));
        let (ready_tx, ready_rx) = oneshot::channel();
        let running = {
            let aggregator = aggregator.clone();
            tokio::spawn(async move { aggregator.run_with_ready(ready_tx).await })
        };
        ready_rx.await.expect("aggregator ready");

        let mut zone = projected_zone("musicassistant:kitchen");
        zone.now_playing = Some(NowPlaying {
            title: "Kind of Blue".to_string(),
            artist: "Miles Davis".to_string(),
            album: "Kind of Blue".to_string(),
            image_key: None,
            seek_position: None,
            duration: None,
            metadata: None,
            repeat_mode: Some(crate::bus::RepeatMode::Off),
            shuffle: Some(false),
        });
        bus.publish(BusEvent::ZoneDiscovered { zone });
        bus.publish(BusEvent::PlaybackModesChanged {
            zone_id: PrefixedZoneId::musicassistant("kitchen"),
            repeat_mode: Some(crate::bus::RepeatMode::All),
            shuffle: Some(true),
        });

        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let matches = aggregator
                    .get_zone("musicassistant:kitchen")
                    .await
                    .and_then(|zone| zone.now_playing)
                    .is_some_and(|track| {
                        track.repeat_mode == Some(crate::bus::RepeatMode::All)
                            && track.shuffle == Some(true)
                    });
                if matches {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("mode event reaches aggregated state");

        bus.publish(BusEvent::ShuttingDown { reason: None });
        tokio::time::timeout(Duration::from_secs(1), running)
            .await
            .expect("aggregator shutdown")
            .expect("aggregator task");
    }
}
