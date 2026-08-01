//! Pure, advisory admission checks for HQPlayer's immediate adaptive commands.
//!
//! This module deliberately knows only an admitted producer snapshot. Reserving a command,
//! native execution, and outcome publication belong to the HQPlayer publisher coordinator; a
//! successful result here is therefore never permission to write by itself.

use crate::adaptive::{
    ApplyLane, Choice, ControlId, ControlKind, ControlValue, RevisionRef, TransportLane,
};

use super::hqplayer::{hqp_semantic_operation, HqpSemanticOperation};
use super::{ProducerKey, ProducerPresence, ProducerSnapshot};

/// One surface's proposed immediate semantic HQPlayer mutation.
///
/// The coordinator repeats these checks while reserving the command, so this request carries a
/// complete citable snapshot position instead of relying on a mutable cache or a native index.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HqpImmediateCommandRequest {
    /// Exact aggregate producer identity addressed by the surface.
    pub key: ProducerKey,
    /// Epoch and revisions observed by the caller.
    pub expected: RevisionRef,
    /// Semantic control to mutate.
    pub control: ControlId,
    /// Desired semantic control value.
    pub requested: ControlValue,
    /// This first path accepts only immediate writes.
    pub lane: ApplyLane,
    /// Caller supplied idempotency correlation. It is opaque to this advisory layer.
    pub correlation_id: String,
}

/// A deterministic semantic identity for one request.
///
/// It deliberately omits a correlation id: the coordinator uses the correlation *with* this
/// fingerprint to distinguish a retry from a conflicting reuse of the same correlation.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct HqpCommandFingerprint(Vec<u8>);

const MAX_CORRELATION_ID_BYTES: usize = 256;

/// Validated value forwarded to the typed HQPlayer executor.
///
/// It contains an engine *name* or exact rate, never an enumerated native index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HqpEngineValue {
    /// An exact engine supplied mode/filter/shaper name.
    Name(String),
    /// An exact integer output rate parsed from the engine-supplied rate name.
    Rate(u32),
}

/// A request that is safe to offer to the publisher coordinator for atomic reservation.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HqpValidatedCommand {
    /// Original semantic request; the coordinator records this intent verbatim.
    pub request: HqpImmediateCommandRequest,
    /// The registered typed operation, not a consumer-owned string switch.
    pub operation: HqpSemanticOperation,
    /// The exact engine value selected from the current runtime enumeration.
    pub engine_value: HqpEngineValue,
    /// Identity used with correlation id for at-most-once reservation.
    pub fingerprint: HqpCommandFingerprint,
}

/// A typed refusal before command reservation or native I/O.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HqpPreflightRefusal {
    /// An empty correlation cannot identify a retry.
    CorrelationIdEmpty,
    /// Correlations are retained for the lifetime of a producer run, so bound caller input.
    CorrelationIdTooLong {
        /// Maximum accepted UTF-8 byte length.
        max_bytes: usize,
        /// Supplied UTF-8 byte length.
        actual_bytes: usize,
    },
    /// The selected snapshot belongs to another producer or target role.
    ProducerMismatch,
    /// The caller's position is no longer the current atomic producer position.
    RevisionMismatch {
        /// Position the caller used.
        expected: RevisionRef,
        /// Position currently served by the aggregator.
        actual: RevisionRef,
    },
    /// Last-known data is never a write target.
    ProducerNotLive,
    /// The producer itself labels this otherwise-live document stale.
    ProducerStale,
    /// No native control lane is represented by the document.
    NativeLaneMissing,
    /// Native control is not connected.
    NativeLaneUnavailable,
    /// Native control readings are no longer current enough to mutate from.
    NativeLaneStale,
    /// The control is absent from the exact document.
    ControlUnknown,
    /// The control exists but is not presently offered for mutation.
    ControlUnavailable,
    /// The control has no apply semantics or is otherwise not mutable.
    ControlReadOnly,
    /// The request selected a write lane different from the descriptor's lane.
    WrongApplyLane,
    /// This initial service only executes `immediate` requests.
    NonImmediateRequest,
    /// The first pipeline slice requires one semantic enumeration choice.
    WrongControlKind,
    /// The requested value is not a semantic choice id.
    WrongValueKind,
    /// The semantic choice is not in the exact current runtime enumeration.
    UnknownChoice,
    /// A choice is known but currently disabled by the producer.
    ChoiceUnavailable,
    /// The runtime choice lacks an engine name, so it cannot be sent truthfully.
    ChoiceMissingEngineName,
    /// The engine rate spelling is not the exact non-negative integer expected by `set_rate`.
    InvalidRateEngineName,
    /// A mutable HQPlayer operation deliberately held for #328 until it has a truthful receipt.
    DeferredToIssue328 {
        /// The registered semantic operation which has been deferred.
        operation: HqpSemanticOperation,
    },
    /// The document contains a control the HQPlayer registry does not make executable.
    UnsupportedControl,
}

/// Check one already-admitted snapshot without mutating any retained state.
///
/// This is deliberately advisory. The publisher coordinator must repeat the exact identity,
/// revision, and actionability checks while atomically recording `Pending` before execution.
pub(crate) fn preflight(
    snapshot: &ProducerSnapshot,
    request: &HqpImmediateCommandRequest,
) -> Result<HqpValidatedCommand, HqpPreflightRefusal> {
    validate_correlation_id(&request.correlation_id)?;
    if snapshot.key != request.key {
        return Err(HqpPreflightRefusal::ProducerMismatch);
    }

    let actual = snapshot.document.position();
    if actual != request.expected {
        return Err(HqpPreflightRefusal::RevisionMismatch {
            expected: request.expected,
            actual,
        });
    }
    if snapshot.presence != ProducerPresence::Live {
        return Err(HqpPreflightRefusal::ProducerNotLive);
    }
    if snapshot.document.stale {
        return Err(HqpPreflightRefusal::ProducerStale);
    }

    let native = snapshot
        .document
        .lane_health(&TransportLane::Native)
        .ok_or(HqpPreflightRefusal::NativeLaneMissing)?;
    if native.state != crate::adaptive::LaneState::Connected {
        return Err(HqpPreflightRefusal::NativeLaneUnavailable);
    }
    if native.freshness.stale {
        return Err(HqpPreflightRefusal::NativeLaneStale);
    }

    if request.lane != ApplyLane::Immediate {
        return Err(HqpPreflightRefusal::NonImmediateRequest);
    }

    let operation =
        hqp_semantic_operation(&request.control).ok_or(HqpPreflightRefusal::UnsupportedControl)?;
    if matches!(
        operation,
        HqpSemanticOperation::Play
            | HqpSemanticOperation::Pause
            | HqpSemanticOperation::Stop
            | HqpSemanticOperation::Previous
            | HqpSemanticOperation::Next
            | HqpSemanticOperation::SetVolumeDb
    ) {
        return Err(HqpPreflightRefusal::DeferredToIssue328 { operation });
    }

    let control = snapshot
        .document
        .control(&request.control)
        .ok_or(HqpPreflightRefusal::ControlUnknown)?;
    if !control.availability.permits_mutation() {
        return Err(HqpPreflightRefusal::ControlUnavailable);
    }
    if !control.is_mutable() {
        return Err(HqpPreflightRefusal::ControlReadOnly);
    }
    let Some(apply) = control.apply.as_ref() else {
        return Err(HqpPreflightRefusal::ControlReadOnly);
    };
    if apply.lane != request.lane {
        return Err(HqpPreflightRefusal::WrongApplyLane);
    }
    if control.kind != ControlKind::Enumeration {
        return Err(HqpPreflightRefusal::WrongControlKind);
    }

    let ControlValue::Choice(requested_choice) = &request.requested else {
        return Err(HqpPreflightRefusal::WrongValueKind);
    };
    let choice = control
        .choices
        .as_ref()
        .and_then(|choices| {
            choices
                .choices
                .iter()
                .find(|choice| choice.id == *requested_choice)
        })
        .ok_or(HqpPreflightRefusal::UnknownChoice)?;
    if !choice_is_mutable(choice) {
        return Err(HqpPreflightRefusal::ChoiceUnavailable);
    }
    let engine_name = choice
        .engine_name
        .as_deref()
        .filter(|name| !name.is_empty())
        .ok_or(HqpPreflightRefusal::ChoiceMissingEngineName)?;

    let engine_value = match operation {
        HqpSemanticOperation::SetMode
        | HqpSemanticOperation::SetFilter1x
        | HqpSemanticOperation::SetFilterNx
        | HqpSemanticOperation::SetShaper => HqpEngineValue::Name(engine_name.to_string()),
        HqpSemanticOperation::SetRate => HqpEngineValue::Rate(
            engine_name
                .parse::<u32>()
                .map_err(|_| HqpPreflightRefusal::InvalidRateEngineName)?,
        ),
        HqpSemanticOperation::Play
        | HqpSemanticOperation::Pause
        | HqpSemanticOperation::Stop
        | HqpSemanticOperation::Previous
        | HqpSemanticOperation::Next
        | HqpSemanticOperation::SetVolumeDb => {
            return Err(HqpPreflightRefusal::DeferredToIssue328 { operation });
        }
    };

    Ok(HqpValidatedCommand {
        request: request.clone(),
        operation,
        engine_value,
        fingerprint: fingerprint(request),
    })
}

fn choice_is_mutable(choice: &Choice) -> bool {
    choice
        .availability
        .as_ref()
        .is_none_or(|availability| availability.permits_mutation())
}

pub(crate) fn fingerprint(request: &HqpImmediateCommandRequest) -> HqpCommandFingerprint {
    // The correlation id is deliberately not part of the fingerprint: the coordinator indexes by
    // correlation and compares this exact semantic identity to distinguish retry from reuse.
    // Every variable-width value is length-delimited and every sum variant has its own tag, so
    // distinct requests cannot alias because of separators, field boundaries, or value types.
    let mut canonical = Vec::new();
    canonical.extend_from_slice(b"hqp-immediate-command-v1\0");
    append_bytes(&mut canonical, request.key.producer_id.as_bytes());
    append_bytes(&mut canonical, request.key.role.as_str().as_bytes());
    append_optional_bytes(
        &mut canonical,
        request.key.zone_id.as_deref().map(str::as_bytes),
    );
    canonical.extend_from_slice(&request.expected.epoch.0.to_be_bytes());
    canonical.extend_from_slice(&request.expected.control_plane.0.to_be_bytes());
    canonical.extend_from_slice(&request.expected.state.0.to_be_bytes());
    append_bytes(&mut canonical, request.control.as_str().as_bytes());
    append_bytes(&mut canonical, request.lane.as_str().as_bytes());
    append_control_value(&mut canonical, &request.requested);
    HqpCommandFingerprint(canonical)
}

pub(crate) fn validate_correlation_id(correlation_id: &str) -> Result<(), HqpPreflightRefusal> {
    if correlation_id.is_empty() {
        return Err(HqpPreflightRefusal::CorrelationIdEmpty);
    }
    let actual_bytes = correlation_id.len();
    if actual_bytes > MAX_CORRELATION_ID_BYTES {
        return Err(HqpPreflightRefusal::CorrelationIdTooLong {
            max_bytes: MAX_CORRELATION_ID_BYTES,
            actual_bytes,
        });
    }
    Ok(())
}

fn append_optional_bytes(output: &mut Vec<u8>, value: Option<&[u8]>) {
    match value {
        None => output.push(0),
        Some(value) => {
            output.push(1);
            append_bytes(output, value);
        }
    }
}

fn append_bytes(output: &mut Vec<u8>, value: &[u8]) {
    output.extend_from_slice(&(value.len() as u64).to_be_bytes());
    output.extend_from_slice(value);
}

fn append_control_value(output: &mut Vec<u8>, value: &ControlValue) {
    match value {
        ControlValue::Empty => output.push(0),
        ControlValue::Bool(value) => {
            output.push(1);
            output.push(u8::from(*value));
        }
        ControlValue::Integer(value) => {
            output.push(2);
            output.extend_from_slice(&value.to_be_bytes());
        }
        ControlValue::Decimal(value) => {
            output.push(3);
            output.extend_from_slice(&value.to_bits().to_be_bytes());
        }
        ControlValue::Text(value) => {
            output.push(4);
            append_bytes(output, value.as_bytes());
        }
        ControlValue::Choice(value) => {
            output.push(5);
            append_bytes(output, value.as_bytes());
        }
        ControlValue::Composite(values) => {
            output.push(6);
            output.extend_from_slice(&(values.len() as u64).to_be_bytes());
            for (key, value) in values {
                append_bytes(output, key.as_bytes());
                append_control_value(output, value);
            }
        }
        ControlValue::List(values) => {
            output.push(7);
            output.extend_from_slice(&(values.len() as u64).to_be_bytes());
            for value in values {
                append_control_value(output, value);
            }
        }
        ControlValue::Unrecognized { kind, raw } => {
            output.push(8);
            append_bytes(output, kind.as_bytes());
            append_json_value(output, raw);
        }
    }
}

fn append_json_value(output: &mut Vec<u8>, value: &serde_json::Value) {
    match value {
        serde_json::Value::Null => output.push(0),
        serde_json::Value::Bool(value) => {
            output.push(1);
            output.push(u8::from(*value));
        }
        serde_json::Value::Number(value) => {
            output.push(2);
            append_bytes(output, value.to_string().as_bytes());
        }
        serde_json::Value::String(value) => {
            output.push(3);
            append_bytes(output, value.as_bytes());
        }
        serde_json::Value::Array(values) => {
            output.push(4);
            output.extend_from_slice(&(values.len() as u64).to_be_bytes());
            for value in values {
                append_json_value(output, value);
            }
        }
        serde_json::Value::Object(values) => {
            output.push(5);
            output.extend_from_slice(&(values.len() as u64).to_be_bytes());
            let mut entries = values.iter().collect::<Vec<_>>();
            entries.sort_unstable_by_key(|(left, _)| *left);
            for (key, value) in entries {
                append_bytes(output, key.as_bytes());
                append_json_value(output, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::*;
    use crate::adaptive::{
        ApplyEffect, ApplySemantics, Availability, ChoiceSet, Control, Disruption,
        DocumentRevisions, Extensions, Freshness, LaneHealth, LaneState, ProducerDocument,
        ProducerEpoch, ProducerIdentity, ProducerTarget, RiskClass, TargetRole, Timestamp,
        Verification, CONSUMER_SCHEMA_VERSION,
    };
    use crate::producers::ProducerSnapshot;

    const MODE: &str = "hqplayer.pipeline.mode";
    const FILTER_1X: &str = "hqplayer.pipeline.filter_1x";
    const FILTER_NX: &str = "hqplayer.pipeline.filter_nx";
    const SHAPER: &str = "hqplayer.pipeline.shaper";
    const RATE: &str = "hqplayer.pipeline.rate";

    fn snapshot() -> ProducerSnapshot {
        let document = ProducerDocument {
            schema_version: CONSUMER_SCHEMA_VERSION,
            producer: ProducerIdentity {
                producer_id: "hqplayer:main".to_string(),
                producer_type: "hqplayer".to_string(),
                instance_label: None,
                product_version: None,
                epoch: ProducerEpoch(7),
                extensions: Extensions::default(),
            },
            target: ProducerTarget {
                role: TargetRole::DspEngine,
                zone_id: None,
                extensions: Extensions::default(),
            },
            revisions: DocumentRevisions::new(3, 9),
            lanes: vec![LaneHealth {
                lane: TransportLane::Native,
                state: LaneState::Connected,
                last_success: Some(Timestamp::new("2026-07-31T00:00:00Z")),
                last_error: None,
                freshness: Freshness::default(),
                extensions: Extensions::default(),
            }],
            groups: Vec::new(),
            controls: vec![
                selection(MODE, "PCM"),
                selection(FILTER_1X, "poly-sinc-gauss"),
                selection(FILTER_NX, "poly-sinc-xtr"),
                selection(SHAPER, "NS9"),
                selection(RATE, "192000"),
            ],
            constraints: Vec::new(),
            draft_policy: None,
            change_sets: Vec::new(),
            operations: Vec::new(),
            stale: false,
            extensions: Extensions::default(),
        };
        let key = ProducerKey::of(&document);
        ProducerSnapshot {
            key,
            document: Arc::new(document),
            lanes: BTreeMap::new(),
            presence: ProducerPresence::Live,
            repairs: Vec::new(),
            last_refusal: None,
        }
    }

    fn selection(id: &str, engine_name: &str) -> Control {
        Control {
            id: ControlId::new(id),
            kind: ControlKind::Enumeration,
            label_key: None,
            description_key: None,
            group: None,
            order: None,
            widget_hint: None,
            unit: None,
            values: Vec::new(),
            choices: Some(ChoiceSet::runtime(vec![Choice::new(
                format!("{id}.engine.{}", hex(engine_name)),
                engine_name,
            )])),
            range: None,
            availability: Availability::available(),
            apply: Some(ApplySemantics {
                lane: ApplyLane::Immediate,
                effect: ApplyEffect::LiveImmediate,
                disruption: Disruption::NoDisruption,
                risk: RiskClass::Safe,
                verification: Verification::own_observed(),
                invalidates: Vec::new(),
                coupled_with: Vec::new(),
            }),
            divergences: Vec::new(),
            members: Vec::new(),
            extensions: Extensions::default(),
        }
    }

    fn hex(value: &str) -> String {
        value
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    fn request(
        snapshot: &ProducerSnapshot,
        control: &str,
        choice: &str,
    ) -> HqpImmediateCommandRequest {
        HqpImmediateCommandRequest {
            key: snapshot.key.clone(),
            expected: snapshot.document.position(),
            control: ControlId::new(control),
            requested: ControlValue::choice(choice),
            lane: ApplyLane::Immediate,
            correlation_id: "gesture-1".to_string(),
        }
    }

    #[test]
    fn resolves_a_current_pipeline_choice_to_its_engine_name() {
        let snapshot = snapshot();
        let choice = "hqplayer.pipeline.mode.engine.50434d";

        let validated = preflight(&snapshot, &request(&snapshot, MODE, choice))
            .expect("the current semantic choice is invokable");

        assert_eq!(validated.operation, HqpSemanticOperation::SetMode);
        assert_eq!(
            validated.engine_value,
            HqpEngineValue::Name("PCM".to_string())
        );
    }

    #[test]
    fn parses_rate_only_from_the_current_engine_choice_name() {
        let snapshot = snapshot();
        let choice = "hqplayer.pipeline.rate.engine.313932303030";

        let validated = preflight(&snapshot, &request(&snapshot, RATE, choice))
            .expect("current numeric engine spelling is invokable");

        assert_eq!(validated.engine_value, HqpEngineValue::Rate(192_000));
    }

    #[test]
    fn every_first_slice_pipeline_control_resolves_through_the_shared_registry() {
        let snapshot = snapshot();
        for (control, choice, operation, engine) in [
            (MODE, "PCM", HqpSemanticOperation::SetMode, "PCM"),
            (
                FILTER_1X,
                "poly-sinc-gauss",
                HqpSemanticOperation::SetFilter1x,
                "poly-sinc-gauss",
            ),
            (
                FILTER_NX,
                "poly-sinc-xtr",
                HqpSemanticOperation::SetFilterNx,
                "poly-sinc-xtr",
            ),
            (SHAPER, "NS9", HqpSemanticOperation::SetShaper, "NS9"),
        ] {
            let choice = format!("{control}.engine.{}", hex(choice));
            let validated = preflight(&snapshot, &request(&snapshot, control, &choice))
                .unwrap_or_else(|error| panic!("{control} did not preflight: {error:?}"));
            assert_eq!(validated.operation, operation);
            assert_eq!(
                validated.engine_value,
                HqpEngineValue::Name(engine.to_string())
            );
        }
    }

    #[test]
    fn refuses_a_stale_revision_before_a_choice_can_be_resolved() {
        let snapshot = snapshot();
        let mut request = request(&snapshot, MODE, "hqplayer.pipeline.mode.engine.50434d");
        request.expected.state.0 -= 1;

        assert!(matches!(
            preflight(&snapshot, &request),
            Err(HqpPreflightRefusal::RevisionMismatch { .. })
        ));
    }

    #[test]
    fn defers_transport_and_volume_through_a_typed_policy() {
        let snapshot = snapshot();
        for control in ["hqplayer.transport.play", "hqplayer.volume.level"] {
            let mut request = request(&snapshot, control, "not-used");
            request.requested = ControlValue::Empty;
            assert!(matches!(
                preflight(&snapshot, &request),
                Err(HqpPreflightRefusal::DeferredToIssue328 { .. })
            ));
        }
    }

    #[test]
    fn refuses_unavailable_choices_and_stale_native_state() {
        let mut snapshot = snapshot();
        Arc::make_mut(&mut snapshot.document).lanes[0]
            .freshness
            .stale = true;
        assert_eq!(
            preflight(
                &snapshot,
                &request(&snapshot, MODE, "hqplayer.pipeline.mode.engine.50434d"),
            ),
            Err(HqpPreflightRefusal::NativeLaneStale)
        );

        Arc::make_mut(&mut snapshot.document).lanes[0]
            .freshness
            .stale = false;
        Arc::make_mut(&mut snapshot.document).controls[0]
            .choices
            .as_mut()
            .unwrap()
            .choices[0]
            .availability = Some(Availability::unavailable(crate::adaptive::Reason {
            code: crate::adaptive::ReasonCode::LockedByProducer,
            scope: crate::adaptive::ReasonScope::Producer,
            display_text_key: None,
            detail: None,
        }));
        assert_eq!(
            preflight(
                &snapshot,
                &request(&snapshot, MODE, "hqplayer.pipeline.mode.engine.50434d"),
            ),
            Err(HqpPreflightRefusal::ChoiceUnavailable)
        );
    }

    #[test]
    fn fingerprint_covers_the_complete_semantic_request() {
        let snapshot = snapshot();
        let base = request(&snapshot, MODE, "hqplayer.pipeline.mode.engine.50434d");
        let base_fingerprint = fingerprint(&base);

        let mutations: Vec<HqpImmediateCommandRequest> = vec![
            HqpImmediateCommandRequest {
                key: ProducerKey {
                    producer_id: "hqplayer:other".to_string(),
                    ..base.key.clone()
                },
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                key: ProducerKey {
                    role: TargetRole::Instance,
                    ..base.key.clone()
                },
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                key: ProducerKey {
                    zone_id: Some("hqplayer:zone".to_string()),
                    ..base.key.clone()
                },
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                expected: RevisionRef {
                    epoch: ProducerEpoch(base.expected.epoch.0 + 1),
                    ..base.expected
                },
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                expected: RevisionRef {
                    control_plane: crate::adaptive::Revision(base.expected.control_plane.0 + 1),
                    ..base.expected
                },
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                expected: RevisionRef {
                    state: crate::adaptive::Revision(base.expected.state.0 + 1),
                    ..base.expected
                },
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                control: ControlId::new(FILTER_1X),
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                lane: ApplyLane::Persistent,
                ..base.clone()
            },
            HqpImmediateCommandRequest {
                requested: ControlValue::choice("hqplayer.pipeline.mode.engine.53444d"),
                ..base.clone()
            },
        ];

        for mutation in mutations {
            assert_ne!(
                fingerprint(&mutation),
                base_fingerprint,
                "every semantic field participates in idempotency"
            );
        }
    }

    #[test]
    fn fingerprint_is_unambiguous_for_boundary_shifting_strings_and_all_value_kinds() {
        let snapshot = snapshot();
        let base = request(&snapshot, MODE, "hqplayer.pipeline.mode.engine.50434d");
        let values = vec![
            ControlValue::Empty,
            ControlValue::Bool(false),
            ControlValue::Bool(true),
            ControlValue::Integer(1),
            ControlValue::Decimal(1.0),
            ControlValue::Text("x:y".to_string()),
            ControlValue::Choice("x:y".to_string()),
            ControlValue::Composite(BTreeMap::from([
                ("a".to_string(), ControlValue::Integer(1)),
                ("b".to_string(), ControlValue::text("2")),
            ])),
            ControlValue::List(vec![ControlValue::Integer(1), ControlValue::text("2")]),
            ControlValue::Unrecognized {
                kind: "future".to_string(),
                raw: serde_json::json!({"z": [1, true], "a": "value"}),
            },
        ];
        let fingerprints = values
            .into_iter()
            .map(|requested| {
                fingerprint(&HqpImmediateCommandRequest {
                    requested,
                    ..base.clone()
                })
            })
            .collect::<std::collections::HashSet<_>>();

        assert_eq!(
            fingerprints.len(),
            10,
            "value types and payloads cannot alias"
        );

        let shifted_a = HqpImmediateCommandRequest {
            key: ProducerKey {
                producer_id: "ab".to_string(),
                ..base.key.clone()
            },
            control: ControlId::new("c"),
            requested: ControlValue::choice("de"),
            ..base.clone()
        };
        let shifted_b = HqpImmediateCommandRequest {
            key: ProducerKey {
                producer_id: "a".to_string(),
                ..base.key.clone()
            },
            control: ControlId::new("bc"),
            requested: ControlValue::choice("de"),
            ..base
        };
        assert_ne!(fingerprint(&shifted_a), fingerprint(&shifted_b));
    }

    #[test]
    fn correlation_id_must_be_nonempty_and_bounded() {
        let snapshot = snapshot();
        let mut command = request(&snapshot, MODE, "hqplayer.pipeline.mode.engine.50434d");
        command.correlation_id.clear();
        assert_eq!(
            preflight(&snapshot, &command),
            Err(HqpPreflightRefusal::CorrelationIdEmpty)
        );

        command.correlation_id = "x".repeat(257);
        assert_eq!(
            preflight(&snapshot, &command),
            Err(HqpPreflightRefusal::CorrelationIdTooLong {
                max_bytes: 256,
                actual_bytes: 257,
            })
        );
    }

    #[test]
    fn refuses_every_non_actionable_snapshot_and_lane_state() {
        let choice = "hqplayer.pipeline.mode.engine.50434d";

        let mut last_known = snapshot();
        last_known.presence = ProducerPresence::LastKnown;
        assert_eq!(
            preflight(&last_known, &request(&last_known, MODE, choice)),
            Err(HqpPreflightRefusal::ProducerNotLive)
        );

        let mut document_stale = snapshot();
        Arc::make_mut(&mut document_stale.document).stale = true;
        assert_eq!(
            preflight(&document_stale, &request(&document_stale, MODE, choice),),
            Err(HqpPreflightRefusal::ProducerStale)
        );

        let mut missing_lane = snapshot();
        Arc::make_mut(&mut missing_lane.document).lanes.clear();
        assert_eq!(
            preflight(&missing_lane, &request(&missing_lane, MODE, choice)),
            Err(HqpPreflightRefusal::NativeLaneMissing)
        );

        let mut disconnected = snapshot();
        Arc::make_mut(&mut disconnected.document).lanes[0].state = LaneState::Disconnected;
        assert_eq!(
            preflight(&disconnected, &request(&disconnected, MODE, choice)),
            Err(HqpPreflightRefusal::NativeLaneUnavailable)
        );

        let mut stale_lane = snapshot();
        Arc::make_mut(&mut stale_lane.document).lanes[0]
            .freshness
            .stale = true;
        assert_eq!(
            preflight(&stale_lane, &request(&stale_lane, MODE, choice)),
            Err(HqpPreflightRefusal::NativeLaneStale)
        );
    }

    #[test]
    fn refuses_every_non_actionable_control_and_choice_shape() {
        let choice = "hqplayer.pipeline.mode.engine.50434d";

        let unknown = snapshot();
        assert_eq!(
            preflight(
                &unknown,
                &request(&unknown, "hqplayer.pipeline.unknown", choice)
            ),
            Err(HqpPreflightRefusal::UnsupportedControl)
        );

        let mut missing_control = snapshot();
        Arc::make_mut(&mut missing_control.document)
            .controls
            .retain(|control| control.id.as_str() != MODE);
        assert_eq!(
            preflight(&missing_control, &request(&missing_control, MODE, choice),),
            Err(HqpPreflightRefusal::ControlUnknown)
        );

        let mut unavailable = snapshot();
        Arc::make_mut(&mut unavailable.document).controls[0].availability = unavailable_reason();
        assert_eq!(
            preflight(&unavailable, &request(&unavailable, MODE, choice)),
            Err(HqpPreflightRefusal::ControlUnavailable)
        );

        let mut read_only = snapshot();
        Arc::make_mut(&mut read_only.document).controls[0].apply = None;
        assert_eq!(
            preflight(&read_only, &request(&read_only, MODE, choice)),
            Err(HqpPreflightRefusal::ControlReadOnly)
        );

        let mut wrong_lane = snapshot();
        Arc::make_mut(&mut wrong_lane.document).controls[0]
            .apply
            .as_mut()
            .unwrap()
            .lane = ApplyLane::Persistent;
        assert_eq!(
            preflight(&wrong_lane, &request(&wrong_lane, MODE, choice)),
            Err(HqpPreflightRefusal::WrongApplyLane)
        );

        let non_immediate = snapshot();
        let mut non_immediate_request = request(&non_immediate, MODE, choice);
        non_immediate_request.lane = ApplyLane::Persistent;
        assert_eq!(
            preflight(&non_immediate, &non_immediate_request),
            Err(HqpPreflightRefusal::NonImmediateRequest)
        );

        let mut wrong_kind = snapshot();
        Arc::make_mut(&mut wrong_kind.document).controls[0].kind = ControlKind::Boolean;
        assert_eq!(
            preflight(&wrong_kind, &request(&wrong_kind, MODE, choice)),
            Err(HqpPreflightRefusal::WrongControlKind)
        );

        let wrong_value = snapshot();
        let mut wrong_value_request = request(&wrong_value, MODE, choice);
        wrong_value_request.requested = ControlValue::Text(choice.to_string());
        assert_eq!(
            preflight(&wrong_value, &wrong_value_request),
            Err(HqpPreflightRefusal::WrongValueKind)
        );

        let unknown_choice = snapshot();
        assert_eq!(
            preflight(
                &unknown_choice,
                &request(
                    &unknown_choice,
                    MODE,
                    "hqplayer.pipeline.mode.engine.unknown"
                ),
            ),
            Err(HqpPreflightRefusal::UnknownChoice)
        );

        let mut unavailable_choice = snapshot();
        Arc::make_mut(&mut unavailable_choice.document).controls[0]
            .choices
            .as_mut()
            .unwrap()
            .choices[0]
            .availability = Some(unavailable_reason());
        assert_eq!(
            preflight(
                &unavailable_choice,
                &request(&unavailable_choice, MODE, choice),
            ),
            Err(HqpPreflightRefusal::ChoiceUnavailable)
        );

        let mut missing_engine = snapshot();
        Arc::make_mut(&mut missing_engine.document).controls[0]
            .choices
            .as_mut()
            .unwrap()
            .choices[0]
            .engine_name = None;
        assert_eq!(
            preflight(&missing_engine, &request(&missing_engine, MODE, choice)),
            Err(HqpPreflightRefusal::ChoiceMissingEngineName)
        );
    }

    #[test]
    fn source_mode_rate_is_not_actionable() {
        let mut source_mode = snapshot();
        let rate = Arc::make_mut(&mut source_mode.document)
            .controls
            .iter_mut()
            .find(|control| control.id.as_str() == RATE)
            .unwrap();
        rate.availability = unavailable_reason();
        rate.apply = None;

        assert_eq!(
            preflight(
                &source_mode,
                &request(
                    &source_mode,
                    RATE,
                    "hqplayer.pipeline.rate.engine.313932303030",
                ),
            ),
            Err(HqpPreflightRefusal::ControlUnavailable)
        );
    }

    fn unavailable_reason() -> Availability {
        Availability::unavailable(crate::adaptive::Reason {
            code: crate::adaptive::ReasonCode::LockedByProducer,
            scope: crate::adaptive::ReasonScope::Producer,
            display_text_key: None,
            detail: None,
        })
    }
}
