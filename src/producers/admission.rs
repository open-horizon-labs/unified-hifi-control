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
//! Version, then constraint bounds, then zone identity, then lane values, then history,
//! then coherence repair, then ordering. Repair runs **before** the ordering comparison so
//! that both sides of that comparison are repaired: the stored document is post-repair, and
//! comparing a raw incoming document against it would report a spurious difference and
//! refuse an identical republication as [`AdmissionRefusal::NotAdvanced`].

use serde::{Deserialize, Serialize};

use crate::adaptive::{
    ChangeSetId, CommandOutcome, ControlId, DocumentRevisions, EntryValidity, Grounding,
    IntentIncoherence, OperationId, ProducerDocument, ProducerEpoch, Reason, ReasonCode, Refusal,
    TargetRole, ValueLane, CONSUMER_SCHEMA_VERSION,
};
use crate::bus::PrefixedZoneId;

/// Stable aggregator key for one producer document.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
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
    if let Some(refusal) = first_illegal_transition(&incoming) {
        return Admission::Refused(refusal);
    }

    let (document, repairs) = repair_intent(incoming);

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
