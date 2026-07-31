//! The publication admission gate.
//!
//! [`admit`] is the only door into [`super::ProducerAggregator`], and the aggregator is the
//! only holder of an admitted document. That is what turns "an incoherent document never
//! reaches a consumer" from a property of a code path into a property of the process.
//!
//! ## Two policies, and why they differ
//!
//! **Envelope problems refuse the whole document.** There is no partially-usable reading of
//! a document stamped with an unsupported major, addressed to an unparseable zone, or
//! carrying a lane that asserts both a reading and no reading. Refusal is safe here
//! *because the aggregator retains the last admitted snapshot*: a refused document fails to
//! advance a producer rather than blanking one. That asymmetry is the entire reason
//! envelope handling can be strict.
//!
//! **Published-intent incoherence demotes instead.** The only repair that would preserve a
//! `valid` entry is inventing the `desired` lane it is missing, and that fabricates intent
//! the user never staged. Demotion lowers validity and touches nothing else — enforced
//! structurally by [`apply_demotions`], whose only write is to `entry.validity`.
//!
//! ## Order of checks
//!
//! Version, then constraint bounds, then zone identity, then lane values, then all timestamp
//! fields, then document shape, then history, then coherence repair, then ordering. Repair runs **before** the
//! ordering comparison so
//! that both sides of that comparison are repaired: the stored document is post-repair, and
//! comparing a raw incoming document against it would report a spurious difference and
//! refuse an identical republication as [`AdmissionRefusal::NotAdvanced`].

use chrono::DateTime;
use std::collections::BTreeSet;

use super::run::AdapterRunId;
use crate::adaptive::{
    AvailabilityState, ChangeSetId, CommandOutcome, ControlId, DocumentRevisions, EntryValidity,
    Grounding, IntentIncoherence, OperationId, ProducerDocument, ProducerEpoch, Reason, ReasonCode,
    Refusal, TargetRole, TransportLane, ValueLane, CONSUMER_SCHEMA_VERSION,
};
use crate::bus::PrefixedZoneId;

/// Catalog key rendered when a staged entry targets a control that no longer exists.
///
/// Named once because two places must agree on it: this module and the canonical
/// `control_removed_after_advance` fixture consumers are built against. Catalog *text* is
/// governed by #343; only the key is fixed here.
pub const CONTROL_REMOVED_TEXT_KEY: &str = "reason.control_no_longer_exists";

/// Stable aggregator key for one producer document.
///
/// Deliberately carries no serde derives. It is aggregator-internal addressing, and the
/// less of this module is serializable the less of it can be exposed by accident. #327 may
/// need to persist a binding keyed on this; that issue can add the derive with a reason.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ProducerKey {
    /// Stable semantic producer id.
    pub producer_id: String,
    /// The role this document's controls act in.
    pub role: TargetRole,
    /// The prefixed zone id, when the producer is bound to a zone.
    pub zone_id: Option<String>,
}

impl ProducerKey {
    /// The key a document belongs under.
    pub fn of(document: &ProducerDocument) -> Self {
        Self {
            producer_id: document.producer.producer_id.clone(),
            role: document.target.role.clone(),
            zone_id: document.target.zone_id.clone(),
        }
    }
}

/// Which way a lane value contradicts itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LaneDefect {
    /// `grounding: grounded` with no value.
    GroundedWithoutValue,
    /// `grounding: ungrounded` carrying a value.
    UngroundedWithValue,
    /// `grounding: ungrounded` with no reason.
    UngroundedWithoutReason,
}

/// Why a document was not admitted.
#[derive(Debug, Clone, PartialEq)]
pub enum AdmissionRefusal {
    /// The contract layer refused it.
    Contract(Refusal),
    /// `target.zone_id` is not a prefixed zone id.
    ZoneIdNotPrefixed {
        /// The offending value.
        zone_id: String,
    },
    /// The epoch went backwards.
    EpochRegressed {
        /// Epoch currently held.
        previous: ProducerEpoch,
        /// Epoch offered.
        incoming: ProducerEpoch,
    },
    /// A revision went backwards within one epoch.
    RevisionRegressed {
        /// Revisions currently held.
        previous: DocumentRevisions,
        /// Revisions offered.
        incoming: DocumentRevisions,
    },
    /// Same revisions, different content.
    NotAdvanced {
        /// The revisions both documents claim.
        at: DocumentRevisions,
    },
    /// A lane value asserts both a reading and no reading.
    LaneValueInconsistent {
        /// The control carrying it.
        control: ControlId,
        /// The lane.
        lane: ValueLane,
        /// Which way.
        detail: LaneDefect,
    },
    /// Coherence repair did not converge.
    ///
    /// Unreachable while every [`IntentIncoherence`] arises from an entry that is currently
    /// `Valid`, because demoting all of those is a fixed point. It exists so that if a
    /// later minor version of the contract adds a violation kind that does not have that
    /// property, the document is refused rather than served incoherent — the failure this
    /// whole gate exists to prevent, arriving by a route this branch cannot foresee.
    IrreparableIntent {
        /// What survived the repair.
        violations: Vec<IntentIncoherence>,
    },
    /// The publishing adapter run is no longer live.
    ///
    /// Adapter lifecycle rather than producer identity, and the two are deliberately separate:
    /// this says *our* connection that sent the document has been replaced or ended, not that
    /// the engine behind it is gone. Decided from the synchronously-maintained run registry, so
    /// it holds however long the document sat in the channel and whatever public-bus events
    /// were processed meanwhile. See [`super::run`].
    StaleAdapterRun {
        /// The adapter.
        adapter: String,
        /// The run that published it.
        published_by: AdapterRunId,
        /// The run that is live now, if any.
        active: Option<AdapterRunId>,
    },
    /// The adapter tried to speak for a producer outside its namespace.
    ProducerOwnershipMismatch {
        /// Adapter that attempted the publication.
        adapter: String,
        /// Producer id it does not own.
        producer_id: String,
    },
    /// The producer was retired, and this document does not announce a restart.
    ///
    /// Lifecycle rather than document coherence, so it is decided by the aggregator and never
    /// by [`admit`]: the gate is pure and sees one document, while "this producer has been
    /// retired" is a fact about the store. Raised when a publication is delivered after an
    /// explicit actor retirement retired its key — and *only* then. Adapter lifecycle does not
    /// reach this variant; that is [`AdmissionRefusal::StaleAdapterRun`], because an adapter
    /// stopping says nothing about whether the engine behind it still exists. A strictly newer
    /// [`ProducerEpoch`] is a restart and is admitted instead.
    ProducerRetired {
        /// The epoch the producer held when it was retired.
        retired_at: ProducerEpoch,
        /// The epoch offered by the late document.
        incoming: ProducerEpoch,
    },
    /// A control publishes the same value lane twice.
    ///
    /// One lane is one reading. Two `desired` lanes make "the staged value" ambiguous, and
    /// `effective_view` resolves a single lane, so C1 coherence could be satisfied by one
    /// entry and contradicted by the other in the same document.
    DuplicateValueLane {
        /// The control carrying it.
        control: ControlId,
        /// The lane published twice.
        lane: ValueLane,
    },
    /// The document publishes health for the same transport lane twice.
    ///
    /// [`super::LaneWitness`] is keyed by lane, so a duplicate is not merely redundant - it
    /// is unrepresentable, and folding it would silently keep whichever entry happened to
    /// come last in a `Vec`.
    DuplicateLaneHealth {
        /// The lane published twice.
        lane: TransportLane,
    },
    /// Some [`crate::adaptive::Timestamp`] in the document is not an RFC 3339 instant.
    ///
    /// Every timestamp field, not only a lane's `last_success`: the type documents itself as
    /// RFC 3339 and consumers parse all of them. `field` is a dotted path so a producer author
    /// is told which one, since a document carries many.
    TimestampNotRfc3339 {
        /// Dotted path to the offending field.
        field: String,
        /// The offending value, quoted back so the author can see what was published.
        value: String,
    },
    /// A lane's `last_success` is not an RFC 3339 instant.
    ///
    /// [`crate::adaptive::Timestamp`] documents itself as RFC 3339, and every consumer that judges freshness
    /// has to parse it. An unparseable value is not a degraded reading but no reading at all,
    /// and admitting one puts a string no consumer can interpret where one it can is expected.
    /// Refused rather than dropped, because silently blanking the field would hide a producer
    /// bug that only its author can fix.
    LaneTimestampNotRfc3339 {
        /// The lane carrying it.
        lane: TransportLane,
        /// The offending value, quoted back so the author can see what was published.
        value: String,
    },
    /// A control is unavailable, deprecated or unknown without saying why.
    ///
    /// `Availability::is_well_formed` is published as a rule by the contract and was called
    /// by nothing on this path. A consumer that can see "disabled" but not why cannot
    /// explain itself, and the user cannot escape the state that caused it.
    AvailabilityNotWellFormed {
        /// The control carrying it.
        control: ControlId,
        /// The state asserted with no reason.
        state: AvailabilityState,
    },
    /// A recorded outcome transition the contract calls impossible.
    IllegalOutcomeHistory {
        /// The operation.
        operation: OperationId,
        /// Recorded source outcome.
        from: CommandOutcome,
        /// Recorded target outcome.
        to: CommandOutcome,
    },
}

/// One demotion applied to keep published intent coherent.
#[derive(Debug, Clone, PartialEq)]
pub struct IntentRepair {
    /// The violation that forced it.
    pub violation: IntentIncoherence,
    /// The draft holding the entry.
    pub change_set: ChangeSetId,
    /// The control the entry targets.
    pub control: ControlId,
    /// Validity before.
    pub from: EntryValidity,
    /// Validity after.
    pub to: EntryValidity,
}

/// How a document related to the one it replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmissionKind {
    /// First document for this key, or the first of a new epoch.
    Fresh,
    /// A revision advance.
    Advanced,
    /// Same revisions, lane health only.
    HealthRefresh,
}

/// A document that passed the gate.
#[derive(Debug, Clone, PartialEq)]
pub struct AdmittedDocument {
    /// The document as it will be served, after any coherence repair.
    pub document: ProducerDocument,
    /// What was demoted, and why.
    pub repairs: Vec<IntentRepair>,
    /// How it related to its predecessor.
    pub kind: AdmissionKind,
}

/// The gate's verdict.
#[derive(Debug, Clone, PartialEq)]
// The admitted variant *is* the document, and admission happens on every publication of
// every producer. Boxing it would trade a stack move for a heap allocation on that path to
// shrink a value that is immediately consumed. `BusEvent` carries the same allow for the
// same reason (`src/bus/events.rs`, "Zone is intentionally large for full state").
#[allow(clippy::large_enum_variant)]
pub enum Admission {
    /// Admitted, possibly repaired.
    Admitted(AdmittedDocument),
    /// Refused. The previous snapshot, if any, stands.
    Refused(AdmissionRefusal),
}

/// Admit `incoming`, given whatever is currently held for the same key.
///
/// Pure: no clock, no I/O, no lock. The aggregator supplies the predecessor and stores the
/// result, which keeps every ordering and coherence rule unit-testable without a runtime.
pub fn admit(previous: Option<&ProducerDocument>, incoming: ProducerDocument) -> Admission {
    // The typed path must apply the same version policy as `admit_document`. A document can
    // reach this gate as a typed value that never passed through JSON — relayed, or built
    // by an adapter — so the check cannot live only in the deserializer.
    if let Some(refusal) = incoming
        .schema_version
        .compatibility_for(CONSUMER_SCHEMA_VERSION)
        .refusal()
    {
        return Admission::Refused(AdmissionRefusal::Contract(refusal.clone()));
    }

    // Same argument for the published expression bounds.
    for constraint in &incoming.constraints {
        if let Err(limit) = constraint.validate() {
            return Admission::Refused(AdmissionRefusal::Contract(Refusal::ConstraintTooComplex {
                constraint: constraint.id.clone(),
                limit,
            }));
        }
    }

    // The obligation #323 recorded at `ProducerTarget::zone_id`: `PrefixedZoneId` is
    // `#[serde(transparent)]`, so its derived `Deserialize` never calls `parse`, and the
    // prefix vocabulary lives in this server-only module rather than in the shared
    // contract layer.
    if let Some(zone_id) = &incoming.target.zone_id {
        if PrefixedZoneId::parse(zone_id).is_none() {
            return Admission::Refused(AdmissionRefusal::ZoneIdNotPrefixed {
                zone_id: zone_id.clone(),
            });
        }
    }

    if let Some(refusal) = first_lane_defect(&incoming) {
        return Admission::Refused(refusal);
    }
    if let Some(refusal) = first_timestamp_defect(&incoming) {
        return Admission::Refused(refusal);
    }
    if let Some(refusal) = first_shape_defect(&incoming) {
        return Admission::Refused(refusal);
    }
    if let Some(refusal) = first_illegal_transition(&incoming) {
        return Admission::Refused(refusal);
    }

    let (document, repairs) = repair_intent(incoming);

    // The repair is a fixed point *given* that every violation arises from an entry that is
    // currently `Valid` — which is true of today's `intent_coherence_violations`. That is a
    // property of a function in another module, owned by another issue, and a later minor
    // version could add a violation kind that arises from a non-`Valid` entry. Rather than
    // rely on reasoning that this branch cannot enforce, the result is re-checked: if
    // anything survived the repair, the document is refused instead of served incoherent.
    // Costs one extra pass on a path that already clones.
    let survivors = document.intent_coherence_violations();
    if !survivors.is_empty() {
        return Admission::Refused(AdmissionRefusal::IrreparableIntent {
            violations: survivors,
        });
    }

    let kind = match previous {
        None => AdmissionKind::Fresh,
        Some(held) => match ordering(held, &document) {
            Ok(kind) => kind,
            Err(refusal) => return Admission::Refused(refusal),
        },
    };

    Admission::Admitted(AdmittedDocument {
        document,
        repairs,
        kind,
    })
}

fn first_timestamp_defect(document: &ProducerDocument) -> Option<AdmissionRefusal> {
    fn invalid(field: String, timestamp: &crate::adaptive::Timestamp) -> Option<AdmissionRefusal> {
        if DateTime::parse_from_rfc3339(timestamp.as_str()).is_ok() {
            return None;
        }
        Some(AdmissionRefusal::TimestampNotRfc3339 {
            field,
            value: timestamp.as_str().to_string(),
        })
    }

    for (lane_index, health) in document.lanes.iter().enumerate() {
        if let Some(timestamp) = &health.freshness.observed_at {
            if let Some(refusal) = invalid(
                format!("lanes[{lane_index}].freshness.observed_at"),
                timestamp,
            ) {
                return Some(refusal);
            }
        }
    }
    for (control_index, control) in document.controls.iter().enumerate() {
        for (value_index, value) in control.values.iter().enumerate() {
            if let Some(timestamp) = &value.freshness.observed_at {
                if let Some(refusal) = invalid(
                    format!(
                        "controls[{control_index}].values[{value_index}].freshness.observed_at"
                    ),
                    timestamp,
                ) {
                    return Some(refusal);
                }
            }
        }
        for (divergence_index, divergence) in control.divergences.iter().enumerate() {
            if let Some(timestamp) = &divergence.detected_at {
                if let Some(refusal) = invalid(
                    format!(
                        "controls[{control_index}].divergences[{divergence_index}].detected_at"
                    ),
                    timestamp,
                ) {
                    return Some(refusal);
                }
            }
        }
    }
    if let Some(policy) = &document.draft_policy {
        if let Some(timestamp) = &policy.retention.expires_at {
            if let Some(refusal) = invalid("draft_policy.retention.expires_at".into(), timestamp) {
                return Some(refusal);
            }
        }
    }
    for (change_set_index, change_set) in document.change_sets.iter().enumerate() {
        if let Some(refusal) = invalid(
            format!("change_sets[{change_set_index}].created_at"),
            &change_set.created_at,
        ) {
            return Some(refusal);
        }
        if let Some(refusal) = invalid(
            format!("change_sets[{change_set_index}].updated_at"),
            &change_set.updated_at,
        ) {
            return Some(refusal);
        }
        if let Some(timestamp) = &change_set.retention.expires_at {
            if let Some(refusal) = invalid(
                format!("change_sets[{change_set_index}].retention.expires_at"),
                timestamp,
            ) {
                return Some(refusal);
            }
        }
    }
    for (operation_index, operation) in document.operations.iter().enumerate() {
        for (history_index, transition) in operation.history.iter().enumerate() {
            if let Some(refusal) = invalid(
                format!("operations[{operation_index}].history[{history_index}].at"),
                &transition.at,
            ) {
                return Some(refusal);
            }
        }
    }
    None
}

/// How `incoming` relates to `held`, or why it may not replace it.
fn ordering(
    held: &ProducerDocument,
    incoming: &ProducerDocument,
) -> Result<AdmissionKind, AdmissionRefusal> {
    let previous_epoch = held.producer.epoch;
    let incoming_epoch = incoming.producer.epoch;

    if incoming_epoch < previous_epoch {
        return Err(AdmissionRefusal::EpochRegressed {
            previous: previous_epoch,
            incoming: incoming_epoch,
        });
    }
    if incoming_epoch > previous_epoch {
        // Revisions are only comparable within one epoch, so a restart legitimately
        // arrives with lower counters. Comparing them across the boundary would refuse
        // every document a restarted producer ever publishes.
        return Ok(AdmissionKind::Fresh);
    }

    if incoming.revisions.regresses_from(&held.revisions) {
        return Err(AdmissionRefusal::RevisionRegressed {
            previous: held.revisions,
            incoming: incoming.revisions,
        });
    }
    if incoming.revisions != held.revisions {
        return Ok(AdmissionKind::Advanced);
    }

    // Equal revisions. Two readings are possible and they have opposite consequences, so
    // the gate distinguishes them rather than picking one.
    //
    // Demanding a `state` bump for a lane transition would be simpler, but a change set is
    // validated against the state revision, so every lane flap would invalidate every open
    // draft on the producer. Admitting a health-only refresh keeps drafts alive and still
    // updates the lane witness. Anything else at the same revision is a producer bug: two
    // different documents cannot share one revision without destroying the meaning of
    // "one revision is one atomic snapshot".
    let mut probe = incoming.clone();
    probe.lanes.clone_from(&held.lanes);
    probe.stale = held.stale;
    if probe == *held {
        Ok(AdmissionKind::HealthRefresh)
    } else {
        Err(AdmissionRefusal::NotAdvanced { at: held.revisions })
    }
}

/// The first lane value that contradicts itself, if any.
///
/// Deliberately **not** [`crate::adaptive::LaneValue::is_consistent`], which returns `false`
/// for an unrecognized grounding. That is right for a consumer — an unrecognized grounding
/// must not be treated as authoritative — but using it here would refuse a
/// forward-compatible document from a newer minor version wholesale, which the compatibility
/// policy in §8 of the specification forbids. Unknown members degrade; they never abort.
fn first_lane_defect(document: &ProducerDocument) -> Option<AdmissionRefusal> {
    for control in &document.controls {
        for value in &control.values {
            let detail = match value.grounding {
                Grounding::Grounded if value.value.is_none() => {
                    Some(LaneDefect::GroundedWithoutValue)
                }
                Grounding::Ungrounded if value.value.is_some() => {
                    Some(LaneDefect::UngroundedWithValue)
                }
                Grounding::Ungrounded if value.ungrounded_reason.is_none() => {
                    Some(LaneDefect::UngroundedWithoutReason)
                }
                _ => None,
            };
            if let Some(detail) = detail {
                return Some(AdmissionRefusal::LaneValueInconsistent {
                    control: control.id.clone(),
                    lane: value.lane.clone(),
                    detail,
                });
            }
        }
    }
    None
}

/// The first structural defect in the document's lane health or controls, if any.
///
/// Four more rules the contract states — three with a published predicate — that had no code
/// path applying them: the defect class decision 3 of `.oh/adaptive-publication.md` names
/// outright. All four refuse rather than repair, because none has a repair that does not invent
/// something: choosing between two lanes with the same name picks one of two readings the
/// producer never disambiguated, inventing a reason for an unexplained unavailability tells the
/// user something no producer said, and guessing at a malformed instant fabricates a freshness
/// claim.
///
/// **Every rule here applies to unrecognized members too, and that is deliberate.** Forward
/// compatibility means an unknown member stays *representable* — the control is kept, its
/// observed value is kept, and the unknown state passes through unnormalized. It does not mean
/// structural invariants lapse when a name is unfamiliar:
///
/// * A duplicate lane is ambiguous whether or not this build knows the lane's name, and
///   [`super::LaneWitness`] is keyed by `TransportLane` including its `Unrecognized` arm, so a
///   duplicate genuinely collides.
/// * `Availability::is_well_formed` is true for `available` and requires at least one reason
///   for *every* other state, `Unrecognized` included. An unknown state is the case where that
///   reason matters most, because a consumer cannot even fall back on knowing what the state
///   means — exempting it would make the invariant weakest exactly where it is needed.
///
/// This is a narrower reading than [`first_lane_defect`] takes, and the difference is real
/// rather than an inconsistency: there, `LaneValue::is_consistent` returns `false` *because of*
/// an unrecognized grounding, so applying it would refuse a document for using a newer
/// vocabulary. Here the predicate is indifferent to the state's name and turns only on whether
/// a reason is present, which is a shape every version agrees on.
fn first_shape_defect(document: &ProducerDocument) -> Option<AdmissionRefusal> {
    let mut lanes_seen: BTreeSet<&TransportLane> = BTreeSet::new();
    for health in &document.lanes {
        if !lanes_seen.insert(&health.lane) {
            return Some(AdmissionRefusal::DuplicateLaneHealth {
                lane: health.lane.clone(),
            });
        }
        // Checked here rather than trusted downstream, so no unparseable instant is ever
        // stored, served, or compared. `chrono` is the same parser the aggregator's witness
        // uses, which is what makes "admitted implies comparable" true rather than hopeful.
        if let Some(last_success) = &health.last_success {
            if DateTime::parse_from_rfc3339(last_success.as_str()).is_err() {
                return Some(AdmissionRefusal::LaneTimestampNotRfc3339 {
                    lane: health.lane.clone(),
                    value: last_success.as_str().to_string(),
                });
            }
        }
    }

    for control in &document.controls {
        let mut value_lanes: BTreeSet<&ValueLane> = BTreeSet::new();
        for value in &control.values {
            if !value_lanes.insert(&value.lane) {
                return Some(AdmissionRefusal::DuplicateValueLane {
                    control: control.id.clone(),
                    lane: value.lane.clone(),
                });
            }
        }
        if !control.availability.is_well_formed() {
            return Some(AdmissionRefusal::AvailabilityNotWellFormed {
                control: control.id.clone(),
                state: control.availability.state.clone(),
            });
        }
    }

    None
}

/// The first recorded outcome transition the contract calls impossible, if any.
///
/// Only per-transition legality is checked, using the contract's own predicate. Chain
/// continuity — that each transition's `from` equals the previous `to` — is true by
/// construction of `OperationRecord::transition`, but the contract publishes no predicate
/// for it, and inventing one at the door would refuse documents on a rule their author was
/// never told about.
fn first_illegal_transition(document: &ProducerDocument) -> Option<AdmissionRefusal> {
    for operation in &document.operations {
        for transition in &operation.history {
            if !transition.from.may_transition_to(&transition.to) {
                return Some(AdmissionRefusal::IllegalOutcomeHistory {
                    operation: operation.id.clone(),
                    from: transition.from.clone(),
                    to: transition.to.clone(),
                });
            }
        }
    }
    None
}

/// One entry validity to lower, and the violation that forced it.
struct Demotion {
    change_set: ChangeSetId,
    control: ControlId,
    to: EntryValidity,
    violation: IntentIncoherence,
}

/// Bring a document's published intent into coherence by lowering validity only.
fn repair_intent(mut document: ProducerDocument) -> (ProducerDocument, Vec<IntentRepair>) {
    let violations = document.intent_coherence_violations();
    if violations.is_empty() {
        return (document, Vec::new());
    }

    let mut demotions: Vec<Demotion> = Vec::new();
    for violation in violations {
        match &violation {
            // C1. The entry claims to be applicable but the document publishes no matching
            // staged value, so `effective_view` would silently fall back to `observed` and
            // every constraint over the control would evaluate against the running engine.
            // Apply is blocked; the producer must republish coherently.
            IntentIncoherence::MissingDesiredLane {
                control,
                change_set,
            }
            | IntentIncoherence::DesiredLaneDisagrees {
                control,
                change_set,
            } => {
                let mut reason = Reason::draft(ReasonCode::DraftInvalid);
                reason.detail = Some(
                    "published entry has no matching grounded desired lane (contract C1)"
                        .to_string(),
                );
                demotions.push(Demotion {
                    change_set: change_set.clone(),
                    control: control.clone(),
                    to: EntryValidity::RequiresProducerValidation { reason },
                    violation: violation.clone(),
                });
            }
            // C2. Both claimants lose validity, not one. A single `desired` lane cannot
            // represent two drafts, the document carries no arrival order to break the tie
            // with, and keeping one `valid` would authorize an apply nobody confirmed.
            // "At most one valid" is satisfied by zero, and zero is the only answer that
            // does not depend on `Vec` order. `draft_policy.on_conflict` is not consulted:
            // it governs staging, and #324 implements no staging path.
            IntentIncoherence::MultipleValidDrafts {
                control,
                change_set,
                also_claimed_by,
            } => {
                for claimant in [change_set, also_claimed_by] {
                    demotions.push(Demotion {
                        change_set: claimant.clone(),
                        control: control.clone(),
                        to: EntryValidity::Conflicts {
                            with: vec![control.clone()],
                        },
                        violation: violation.clone(),
                    });
                }
            }
            // A control-plane advance removed a control an open draft still targets. This
            // must stay distinguishable from "needs producer validation": no amount of
            // revalidation brings a removed control back, and a user told to wait for
            // validation will wait forever.
            IntentIncoherence::UnknownControl {
                control,
                change_set,
            } => {
                let mut reason = Reason::draft(ReasonCode::ControlRemoved);
                // Both fields, and they are not interchangeable. `detail` names the specific
                // control for a log reader; `display_text_key` is what a consumer can
                // actually render to a user, because catalog keys localise and English
                // diagnostic prose does not. Emitting only `detail` would leave the user
                // with either untranslated text or nothing at all.
                reason.display_text_key = Some(CONTROL_REMOVED_TEXT_KEY.to_string());
                reason.detail = Some(format!(
                    "control {control} is no longer described by this producer document"
                ));
                demotions.push(Demotion {
                    change_set: change_set.clone(),
                    control: control.clone(),
                    to: EntryValidity::DraftInvalid { reason },
                    violation: violation.clone(),
                });
            }
        }
    }

    let repairs = apply_demotions(&mut document, &demotions);
    (document, repairs)
}

/// Apply demotions. **The only field this function writes is `entry.validity`.**
///
/// That is the mechanical form of "never fabricate desired intent". Every violation names a
/// `(change_set, control)` pair, every valid entry for that pair is lowered, and violations
/// only ever arise from entries that are currently `Valid` — so one pass is a fixed point
/// and no second sweep can find anything left to repair.
fn apply_demotions(document: &mut ProducerDocument, demotions: &[Demotion]) -> Vec<IntentRepair> {
    let mut repairs = Vec::new();
    for change_set in &mut document.change_sets {
        for entry in &mut change_set.entries {
            if entry.validity != EntryValidity::Valid {
                continue;
            }
            let Some(demotion) = demotions.iter().find(|candidate| {
                candidate.change_set == change_set.id && candidate.control == entry.control
            }) else {
                continue;
            };
            let from = entry.validity.clone();
            entry.validity = demotion.to.clone();
            repairs.push(IntentRepair {
                violation: demotion.violation.clone(),
                change_set: change_set.id.clone(),
                control: entry.control.clone(),
                from,
                to: demotion.to.clone(),
            });
        }
    }
    repairs
}
