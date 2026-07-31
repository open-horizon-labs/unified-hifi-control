//! Surface-appropriate projections of one aggregator-owned producer document (#331).
//!
//! ## Why a projection layer exists at all
//!
//! #323 defines what a producer may say; #324 makes the aggregator the only thing that holds an
//! admitted document. Neither answers the question three surfaces have to answer identically:
//! *given this document, what may I show, what may I offer, and what must I say out loud when I
//! cannot do either faithfully?*
//!
//! Left to each surface, that question is answered three times. Lane resolution alone has five
//! lanes; availability is reason-carrying; enumerations can be unknown, stale, invalidated or
//! merely long. Each of those has a wrong answer that looks right, and three independent wrong
//! answers is exactly the drift #331 exists to remove.
//!
//! ## Surfaces are data, not code
//!
//! A [`SurfaceProfile`] *declares* what a consumer can faithfully render — which control
//! primitives, whether it can show a per-choice availability reason, how much it can hold, how
//! risky an operation it may offer. Nothing here branches on a device class or a board name; the
//! program session's hard constraint is "negotiate capabilities; do not infer from board names",
//! and a `SurfaceKind` enum is a board name.
//!
//! ## The rule that makes a subset safe
//!
//! Surface-specific subsets are allowed; contradictory semantics are not. So every reduction this
//! module performs is **declared**:
//!
//! * a control this surface cannot render faithfully is still *displayed*, with its mutation
//!   [`Mutability::WithheldFromSurface`] and a reason — never silently flattened into a primitive
//!   that loses per-choice availability;
//! * a budget that drops controls reports them in [`RenderedProjection::omitted`];
//! * a truncated enumeration says so, and never truncates away the current selection;
//! * an enumeration invalidated by an in-flight write withholds the stale list and says which
//!   operation invalidated it, rather than serving choices that are about to be wrong.
//!
//! Nothing here resolves catalog text (#343 owns that), invents a value, or reorders a producer's
//! own enumeration.

use std::collections::{BTreeMap, BTreeSet};

use crate::adaptive::{
    ApplyEffect, ApplyLane, ApplySemantics, Authority, Availability, AvailabilityState, Choice,
    CommandOutcome, Constraint, ControlGroup, ControlId, ControlKind, ControlValue, Disruption,
    Divergence, LaneHealth, LaneValue, NumericRange, OperationId, ProducerDocument, ProducerEpoch,
    Reason, ReasonCode, ReasonScope, RecoveryState, Refusal, RevisionRef, RiskClass, SchemaVersion,
    SourceClass, TargetRole, ValueLane, WriteAttempt, CONSUMER_SCHEMA_VERSION,
};

use super::aggregator::{ProducerPresence, ProducerSnapshot};

/// The version of the *projection* contract, deliberately independent of the producer contract's.
///
/// A surface negotiates what it can render; the producer negotiates what it can say. Tying the two
/// together would mean a new producer minor forced every client to re-qualify.
pub const SURFACE_PROJECTION_SCHEMA: SchemaVersion = SchemaVersion::new(1, 0);

/// Catalog keys this module attaches to reasons it originates.
///
/// Keys, never prose: #343 owns the catalog and its provenance. A surface that has no catalog entry
/// still has the [`ReasonCode`] to branch on.
pub const WITHHELD_UNRENDERABLE_KIND: &str = "surface.withheld.unrenderable_kind";
/// A control whose per-choice availability this surface cannot express.
pub const WITHHELD_PER_CHOICE_REASON: &str = "surface.withheld.per_choice_reason";
/// A control whose risk class exceeds what this surface may offer.
pub const WITHHELD_RISK_EXCEEDS_SURFACE: &str = "surface.withheld.risk_exceeds_surface";
/// A control dropped because the surface's control budget was exhausted.
pub const OMITTED_BUDGET_EXHAUSTED: &str = "surface.omitted.budget_exhausted";
/// A control dropped because the container it belongs to was dropped.
pub const OMITTED_CONTAINER_DROPPED: &str = "surface.omitted.container_dropped";

// =============================================================================
// Profile
// =============================================================================

/// What a surface can faithfully render, declared rather than inferred.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceCapabilities {
    /// Control primitives this surface can render without losing a published semantic.
    pub renders_kinds: BTreeSet<ControlKind>,
    /// Whether the surface can show *per-choice* availability and its reason.
    ///
    /// The HQPTuner renderer audit on #331 is the reason this is a capability rather than an
    /// assumption: a dropdown can disable one option and explain it; a segmented control can only
    /// disable the whole control. Swapping one for the other silently deletes a published
    /// constraint.
    pub per_choice_reasons: bool,
    /// Whether the surface can display reason text at all, as opposed to a bare state.
    pub reason_text: bool,
    /// The highest risk class this surface may advertise as invokable.
    ///
    /// [`RiskClass::Unrecognized`] sorts above every recognized member, so a risk class from a
    /// newer minor is never advertised as invokable by an older surface. That is the safe
    /// direction: the control stays visible, the mutation does not.
    pub max_risk: RiskClass,
}

impl SurfaceCapabilities {
    /// A surface that can render every primitive this build knows, with full reason text.
    pub fn unrestricted() -> Self {
        Self {
            renders_kinds: ControlKind::known().into_iter().collect(),
            per_choice_reasons: true,
            reason_text: true,
            max_risk: RiskClass::Destructive,
        }
    }
}

/// How much of a document a surface can hold, and what it wants first.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SurfaceBudget {
    /// Maximum number of controls the surface can present.
    pub max_controls: Option<usize>,
    /// Maximum number of choices the surface can present for one control.
    pub max_choices: Option<usize>,
    /// Group ids in the order this surface wants them, most important first.
    ///
    /// Deliberately surface policy rather than producer truth. "Transport and volume before
    /// tuning" is a statement about a knob, not about HQPlayer, and putting it in the producer
    /// would make every adapter know what a knob is.
    pub priority_groups: Vec<String>,
}

/// One consumer's declared capabilities and limits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SurfaceProfile {
    /// Stable identifier for this surface, echoed into every projection it receives.
    pub surface_id: String,
    /// What it can render.
    pub capabilities: SurfaceCapabilities,
    /// How much, and in what order.
    pub budget: SurfaceBudget,
}

impl SurfaceProfile {
    /// A browser: renders every primitive, no budget.
    pub fn web() -> Self {
        Self {
            surface_id: "web".to_string(),
            capabilities: SurfaceCapabilities::unrestricted(),
            budget: SurfaceBudget::default(),
        }
    }

    /// An AI tool client: no pixels, so every primitive is expressible as data, but it cannot ask a
    /// human to confirm, so it may not be offered a disruptive or destructive operation unattended.
    pub fn mcp() -> Self {
        Self {
            surface_id: "mcp".to_string(),
            capabilities: SurfaceCapabilities {
                max_risk: RiskClass::Caution,
                ..SurfaceCapabilities::unrestricted()
            },
            budget: SurfaceBudget::default(),
        }
    }

    /// A small control device: bounded, transport and volume first, and no primitive that needs a
    /// per-option explanation.
    pub fn compact_device(surface_id: impl Into<String>, max_controls: usize) -> Self {
        Self {
            surface_id: surface_id.into(),
            capabilities: SurfaceCapabilities {
                renders_kinds: [
                    ControlKind::Action,
                    ControlKind::Boolean,
                    ControlKind::Enumeration,
                    ControlKind::NumericRange,
                    ControlKind::Text,
                ]
                .into_iter()
                .collect(),
                per_choice_reasons: false,
                reason_text: false,
                max_risk: RiskClass::Safe,
            },
            budget: SurfaceBudget {
                max_controls: Some(max_controls),
                max_choices: Some(8),
                priority_groups: vec![
                    "transport".to_string(),
                    "volume".to_string(),
                    "metadata".to_string(),
                ],
            },
        }
    }
}

// =============================================================================
// Projection
// =============================================================================

/// What a surface receives.
#[derive(Debug, Clone, PartialEq)]
pub enum SurfaceProjection {
    /// The document was admitted and projected.
    Rendered(Box<RenderedProjection>),
    /// The document could not be admitted at this schema generation.
    Fallback(FallbackProjection),
}

impl SurfaceProjection {
    /// The rendered projection, if this is one.
    pub fn rendered(&self) -> Option<&RenderedProjection> {
        match self {
            Self::Rendered(rendered) => Some(rendered),
            Self::Fallback(_) => None,
        }
    }

    /// The surface this projection was produced for.
    pub fn surface_id(&self) -> &str {
        match self {
            Self::Rendered(rendered) => &rendered.surface_id,
            Self::Fallback(fallback) => &fallback.surface_id,
        }
    }
}

/// A safe, informative degradation for a document this build cannot admit.
///
/// It carries whatever identity was legible *without* trusting the body, so a surface can say "this
/// producer speaks a generation UHC does not" instead of showing an empty panel with no explanation.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackProjection {
    /// Projection contract version.
    pub schema: SchemaVersion,
    /// The surface this was produced for.
    pub surface_id: String,
    /// The producer id, when the raw value carried a legible one.
    pub producer_id: Option<String>,
    /// The producer family, when the raw value carried a legible one.
    pub producer_type: Option<String>,
    /// The `schema_version` string exactly as the producer stamped it.
    pub declared_schema: Option<String>,
    /// Why the document was not admitted.
    pub refusal: Refusal,
}

/// Producer identity, as a surface needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedProducer {
    /// Stable semantic producer id.
    pub producer_id: String,
    /// Producer family.
    pub producer_type: String,
    /// Operator-visible label, if any.
    pub instance_label: Option<String>,
    /// Backend product version, informational.
    pub product_version: Option<String>,
    /// The producer's restart counter.
    pub epoch: ProducerEpoch,
    /// The role these controls act in.
    pub role: TargetRole,
    /// The prefixed zone id this producer is bound to, if any.
    pub zone_id: Option<String>,
}

/// Something a surface must tell the user about the projection as a whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionNotice {
    /// The document declares a newer minor. Unknown members were preserved, not rendered.
    UnknownSchemaAdditions {
        /// The document's declared version.
        document: SchemaVersion,
        /// The version this build implements.
        consumer: SchemaVersion,
    },
    /// The producer's own product version is outside the series this build has verified.
    ///
    /// Deliberately *not* a reason to withhold controls: the runtime enumeration is still the
    /// authority on what the engine accepts, and blanking a working control because a version
    /// string is unfamiliar removes the user's way out of the state.
    UntestedProducerVersion {
        /// The version the producer reported.
        product_version: String,
    },
    /// The whole document is last-known rather than current.
    LastKnown,
    /// The producer declares itself stale.
    ProducerStale,
    /// A transport lane is not healthy. Values sourced from it may be last-good.
    LaneNotHealthy {
        /// Which lane.
        lane: crate::adaptive::TransportLane,
        /// Its state.
        state: crate::adaptive::LaneState,
    },
    /// The aggregator demoted published intent before serving this snapshot.
    IntentRepaired {
        /// How many change-set entries were demoted.
        count: usize,
    },
    /// The surface budget dropped controls. See [`RenderedProjection::omitted`].
    Truncated {
        /// How many the surface can hold.
        budget: usize,
        /// How many the document advertised.
        advertised: usize,
    },
    /// A control uses a primitive this build does not recognize.
    UnrecognizedControlKind {
        /// The control.
        control: ControlId,
        /// The unrecognized wire spelling.
        kind: String,
    },
}

/// A control dropped from this surface, with the reason it was dropped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OmittedControl {
    /// Which control.
    pub id: ControlId,
    /// Machine-readable reason code.
    pub code: ReasonCode,
    /// Catalog key for display text.
    pub display_text_key: &'static str,
}

/// A presentation container, projected verbatim.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectedGroup {
    /// Stable group id.
    pub id: String,
    /// Catalog key for the label.
    pub label_key: Option<String>,
    /// The surface's rank for this group, lower first.
    pub rank: usize,
    /// The group's own ordering hint.
    pub order: Option<i32>,
}

/// Transport-lane health, projected verbatim plus the aggregator's own witness.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedLaneHealth {
    /// The producer's published health for this lane.
    pub health: LaneHealth,
    /// When the aggregator last saw this lane succeed, across admissions.
    pub last_success_seen: Option<crate::adaptive::Timestamp>,
}

/// Whether, and how, a surface may offer mutation.
#[derive(Debug, Clone, PartialEq)]
pub enum Mutability {
    /// Writable, with the cost of writing spelled out.
    Mutable {
        /// The producer's apply semantics, verbatim.
        apply: Box<ApplySemantics>,
        /// The surface-neutral description derived from them.
        description: ApplyDescription,
    },
    /// The producer publishes no apply semantics: this control is observation only.
    ObservationOnly,
    /// The producer says the control is not writable now, and why.
    Blocked {
        /// The producer's reasons, verbatim. Never empty for a non-available state.
        reasons: Vec<Reason>,
    },
    /// The producer would allow it; this surface cannot offer it faithfully.
    ///
    /// A different verdict from [`Mutability::Blocked`], and the distinction matters: a surface
    /// that reports its own limitation as the producer's is how a user concludes the engine is
    /// broken.
    WithheldFromSurface {
        /// Why this surface withheld it.
        reason: Reason,
    },
}

/// The surface-neutral answer to "when does this become true, and what does it cost".
///
/// An MCP tool description, a web apply button and a device confirmation prompt are all derived
/// from this one value, which is why the timing key is a closed set rather than a sentence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyDescription {
    /// Where the write is routed.
    pub lane: ApplyLane,
    /// When it becomes true.
    pub effect: ApplyEffect,
    /// What the user notices.
    pub disruption: Disruption,
    /// How much care it deserves.
    pub risk: RiskClass,
    /// Closed catalog key naming the timing class. Distinct per [`ApplyEffect`].
    pub timing_key: &'static str,
    /// Whether a returned call is evidence the value changed.
    ///
    /// False when the producer marks the acknowledgement provisional: the write is confirmed by
    /// readback, and a dropped connection after it is indeterminate rather than failed.
    pub acknowledgement_is_evidence: bool,
    /// Controls whose enumerations this write invalidates.
    pub invalidates: Vec<ControlId>,
    /// Controls the producer writes together with this one, as one plan.
    pub coupled_with: Vec<ControlId>,
}

/// Catalog key for a control that takes effect on the running engine at once.
pub const TIMING_LIVE_IMMEDIATE: &str = "apply.timing.live_immediate";
/// Catalog key for a control whose write is confirmed by readback.
pub const TIMING_VERIFIED_PENDING: &str = "apply.timing.verified_pending";
/// Catalog key for a control that is stored now and audible after a restart.
pub const TIMING_RESTART_REQUIRED: &str = "apply.timing.restart_required";
/// Catalog key for a control that changes only the persisted baseline.
pub const TIMING_PERSISTENT_ONLY: &str = "apply.timing.persistent_only";
/// Catalog key for a control retained for a chain that is not loaded.
pub const TIMING_HELD_UNTIL_CHAIN_LOADED: &str = "apply.timing.held_until_chain_loaded";
/// Catalog key for an apply effect this build does not recognize.
pub const TIMING_UNKNOWN: &str = "apply.timing.unknown";

/// The timing key for one apply effect.
///
/// Total, including the unrecognized arm: a newer minor's effect must not silently borrow the
/// wording of a recognized one, because "live" and "after restart" are the two answers a user acts
/// on differently.
pub fn timing_key(effect: &ApplyEffect) -> &'static str {
    match effect {
        ApplyEffect::LiveImmediate => TIMING_LIVE_IMMEDIATE,
        ApplyEffect::VerifiedPending => TIMING_VERIFIED_PENDING,
        ApplyEffect::RestartRequired => TIMING_RESTART_REQUIRED,
        ApplyEffect::PersistentOnly => TIMING_PERSISTENT_ONLY,
        ApplyEffect::HeldUntilChainLoaded => TIMING_HELD_UNTIL_CHAIN_LOADED,
        ApplyEffect::Unrecognized(_) => TIMING_UNKNOWN,
    }
}

/// What a surface may offer as selectable options.
#[derive(Debug, Clone, PartialEq)]
pub enum ChoiceProjection {
    /// This control is not an enumeration.
    NotApplicable,
    /// The producer published a set.
    Enumerated {
        /// Whose enumeration this is.
        authority: Authority,
        /// How it was obtained.
        source: SourceClass,
        /// The options, in the authority's own order.
        choices: Vec<Choice>,
        /// How many the surface budget dropped, when it dropped any.
        truncated: Option<usize>,
    },
    /// A write in flight invalidates this enumeration. The stale list is deliberately withheld.
    Reloading {
        /// The control whose pending write invalidates this one.
        invalidated_by: ControlId,
        /// The operation doing the invalidating.
        operation: OperationId,
    },
    /// The producer could not read the set.
    ///
    /// Distinct from `Enumerated { choices: [] }`, which is a *successful* read of an empty
    /// collection. Collapsing the two is how "the engine offers nothing" and "we do not know what
    /// the engine offers" become the same blank dropdown.
    Unknown {
        /// Why it is unknown.
        reason: Reason,
    },
}

/// One operation the document still publishes for this control.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedOperation {
    /// Stable operation identity. Busy state is scoped to this, never to the surface.
    pub id: OperationId,
    /// The correlation the submitting surface supplied. Every surface follows the same one.
    pub correlation_id: String,
    /// How far the write got.
    pub write_attempt: WriteAttempt,
    /// The current outcome.
    pub outcome: CommandOutcome,
    /// Whether this outcome is evidence about the producer's observed state.
    pub is_state_evidence: bool,
    /// Whether the outcome may still change.
    pub awaits_convergence: bool,
    /// The last producer position this operation observed, so a surface can order results.
    pub observed: Option<RevisionRef>,
    /// What is needed to reach a coherent state.
    pub recovery: Option<RecoveryState>,
    /// Why, when the outcome needs one.
    pub reason: Option<Reason>,
    /// The value that was requested, when the record carries one.
    pub requested: Option<ControlValue>,
}

/// A constraint the producer published about this control.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedConstraint {
    /// Stable constraint id, so a surface can suppress a duplicate without matching a message.
    pub id: String,
    /// What holds when the condition holds.
    pub effect: crate::adaptive::ConstraintEffect,
    /// Machine-readable reason with a catalog key.
    pub reason: Reason,
}

/// One control, as one surface sees it.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectedControl {
    /// Stable semantic identity. Identical across every surface.
    pub id: ControlId,
    /// The producer's primitive, never substituted.
    pub kind: ControlKind,
    /// Catalog key for the label.
    pub label_key: Option<String>,
    /// Catalog key for help text.
    pub description_key: Option<String>,
    /// Group this control belongs to.
    pub group: Option<String>,
    /// Ordering hint within the group.
    pub order: Option<i32>,
    /// Unit symbol for scalar controls.
    pub unit: Option<String>,
    /// Availability, reason-carrying, verbatim from the producer.
    pub availability: Availability,
    /// The running value.
    ///
    /// Held separately from every other lane on purpose: the signal path must never preview intent
    /// the engine has not adopted.
    pub observed: Option<LaneValue>,
    /// Every other published lane, kept distinct rather than collapsed into "the value".
    pub intent: Vec<LaneValue>,
    /// Selectable options, or the reason there are none to offer.
    pub choices: ChoiceProjection,
    /// Numeric domain, for ranges.
    pub range: Option<NumericRange>,
    /// Whether and how this surface may offer mutation.
    pub mutability: Mutability,
    /// Operations the document still publishes for this control, scoped per operation.
    pub operations: Vec<ProjectedOperation>,
    /// Recorded lane disagreements.
    pub divergences: Vec<Divergence>,
    /// Constraints that name this control.
    pub constraints: Vec<ProjectedConstraint>,
}

impl ProjectedControl {
    /// The choice ids a surface may offer, empty for anything that is not an enumerated set.
    pub fn offered_choice_ids(&self) -> Vec<&str> {
        match &self.choices {
            ChoiceProjection::Enumerated { choices, .. } => {
                choices.iter().map(|choice| choice.id.as_str()).collect()
            }
            _ => Vec::new(),
        }
    }

    /// Whether this surface may offer mutation.
    pub fn is_mutable_here(&self) -> bool {
        matches!(self.mutability, Mutability::Mutable { .. })
    }
}

/// One surface's whole view of one producer, at one revision.
#[derive(Debug, Clone, PartialEq)]
pub struct RenderedProjection {
    /// Projection contract version.
    pub schema: SchemaVersion,
    /// The surface this was produced for.
    pub surface_id: String,
    /// Who is producing.
    pub producer: ProjectedProducer,
    /// The exact producer position this whole projection was taken at.
    ///
    /// One position for the whole value: a surface can never combine a mode read at one revision
    /// with an enumeration read at another, because it never receives two.
    pub position: RevisionRef,
    /// Whether the aggregator considers this current or last-known.
    pub presence: ProducerPresence,
    /// Per-lane health.
    pub lanes: Vec<ProjectedLaneHealth>,
    /// Presentation containers, in this surface's priority order.
    pub groups: Vec<ProjectedGroup>,
    /// The controls, in this surface's order.
    pub controls: Vec<ProjectedControl>,
    /// Controls this surface did not receive, and why.
    pub omitted: Vec<OmittedControl>,
    /// What the surface must tell the user about the projection as a whole.
    pub notices: Vec<ProjectionNotice>,
}

impl RenderedProjection {
    /// One control by id.
    pub fn control(&self, id: &str) -> Option<&ProjectedControl> {
        self.controls
            .iter()
            .find(|control| control.id.as_str() == id)
    }

    /// Whether a control was omitted from this surface.
    pub fn omitted_control(&self, id: &str) -> Option<&OmittedControl> {
        self.omitted.iter().find(|entry| entry.id.as_str() == id)
    }
}

// =============================================================================
// Projection
// =============================================================================

/// Project one admitted snapshot for one surface.
pub fn project(snapshot: &ProducerSnapshot, profile: &SurfaceProfile) -> SurfaceProjection {
    // RED stub: returns an empty rendered projection so the contract tests compile and fail.
    let document = snapshot.document.as_ref();
    SurfaceProjection::Rendered(Box::new(RenderedProjection {
        schema: SURFACE_PROJECTION_SCHEMA,
        surface_id: profile.surface_id.clone(),
        producer: projected_producer(document),
        position: document.position(),
        presence: snapshot.presence,
        lanes: Vec::new(),
        groups: Vec::new(),
        controls: Vec::new(),
        omitted: Vec::new(),
        notices: Vec::new(),
    }))
}

/// Admit a raw producer value and project it, degrading to a fallback when it cannot be admitted.
pub fn project_raw(raw: &serde_json::Value, profile: &SurfaceProfile) -> SurfaceProjection {
    // RED stub.
    let _ = raw;
    SurfaceProjection::Fallback(FallbackProjection {
        schema: SURFACE_PROJECTION_SCHEMA,
        surface_id: profile.surface_id.clone(),
        producer_id: None,
        producer_type: None,
        declared_schema: None,
        refusal: Refusal::MissingVersion,
    })
}

fn projected_producer(document: &ProducerDocument) -> ProjectedProducer {
    ProjectedProducer {
        producer_id: document.producer.producer_id.clone(),
        producer_type: document.producer.producer_type.clone(),
        instance_label: document.producer.instance_label.clone(),
        product_version: document.producer.product_version.clone(),
        epoch: document.producer.epoch,
        role: document.target.role.clone(),
        zone_id: document.target.zone_id.clone(),
    }
}

// Silence unused-import warnings while the stub is in place; the real implementation uses all of
// these.
#[allow(dead_code)]
type StubUnused = (
    AvailabilityState,
    BTreeMap<String, ()>,
    ControlGroup,
    Constraint,
    ProjectedConstraint,
    ValueLane,
    ReasonScope,
);
#[allow(dead_code)]
const STUB_UNUSED_KEYS: [&str; 5] = [
    WITHHELD_UNRENDERABLE_KIND,
    WITHHELD_PER_CHOICE_REASON,
    WITHHELD_RISK_EXCEEDS_SURFACE,
    OMITTED_BUDGET_EXHAUSTED,
    OMITTED_CONTAINER_DROPPED,
];
#[allow(dead_code)]
fn stub_unused(code: ReasonCode) -> Reason {
    Reason::observed(code)
}
#[allow(dead_code)]
const STUB_UNUSED_VERSION: SchemaVersion = CONSUMER_SCHEMA_VERSION;
