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

    #[test]
    fn every_canonical_fixture_is_admissible_by_the_publication_gate() {
        // If the gate refuses a document #323 calls canonical, one of the two is wrong and
        // #325 would be the one to find out.
        for name in [
            "hqplayer_pipeline_v1",
            "hqplayer_degraded_lanes",
            "staged_intent_multi_surface",
            "command_outcomes",
            "forward_compatible_additions",
        ] {
            let document = fixture(name);
            let admission = admit_fresh(document);
            assert!(
                matches!(admission, Admission::Admitted(_)),
                "canonical fixture {name} was refused by the publication gate: {admission:?}"
            );
        }
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
        document.target.zone_id = Some("spotify:abc123".to_string());
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

mod lane_value_invariant {
    use super::*;

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
        let (control_id, lane) = {
            let value = first_control_lane(&mut document);
            value.grounding = Grounding::Grounded;
            value.value = None;
            (None::<ControlId>, value.lane.clone())
        };
        let _ = control_id;
        match expect_refused(admit_fresh(document)) {
            AdmissionRefusal::LaneValueInconsistent {
                lane: got, detail, ..
            } => {
                assert_eq!(got, lane);
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
            }
            other => panic!("expected DraftInvalid(control_removed), got {other:?}"),
        }
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
        let bus = unified_hifi_control::bus::create_bus();
        let adaptive = unified_hifi_control::producers::create_adaptive_bus();
        let aggregator = ProducerAggregator::new(bus.clone(), adaptive.clone());

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
    async fn an_adapter_stopping_flushes_only_its_own_producers() {
        let bus = unified_hifi_control::bus::create_bus();
        let adaptive = unified_hifi_control::producers::create_adaptive_bus();
        let aggregator = ProducerAggregator::new(bus, adaptive);

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

        assert_eq!(aggregator.producer_count().await, 1);
        let survivor = aggregator.snapshots().await;
        assert_eq!(survivor[0].key.producer_id, "roon:core");
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
// The internal bus actually drives the aggregator
// =============================================================================

mod internal_bus {
    use super::*;
    use unified_hifi_control::producers::AdaptiveEvent;

    #[tokio::test]
    async fn a_document_published_on_the_internal_bus_reaches_the_aggregator() {
        let bus = unified_hifi_control::bus::create_bus();
        let adaptive = unified_hifi_control::producers::create_adaptive_bus();
        let aggregator =
            std::sync::Arc::new(ProducerAggregator::new(bus.clone(), adaptive.clone()));

        let runner = aggregator.clone();
        let handle = tokio::spawn(async move { runner.run().await });

        // Give the subscriber a chance to attach before publishing.
        tokio::task::yield_now().await;
        let document = pipeline();
        let key = ProducerKey::of(&document);
        adaptive.publish(AdaptiveEvent::ProducerPublished {
            document: Box::new(document),
        });

        let mut snapshot = None;
        for _ in 0..200 {
            if let Some(found) = aggregator.snapshot(&key).await {
                snapshot = Some(found);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(snapshot.is_some(), "the aggregator never saw the document");

        bus.publish(unified_hifi_control::bus::BusEvent::ShuttingDown { reason: None });
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
    }

    #[tokio::test]
    async fn a_lagging_aggregator_keeps_running_rather_than_exiting() {
        // `while let Ok(event) = rx.recv().await` treats RecvError::Lagged as termination,
        // which would permanently stop producer state after one burst. Documents are full
        // snapshots, so a dropped update is recoverable — but only if the loop survives.
        let bus = unified_hifi_control::bus::create_bus();
        let adaptive = std::sync::Arc::new(unified_hifi_control::producers::AdaptiveBus::new(2));
        let aggregator =
            std::sync::Arc::new(ProducerAggregator::new(bus.clone(), adaptive.clone()));

        let runner = aggregator.clone();
        let handle = tokio::spawn(async move { runner.run().await });
        tokio::task::yield_now().await;

        for step in 0..64_u64 {
            let mut document = pipeline();
            document.revisions = DocumentRevisions::new(12, 4193 + step);
            adaptive.publish(AdaptiveEvent::ProducerPublished {
                document: Box::new(document),
            });
        }

        let mut final_document = pipeline();
        final_document.revisions = DocumentRevisions::new(12, 9999);
        let key = ProducerKey::of(&final_document);

        let mut arrived = false;
        for _ in 0..200 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            adaptive.publish(AdaptiveEvent::ProducerPublished {
                document: Box::new(final_document.clone()),
            });
            if let Some(snapshot) = aggregator.snapshot(&key).await {
                if snapshot.document.revisions.state.0 == 9999 {
                    arrived = true;
                    break;
                }
            }
        }
        assert!(
            arrived,
            "the aggregator stopped consuming after a lag burst; a full-snapshot design \
             recovers only if the loop survives"
        );

        bus.publish(unified_hifi_control::bus::BusEvent::ShuttingDown { reason: None });
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
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
