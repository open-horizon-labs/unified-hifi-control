//! Change sets: multi-surface staged intent with explicit generations.
//!
//! Staged intent is server-side state that several surfaces can see at once. That makes
//! it concurrency-control, not UI state, and the failure modes are the classic ones.
//!
//! The audit recorded on issue #323 documents the exact shape of the bug this module
//! exists to make unrepresentable: an apply that passed the live pending dictionaries
//! straight into an async operation, and a completion handler that cleared the whole
//! store. Edits staged by another surface *during* the in-flight apply were deleted by
//! a completion that had never seen them.
//!
//! The fix is a generation counter with one rule: **completion may retire only the exact
//! generation it detached.** Staging during execution creates a successor generation,
//! and retiring an older generation is refused rather than absorbing later intent.

use serde::{Deserialize, Serialize};

use super::control::{ApplyLane, ControlId, Reason, ReasonCode};
use super::document::{DocumentRevisions, Extensions, ProducerEpoch, RevisionRef};
use super::value::{ControlValue, Timestamp};
use super::vocab::semantic_enum;

/// A stable change-set identifier.
///
/// Every apply, discard, retry and inspect operation names one of these. That is what
/// prevents a web page, an MCP client or a device from flushing a draft it does not own:
/// there is no "the pending changes" to address.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ChangeSetId(String);

impl ChangeSetId {
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
    /// What kind of actor staged an edit.
    ///
    /// Recorded so a shared draft can name its contributors, and so an automation's
    /// edits are distinguishable from a person's when they conflict.
    pub enum ActorClass {
        /// A person.
        Human => "human",
        /// A scheduled or rule-driven automation.
        Automation => "automation",
        /// A hardware surface acting on physical input.
        Device => "device",
        /// An AI agent acting through MCP.
        Agent => "agent",
    }
}

semantic_enum! {
    /// Which surface an edit arrived through.
    pub enum Surface {
        /// The web UI.
        Web => "web",
        /// The MCP server.
        Mcp => "mcp",
        /// A hardware control surface.
        Device => "device",
        /// A direct HTTP client.
        Http => "http",
        /// The server itself, e.g. a migration or a recovery step.
        Internal => "internal",
    }
}

semantic_enum! {
    /// Whether a producer keeps one shared draft or one draft per actor.
    pub enum DraftMode {
        /// One draft all surfaces contribute to. Every contributor and conflict must be
        /// exposed, because a user who cannot see another surface's edit will apply it
        /// without knowing.
        Shared => "shared",
        /// One draft per actor. Merge and takeover behaviour must be defined.
        ActorScoped => "actor_scoped",
    }
}

semantic_enum! {
    /// What happens when two actors want the same control.
    pub enum ConflictPolicy {
        /// Surface every contributor and conflicting value; resolve nothing implicitly.
        ExposeContributors => "expose_contributors",
        /// Merge non-overlapping edits; expose overlaps as conflicts.
        MergeNonOverlapping => "merge_non_overlapping",
        /// The most recent actor takes ownership; the displaced actor is told.
        LastActorTakeover => "last_actor_takeover",
    }
}

semantic_enum! {
    /// Lifecycle state of a change set.
    pub enum ChangeSetState {
        /// Accepting edits.
        Draft => "draft",
        /// Detached as an immutable generation for preflight or execution. Further
        /// staging goes to a successor generation.
        Detached => "detached",
        /// Being validated against the current producer revision.
        Validating => "validating",
        /// Being executed.
        Applying => "applying",
        /// Every entry applied.
        Applied => "applied",
        /// Some entries applied and some did not. Retry is a new plan, not a replay.
        PartiallyApplied => "partially_applied",
        /// Refused, with reasons on the entries.
        Rejected => "rejected",
        /// Replaced by a newer generation or another actor.
        Superseded => "superseded",
        /// Retention elapsed before it was applied.
        Expired => "expired",
        /// Explicitly discarded by its owner.
        Discarded => "discarded",
    }
}

/// Who staged this, from where.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    /// What kind of actor.
    pub actor_class: ActorClass,
    /// Which surface.
    pub surface: Surface,
    /// Opaque actor identifier, when the producer tracks one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
}

/// How long a draft survives, and what ends it.
///
/// Retention is contract semantics rather than frontend behaviour: a draft that
/// evaporates on reconnect, or lingers forever holding a resource, is a producer
/// property every surface needs to know about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Retention {
    /// Whether the draft survives a consumer reconnect.
    pub survives_reconnect: bool,
    /// Whether the draft survives a producer restart.
    pub survives_producer_restart: bool,
    /// What a producer-epoch change does to held intent in this draft.
    pub on_epoch_change: EpochPolicy,
    /// When the draft expires, if it does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<Timestamp>,
    /// Whether applying requires authorization beyond staging.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_requires_authorization: Option<bool>,
}

semantic_enum! {
    /// What a producer-epoch change does to staged and held intent.
    ///
    /// The producer must publish this transition to every surface. Held intent that
    /// silently disappears is the failure this vocabulary exists to prevent.
    pub enum EpochPolicy {
        /// The draft is expired with a reason and a correlation.
        Expire => "expire",
        /// The draft is retained across the epoch change unchanged.
        Retain => "retain",
        /// The draft is retained but every entry is revalidated before it may apply.
        Revalidate => "revalidate",
    }
}

/// A producer's draft policy.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DraftPolicy {
    /// One shared draft or one per actor.
    pub mode: DraftMode,
    /// How conflicting edits are resolved.
    pub on_conflict: ConflictPolicy,
    /// Retention defaults for drafts of this producer.
    pub retention: Retention,
}

/// Whether an entry may be applied as staged.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EntryValidity {
    /// Valid against the current producer revision.
    Valid,
    /// The observed or persisted value moved after the base revision. The entry must be
    /// revalidated and shown to the user; it must not be silently rebased.
    StaleBase {
        /// The value observed now, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed_now: Option<ControlValue>,
    },
    /// Another staged entry conflicts with this one.
    Conflicts {
        /// The conflicting controls.
        with: Vec<ControlId>,
    },
    /// Available on the running engine, but invalid in the proposed combination.
    DraftInvalid {
        /// Why.
        reason: Reason,
    },
    /// This consumer could not decide; the producer must validate.
    RequiresProducerValidation {
        /// Why.
        reason: Reason,
    },
}

impl EntryValidity {
    /// Whether this entry may be executed without further validation.
    pub fn may_apply(&self) -> bool {
        matches!(self, Self::Valid)
    }
}

/// One staged edit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeSetEntry {
    /// The control being edited.
    pub control: ControlId,
    /// Where the write will be routed.
    pub lane: ApplyLane,
    /// The staged value.
    pub desired: ControlValue,
    /// What was observed when the edit was staged, so a later divergence is provable
    /// rather than inferred.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_observed: Option<ControlValue>,
    /// Whether the entry may be applied as staged.
    pub validity: EntryValidity,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// A named, generation-versioned set of staged edits.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChangeSet {
    /// Stable identifier. Every operation targets this.
    pub id: ChangeSetId,
    /// The producer and epoch this draft targets.
    pub producer_id: String,
    /// Who staged it.
    pub origin: Origin,
    /// The producer revision every entry was validated against.
    pub base: RevisionRef,
    /// Immutable generation counter.
    ///
    /// Incremented by every accepted edit. A completion may retire only the exact
    /// generation it detached; see [`ChangeSet::retire`].
    pub generation: u64,
    /// Lifecycle state.
    pub state: ChangeSetState,
    /// When it was created.
    pub created_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
    /// The staged edits.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entries: Vec<ChangeSetEntry>,
    /// Retention and authorization for this draft.
    pub retention: Retention,
    /// Every actor that has contributed, for shared drafts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub contributors: Vec<Origin>,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

/// An immutable snapshot of a change set at one generation.
///
/// Preflight and execution operate on this, never on the live draft. Detaching is what
/// makes "the operation applied what it displayed" true.
#[derive(Debug, Clone, PartialEq)]
pub struct DetachedChangeSet {
    /// The change set this was detached from.
    pub id: ChangeSetId,
    /// The generation detached.
    pub generation: u64,
    /// The revision the entries were validated against.
    pub base: RevisionRef,
    /// The entries as they were at detach time.
    pub entries: Vec<ChangeSetEntry>,
}

/// Result of attempting to retire a generation.
#[derive(Debug, Clone, PartialEq)]
pub enum RetireOutcome {
    /// The exact generation was retired.
    Retired,
    /// A newer generation exists. Nothing was retired and nothing was cleared: the
    /// later intent belongs to whoever staged it.
    GenerationSuperseded {
        /// The generation the caller tried to retire.
        attempted: u64,
        /// The generation the draft is at now.
        current: u64,
    },
}

impl ChangeSet {
    /// Stage an edit, producing a successor generation.
    ///
    /// Returns the new generation. If the draft is currently detached or executing, the
    /// edit still lands — it simply belongs to a generation the in-flight operation has
    /// never seen, and that operation may not retire it.
    pub fn stage(&mut self, entry: ChangeSetEntry, at: Timestamp) -> u64 {
        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|candidate| candidate.control == entry.control && candidate.lane == entry.lane)
        {
            *existing = entry;
        } else {
            self.entries.push(entry);
        }
        self.generation += 1;
        self.updated_at = at;
        if self.state == ChangeSetState::Detached {
            // Staging during execution reopens the draft as a successor generation.
            self.state = ChangeSetState::Draft;
        }
        self.generation
    }

    /// Detach an immutable snapshot for preflight and execution.
    pub fn detach(&mut self) -> DetachedChangeSet {
        self.state = ChangeSetState::Detached;
        DetachedChangeSet {
            id: self.id.clone(),
            generation: self.generation,
            base: self.base,
            entries: self.entries.clone(),
        }
    }

    /// Retire exactly the generation an operation detached.
    ///
    /// Refuses when a newer generation exists. This is the whole guard against the
    /// lost-update race: a completion that arrives after another surface has staged more
    /// intent must not clear the draft.
    pub fn retire(&mut self, generation: u64, at: Timestamp) -> RetireOutcome {
        if generation != self.generation {
            return RetireOutcome::GenerationSuperseded {
                attempted: generation,
                current: self.generation,
            };
        }
        self.entries.clear();
        self.state = ChangeSetState::Applied;
        self.updated_at = at;
        RetireOutcome::Retired
    }

    /// The entry for a control and lane, if staged.
    pub fn entry(&self, control: &ControlId, lane: &ApplyLane) -> Option<&ChangeSetEntry> {
        self.entries
            .iter()
            .find(|entry| &entry.control == control && &entry.lane == lane)
    }

    /// Whether every entry may be applied without further validation.
    pub fn may_apply(&self) -> bool {
        !self.entries.is_empty() && self.entries.iter().all(|entry| entry.validity.may_apply())
    }
}

/// How far the producer has moved since a change set's base revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Staleness {
    /// The producer is exactly where the change set was validated.
    Current,
    /// Observed state moved. Entries must be revalidated and stale fields reported.
    StateAdvanced,
    /// Control identity moved: controls may have been added, removed or redefined.
    ControlPlaneAdvanced,
    /// The producer restarted or re-established identity. Held intent is governed by
    /// [`Retention::on_epoch_change`].
    EpochChanged,
}

impl Staleness {
    /// Compare a change set's base against the producer's current position.
    ///
    /// Precedence is deliberate and total: an epoch change dominates a control-plane
    /// move, which dominates a state move. A consumer therefore never has to compare
    /// two revisions itself and cannot pick the wrong pair.
    pub fn evaluate(
        base: &RevisionRef,
        current_epoch: ProducerEpoch,
        current: &DocumentRevisions,
    ) -> Self {
        if base.epoch != current_epoch {
            return Self::EpochChanged;
        }
        if current.control_plane != base.control_plane {
            return Self::ControlPlaneAdvanced;
        }
        if current.state != base.state {
            return Self::StateAdvanced;
        }
        Self::Current
    }

    /// Whether the change set must be revalidated before any mutation.
    ///
    /// True for everything except [`Staleness::Current`]. Silent rebasing is never
    /// permitted: the user staged a value against what they were shown.
    pub fn requires_revalidation(&self) -> bool {
        !matches!(self, Self::Current)
    }

    /// The reason code to report on affected entries.
    pub fn reason_code(&self) -> Option<ReasonCode> {
        match self {
            Self::Current => None,
            Self::StateAdvanced | Self::ControlPlaneAdvanced => Some(ReasonCode::StaleBase),
            Self::EpochChanged => Some(ReasonCode::Superseded),
        }
    }
}
