//! Aggregator-owned adaptive producer state.
//!
//! The aggregator is the single owner of every current producer document, exactly as it is
//! for zones (`docs/ARCHITECTURE.md`). Adapters publish through the bounded
//! [`super::AdaptiveHandle`]; nothing queries an adapter; every read is served from an admitted
//! snapshot.
//!
//! Three properties are worth stating because they are not obvious from the types:
//!
//! * **Snapshots are atomic.** One snapshot is one whole published document. A consumer can
//!   never see a mode read at revision 41 beside an enumeration read at revision 43.
//! * **Lane history lives beside the document, never inside it.** [`LaneWitness`] is
//!   aggregator-observed and labelled as such. Merging a remembered timestamp into the
//!   producer's own `lanes` would put words in its mouth.
//! * **A refusal is retained, not merely logged.** A publisher may request an immediate actor
//!   verdict, but retained diagnostics remain necessary for detached publication and operators.

use chrono::{DateTime, FixedOffset};
use std::collections::BTreeMap;
use std::sync::{Arc, RwLock, RwLockReadGuard, RwLockWriteGuard};
use tracing::warn;

use super::admission::{admit, Admission, AdmissionRefusal, IntentRepair, ProducerKey};
use super::event::{AdaptiveEvent, SharedAdaptiveBus};
use super::run::{AdapterRuns, PublicationOrigin};
use crate::adaptive::{
    LaneState, ProducerDocument, ProducerEpoch, Reason, Timestamp, TransportLane,
};
#[cfg(debug_assertions)]
use crate::bus::BusEvent;

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
    /// The newest success ever seen for this lane. Monotonic **as an instant**: never
    /// regresses, so neither a failing poll nor an advancing document that happens to report
    /// an older instant can erase the evidence that the lane worked five seconds ago.
    ///
    /// Compared with `chrono`, not as a string and not by arrival order — see
    /// [`witness_lanes`] for why each of those was tried and is wrong.
    pub last_success: Option<Timestamp>,
    /// The most recent error seen for this lane.
    pub last_error: Option<Reason>,
    /// The epoch in which the **currently retained** `last_success` was observed.
    ///
    /// Not "the epoch of the last document that mentioned this lane". The two differ exactly
    /// when a restart reports an instant older than one already witnessed: the older-instant
    /// document is ignored, and this keeps naming the epoch that actually observed the value
    /// still being shown. The pair moves together or not at all.
    ///
    /// Carrying a last-good timestamp across a producer restart is honest only if a consumer
    /// can tell that it predates the restart. Resetting it instead would discard true
    /// information; presenting it unqualified would imply it was observed by the process now
    /// running.
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
    /// The most recent refusal for this producer, retained across later successes.
    pub last_refusal: Option<AdmissionRefusal>,
}

struct Entry {
    document: Arc<ProducerDocument>,
    lanes: BTreeMap<TransportLane, LaneWitness>,
    repairs: Vec<IntentRepair>,
    /// The adapter run this document was admitted under, if it arrived through one.
    ///
    /// Presence is projected from this exact origin at read time. Public-bus stop events never
    /// flush adaptive state, so an ended run remains honestly visible as last-known.
    run: Option<PublicationOrigin>,
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
    tombstones: BTreeMap<ProducerKey, Tombstone>,
    producer_tombstones: BTreeMap<String, ProducerEpoch>,
    watermarks: BTreeMap<ProducerKey, Watermark>,
}

/// A retained refusal, with the exact adapter run that published it when one exists.
///
/// Public-bus stop hints are informational and never clear adaptive diagnostics. The origin is
/// retained solely to keep a delayed stale-run refusal from overwriting a newer live-run
/// refusal for the same key.
struct RefusalRecord {
    refusal: AdmissionRefusal,
    /// The rejected document's claimed epoch scopes diagnostic cleanup only. It is never used
    /// to advance a retirement floor or watermark.
    claimed_epoch: ProducerEpoch,
    origin: Option<PublicationOrigin>,
}

/// A producer retired by an explicit `ProducerRemoved`, and the epoch it held.
///
/// **Producer-epoch retirement only.** Adapter lifecycle does not create these, and nothing
/// clears them but the producer itself announcing a restart with a strictly higher epoch. An
/// earlier draft also set them on `AdapterStopping` and cleared them on `AdapterConnected`,
/// which was unsound twice over: the clear could be processed before a straggler that the
/// tombstone existed to stop, and requiring a higher epoch stranded any adapter that
/// reconnected to an engine that had never restarted. Adapter staleness is now
/// [`super::run`]'s job, and this is only about the producer's own identity.
struct Tombstone {
    /// Documents at this epoch or lower are refused; the producer said it was gone.
    epoch: ProducerEpoch,
}

/// Monotonic ordering evidence retained even when the visible snapshot is swept.
#[derive(Clone, Copy)]
struct Watermark {
    epoch: ProducerEpoch,
    revisions: crate::adaptive::DocumentRevisions,
}

impl Store {
    /// Keep the newest run's diagnostic for a key. A delayed stale run may be observed later,
    /// but it must not overwrite the current run's refusal and then erase its evidence.
    fn record_refusal(&mut self, key: ProducerKey, incoming: RefusalRecord) {
        let owner = key.producer_id.split_once(':').map(|(owner, _)| owner);
        let authority = |origin: &Option<PublicationOrigin>| match origin {
            Some(origin) if Some(origin.adapter.as_str()) == owner => 2_u8,
            None => 1,
            Some(_) => 0,
        };
        let replace = self.refusals.get(&key).is_none_or(|held| {
            match authority(&incoming.origin).cmp(&authority(&held.origin)) {
                std::cmp::Ordering::Greater => true,
                std::cmp::Ordering::Less => false,
                std::cmp::Ordering::Equal => match (&held.origin, &incoming.origin) {
                    (Some(held), Some(incoming)) if held.adapter == incoming.adapter => {
                        incoming.run >= held.run
                    }
                    // Diagnostics from two non-owning adapters have equal lack of authority;
                    // keep the first rather than letting a process-global run id arbitrate.
                    (Some(_), Some(_)) => false,
                    (Some(_), None) => false,
                    (None, _) => true,
                },
            }
        });
        if replace {
            self.refusals.insert(key, incoming);
        }
    }

    /// Whether a document may proceed to admission, given what has been retired.
    ///
    /// `Err(epoch)` is a document for a producer that said it was gone. A strictly newer epoch
    /// is a restart and passes the retained floor. The floor remains monotonic evidence so a
    /// later straggler under this or another target key is still refused.
    fn admit_past_retirement(
        &self,
        key: &ProducerKey,
        incoming: ProducerEpoch,
    ) -> Result<(), ProducerEpoch> {
        let key_epoch = self.tombstones.get(key).map(|t| t.epoch);
        let producer_epoch = self.producer_tombstones.get(&key.producer_id).copied();
        let retired_at = match (key_epoch, producer_epoch) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        let Some(retired_at) = retired_at else {
            return Ok(());
        };
        if incoming <= retired_at {
            return Err(retired_at);
        }
        // Floors are monotonic evidence, not gates to consume. A higher-epoch restart passes,
        // but cannot erase the floor for a later straggler under this or an unseen target key.
        Ok(())
    }

    /// Retire `key` at `epoch`, keeping the highest epoch if it is already retired.
    ///
    /// Highest wins so that a later retirement cannot lower the bar a straggler has to clear.
    fn retire(&mut self, key: ProducerKey, epoch: ProducerEpoch) {
        let tombstone = self.tombstones.entry(key).or_insert(Tombstone { epoch });
        if epoch > tombstone.epoch {
            tombstone.epoch = epoch;
        }
    }

    fn retire_producer_through(
        &mut self,
        producer_id: &str,
        epoch: ProducerEpoch,
    ) -> ProducerEpoch {
        let retired = self
            .producer_tombstones
            .entry(producer_id.to_string())
            .or_insert(epoch);
        if epoch > *retired {
            *retired = epoch;
        }
        *retired
    }

    fn watermark_refusal(
        &self,
        key: &ProducerKey,
        epoch: ProducerEpoch,
        revisions: crate::adaptive::DocumentRevisions,
    ) -> Option<AdmissionRefusal> {
        let watermark = self.watermarks.get(key)?;
        if epoch < watermark.epoch {
            return Some(AdmissionRefusal::EpochRegressed {
                previous: watermark.epoch,
                incoming: epoch,
            });
        }
        if epoch == watermark.epoch && revisions.regresses_from(&watermark.revisions) {
            return Some(AdmissionRefusal::RevisionRegressed {
                previous: watermark.revisions,
                incoming: revisions,
            });
        }
        None
    }

    fn observe_watermark(
        &mut self,
        key: ProducerKey,
        epoch: ProducerEpoch,
        revisions: crate::adaptive::DocumentRevisions,
    ) {
        let replace = self.watermarks.get(&key).is_none_or(|held| {
            epoch > held.epoch || (epoch == held.epoch && revisions.advances_over(&held.revisions))
        });
        if replace {
            self.watermarks.insert(key, Watermark { epoch, revisions });
        }
    }

    fn admitted_epoch_of_producer(&self, producer_id: &str) -> Option<ProducerEpoch> {
        self.producers
            .iter()
            .filter(|(key, _)| key.producer_id == producer_id)
            .map(|(_, entry)| entry.document.producer.epoch)
            .max()
    }
}

/// Owns every current adaptive producer document.
pub struct ProducerAggregator {
    store: Arc<RwLock<Store>>,
    adaptive: SharedAdaptiveBus,
    /// The same registry publishers allocate runs from, so staleness is judged against
    /// synchronously-maintained state rather than event arrival order.
    runs: Arc<AdapterRuns>,
}

impl ProducerAggregator {
    /// Construct with no bus, for callers that drive [`ProducerAggregator::ingest`]
    /// directly.
    #[cfg(debug_assertions)]
    pub fn detached() -> Self {
        Self::detached_with_runs(Arc::new(AdapterRuns::default()))
    }

    #[cfg(debug_assertions)]
    pub(super) fn detached_with_runs(runs: Arc<AdapterRuns>) -> Self {
        Self::detached_with_runs_and_events(runs, super::event::create_adaptive_bus())
    }

    pub(super) fn detached_with_runs_and_events(
        runs: Arc<AdapterRuns>,
        adaptive: SharedAdaptiveBus,
    ) -> Self {
        Self {
            store: Arc::new(RwLock::new(Store::default())),
            adaptive,
            runs,
        }
    }

    /// Handle one public-bus event. Returns whether the loop should stop.
    ///
    /// Public-bus events are informational only for adaptive state.
    ///
    /// Retained as a debug-profile seam for the frozen bus-isolation probes. Release lifecycle
    /// authority is exclusively the actor command channel and run leases; see the explicit
    /// profile boundary on the re-export in [`super`].
    #[cfg(debug_assertions)]
    pub async fn apply_bus_event(&self, event: BusEvent) -> bool {
        matches!(event, BusEvent::ShuttingDown { .. })
    }

    /// Admit a document with no adapter-run context.
    ///
    /// The direct path, for callers that drive the aggregator themselves rather than through
    /// the debug test seam. Run staleness cannot be judged without an origin, so this skips that
    /// check — every other rule still applies. Production ingress goes through
    /// [`super::AdaptiveHandle`].
    #[cfg(debug_assertions)]
    pub async fn ingest(&self, document: ProducerDocument) -> Admission {
        self.admit_document(None, document).await
    }

    /// Admit a document published by a known adapter run.
    ///
    /// Refuses anything from a run the registry no longer considers live, which is what makes
    /// a straggler harmless however long it sat in the channel. See [`super::run`].
    pub async fn ingest_from(
        &self,
        origin: &PublicationOrigin,
        document: ProducerDocument,
    ) -> Admission {
        self.admit_document(Some(origin), document).await
    }

    /// The whole read-modify-write runs under one write guard with no `.await` inside it:
    /// [`admit`] is pure and synchronous, so there is no lock-across-await, and no window in
    /// which a concurrent publication could be compared against a predecessor that is about
    /// to be replaced.
    async fn admit_document(
        &self,
        origin: Option<&PublicationOrigin>,
        document: ProducerDocument,
    ) -> Admission {
        let key = ProducerKey::of(&document);
        let claimed_epoch = document.producer.epoch;
        if let Some(origin) = origin {
            if !origin.owns(&key.producer_id) {
                let refusal = AdmissionRefusal::ProducerOwnershipMismatch {
                    adapter: origin.adapter.clone(),
                    producer_id: key.producer_id.clone(),
                };
                // This is a command-authority failure by another adapter, not a defect in the
                // named producer's document. Return it and emit an egress hint, but never attach
                // it to the owner's retained diagnostic slot.
                warn!(
                    adapter = %origin.adapter,
                    producer = %key.producer_id,
                    "adapter attempted to publish outside its producer namespace"
                );
                self.emit(AdaptiveEvent::SnapshotRefused {
                    key,
                    refusal: refusal.clone(),
                });
                return Admission::Refused(refusal);
            }
        }
        let admission = match origin {
            Some(origin) => match self
                .runs
                .with_active(origin, || self.admit_active(Some(origin), document))
            {
                Ok(admission) => admission,
                Err(active) => {
                    let refusal = AdmissionRefusal::StaleAdapterRun {
                        adapter: origin.adapter.clone(),
                        published_by: origin.run,
                        active,
                    };
                    warn!(
                        adapter = %origin.adapter,
                        published_by = ?origin.run,
                        "document published by an adapter run that is no longer live; refused"
                    );
                    self.retain_refusal(
                        key.clone(),
                        refusal.clone(),
                        claimed_epoch,
                        Some(origin.clone()),
                    );
                    Admission::Refused(refusal)
                }
            },
            None => self.admit_active(None, document),
        };

        // Reporting happens after both the lifecycle and store guards are released.
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

    /// Apply one admission after lifecycle authority has been established.
    ///
    /// For run-stamped ingress this is called *inside* [`AdapterRuns::with_active`], so the
    /// liveness decision and this complete read-modify-write are one linearizable turn.
    fn admit_active(
        &self,
        origin: Option<&PublicationOrigin>,
        document: ProducerDocument,
    ) -> Admission {
        let key = ProducerKey::of(&document);
        let incoming_epoch = document.producer.epoch;
        let mut store = self.write_store();

        if let Err(retired_at) = store.admit_past_retirement(&key, incoming_epoch) {
            let refusal = AdmissionRefusal::ProducerRetired {
                retired_at,
                incoming: incoming_epoch,
            };
            store.record_refusal(
                key,
                RefusalRecord {
                    refusal: refusal.clone(),
                    claimed_epoch: incoming_epoch,
                    origin: origin.cloned(),
                },
            );
            return Admission::Refused(refusal);
        }

        if let Some(refusal) = store.watermark_refusal(&key, incoming_epoch, document.revisions) {
            store.record_refusal(
                key,
                RefusalRecord {
                    refusal: refusal.clone(),
                    claimed_epoch: incoming_epoch,
                    origin: origin.cloned(),
                },
            );
            return Admission::Refused(refusal);
        }

        // Adapter reconnect is not a producer revision. Compare against the held document
        // even when the publishing run changed, otherwise a reconnect could change content
        // under the same epoch/revision and make downstream revision caches lie.
        let previous = store
            .producers
            .get(&key)
            .map(|entry| entry.document.clone());
        let admission = admit(previous.as_deref(), document);

        match &admission {
            Admission::Admitted(admitted) => {
                store.observe_watermark(
                    key.clone(),
                    admitted.document.producer.epoch,
                    admitted.document.revisions,
                );
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
                        run: origin.cloned(),
                    },
                );
            }
            Admission::Refused(refusal) => {
                store.record_refusal(
                    key,
                    RefusalRecord {
                        refusal: refusal.clone(),
                        claimed_epoch: incoming_epoch,
                        origin: origin.cloned(),
                    },
                );
            }
        }
        admission
    }

    fn retain_refusal(
        &self,
        key: ProducerKey,
        refusal: AdmissionRefusal,
        claimed_epoch: ProducerEpoch,
        origin: Option<PublicationOrigin>,
    ) {
        self.write_store().record_refusal(
            key,
            RefusalRecord {
                refusal,
                claimed_epoch,
                origin,
            },
        );
    }

    /// Remove every target of one producer, with no adapter-run context.
    ///
    /// Debug-test direct path; production retirement goes through [`super::AdaptiveHandle`].
    #[cfg(debug_assertions)]
    pub async fn remove(&self, producer_id: &str) -> usize {
        self.retire_producer(None, producer_id, None).await.0
    }

    /// Remove every target of one producer, as retired by a known adapter run.
    ///
    /// A removal from a run that is no longer live does nothing: it is as stale as a
    /// publication from that run, and acting on it would retire a producer belonging to the
    /// run that replaced it.
    #[cfg(debug_assertions)]
    pub async fn remove_from(&self, origin: &PublicationOrigin, producer_id: &str) -> usize {
        self.retire_producer(Some(origin), producer_id, None)
            .await
            .0
    }

    /// Apply a retirement whose run authority was committed atomically with queue insertion.
    pub(super) fn retire_committed(
        &self,
        origin: &PublicationOrigin,
        producer_id: &str,
        epoch: ProducerEpoch,
    ) -> usize {
        debug_assert!(origin.owns(producer_id));
        let (removed, applied_floor) = self.retire_active(producer_id, Some(epoch));
        self.emit(AdaptiveEvent::ProducerRetired {
            producer_id: producer_id.to_string(),
            retired_through: applied_floor.unwrap_or(epoch),
            removed,
        });
        removed
    }

    /// Retire a producer by identity. Returns how many entries went.
    ///
    /// Removal is not disconnection. A producer that has lost its transport keeps publishing
    /// documents marked `stale`, and those stay visible as last-known.
    ///
    /// **This is producer-epoch retirement, and it is the only thing that writes a tombstone.**
    /// The producer said it is gone, so only the producer can contradict that, by announcing a
    /// restart with a strictly higher epoch. Adapter lifecycle is a separate question answered
    /// by [`super::run`], and deliberately cannot clear this: an adapter reconnecting does not
    /// speak for whether a producer still exists.
    ///
    /// The retained refusal goes with the producer. Refusals outlive *successes*, so a producer
    /// author can see that its documents were dropped even after one lands — but outliving the
    /// producer itself would make `refusals()` name something that no longer exists, and a
    /// surface listing "producers with problems" would show a ghost.
    #[cfg(debug_assertions)]
    async fn retire_producer(
        &self,
        origin: Option<&PublicationOrigin>,
        producer_id: &str,
        explicit_epoch: Option<ProducerEpoch>,
    ) -> (usize, Option<ProducerEpoch>) {
        if let Some(origin) = origin {
            return self
                .runs
                .with_active(origin, || self.retire_active(producer_id, explicit_epoch))
                .unwrap_or((0, None));
        }
        self.retire_active(producer_id, explicit_epoch)
    }

    fn retire_active(
        &self,
        producer_id: &str,
        explicit_epoch: Option<ProducerEpoch>,
    ) -> (usize, Option<ProducerEpoch>) {
        let mut store = self.write_store();
        let retired_through =
            explicit_epoch.or_else(|| store.admitted_epoch_of_producer(producer_id));
        let applied_floor =
            retired_through.map(|epoch| store.retire_producer_through(producer_id, epoch));
        let remove_all = explicit_epoch.is_none();
        let producer_keys: Vec<ProducerKey> = store
            .producers
            .iter()
            .filter(|(key, entry)| {
                key.producer_id == producer_id
                    && (remove_all
                        || applied_floor
                            .is_some_and(|floor| entry.document.producer.epoch <= floor))
            })
            .map(|(key, _)| key.clone())
            .collect();
        let refusal_keys: Vec<ProducerKey> = store
            .refusals
            .iter()
            .filter(|(key, record)| {
                key.producer_id == producer_id
                    && (remove_all
                        || applied_floor.is_some_and(|floor| record.claimed_epoch <= floor))
            })
            .map(|(key, _)| key.clone())
            .collect();
        let mut removed = 0usize;
        for key in &producer_keys {
            if store.producers.remove(key).is_some() {
                removed += 1;
            }
            if let Some(epoch) = applied_floor {
                store.retire(key.clone(), epoch);
            }
        }
        for key in refusal_keys {
            store.refusals.remove(&key);
            if let Some(epoch) = applied_floor {
                store.retire(key, epoch);
            }
        }
        (removed, applied_floor)
    }

    /// The run registry this aggregator judges publications against.
    ///
    /// Exposed so a publisher wired to a detached aggregator, and the tests that stand in for
    /// one, allocate runs from the same registry rather than a parallel one.
    #[cfg(debug_assertions)]
    pub fn runs(&self) -> &Arc<AdapterRuns> {
        &self.runs
    }

    fn emit(&self, event: AdaptiveEvent) {
        self.adaptive.publish(event);
    }

    fn project(
        &self,
        store: &Store,
        key: &ProducerKey,
        entry: &Entry,
        run_is_live: bool,
    ) -> ProducerSnapshot {
        ProducerSnapshot {
            key: key.clone(),
            document: entry.document.clone(),
            lanes: entry.lanes.clone(),
            presence: if entry.document.stale || !run_is_live {
                ProducerPresence::LastKnown
            } else {
                ProducerPresence::Live
            },
            repairs: entry.repairs.clone(),
            last_refusal: store.refusals.get(key).map(|r| r.refusal.clone()),
        }
    }

    /// One producer's snapshot, read under a single guard so it cannot be a mixture.
    #[cfg(debug_assertions)]
    pub async fn snapshot(&self, key: &ProducerKey) -> Option<ProducerSnapshot> {
        self.snapshot_now(key)
    }

    /// Every producer's snapshot, in deterministic key order.
    #[cfg(debug_assertions)]
    pub async fn snapshots(&self) -> Vec<ProducerSnapshot> {
        self.snapshots_now()
    }

    /// How many producer entries are held.
    #[cfg(debug_assertions)]
    pub async fn producer_count(&self) -> usize {
        self.read_store().producers.len()
    }

    /// Every retained refusal, in deterministic key order.
    #[cfg(debug_assertions)]
    pub async fn refusals(&self) -> Vec<(ProducerKey, AdmissionRefusal)> {
        self.refusals_now()
    }

    pub(super) fn snapshot_now(&self, key: &ProducerKey) -> Option<ProducerSnapshot> {
        self.runs.with_liveness(|live| {
            let store = self.read_store();
            store.producers.get(key).map(|entry| {
                let run_is_live = entry
                    .run
                    .as_ref()
                    .is_none_or(|origin| live.get(&origin.adapter).copied() == Some(origin.run));
                self.project(&store, key, entry, run_is_live)
            })
        })
    }

    pub(super) fn snapshots_now(&self) -> Vec<ProducerSnapshot> {
        self.runs.with_liveness(|live| {
            let store = self.read_store();
            store
                .producers
                .iter()
                .map(|(key, entry)| {
                    let run_is_live = entry.run.as_ref().is_none_or(|origin| {
                        live.get(&origin.adapter).copied() == Some(origin.run)
                    });
                    self.project(&store, key, entry, run_is_live)
                })
                .collect()
        })
    }

    pub(super) fn refusals_now(&self) -> Vec<(ProducerKey, AdmissionRefusal)> {
        let store = self.read_store();
        store
            .refusals
            .iter()
            .map(|(key, record)| (key.clone(), record.refusal.clone()))
            .collect()
    }

    fn read_store(&self) -> RwLockReadGuard<'_, Store> {
        self.store
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn write_store(&self) -> RwLockWriteGuard<'_, Store> {
        self.store
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Fold a document's lane health into the witness map.
///
/// A `last_success` is retained until the producer publishes a **genuinely newer instant**. A
/// poll that fails publishes a lane with no success timestamp, and forgetting the previous one
/// would blank exactly the information a user needs to judge how stale a reading is.
///
/// **Instants are compared as instants, never as strings, and never merely by arrival order.**
/// Two earlier drafts each got one half of this wrong:
///
/// * Comparing the raw strings looked like a monotonicity guard and was a bug. [`Timestamp`]
///   is documented as RFC 3339 but its spelling is not pinned, so `…:21.500Z` sorts *before*
///   `…:21Z` (`.` is 0x2E, `Z` is 0x5A) despite being 500ms later, and a `+02:00` offset does
///   not compare against a `Z` at all.
/// * Taking the producer's latest word unconditionally fixed that and broke the documented
///   contract instead. [`admit`] orders *documents* by epoch and revision, which says nothing
///   about the instants inside them: a producer that polls its lanes on separate schedules,
///   or re-reports a cached success, routinely publishes an advancing document carrying an
///   older `last_success`. "Newest ever seen" then quietly became "most recently mentioned".
///
/// So the comparison is `chrono`'s. It lives here rather than on [`Timestamp`] because
/// `src/adaptive/` is lint-enforced (`tests/adaptive_dependency_lint.rs`) to depend on nothing
/// but `serde`, `serde_json` and `std`, and this is server-side interpretation of a producer's
/// claim rather than part of the contract's shape.
///
/// **Unparseable instants never get here.** [`admit`] refuses any document carrying a
/// `last_success` that is not RFC 3339, so "admitted implies comparable" is an invariant rather
/// than a hope, and the witness never holds a value a consumer cannot interpret. The defensive
/// arms in [`supersedes`] are kept as belt-and-braces: they make the failure mode "keep what we
/// have" rather than "silently regress" if that guarantee is ever weakened, and there is
/// deliberately no lexical fallback, which is the first bug above.
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
        if let Some(offered) = &health.last_success {
            if supersedes(offered, witness.last_success.as_ref()) {
                witness.last_success = Some(offered.clone());
                // Moved together with the value, never independently: the epoch's whole job
                // is to say which process observed *this* instant. Advancing it while keeping
                // an older instant would claim the new process saw something it never did.
                witness.observed_in_epoch = document.producer.epoch;
            }
        }
        if health.last_error.is_some() {
            witness.last_error.clone_from(&health.last_error);
        }
    }
}

/// Whether `offered` is a strictly later instant than `held`.
///
/// `None` held means anything offered is an improvement on knowing nothing. The unparseable
/// arms are unreachable for any admitted document — [`admit`] refuses those — and are written
/// to fail safe rather than to be relied on: an instant that cannot be parsed cannot be shown
/// to be newer, so a known-good value stands.
fn supersedes(offered: &Timestamp, held: Option<&Timestamp>) -> bool {
    let Some(held) = held else {
        return true;
    };
    match (instant(offered), instant(held)) {
        (Some(offered), Some(held)) => offered > held,
        (Some(_), None) => true,
        (None, _) => false,
    }
}

/// One [`Timestamp`] as an instant, or `None` if it is not RFC 3339.
fn instant(timestamp: &Timestamp) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(timestamp.as_str()).ok()
}
