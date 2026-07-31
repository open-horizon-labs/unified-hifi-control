//! The one application service every surface goes through (#331).
//!
//! Three jobs, and the reason they are one type is that separating them is what lets them disagree:
//!
//! 1. **Routing.** A prefixed zone id resolves to a [`ProducerKey`] *only* from admitted aggregator
//!    state. There is no fallthrough arm. #328 records what a fallthrough costs: `hqplayer:Living
//!    Room` reached the Roon adapter with its prefix stripped, where it either failed or matched an
//!    unrelated Roon zone.
//! 2. **Projection.** [`super::surface::project`] over the snapshot the aggregator serves, for the
//!    profile the surface declared.
//! 3. **Submission.** A surface-neutral semantic command, executed by the registered executor for
//!    the routed producer's family — HQPlayer's being #329's bounded actor, reached through #375's
//!    single control-to-operation registry. A family with no executor is refused, never defaulted.
//!
//! ## Why the surface never sees the internals
//!
//! [`SemanticRefusal`] is a public, surface-neutral vocabulary. The HQPlayer preflight has
//! twenty-odd typed refusals naming lanes, chains and rate spellings; a browser and an MCP client
//! need to know *what to do next*, and the answer is one of ten things. The mapping is an
//! exhaustive `match`, so a new internal refusal cannot silently become an existing surface verdict.

use std::collections::BTreeMap;
use std::sync::Arc;

use async_trait::async_trait;

use crate::adaptive::{ApplyLane, ControlId, ControlValue, RevisionRef};
use crate::bus::events::PrefixedZoneId;

use super::surface::{project, SurfaceProfile, SurfaceProjection};
use super::{AdaptiveView, ProducerKey, ProducerSnapshot};

/// Why a zone id did not resolve to a producer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteRefusal {
    /// The string carries no recognized zone prefix.
    UnknownZonePrefix {
        /// The id the surface supplied.
        zone_id: String,
    },
    /// No admitted producer is bound to that zone.
    ///
    /// The refusal a fallthrough would have hidden. An unknown `hqplayer:` identity ends here.
    UnknownProducer {
        /// The id the surface supplied.
        zone_id: String,
    },
    /// More than one admitted producer claims that zone, so the surface must name a role.
    AmbiguousProducer {
        /// The id the surface supplied.
        zone_id: String,
        /// Every producer claiming it.
        candidates: Vec<ProducerKey>,
    },
}

/// A surface-neutral reason a semantic command was not admitted.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticRefusal {
    /// The correlation id is empty or too long to retain.
    CorrelationUnusable,
    /// The addressed producer is not the one the aggregator holds.
    ProducerMismatch,
    /// The caller's position is not current. Re-read the projection and retry.
    PositionStale {
        /// The position the caller cited.
        expected: RevisionRef,
        /// The position the aggregator serves.
        actual: RevisionRef,
    },
    /// The producer is last-known, or declares itself stale. Last-known data is not a write target.
    ProducerNotWritable,
    /// The transport lane that would carry the write is missing, down or too stale to mutate from.
    LaneUnusable,
    /// The control is unknown to the exact document, read-only, or not available for mutation.
    ControlNotWritable,
    /// The request contradicts the descriptor: wrong apply lane, wrong kind, wrong value shape.
    RequestMismatch,
    /// The value is not a currently selectable, sendable choice.
    ValueNotSelectable,
    /// The control has no verified semantic operation yet, so no truthful receipt is possible.
    NoVerifiedOperation,
    /// The coordinator would not reserve the operation: a conflicting correlation, a superseding
    /// operation, or a closed producer.
    NotReserved,
}

/// Why a command did not reach an executor, or was not admitted by one.
#[derive(Debug, Clone, PartialEq)]
pub enum CommandRefusal {
    /// The zone did not resolve.
    Route(RouteRefusal),
    /// The routed producer's family has no registered semantic executor.
    ///
    /// The alternative — trying the executor we happen to have — is the #328 defect with a
    /// different prefix.
    NoExecutorForProducer {
        /// The family that has none.
        producer_type: String,
    },
    /// The executor refused before writing anything.
    Refused(SemanticRefusal),
    /// The executor is shutting down and accepted nothing.
    ServiceClosed,
}

/// One surface's proposed semantic mutation, in producer terms only.
#[derive(Debug, Clone, PartialEq)]
pub struct SemanticCommand {
    /// The producer to address.
    pub key: ProducerKey,
    /// The position the surface projected. The executor fences against it.
    pub expected: RevisionRef,
    /// The semantic control.
    pub control: ControlId,
    /// The semantic value. Never a native index.
    pub requested: ControlValue,
    /// The apply lane the descriptor declared.
    pub lane: ApplyLane,
    /// Idempotency correlation, shared by every surface following this operation.
    pub correlation_id: String,
    /// Which surface submitted it.
    pub surface_id: String,
}

/// An admitted command, before its outcome converges.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandAcceptance {
    /// The operation the producer will report against.
    pub operation: crate::adaptive::OperationRecord,
    /// True when this correlation matched an operation already reserved: a retry, not a new write.
    pub duplicate: bool,
}

/// Executes semantic commands for one producer family.
#[async_trait]
pub trait SemanticCommandExecutor: Send + Sync {
    /// The `producer_type` this executor owns.
    fn producer_type(&self) -> &str;

    /// Submit one command.
    async fn submit(&self, command: SemanticCommand) -> Result<CommandAcceptance, CommandRefusal>;
}

/// The single surface-facing entry point to aggregator-owned producer state.
#[derive(Clone)]
pub struct ControlPlaneService {
    view: AdaptiveView,
    executors: BTreeMap<String, Arc<dyn SemanticCommandExecutor>>,
}

impl ControlPlaneService {
    /// Build a service over the aggregator's read-only view.
    pub fn new(view: AdaptiveView) -> Self {
        Self {
            view,
            executors: BTreeMap::new(),
        }
    }

    /// Register the executor for one producer family.
    pub fn with_executor(mut self, executor: Arc<dyn SemanticCommandExecutor>) -> Self {
        self.executors
            .insert(executor.producer_type().to_string(), executor);
        self
    }

    /// Every producer the aggregator currently holds.
    pub fn producers(&self) -> Vec<ProducerKey> {
        // RED stub.
        Vec::new()
    }

    /// Resolve a prefixed zone id to the producer bound to it.
    pub fn route(&self, zone_id: &str) -> Result<ProducerKey, RouteRefusal> {
        // RED stub: the naive first-match a fallthrough would have written.
        let _ = zone_id;
        self.view
            .snapshots()
            .first()
            .map(|snapshot| snapshot.key.clone())
            .ok_or_else(|| RouteRefusal::UnknownProducer {
                zone_id: zone_id.to_string(),
            })
    }

    /// The aggregator's snapshot for one producer.
    pub fn snapshot(&self, key: &ProducerKey) -> Option<ProducerSnapshot> {
        self.view.snapshot(key)
    }

    /// Project one producer for one surface.
    pub fn project(&self, key: &ProducerKey, profile: &SurfaceProfile) -> Option<SurfaceProjection> {
        self.view
            .snapshot(key)
            .map(|snapshot| project(&snapshot, profile))
    }

    /// Submit one semantic command through the executor that owns the routed producer's family.
    pub async fn submit(
        &self,
        command: SemanticCommand,
    ) -> Result<CommandAcceptance, CommandRefusal> {
        // RED stub.
        let _ = &command;
        Err(CommandRefusal::ServiceClosed)
    }
}

#[allow(dead_code)]
fn stub_unused(zone_id: &str) -> Option<PrefixedZoneId> {
    PrefixedZoneId::parse(zone_id)
}
