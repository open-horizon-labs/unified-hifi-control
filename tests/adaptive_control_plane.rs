//! Contract tests for the adaptive control plane (#331).
//!
//! Every test here compares **projections of the same fixture**. That is the whole point: the
//! acceptance criterion is not "the web page works" but "the web page, an MCP client and a device
//! cannot disagree about the same producer", and the only way to falsify that is to project one
//! document three ways and look for a contradiction.
//!
//! Hermetic by construction: canonical `#323` fixtures on disk, plus documents derived from them in
//! memory. No socket, no clock, no live HQPlayer host.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;

use async_trait::async_trait;

use unified_hifi_control::adaptive::{
    admit_document_str, ApplyEffect, ApplyLane, Authority, AvailabilityState, ChoiceSet,
    CommandOutcome, ControlId, ControlKind, ControlValue, DocumentRevisions, OperationId,
    OperationRecord, OperationRequest, ProducerDocument, ProducerEpoch, Reason, ReasonCode,
    RevisionRef, RiskClass, SourceClass, TargetRole, WriteAttempt, CONSUMER_SCHEMA_VERSION,
};
use unified_hifi_control::producers::control_plane::{
    CommandAcceptance, CommandRefusal, ControlPlaneService, RouteRefusal, SemanticCommand,
    SemanticCommandExecutor,
};
use unified_hifi_control::producers::surface::{
    project, project_raw, timing_key, ChoiceProjection, Mutability, ProjectionNotice,
    RenderedProjection, SurfaceProfile, SurfaceProjection, OMITTED_BUDGET_EXHAUSTED,
    OMITTED_CONTAINER_DROPPED, SURFACE_PROJECTION_SCHEMA, TIMING_HELD_UNTIL_CHAIN_LOADED,
    TIMING_LIVE_IMMEDIATE, TIMING_PERSISTENT_ONLY, TIMING_RESTART_REQUIRED, TIMING_UNKNOWN,
    TIMING_VERIFIED_PENDING, WITHHELD_PER_CHOICE_REASON, WITHHELD_RISK_EXCEEDS_SURFACE,
    WITHHELD_UNRENDERABLE_KIND,
};
use unified_hifi_control::producers::{
    AdaptiveRuntime, ProducerKey, ProducerPresence, ProducerSnapshot,
};

// =============================================================================
// Harness
// =============================================================================

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

fn raw_fixture(name: &str) -> serde_json::Value {
    let path = format!(
        "{}/tests/fixtures/adaptive/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
    serde_json::from_str(&raw).unwrap_or_else(|e| panic!("parse {path}: {e}"))
}

fn pipeline() -> ProducerDocument {
    fixture("hqplayer_pipeline_v1")
}

fn snapshot(document: ProducerDocument) -> ProducerSnapshot {
    ProducerSnapshot {
        key: ProducerKey::of(&document),
        document: Arc::new(document),
        lanes: BTreeMap::new(),
        presence: ProducerPresence::Live,
        repairs: Vec::new(),
        last_refusal: None,
    }
}

fn rendered(document: ProducerDocument, profile: &SurfaceProfile) -> RenderedProjection {
    let snapshot = snapshot(document);
    match project(&snapshot, profile) {
        SurfaceProjection::Rendered(rendered) => *rendered,
        SurfaceProjection::Fallback(fallback) => panic!(
            "expected a rendered projection for {}, got a fallback: {:?}",
            profile.surface_id, fallback.refusal
        ),
    }
}

fn web() -> SurfaceProfile {
    SurfaceProfile::web()
}

fn mcp() -> SurfaceProfile {
    SurfaceProfile::mcp()
}

fn device(max_controls: usize) -> SurfaceProfile {
    SurfaceProfile::compact_device("knob", max_controls)
}

/// A non-terminal operation targeting one control, so invalidation and busy scoping are reachable.
fn operation(id: &str, control: &str, outcome: CommandOutcome) -> OperationRecord {
    OperationRecord {
        id: OperationId::new(id),
        correlation_id: format!("corr-{id}"),
        request: Some(OperationRequest {
            control: ControlId::new(control),
            requested: ControlValue::Choice("pcm".to_string()),
            lane: ApplyLane::Immediate,
            base: RevisionRef {
                epoch: ProducerEpoch(7),
                control_plane: unified_hifi_control::adaptive::Revision(12),
                state: unified_hifi_control::adaptive::Revision(4193),
            },
        }),
        change_set: None,
        generation: None,
        write_attempt: WriteAttempt::Attempted,
        outcome,
        observed: None,
        steps: Vec::new(),
        revision_boundaries: Vec::new(),
        history: Vec::new(),
        recovery: None,
        reason: None,
        extensions: Default::default(),
    }
}

fn control_mut<'a>(
    document: &'a mut ProducerDocument,
    id: &str,
) -> &'a mut unified_hifi_control::adaptive::Control {
    document
        .controls
        .iter_mut()
        .find(|control| control.id.as_str() == id)
        .unwrap_or_else(|| panic!("fixture has no control {id}"))
}

// =============================================================================
// The projection is one atomic view of one revision
// =============================================================================

#[test]
fn a_projection_carries_exactly_one_producer_position() {
    let document = pipeline();
    let expected = document.position();
    let view = rendered(document, &web());

    assert_eq!(
        view.position, expected,
        "a projection must be stamped with the exact position of the document it came from, so a \
         surface can never combine a mode read at one revision with an enumeration read at another"
    );
    assert_eq!(view.schema, SURFACE_PROJECTION_SCHEMA);
    assert_eq!(view.surface_id, "web");
    assert_eq!(view.producer.producer_id, "hqplayer:living-room");
    assert_eq!(view.producer.role, TargetRole::DspEngine);
}

#[test]
fn every_advertised_control_reaches_an_unbounded_surface() {
    let document = pipeline();
    let advertised: Vec<String> = document
        .controls
        .iter()
        .map(|control| control.id.to_string())
        .collect();
    let view = rendered(document, &web());

    let projected: Vec<String> = view
        .controls
        .iter()
        .map(|control| control.id.to_string())
        .collect();
    for id in &advertised {
        assert!(
            projected.contains(id),
            "an unbudgeted surface must receive every control the producer advertised; {id} is \
             missing"
        );
    }
    assert!(
        view.omitted.is_empty(),
        "nothing may be omitted without a budget: {:?}",
        view.omitted
    );
}

#[test]
fn the_observed_lane_never_carries_staged_or_held_intent() {
    let view = rendered(pipeline(), &web());

    let filter = view
        .control("hqplayer.pipeline.filter_1x")
        .expect("filter_1x is projected");
    let observed = filter.observed.as_ref().expect("filter_1x has an observed lane");
    assert_eq!(
        observed.grounded_value(),
        Some(&ControlValue::Choice("poly-sinc-gauss-long".to_string())),
        "the running signal path must show what the engine reports, never the staged edit"
    );
    let desired: Vec<_> = filter
        .intent
        .iter()
        .filter(|value| value.lane == unified_hifi_control::adaptive::ValueLane::Desired)
        .collect();
    assert_eq!(
        desired.len(),
        1,
        "the staged value must still be projected, but separately from observed"
    );

    let held = view
        .control("hqplayer.volume.fixed.level")
        .expect("the dormant fixed level is projected");
    assert!(
        held.observed
            .as_ref()
            .is_none_or(|value| value.grounded_value().is_none()),
        "a dormant control's observed lane is ungrounded and must not be filled from held intent"
    );
    assert!(
        held.intent
            .iter()
            .any(|value| value.lane == unified_hifi_control::adaptive::ValueLane::Held),
        "held intent must remain visible with held-intent language rather than disappearing"
    );
}

#[test]
fn availability_reasons_survive_projection_on_every_surface() {
    for profile in [web(), mcp(), device(64)] {
        let view = rendered(pipeline(), &profile);
        for control in &view.controls {
            if control.availability.state != AvailabilityState::Available {
                assert!(
                    !control.availability.reasons.is_empty(),
                    "{}: {} is {:?} with no reason; a user who cannot see why cannot escape the \
                     state that caused it",
                    profile.surface_id,
                    control.id,
                    control.availability.state
                );
            }
        }
        let rate = view
            .control("hqplayer.pipeline.rate.exact")
            .expect("the unavailable exact-rate control is still displayed");
        assert_eq!(rate.availability.reasons.len(), 2);
        assert!(
            rate.availability
                .reasons
                .iter()
                .all(|reason| reason.display_text_key.is_some()),
            "reason text must be addressable through the catalog, not carried as a hover title"
        );
    }
}

#[test]
fn constraints_that_name_a_control_are_projected_onto_it() {
    let view = rendered(pipeline(), &web());
    let rate = view.control("hqplayer.pipeline.rate.exact").expect("projected");
    let ids: Vec<&str> = rate
        .constraints
        .iter()
        .map(|constraint| constraint.id.as_str())
        .collect();
    assert!(
        ids.contains(&"hqplayer.constraint.exact_rate_requires_explicit_mode")
            && ids.contains(&"hqplayer.constraint.exact_rate_inert_when_adaptive"),
        "a surface must be able to explain a constraint without re-deriving it: {ids:?}"
    );
}

// =============================================================================
// Falsifying cross-surface projection drift
// =============================================================================

#[test]
fn no_two_surfaces_disagree_about_what_the_producer_said() {
    let profiles = [web(), mcp(), device(64)];
    let views: Vec<RenderedProjection> = profiles
        .iter()
        .map(|profile| rendered(pipeline(), profile))
        .collect();

    for (left_profile, left) in profiles.iter().zip(&views) {
        for (right_profile, right) in profiles.iter().zip(&views) {
            if left_profile.surface_id == right_profile.surface_id {
                continue;
            }
            assert_eq!(
                left.position, right.position,
                "two surfaces projecting one snapshot must cite one position"
            );
            for control in &left.controls {
                let Some(other) = right.control(control.id.as_str()) else {
                    continue;
                };
                assert_eq!(
                    control.kind, other.kind,
                    "{}: kind must never be substituted per surface",
                    control.id
                );
                assert_eq!(
                    control.availability, other.availability,
                    "{}: availability is the producer's verdict, identical everywhere",
                    control.id
                );
                assert_eq!(
                    control.observed, other.observed,
                    "{}: the observed lane is one fact",
                    control.id
                );
                assert_eq!(
                    control.unit, other.unit,
                    "{}: units cannot differ per surface",
                    control.id
                );
                assert_eq!(
                    control.range, other.range,
                    "{}: the numeric domain is the producer's, not the widget's",
                    control.id
                );
            }
        }
    }
}

#[test]
fn a_surface_that_offers_less_never_offers_something_different() {
    let unbounded = rendered(pipeline(), &web());
    for profile in [mcp(), device(64)] {
        let view = rendered(pipeline(), &profile);
        for control in &view.controls {
            let reference = unbounded
                .control(control.id.as_str())
                .expect("the unbounded surface sees everything");

            let offered: Vec<&str> = control.offered_choice_ids();
            let reference_offered: Vec<&str> = reference.offered_choice_ids();
            for id in &offered {
                assert!(
                    reference_offered.contains(id),
                    "{} on {}: offered choice {id} does not exist on the unbounded projection",
                    control.id,
                    profile.surface_id
                );
            }

            match (&control.mutability, &reference.mutability) {
                (Mutability::Mutable { description, .. }, Mutability::Mutable { description: reference_description, .. }) => {
                    assert_eq!(
                        description, reference_description,
                        "{} on {}: the cost of writing is the producer's, not the surface's",
                        control.id, profile.surface_id
                    );
                }
                (Mutability::Mutable { .. }, other) => panic!(
                    "{} on {}: offered as mutable while the unbounded surface reports {other:?}",
                    control.id, profile.surface_id
                ),
                (Mutability::Blocked { reasons }, reference_mutability) => {
                    assert!(
                        matches!(reference_mutability, Mutability::Blocked { reasons: reference_reasons } if reference_reasons == reasons),
                        "{} on {}: a producer block must be reported identically everywhere",
                        control.id,
                        profile.surface_id
                    );
                }
                (Mutability::ObservationOnly, reference_mutability) => {
                    assert!(
                        matches!(reference_mutability, Mutability::ObservationOnly),
                        "{} on {}: observation-only is a property of the document",
                        control.id,
                        profile.surface_id
                    );
                }
                (Mutability::WithheldFromSurface { .. }, _) => {}
            }
        }
    }
}

#[test]
fn a_surface_limitation_is_never_reported_as_the_producers_verdict() {
    let view = rendered(pipeline(), &device(64));

    let matrix = view.control("hqplayer.matrix.profile").expect("projected");
    match &matrix.mutability {
        Mutability::WithheldFromSurface { reason } => {
            assert_eq!(reason.code, ReasonCode::RequiresCapability);
            assert_eq!(
                reason.display_text_key.as_deref(),
                Some(WITHHELD_PER_CHOICE_REASON),
                "a control carrying per-choice availability must not be offered by a surface that \
                 cannot express it; the segmented-control case from the HQPTuner audit"
            );
        }
        other => panic!("expected the matrix profile to be withheld from a device, got {other:?}"),
    }
    assert_eq!(
        matrix.availability.state,
        AvailabilityState::Available,
        "withholding is about this surface's offer, never a rewrite of the producer's availability"
    );
    assert_eq!(
        matrix.offered_choice_ids(),
        vec!["", "room-a", "room-b"],
        "the option set stays visible even where mutation is withheld"
    );
}

#[test]
fn an_unrenderable_primitive_is_withheld_rather_than_flattened() {
    let view = rendered(pipeline(), &device(64));
    let composite = view.control("hqplayer.matrix.channel_map").expect("projected");
    assert_eq!(composite.kind, ControlKind::Composite);
    match &composite.mutability {
        Mutability::WithheldFromSurface { reason } => {
            assert_eq!(
                reason.display_text_key.as_deref(),
                Some(WITHHELD_UNRENDERABLE_KIND)
            );
        }
        other => panic!("expected a composite to be withheld from a device, got {other:?}"),
    }
}

#[test]
fn an_operation_riskier_than_the_surface_may_offer_is_withheld_with_that_reason() {
    let view = rendered(pipeline(), &mcp());
    let dither = view.control("hqplayer.settings.legacy_dither").expect("projected");
    assert_eq!(
        dither.availability.state,
        AvailabilityState::Deprecated,
        "a deprecated control still renders; the contract says so explicitly"
    );
    match &dither.mutability {
        Mutability::WithheldFromSurface { reason } => assert_eq!(
            reason.display_text_key.as_deref(),
            Some(WITHHELD_RISK_EXCEEDS_SURFACE),
            "a destructive operation must not be advertised to a surface that cannot confirm it"
        ),
        other => panic!("expected the destructive control to be withheld from MCP, got {other:?}"),
    }
}

#[test]
fn an_unrecognized_risk_class_is_never_advertised_as_invokable() {
    let mut document = pipeline();
    if let Some(apply) = control_mut(&mut document, "hqplayer.transport.play")
        .apply
        .as_mut()
    {
        apply.risk = RiskClass::Unrecognized("catastrophic".to_string());
    }
    let view = rendered(document, &web());
    let play = view.control("hqplayer.transport.play").expect("projected");
    assert!(
        matches!(play.mutability, Mutability::WithheldFromSurface { .. }),
        "a risk class from a newer minor must fail closed for mutation and open for display"
    );
    assert!(
        play.availability.state == AvailabilityState::Available,
        "failing closed on mutation must not hide the control"
    );
}

#[test]
fn the_producers_own_block_outranks_a_surface_limitation() {
    // `chain.sdm.filter` is unavailable *and* would be withheld from a device. The user must be
    // told the chain is not loaded, not that their knob is too small.
    let view = rendered(pipeline(), &device(64));
    let sdm = view.control("hqplayer.chain.sdm.filter").expect("projected");
    match &sdm.mutability {
        Mutability::Blocked { reasons } => assert_eq!(reasons[0].code, ReasonCode::ChainNotLoaded),
        other => panic!("the producer's reason must win, got {other:?}"),
    }
}

// =============================================================================
// Falsifying unknown schema / version fallback
// =============================================================================

#[test]
fn an_unsupported_major_degrades_to_an_informative_fallback() {
    let raw = raw_fixture("unsupported_major");
    match project_raw(&raw, &web()) {
        SurfaceProjection::Fallback(fallback) => {
            assert_eq!(fallback.surface_id, "web");
            assert_eq!(fallback.declared_schema.as_deref(), Some("2.0"));
            assert!(
                matches!(
                    fallback.refusal,
                    unified_hifi_control::adaptive::Refusal::UnsupportedMajor { document: 2, .. }
                ),
                "the surface must be told which generation it could not read: {:?}",
                fallback.refusal
            );
            assert!(
                fallback.producer_id.is_some(),
                "identity legible without trusting the body must survive, so the surface can name \
                 the producer it cannot render"
            );
        }
        SurfaceProjection::Rendered(_) => {
            panic!("a v2 document must never be partially rendered by a v1 consumer")
        }
    }
}

#[test]
fn a_newer_minor_renders_and_says_that_it_did_not_understand_everything() {
    let raw = raw_fixture("forward_compatible_additions");
    let projection = project_raw(&raw, &web());
    let view = projection
        .rendered()
        .expect("a newer minor of the same major must render");

    assert!(
        view.notices.iter().any(|notice| matches!(
            notice,
            ProjectionNotice::UnknownSchemaAdditions { document, consumer }
                if document.major == CONSUMER_SCHEMA_VERSION.major && *consumer == CONSUMER_SCHEMA_VERSION
        )),
        "the surface must know it is looking at a newer minor: {:?}",
        view.notices
    );
    assert!(
        view.control("future.spatial.render_mode").is_some(),
        "controls a v1.0 consumer does understand must still be rendered"
    );
    let vector = view
        .control("future.spatial.head_tracking")
        .expect("an unrecognized primitive is still displayed, never dropped");
    assert!(
        matches!(vector.mutability, Mutability::WithheldFromSurface { .. }),
        "an unrecognized control kind cannot be offered for mutation"
    );
    assert!(
        view.notices.iter().any(|notice| matches!(
            notice,
            ProjectionNotice::UnrecognizedControlKind { control, .. }
                if control.as_str() == "future.spatial.head_tracking"
        )),
        "and the surface must be told which control it could not fully interpret"
    );
    let quarantined = view
        .control("future.spatial.room_profile")
        .expect("an unrecognized availability state still renders");
    assert!(
        !quarantined.is_mutable_here(),
        "an availability state this build does not recognize must fail closed for mutation"
    );
}

#[test]
fn an_untested_product_version_notices_without_disabling_discovered_controls() {
    let mut document = pipeline();
    document.producer.product_version = Some("7.0.0-beta".to_string());
    let view = rendered(document, &web());

    assert!(
        view.notices.iter().any(|notice| matches!(
            notice,
            ProjectionNotice::UntestedProducerVersion { product_version } if product_version == "7.0.0-beta"
        )),
        "an unverified engine series is worth saying: {:?}",
        view.notices
    );
    assert!(
        view.control("hqplayer.pipeline.mode")
            .expect("projected")
            .is_mutable_here(),
        "runtime enumeration remains the authority on what the engine accepts; an unfamiliar \
         version string must not blank a working control"
    );
}

#[test]
fn a_verified_product_version_raises_no_untested_notice() {
    let view = rendered(pipeline(), &web());
    assert!(
        !view
            .notices
            .iter()
            .any(|notice| matches!(notice, ProjectionNotice::UntestedProducerVersion { .. })),
        "6.0.4 is a verified series; a notice here would train users to ignore notices"
    );
}

#[test]
fn a_last_known_producer_says_so_rather_than_looking_current() {
    let mut snapshot = snapshot(pipeline());
    snapshot.presence = ProducerPresence::LastKnown;
    let projection = project(&snapshot, &web());
    let view = projection.rendered().expect("last-known still renders");

    assert_eq!(view.presence, ProducerPresence::LastKnown);
    assert!(
        view.notices.contains(&ProjectionNotice::LastKnown),
        "retaining last-good values through a failure is right; presenting them as current is not"
    );
    assert!(
        !view.controls.is_empty(),
        "a failed poll must not blank the whole form"
    );
}

// =============================================================================
// Falsifying dependency invalidation
// =============================================================================

#[test]
fn a_pending_write_puts_its_dependents_in_reloading_and_withholds_stale_choices() {
    let mut document = pipeline();
    document.operations = vec![operation(
        "op-mode-1",
        "hqplayer.pipeline.mode",
        CommandOutcome::Pending,
    )];
    let view = rendered(document, &web());

    for dependent in [
        "hqplayer.pipeline.filter_1x",
        "hqplayer.pipeline.rate.exact",
        "hqplayer.chain.sdm.filter",
    ] {
        let control = view.control(dependent).expect("projected");
        match &control.choices {
            ChoiceProjection::Reloading {
                invalidated_by,
                operation,
            } => {
                assert_eq!(invalidated_by.as_str(), "hqplayer.pipeline.mode");
                assert_eq!(operation.as_str(), "op-mode-1");
            }
            other => panic!(
                "{dependent} must withhold its stale enumeration while the write that invalidates \
                 it is in flight, got {other:?}"
            ),
        }
        assert!(
            control.offered_choice_ids().is_empty(),
            "{dependent}: a choice about to be wrong must not be offered"
        );
    }

    let mode = view.control("hqplayer.pipeline.mode").expect("projected");
    assert_eq!(
        mode.offered_choice_ids(),
        vec!["source", "pcm", "sdm"],
        "the control being written is not itself invalidated by its own write"
    );
}

#[test]
fn a_settled_write_does_not_hold_its_dependents_in_reloading() {
    let mut document = pipeline();
    document.operations = vec![operation(
        "op-mode-1",
        "hqplayer.pipeline.mode",
        CommandOutcome::Applied,
    )];
    let view = rendered(document, &web());
    let filter = view.control("hqplayer.pipeline.filter_1x").expect("projected");
    assert!(
        matches!(filter.choices, ChoiceProjection::Enumerated { .. }),
        "once the outcome is state evidence the enumeration published beside it is current"
    );
}

#[test]
fn an_unreadable_enumeration_is_not_the_same_as_an_empty_one() {
    let mut unreadable = pipeline();
    control_mut(&mut unreadable, "hqplayer.pipeline.mode").choices = None;
    let view = rendered(unreadable, &web());
    match &view.control("hqplayer.pipeline.mode").expect("projected").choices {
        ChoiceProjection::Unknown { reason } => {
            assert_eq!(reason.code, ReasonCode::Unobservable)
        }
        other => panic!("an enumeration with no published set is unknown, not empty: {other:?}"),
    }

    let mut empty = pipeline();
    control_mut(&mut empty, "hqplayer.pipeline.mode").choices = Some(ChoiceSet {
        authority: Authority::Runtime,
        source: SourceClass::LiveProtocol,
        revision: None,
        choices: Vec::new(),
    });
    let view = rendered(empty, &web());
    match &view.control("hqplayer.pipeline.mode").expect("projected").choices {
        ChoiceProjection::Enumerated { choices, .. } => assert!(
            choices.is_empty(),
            "a successful read of an empty collection is a fact, and a different fact"
        ),
        other => panic!("a published empty set is enumerated, not unknown: {other:?}"),
    }
}

// =============================================================================
// Falsifying stale choice / operation races
// =============================================================================

#[test]
fn busy_state_is_scoped_per_operation_and_per_control() {
    let mut document = pipeline();
    document.operations = vec![
        operation("op-mode-1", "hqplayer.pipeline.mode", CommandOutcome::Pending),
        operation("op-vol-2", "hqplayer.volume.level", CommandOutcome::Applied),
    ];
    let view = rendered(document, &web());

    let mode = view.control("hqplayer.pipeline.mode").expect("projected");
    assert_eq!(mode.operations.len(), 1);
    assert_eq!(mode.operations[0].id.as_str(), "op-mode-1");
    assert!(mode.operations[0].awaits_convergence);
    assert!(!mode.operations[0].is_state_evidence);

    let volume = view.control("hqplayer.volume.level").expect("projected");
    assert_eq!(volume.operations.len(), 1);
    assert_eq!(volume.operations[0].id.as_str(), "op-vol-2");
    assert!(
        volume.operations[0].is_state_evidence,
        "an applied write is evidence about the producer; a pending one is not"
    );

    for control in &view.controls {
        for op in &control.operations {
            assert_eq!(
                op.correlation_id,
                format!("corr-{}", op.id.as_str()),
                "every surface follows one correlation id per operation, supplied by whoever \
                 submitted it"
            );
        }
    }
    let elsewhere: Vec<&str> = view
        .controls
        .iter()
        .filter(|control| {
            control.id.as_str() != "hqplayer.pipeline.mode"
                && control.id.as_str() != "hqplayer.volume.level"
        })
        .flat_map(|control| control.operations.iter().map(|op| op.id.as_str()))
        .collect();
    assert!(
        elsewhere.is_empty(),
        "one control's operation may never appear as another control's busy marker: {elsewhere:?}"
    );
}

#[test]
fn an_older_projection_is_distinguishable_from_a_newer_one() {
    let older = rendered(pipeline(), &web());

    let mut newer_document = pipeline();
    newer_document.revisions = DocumentRevisions::new(12, 4200);
    control_mut(&mut newer_document, "hqplayer.volume.level").values = pipeline()
        .control(&ControlId::new("hqplayer.volume.level"))
        .expect("present")
        .values
        .clone();
    let newer = rendered(newer_document, &web());

    assert!(
        newer.position.state > older.position.state,
        "a surface must be able to discard an older refresh rather than let it overwrite a newer \
         result"
    );
    assert_eq!(
        newer.position.epoch, older.position.epoch,
        "revisions are only comparable within one epoch"
    );
}

#[test]
fn every_control_in_one_projection_comes_from_one_document() {
    // The mechanical form of "never combine two polls": each projected observed value must equal
    // the value in the document the projection cites, for the document it cites.
    let document = pipeline();
    let expected: BTreeMap<String, Option<ControlValue>> = document
        .controls
        .iter()
        .map(|control| {
            (
                control.id.to_string(),
                control.observed().cloned(),
            )
        })
        .collect();
    let view = rendered(document, &web());

    for control in &view.controls {
        let projected = control
            .observed
            .as_ref()
            .and_then(|value| value.grounded_value())
            .cloned();
        assert_eq!(
            projected,
            expected[control.id.as_str()],
            "{}: projected observed value does not match the cited document",
            control.id
        );
    }
}

// =============================================================================
// Falsifying budget behaviour on a small device
// =============================================================================

#[test]
fn a_bounded_device_gets_transport_and_volume_first_and_says_what_it_dropped() {
    let view = rendered(pipeline(), &device(6));

    let ids: Vec<&str> = view.controls.iter().map(|c| c.id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "hqplayer.transport.play",
            "hqplayer.transport.pause",
            "hqplayer.transport.state",
            "hqplayer.volume.level",
            "hqplayer.volume.fixed",
            "hqplayer.volume.fixed.enabled",
        ],
        "the profile ranks transport and volume ahead of tuning; the producer knows nothing about \
         that ordering"
    );
    assert!(
        view.notices.iter().any(|notice| matches!(
            notice,
            ProjectionNotice::Truncated { budget: 6, advertised } if *advertised == 16
        )),
        "truncation is declared, never silent: {:?}",
        view.notices
    );
    assert_eq!(view.omitted.len(), 10);
    let omitted = view
        .omitted_control("hqplayer.pipeline.mode")
        .expect("dropped controls are named");
    assert_eq!(omitted.display_text_key, OMITTED_BUDGET_EXHAUSTED);
}

#[test]
fn dropping_a_container_drops_its_members_for_the_containers_reason() {
    let view = rendered(pipeline(), &device(4));
    let ids: Vec<&str> = view.controls.iter().map(|c| c.id.as_str()).collect();
    assert!(
        !ids.contains(&"hqplayer.volume.fixed"),
        "the container did not fit"
    );
    for member in [
        "hqplayer.volume.fixed.enabled",
        "hqplayer.volume.fixed.level",
    ] {
        let omitted = view
            .omitted_control(member)
            .unwrap_or_else(|| panic!("{member} must be reported as omitted"));
        assert_eq!(
            omitted.display_text_key, OMITTED_CONTAINER_DROPPED,
            "an orphaned member is worse than a missing one: the surface would render a coupled \
             half with no container"
        );
    }
}

#[test]
fn narrowing_an_option_list_never_hides_the_current_selection() {
    let mut document = pipeline();
    // Twelve options, with the engine's current one last, so a naive head-truncation drops it.
    let mut choices: Vec<unified_hifi_control::adaptive::Choice> = (0..11)
        .map(|index| {
            unified_hifi_control::adaptive::Choice::new(
                format!("filter-{index}"),
                format!("Filter {index}"),
            )
        })
        .collect();
    choices.push(unified_hifi_control::adaptive::Choice::new(
        "poly-sinc-gauss-long",
        "poly-sinc-gauss-long",
    ));
    control_mut(&mut document, "hqplayer.pipeline.filter_1x").choices = Some(ChoiceSet {
        authority: Authority::Runtime,
        source: SourceClass::LiveProtocol,
        revision: None,
        choices,
    });

    let view = rendered(document, &device(64));
    let filter = view.control("hqplayer.pipeline.filter_1x").expect("projected");
    let offered = filter.offered_choice_ids();
    assert!(
        offered.contains(&"poly-sinc-gauss-long"),
        "the running selection must survive narrowing, or the surface shows a control whose value \
         is not among its options: {offered:?}"
    );
    match &filter.choices {
        ChoiceProjection::Enumerated { truncated, .. } => assert_eq!(
            *truncated,
            Some(4),
            "and the surface must be told the list was shortened"
        ),
        other => panic!("expected an enumerated set, got {other:?}"),
    }
}

// =============================================================================
// Falsifying MCP discoverability
// =============================================================================

#[test]
fn every_operation_mcp_may_invoke_is_fully_described_without_guessing() {
    let view = rendered(pipeline(), &mcp());
    let mut mutable = 0usize;

    for control in &view.controls {
        let Mutability::Mutable { description, .. } = &control.mutability else {
            continue;
        };
        mutable += 1;
        assert!(
            control.availability.permits_mutation(),
            "{}: advertised as invokable while the producer forbids mutation",
            control.id
        );
        assert_eq!(
            description.timing_key,
            timing_key(&description.effect),
            "{}: an MCP description must state the real timing",
            control.id
        );
        assert_ne!(
            description.timing_key, TIMING_UNKNOWN,
            "{}: an effect this build cannot describe must not be advertised",
            control.id
        );
        match control.kind {
            ControlKind::Enumeration => assert!(
                !control.offered_choice_ids().is_empty(),
                "{}: an enumeration offered for mutation with no options makes a caller guess",
                control.id
            ),
            ControlKind::NumericRange => assert!(
                control.range.is_some(),
                "{}: a numeric control offered without a domain makes a caller guess",
                control.id
            ),
            _ => {}
        }
    }
    assert!(
        mutable >= 5,
        "the fixture must actually exercise this: only {mutable} mutable controls reached MCP"
    );
}

#[test]
fn live_persistent_and_restart_behaviour_are_described_distinctly() {
    let view = rendered(pipeline(), &web());
    let described: BTreeMap<&str, &'static str> = view
        .controls
        .iter()
        .filter_map(|control| match &control.mutability {
            Mutability::Mutable { description, .. } => {
                Some((control.id.as_str(), description.timing_key))
            }
            _ => None,
        })
        .collect();

    assert_eq!(
        described.get("hqplayer.pipeline.mode"),
        Some(&TIMING_LIVE_IMMEDIATE)
    );
    assert_eq!(
        described.get("hqplayer.volume.level"),
        Some(&TIMING_VERIFIED_PENDING)
    );
    assert_eq!(
        described.get("hqplayer.settings.buffer_time"),
        Some(&TIMING_RESTART_REQUIRED),
        "a stored-until-restart control described as live is the failure this key exists to stop"
    );
    assert_eq!(
        described.get("hqplayer.settings.legacy_dither"),
        Some(&TIMING_PERSISTENT_ONLY)
    );
    assert_eq!(
        described.get("hqplayer.volume.fixed.enabled"),
        Some(&TIMING_LIVE_IMMEDIATE)
    );

    let mut distinct: Vec<&'static str> = described.values().copied().collect();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(
        distinct.len() >= 4,
        "the five apply effects must not collapse into one wording: {distinct:?}"
    );
}

#[test]
fn every_apply_effect_has_its_own_timing_key() {
    let mut keys: Vec<&'static str> = ApplyEffect::known().iter().map(timing_key).collect();
    let total = keys.len();
    keys.sort_unstable();
    keys.dedup();
    assert_eq!(
        keys.len(),
        total,
        "two effects sharing a key would let a restart-required control borrow live wording"
    );
    assert_eq!(
        timing_key(&ApplyEffect::Unrecognized("teleported".to_string())),
        TIMING_UNKNOWN,
        "an effect from a newer minor must not silently borrow a recognized wording"
    );
    assert_eq!(
        timing_key(&ApplyEffect::HeldUntilChainLoaded),
        TIMING_HELD_UNTIL_CHAIN_LOADED
    );
}

#[test]
fn a_provisional_acknowledgement_is_never_described_as_proof() {
    let view = rendered(pipeline(), &web());
    let volume = view.control("hqplayer.volume.level").expect("projected");
    match &volume.mutability {
        Mutability::Mutable { description, .. } => assert!(
            !description.acknowledgement_is_evidence,
            "the producer marks this acknowledgement provisional; a surface that treats a returned \
             call as proof will show a value the engine never adopted"
        ),
        other => panic!("volume must be mutable on the web, got {other:?}"),
    }
    let mode = view.control("hqplayer.pipeline.mode").expect("projected");
    match &mode.mutability {
        Mutability::Mutable { description, .. } => {
            assert!(description.acknowledgement_is_evidence)
        }
        other => panic!("mode must be mutable on the web, got {other:?}"),
    }
}

#[test]
fn coupled_controls_are_advertised_as_one_plan() {
    let view = rendered(pipeline(), &web());
    let enabled = view.control("hqplayer.volume.fixed.enabled").expect("projected");
    match &enabled.mutability {
        Mutability::Mutable { description, .. } => {
            assert_eq!(
                description.coupled_with,
                vec![ControlId::new("hqplayer.volume.fixed.level")],
                "coupling is the producer's job; a consumer issuing the extra write itself is how \
                 a level edit silently enables a feature nobody asked for"
            );
            assert_eq!(description.lane, ApplyLane::Composite);
        }
        other => panic!("expected a mutable coupled control, got {other:?}"),
    }
}

// =============================================================================
// Falsifying capability routing
// =============================================================================

/// A settled aggregator holding exactly `documents`, plus the live run leases that admitted them.
///
/// The leases are returned rather than dropped because presence is evaluated at read time against
/// the admitting run: dropping them would make every producer `LastKnown`, which is a different
/// scenario from the one these tests are about.
async fn service_with(
    documents: Vec<ProducerDocument>,
) -> (
    ControlPlaneService,
    Vec<unified_hifi_control::producers::AdapterRun>,
) {
    let (handle, actor, view) =
        AdaptiveRuntime::build(tokio_util::sync::CancellationToken::new(), 64);
    // The actor must be running before `publish` awaits its verdict, and joined before the service
    // reads, so every test sees a settled aggregator rather than a racing one.
    let running = tokio::spawn(actor.run());
    let mut runs: BTreeMap<String, unified_hifi_control::producers::AdapterRun> = BTreeMap::new();
    for document in documents {
        let adapter = document
            .producer
            .producer_id
            .split(':')
            .next()
            .expect("a producer id has a namespace")
            .to_string();
        let run = runs
            .entry(adapter.clone())
            .or_insert_with(|| handle.begin_run(&adapter));
        handle
            .publish(run, document)
            .await
            .expect("the actor accepts the publication");
    }
    drop(handle);
    tokio::time::timeout(std::time::Duration::from_secs(5), running)
        .await
        .expect("the actor drains once its channel closes")
        .expect("the actor task must not panic");
    (
        ControlPlaneService::new(view),
        runs.into_values().collect(),
    )
}

fn hqplayer_zone_document(zone: &str) -> ProducerDocument {
    let mut document = pipeline();
    document.target.zone_id = Some(zone.to_string());
    document.target.role = TargetRole::Playback;
    document
}

#[tokio::test]
async fn an_unknown_hqplayer_identity_is_refused_rather_than_routed_anywhere_else() {
    // The #328 defect class, expressed at the control plane: the only producer present is bound to
    // a Roon zone, and a `hqplayer:` id that names nothing must not reach it.
    let (service, _runs) = service_with(vec![pipeline()]).await;

    match service.route("hqplayer:Ghost Room") {
        Err(RouteRefusal::UnknownProducer { zone_id }) => {
            assert_eq!(zone_id, "hqplayer:Ghost Room")
        }
        other => panic!("an unknown HQPlayer identity must refuse, got {other:?}"),
    }
    assert!(
        service.route("roon:1601bb42ed14351b99c2926214f6cbb80724").is_ok(),
        "the producer that does exist still routes"
    );
}

#[tokio::test]
async fn an_unprefixed_zone_id_never_resolves() {
    let (service, _runs) = service_with(vec![pipeline()]).await;
    match service.route("Living Room") {
        Err(RouteRefusal::UnknownZonePrefix { zone_id }) => assert_eq!(zone_id, "Living Room"),
        other => panic!("an unprefixed id is not a zone id, got {other:?}"),
    }
}

#[tokio::test]
async fn two_producers_claiming_one_zone_refuse_rather_than_pick() {
    let (service, _runs) = service_with(vec![
        pipeline(),
        hqplayer_zone_document("roon:1601bb42ed14351b99c2926214f6cbb80724"),
    ])
    .await;

    match service.route("roon:1601bb42ed14351b99c2926214f6cbb80724") {
        Err(RouteRefusal::AmbiguousProducer { candidates, .. }) => {
            assert_eq!(candidates.len(), 2, "both claimants must be named")
        }
        other => panic!("picking one silently is how a command lands on the wrong role: {other:?}"),
    }
}

#[tokio::test]
async fn routing_reads_only_admitted_state() {
    let (service, _runs) = service_with(Vec::new()).await;
    assert!(service.producers().is_empty());
    assert!(matches!(
        service.route("hqplayer:Living Room"),
        Err(RouteRefusal::UnknownProducer { .. })
    ));
}

// =============================================================================
// Falsifying command dispatch
// =============================================================================

#[derive(Default)]
struct RecordingExecutor {
    producer_type: String,
    seen: Mutex<Vec<SemanticCommand>>,
}

impl RecordingExecutor {
    fn new(producer_type: &str) -> Arc<Self> {
        Arc::new(Self {
            producer_type: producer_type.to_string(),
            seen: Mutex::new(Vec::new()),
        })
    }

    fn commands(&self) -> Vec<SemanticCommand> {
        self.seen.lock().expect("lock").clone()
    }
}

#[async_trait]
impl SemanticCommandExecutor for RecordingExecutor {
    fn producer_type(&self) -> &str {
        &self.producer_type
    }

    async fn submit(&self, command: SemanticCommand) -> Result<CommandAcceptance, CommandRefusal> {
        self.seen.lock().expect("lock").push(command.clone());
        Ok(CommandAcceptance {
            operation: operation(
                "op-accepted",
                command.control.as_str(),
                CommandOutcome::Pending,
            ),
            duplicate: false,
        })
    }
}

fn command_for(key: &ProducerKey, position: RevisionRef) -> SemanticCommand {
    SemanticCommand {
        key: key.clone(),
        expected: position,
        control: ControlId::new("hqplayer.pipeline.mode"),
        requested: ControlValue::Choice("pcm".to_string()),
        lane: ApplyLane::Immediate,
        correlation_id: "corr-web-1".to_string(),
        surface_id: "web".to_string(),
    }
}

#[tokio::test]
async fn a_command_reaches_the_executor_that_owns_the_routed_producer() {
    let executor = RecordingExecutor::new("hqplayer");
    let (service, _runs) = service_with(vec![pipeline()]).await;
    let service = service.with_executor(executor.clone());
    let key = ProducerKey::of(&pipeline());
    let position = pipeline().position();

    let acceptance = service
        .submit(command_for(&key, position))
        .await
        .expect("the HQPlayer executor is registered");
    assert!(!acceptance.duplicate);
    let seen = executor.commands();
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].key, key);
    assert_eq!(seen[0].correlation_id, "corr-web-1");
}

#[tokio::test]
async fn a_producer_family_with_no_executor_is_refused_not_defaulted() {
    // The registered executor belongs to another family. Handing it the command anyway is the #328
    // fallthrough with a different prefix.
    let executor = RecordingExecutor::new("roon");
    let (service, _runs) = service_with(vec![pipeline()]).await;
    let service = service.with_executor(executor.clone());
    let key = ProducerKey::of(&pipeline());
    let position = pipeline().position();

    match service.submit(command_for(&key, position)).await {
        Err(CommandRefusal::NoExecutorForProducer { producer_type }) => {
            assert_eq!(producer_type, "hqplayer")
        }
        other => panic!("expected a refusal naming the family, got {other:?}"),
    }
    assert!(
        executor.commands().is_empty(),
        "the wrong executor must never be handed the command"
    );
}

#[tokio::test]
async fn a_command_for_a_producer_the_aggregator_does_not_hold_is_refused() {
    let executor = RecordingExecutor::new("hqplayer");
    let (service, _runs) = service_with(Vec::new()).await;
    let service = service.with_executor(executor.clone());
    let key = ProducerKey {
        producer_id: "hqplayer:ghost".to_string(),
        role: TargetRole::Playback,
        zone_id: Some("hqplayer:ghost".to_string()),
    };

    match service
        .submit(command_for(
            &key,
            RevisionRef {
                epoch: ProducerEpoch(1),
                control_plane: unified_hifi_control::adaptive::Revision(1),
                state: unified_hifi_control::adaptive::Revision(1),
            },
        ))
        .await
    {
        Err(CommandRefusal::Route(RouteRefusal::UnknownProducer { .. })) => {}
        other => panic!("a command may only address admitted state, got {other:?}"),
    }
    assert!(executor.commands().is_empty());
}

#[tokio::test]
async fn a_projection_and_a_command_agree_about_the_position_to_fence_on() {
    let executor = RecordingExecutor::new("hqplayer");
    let (service, _runs) = service_with(vec![pipeline()]).await;
    let service = service.with_executor(executor.clone());
    let key = ProducerKey::of(&pipeline());

    let projection = service
        .project(&key, &web())
        .expect("the producer is admitted");
    let view = projection.rendered().expect("rendered");

    service
        .submit(command_for(&key, view.position))
        .await
        .expect("accepted");
    assert_eq!(
        executor.commands()[0].expected,
        view.position,
        "the position a surface renders is the position its command fences on; anything else lets \
         a stale surface authorise a write against a producer that moved"
    );
}
