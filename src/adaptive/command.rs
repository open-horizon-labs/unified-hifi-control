//! Command outcomes, correlation and the audit trail.
//!
//! The rule this module enforces is: **a consumer never infers observed state from
//! whether a call returned or threw.** A write whose transport dropped after the bytes
//! left is not a failure and not a success — it is *possibly applied*, and the only
//! thing that resolves it is authoritative convergence with the producer.
//!
//! Success/failure is therefore not a sufficient vocabulary. Applied, ignored,
//! rejected, superseded, disconnected, timed out, held, restart-required and
//! indeterminate are distinguishable states, and multi-step plans add partially
//! applied, compensating, compensated and recovery-required.

use serde::{Deserialize, Serialize};

use super::change_set::ChangeSetId;
use super::control::{ApplyLane, ControlId, Reason};
use super::document::{Extensions, RevisionRef};
use super::value::Timestamp;
use super::vocab::semantic_enum;

/// A stable operation identifier.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OperationId(String);

impl OperationId {
    /// Wrap an id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

semantic_enum! {
    /// How far the write attempt itself got.
    ///
    /// Kept separate from [`CommandOutcome`] because "we sent it" and "it took effect"
    /// are different facts, and conflating them is what makes a dropped connection look
    /// like a rejection.
    pub enum WriteAttempt {
        /// Nothing was sent. Preflight refused it.
        NotAttempted => "not_attempted",
        /// Bytes left, and nothing came back.
        Attempted => "attempted",
        /// The transport acknowledged, but the acknowledgement is provisional: the
        /// producer has not been read back yet.
        AcknowledgedProvisional => "acknowledged_provisional",
        /// Readback confirmed the effect against the authoritative observed field.
        Confirmed => "confirmed",
    }
}

semantic_enum! {
    /// What became of an operation.
    pub enum CommandOutcome {
        /// The requested effect is observed on the producer.
        Applied => "applied",
        /// The producer accepted the write and nothing changed, because the value was
        /// already what was requested or the setting is inert in the current state.
        Ignored => "ignored",
        /// The producer refused it, with a reason.
        Rejected => "rejected",
        /// A newer operation or generation replaced this one before it took effect.
        Superseded => "superseded",
        /// The transport was down before the write was attempted.
        Disconnected => "disconnected",
        /// No response arrived within the operation's budget.
        TimedOut => "timed_out",
        /// Stored as intent for a chain, profile or feature that is not loaded.
        Held => "held",
        /// Stored, and inert until the producer restarts.
        RestartRequired => "restart_required",
        /// The write was attempted and the transport dropped. **Possibly applied.**
        /// Not a failure. Resolved only by authoritative convergence.
        Indeterminate => "indeterminate",
        /// Some steps of a multi-step plan applied and some did not.
        PartiallyApplied => "partially_applied",
        /// Compensation for a partial application is in progress.
        Compensating => "compensating",
        /// Compensation completed; the producer is back at a coherent state.
        Compensated => "compensated",
        /// Compensation is not possible automatically; a human or a new plan is needed.
        RecoveryRequired => "recovery_required",
        /// Held intent proved invalid or its retention elapsed.
        ///
        /// A visible terminal state with a reason and a correlation — held intent is
        /// never silently forgotten.
        Expired => "expired",
        /// Readback found a state that matches neither the request nor the prior value.
        Divergent => "divergent",
    }
}

impl CommandOutcome {
    /// Whether no further transition is expected.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Applied
                | Self::Ignored
                | Self::Rejected
                | Self::Superseded
                | Self::Compensated
                | Self::Expired
        )
    }

    /// Whether this outcome is evidence about the producer's observed state.
    ///
    /// False for [`CommandOutcome::Indeterminate`], [`CommandOutcome::TimedOut`] and
    /// [`CommandOutcome::Compensating`]: those describe what happened to the *operation*,
    /// not what is true of the producer. A consumer that treats them as state evidence
    /// will display a value the engine never adopted.
    pub fn is_state_evidence(&self) -> bool {
        !matches!(
            self,
            Self::Indeterminate | Self::TimedOut | Self::Compensating
        )
    }

    /// Whether the operation may still resolve to something else.
    pub fn awaits_convergence(&self) -> bool {
        matches!(
            self,
            Self::Indeterminate
                | Self::TimedOut
                | Self::Compensating
                | Self::PartiallyApplied
                | Self::Held
                | Self::RestartRequired
        )
    }

    /// Whether this outcome may transition to `next`.
    ///
    /// Reconnect and readback legitimately move an operation from indeterminate to
    /// applied, superseded, rejected or divergent. The reverse is forbidden: once an
    /// outcome is authoritative it may not decay back into a maybe, because that would
    /// let a later failed poll erase a known fact.
    pub fn may_transition_to(&self, next: &CommandOutcome) -> bool {
        if self == next {
            return true;
        }
        // An unrecognized outcome from a newer minor version is always allowed to be
        // recorded; refusing it would drop audit history a v1.0 consumer cannot read.
        if !self.is_recognized() || !next.is_recognized() {
            return true;
        }
        if self.is_terminal() {
            return false;
        }
        // Nothing may regress into an unresolved state.
        !matches!(next, Self::Indeterminate | Self::TimedOut)
    }
}

/// A rejected attempt to record a transition.
#[derive(Debug, Clone, PartialEq)]
pub struct TransitionRejected {
    /// The outcome the record is currently at.
    pub from: CommandOutcome,
    /// The outcome that was refused.
    pub to: CommandOutcome,
}

/// One recorded outcome change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OutcomeTransition {
    /// What it was.
    pub from: CommandOutcome,
    /// What it became.
    pub to: CommandOutcome,
    /// When.
    pub at: Timestamp,
    /// The producer revision observed at the time, when one was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<RevisionRef>,
    /// Why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

/// One step of a command plan.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlanStep {
    /// Position in the plan.
    pub index: u32,
    /// The control written.
    pub control: ControlId,
    /// Where the write was routed.
    pub lane: ApplyLane,
    /// The producer revision this step observed.
    ///
    /// Per-step rather than per-plan: a step that must change mode before the next
    /// step's capabilities become observable crosses a revision boundary, and a plan
    /// that claims one atomic preflight across that boundary is lying.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<RevisionRef>,
    /// What became of this step.
    pub outcome: CommandOutcome,
    /// Why, when the outcome needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
}

semantic_enum! {
    /// What is needed to get back to a coherent state.
    pub enum RecoveryState {
        /// Nothing; the producer is coherent.
        NotRequired => "not_required",
        /// A readback will resolve it.
        AwaitingReadback => "awaiting_readback",
        /// A reconnect is needed before anything can be resolved.
        AwaitingReconnect => "awaiting_reconnect",
        /// Automatic compensation is running.
        Compensating => "compensating",
        /// A new plan is required; this one cannot be replayed.
        ReplanRequired => "replan_required",
        /// A human must decide.
        ManualInterventionRequired => "manual_intervention_required",
    }
}

/// The record of one operation, from request to authoritative resolution.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OperationRecord {
    /// Stable operation identity.
    pub id: OperationId,
    /// Correlation id shared with the request that started it, so every surface can
    /// follow the same operation without inventing its own tracking.
    pub correlation_id: String,
    /// The change set this operation executed, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_set: Option<ChangeSetId>,
    /// The change-set generation detached for execution. Completion may retire only
    /// this generation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
    /// How far the write attempt got.
    pub write_attempt: WriteAttempt,
    /// The current outcome.
    pub outcome: CommandOutcome,
    /// The last producer revision this operation observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed: Option<RevisionRef>,
    /// The steps, for multi-step plans.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<PlanStep>,
    /// Revision boundaries this plan crosses.
    ///
    /// A non-empty list is the plan declaring that it cannot claim single-revision
    /// atomic preflight.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub revision_boundaries: Vec<RevisionRef>,
    /// Append-only outcome history.
    ///
    /// A new operation may not erase this. It is the only record that distinguishes
    /// "was never applied" from "was applied, then superseded".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<OutcomeTransition>,
    /// What is needed to reach a coherent state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RecoveryState>,
    /// Why, when the outcome needs one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<Reason>,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl OperationRecord {
    /// Record a transition, appending to the audit trail.
    ///
    /// Refuses transitions that would regress an authoritative outcome into an
    /// unresolved one, or move on from a terminal state.
    pub fn transition(
        &mut self,
        to: CommandOutcome,
        at: Timestamp,
        observed: Option<RevisionRef>,
        reason: Option<Reason>,
    ) -> Result<(), TransitionRejected> {
        if !self.outcome.may_transition_to(&to) {
            return Err(TransitionRejected {
                from: self.outcome.clone(),
                to,
            });
        }
        self.history.push(OutcomeTransition {
            from: self.outcome.clone(),
            to: to.clone(),
            at,
            observed,
            reason: reason.clone(),
        });
        self.outcome = to;
        if observed.is_some() {
            self.observed = observed;
        }
        self.reason = reason;
        Ok(())
    }

    /// Whether this operation's outcome may be used as evidence of producer state.
    pub fn is_state_evidence(&self) -> bool {
        self.outcome.is_state_evidence()
    }

    /// Whether the plan crossed more than one producer revision.
    pub fn is_multi_revision(&self) -> bool {
        !self.revision_boundaries.is_empty()
    }
}
