//! Control descriptors: identity, presentation hints, choices, ranges, availability
//! and apply semantics.
//!
//! A control descriptor answers four questions for a consumer with no backend
//! knowledge: what is this, what is it currently, may I change it, and what does
//! changing it cost. Nothing here names an adapter field, an XML element or a list
//! index; nothing here contains human-facing prose.

use serde::{Deserialize, Serialize};

use super::document::{Extensions, Revision};
use super::value::{
    is_false, Authority, ControlValue, Divergence, LaneValue, SourceClass, ValueLane,
};
use super::vocab::semantic_enum;

/// A stable semantic control identifier.
///
/// Stability rules, which the compatibility policy depends on:
///
/// * An id names a *concept*, not a storage location. `hqplayer.pipeline.filter_1x`
///   survives a backend renumbering its filter list.
/// * An id is never a backend index, handle or array position. Those are ephemeral and
///   would break persisted bindings on restart (issue #327).
/// * An id is never reused for a different concept. Retiring a control means marking it
///   deprecated, not recycling its id.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ControlId(String);

impl ControlId {
    /// Wrap a semantic id.
    pub fn new(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    /// The id as a string slice.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for ControlId {
    fn from(raw: &str) -> Self {
        Self::new(raw)
    }
}

impl std::fmt::Display for ControlId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

semantic_enum! {
    /// The control primitive a consumer should render.
    pub enum ControlKind {
        /// A one-shot operation with no value, e.g. play or next.
        Action => "action",
        /// An on/off value.
        Boolean => "boolean",
        /// A selection from a [`ChoiceSet`].
        Enumeration => "enumeration",
        /// A number within a [`NumericRange`].
        NumericRange => "numeric_range",
        /// Free text or an opaque value.
        Text => "text",
        /// A container that orders and nests other controls but holds no value itself.
        Group => "group",
        /// An atomic value made of named members that must be read and written as a
        /// unit, e.g. a matrix cell set or a coupled enabled/level pair.
        Composite => "composite",
    }
}

semantic_enum! {
    /// Where a write is routed.
    ///
    /// Lanes are independent: a write to [`ApplyLane::Immediate`] must not read,
    /// clear or flush staged intent in [`ApplyLane::Staged`].
    pub enum ApplyLane {
        /// Sent to the running engine now.
        Immediate => "immediate",
        /// Accumulated in a change set for a later explicit apply.
        Staged => "staged",
        /// Written to persisted configuration.
        Persistent => "persistent",
        /// Routed through a matrix or profile switch, which may replace many values at
        /// once.
        MatrixProfile => "matrix_profile",
        /// Executed as one server-side composite plan over several coupled controls.
        Composite => "composite",
    }
}

semantic_enum! {
    /// When a write becomes true, and what it costs.
    pub enum ApplyEffect {
        /// Takes effect immediately and is observable on the next read.
        LiveImmediate => "live_immediate",
        /// Accepted, but not authoritative until readback confirms it.
        VerifiedPending => "verified_pending",
        /// Stored, but not audible until the producer restarts.
        RestartRequired => "restart_required",
        /// Changes only the persisted baseline; the running engine is untouched.
        PersistentOnly => "persistent_only",
        /// Retained for a chain or feature that is not currently loaded, and applied if
        /// and when it becomes active.
        HeldUntilChainLoaded => "held_until_chain_loaded",
    }
}

semantic_enum! {
    /// What the user will notice when the write lands.
    pub enum Disruption {
        /// Nothing audible.
        NoDisruption => "none",
        /// A brief audible artefact.
        AudibleGlitch => "audible_glitch",
        /// Playback stops or restarts.
        PlaybackInterruption => "playback_interruption",
        /// The engine or daemon restarts.
        EngineRestart => "engine_restart",
        /// The producer disconnects and must be rediscovered.
        Reconnect => "reconnect",
    }
}

semantic_enum! {
    /// How much care an operation deserves before it is invoked.
    pub enum RiskClass {
        /// Freely invokable.
        Safe => "safe",
        /// Worth confirming on a surface that can confirm.
        Caution => "caution",
        /// Will interrupt listening.
        Disruptive => "disruptive",
        /// Can destroy configuration or retained values.
        Destructive => "destructive",
    }
}

semantic_enum! {
    /// Whether a control may be used, as a state rather than a bare boolean.
    pub enum AvailabilityState {
        /// Present, observable and mutable.
        Available => "available",
        /// Present but not usable now. [`Availability::reasons`] says why.
        Unavailable => "unavailable",
        /// Observable but not mutable by this consumer.
        ReadOnly => "read_only",
        /// Still present and still renderable, but scheduled for removal. A consumer
        /// must keep rendering it so existing layouts do not lose controls.
        Deprecated => "deprecated",
    }
}

semantic_enum! {
    /// Machine-readable reason codes.
    ///
    /// Consumers branch on these; humans read text resolved from
    /// [`Reason::display_text_key`] through the catalog. The two are governed
    /// separately so a wording change is never a contract change, and so catalog
    /// licensing (issue #343) never leaks into protocol semantics.
    pub enum ReasonCode {
        /// The producer does not implement this at all.
        NotSupported => "not_supported",
        /// Valid in general, meaningless in the current mode or chain.
        NotApplicableInMode => "not_applicable_in_mode",
        /// Needs a transport lane that is not connected.
        RequiresConnection => "requires_connection",
        /// Needs a capability the producer or surface lacks.
        RequiresCapability => "requires_capability",
        /// Needs credentials or permission.
        RequiresAuthorization => "requires_authorization",
        /// The producer is holding this control against modification.
        LockedByProducer => "locked_by_producer",
        /// Retained for compatibility and scheduled for removal.
        Deprecated => "deprecated",
        /// The producer cannot currently read this value.
        Unobservable => "unobservable",
        /// Valid on the running engine, but the staged combination would be invalid.
        DraftInvalid => "draft_invalid",
        /// A constraint could not be evaluated by this consumer.
        UnknownConstraint => "unknown_constraint",
        /// The lane that owns this value is not configured.
        LaneUnconfigured => "lane_unconfigured",
        /// The value belongs to a chain or profile that is not loaded.
        ChainNotLoaded => "chain_not_loaded",
        /// The change set's base revision no longer matches the producer.
        StaleBase => "stale_base",
        /// Another staged value conflicts with this one.
        ConflictingSibling => "conflicting_sibling",
        /// Stored, but inert until the producer restarts.
        RestartRequired => "restart_required",
        /// A newer operation or generation replaced this one.
        Superseded => "superseded",
        /// Retention expired before this could be applied.
        Expired => "expired",
    }
}

semantic_enum! {
    /// What a reason is about, so a consumer can attribute it correctly.
    pub enum ReasonScope {
        /// The running engine's observed state.
        Observed => "observed",
        /// The staged change set.
        Draft => "draft",
        /// A transport lane.
        Lane => "lane",
        /// The producer as a whole.
        Producer => "producer",
    }
}

/// A machine-readable reason, optionally paired with a catalog key for display text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reason {
    /// The code consumers branch on.
    pub code: ReasonCode,
    /// What the reason is about.
    pub scope: ReasonScope,
    /// Catalog key resolving to human-facing text. Never prose, so that catalog
    /// provenance and licensing stay governed by issue #343.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_text_key: Option<String>,
    /// Non-localised diagnostic detail, for logs rather than for users.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl Reason {
    /// A reason about observed engine state.
    pub fn observed(code: ReasonCode) -> Self {
        Self {
            code,
            scope: ReasonScope::Observed,
            display_text_key: None,
            detail: None,
        }
    }

    /// A reason about a staged change set.
    pub fn draft(code: ReasonCode) -> Self {
        Self {
            code,
            scope: ReasonScope::Draft,
            display_text_key: None,
            detail: None,
        }
    }
}

/// Availability as reason-carrying data.
///
/// Never a bare boolean: a consumer that can only see "disabled" cannot explain
/// itself, and a user who cannot see why a control is unavailable cannot escape the
/// state that made it unavailable.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Availability {
    /// Whether the control may be used.
    pub state: AvailabilityState,
    /// Why, when the state is anything other than available. At least one reason is
    /// required for every non-available state; see [`Availability::is_well_formed`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<Reason>,
}

impl Availability {
    /// An available control.
    pub fn available() -> Self {
        Self {
            state: AvailabilityState::Available,
            reasons: Vec::new(),
        }
    }

    /// An unavailable control with its reason.
    pub fn unavailable(reason: Reason) -> Self {
        Self {
            state: AvailabilityState::Unavailable,
            reasons: vec![reason],
        }
    }

    /// Whether a non-available state carries at least one reason.
    pub fn is_well_formed(&self) -> bool {
        match self.state {
            AvailabilityState::Available => true,
            _ => !self.reasons.is_empty(),
        }
    }

    /// Whether a consumer may offer mutation.
    pub fn permits_mutation(&self) -> bool {
        matches!(
            self.state,
            AvailabilityState::Available | AvailabilityState::Deprecated
        )
    }

    /// Whether a consumer must keep rendering the control's observed value.
    ///
    /// Always true. Unavailability restricts *mutation*, never *visibility*: hiding a
    /// control removes the user's ability to see, and often to escape, the state that
    /// made it unavailable.
    pub fn permits_display(&self) -> bool {
        true
    }
}

/// One selectable option.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    /// Stable semantic id. Never a backend index.
    pub id: String,
    /// Catalog key for the display label. Never prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_key: Option<String>,
    /// Engine-reported name, used verbatim when no catalog entry matches. This is what
    /// lets an option the catalog has never heard of stay renderable and invokable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_name: Option<String>,
    /// Per-choice availability, when it differs from the control's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub availability: Option<Availability>,
}

impl Choice {
    /// A choice with only a stable id and the engine's own name.
    pub fn new(id: impl Into<String>, engine_name: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label_key: None,
            engine_name: Some(engine_name.into()),
            availability: None,
        }
    }
}

/// An enumeration with an explicit authority.
///
/// Runtime enumeration wins. A catalog may add label keys to options the engine
/// reported; it may never add, remove, reorder or re-permit options, because the engine
/// is the only thing that knows what it will accept.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChoiceSet {
    /// Whose enumeration this is.
    pub authority: Authority,
    /// How it was obtained.
    pub source: SourceClass,
    /// The producer revision it was enumerated at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
    /// The options, in the order the authority reported them.
    pub choices: Vec<Choice>,
}

impl ChoiceSet {
    /// A runtime-enumerated choice set.
    pub fn runtime(choices: Vec<Choice>) -> Self {
        Self {
            authority: Authority::Runtime,
            source: SourceClass::LiveProtocol,
            revision: None,
            choices,
        }
    }

    /// Add catalog label keys to options this set already contains.
    ///
    /// Enrichment is presentation-only and lossless in one direction: the returned set
    /// has the same options, in the same order, with the same availability. Options
    /// present only in the catalog are dropped, because a catalog cannot know what the
    /// engine will accept.
    pub fn enrich_from_catalog(&self, catalog: &ChoiceSet) -> ChoiceSet {
        let mut enriched = self.clone();
        for choice in &mut enriched.choices {
            if choice.label_key.is_some() {
                continue;
            }
            if let Some(entry) = catalog.choices.iter().find(|entry| entry.id == choice.id) {
                choice.label_key = entry.label_key.clone();
            }
        }
        enriched
    }

    /// Whether a choice id is offered by this authority.
    pub fn contains(&self, id: &str) -> bool {
        self.choices.iter().any(|choice| choice.id == id)
    }
}

/// A numeric domain with an explicit authority.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NumericRange {
    /// Inclusive minimum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<f64>,
    /// Inclusive maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<f64>,
    /// Smallest meaningful increment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<f64>,
    /// Unit symbol, e.g. `dB`. Presentation resolves it; the contract does not
    /// interpret it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// Whose range this is.
    pub authority: Authority,
    /// The producer revision it was read at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<Revision>,
}

/// How a write to this control is confirmed.
///
/// Verification is *data*, so a producer implementation can drive readback from the
/// descriptor rather than from setter-specific UI logic — which is how a verifier ends
/// up checking a different field than the one the write touched.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    /// The control whose observed lane is authoritative for confirming this write.
    /// Usually the control itself; occasionally a sibling that reflects the effect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_via: Option<ControlId>,
    /// Which lane of `verify_via` to read.
    pub verify_lane: ValueLane,
    /// Whether the transport's acknowledgement is provisional until readback.
    ///
    /// When true, a consumer must not treat a returned call as evidence the value
    /// changed. A dropped connection after such a write is
    /// [`super::CommandOutcome::Indeterminate`], not a failure.
    #[serde(default, skip_serializing_if = "is_false")]
    pub provisional_ack: bool,
    /// Sibling state that must be asserted *unchanged* after the write.
    ///
    /// A verifier that only checks the field it set will report success after
    /// destroying a retained inactive value it never looked at.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preserves: Vec<ControlId>,
}

impl Verification {
    /// Verify by reading this control's own observed lane.
    pub fn own_observed() -> Self {
        Self {
            verify_via: None,
            verify_lane: ValueLane::Observed,
            provisional_ack: false,
            preserves: Vec::new(),
        }
    }
}

/// How a control is written and what that costs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApplySemantics {
    /// Where the write is routed.
    pub lane: ApplyLane,
    /// When it becomes true.
    pub effect: ApplyEffect,
    /// What the user will notice.
    pub disruption: Disruption,
    /// How much care it deserves.
    pub risk: RiskClass,
    /// How it is confirmed.
    pub verification: Verification,
    /// Controls whose choices, ranges or observability become invalid or unobservable
    /// once this is written. A consumer must re-read them rather than trusting a cached
    /// enumeration.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub invalidates: Vec<ControlId>,
    /// Controls that must be written together as one server-side composite plan.
    ///
    /// Coupling is the producer's job. A consumer issuing the extra writes itself is
    /// how a level edit silently enables a feature the user did not ask for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub coupled_with: Vec<ControlId>,
}

/// A single control.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Control {
    /// Stable semantic identity.
    pub id: ControlId,
    /// Which primitive to render.
    pub kind: ControlKind,
    /// Catalog key for the label. Never prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_key: Option<String>,
    /// Catalog key for longer help text. Never prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_key: Option<String>,
    /// Group this control belongs to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
    /// Ordering hint within the group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    /// Suggested widget. A hint only: a surface may honour, substitute or ignore it,
    /// but it can never use a hint to override availability or a constraint.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub widget_hint: Option<String>,
    /// Unit symbol for scalar controls.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    /// The lane values. At most one entry per lane.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<LaneValue>,
    /// Options, for enumerations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub choices: Option<ChoiceSet>,
    /// Numeric domain, for ranges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<NumericRange>,
    /// Whether the control may be used, with reasons.
    pub availability: Availability,
    /// How it is written. `None` means observation-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply: Option<ApplySemantics>,
    /// Recorded lane disagreements.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub divergences: Vec<Divergence>,
    /// Member ids, for groups and composites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ControlId>,
    /// Additive fields from a newer minor version, preserved for pass-through.
    #[serde(flatten, default, skip_serializing_if = "Extensions::is_empty")]
    pub extensions: Extensions,
}

impl Control {
    /// The value for one lane, if present.
    pub fn lane(&self, lane: &ValueLane) -> Option<&LaneValue> {
        self.values.iter().find(|value| &value.lane == lane)
    }

    /// The observed lane's grounded value, if any.
    pub fn observed(&self) -> Option<&ControlValue> {
        self.lane(&ValueLane::Observed)
            .and_then(LaneValue::grounded_value)
    }

    /// Whether a consumer may offer mutation of this control.
    ///
    /// Requires both an availability state that permits it and apply semantics that
    /// describe how. A control with no [`ApplySemantics`] is observation-only no matter
    /// what its availability says.
    pub fn is_mutable(&self) -> bool {
        self.availability.permits_mutation() && self.apply.is_some()
    }

    /// Whether at most one value is present per lane.
    pub fn has_unique_lanes(&self) -> bool {
        let mut seen: Vec<&ValueLane> = Vec::with_capacity(self.values.len());
        for value in &self.values {
            if seen.contains(&&value.lane) {
                return false;
            }
            seen.push(&value.lane);
        }
        true
    }
}

/// An ordered container of controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControlGroup {
    /// Stable semantic group id.
    pub id: String,
    /// Catalog key for the group label. Never prose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label_key: Option<String>,
    /// Ordering hint among sibling groups.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<i32>,
    /// Member control ids, in presentation order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<ControlId>,
}
