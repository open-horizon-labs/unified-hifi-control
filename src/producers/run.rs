//! Adapter run identity, and why no bus event can supply it.
//!
//! An adapter starts, publishes producer documents, stops, and may start again. The aggregator
//! must distinguish "from the run that is currently live" from "from a run that has ended",
//! because admitting the latter resurrects state nothing stands behind.
//!
//! ## Why a public-bus event cannot be the boundary
//!
//! [`crate::bus::BusEvent`] is a broadcast channel independent of the aggregator's command
//! inbox, so nothing orders the two. A publication from run *N* can be read after any number of
//! public lifecycle events, and an `AdapterStopping` belonging to run *N* can be read after run
//! *N+1* is already publishing. Every scheme that treats a public event as "everything before
//! this is stale" is defeated by one of those two orderings.
//!
//! ## What is authoritative
//!
//! Identity allocated **synchronously** and carried *in the command*. [`AdapterRuns::begin`]
//! takes a lock, allocates a monotonically increasing [`AdapterRunId`], and records it as that
//! adapter's live run before it returns — so "run *N+1* is live" is already true before run
//! *N+1* enqueues anything. Ordering comes from a counter under a lock, never from the relative
//! delivery order of two channels.
//!
//! ## Liveness is a lease, and it is RAII
//!
//! [`AdapterRun`] ends its run when dropped, which is what makes cancellation and panic safe: a
//! `tokio::spawn`ed adapter that is aborted or unwinds runs `Drop` without cooperating. `Drop`
//! cannot `await`, so it performs the one operation correctness requires:
//!
//! It marks the run ended in the registry **synchronously**. A reader immediately projects the
//! run's producers as last-known, and a queued command from it is refused at application time.
//!
//! `Drop` deliberately sends no cleanup command: it cannot await bounded capacity, and
//! `try_send` would turn a lossless lifecycle into a best-effort one. Diagnostics are retained;
//! the store prevents an older run's late refusal from replacing a newer run's evidence. The
//! lease holds no sender, so a live lease cannot keep the actor's inbox open after every
//! publisher handle is gone.
//!
//! RAII covers task cancellation/abort and panic-unwind, because those drop the future holding
//! the lease. It cannot cover `std::mem::forget`, process termination, or `panic = "abort"`.
//! [`AdapterRuns::begin`] therefore supersedes the previous run unconditionally as the backstop:
//! an adapter that died without dropping its lease cannot keep its old run live past reconnect.
//!
//! ## Run identity is not producer epoch
//!
//! Two lifecycles, deliberately apart:
//!
//! * [`AdapterRunId`] is **ours** — one connection attempt by one adapter in this process. It
//!   says nothing about the engine on the other side.
//! * `ProducerEpoch` is the **producer's** — its own restart counter, which only it can bump.
//!
//! An adapter reconnecting to an unchanged engine is a new run at the *same* epoch and must be
//! able to republish, so adapter lifecycle never bars an epoch. A producer that genuinely
//! restarted announces a higher one.

use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

/// Identity of one run of one adaptive publisher.
///
/// Monotonic across the process rather than per adapter, so two ids are never equal for
/// different runs even if an adapter name is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct AdapterRunId(u64);

/// Where one command came from.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PublicationOrigin {
    /// The publishing adapter, matching `BusEvent`'s `adapter` spelling.
    pub(crate) adapter: String,
    /// Which run of it.
    pub(crate) run: AdapterRunId,
}

impl PublicationOrigin {
    /// The publishing adapter.
    pub fn adapter(&self) -> &str {
        &self.adapter
    }

    /// The allocator-issued run identity.
    pub fn run(&self) -> AdapterRunId {
        self.run
    }

    /// Whether this adapter namespace owns `producer_id`.
    pub(crate) fn owns(&self, producer_id: &str) -> bool {
        producer_id
            .split_once(':')
            .is_some_and(|(namespace, local)| namespace == self.adapter && !local.is_empty())
    }
}

/// A live lease on one run of one adapter.
///
/// Deliberately **not** `Clone`: a clone would end the run when the first copy dropped. Held by
/// the publisher for the lifetime of its connection; dropping it ends the run.
#[derive(Debug)]
pub struct AdapterRun {
    origin: PublicationOrigin,
    runs: std::sync::Arc<AdapterRuns>,
}

impl AdapterRun {
    /// The origin to stamp commands with.
    pub fn origin(&self) -> &PublicationOrigin {
        &self.origin
    }

    /// Which adapter.
    pub fn adapter(&self) -> &str {
        &self.origin.adapter
    }

    /// Which run.
    pub fn id(&self) -> AdapterRunId {
        self.origin.run
    }
}

impl Drop for AdapterRun {
    fn drop(&mut self) {
        // Synchronous and unconditional. Cleanup is a projection concern; Drop must never
        // turn the lossless command inbox into a best-effort path.
        self.runs.mark_ended(&self.origin);
    }
}

#[derive(Debug, Default)]
struct RunState {
    next: u64,
    live: BTreeMap<String, AdapterRunId>,
}

/// Allocates adapter runs and tracks which one is live per adapter.
///
/// Read by the aggregator inside a single turn, with no `.await` between the read and the
/// mutation it authorises — which is why the check-then-act race the previous design had cannot
/// be expressed here.
#[derive(Debug, Default)]
pub struct AdapterRuns {
    state: Mutex<RunState>,
}

impl AdapterRuns {
    /// Begin a run for `adapter`, live before this returns.
    ///
    /// Supersedes any previous run for the same adapter without requiring it to have ended: an
    /// adapter that died without cleanup must not keep its old run live, or its stragglers
    /// would stay admissible forever.
    pub fn begin(self: &std::sync::Arc<Self>, adapter: &str) -> AdapterRun {
        assert!(
            !adapter.is_empty() && !adapter.contains(':'),
            "adapter namespace must be non-empty and contain no colon"
        );
        let run = {
            let mut state = self.lock();
            state.next = state.next.wrapping_add(1);
            assert_ne!(state.next, 0, "adapter run identity space exhausted");
            let run = AdapterRunId(state.next);
            state.live.insert(adapter.to_string(), run);
            run
        };
        AdapterRun {
            origin: PublicationOrigin {
                adapter: adapter.to_string(),
                run,
            },
            runs: self.clone(),
        }
    }

    /// End `run` if it is still the live run of its adapter.
    pub fn end(&self, run: &AdapterRun) -> bool {
        self.mark_ended(run.origin())
    }

    /// The live run for `adapter`, if it has one.
    pub fn active(&self, adapter: &str) -> Option<AdapterRunId> {
        self.lock().live.get(adapter).copied()
    }

    /// Whether `origin` names the live run of its adapter.
    pub fn is_active(&self, origin: &PublicationOrigin) -> bool {
        self.active(&origin.adapter) == Some(origin.run)
    }

    /// Execute one store mutation while the origin's liveness decision cannot change.
    ///
    /// The closure must be synchronous. Holding this guard across the entire mutation is the
    /// linearization boundary: `begin` and `Drop` cannot interleave between check and commit.
    pub(super) fn with_active<R>(
        &self,
        origin: &PublicationOrigin,
        apply: impl FnOnce() -> R,
    ) -> Result<R, Option<AdapterRunId>> {
        let state = self.lock();
        let active = state.live.get(&origin.adapter).copied();
        if active != Some(origin.run) {
            return Err(active);
        }
        Ok(apply())
    }

    /// Read projected state while adapter liveness is stable.
    pub(super) fn with_liveness<R>(
        &self,
        project: impl FnOnce(&BTreeMap<String, AdapterRunId>) -> R,
    ) -> R {
        let state = self.lock();
        project(&state.live)
    }

    /// End the run named by `origin`, but only if it is still the live one.
    ///
    /// The guard that makes late cleanup harmless: an end belonging to run *N*, applied after
    /// run *N+1* has begun, finds *N* is not live and does nothing.
    pub fn mark_ended(&self, origin: &PublicationOrigin) -> bool {
        let mut state = self.lock();
        if state.live.get(&origin.adapter) == Some(&origin.run) {
            state.live.remove(&origin.adapter);
            return true;
        }
        false
    }

    fn lock(&self) -> MutexGuard<'_, RunState> {
        // The guarded value is a counter and a map; a panic cannot leave either torn, so
        // recovering beats propagating a panic into the aggregator's event loop.
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
