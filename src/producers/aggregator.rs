//! Aggregator-owned adaptive producer state.
//!
//! The aggregator is the single owner of every current producer document, exactly as it is
//! for zones (`docs/ARCHITECTURE.md`). Adapters publish on the internal
//! [`super::AdaptiveBus`]; nothing queries an adapter; every read is served from an
//! admitted snapshot.
//!
//! Three properties are worth stating because they are not obvious from the types:
//!
//! * **Snapshots are atomic.** One snapshot is one whole published document. A consumer can
//!   never see a mode read at revision 41 beside an enumeration read at revision 43.
//! * **Lane history lives beside the document, never inside it.** [`LaneWitness`] is
//!   aggregator-observed and labelled as such. Merging a remembered timestamp into the
//!   producer's own `lanes` would put words in its mouth.
//! * **A refusal is retained, not merely logged.** Publication is asynchronous, so the
//!   publishing adapter gets no return value; if a refusal were only a log line, a producer
//!   author would have no way to learn its documents were being dropped.

use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use super::admission::{admit, Admission, AdmissionRefusal, IntentRepair, ProducerKey};
use super::event::{AdaptiveEvent, SharedAdaptiveBus};
use crate::adaptive::{
    LaneState, ProducerDocument, ProducerEpoch, Reason, Timestamp, TransportLane,
};
use crate::bus::{BusEvent, SharedBus};

/// What the aggregator has ever observed about one transport lane.
///
/// Kept beside the document rather than merged into it. A producer that fails to reach its
/// persistence path publishes a lane with no `last_success`; the aggregator remembers when
/// that lane last worked, which is what "retain last-good per-lane state instead of blanking
/// the producer" actually requires — without rewriting what the producer said.
#[derive(Debug, Clone, PartialEq)]
pub struct LaneWitness {
    /// Which lane.
    pub lane: TransportLane,
    /// Its state in the most recently admitted document.
    pub state: LaneState,
    /// The newest success ever seen for this lane. Monotonic: never regresses, so a failing
    /// poll cannot erase the evidence that the lane worked five seconds ago.
    pub last_success: Option<Timestamp>,
    /// The most recent error seen for this lane.
    pub last_error: Option<Reason>,
    /// The epoch `last_success` was observed in.
    ///
    /// Carrying a last-good timestamp across a producer restart is honest only if a
    /// consumer can tell that it predates the restart. Resetting it instead would discard
    /// true information; presenting it unqualified would imply it was observed by the
    /// process now running.
    pub observed_in_epoch: ProducerEpoch,
}

/// Whether a snapshot is current or last-known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProducerPresence {
    /// The producer is publishing and does not mark itself stale.
    Live,
    /// Last-known data. Never presented as current.
    ///
    /// Driven by the producer's own `stale` flag rather than by the aggregator inferring it
    /// from lane health: whole-document staleness is an assertion only the producer can
    /// make, and per-lane degradation is already visible through [`LaneWitness`].
    LastKnown,
}

/// One producer's state, served atomically.
#[derive(Debug, Clone)]
pub struct ProducerSnapshot {
    /// Which producer.
    pub key: ProducerKey,
    /// The admitted document, shared rather than copied per reader.
    pub document: Arc<ProducerDocument>,
    /// Per-lane last-good history, observed by the aggregator.
    pub lanes: BTreeMap<TransportLane, LaneWitness>,
    /// Current or last-known.
    pub presence: ProducerPresence,
    /// Coherence demotions applied to the document being served.
    pub repairs: Vec<IntentRepair>,
    /// Updates dropped because the internal bus lagged.
    ///
    /// Channel-global rather than per-producer: `broadcast` reports a receiver's shortfall,
    /// not which messages were lost. Non-zero means some revision was skipped; because
    /// documents are whole snapshots rather than deltas, the store converges on the next
    /// publication and the count is a staleness hint, not a corruption warning.
    pub missed_updates: u64,
    /// The most recent refusal for this producer, retained across later successes.
    pub last_refusal: Option<AdmissionRefusal>,
}

struct Entry {
    document: Arc<ProducerDocument>,
    lanes: BTreeMap<TransportLane, LaneWitness>,
    repairs: Vec<IntentRepair>,
}

/// Producers plus the refusals that did not become producers.
///
/// One lock over both, so a reader cannot observe a refusal recorded against a producer
/// whose document has not landed yet. Refusals are stored separately because the first
/// document for a key can be refused, and that is exactly the case where a silent drop
/// would be least diagnosable — there is no entry to hang the record on.
#[derive(Default)]
struct Store {
    producers: BTreeMap<ProducerKey, Entry>,
    refusals: BTreeMap<ProducerKey, RefusalRecord>,
    missed_updates: u64,
}

/// A retained refusal, with the identity of the adapter that published it.
///
/// The producer type is kept beside the refusal because a first-publication refusal never
/// creates a producer entry, and an `AdapterStopping` flush derives its victims from the
/// producer map. Without this, a refusal for a producer that never existed outlived the
/// adapter that caused it. Found by CodeRabbit at `9c53079`.
struct RefusalRecord {
    refusal: AdmissionRefusal,
    producer_type: String,
}

/// Owns every current adaptive producer document.
pub struct ProducerAggregator {
    store: Arc<RwLock<Store>>,
    bus: Option<SharedBus>,
    adaptive: Option<SharedAdaptiveBus>,
}

impl ProducerAggregator {
    /// Construct wired to the public bus (for lifecycle) and the internal bus (for
    /// producer documents).
    pub fn new(bus: SharedBus, adaptive: SharedAdaptiveBus) -> Self {
        Self {
            store: Arc::new(RwLock::new(Store::default())),
            bus: Some(bus),
            adaptive: Some(adaptive),
        }
    }

    /// Construct with no bus, for callers that drive [`ProducerAggregator::ingest`]
    /// directly.
    pub fn detached() -> Self {
        Self {
            store: Arc::new(RwLock::new(Store::default())),
            bus: None,
            adaptive: None,
        }
    }

    /// Event loop. Spawn this; it returns on `ShuttingDown` or when both buses close.
    ///
    /// The loop is here rather than inside a `tokio::spawn` block so that
    /// `tests/spawn_cancellation_lint.rs` sees a cancellable shape, matching
    /// `ZoneAggregator::run`.
    pub async fn run(&self) {
        let (Some(adaptive), Some(bus)) = (self.adaptive.as_ref(), self.bus.as_ref()) else {
            return;
        };
        let mut adaptive_rx = adaptive.subscribe();
        let mut bus_rx = bus.subscribe();

        info!("ProducerAggregator started");

        loop {
            tokio::select! {
                received = adaptive_rx.recv() => match received {
                    Ok(event) => self.apply_adaptive_event(event).await,
                    // Lag must not end the loop. `while let Ok(..) = rx.recv().await`
                    // treats `Lagged` as termination, which would permanently stop
                    // producer state after one burst - see `ZoneAggregator::run`. Because
                    // documents are whole snapshots, a dropped update is recovered by the
                    // next publication, but only if this loop is still consuming.
                    Err(RecvError::Lagged(missed)) => self.record_lag(missed).await,
                    Err(RecvError::Closed) => break,
                },
                received = bus_rx.recv() => match received {
                    Ok(event) => {
                        if self.apply_bus_event(event).await {
                            break;
                        }
                    }
                    Err(RecvError::Lagged(missed)) => {
                        debug!("ProducerAggregator lagged {missed} public bus events");
                    }
                    Err(RecvError::Closed) => break,
                },
            }
        }

        info!("ProducerAggregator stopped");
    }

    /// Handle one internal-bus event.
    async fn apply_adaptive_event(&self, event: AdaptiveEvent) {
        match event {
            AdaptiveEvent::ProducerPublished { document } => {
                self.ingest(*document).await;
            }
            AdaptiveEvent::ProducerRemoved { producer_id } => {
                let removed = self.remove(&producer_id).await;
                debug!("Producer removed: {producer_id} ({removed} targets)");
            }
            // The aggregator's own egress. Consuming it would be a loop.
            AdaptiveEvent::SnapshotAdmitted { .. } | AdaptiveEvent::SnapshotRefused { .. } => {}
        }
    }

    /// Handle one public-bus event. Returns whether the loop should stop.
    ///
    /// Only lifecycle events do anything. Every command, volume and pipeline event is inert
    /// **by construction**, which is how "immediate/live commands cannot read, clear or
    /// flush staged intent" is guaranteed rather than merely intended: there is no code path
    /// from a live command to producer state.
    pub async fn apply_bus_event(&self, event: BusEvent) -> bool {
        match event {
            BusEvent::AdapterStopping { adapter, .. } => {
                let removed = self.flush_adapter(&adapter).await;
                if removed > 0 {
                    info!("Flushed {removed} producers for adapter {adapter}");
                }
                false
            }
            BusEvent::ShuttingDown { .. } => true,
            _ => false,
        }
    }

    /// Admit a document into the store.
    ///
    /// The whole read-modify-write runs under one write guard with no `.await` inside it:
    /// [`admit`] is pure and synchronous, so there is no lock-across-await, and no window in
    /// which a concurrent publication could be compared against a predecessor that is about
    /// to be replaced.
    pub async fn ingest(&self, document: ProducerDocument) -> Admission {
        let key = ProducerKey::of(&document);
        let producer_type = document.producer.producer_type.clone();
        let admission = {
            let mut store = self.store.write().await;
            let previous = store
                .producers
                .get(&key)
                .map(|entry| entry.document.clone());
            let admission = admit(previous.as_deref(), document);

            match &admission {
                Admission::Admitted(admitted) => {
                    let mut lanes = store
                        .producers
                        .get(&key)
                        .map(|entry| entry.lanes.clone())
                        .unwrap_or_default();
                    witness_lanes(&mut lanes, &admitted.document);
                    store.producers.insert(
                        key.clone(),
                        Entry {
                            document: Arc::new(admitted.document.clone()),
                            lanes,
                            repairs: admitted.repairs.clone(),
                        },
                    );
                }
                Admission::Refused(refusal) => {
                    // Retained rather than replacing anything. The previously admitted
                    // document, if there is one, keeps being served. The publishing
                    // adapter's identity is retained with it, because a refusal on a first
                    // publication has no producer entry to be flushed alongside.
                    store.refusals.insert(
                        key.clone(),
                        RefusalRecord {
                            refusal: refusal.clone(),
                            producer_type: producer_type.clone(),
                        },
                    );
                }
            }
            admission
        };

        // Reporting happens after the guard is released.
        match &admission {
            Admission::Admitted(admitted) => {
                if !admitted.repairs.is_empty() {
                    warn!(
                        producer = %key.producer_id,
                        repairs = admitted.repairs.len(),
                        "producer document published incoherent intent; entries demoted"
                    );
                }
                self.emit(AdaptiveEvent::SnapshotAdmitted {
                    key,
                    revisions: admitted.document.revisions,
                    repairs: admitted.repairs.len(),
                });
            }
            Admission::Refused(refusal) => {
                warn!(
                    producer = %key.producer_id,
                    refusal = ?refusal,
                    "producer document refused; the previous snapshot stands"
                );
                self.emit(AdaptiveEvent::SnapshotRefused {
                    key,
                    refusal: refusal.clone(),
                });
            }
        }

        admission
    }

    /// Remove every target of one producer. Returns how many entries went.
    ///
    /// Removal is not disconnection. A producer that has lost its transport keeps
    /// publishing documents marked `stale`, and those stay visible as last-known.
    ///
    /// The retained refusal goes with the producer. Refusals outlive *successes*, so a
    /// producer author can see that its documents were dropped even after one lands — but
    /// outliving the producer itself would make `refusals()` name something that no longer
    /// exists, and a surface listing "producers with problems" would show a ghost.
    pub async fn remove(&self, producer_id: &str) -> usize {
        let mut store = self.store.write().await;
        let doomed: Vec<ProducerKey> = store
            .producers
            .keys()
            .filter(|key| key.producer_id == producer_id)
            .cloned()
            .collect();
        for key in &doomed {
            store.producers.remove(key);
        }
        store
            .refusals
            .retain(|key, _| key.producer_id != producer_id);
        doomed.len()
    }

    /// Drop every producer belonging to an adapter that is stopping.
    ///
    /// Matched on `producer_type` or on the `adapter:` id prefix, mirroring how
    /// `ZoneAggregator` flushes zones, so a stopped adapter cannot leave ghost producers
    /// that no longer correspond to anything running.
    async fn flush_adapter(&self, adapter: &str) -> usize {
        let prefix = format!("{adapter}:");
        let mut store = self.store.write().await;
        let doomed: Vec<ProducerKey> = store
            .producers
            .iter()
            .filter(|(key, entry)| {
                entry.document.producer.producer_type == adapter
                    || key.producer_id.starts_with(&prefix)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in &doomed {
            store.producers.remove(key);
        }
        // A refusal must not outlive the adapter that caused it, and a first-publication
        // refusal has no producer entry to be found among `doomed`. Matched the same way
        // the producers are: by producer type, or by the `adapter:` id prefix.
        store.refusals.retain(|key, record| {
            !(doomed.iter().any(|gone| gone == key)
                || record.producer_type == adapter
                || key.producer_id.starts_with(&prefix))
        });
        doomed.len()
    }

    async fn record_lag(&self, missed: u64) {
        {
            let mut store = self.store.write().await;
            store.missed_updates = store.missed_updates.saturating_add(missed);
        }
        warn!(
            missed,
            "internal adaptive bus lagged; producer revisions were skipped. Documents are \
             whole snapshots, so the store converges on the next publication"
        );
    }

    fn emit(&self, event: AdaptiveEvent) {
        if let Some(adaptive) = self.adaptive.as_ref() {
            adaptive.publish(event);
        }
    }

    fn project(&self, store: &Store, key: &ProducerKey, entry: &Entry) -> ProducerSnapshot {
        ProducerSnapshot {
            key: key.clone(),
            document: entry.document.clone(),
            lanes: entry.lanes.clone(),
            presence: if entry.document.stale {
                ProducerPresence::LastKnown
            } else {
                ProducerPresence::Live
            },
            repairs: entry.repairs.clone(),
            missed_updates: store.missed_updates,
            last_refusal: store.refusals.get(key).map(|r| r.refusal.clone()),
        }
    }

    /// One producer's snapshot, read under a single guard so it cannot be a mixture.
    pub async fn snapshot(&self, key: &ProducerKey) -> Option<ProducerSnapshot> {
        let store = self.store.read().await;
        store
            .producers
            .get(key)
            .map(|entry| self.project(&store, key, entry))
    }

    /// Every producer's snapshot, in deterministic key order.
    pub async fn snapshots(&self) -> Vec<ProducerSnapshot> {
        let store = self.store.read().await;
        store
            .producers
            .iter()
            .map(|(key, entry)| self.project(&store, key, entry))
            .collect()
    }

    /// How many producer entries are held.
    pub async fn producer_count(&self) -> usize {
        self.store.read().await.producers.len()
    }

    /// Every retained refusal, in deterministic key order.
    pub async fn refusals(&self) -> Vec<(ProducerKey, AdmissionRefusal)> {
        let store = self.store.read().await;
        store
            .refusals
            .iter()
            .map(|(key, record)| (key.clone(), record.refusal.clone()))
            .collect()
    }

    /// Updates dropped to internal-bus lag since start.
    pub async fn missed_updates(&self) -> u64 {
        self.store.read().await.missed_updates
    }
}

/// Fold a document's lane health into the witness map.
///
/// A `last_success` is retained until the producer publishes a newer one. A poll that fails
/// publishes a lane with no success timestamp, and forgetting the previous one would blank
/// exactly the information a user needs to judge how stale a reading is.
///
/// **Ordering comes from admission, not from comparing timestamps.** An earlier draft
/// advanced `last_success` only when the incoming string sorted after the held one, which
/// looked like a monotonicity guard and was in fact a bug: [`Timestamp`] is documented as
/// RFC 3339 but its format is not pinned, so `…:21.500Z` sorts *before* `…:21Z` (`.` is
/// 0x2E, `Z` is 0x5A) and a `+00:00` offset does not compare with a `Z` at all. The guard
/// was also unnecessary — [`admit`] refuses any document that regresses in epoch or
/// revision, so an admitted document is never older than the one it replaces. Taking the
/// producer's latest word is both simpler and correct.
fn witness_lanes(
    witnesses: &mut BTreeMap<TransportLane, LaneWitness>,
    document: &ProducerDocument,
) {
    for health in &document.lanes {
        let witness = witnesses
            .entry(health.lane.clone())
            .or_insert_with(|| LaneWitness {
                lane: health.lane.clone(),
                state: health.state.clone(),
                last_success: None,
                last_error: None,
                observed_in_epoch: document.producer.epoch,
            });
        witness.state = health.state.clone();
        if health.last_success.is_some() {
            witness.last_success.clone_from(&health.last_success);
            witness.observed_in_epoch = document.producer.epoch;
        }
        if health.last_error.is_some() {
            witness.last_error.clone_from(&health.last_error);
        }
    }
}
