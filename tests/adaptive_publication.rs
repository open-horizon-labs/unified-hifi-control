//! Consumer-expectation tests for adaptive producer publication (issue #324).
//!
//! Written before the implementation, per `AGENTS.md`. Each test states what a *consumer*
//! of the aggregator is entitled to assume, not what the implementation happens to do.
//!
//! The load-bearing expectation, from which most of the others follow: **a consumer cannot
//! obtain a producer document that did not come out of the admission gate.** Everything
//! about coherence, monotonicity and zone identity is therefore checkable as a property of
//! the store rather than of a code path.

use std::collections::BTreeMap;

use unified_hifi_control::adaptive::{
    admit_document_str, ApplyLane, ChangeSetId, CommandOutcome, ControlId, ControlValue,
    DocumentRevisions, EntryValidity, Grounding, IntentIncoherence, LaneValue, OperationId,
    OutcomeTransition, ProducerDocument, ProducerEpoch, Provenance, Reason, ReasonCode, TargetRole,
    Timestamp, TransportLane, ValueLane,
};
use unified_hifi_control::producers::{
    admit, Admission, AdmissionKind, AdmissionRefusal, LaneDefect, ProducerAggregator, ProducerKey,
    ProducerPresence,
};

// =============================================================================
// Fixtures and helpers
// =============================================================================

/// Load a canonical #323 fixture through the contract's own admission path.
///
/// Deliberately the canonical fixtures rather than bespoke test documents: the publication
/// path should be exercised by the contract's own worked examples, so a disagreement
/// between #323's idea of a valid document and #324's is a test failure rather than a
/// discovery made by #325.
fn fixture(name: &str) -> ProducerDocument {
    let path = format!(
        "{}/tests/fixtures/adaptive/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    match admit_document_str(&raw) {
        Ok(document) => document,
        Err(refusal) => panic!("canonical fixture {name} is not admissible: {refusal}"),
    }
}

fn pipeline() -> ProducerDocument {
    fixture("hqplayer_pipeline_v1")
}

fn staged() -> ProducerDocument {
    fixture("staged_intent_multi_surface")
}

fn degraded() -> ProducerDocument {
    fixture("hqplayer_degraded_lanes")
}

fn outcomes() -> ProducerDocument {
    fixture("command_outcomes")
}

fn ts(raw: &str) -> Timestamp {
    Timestamp::new(raw)
}

/// Admit with no predecessor.
fn admit_fresh(document: ProducerDocument) -> Admission {
    admit(None, document)
}

fn expect_admitted(admission: Admission) -> unified_hifi_control::producers::AdmittedDocument {
    match admission {
        Admission::Admitted(admitted) => admitted,
        Admission::Refused(refusal) => panic!("expected admission, got refusal: {refusal:?}"),
    }
}

fn expect_refused(admission: Admission) -> AdmissionRefusal {
    match admission {
        Admission::Refused(refusal) => refusal,
        Admission::Admitted(admitted) => panic!(
            "expected refusal, document was admitted at {:?}",
            admitted.document.revisions
        ),
    }
}

// =============================================================================
// Envelope admission: version, identity, ordering
// =============================================================================

mod envelope {
    use super::*;

    /// Fixtures the publication gate must admit.
    ///
    /// A superset of `adaptive_contract.rs`'s `fixtures::CANONICAL`: it adds
    /// `forward_compatible_additions`, which is a compatibility probe rather than a canonical
    /// worked example but must still pass the gate, because §8 forbids refusing a document
    /// from a newer minor wholesale.
    const ADMISSIBLE: &[&str] = &[
        "hqplayer_pipeline_v1",
        "hqplayer_degraded_lanes",
        "staged_intent_multi_surface",
        "command_outcomes",
        "control_removed_after_advance",
        "forward_compatible_additions",
    ];

    /// Fixtures that exist precisely to be refused, so they must not be asserted admissible.
    ///
    /// Refusal is asserted where the reason lives - `unsupported_major` is refused before
    /// parsing, in `adaptive_contract.rs`.
    const INADMISSIBLE: &[&str] = &["unsupported_major"];

    #[test]
    fn every_canonical_fixture_is_admissible_by_the_publication_gate() {
        // If the gate refuses a document #323 calls canonical, one of the two is wrong and
        // #325 would be the one to find out.
        for name in ADMISSIBLE {
            let document = fixture(name);
            let admission = admit_fresh(document);
            assert!(
                matches!(admission, Admission::Admitted(_)),
                "canonical fixture {name} was refused by the publication gate: {admission:?}"
            );
        }
    }

    #[test]
    fn every_fixture_on_disk_is_classified_admissible_or_inadmissible() {
        // The list above is a literal, and a literal drifts: `control_removed_after_advance`
        // was added to `fixtures::CANONICAL` while this list still omitted it, so the one
        // document demonstrating the post-repair `control_removed` state was never put
        // through the gate. Found by CodeRabbit on #363. Enumerating the directory turns
        // the next omission into a failure here instead of silent non-coverage - adding a
        // fixture now forces a decision about which side of the gate it belongs on.
        let mut unclassified = Vec::new();
        for entry in std::fs::read_dir("tests/fixtures/adaptive")
            .expect("the adaptive fixture directory must exist")
        {
            let path = entry.expect("readable directory entry").path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .expect("fixture file name must be UTF-8")
                .to_string();
            if !ADMISSIBLE.contains(&name.as_str()) && !INADMISSIBLE.contains(&name.as_str()) {
                unclassified.push(name);
            }
        }
        unclassified.sort();
        assert!(
            unclassified.is_empty(),
            "fixtures on disk that the publication gate neither admits nor refuses: \
             {unclassified:?}. Add each to ADMISSIBLE or INADMISSIBLE."
        );
    }

    #[test]
    fn a_document_stamped_with_an_unsupported_major_is_refused() {
        // The typed path must apply the same version policy as the JSON path: a document
        // can reach the gate as a typed value that never passed through admit_document.
        let mut document = pipeline();
        document.schema_version = unified_hifi_control::adaptive::SchemaVersion::new(2, 0);
        let refusal = expect_refused(admit_fresh(document));
        assert!(
            matches!(refusal, AdmissionRefusal::Contract(_)),
            "expected a contract refusal for major 2, got {refusal:?}"
        );
    }

    #[test]
    fn a_newer_minor_is_admitted_rather_than_refused() {
        let mut document = pipeline();
        document.schema_version = unified_hifi_control::adaptive::SchemaVersion::new(1, 9);
        expect_admitted(admit_fresh(document));
    }

    #[test]
    fn a_zone_id_without_a_known_prefix_is_refused_and_names_the_offender() {
        // #323 left this to #324 deliberately: PrefixedZoneId is #[serde(transparent)], so
        // deserialization never validates, and the prefix vocabulary lives in the
        // server-only bus module the contract layer may not name.
        let mut document = pipeline();
        document.target.zone_id = Some("1601bb42ed14351b99c2926214f6cbb80724".to_string());
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::ZoneIdNotPrefixed { zone_id } => {
                assert_eq!(zone_id, "1601bb42ed14351b99c2926214f6cbb80724");
            }
            other => panic!("expected ZoneIdNotPrefixed, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_prefix_is_refused_even_though_it_looks_prefixed() {
        let mut document = pipeline();
        document.target.zone_id = Some("unknown:abc123".to_string());
        assert!(matches!(
            expect_refused(admit_fresh(document)),
            AdmissionRefusal::ZoneIdNotPrefixed { .. }
        ));
    }

    #[test]
    fn an_absent_zone_id_is_fine_for_an_instance_scoped_producer() {
        let mut document = pipeline();
        document.target.role = TargetRole::Instance;
        document.target.zone_id = None;
        expect_admitted(admit_fresh(document));
    }

    #[test]
    fn a_regressing_state_revision_is_refused() {
        let previous = pipeline();
        let mut incoming = pipeline();
        incoming.revisions = DocumentRevisions::new(12, 4192);
        match expect_refused(admit(Some(&previous), incoming)) {
            AdmissionRefusal::RevisionRegressed {
                previous: p,
                incoming: i,
            } => {
                assert_eq!(p, DocumentRevisions::new(12, 4193));
                assert_eq!(i, DocumentRevisions::new(12, 4192));
            }
            other => panic!("expected RevisionRegressed, got {other:?}"),
        }
    }

    #[test]
    fn a_regressing_control_plane_revision_is_refused_even_when_state_advances() {
        let previous = pipeline();
        let mut incoming = pipeline();
        incoming.revisions = DocumentRevisions::new(11, 9999);
        assert!(matches!(
            expect_refused(admit(Some(&previous), incoming)),
            AdmissionRefusal::RevisionRegressed { .. }
        ));
    }

    #[test]
    fn a_regressing_epoch_is_refused_as_an_out_of_order_delivery() {
        let previous = pipeline();
        let mut incoming = pipeline();
        incoming.producer.epoch = ProducerEpoch(6);
        incoming.revisions = DocumentRevisions::new(99, 99999);
        match expect_refused(admit(Some(&previous), incoming)) {
            AdmissionRefusal::EpochRegressed {
                previous: p,
                incoming: i,
            } => {
                assert_eq!(p, ProducerEpoch(7));
                assert_eq!(i, ProducerEpoch(6));
            }
            other => panic!("expected EpochRegressed, got {other:?}"),
        }
    }

    #[test]
    fn a_higher_epoch_is_admitted_even_though_its_revisions_are_lower() {
        // A restart resets revisions. They are only comparable within one epoch, so the
        // monotonicity rule must not fire across an epoch boundary.
        let previous = pipeline();
        let mut incoming = pipeline();
        incoming.producer.epoch = ProducerEpoch(8);
        incoming.revisions = DocumentRevisions::new(1, 1);
        let admitted = expect_admitted(admit(Some(&previous), incoming));
        assert_eq!(admitted.kind, AdmissionKind::Fresh);
    }

    #[test]
    fn republishing_the_same_revision_with_different_content_is_refused() {
        let previous = pipeline();
        let mut incoming = pipeline();
        incoming.controls.truncate(3);
        match expect_refused(admit(Some(&previous), incoming)) {
            AdmissionRefusal::NotAdvanced { at } => {
                assert_eq!(at, DocumentRevisions::new(12, 4193));
            }
            other => panic!("expected NotAdvanced, got {other:?}"),
        }
    }

    #[test]
    fn republishing_the_same_revision_with_only_lane_health_changed_is_admitted() {
        // The alternative — demanding a state bump for a lane transition — invalidates
        // every open change set on every lane flap, because a change set is validated
        // against the state revision. Admitting a health-only refresh keeps drafts alive
        // and still updates the lane witness.
        let previous = pipeline();
        let mut incoming = pipeline();
        if let Some(lane) = incoming
            .lanes
            .iter_mut()
            .find(|l| l.lane == TransportLane::Persistent)
        {
            lane.state = unified_hifi_control::adaptive::LaneState::Disconnected;
            lane.last_error = Some(Reason::observed(ReasonCode::RequiresConnection));
        }
        let admitted = expect_admitted(admit(Some(&previous), incoming));
        assert_eq!(admitted.kind, AdmissionKind::HealthRefresh);
    }

    #[test]
    fn an_identical_republication_is_admitted_as_a_health_refresh_and_changes_nothing() {
        let previous = pipeline();
        let incoming = pipeline();
        let admitted = expect_admitted(admit(Some(&previous), incoming));
        assert_eq!(admitted.kind, AdmissionKind::HealthRefresh);
        assert_eq!(admitted.document, previous);
    }
}

// =============================================================================
// Invariant: LaneValue consistency (decision 3)
// =============================================================================

// =============================================================================
// Document invariants the contract publishes and nothing applied
//
// Each of these is a rule #323 states and provides a predicate for, on a path that never
// called it - the same defect class as `LaneValue::is_consistent`, which decision 3 of
// `.oh/adaptive-publication.md` calls out by name: *the contract published a rule and no code
// path applied it*. Envelope class, so the whole document is refused; the previous snapshot
// stands, and a refused document fails to advance a producer rather than blanking one.
// =============================================================================

mod document_invariants {
    use super::*;
    use unified_hifi_control::adaptive::{Availability, AvailabilityState};

    #[test]
    fn a_control_publishing_the_same_value_lane_twice_is_refused() {
        // One lane is one reading. Two `desired` lanes make "the staged value" ambiguous,
        // and because `effective_view` resolves a single lane, C1 coherence could be
        // satisfied by one of them and contradicted by the other in the same document.
        let mut document = pipeline();
        let control = document
            .controls
            .iter_mut()
            .find(|c| !c.values.is_empty())
            .expect("a canonical fixture has a control with a lane");
        let control_id = control.id.clone();
        let duplicate = control.values[0].clone();
        let duplicated_lane = duplicate.lane.clone();
        control.values.push(duplicate);

        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::DuplicateValueLane { control, lane } => {
                assert_eq!(control, control_id, "the refusal named the wrong control");
                assert_eq!(lane, duplicated_lane, "the refusal named the wrong lane");
            }
            other => panic!("expected DuplicateValueLane, got {other:?}"),
        }
    }

    #[test]
    fn a_document_publishing_health_for_the_same_transport_lane_twice_is_refused() {
        // `LaneWitness` is keyed by lane, so a duplicate is not redundant but
        // unrepresentable: folding it would silently keep whichever entry came last in a
        // `Vec`. `lane_witnesses_are_keyed_by_lane_without_duplicates` asserts the fixtures
        // respect this; this asserts the gate enforces it.
        let mut document = pipeline();
        let duplicate = document.lanes[0].clone();
        let duplicated_lane = duplicate.lane.clone();
        document.lanes.push(duplicate);

        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::DuplicateLaneHealth { lane } => {
                assert_eq!(lane, duplicated_lane, "the refusal named the wrong lane");
            }
            other => panic!("expected DuplicateLaneHealth, got {other:?}"),
        }
    }

    #[test]
    fn an_unavailable_control_that_does_not_say_why_is_refused() {
        // A consumer that can see "disabled" but not why cannot explain itself, and the user
        // cannot escape the state that caused it. The contract requires at least one reason
        // for every non-available state and publishes `is_well_formed` to check it.
        let mut document = pipeline();
        let control_id = document.controls[0].id.clone();
        document.controls[0].availability = Availability {
            state: AvailabilityState::Unavailable,
            reasons: Vec::new(),
        };

        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::AvailabilityNotWellFormed { control, state } => {
                assert_eq!(control, control_id);
                assert_eq!(state, AvailabilityState::Unavailable);
            }
            other => panic!("expected AvailabilityNotWellFormed, got {other:?}"),
        }
    }

    #[test]
    fn a_read_only_control_that_does_not_say_why_is_refused_too() {
        // Every recognized non-available state, not just `unavailable`.
        let mut document = pipeline();
        document.controls[0].availability = Availability {
            state: AvailabilityState::ReadOnly,
            reasons: Vec::new(),
        };
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::AvailabilityNotWellFormed { state, .. } => {
                assert_eq!(state, AvailabilityState::ReadOnly);
            }
            other => panic!("expected AvailabilityNotWellFormed, got {other:?}"),
        }
    }

    #[test]
    fn an_unrecognized_availability_state_with_a_reason_is_admitted() {
        // Forward compatibility means an unknown member stays *representable*: a v1.0 build
        // meeting a v1.4 availability state must keep the control, keep its observed value,
        // and pass the state through untouched. That is about the vocabulary, and this
        // document satisfies the structural rule regardless of whether we recognize the word.
        let mut document = pipeline();
        document.controls[0].availability = Availability {
            state: AvailabilityState::from("quantum_superposition"),
            reasons: vec![Reason::observed(ReasonCode::from("quantum_decoherence"))],
        };
        let admitted = expect_admitted(admit_fresh(document));
        assert_eq!(
            admitted.document.controls[0].availability.state,
            AvailabilityState::from("quantum_superposition"),
            "an unrecognized state must be passed through, not normalized away"
        );
    }

    #[test]
    fn an_unrecognized_availability_state_without_a_reason_is_refused() {
        // The structural invariant does not lapse just because the state's name is unknown.
        // `is_well_formed` is true for `available` and requires at least one reason for
        // *every* other state, including `Unrecognized` - and the reason it exists is that a
        // consumer which can see "not usable" but not why cannot explain itself to a user.
        // An unknown state is exactly the case where that explanation matters most: the
        // consumer cannot even fall back on knowing what the state means. Exempting unknown
        // members would make the invariant weakest precisely where it is needed.
        let mut document = pipeline();
        let control_id = document.controls[0].id.clone();
        document.controls[0].availability = Availability {
            state: AvailabilityState::from("quantum_superposition"),
            reasons: Vec::new(),
        };

        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::AvailabilityNotWellFormed { control, state } => {
                assert_eq!(control, control_id);
                assert_eq!(state, AvailabilityState::from("quantum_superposition"));
            }
            other => panic!("expected AvailabilityNotWellFormed, got {other:?}"),
        }
    }

    #[test]
    fn a_well_formed_unavailable_control_is_still_admitted() {
        // The guard must not refuse the legitimate shape it is policing.
        let mut document = pipeline();
        document.controls[0].availability =
            Availability::unavailable(Reason::observed(ReasonCode::NotApplicableInMode));
        expect_admitted(admit_fresh(document));
    }
}

mod lane_value_invariant {
    use super::*;

    /// Break the first lane of the first control that has one, returning what was broken.
    ///
    /// Returns the owning control id as well as the lane, so a refusal can be asserted to
    /// name both rather than only the lane.
    fn break_first_lane(
        document: &mut ProducerDocument,
        mutate: impl FnOnce(&mut LaneValue),
    ) -> (ControlId, ValueLane) {
        let control = document
            .controls
            .iter_mut()
            .find(|c| !c.values.is_empty())
            .expect("a canonical fixture has at least one control with a lane");
        let control_id = control.id.clone();
        let value = control
            .values
            .first_mut()
            .expect("checked non-empty just above");
        let lane = value.lane.clone();
        mutate(value);
        (control_id, lane)
    }

    fn first_control_lane(document: &mut ProducerDocument) -> &mut LaneValue {
        let control = document
            .controls
            .iter_mut()
            .find(|c| !c.values.is_empty())
            .expect("a canonical fixture has at least one control with a lane");
        control
            .values
            .first_mut()
            .expect("checked non-empty just above")
    }

    #[test]
    fn a_grounded_lane_with_no_value_is_refused_and_names_control_and_lane() {
        let mut document = pipeline();
        let (control_id, lane) = break_first_lane(&mut document, |value| {
            value.grounding = Grounding::Grounded;
            value.value = None;
        });
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::LaneValueInconsistent {
                control,
                lane: got,
                detail,
            } => {
                // Both halves are asserted: a refusal that names the wrong control is as
                // unactionable as one that names none. An earlier version of this test
                // carried a `None::<ControlId>` placeholder and matched the control with
                // `..`, so it proved nothing about it.
                assert_eq!(control, control_id, "the refusal named the wrong control");
                assert_eq!(got, lane, "the refusal named the wrong lane");
                assert_eq!(detail, LaneDefect::GroundedWithoutValue);
            }
            other => panic!("expected LaneValueInconsistent, got {other:?}"),
        }
    }

    #[test]
    fn an_ungrounded_lane_carrying_a_value_is_refused() {
        let mut document = pipeline();
        {
            let value = first_control_lane(&mut document);
            value.grounding = Grounding::Ungrounded;
            value.ungrounded_reason = Some(ReasonCode::Unobservable);
            value.value = Some(ControlValue::Bool(true));
        }
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::LaneValueInconsistent { detail, .. } => {
                assert_eq!(detail, LaneDefect::UngroundedWithValue);
            }
            other => panic!("expected LaneValueInconsistent, got {other:?}"),
        }
    }

    #[test]
    fn an_ungrounded_lane_without_a_reason_is_refused() {
        let mut document = pipeline();
        {
            let value = first_control_lane(&mut document);
            value.grounding = Grounding::Ungrounded;
            value.value = None;
            value.ungrounded_reason = None;
        }
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::LaneValueInconsistent { detail, .. } => {
                assert_eq!(detail, LaneDefect::UngroundedWithoutReason);
            }
            other => panic!("expected LaneValueInconsistent, got {other:?}"),
        }
    }

    #[test]
    fn an_unrecognized_grounding_from_a_newer_minor_is_admitted_not_refused() {
        // The whole point of the compatibility policy. `LaneValue::is_consistent` returns
        // false here — correctly, for a *consumer*, which must not treat the payload as
        // authoritative — but using that predicate at the door would refuse a
        // forward-compatible document wholesale, which §8 forbids.
        let mut document = pipeline();
        {
            let value = first_control_lane(&mut document);
            value.grounding = Grounding::Unrecognized("provisional".to_string());
        }
        expect_admitted(admit_fresh(document));
    }
}

// =============================================================================
// Invariant: deserialized operation-history legality (decision 4)
// =============================================================================

mod history_legality {
    use super::*;

    #[test]
    fn an_illegal_recorded_transition_refuses_the_document_and_names_it() {
        // `applied` is terminal, so nothing may follow it. Deserialization never checks
        // this, because `history` is a plain Vec.
        let mut document = outcomes();
        let operation = document
            .operations
            .first_mut()
            .expect("command_outcomes publishes operations");
        let id = operation.id.clone();
        operation.history.push(OutcomeTransition {
            from: CommandOutcome::Applied,
            to: CommandOutcome::Indeterminate,
            at: ts("2026-07-30T11:20:00Z"),
            observed: None,
            reason: None,
        });
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::IllegalOutcomeHistory {
                operation,
                from,
                to,
            } => {
                assert_eq!(operation, id);
                assert_eq!(from, CommandOutcome::Applied);
                assert_eq!(to, CommandOutcome::Indeterminate);
            }
            other => panic!("expected IllegalOutcomeHistory, got {other:?}"),
        }
    }

    #[test]
    fn a_transition_involving_an_unrecognized_outcome_is_admitted() {
        // `may_transition_to` permits these deliberately: refusing would drop audit history
        // a v1.0 consumer cannot read. The gate must not be stricter than the predicate.
        let mut document = outcomes();
        let operation = document
            .operations
            .first_mut()
            .expect("command_outcomes publishes operations");
        operation.history.push(OutcomeTransition {
            from: CommandOutcome::Applied,
            to: CommandOutcome::Unrecognized("quarantined".to_string()),
            at: ts("2026-07-30T11:20:00Z"),
            observed: None,
            reason: None,
        });
        expect_admitted(admit_fresh(document));
    }

    #[test]
    fn a_legal_recorded_transition_is_admitted() {
        let mut document = outcomes();
        let operation = document
            .operations
            .first_mut()
            .expect("command_outcomes publishes operations");
        operation.history.push(OutcomeTransition {
            from: CommandOutcome::Indeterminate,
            to: CommandOutcome::Applied,
            at: ts("2026-07-30T11:20:00Z"),
            observed: None,
            reason: None,
        });
        expect_admitted(admit_fresh(document));
    }
}

// =============================================================================
// Published-intent coherence: C1, C2 and the removed control
// =============================================================================

mod intent_coherence {
    use super::*;

    fn valid_entry_control(document: &ProducerDocument) -> (ChangeSetId, ControlId) {
        for change_set in &document.change_sets {
            for entry in &change_set.entries {
                if entry.validity == EntryValidity::Valid {
                    return (change_set.id.clone(), entry.control.clone());
                }
            }
        }
        panic!("staged_intent_multi_surface must publish a valid entry")
    }

    /// Find the `display_text_key` of the first reason in `value` carrying `code`, at any
    /// depth. Searching rather than indexing a fixed path keeps this test measuring the
    /// published key itself rather than the fixture's current nesting.
    fn find_key(value: &serde_json::Value, code: &str) -> Option<String> {
        match value {
            serde_json::Value::Object(map) => {
                if map.get("code").and_then(serde_json::Value::as_str) == Some(code) {
                    if let Some(key) = map
                        .get("display_text_key")
                        .and_then(serde_json::Value::as_str)
                    {
                        return Some(key.to_string());
                    }
                }
                map.values().find_map(|nested| find_key(nested, code))
            }
            serde_json::Value::Array(items) => {
                items.iter().find_map(|nested| find_key(nested, code))
            }
            _ => None,
        }
    }

    #[test]
    fn a_coherent_document_is_admitted_with_no_repairs() {
        let admitted = expect_admitted(admit_fresh(staged()));
        assert!(
            admitted.repairs.is_empty(),
            "canonical staged-intent fixture should need no repair, got {:?}",
            admitted.repairs
        );
    }

    #[test]
    fn no_admitted_document_ever_carries_an_intent_incoherence() {
        // The invariant a consumer relies on, stated over the store rather than over a
        // code path. Every mutation below is a distinct way to break C1 or C2.
        let mut broken = Vec::new();

        let mut missing_lane = staged();
        let (_, control) = valid_entry_control(&missing_lane);
        for descriptor in &mut missing_lane.controls {
            if descriptor.id == control {
                descriptor.values.retain(|v| v.lane != ValueLane::Desired);
            }
        }
        broken.push(missing_lane);

        let mut disagreeing = staged();
        let (_, control) = valid_entry_control(&disagreeing);
        for descriptor in &mut disagreeing.controls {
            if descriptor.id == control {
                for value in &mut descriptor.values {
                    if value.lane == ValueLane::Desired {
                        value.value = Some(ControlValue::choice("something-else"));
                    }
                }
            }
        }
        broken.push(disagreeing);

        let mut unknown_control = staged();
        let (_, control) = valid_entry_control(&unknown_control);
        unknown_control.controls.retain(|c| c.id != control);
        broken.push(unknown_control);

        // C2, which the earlier version of this test omitted: it was covered only by
        // `two_drafts_claiming_one_control_both_lose_validity`, which asserts the demoted
        // validity but never re-checks the document against the coherence rules. The
        // invariant has to hold for every way of breaking it, not for three of the four.
        let mut contested = staged();
        let (first_id, control) = valid_entry_control(&contested);
        let second_id = contested
            .change_sets
            .iter()
            .map(|cs| cs.id.clone())
            .find(|id| id != &first_id)
            .expect("the fixture publishes more than one change set");
        let template = contested
            .change_sets
            .iter()
            .find(|cs| cs.id == first_id)
            .and_then(|cs| cs.entries.iter().find(|e| e.control == control))
            .cloned()
            .expect("located above");
        for change_set in &mut contested.change_sets {
            if change_set.id == second_id {
                change_set.entries.retain(|e| e.control != control);
                change_set.entries.push(template.clone());
            }
        }
        broken.push(contested);

        for document in broken {
            let admitted = expect_admitted(admit_fresh(document));
            assert!(
                admitted.document.intent_coherence_violations().is_empty(),
                "an incoherent document reached a consumer: {:?}",
                admitted.document.intent_coherence_violations()
            );
            assert!(
                !admitted.repairs.is_empty(),
                "a repaired document must report what was repaired"
            );
        }
    }

    #[test]
    fn a_missing_desired_lane_demotes_to_requires_producer_validation_not_to_valid() {
        let mut document = staged();
        let (change_set, control) = valid_entry_control(&document);
        for descriptor in &mut document.controls {
            if descriptor.id == control {
                descriptor.values.retain(|v| v.lane != ValueLane::Desired);
            }
        }
        let admitted = expect_admitted(admit_fresh(document));
        let repair = admitted
            .repairs
            .iter()
            .find(|r| r.control == control && r.change_set == change_set)
            .expect("the offending entry must be reported");
        assert!(matches!(
            repair.violation,
            IntentIncoherence::MissingDesiredLane { .. }
        ));
        assert!(matches!(
            repair.to,
            EntryValidity::RequiresProducerValidation { .. }
        ));
    }

    #[test]
    fn the_c1_demotion_reason_code_is_draft_invalid_and_validity_state_does_not_dictate_it() {
        // Pins a decision, because the pairing reads like a mistake and was queried on #363:
        // the C1 demotion sets `EntryValidity::RequiresProducerValidation` while its reason
        // carries `ReasonCode::DraftInvalid`. Those are two independent axes - the state says
        // what a consumer may *do* (here: ask the producer, never apply), the code says
        // *why* - and the contract's own canonical worked example proves it, so this is not
        // a convention the gate is violating. Asserted from the fixture rather than
        // described in a comment, so the justification cannot go stale while the test passes.
        let staged_fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string("tests/fixtures/adaptive/staged_intent_multi_surface.json")
                .expect("the canonical staged-intent fixture must exist"),
        )
        .expect("the canonical fixture must be valid JSON");
        let crossed = crossed_validity_pairings(&staged_fixture);
        assert!(
            !crossed.is_empty(),
            "the canonical staged-intent fixture no longer pairs an entry validity state \
             with a differently-named reason code, so the independence this demotion relies \
             on is no longer demonstrated by the contract"
        );

        let mut document = staged();
        let (change_set, control) = valid_entry_control(&document);
        for descriptor in &mut document.controls {
            if descriptor.id == control {
                descriptor.values.retain(|v| v.lane != ValueLane::Desired);
            }
        }
        let admitted = expect_admitted(admit_fresh(document));
        let repair = admitted
            .repairs
            .iter()
            .find(|r| r.control == control && r.change_set == change_set)
            .expect("the offending entry must be reported");

        match &repair.to {
            EntryValidity::RequiresProducerValidation { reason } => {
                assert_eq!(
                    reason.code,
                    ReasonCode::DraftInvalid,
                    "a C1 demotion reports that the published draft is invalid; the crossed \
                     pairings the canonical fixture publishes are {crossed:?}"
                );
                assert_eq!(
                    reason.scope,
                    unified_hifi_control::adaptive::ReasonScope::Draft,
                    "a C1 violation is a property of the draft, not of the running engine"
                );
            }
            other => panic!("expected RequiresProducerValidation, got {other:?}"),
        }
    }

    /// Every `(validity state, reason code)` pair in `value` whose two names differ.
    ///
    /// Evidence that the contract treats the two as independent vocabularies rather than one
    /// state spelled twice.
    fn crossed_validity_pairings(value: &serde_json::Value) -> Vec<(String, String)> {
        let mut found = Vec::new();
        collect_crossed_pairings(value, &mut found);
        found
    }

    fn collect_crossed_pairings(value: &serde_json::Value, out: &mut Vec<(String, String)>) {
        match value {
            serde_json::Value::Object(map) => {
                if let Some(validity) = map.get("validity").and_then(serde_json::Value::as_object) {
                    let state = validity.get("state").and_then(serde_json::Value::as_str);
                    let code = validity
                        .get("reason")
                        .and_then(serde_json::Value::as_object)
                        .and_then(|reason| reason.get("code"))
                        .and_then(serde_json::Value::as_str);
                    if let (Some(state), Some(code)) = (state, code) {
                        if state != code {
                            out.push((state.to_string(), code.to_string()));
                        }
                    }
                }
                for nested in map.values() {
                    collect_crossed_pairings(nested, out);
                }
            }
            serde_json::Value::Array(items) => {
                for nested in items {
                    collect_crossed_pairings(nested, out);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn a_valid_entry_for_a_removed_control_is_distinctly_visible_as_control_removed() {
        // The issue is explicit: users must see "this control no longer exists", not the
        // generic "needs producer validation" that a fail-closed rule would produce.
        let mut document = staged();
        let (change_set, control) = valid_entry_control(&document);
        document.controls.retain(|c| c.id != control);
        document.revisions = DocumentRevisions::new(13, 4194);

        let admitted = expect_admitted(admit_fresh(document));
        let repair = admitted
            .repairs
            .iter()
            .find(|r| r.control == control && r.change_set == change_set)
            .expect("the entry targeting the removed control must be reported");
        assert!(matches!(
            repair.violation,
            IntentIncoherence::UnknownControl { .. }
        ));
        match &repair.to {
            EntryValidity::DraftInvalid { reason } => {
                assert_eq!(
                    reason.code,
                    ReasonCode::ControlRemoved,
                    "a removed control must be distinguishable from an unvalidated one"
                );
                // `detail` is documented as non-localised diagnostic text "for logs rather
                // than for users". Emitting only `detail` means the sole explanation a user
                // can be shown is untranslatable English prose - which is precisely the
                // failure `display_text_key` exists to prevent.
                assert_eq!(
                    reason.display_text_key.as_deref(),
                    Some("reason.control_no_longer_exists"),
                    "a removed control must carry the catalog key consumers render, not \
                     only log detail"
                );
            }
            other => panic!("expected DraftInvalid(control_removed), got {other:?}"),
        }
    }

    #[test]
    fn the_removed_control_key_matches_the_one_the_canonical_fixture_publishes() {
        // Two sources claim to define this key: production and the worked example that
        // `every_vocabulary_member_has_a_worked_example` forces to exist. If they drift, a
        // consumer built against the fixture renders a blank string against the real thing.
        let fixture: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string("tests/fixtures/adaptive/control_removed_after_advance.json")
                .expect("the canonical control-removed fixture must exist"),
        )
        .expect("the canonical fixture must be valid JSON");

        let published = find_key(&fixture, "control_removed")
            .expect("the fixture must carry a control_removed reason with a display_text_key");

        let mut document = staged();
        let (_, control) = valid_entry_control(&document);
        document.controls.retain(|c| c.id != control);
        document.revisions = DocumentRevisions::new(13, 4194);
        let admitted = expect_admitted(admit_fresh(document));
        let produced = admitted
            .repairs
            .iter()
            .find_map(|repair| match &repair.to {
                EntryValidity::DraftInvalid { reason }
                    if reason.code == ReasonCode::ControlRemoved =>
                {
                    reason.display_text_key.clone()
                }
                _ => None,
            })
            .expect("production must emit a display_text_key for a removed control");

        assert_eq!(
            produced, published,
            "production and the canonical fixture disagree on the control-removed catalog key"
        );
    }

    #[test]
    fn two_drafts_claiming_one_control_both_lose_validity() {
        // The aggregator cannot know which draft the user meant, and keeping one valid
        // would authorize an apply nobody confirmed. "At most one valid" is satisfied by
        // zero, and zero is the only order-independent answer.
        let mut document = staged();
        let (first_id, control) = valid_entry_control(&document);
        let second_id = document
            .change_sets
            .iter()
            .map(|cs| cs.id.clone())
            .find(|id| id != &first_id)
            .expect("the fixture publishes more than one change set");

        let template = document
            .change_sets
            .iter()
            .find(|cs| cs.id == first_id)
            .and_then(|cs| cs.entries.iter().find(|e| e.control == control))
            .cloned()
            .expect("located above");

        for change_set in &mut document.change_sets {
            if change_set.id == second_id {
                change_set.entries.retain(|e| e.control != control);
                change_set.entries.push(template.clone());
            }
        }

        let admitted = expect_admitted(admit_fresh(document));
        for id in [&first_id, &second_id] {
            let change_set = admitted
                .document
                .change_sets
                .iter()
                .find(|cs| &cs.id == id)
                .expect("change set survives repair");
            let entry = change_set
                .entries
                .iter()
                .find(|e| e.control == control)
                .expect("entry survives repair");
            match &entry.validity {
                EntryValidity::Conflicts { with } => assert!(with.contains(&control)),
                other => panic!("expected Conflicts naming the control, got {other:?}"),
            }
        }
    }

    #[test]
    fn a_declared_last_actor_takeover_policy_does_not_change_publication_time_repair() {
        // `draft_policy.on_conflict` governs *staging* — what happens when a new edit
        // arrives for a contested control. #324 implements no staging API, so the gate
        // never reaches that decision. Resolving an already-published C2 violation by
        // `last_actor_takeover` would require inventing an arrival order the document does
        // not carry.
        let mut document = staged();
        if let Some(policy) = document.draft_policy.as_mut() {
            policy.on_conflict = unified_hifi_control::adaptive::ConflictPolicy::LastActorTakeover;
        }
        let (first_id, control) = valid_entry_control(&document);
        let second_id = document
            .change_sets
            .iter()
            .map(|cs| cs.id.clone())
            .find(|id| id != &first_id)
            .expect("more than one change set");
        let template = document
            .change_sets
            .iter()
            .find(|cs| cs.id == first_id)
            .and_then(|cs| cs.entries.iter().find(|e| e.control == control))
            .cloned()
            .expect("located above");
        for change_set in &mut document.change_sets {
            if change_set.id == second_id {
                change_set.entries.retain(|e| e.control != control);
                change_set.entries.push(template.clone());
            }
        }

        let admitted = expect_admitted(admit_fresh(document));
        assert_eq!(
            admitted.repairs.len(),
            2,
            "both claimants demote regardless of the declared conflict policy"
        );
    }

    #[test]
    fn one_draft_holding_one_control_on_two_apply_lanes_is_left_alone() {
        // Regression cover for the C2 fix in `fdc1ca4`: C2 constrains drafts, not entries.
        // A producer mirroring one write to the running engine and to persisted
        // configuration is coherent — one desired lane describes both entries.
        let mut document = staged();
        let (change_set_id, control) = valid_entry_control(&document);
        let mut mirrored = document
            .change_sets
            .iter()
            .find(|cs| cs.id == change_set_id)
            .and_then(|cs| cs.entries.iter().find(|e| e.control == control))
            .cloned()
            .expect("located above");
        mirrored.lane = ApplyLane::Persistent;
        for change_set in &mut document.change_sets {
            if change_set.id == change_set_id {
                change_set.entries.push(mirrored.clone());
            }
        }

        let admitted = expect_admitted(admit_fresh(document));
        assert!(
            admitted.repairs.is_empty(),
            "one draft on two apply lanes is coherent, but was repaired: {:?}",
            admitted.repairs
        );
    }

    #[test]
    fn repair_changes_validity_and_nothing_else() {
        // "Never fabricate desired intent", made checkable. Demotion may only lower
        // validity: it may not add a lane, edit a value, or touch a control.
        let mut document = staged();
        let (_, control) = valid_entry_control(&document);
        for descriptor in &mut document.controls {
            if descriptor.id == control {
                descriptor.values.retain(|v| v.lane != ValueLane::Desired);
            }
        }
        let published = document.clone();
        let admitted = expect_admitted(admit_fresh(document));
        let repaired = &admitted.document;

        assert_eq!(
            repaired.controls, published.controls,
            "controls were touched"
        );
        assert_eq!(repaired.lanes, published.lanes, "lane health was touched");
        assert_eq!(
            repaired.constraints, published.constraints,
            "constraints were touched"
        );
        assert_eq!(
            repaired.operations, published.operations,
            "operations were touched"
        );
        assert_eq!(
            repaired.revisions, published.revisions,
            "revisions were touched"
        );

        for (after, before) in repaired
            .change_sets
            .iter()
            .zip(published.change_sets.iter())
        {
            assert_eq!(after.id, before.id);
            assert_eq!(
                after.generation, before.generation,
                "generation was touched"
            );
            assert_eq!(
                after.updated_at, before.updated_at,
                "updated_at was touched"
            );
            assert_eq!(after.state, before.state, "change-set state was touched");
            for (a, b) in after.entries.iter().zip(before.entries.iter()) {
                assert_eq!(a.control, b.control);
                assert_eq!(a.lane, b.lane);
                assert_eq!(a.desired, b.desired, "a staged value was rewritten");
                assert_eq!(a.base_observed, b.base_observed);
            }
        }
    }
}

// =============================================================================
// Aggregator ownership: keying, atomicity, lifecycle
// =============================================================================

mod aggregator_state {
    use super::*;

    fn aggregator() -> ProducerAggregator {
        ProducerAggregator::detached()
    }

    #[tokio::test]
    async fn a_published_document_becomes_a_snapshot_keyed_by_producer_and_role() {
        let aggregator = aggregator();
        let document = pipeline();
        let key = ProducerKey::of(&document);
        aggregator.ingest(document).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert_eq!(snapshot.key.producer_id, "hqplayer:living-room");
        assert_eq!(snapshot.key.role, TargetRole::DspEngine);
        assert_eq!(
            snapshot.key.zone_id.as_deref(),
            Some("roon:1601bb42ed14351b99c2926214f6cbb80724")
        );
    }

    #[tokio::test]
    async fn producers_with_the_same_id_but_different_roles_are_separate_entries() {
        let aggregator = aggregator();
        let mut instance = pipeline();
        instance.target.role = TargetRole::Instance;
        instance.target.zone_id = None;

        aggregator.ingest(pipeline()).await;
        aggregator.ingest(instance).await;
        assert_eq!(aggregator.producer_count().await, 2);
    }

    #[tokio::test]
    async fn multiple_producers_are_held_independently() {
        let aggregator = aggregator();
        for (id, zone) in [
            ("hqplayer:living-room", "roon:aaa"),
            ("hqplayer:study", "roon:bbb"),
            ("hqplayer:kitchen", "lms:ccc"),
        ] {
            let mut document = pipeline();
            document.producer.producer_id = id.to_string();
            document.target.zone_id = Some(zone.to_string());
            aggregator.ingest(document).await;
        }
        assert_eq!(aggregator.producer_count().await, 3);
        assert_eq!(aggregator.snapshots().await.len(), 3);
    }

    #[tokio::test]
    async fn a_snapshot_is_one_whole_published_document_never_a_mixture() {
        // The atomicity guarantee. A mode read at revision 4193 and an enumeration read at
        // 4195 can describe a combination that never existed.
        let aggregator = aggregator();
        let key = ProducerKey::of(&pipeline());

        for state in [4193_u64, 4194, 4195, 4196] {
            let mut document = pipeline();
            document.revisions = DocumentRevisions::new(12, state);
            document.producer.instance_label = Some(format!("label-{state}"));
            aggregator.ingest(document).await;

            let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
            assert_eq!(snapshot.document.revisions.state.0, state);
            assert_eq!(
                snapshot.document.producer.instance_label.as_deref(),
                Some(format!("label-{state}").as_str()),
                "snapshot mixes fields from different revisions"
            );
        }
    }

    #[tokio::test]
    async fn a_refused_document_leaves_the_previous_snapshot_intact() {
        // Why refusal is a safe policy for envelope violations: it fails to advance a
        // producer rather than blanking one.
        let aggregator = aggregator();
        let key = ProducerKey::of(&pipeline());
        aggregator.ingest(pipeline()).await;

        let mut stale = pipeline();
        stale.revisions = DocumentRevisions::new(12, 4100);
        stale.producer.instance_label = Some("should-not-appear".to_string());
        aggregator.ingest(stale).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot retained");
        assert_eq!(snapshot.document.revisions.state.0, 4193);
        assert_eq!(
            snapshot.document.producer.instance_label.as_deref(),
            Some("HQP-Main")
        );
    }

    #[tokio::test]
    async fn removal_deletes_every_target_of_that_producer() {
        let aggregator = aggregator();
        let mut instance = pipeline();
        instance.target.role = TargetRole::Instance;
        instance.target.zone_id = None;
        aggregator.ingest(pipeline()).await;
        aggregator.ingest(instance).await;
        assert_eq!(aggregator.producer_count().await, 2);

        let removed = aggregator.remove("hqplayer:living-room").await;
        assert_eq!(removed, 2);
        assert_eq!(aggregator.producer_count().await, 0);
    }

    #[tokio::test]
    async fn removing_an_unknown_producer_removes_nothing_and_does_not_error() {
        let aggregator = aggregator();
        aggregator.ingest(pipeline()).await;
        assert_eq!(aggregator.remove("hqplayer:nowhere").await, 0);
        assert_eq!(aggregator.producer_count().await, 1);
    }
}

// =============================================================================
// Restart, reconnect, and last-known state
// =============================================================================

mod witness_monotonicity {
    use super::*;

    /// Republish `pipeline()` at an advanced revision with the native lane's `last_success`
    /// set to `instant`, and return the witnessed timestamp afterwards.
    async fn witness_after_native_success(
        first: &str,
        second: &str,
        second_epoch: ProducerEpoch,
    ) -> (Option<String>, ProducerEpoch) {
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());

        let mut opening = pipeline();
        set_native_success(&mut opening, first);
        aggregator.ingest(opening).await;

        let mut later = pipeline();
        later.producer.epoch = second_epoch;
        later.revisions = DocumentRevisions::new(12, 4194);
        set_native_success(&mut later, second);
        aggregator.ingest(later).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        let witness = snapshot
            .lanes
            .get(&TransportLane::Native)
            .expect("native lane witnessed");
        (
            witness
                .last_success
                .as_ref()
                .map(|t| t.as_str().to_string()),
            witness.observed_in_epoch,
        )
    }

    fn set_native_success(document: &mut ProducerDocument, instant: &str) {
        for lane in &mut document.lanes {
            if lane.lane == TransportLane::Native {
                lane.last_success = Some(Timestamp::new(instant));
            }
        }
    }

    #[tokio::test]
    async fn a_newer_document_carrying_an_older_instant_does_not_regress_the_witness() {
        // `LaneWitness::last_success` is documented as "the newest success ever seen for this
        // lane. Monotonic: never regresses, so a failing poll cannot erase the evidence that
        // the lane worked five seconds ago." Taking the producer's latest word unconditionally
        // breaks that whenever a document reports an *older* instant than one already seen -
        // which a producer with more than one polling path does routinely.
        let (witnessed, _) = witness_after_native_success(
            "2026-07-30T11:18:04Z",
            "2026-07-30T11:17:00Z",
            ProducerEpoch(7),
        )
        .await;
        assert_eq!(
            witnessed.as_deref(),
            Some("2026-07-30T11:18:04Z"),
            "the witness regressed to an older instant"
        );
    }

    #[tokio::test]
    async fn an_equivalent_instant_written_with_an_offset_is_not_a_regression() {
        // `2026-07-30T13:18:04+02:00` *is* `2026-07-30T11:18:04Z`. A comparison that is not
        // instant-aware cannot see that: a `+00:00` offset does not compare with a `Z` at all
        // under string ordering. Either spelling is a correct answer here because they denote
        // one instant; what must not happen is regressing to something genuinely earlier.
        let (witnessed, _) = witness_after_native_success(
            "2026-07-30T11:18:04Z",
            "2026-07-30T13:18:04+02:00",
            ProducerEpoch(7),
        )
        .await;
        let witnessed = witnessed.expect("a success was witnessed");
        let parsed = chrono::DateTime::parse_from_rfc3339(&witnessed)
            .expect("the witnessed timestamp must be RFC 3339");
        let expected = chrono::DateTime::parse_from_rfc3339("2026-07-30T11:18:04Z").unwrap();
        assert_eq!(
            parsed, expected,
            "an equivalent instant written with an offset moved the witness"
        );
    }

    #[tokio::test]
    async fn fractional_seconds_advance_the_witness_despite_sorting_before_z() {
        // The trap the previous lexical guard fell into, kept as a test: `…:04.500Z` sorts
        // *before* `…:04Z` because `.` is 0x2E and `Z` is 0x5A, yet it is 500ms later.
        let (witnessed, _) = witness_after_native_success(
            "2026-07-30T11:18:04Z",
            "2026-07-30T11:18:04.500Z",
            ProducerEpoch(7),
        )
        .await;
        assert_eq!(
            witnessed.as_deref(),
            Some("2026-07-30T11:18:04.500Z"),
            "a genuinely later instant was rejected because it sorts earlier as a string"
        );
    }

    #[tokio::test]
    async fn an_epoch_advance_carrying_an_older_instant_keeps_the_newer_one_and_its_epoch() {
        // `observed_in_epoch` exists so a consumer can tell that a carried-over timestamp
        // predates a restart. It must therefore name the epoch the *retained* instant was
        // observed in - not the epoch of whichever document happened to arrive last, which
        // would claim the new process observed something it never did.
        let (witnessed, epoch) = witness_after_native_success(
            "2026-07-30T11:18:04Z",
            "2026-07-30T11:17:00Z",
            ProducerEpoch(8),
        )
        .await;
        assert_eq!(
            witnessed.as_deref(),
            Some("2026-07-30T11:18:04Z"),
            "a restart regressed the newest-ever success"
        );
        assert_eq!(
            epoch,
            ProducerEpoch(7),
            "observed_in_epoch must name the epoch the retained instant was observed in"
        );
    }

    #[tokio::test]
    async fn a_genuinely_newer_instant_after_a_restart_advances_both_fields() {
        let (witnessed, epoch) = witness_after_native_success(
            "2026-07-30T11:18:04Z",
            "2026-07-30T11:19:00Z",
            ProducerEpoch(8),
        )
        .await;
        assert_eq!(witnessed.as_deref(), Some("2026-07-30T11:19:00Z"));
        assert_eq!(
            epoch,
            ProducerEpoch(8),
            "a newly observed success belongs to the epoch that observed it"
        );
    }

    #[tokio::test]
    async fn a_malformed_instant_in_a_later_document_leaves_the_old_witness_standing() {
        // The document is refused outright, so it never reaches the witness at all - and
        // because a refusal fails to advance a producer rather than blanking one, the
        // known-good instant and the snapshot it belongs to both stand.
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());

        let mut opening = pipeline();
        set_native_success(&mut opening, "2026-07-30T11:18:04Z");
        aggregator.ingest(opening).await;

        let mut malformed = pipeline();
        malformed.revisions = DocumentRevisions::new(12, 4194);
        set_native_success(&mut malformed, "not-a-timestamp");
        let admission = aggregator.ingest(malformed).await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a malformed RFC 3339 timestamp was admitted: {admission:?}"
        );

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert_eq!(
            snapshot.document.revisions.state.0, 4193,
            "the refused document advanced the producer"
        );
        let witness = snapshot
            .lanes
            .get(&TransportLane::Native)
            .expect("native lane witnessed");
        assert_eq!(
            witness.last_success.as_ref().map(Timestamp::as_str),
            Some("2026-07-30T11:18:04Z"),
            "a refused document disturbed the witness"
        );
    }
}

// =============================================================================
// Timestamps are RFC 3339, and the gate says so
// =============================================================================

mod timestamp_validity {
    use super::*;

    fn with_native_success(instant: &str) -> ProducerDocument {
        let mut document = pipeline();
        for lane in &mut document.lanes {
            if lane.lane == TransportLane::Native {
                lane.last_success = Some(Timestamp::new(instant));
            }
        }
        document
    }

    #[test]
    fn a_first_document_carrying_a_malformed_instant_is_refused() {
        // `Timestamp`'s own documentation is "Wrap an RFC 3339 string", and every consumer
        // that does anything with a lane's freshness has to parse it. A value that cannot be
        // parsed is not a degraded reading, it is not a reading - and retaining it would put
        // a string no consumer can interpret where one it can is expected. There is no
        // predecessor here, so refusing is the only option that does not publish garbage.
        match expect_refused(admit_fresh(with_native_success("not-a-timestamp"))) {
            AdmissionRefusal::LaneTimestampNotRfc3339 { lane, value } => {
                assert_eq!(lane, TransportLane::Native);
                assert_eq!(value, "not-a-timestamp");
            }
            other => panic!("expected LaneTimestampNotRfc3339, got {other:?}"),
        }
    }

    #[test]
    fn an_advanced_document_carrying_a_malformed_instant_is_refused() {
        let held = with_native_success("2026-07-30T11:18:04Z");
        let mut advanced = with_native_success("2026-07-30T11-18-04Z");
        advanced.revisions = DocumentRevisions::new(12, 4194);
        match expect_refused(admit(Some(&held), advanced)) {
            AdmissionRefusal::LaneTimestampNotRfc3339 { value, .. } => {
                assert_eq!(value, "2026-07-30T11-18-04Z");
            }
            other => panic!("expected LaneTimestampNotRfc3339, got {other:?}"),
        }
    }

    #[test]
    fn every_malformed_spelling_is_refused() {
        for malformed in [
            "",
            "not-a-timestamp",
            "2026-07-30",
            "2026-07-30T11:18:04",
            "2026-13-30T11:18:04Z",
            "2026-07-30T25:18:04Z",
            "2026-07-30T11:18:04+25:00",
            "1753977484",
        ] {
            let admission = admit_fresh(with_native_success(malformed));
            assert!(
                matches!(
                    admission,
                    Admission::Refused(AdmissionRefusal::LaneTimestampNotRfc3339 { .. })
                ),
                "{malformed:?} was not refused as a malformed RFC 3339 instant: {admission:?}"
            );
        }
    }

    #[test]
    fn every_valid_rfc_3339_spelling_is_admitted() {
        // The guard must not refuse spellings the format allows. Offsets, fractional seconds,
        // a lower-case designator and - per the NOTE in RFC 3339 §5.6, which permits replacing
        // `T` with a space by agreement - a space separator are all accepted by `chrono`, and
        // `chrono` is the parser both this gate and the witness use. Pinning them here means a
        // future swap of parser cannot silently narrow what producers may publish.
        for valid in [
            "2026-07-30T11:18:04Z",
            "2026-07-30T11:18:04.5Z",
            "2026-07-30T11:18:04.123456789Z",
            "2026-07-30T13:18:04+02:00",
            "2026-07-30T11:18:04-00:00",
            "2026-07-30t11:18:04z",
            "2026-07-30 11:18:04Z",
        ] {
            let admission = admit_fresh(with_native_success(valid));
            assert!(
                matches!(admission, Admission::Admitted(_)),
                "{valid:?} is valid RFC 3339 but was refused: {admission:?}"
            );
        }
    }

    #[test]
    fn a_lane_with_no_success_timestamp_is_still_admitted() {
        // Absence is not malformation: a lane that has never succeeded says so by omitting
        // the field, and that is the shape a failing poll publishes.
        let mut document = pipeline();
        for lane in &mut document.lanes {
            lane.last_success = None;
        }
        expect_admitted(admit_fresh(document));
    }

    #[test]
    fn the_refusal_names_the_offending_lane_rather_than_the_first_one() {
        // A refusal that names the wrong lane sends a producer author to the wrong place.
        let mut document = pipeline();
        let mut named = None;
        for lane in &mut document.lanes {
            if lane.lane == TransportLane::Persistent {
                lane.last_success = Some(Timestamp::new("nope"));
                named = Some(lane.lane.clone());
            }
        }
        let expected = named.expect("the fixture publishes a persistent lane");
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::LaneTimestampNotRfc3339 { lane, .. } => {
                assert_eq!(lane, expected, "the refusal named the wrong lane");
            }
            other => panic!("expected LaneTimestampNotRfc3339, got {other:?}"),
        }
    }
}

mod restart_and_reconnect {
    use super::*;

    #[tokio::test]
    async fn a_restart_replaces_state_rather_than_merging_across_the_epoch() {
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());
        aggregator.ingest(pipeline()).await;

        let mut restarted = pipeline();
        restarted.producer.epoch = ProducerEpoch(8);
        restarted.revisions = DocumentRevisions::new(1, 1);
        restarted.controls.truncate(2);
        aggregator.ingest(restarted).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert_eq!(snapshot.document.producer.epoch, ProducerEpoch(8));
        assert_eq!(
            snapshot.document.controls.len(),
            2,
            "controls from the previous epoch survived a restart"
        );
    }

    #[tokio::test]
    async fn a_disconnected_producer_is_last_known_rather_than_removed() {
        let aggregator = ProducerAggregator::detached();
        let mut document = degraded();
        document.stale = true;
        let key = ProducerKey::of(&document);
        aggregator.ingest(document).await;

        let snapshot = aggregator.snapshot(&key).await.expect("still present");
        assert_eq!(snapshot.presence, ProducerPresence::LastKnown);
        assert!(
            !snapshot.document.controls.is_empty(),
            "last-known controls must still be visible, marked stale"
        );
    }

    #[tokio::test]
    async fn a_lane_that_fails_keeps_its_last_good_timestamp_in_the_witness() {
        // "Retain last-good per-lane snapshots with explicit stale/error timestamps
        // instead of blanking the whole producer when one poll fails."
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());
        aggregator.ingest(pipeline()).await;

        let mut broken = pipeline();
        broken.revisions = DocumentRevisions::new(12, 4194);
        for lane in &mut broken.lanes {
            if lane.lane == TransportLane::Persistent {
                lane.state = unified_hifi_control::adaptive::LaneState::Disconnected;
                lane.last_success = None;
                lane.last_error = Some(Reason::observed(ReasonCode::RequiresConnection));
            }
        }
        aggregator.ingest(broken).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        let witness = snapshot
            .lanes
            .get(&TransportLane::Persistent)
            .expect("persistent lane witnessed");
        assert_eq!(
            witness.last_success.as_ref().map(Timestamp::as_str),
            Some("2026-07-30T10:58:02Z"),
            "the aggregator dropped the last-good timestamp when the lane failed"
        );
        assert!(
            witness.last_error.is_some(),
            "the error must be visible too"
        );
        assert_eq!(
            witness.state,
            unified_hifi_control::adaptive::LaneState::Disconnected
        );
    }

    #[tokio::test]
    async fn one_failing_lane_does_not_blank_the_other_lanes() {
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());
        aggregator.ingest(pipeline()).await;

        let mut broken = pipeline();
        broken.revisions = DocumentRevisions::new(12, 4194);
        for lane in &mut broken.lanes {
            if lane.lane == TransportLane::Persistent {
                lane.state = unified_hifi_control::adaptive::LaneState::Disconnected;
            }
        }
        aggregator.ingest(broken).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        let native = snapshot
            .lanes
            .get(&TransportLane::Native)
            .expect("native lane witnessed");
        assert_eq!(
            native.state,
            unified_hifi_control::adaptive::LaneState::Connected
        );
    }

    #[tokio::test]
    async fn a_lane_witness_records_the_epoch_it_was_observed_in() {
        // Carrying a last-good timestamp across a restart is honest only if a consumer can
        // tell that it predates the restart.
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());
        aggregator.ingest(pipeline()).await;

        let mut restarted = pipeline();
        restarted.producer.epoch = ProducerEpoch(8);
        restarted.revisions = DocumentRevisions::new(1, 1);
        for lane in &mut restarted.lanes {
            if lane.lane == TransportLane::Persistent {
                lane.state = unified_hifi_control::adaptive::LaneState::Disconnected;
                lane.last_success = None;
            }
        }
        aggregator.ingest(restarted).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        let witness = snapshot
            .lanes
            .get(&TransportLane::Persistent)
            .expect("persistent lane witnessed");
        assert_eq!(witness.observed_in_epoch, ProducerEpoch(7));
    }
}

// =============================================================================
// Isolation of staged intent from live commands
// =============================================================================

mod live_command_isolation {
    use super::*;
    use unified_hifi_control::bus::{BusEvent, Command, PrefixedZoneId};

    #[tokio::test]
    async fn live_command_traffic_cannot_touch_staged_intent() {
        // "Immediate/live commands cannot read, clear, or flush persistent staged intent."
        // Structural: the aggregator mutates producer state only on adaptive ingress, so
        // every other event on the public bus is inert by construction.
        let aggregator = ProducerAggregator::detached();

        let document = staged();
        let key = ProducerKey::of(&document);
        aggregator.ingest(document).await;
        let before = aggregator.snapshot(&key).await.expect("snapshot present");

        for event in [
            BusEvent::CommandReceived {
                zone_id: "roon:1601bb42ed14351b99c2926214f6cbb80724".to_string(),
                command: Command::Pause,
                request_id: Some("req-1".to_string()),
            },
            BusEvent::VolumeChanged {
                output_id: "roon:out-1".to_string(),
                value: 40.0,
                is_muted: false,
            },
            BusEvent::HqpPipelineChanged {
                host: "living-room".to_string(),
                filter: Some("poly-sinc-gauss-long".to_string()),
                shaper: Some("ASDM7EC".to_string()),
                rate: Some("dsd256".to_string()),
            },
            BusEvent::ControlCommand {
                zone_id: "roon:1601bb42ed14351b99c2926214f6cbb80724".to_string(),
                action: "pause".to_string(),
                value: None,
            },
            BusEvent::SeekPositionChanged {
                zone_id: PrefixedZoneId::roon("1601bb42ed14351b99c2926214f6cbb80724"),
                position: 42,
            },
        ] {
            aggregator.apply_bus_event(event).await;
        }

        let after = aggregator.snapshot(&key).await.expect("snapshot present");
        assert_eq!(
            after.document.change_sets, before.document.change_sets,
            "live command traffic mutated staged intent"
        );
        assert_eq!(after.document, before.document);
    }

    #[tokio::test]
    async fn an_adapter_stopping_is_informational_and_flushes_no_producer() {
        let aggregator = ProducerAggregator::detached();

        aggregator.ingest(pipeline()).await;
        let mut other = pipeline();
        other.producer.producer_id = "roon:core".to_string();
        other.producer.producer_type = "roon".to_string();
        aggregator.ingest(other).await;
        assert_eq!(aggregator.producer_count().await, 2);

        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: Some("test".to_string()),
            })
            .await;

        assert_eq!(aggregator.producer_count().await, 2);
    }
}

// =============================================================================
// Forward compatibility through the publication path
// =============================================================================

mod forward_compatibility {
    use super::*;

    #[tokio::test]
    async fn unknown_additive_fields_survive_publication() {
        let aggregator = ProducerAggregator::detached();
        let document = fixture("forward_compatible_additions");
        let published_paths = document.extension_key_paths();
        assert!(
            !published_paths.is_empty(),
            "the forward-compatibility fixture must actually carry unknown fields"
        );
        let key = ProducerKey::of(&document);
        aggregator.ingest(document).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert_eq!(
            snapshot.document.extension_key_paths(),
            published_paths,
            "the publication path amputated a newer document"
        );
    }
}

// =============================================================================
// A poll loop, and what a consumer sees during one
// =============================================================================

mod poll_loop {
    use super::*;

    #[tokio::test]
    async fn a_sequence_of_polls_always_serves_exactly_one_published_document() {
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());
        let mut published: Vec<ProducerDocument> = Vec::new();

        for step in 0..12_u64 {
            let mut document = pipeline();
            document.revisions = DocumentRevisions::new(12, 4193 + step);
            document.producer.instance_label = Some(format!("poll-{step}"));
            published.push(document.clone());
            aggregator.ingest(document).await;

            let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
            assert!(
                published.contains(snapshot.document.as_ref()),
                "the served snapshot is not any document that was published"
            );
        }

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert_eq!(
            snapshot.document.as_ref(),
            published.last().expect("published twelve"),
            "the last published document is not the one being served"
        );
    }

    #[tokio::test]
    async fn out_of_order_arrivals_never_resurrect_a_removed_control() {
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());

        let mut advanced = pipeline();
        advanced.revisions = DocumentRevisions::new(13, 4200);
        let removed: ControlId = advanced
            .controls
            .last()
            .map(|c| c.id.clone())
            .expect("controls present");
        advanced.controls.retain(|c| c.id != removed);
        aggregator.ingest(advanced).await;

        // A delayed delivery from before the control-plane advance.
        aggregator.ingest(pipeline()).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert!(
            snapshot.document.control(&removed).is_none(),
            "a late delivery resurrected control {removed}"
        );
        assert_eq!(snapshot.document.revisions.control_plane.0, 13);
    }
}

// =============================================================================
// Refusals are observable
// =============================================================================

mod refusal_observability {
    use super::*;

    #[tokio::test]
    async fn a_refusal_is_retained_and_queryable_rather_than_only_logged() {
        // Option E would have returned this synchronously to the publishing adapter. With a
        // bus it has to be recorded, or a producer author has no way to learn that its
        // documents are being dropped.
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());
        aggregator.ingest(pipeline()).await;

        let mut stale = pipeline();
        stale.revisions = DocumentRevisions::new(12, 4000);
        aggregator.ingest(stale).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert!(
            matches!(
                snapshot.last_refusal,
                Some(AdmissionRefusal::RevisionRegressed { .. })
            ),
            "the refusal was not retained: {:?}",
            snapshot.last_refusal
        );
        assert_eq!(aggregator.refusals().await.len(), 1);
    }

    #[tokio::test]
    async fn a_first_document_that_is_refused_is_still_recorded() {
        // There is no entry to hang the refusal on, and that is exactly the case where a
        // silent drop would be least diagnosable.
        let aggregator = ProducerAggregator::detached();
        let mut document = pipeline();
        document.target.zone_id = Some("no-prefix".to_string());
        aggregator.ingest(document).await;

        assert_eq!(aggregator.producer_count().await, 0);
        let refusals = aggregator.refusals().await;
        assert_eq!(refusals.len(), 1);
        assert!(matches!(
            refusals[0].1,
            AdmissionRefusal::ZoneIdNotPrefixed { .. }
        ));
    }

    #[tokio::test]
    async fn a_refusal_does_not_outlive_the_producer_it_names() {
        // Refusals outlive successes deliberately, so a producer author can see its
        // documents were dropped. Outliving the producer itself is different: a surface
        // listing "producers with problems" would show something that no longer exists.
        let aggregator = ProducerAggregator::detached();
        let mut document = pipeline();
        document.target.zone_id = Some("no-prefix".to_string());
        aggregator.ingest(document).await;
        assert_eq!(aggregator.refusals().await.len(), 1);

        aggregator.remove("hqplayer:living-room").await;
        assert!(
            aggregator.refusals().await.is_empty(),
            "a refusal survived the removal of the producer it names"
        );
    }

    #[tokio::test]
    async fn an_unscoped_stop_hint_cannot_clear_a_first_publication_refusal() {
        // AdapterStopping has no run identity. Clearing by adapter name would let a delayed
        // stop from run N erase a refusal recorded by run N+1.
        use unified_hifi_control::bus::BusEvent;
        let aggregator = ProducerAggregator::detached();

        // Refused on its very first document, so no producer entry is ever created.
        let mut never_admitted = pipeline();
        never_admitted.target.zone_id = Some("no-prefix".to_string());
        aggregator.ingest(never_admitted).await;
        assert_eq!(aggregator.producer_count().await, 0);
        assert_eq!(aggregator.refusals().await.len(), 1);

        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert_eq!(aggregator.refusals().await.len(), 1);
    }

    #[tokio::test]
    async fn producer_type_is_not_used_as_a_cleanup_authority() {
        // Ownership is an exact PublicationOrigin, not a producer-type string.
        use unified_hifi_control::bus::BusEvent;
        let aggregator = ProducerAggregator::detached();

        let mut unprefixed = pipeline();
        unprefixed.producer.producer_id = "living-room".to_string();
        unprefixed.producer.producer_type = "hqplayer".to_string();
        unprefixed.target.zone_id = Some("no-prefix".to_string());
        aggregator.ingest(unprefixed).await;
        assert_eq!(aggregator.refusals().await.len(), 1);

        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert_eq!(aggregator.refusals().await.len(), 1);
    }

    #[tokio::test]
    async fn stopping_one_adapter_leaves_another_adapters_refusal_alone() {
        // The other direction: the flush must be as narrow for refusals as it is for
        // producers.
        use unified_hifi_control::bus::BusEvent;
        let aggregator = ProducerAggregator::detached();

        let mut other = pipeline();
        other.producer.producer_id = "roon:core".to_string();
        other.producer.producer_type = "roon".to_string();
        other.target.zone_id = Some("not-prefixed".to_string());
        aggregator.ingest(other).await;
        assert_eq!(aggregator.refusals().await.len(), 1);

        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert_eq!(
            aggregator.refusals().await.len(),
            1,
            "stopping hqplayer removed a roon refusal"
        );
    }

    #[tokio::test]
    async fn a_later_success_does_not_erase_the_record_of_a_refusal() {
        let aggregator = ProducerAggregator::detached();
        let key = ProducerKey::of(&pipeline());
        aggregator.ingest(pipeline()).await;

        let mut stale = pipeline();
        stale.revisions = DocumentRevisions::new(12, 4000);
        aggregator.ingest(stale).await;

        let mut good = pipeline();
        good.revisions = DocumentRevisions::new(12, 4194);
        aggregator.ingest(good).await;

        let snapshot = aggregator.snapshot(&key).await.expect("snapshot present");
        assert_eq!(snapshot.document.revisions.state.0, 4194);
        assert!(
            snapshot.last_refusal.is_some(),
            "the refusal record was erased by a later success"
        );
    }
}

// =============================================================================
// The bounded actor command channel drives the aggregator
// =============================================================================

// =============================================================================
// Retirement is not undone by a straggler
//
// The withdrawn design used two independent broadcast channels and therefore had no relative
// lifecycle/publication order. These tests retain the hostile sequences as regression probes;
// production ingress now uses one bounded actor command channel and synchronous run leases.
// =============================================================================

mod retirement {
    use super::*;
    use unified_hifi_control::bus::BusEvent;
    use unified_hifi_control::producers::AdapterRuns;

    fn at_epoch(mut document: ProducerDocument, epoch: u64) -> ProducerDocument {
        document.producer.epoch = ProducerEpoch(epoch);
        document
    }

    /// An aggregator plus the run registry it judges publications against.
    fn rigged() -> (ProducerAggregator, std::sync::Arc<AdapterRuns>) {
        let aggregator = ProducerAggregator::detached();
        let runs = aggregator.runs().clone();
        (aggregator, runs)
    }

    #[tokio::test]
    async fn a_delayed_publish_from_run_n_is_rejected_after_run_n_plus_one_starts() {
        // The interleaving that defeats every public-bus-ordered scheme: run N publishes, the
        // document sits unread in the adaptive channel, run N ends, run N+1 begins, and only
        // then is the document read. Nothing about the *public* bus is involved, so no amount
        // of care about `AdapterStopping` ordering could have caught it.
        let (aggregator, runs) = rigged();
        let document = pipeline();
        let key = ProducerKey::of(&document);

        let first = runs.begin("hqplayer");
        let queued = first.origin().clone();
        runs.end(&first);
        let _second = runs.begin("hqplayer");

        let admission = aggregator.ingest_from(&queued, document).await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a document from an ended run was admitted: {admission:?}"
        );
        assert!(
            aggregator.snapshot(&key).await.is_none(),
            "an ended run's straggler created a producer"
        );
    }

    #[tokio::test]
    async fn a_delayed_publish_from_run_n_is_rejected_even_before_a_successor_starts() {
        // The run simply ending is enough; a successor is not required.
        let (aggregator, runs) = rigged();
        let run = runs.begin("hqplayer");
        let origin = run.origin().clone();
        runs.end(&run);

        let admission = aggregator.ingest_from(&origin, pipeline()).await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a document from an ended run was admitted: {admission:?}"
        );
    }

    #[tokio::test]
    async fn a_delayed_stop_from_run_n_cannot_flush_run_n_plus_one() {
        // `AdapterStopping` carries no run, so it cannot be the boundary. It is honoured only
        // as a hint: producers are flushed when the registry says their run is no longer live.
        // Here run N+1 *is* live, so its producers survive a stop that belongs to run N.
        let (aggregator, runs) = rigged();
        let first = runs.begin("hqplayer");
        runs.end(&first);
        let second = runs.begin("hqplayer");

        let document = pipeline();
        let key = ProducerKey::of(&document);
        aggregator.ingest_from(second.origin(), document).await;
        assert_eq!(aggregator.producer_count().await, 1);

        // The stop from run N, arriving late.
        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert!(
            aggregator.snapshot(&key).await.is_some(),
            "a delayed stop from an earlier run flushed the live run's producers"
        );
    }

    #[tokio::test]
    async fn an_ended_run_projects_last_known_without_a_stop_flush() {
        let (aggregator, runs) = rigged();
        let run = runs.begin("hqplayer");
        let document = pipeline();
        let key = ProducerKey::of(&document);
        aggregator.ingest_from(run.origin(), document).await;
        runs.end(&run);

        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert_eq!(
            aggregator
                .snapshot(&key)
                .await
                .expect("last-known")
                .presence,
            ProducerPresence::LastKnown
        );
    }

    #[tokio::test]
    async fn run_n_plus_one_may_republish_the_same_producer_epoch() {
        // Adapter restart is not producer restart. An engine that never restarted keeps its
        // epoch, and a reconnecting adapter must be able to republish it - the case an
        // epoch-based adapter guard stranded permanently.
        let (aggregator, runs) = rigged();
        let document = pipeline();
        let key = ProducerKey::of(&document);
        let epoch = document.producer.epoch;

        let first = runs.begin("hqplayer");
        aggregator
            .ingest_from(first.origin(), document.clone())
            .await;
        runs.end(&first);
        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        let second = runs.begin("hqplayer");
        let admission = aggregator.ingest_from(second.origin(), document).await;
        assert!(
            matches!(admission, Admission::Admitted(_)),
            "a reconnected adapter could not republish an unchanged producer: {admission:?}"
        );
        let snapshot = aggregator.snapshot(&key).await.expect("republished");
        assert_eq!(
            snapshot.document.producer.epoch, epoch,
            "the same producer epoch must be admissible under a new run"
        );
    }

    #[tokio::test]
    async fn a_new_run_cannot_lower_the_producer_watermark() {
        // Run identity is connection liveness, not producer history. Reconnecting cannot
        // make a lower revision authoritative at the same producer epoch.
        let (aggregator, runs) = rigged();
        let mut ahead = pipeline();
        ahead.revisions = DocumentRevisions::new(12, 5000);
        let key = ProducerKey::of(&ahead);

        let first = runs.begin("hqplayer");
        aggregator.ingest_from(first.origin(), ahead).await;
        runs.end(&first);

        let second = runs.begin("hqplayer");
        let admission = aggregator.ingest_from(second.origin(), pipeline()).await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a new run lowered the producer watermark: {admission:?}"
        );
        let snapshot = aggregator.snapshot(&key).await.expect("previous snapshot");
        assert_eq!(snapshot.document.revisions.state.0, 5000);
    }

    #[tokio::test]
    async fn an_explicit_producer_removal_still_requires_a_strictly_higher_epoch() {
        // Producer-epoch retirement is a *different* lifecycle from adapter runs and keeps its
        // own rule: `ProducerRemoved` says the producer is gone, and only the producer can
        // contradict that by announcing a restart.
        let (aggregator, runs) = rigged();
        let document = pipeline();
        let key = ProducerKey::of(&document);
        let epoch = document.producer.epoch.0;

        let run = runs.begin("hqplayer");
        aggregator.ingest_from(run.origin(), document.clone()).await;
        aggregator
            .remove_from(run.origin(), &document.producer.producer_id)
            .await;
        assert_eq!(aggregator.producer_count().await, 0);

        // Same epoch, same live run: refused, because the producer was retired by identity.
        let admission = aggregator.ingest_from(run.origin(), document.clone()).await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a removed producer was resurrected at the same epoch: {admission:?}"
        );
        assert!(aggregator.snapshot(&key).await.is_none());

        // Strictly higher epoch: admitted, because that is a restart.
        let restarted = at_epoch(document, epoch + 1);
        let admission = aggregator.ingest_from(run.origin(), restarted).await;
        assert!(
            matches!(admission, Admission::Admitted(_)),
            "a restarted producer at a higher epoch was refused: {admission:?}"
        );
        assert!(aggregator.snapshot(&key).await.is_some());
    }

    #[tokio::test]
    async fn a_producer_removal_is_not_undone_by_a_new_adapter_run() {
        // The unsoundness this replaces: a reconnect used to clear the bar, so a straggler
        // from before the removal could resurrect the producer. A new run does not speak for
        // the producer's own lifecycle.
        let (aggregator, runs) = rigged();
        let document = pipeline();
        let key = ProducerKey::of(&document);

        let first = runs.begin("hqplayer");
        aggregator
            .ingest_from(first.origin(), document.clone())
            .await;
        aggregator
            .remove_from(first.origin(), &document.producer.producer_id)
            .await;
        runs.end(&first);

        let second = runs.begin("hqplayer");
        let admission = aggregator.ingest_from(second.origin(), document).await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a new adapter run undid a producer-epoch retirement: {admission:?}"
        );
        assert!(aggregator.snapshot(&key).await.is_none());
    }

    #[tokio::test]
    async fn a_delayed_removal_from_an_ended_run_cannot_retire_a_live_producer() {
        // Removals are stamped too, for the same reason publications are.
        let (aggregator, runs) = rigged();
        let document = pipeline();
        let key = ProducerKey::of(&document);

        let first = runs.begin("hqplayer");
        let stale = first.origin().clone();
        runs.end(&first);
        let second = runs.begin("hqplayer");
        aggregator
            .ingest_from(second.origin(), document.clone())
            .await;

        let removed = aggregator
            .remove_from(&stale, &document.producer.producer_id)
            .await;
        assert_eq!(removed, 0, "a removal from an ended run retired a producer");
        assert!(
            aggregator.snapshot(&key).await.is_some(),
            "a delayed removal from run N retired run N+1's producer"
        );
    }

    #[tokio::test]
    async fn stopping_one_adapter_does_not_flush_another_adapters_producer() {
        let (aggregator, runs) = rigged();
        let mut other = pipeline();
        other.producer.producer_id = "roon:core".to_string();
        other.producer.producer_type = "roon".to_string();
        let key = ProducerKey::of(&other);

        let roon = runs.begin("roon");
        aggregator.ingest_from(roon.origin(), other).await;
        let hqp = runs.begin("hqplayer");
        runs.end(&hqp);

        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert!(
            aggregator.snapshot(&key).await.is_some(),
            "stopping hqplayer flushed a roon producer"
        );
    }

    #[tokio::test]
    async fn a_stale_run_publication_retains_a_diagnostic_without_creating_a_producer() {
        let (aggregator, runs) = rigged();
        let run = runs.begin("hqplayer");
        let origin = run.origin().clone();
        runs.end(&run);
        aggregator.ingest_from(&origin, pipeline()).await;

        assert_eq!(aggregator.producer_count().await, 0);
        assert!(matches!(
            aggregator.refusals().await.as_slice(),
            [(_, AdmissionRefusal::StaleAdapterRun { .. })]
        ));
    }
}

// =============================================================================
// RED counterexamples for the lifecycle redesign (solution-space revision 2)
//
// Each states a consequence of an invariant in `.oh/adaptive-publication.md`. Written against
// whatever API expresses them, and migrated onto the actor API as it lands.
// =============================================================================

mod lifecycle_counterexamples {
    use super::*;
    use unified_hifi_control::bus::BusEvent;
    use unified_hifi_control::producers::AdapterRuns;

    fn rigged() -> (ProducerAggregator, std::sync::Arc<AdapterRuns>) {
        let aggregator = ProducerAggregator::detached();
        let runs = aggregator.runs().clone();
        (aggregator, runs)
    }

    fn at(document: &ProducerDocument, epoch: u64, state: u64) -> ProducerDocument {
        let mut next = document.clone();
        next.producer.epoch = ProducerEpoch(epoch);
        next.revisions = DocumentRevisions::new(12, state);
        next
    }

    // I3: no resurrection. A new run must not be able to publish a lower revision at the
    // same epoch just because it is a new run.
    #[tokio::test]
    async fn red_same_epoch_lower_revision_across_runs_is_refused() {
        let (aggregator, runs) = rigged();
        let base = pipeline();
        let key = ProducerKey::of(&base);

        let first = runs.begin("hqplayer");
        aggregator
            .ingest_from(first.origin(), at(&base, 7, 5000))
            .await;
        runs.end(&first);

        let second = runs.begin("hqplayer");
        let admission = aggregator
            .ingest_from(second.origin(), at(&base, 7, 4193))
            .await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a new run published a LOWER revision at the same epoch and it was admitted: \
             {admission:?}"
        );
        let snapshot = aggregator.snapshot(&key).await.expect("snapshot");
        assert_eq!(
            snapshot.document.revisions.state.0, 5000,
            "the served revision regressed across an adapter run boundary"
        );
    }

    // I3: the epoch floor is a watermark, not a property of the held document.
    #[tokio::test]
    async fn red_lower_epoch_across_runs_is_refused() {
        let (aggregator, runs) = rigged();
        let base = pipeline();
        let key = ProducerKey::of(&base);

        let first = runs.begin("hqplayer");
        aggregator
            .ingest_from(first.origin(), at(&base, 8, 4193))
            .await;
        runs.end(&first);

        let second = runs.begin("hqplayer");
        let admission = aggregator
            .ingest_from(second.origin(), at(&base, 7, 9999))
            .await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a new run published a LOWER epoch and it was admitted: {admission:?}"
        );
        let snapshot = aggregator.snapshot(&key).await.expect("snapshot");
        assert_eq!(
            snapshot.document.producer.epoch,
            ProducerEpoch(8),
            "the served epoch regressed across an adapter run boundary"
        );
    }

    // The guard on over-correction: reconnect at the same epoch with real progress works.
    #[tokio::test]
    async fn red_same_epoch_higher_revision_across_runs_is_admitted() {
        let (aggregator, runs) = rigged();
        let base = pipeline();

        let first = runs.begin("hqplayer");
        aggregator
            .ingest_from(first.origin(), at(&base, 7, 4193))
            .await;
        runs.end(&first);

        let second = runs.begin("hqplayer");
        let admission = aggregator
            .ingest_from(second.origin(), at(&base, 7, 4200))
            .await;
        assert!(
            matches!(admission, Admission::Admitted(_)),
            "a reconnected adapter making real progress was refused: {admission:?}"
        );
    }

    // I2: informational stop events cannot erase either the snapshot or ordering floor.
    #[tokio::test]
    async fn red_stop_cannot_erase_the_snapshot_or_watermark() {
        let (aggregator, runs) = rigged();
        let base = pipeline();

        let first = runs.begin("hqplayer");
        aggregator
            .ingest_from(first.origin(), at(&base, 7, 5000))
            .await;
        runs.end(&first);
        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;
        assert_eq!(
            aggregator.producer_count().await,
            1,
            "an informational stop event erased the last-known snapshot"
        );

        let second = runs.begin("hqplayer");
        let admission = aggregator
            .ingest_from(second.origin(), at(&base, 7, 4193))
            .await;
        assert!(
            matches!(admission, Admission::Refused(_)),
            "the stop event erased the ordering floor: {admission:?}"
        );
    }

    // I6: retirement covers keys the aggregator has never seen.
    #[tokio::test]
    async fn red_retiring_an_unseen_producer_blocks_a_later_straggler() {
        let (handle, actor, view) = unified_hifi_control::producers::AdaptiveRuntime::build(
            tokio_util::sync::CancellationToken::new(),
            8,
        );
        let worker = tokio::spawn(async move { actor.run().await });
        let base = pipeline();
        let key = ProducerKey::of(&base);
        let run = handle.begin_run("hqplayer");

        // Nothing has ever been published for this key.
        handle
            .retire(&run, &base.producer.producer_id, base.producer.epoch)
            .await
            .expect("retirement applied");

        let admission = handle.publish(&run, base).await.expect("verdict");
        assert!(
            matches!(admission, Admission::Refused(_)),
            "a straggler for a producer retired before it was ever seen was admitted: \
             {admission:?}"
        );
        assert!(
            view.snapshot(&key).is_none(),
            "an unseen retirement failed to block resurrection"
        );
        drop(run);
        drop(handle);
        worker.await.expect("actor");
    }

    // I5: cleanup and refusal isolation must be by exact origin, never by adapter identity.
    #[tokio::test]
    async fn red_ending_an_old_run_cannot_clear_the_replacement_runs_refusal() {
        let (aggregator, runs) = rigged();

        let first = runs.begin("hqplayer");
        runs.end(&first);
        let second = runs.begin("hqplayer");

        // The live run records a refusal.
        let mut bad = pipeline();
        bad.target.zone_id = Some("not-prefixed".to_string());
        aggregator.ingest_from(second.origin(), bad).await;
        assert_eq!(
            aggregator.refusals().await.len(),
            1,
            "precondition: the live run has a refusal on record"
        );

        // The ended run's cleanup arrives late.
        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert_eq!(
            aggregator.refusals().await.len(),
            1,
            "an ended run's cleanup cleared the live run's refusal: {:?}",
            aggregator.refusals().await
        );
    }

    // I5/I8: no string-inferred cleanup; each ended run remains visible as last-known.
    #[tokio::test]
    async fn red_ended_runs_remain_last_known_without_string_inferred_cleanup() {
        let (aggregator, runs) = rigged();

        let stopping = runs.begin("hqplayer");
        let mut old = pipeline();
        old.producer.producer_id = "hqplayer:one".to_string();
        let old_key = ProducerKey::of(&old);
        aggregator.ingest_from(stopping.origin(), old).await;

        let live = runs.begin("hqplayer");
        let mut fresh = pipeline();
        fresh.producer.producer_id = "hqplayer:two".to_string();
        let live_key = ProducerKey::of(&fresh);
        aggregator.ingest_from(live.origin(), fresh).await;

        runs.end(&live);

        // `stopping` is already superseded and `live` ended. The public event is only a hint.
        aggregator
            .apply_bus_event(BusEvent::AdapterStopping {
                adapter: "hqplayer".to_string(),
                reason: None,
            })
            .await;

        assert_eq!(
            aggregator
                .snapshot(&old_key)
                .await
                .expect("old last-known")
                .presence,
            ProducerPresence::LastKnown
        );
        assert_eq!(
            aggregator
                .snapshot(&live_key)
                .await
                .expect("live last-known")
                .presence,
            ProducerPresence::LastKnown
        );
    }
}

mod timestamp_fields {
    use super::*;

    fn expect_timestamp_refusal(document: ProducerDocument, field: &str) {
        let admission = admit_fresh(document);
        assert!(
            matches!(
                admission,
                Admission::Refused(AdmissionRefusal::TimestampNotRfc3339 { .. })
            ),
            "a malformed {field} was admitted: {admission:?}"
        );
    }

    #[test]
    fn red_lane_freshness_observed_at_must_be_rfc3339() {
        let mut document = pipeline();
        document.lanes[0].freshness.observed_at = Some(Timestamp::new("nope"));
        expect_timestamp_refusal(document, "LaneHealth.freshness.observed_at");
    }

    #[test]
    fn red_lane_value_freshness_observed_at_must_be_rfc3339() {
        let mut document = pipeline();
        let control = document
            .controls
            .iter_mut()
            .find(|c| !c.values.is_empty())
            .expect("a control with a lane");
        control.values[0].freshness.observed_at = Some(Timestamp::new("nope"));
        expect_timestamp_refusal(document, "LaneValue.freshness.observed_at");
    }

    #[test]
    fn red_divergence_detected_at_must_be_rfc3339() {
        let mut document = pipeline();
        let control = document
            .controls
            .iter_mut()
            .find(|c| !c.divergences.is_empty())
            .expect("a control with a divergence");
        control.divergences[0].detected_at = Some(Timestamp::new("nope"));
        expect_timestamp_refusal(document, "Divergence.detected_at");
    }

    #[test]
    fn red_change_set_created_at_must_be_rfc3339() {
        let mut document = staged();
        document.change_sets[0].created_at = Timestamp::new("nope");
        expect_timestamp_refusal(document, "ChangeSet.created_at");
    }

    #[test]
    fn red_change_set_updated_at_must_be_rfc3339() {
        let mut document = staged();
        document.change_sets[0].updated_at = Timestamp::new("nope");
        expect_timestamp_refusal(document, "ChangeSet.updated_at");
    }

    #[test]
    fn red_change_set_retention_expires_at_must_be_rfc3339() {
        let mut document = staged();
        document.change_sets[0].retention.expires_at = Some(Timestamp::new("nope"));
        expect_timestamp_refusal(document, "ChangeSet.retention.expires_at");
    }

    #[test]
    fn red_draft_policy_retention_expires_at_must_be_rfc3339() {
        let mut document = pipeline();
        let policy = document
            .draft_policy
            .as_mut()
            .expect("the fixture publishes a draft policy");
        policy.retention.expires_at = Some(Timestamp::new("nope"));
        expect_timestamp_refusal(document, "DraftPolicy.retention.expires_at");
    }

    #[test]
    fn red_operation_history_at_must_be_rfc3339() {
        let mut document = outcomes();
        let operation = document
            .operations
            .iter_mut()
            .find(|o| !o.history.is_empty())
            .expect("the fixture publishes an operation with history");
        operation.history[0].at = Timestamp::new("nope");
        expect_timestamp_refusal(document, "OutcomeTransition.at");
    }

    #[test]
    fn red_every_canonical_fixture_still_admits_with_full_timestamp_validation() {
        for name in [
            "hqplayer_pipeline_v1",
            "hqplayer_degraded_lanes",
            "staged_intent_multi_surface",
            "command_outcomes",
            "control_removed_after_advance",
            "forward_compatible_additions",
        ] {
            let admission = admit_fresh(fixture(name));
            assert!(
                matches!(admission, Admission::Admitted(_)),
                "{name} was refused once every Timestamp field is validated: {admission:?}"
            );
        }
    }
}

mod internal_bus {
    use super::*;
    use unified_hifi_control::producers::AdaptiveRuntime;

    #[tokio::test]
    async fn a_document_published_after_construction_but_before_run_is_still_admitted() {
        let (handle, actor, view) =
            AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 8);
        let run = handle.begin_run("hqplayer");
        let document = pipeline();
        let key = ProducerKey::of(&document);
        handle
            .publish_detached(&run, document)
            .await
            .expect("queued before actor start");
        drop(handle);
        tokio::time::timeout(std::time::Duration::from_secs(5), actor.run())
            .await
            .expect("actor drained");
        assert!(
            view.snapshot(&key).is_some(),
            "a document queued between construction and actor start was dropped"
        );
        drop(run);
    }

    #[tokio::test]
    async fn a_document_published_on_the_internal_bus_reaches_the_aggregator() {
        let (handle, actor, view) =
            AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 8);
        let worker = tokio::spawn(async move { actor.run().await });
        let run = handle.begin_run("hqplayer");

        let document = pipeline();
        let key = ProducerKey::of(&document);
        handle.publish(&run, document).await.expect("verdict");
        assert!(
            view.snapshot(&key).is_some(),
            "the actor never saw the document"
        );
        drop(run);
        drop(handle);
        worker.await.expect("actor");
    }

    #[tokio::test]
    async fn a_capacity_two_actor_applies_every_command_under_backpressure() {
        let (handle, actor, view) =
            AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 2);
        let worker = tokio::spawn(async move { actor.run().await });
        let run = handle.begin_run("hqplayer");
        for step in 0..64_u64 {
            let mut document = pipeline();
            document.revisions = DocumentRevisions::new(12, 4193 + step);
            handle
                .publish_detached(&run, document)
                .await
                .expect("lossless enqueue");
        }

        let mut final_document = pipeline();
        final_document.revisions = DocumentRevisions::new(12, 9999);
        let key = ProducerKey::of(&final_document);
        handle
            .publish(&run, final_document)
            .await
            .expect("final verdict");
        assert_eq!(
            view.snapshot(&key)
                .expect("final snapshot")
                .document
                .revisions
                .state
                .0,
            9999
        );
        drop(run);
        drop(handle);
        worker.await.expect("actor");
    }
}

// =============================================================================
// Determinism of the store
// =============================================================================

#[tokio::test]
async fn snapshots_are_returned_in_a_deterministic_order() {
    let aggregator = ProducerAggregator::detached();
    for id in ["hqplayer:z", "hqplayer:a", "hqplayer:m"] {
        let mut document = pipeline();
        document.producer.producer_id = id.to_string();
        aggregator.ingest(document).await;
    }
    let ids: Vec<String> = aggregator
        .snapshots()
        .await
        .into_iter()
        .map(|s| s.key.producer_id)
        .collect();
    let mut sorted = ids.clone();
    sorted.sort();
    assert_eq!(ids, sorted, "snapshot order must not depend on hashing");
}

#[test]
fn lane_witnesses_are_keyed_by_lane_without_duplicates() {
    // A guard on the witness map's shape, independent of any aggregator instance.
    let document = pipeline();
    let mut seen: BTreeMap<TransportLane, usize> = BTreeMap::new();
    for lane in &document.lanes {
        *seen.entry(lane.lane.clone()).or_default() += 1;
    }
    assert!(
        seen.values().all(|count| *count == 1),
        "a fixture publishes a lane twice, which the witness map cannot represent"
    );
}

#[test]
fn provenance_helper_is_available_for_constructing_test_lanes() {
    // Keeps the test module honest about using the contract's own constructors rather than
    // hand-rolling lane values that could not occur in practice.
    let value = LaneValue::grounded(
        ValueLane::Desired,
        ControlValue::choice("sdm"),
        Provenance::runtime(),
    );
    assert!(value.is_consistent());
    assert_eq!(value.grounded_value(), Some(&ControlValue::choice("sdm")));
    let _ = OperationId::new("op-1");
}
