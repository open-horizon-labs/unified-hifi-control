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
//! ## What this module deliberately does not know
//!
//! No producer family, no engine, no catalog text. Two facts that look producer-specific are
//! parameters rather than knowledge: a surface's group priority comes from its own
//! [`SurfaceBudget`], and the set of verified backend releases comes from a caller-supplied
//! [`VerifiedSeries`]. The HQPlayer entry for the latter lives beside the HQPlayer producer.

use std::collections::{BTreeMap, BTreeSet};

use crate::adaptive::{
    ApplyEffect, ApplyLane, ApplySemantics, Authority, Availability, Choice, CommandOutcome,
    Constraint, ConstraintEffect, Control, ControlId, ControlKind, ControlValue, Disruption,
    Divergence, LaneHealth, LaneState, LaneValue, NumericRange, OperationId, OperationRecord,
    ProducerDocument, ProducerEpoch, Reason, ReasonCode, ReasonScope, RecoveryState, Refusal,
    RevisionRef, RiskClass, SchemaVersion, SourceClass, TargetRole, Timestamp, TransportLane,
    ValueLane, WriteAttempt, CONSUMER_SCHEMA_VERSION,
};

use super::aggregator::{ProducerPresence, ProducerSnapshot};

/// The version of the *projection* contract, deliberately independent of the producer contract's.
///
/// A surface negotiates what it can render; the producer negotiates what it can say. Tying the two
/// together would mean a new producer minor forced every client to re-qualify.
pub const SURFACE_PROJECTION_SCHEMA: SchemaVersion = SchemaVersion::new(1, 0);

/// Catalog key for a control whose primitive this surface cannot render.
///
/// Keys, never prose: #343 owns the catalog and its provenance. A surface with no catalog entry
/// still has the [`ReasonCode`] to branch on.
pub const WITHHELD_UNRENDERABLE_KIND: &str = "surface.withheld.unrenderable_kind";
/// Catalog key for a control whose per-choice availability this surface cannot express.
pub const WITHHELD_PER_CHOICE_REASON: &str = "surface.withheld.per_choice_reason";
/// Catalog key for a control whose risk class exceeds what this surface may offer.
pub const WITHHELD_RISK_EXCEEDS_SURFACE: &str = "surface.withheld.risk_exceeds_surface";
/// Catalog key for a control dropped because the surface's control budget was exhausted.
pub const OMITTED_BUDGET_EXHAUSTED: &str = "surface.omitted.budget_exhausted";
/// Catalog key for a control dropped because the container it belongs to was dropped.
pub const OMITTED_CONTAINER_DROPPED: &str = "surface.omitted.container_dropped";
/// Catalog key for an enumeration the producer declared but did not publish.
pub const CHOICES_NOT_PUBLISHED: &str = "surface.choices.not_published";

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
    /// The highest risk class this surface may advertise as invokable.
    ///
    /// [`RiskClass::Unrecognized`] sorts above every recognized member, so a risk class from a
    /// newer minor is never advertised as invokable by an older surface. That is the safe
    /// direction: the control stays visible, the mutation does not.
    pub max_risk: RiskClass,
}

impl SurfaceCapabilities {
    /// A surface that can render every primitive this build knows, including per-choice reasons.
    pub fn unrestricted() -> Self {
        Self {
            renders_kinds: ControlKind::known().into_iter().collect(),
            per_choice_reasons: true,
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
    /// A browser: renders every primitive, no budget, and a human present to confirm.
    pub fn web() -> Self {
        Self {
            surface_id: "web".to_string(),
            capabilities: SurfaceCapabilities::unrestricted(),
            budget: SurfaceBudget::default(),
        }
    }

    /// An AI tool client: every primitive is expressible as data, but nothing confirms at invoke
    /// time, so a destructive operation is not advertised as invokable.
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

    /// A small control device: bounded, transport and volume first, no confirmation dialog, and no
    /// primitive that needs a per-option explanation.
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
                max_risk: RiskClass::Caution,
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

/// Backend release series this build has actually verified, per producer family.
///
/// Empty means "no opinion", which raises no notice. The projector holds no engine knowledge; the
/// HQPlayer entry is supplied by the HQPlayer producer, which owns the evidence ledger that decides
/// what "verified" means.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VerifiedSeries {
    prefixes: BTreeMap<String, Vec<String>>,
}

impl VerifiedSeries {
    /// Declare the verified `product_version` prefixes for one producer family.
    pub fn with(
        mut self,
        producer_type: impl Into<String>,
        prefixes: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        self.prefixes.insert(
            producer_type.into(),
            prefixes.into_iter().map(Into::into).collect(),
        );
        self
    }

    /// Whether this build has an opinion about `producer_type` at all.
    pub fn has_opinion(&self, producer_type: &str) -> bool {
        self.prefixes.contains_key(producer_type)
    }

    /// Whether `product_version` falls in a verified series.
    ///
    /// True when there is no opinion: silence is not evidence of a problem, and a notice nobody can
    /// act on trains users to ignore notices.
    pub fn covers(&self, producer_type: &str, product_version: &str) -> bool {
        match self.prefixes.get(producer_type) {
            None => true,
            Some(prefixes) => prefixes
                .iter()
                .any(|prefix| product_version.starts_with(prefix.as_str())),
        }
    }
}

// =============================================================================
// Projection value types
// =============================================================================

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
        lane: TransportLane,
        /// Its state.
        state: LaneState,
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

/// A presentation container, in this surface's order.
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
    ///
    /// Kept beside the producer's own words rather than merged into them: editing a producer's
    /// statement is the fabrication #324's coherence rules forbid.
    pub last_success_seen: Option<Timestamp>,
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
        /// The producer's reasons, verbatim.
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
    pub effect: ConstraintEffect,
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
    /// Member ids, for groups and composites.
    pub members: Vec<ControlId>,
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
// Admission for a surface
// =============================================================================

/// Admit a raw producer value for a surface, or produce the fallback that explains why not.
///
/// This is the *compatibility* entry point, and it is deliberately the same
/// [`crate::adaptive::admit_document`] gate every other consumer uses — it does not open a second
/// path into the aggregator. Its whole contribution is turning a refusal into something a surface
/// can render: a legible identity plus the reason, rather than an empty panel.
/// The error is boxed because it is the rare branch and the whole point of it is to be rich: a
/// legible identity plus a structured refusal is bigger than the document handle it replaces.
pub fn admit_for_surface(
    raw: &serde_json::Value,
    profile: &SurfaceProfile,
) -> Result<ProducerDocument, Box<FallbackProjection>> {
    crate::adaptive::admit_document(raw).map_err(|refusal| {
        Box::new(FallbackProjection {
            schema: SURFACE_PROJECTION_SCHEMA,
            surface_id: profile.surface_id.clone(),
            // Read straight off the raw value, because the body is exactly what could not be
            // trusted. Both spellings are tried: a generation that reshaped identity still usually
            // leaves a string a human can recognize somewhere obvious, and naming the producer you
            // cannot render is the difference between a diagnosable state and a blank one.
            producer_id: legible_string(raw, &["producer", "producer_id"])
                .or_else(|| legible_string(raw, &["producer", "identity", "urn"])),
            producer_type: legible_string(raw, &["producer", "producer_type"]),
            declared_schema: legible_string(raw, &["schema_version"]),
            refusal,
        })
    })
}

fn legible_string(raw: &serde_json::Value, path: &[&str]) -> Option<String> {
    let mut cursor = raw;
    for segment in path {
        cursor = cursor.get(segment)?;
    }
    cursor.as_str().map(str::to_string)
}

// =============================================================================
// Projection
// =============================================================================

/// Project one admitted snapshot for one surface, with no opinion about backend releases.
pub fn project(snapshot: &ProducerSnapshot, profile: &SurfaceProfile) -> SurfaceProjection {
    project_with(snapshot, profile, &VerifiedSeries::default())
}

/// Project one admitted snapshot for one surface.
pub fn project_with(
    snapshot: &ProducerSnapshot,
    profile: &SurfaceProfile,
    verified: &VerifiedSeries,
) -> SurfaceProjection {
    let document = snapshot.document.as_ref();
    let mut notices = document_notices(snapshot, document, verified);

    let invalidation = Invalidation::of(document);
    let ranked = rank_controls(document, profile);
    let (kept, omitted) = apply_budget(document, ranked, profile, &mut notices);

    let controls: Vec<ProjectedControl> = kept
        .iter()
        .map(|control| project_control(control, document, profile, &invalidation, &mut notices))
        .collect();

    SurfaceProjection::Rendered(Box::new(RenderedProjection {
        schema: SURFACE_PROJECTION_SCHEMA,
        surface_id: profile.surface_id.clone(),
        producer: projected_producer(document),
        position: document.position(),
        presence: snapshot.presence,
        lanes: project_lanes(snapshot, document),
        groups: project_groups(document, profile),
        controls,
        omitted,
        notices,
    }))
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

fn document_notices(
    snapshot: &ProducerSnapshot,
    document: &ProducerDocument,
    verified: &VerifiedSeries,
) -> Vec<ProjectionNotice> {
    let mut notices = Vec::new();

    if document.schema_version.major == CONSUMER_SCHEMA_VERSION.major
        && document.schema_version.minor > CONSUMER_SCHEMA_VERSION.minor
    {
        notices.push(ProjectionNotice::UnknownSchemaAdditions {
            document: document.schema_version,
            consumer: CONSUMER_SCHEMA_VERSION,
        });
    }

    if let Some(product_version) = &document.producer.product_version {
        if !verified.covers(&document.producer.producer_type, product_version) {
            notices.push(ProjectionNotice::UntestedProducerVersion {
                product_version: product_version.clone(),
            });
        }
    }

    if snapshot.presence == ProducerPresence::LastKnown {
        notices.push(ProjectionNotice::LastKnown);
    }
    if document.stale {
        notices.push(ProjectionNotice::ProducerStale);
    }
    for health in &document.lanes {
        if health.state != LaneState::Connected {
            notices.push(ProjectionNotice::LaneNotHealthy {
                lane: health.lane.clone(),
                state: health.state.clone(),
            });
        }
    }
    if !snapshot.repairs.is_empty() {
        notices.push(ProjectionNotice::IntentRepaired {
            count: snapshot.repairs.len(),
        });
    }
    notices
}

fn project_lanes(
    snapshot: &ProducerSnapshot,
    document: &ProducerDocument,
) -> Vec<ProjectedLaneHealth> {
    document
        .lanes
        .iter()
        .map(|health| ProjectedLaneHealth {
            health: health.clone(),
            last_success_seen: snapshot
                .lanes
                .get(&health.lane)
                .and_then(|witness| witness.last_success.clone()),
        })
        .collect()
}

/// The surface's rank for a group id: its position in the profile's priority list, then everything
/// else. Groups the profile did not rank keep the producer's own ordering among themselves.
fn group_rank(profile: &SurfaceProfile, group: Option<&str>) -> usize {
    let ranked = &profile.budget.priority_groups;
    match group {
        Some(id) => ranked
            .iter()
            .position(|candidate| candidate == id)
            .unwrap_or(ranked.len()),
        None => ranked.len(),
    }
}

fn project_groups(document: &ProducerDocument, profile: &SurfaceProfile) -> Vec<ProjectedGroup> {
    let mut groups: Vec<ProjectedGroup> = document
        .groups
        .iter()
        .map(|group| ProjectedGroup {
            id: group.id.clone(),
            label_key: group.label_key.clone(),
            rank: group_rank(profile, Some(group.id.as_str())),
            order: group.order,
        })
        .collect();
    groups.sort_by(|left, right| {
        left.rank
            .cmp(&right.rank)
            .then(
                left.order
                    .unwrap_or(i32::MAX)
                    .cmp(&right.order.unwrap_or(i32::MAX)),
            )
            .then(left.id.cmp(&right.id))
    });
    groups
}

/// Controls in this surface's presentation order.
///
/// Total and deterministic: rank, then the group's own order, then the control's order, then id.
/// A partial ordering would make truncation depend on `Vec` order, which is producer-arbitrary.
fn rank_controls<'a>(document: &'a ProducerDocument, profile: &SurfaceProfile) -> Vec<&'a Control> {
    let group_order: BTreeMap<&str, i32> = document
        .groups
        .iter()
        .map(|group| (group.id.as_str(), group.order.unwrap_or(i32::MAX)))
        .collect();

    let mut ordered: Vec<&Control> = document.controls.iter().collect();
    ordered.sort_by(|left, right| {
        let left_group = left.group.as_deref();
        let right_group = right.group.as_deref();
        group_rank(profile, left_group)
            .cmp(&group_rank(profile, right_group))
            .then(
                group_order
                    .get(left_group.unwrap_or_default())
                    .copied()
                    .unwrap_or(i32::MAX)
                    .cmp(
                        &group_order
                            .get(right_group.unwrap_or_default())
                            .copied()
                            .unwrap_or(i32::MAX),
                    ),
            )
            .then(left_group.cmp(&right_group))
            .then(
                left.order
                    .unwrap_or(i32::MAX)
                    .cmp(&right.order.unwrap_or(i32::MAX)),
            )
            .then(left.id.cmp(&right.id))
    });
    ordered
}

/// Cut the ordered control list to the surface's budget, declaring every drop.
///
/// Two passes, and the second is the one that matters: after the budget cut, any surviving control
/// whose container was cut is cut too, and reported under the container's reason rather than the
/// budget's. A coupled half rendered with no container is worse than a missing control, because the
/// surface would offer an edit whose other half it cannot see.
fn apply_budget<'a>(
    document: &ProducerDocument,
    ordered: Vec<&'a Control>,
    profile: &SurfaceProfile,
    notices: &mut Vec<ProjectionNotice>,
) -> (Vec<&'a Control>, Vec<OmittedControl>) {
    let advertised = ordered.len();
    let mut omitted: Vec<OmittedControl> = Vec::new();

    let mut kept: Vec<&Control> = match profile.budget.max_controls {
        Some(budget) if advertised > budget => {
            notices.push(ProjectionNotice::Truncated { budget, advertised });
            for control in ordered.iter().skip(budget) {
                omitted.push(OmittedControl {
                    id: control.id.clone(),
                    code: ReasonCode::RequiresCapability,
                    display_text_key: OMITTED_BUDGET_EXHAUSTED,
                });
            }
            ordered.into_iter().take(budget).collect()
        }
        _ => ordered,
    };

    // Containers may nest, so iterate to a fixed point rather than assuming one level.
    loop {
        let present: BTreeSet<&ControlId> = kept.iter().map(|control| &control.id).collect();
        let orphaned: BTreeSet<ControlId> = document
            .controls
            .iter()
            .filter(|container| !present.contains(&container.id))
            .flat_map(|container| container.members.iter().cloned())
            .filter(|member| present.contains(member))
            .collect();
        if orphaned.is_empty() {
            break;
        }
        kept.retain(|control| !orphaned.contains(&control.id));
        for id in orphaned {
            omitted.push(OmittedControl {
                id,
                code: ReasonCode::RequiresCapability,
                display_text_key: OMITTED_CONTAINER_DROPPED,
            });
        }
    }

    // A member dropped by the budget whose container also went is reported for the container.
    let dropped: BTreeSet<&ControlId> = document
        .controls
        .iter()
        .filter(|control| !kept.iter().any(|keeper| keeper.id == control.id))
        .map(|control| &control.id)
        .collect();
    let orphan_ids: BTreeSet<ControlId> = document
        .controls
        .iter()
        .filter(|container| dropped.contains(&container.id))
        .flat_map(|container| container.members.iter().cloned())
        .collect();
    for entry in &mut omitted {
        if orphan_ids.contains(&entry.id) {
            entry.display_text_key = OMITTED_CONTAINER_DROPPED;
        }
    }
    omitted.sort_by(|left, right| left.id.cmp(&right.id));
    omitted.dedup_by(|left, right| left.id == right.id);

    (kept, omitted)
}

/// Which controls have their enumeration invalidated by a write that has not settled.
///
/// The predicate is [`CommandOutcome::is_state_evidence`], negated: `Pending`, `Indeterminate`,
/// `TimedOut` and `Compensating` are the four outcomes #323 says describe the *operation* rather
/// than the producer. While one of those is outstanding on a control that declares `invalidates`,
/// the dependent enumerations published beside it may already be wrong.
struct Invalidation {
    by_control: BTreeMap<ControlId, (ControlId, OperationId)>,
}

impl Invalidation {
    fn of(document: &ProducerDocument) -> Self {
        let mut by_control = BTreeMap::new();
        // Two outstanding writes can invalidate the same enumeration, and the surface is shown one
        // of them. Choosing by operation id rather than by position in `operations` keeps that
        // choice independent of a `Vec` order the producer never promised — the same arbitrariness
        // #324 refused to depend on when it demoted every C2 claimant instead of keeping the first.
        let mut operations: Vec<&OperationRecord> = document.operations.iter().collect();
        operations.sort_by(|left, right| left.id.as_str().cmp(right.id.as_str()));
        for operation in operations {
            if operation.outcome.is_state_evidence() {
                continue;
            }
            let Some(request) = &operation.request else {
                continue;
            };
            let Some(control) = document.control(&request.control) else {
                continue;
            };
            let Some(apply) = &control.apply else {
                continue;
            };
            for dependent in &apply.invalidates {
                if dependent == &control.id {
                    // A write does not invalidate its own enumeration; the producer republishes it
                    // with the result. Guarding here keeps a self-referential descriptor from
                    // blanking the control the user is holding.
                    continue;
                }
                by_control
                    .entry(dependent.clone())
                    .or_insert_with(|| (control.id.clone(), operation.id.clone()));
            }
        }
        Self { by_control }
    }

    fn for_control(&self, id: &ControlId) -> Option<&(ControlId, OperationId)> {
        self.by_control.get(id)
    }
}

fn project_control(
    control: &Control,
    document: &ProducerDocument,
    profile: &SurfaceProfile,
    invalidation: &Invalidation,
    notices: &mut Vec<ProjectionNotice>,
) -> ProjectedControl {
    if !control.kind.is_recognized() {
        notices.push(ProjectionNotice::UnrecognizedControlKind {
            control: control.id.clone(),
            kind: control.kind.as_str().to_string(),
        });
    }

    let observed = control.lane(&ValueLane::Observed).cloned();
    let intent: Vec<LaneValue> = control
        .values
        .iter()
        .filter(|value| value.lane != ValueLane::Observed)
        .cloned()
        .collect();

    let choices = project_choices(control, profile, invalidation);
    let mutability = project_mutability(control, profile);

    ProjectedControl {
        id: control.id.clone(),
        kind: control.kind.clone(),
        label_key: control.label_key.clone(),
        description_key: control.description_key.clone(),
        group: control.group.clone(),
        order: control.order,
        unit: control.unit.clone(),
        availability: control.availability.clone(),
        observed,
        intent,
        choices,
        range: control.range.clone(),
        mutability,
        operations: project_operations(control, document),
        divergences: control.divergences.clone(),
        constraints: project_constraints(control, document),
        members: control.members.clone(),
    }
}

fn project_choices(
    control: &Control,
    profile: &SurfaceProfile,
    invalidation: &Invalidation,
) -> ChoiceProjection {
    if let Some((invalidated_by, operation)) = invalidation.for_control(&control.id) {
        return ChoiceProjection::Reloading {
            invalidated_by: invalidated_by.clone(),
            operation: operation.clone(),
        };
    }

    let Some(set) = &control.choices else {
        // An enumeration with no published set is not an empty set: the producer declared a control
        // whose options it could not read. Anything else is simply not a chooser.
        return if control.kind == ControlKind::Enumeration {
            ChoiceProjection::Unknown {
                reason: Reason {
                    code: ReasonCode::Unobservable,
                    scope: ReasonScope::Observed,
                    display_text_key: Some(CHOICES_NOT_PUBLISHED.to_string()),
                    detail: None,
                },
            }
        } else {
            ChoiceProjection::NotApplicable
        };
    };

    let (choices, truncated) = narrow(&set.choices, control, profile);
    ChoiceProjection::Enumerated {
        authority: set.authority.clone(),
        source: set.source.clone(),
        choices,
        truncated,
    }
}

/// Shorten an option list to the surface's budget without losing the current selection.
///
/// Head truncation alone produces a control whose value is not among its options, which is the
/// state a user cannot reason about or escape. Every currently-held selection — observed, desired
/// and held — is retained, and the drop count is reported so the surface can say the list is
/// partial.
fn narrow(
    choices: &[Choice],
    control: &Control,
    profile: &SurfaceProfile,
) -> (Vec<Choice>, Option<usize>) {
    let Some(budget) = profile.budget.max_choices else {
        return (choices.to_vec(), None);
    };
    if choices.len() <= budget {
        return (choices.to_vec(), None);
    }

    let selected: BTreeSet<String> = [ValueLane::Observed, ValueLane::Desired, ValueLane::Held]
        .iter()
        .filter_map(|lane| control.lane(lane))
        .filter_map(LaneValue::grounded_value)
        .filter_map(|value| match value {
            ControlValue::Choice(id) => Some(id.clone()),
            _ => None,
        })
        .collect();

    // Membership first: every held selection, then fill to the budget from the top of the
    // authority's list. The budget can be exceeded when a control holds more selections than the
    // surface asked for — deliberately, because a control whose own value is missing from its
    // options is a state the user cannot reason about, and a list one item too long is not.
    let mut kept: BTreeSet<&str> = choices
        .iter()
        .map(|choice| choice.id.as_str())
        .filter(|id| selected.contains(*id))
        .collect();
    for choice in choices {
        if kept.len() >= budget {
            break;
        }
        kept.insert(choice.id.as_str());
    }
    // Then position: the authority's own order, which the contract says is meaningful.
    let ordered: Vec<Choice> = choices
        .iter()
        .filter(|choice| kept.contains(choice.id.as_str()))
        .cloned()
        .collect();
    let dropped = choices.len() - ordered.len();
    (ordered, (dropped > 0).then_some(dropped))
}

/// The precedence that decides who is responsible for a control not being writable.
///
/// Producer truth first, always: a surface that reports its own limitation where the producer
/// already had a reason is how a user concludes the engine is broken and goes looking for a fault
/// that is not there.
///
/// 1. No apply semantics at all — observation only, whatever availability says.
/// 2. The producer's availability forbids mutation — [`Mutability::Blocked`], with its reasons.
/// 3. This surface cannot render the primitive.
/// 4. This surface cannot express the control's per-choice availability.
/// 5. The risk exceeds what this surface may offer.
fn project_mutability(control: &Control, profile: &SurfaceProfile) -> Mutability {
    let Some(apply) = &control.apply else {
        return Mutability::ObservationOnly;
    };
    if !control.availability.permits_mutation() {
        return Mutability::Blocked {
            reasons: control.availability.reasons.clone(),
        };
    }

    if !profile.capabilities.renders_kinds.contains(&control.kind) {
        return withheld(WITHHELD_UNRENDERABLE_KIND, control.kind.as_str());
    }

    let carries_choice_reasons = control.choices.as_ref().is_some_and(|set| {
        set.choices
            .iter()
            .any(|choice| choice.availability.is_some())
    });
    if carries_choice_reasons && !profile.capabilities.per_choice_reasons {
        return withheld(WITHHELD_PER_CHOICE_REASON, control.id.as_str());
    }

    if apply.risk > profile.capabilities.max_risk {
        return withheld(WITHHELD_RISK_EXCEEDS_SURFACE, apply.risk.as_str());
    }

    Mutability::Mutable {
        apply: Box::new(apply.clone()),
        description: describe(apply),
    }
}

fn withheld(display_text_key: &str, detail: &str) -> Mutability {
    Mutability::WithheldFromSurface {
        reason: Reason {
            code: ReasonCode::RequiresCapability,
            // The gap is the *surface's*, and `ReasonScope` has no surface member in v1. `Producer`
            // would misattribute it, so the reason is scoped to the lane-independent `Draft` view
            // the surface owns, and the catalog key names the surface limitation exactly.
            scope: ReasonScope::Draft,
            display_text_key: Some(display_text_key.to_string()),
            detail: Some(detail.to_string()),
        },
    }
}

fn describe(apply: &ApplySemantics) -> ApplyDescription {
    ApplyDescription {
        lane: apply.lane.clone(),
        effect: apply.effect.clone(),
        disruption: apply.disruption.clone(),
        risk: apply.risk.clone(),
        timing_key: timing_key(&apply.effect),
        acknowledgement_is_evidence: !apply.verification.provisional_ack,
        invalidates: apply.invalidates.clone(),
        coupled_with: apply.coupled_with.clone(),
    }
}

fn project_operations(control: &Control, document: &ProducerDocument) -> Vec<ProjectedOperation> {
    document
        .operations
        .iter()
        .filter(|operation| targets(operation, &control.id))
        .map(|operation| ProjectedOperation {
            id: operation.id.clone(),
            correlation_id: operation.correlation_id.clone(),
            write_attempt: operation.write_attempt.clone(),
            outcome: operation.outcome.clone(),
            is_state_evidence: operation.outcome.is_state_evidence(),
            awaits_convergence: operation.outcome.awaits_convergence(),
            observed: operation.observed,
            recovery: operation.recovery.clone(),
            reason: operation.reason.clone(),
            requested: operation
                .request
                .as_ref()
                .map(|request| request.requested.clone()),
        })
        .collect()
}

/// Whether an operation is *this* control's busy marker.
///
/// Direct requests and plan steps both count; nothing else does. A global busy flag is the pattern
/// the HQPTuner audit told this issue not to inherit — one control's completion clearing another's
/// marker is exactly what unscoped state produces.
fn targets(operation: &OperationRecord, control: &ControlId) -> bool {
    operation
        .request
        .as_ref()
        .is_some_and(|request| &request.control == control)
        || operation.steps.iter().any(|step| &step.control == control)
}

fn project_constraints(control: &Control, document: &ProducerDocument) -> Vec<ProjectedConstraint> {
    document
        .constraints
        .iter()
        .filter(|constraint| constraint.applies_to.contains(&control.id))
        .map(|constraint: &Constraint| ProjectedConstraint {
            id: constraint.id.clone(),
            effect: constraint.effect.clone(),
            reason: constraint.reason.clone(),
        })
        .collect()
}
