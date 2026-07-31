//! Pure HQPlayer native-observation projection into the adaptive producer contract.
//!
//! This module performs no I/O. The adapter owns the generation-fenced native read;
//! the projector accepts exactly one coherent value and cannot reach a socket, cache,
//! clock, or adapter handle.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use tokio::sync::{mpsc, oneshot};

use crate::adapters::hqplayer::{
    HqpNativeExecutionTarget, HqpNativeObservation, HqpNativeObservationSink, HqpNativeSelection,
    HqpNativeTransportState,
};
use crate::adaptive::{
    ApplyEffect, ApplyLane, ApplySemantics, Availability, AvailabilityState, Choice, ChoiceSet,
    CommandOutcome, Control, ControlGroup, ControlId, ControlKind, ControlValue, Disruption,
    DocumentRevisions, Extensions, Freshness, LaneHealth, LaneState, LaneValue, NumericRange,
    OperationId, OperationRecord, OperationRequest, ProducerDocument, ProducerEpoch,
    ProducerIdentity, ProducerTarget, Provenance, Reason, ReasonCode, ReasonScope, RecoveryState,
    RiskClass, TargetRole, Timestamp, TransportLane, ValueLane, Verification, WriteAttempt,
    CONSUMER_SCHEMA_VERSION,
};
use crate::producers::hqplayer_command::{
    fingerprint, preflight, HqpCommandFingerprint, HqpImmediateCommandRequest, HqpPreflightRefusal,
    HqpValidatedCommand,
};
use crate::producers::{
    AdapterRun, AdapterRunId, AdaptiveHandle, Admission, ProducerKey, ProducerPresence,
    ProducerSnapshot, RetirementOutcome,
};

const CONTROL_TRANSPORT_STATE: &str = "hqplayer.transport.state";
const CONTROL_TRANSPORT_PLAY: &str = "hqplayer.transport.play";
const CONTROL_TRANSPORT_PAUSE: &str = "hqplayer.transport.pause";
const CONTROL_TRANSPORT_STOP: &str = "hqplayer.transport.stop";
const CONTROL_TRANSPORT_PREVIOUS: &str = "hqplayer.transport.previous";
const CONTROL_TRANSPORT_NEXT: &str = "hqplayer.transport.next";
const CONTROL_METADATA_TRACK_ID: &str = "hqplayer.metadata.track_id";
const CONTROL_METADATA_TITLE: &str = "hqplayer.metadata.title";
const CONTROL_METADATA_ARTIST: &str = "hqplayer.metadata.artist";
const CONTROL_METADATA_ALBUM: &str = "hqplayer.metadata.album";
const CONTROL_VOLUME_LEVEL: &str = "hqplayer.volume.level";
const CONTROL_PIPELINE_MODE: &str = "hqplayer.pipeline.mode";
const CONTROL_PIPELINE_FILTER_1X: &str = "hqplayer.pipeline.filter_1x";
const CONTROL_PIPELINE_FILTER_NX: &str = "hqplayer.pipeline.filter_nx";
const CONTROL_PIPELINE_SHAPER: &str = "hqplayer.pipeline.shaper";
const CONTROL_PIPELINE_RATE: &str = "hqplayer.pipeline.rate";

const GROUP_TRANSPORT: &str = "transport";
const GROUP_METADATA: &str = "metadata";
const GROUP_VOLUME: &str = "volume";
const GROUP_PIPELINE: &str = "pipeline";

/// The protocol-accurate HQPlayer operation which owns one adaptive control's mutation.
///
/// This is intentionally a semantic, adapter-facing vocabulary rather than a wire vocabulary:
/// callers carry names and decimal values, and [`crate::adapters::hqplayer::HqpAdapter`] resolves
/// them against the daemon's current state before it writes. Issue #329 adds the checked internal
/// dispatch path; #331's public consumers must reuse it instead of inventing a second mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum HqpSemanticOperation {
    /// [`crate::adapters::hqplayer::HqpAdapter::play`].
    Play,
    /// [`crate::adapters::hqplayer::HqpAdapter::pause`].
    Pause,
    /// [`crate::adapters::hqplayer::HqpAdapter::stop`].
    Stop,
    /// [`crate::adapters::hqplayer::HqpAdapter::previous`].
    Previous,
    /// [`crate::adapters::hqplayer::HqpAdapter::next`].
    Next,
    /// [`crate::adapters::hqplayer::HqpAdapter::set_volume_db`].
    SetVolumeDb,
    /// [`crate::adapters::hqplayer::HqpAdapter::set_mode`].
    SetMode,
    /// [`crate::adapters::hqplayer::HqpAdapter::set_filter_1x`].
    SetFilter1x,
    /// [`crate::adapters::hqplayer::HqpAdapter::set_filter_nx`].
    SetFilterNx,
    /// [`crate::adapters::hqplayer::HqpAdapter::set_shaper`].
    SetShaper,
    /// [`crate::adapters::hqplayer::HqpAdapter::set_rate`].
    SetRate,
}

/// One side of the producer-to-adapter semantic-operation bijection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct HqpControlOperation {
    pub control_id: &'static str,
    pub operation: HqpSemanticOperation,
}

// This is deliberately one registry, not a second set of string matches in every consumer. The
// producer validates every dynamically mutable control against it before publication, and #329's
// command service uses the same mapping after it has preflighted a client command.
const HQP_SEMANTIC_OPERATION_REGISTRY: [HqpControlOperation; 11] = [
    HqpControlOperation {
        control_id: CONTROL_TRANSPORT_PLAY,
        operation: HqpSemanticOperation::Play,
    },
    HqpControlOperation {
        control_id: CONTROL_TRANSPORT_PAUSE,
        operation: HqpSemanticOperation::Pause,
    },
    HqpControlOperation {
        control_id: CONTROL_TRANSPORT_STOP,
        operation: HqpSemanticOperation::Stop,
    },
    HqpControlOperation {
        control_id: CONTROL_TRANSPORT_PREVIOUS,
        operation: HqpSemanticOperation::Previous,
    },
    HqpControlOperation {
        control_id: CONTROL_TRANSPORT_NEXT,
        operation: HqpSemanticOperation::Next,
    },
    HqpControlOperation {
        control_id: CONTROL_VOLUME_LEVEL,
        operation: HqpSemanticOperation::SetVolumeDb,
    },
    HqpControlOperation {
        control_id: CONTROL_PIPELINE_MODE,
        operation: HqpSemanticOperation::SetMode,
    },
    HqpControlOperation {
        control_id: CONTROL_PIPELINE_FILTER_1X,
        operation: HqpSemanticOperation::SetFilter1x,
    },
    HqpControlOperation {
        control_id: CONTROL_PIPELINE_FILTER_NX,
        operation: HqpSemanticOperation::SetFilterNx,
    },
    HqpControlOperation {
        control_id: CONTROL_PIPELINE_SHAPER,
        operation: HqpSemanticOperation::SetShaper,
    },
    HqpControlOperation {
        control_id: CONTROL_PIPELINE_RATE,
        operation: HqpSemanticOperation::SetRate,
    },
];

/// The stable semantic-control mapping used by the future adaptive command dispatcher.
pub(crate) fn hqp_semantic_operation_registry() -> &'static [HqpControlOperation] {
    &HQP_SEMANTIC_OPERATION_REGISTRY
}

/// Resolve a published HQPlayer control to the one adapter operation allowed to mutate it.
///
/// A malformed internal registry is deliberately not hidden behind an arbitrary first match. The
/// producer rejects that document before publication in [`validate_mutable_control_operations`].
pub(crate) fn hqp_semantic_operation(control_id: &ControlId) -> Option<HqpSemanticOperation> {
    let mut matches = hqp_semantic_operation_registry()
        .iter()
        .filter(|entry| entry.control_id == control_id.as_str());
    let operation = matches.next()?.operation;
    matches.next().is_none().then_some(operation)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum ProjectionError {
    InvalidInstanceName,
    InvalidVolumeEvidence {
        reason: &'static str,
    },
    SelectedChoiceMissing {
        control_id: &'static str,
        selected: String,
    },
    MutableControlHasNoSemanticOperation {
        control_id: String,
    },
    MutableControlHasMultipleSemanticOperations {
        control_id: String,
    },
}

pub(super) struct RevisionOutcome {
    pub document: ProducerDocument,
    #[cfg(test)]
    pub control_plane_advanced: bool,
    #[cfg(test)]
    pub state_advanced: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RevisionError {
    EpochRegressed { current: u64, incoming: u64 },
    CounterExhausted { plane: &'static str },
}

#[derive(Clone, Default)]
pub(super) struct RevisionTracker {
    epoch: Option<u64>,
    control_plane: u64,
    state: u64,
    previous_control_view: Option<ProducerDocument>,
    previous_state_view: Option<ProducerDocument>,
    last_document: Option<ProducerDocument>,
}

impl RevisionTracker {
    pub fn materialize(
        &mut self,
        mut projected: ProducerDocument,
    ) -> Result<RevisionOutcome, RevisionError> {
        let epoch = projected.producer.epoch.0;
        if let Some(current) = self.epoch {
            if epoch < current {
                return Err(RevisionError::EpochRegressed {
                    current,
                    incoming: epoch,
                });
            }
        }
        let control_view = canonical_control_plane(&projected);
        let state_view = canonical_state(&projected);

        let epoch_changed = self.epoch != Some(epoch);
        let control_plane_advanced = epoch_changed
            || self
                .previous_control_view
                .as_ref()
                .is_none_or(|previous| previous != &control_view);
        let state_advanced = epoch_changed
            || self
                .previous_state_view
                .as_ref()
                .is_none_or(|previous| previous != &state_view);

        let (next_control_plane, next_state) = if epoch_changed {
            // Revisions are comparable only within an epoch. A coherent reconnect starts
            // a fresh sequence instead of inheriting counters from a different engine session.
            (1, 1)
        } else {
            let control_plane = if control_plane_advanced {
                next_revision(self.control_plane, "control_plane")?
            } else {
                self.control_plane
            };
            let state = if state_advanced {
                next_revision(self.state, "state")?
            } else {
                self.state
            };
            (control_plane, state)
        };

        self.epoch = Some(epoch);
        self.control_plane = next_control_plane;
        self.state = next_state;
        self.previous_control_view = Some(control_view);
        self.previous_state_view = Some(state_view);
        projected.revisions = DocumentRevisions::new(self.control_plane, self.state);
        self.last_document = Some(projected.clone());

        Ok(RevisionOutcome {
            document: projected,
            #[cfg(test)]
            control_plane_advanced,
            #[cfg(test)]
            state_advanced,
        })
    }

    /// Publish retained facts as last-known after a transient native observation failure. The first
    /// stale transition is an observed-state change; later lane-only error refreshes intentionally
    /// retain the same revisions so open drafts do not churn with retry timing.
    pub fn mark_transient_failure(
        &mut self,
        observed_at: Timestamp,
    ) -> Result<Option<RevisionOutcome>, RevisionError> {
        let Some(mut retained) = self.last_document.clone() else {
            return Ok(None);
        };
        retained.stale = true;
        if let Some(native) = retained
            .lanes
            .iter_mut()
            .find(|lane| lane.lane == TransportLane::Native)
        {
            native.state = LaneState::Disconnected;
            native.last_error = Some(Reason::observed(ReasonCode::RequiresConnection));
            native.freshness.observed_at = Some(observed_at);
            native.freshness.stale = true;
        }
        self.materialize(retained).map(Some)
    }
}

pub(super) fn project_native(
    observation: &HqpNativeObservation,
) -> Result<ProducerDocument, ProjectionError> {
    validate_native_observation(observation)?;
    let volume = volume_control(observation);
    let mode = selection_control(
        CONTROL_PIPELINE_MODE,
        "control.pipeline.mode",
        observation,
        &observation.mode,
        Some(pipeline_apply(
            Disruption::PlaybackInterruption,
            RiskClass::Disruptive,
            vec![
                control_id(CONTROL_PIPELINE_FILTER_1X),
                control_id(CONTROL_PIPELINE_FILTER_NX),
                control_id(CONTROL_PIPELINE_SHAPER),
                control_id(CONTROL_PIPELINE_RATE),
            ],
            Vec::new(),
        )),
        Availability::available(),
    )?;
    let filter_1x = selection_control(
        CONTROL_PIPELINE_FILTER_1X,
        "control.pipeline.filter_1x",
        observation,
        &observation.filter_1x,
        Some(pipeline_apply(
            Disruption::AudibleGlitch,
            RiskClass::Caution,
            Vec::new(),
            vec![control_id(CONTROL_PIPELINE_FILTER_NX)],
        )),
        Availability::available(),
    )?;
    let filter_nx = selection_control(
        CONTROL_PIPELINE_FILTER_NX,
        "control.pipeline.filter_nx",
        observation,
        &observation.filter_nx,
        Some(pipeline_apply(
            Disruption::AudibleGlitch,
            RiskClass::Caution,
            Vec::new(),
            vec![control_id(CONTROL_PIPELINE_FILTER_1X)],
        )),
        Availability::available(),
    )?;
    let shaper = selection_control(
        CONTROL_PIPELINE_SHAPER,
        "control.pipeline.shaper",
        observation,
        &observation.shaper,
        Some(pipeline_apply(
            Disruption::AudibleGlitch,
            RiskClass::Caution,
            Vec::new(),
            Vec::new(),
        )),
        Availability::available(),
    )?;
    let rate_availability = if observation.mode_is_source {
        unavailable(
            ReasonCode::NotApplicableInMode,
            ReasonScope::Observed,
            "reason.pipeline.rate_source_mode",
        )
    } else {
        Availability::available()
    };
    let rate = selection_control(
        CONTROL_PIPELINE_RATE,
        "control.pipeline.rate",
        observation,
        &observation.rate,
        (!observation.mode_is_source).then(|| {
            pipeline_apply(
                Disruption::AudibleGlitch,
                RiskClass::Caution,
                Vec::new(),
                Vec::new(),
            )
        }),
        rate_availability,
    )?;

    let controls = vec![
        transport_action_control(
            CONTROL_TRANSPORT_PLAY,
            "control.transport.play",
            CONTROL_TRANSPORT_STATE,
        ),
        transport_action_control(
            CONTROL_TRANSPORT_PAUSE,
            "control.transport.pause",
            CONTROL_TRANSPORT_STATE,
        ),
        transport_action_control(
            CONTROL_TRANSPORT_STOP,
            "control.transport.stop",
            CONTROL_TRANSPORT_STATE,
        ),
        transport_action_control(
            CONTROL_TRANSPORT_PREVIOUS,
            "control.transport.previous",
            CONTROL_METADATA_TRACK_ID,
        ),
        transport_action_control(
            CONTROL_TRANSPORT_NEXT,
            "control.transport.next",
            CONTROL_METADATA_TRACK_ID,
        ),
        transport_state_control(observation),
        metadata_control(
            CONTROL_METADATA_TRACK_ID,
            "control.metadata.track_id",
            observation,
            observation.metadata.track_id.as_deref(),
        ),
        metadata_control(
            CONTROL_METADATA_TITLE,
            "control.metadata.title",
            observation,
            observation.metadata.title.as_deref(),
        ),
        metadata_control(
            CONTROL_METADATA_ARTIST,
            "control.metadata.artist",
            observation,
            observation.metadata.artist.as_deref(),
        ),
        metadata_control(
            CONTROL_METADATA_ALBUM,
            "control.metadata.album",
            observation,
            observation.metadata.album.as_deref(),
        ),
        volume,
        mode,
        filter_1x,
        filter_nx,
        shaper,
        rate,
    ];
    validate_mutable_control_operations(&controls)?;

    Ok(ProducerDocument {
        schema_version: CONSUMER_SCHEMA_VERSION,
        producer: ProducerIdentity {
            producer_id: format!("hqplayer:{}", observation.instance_name),
            producer_type: "hqplayer".to_string(),
            instance_label: observation.instance_label.clone(),
            product_version: observation.product_version.clone(),
            epoch: ProducerEpoch(observation.producer_epoch),
            extensions: Extensions::default(),
        },
        target: ProducerTarget {
            role: TargetRole::DspEngine,
            zone_id: None,
            extensions: Extensions::default(),
        },
        revisions: DocumentRevisions::new(0, 0),
        lanes: vec![LaneHealth {
            lane: TransportLane::Native,
            state: LaneState::Connected,
            last_success: Some(contract_timestamp(observation.observed_at)),
            last_error: None,
            freshness: Freshness {
                observed_at: Some(contract_timestamp(observation.observed_at)),
                age_ms: None,
                stale: false,
            },
            extensions: Extensions::default(),
        }],
        groups: vec![
            group(
                GROUP_TRANSPORT,
                "group.transport",
                vec![
                    control_id(CONTROL_TRANSPORT_PLAY),
                    control_id(CONTROL_TRANSPORT_PAUSE),
                    control_id(CONTROL_TRANSPORT_STOP),
                    control_id(CONTROL_TRANSPORT_PREVIOUS),
                    control_id(CONTROL_TRANSPORT_NEXT),
                    control_id(CONTROL_TRANSPORT_STATE),
                ],
            ),
            group(
                GROUP_METADATA,
                "group.metadata",
                vec![
                    control_id(CONTROL_METADATA_TRACK_ID),
                    control_id(CONTROL_METADATA_TITLE),
                    control_id(CONTROL_METADATA_ARTIST),
                    control_id(CONTROL_METADATA_ALBUM),
                ],
            ),
            group(
                GROUP_VOLUME,
                "group.volume",
                vec![control_id(CONTROL_VOLUME_LEVEL)],
            ),
            group(
                GROUP_PIPELINE,
                "group.pipeline",
                vec![
                    control_id(CONTROL_PIPELINE_MODE),
                    control_id(CONTROL_PIPELINE_FILTER_1X),
                    control_id(CONTROL_PIPELINE_FILTER_NX),
                    control_id(CONTROL_PIPELINE_SHAPER),
                    control_id(CONTROL_PIPELINE_RATE),
                ],
            ),
        ],
        controls,
        constraints: Vec::new(),
        draft_policy: None,
        change_sets: Vec::new(),
        operations: Vec::new(),
        stale: false,
        extensions: Extensions::default(),
    })
}

fn contract_timestamp(observed_at: SystemTime) -> Timestamp {
    let observed_at: chrono::DateTime<chrono::Utc> = observed_at.into();
    Timestamp::new(observed_at.to_rfc3339())
}

fn validate_native_observation(observation: &HqpNativeObservation) -> Result<(), ProjectionError> {
    if observation.instance_name.trim().is_empty() {
        return Err(ProjectionError::InvalidInstanceName);
    }

    let volume = &observation.volume;
    if !volume.value_db.is_finite() || !volume.min_db.is_finite() || !volume.max_db.is_finite() {
        return Err(ProjectionError::InvalidVolumeEvidence {
            reason: "volume value and bounds must be finite",
        });
    }
    if volume.min_db > volume.max_db {
        return Err(ProjectionError::InvalidVolumeEvidence {
            reason: "minimum exceeds maximum",
        });
    }
    if volume.value_db < volume.min_db || volume.value_db > volume.max_db {
        return Err(ProjectionError::InvalidVolumeEvidence {
            reason: "observed volume is outside the reported range",
        });
    }
    if volume
        .step_db
        .is_some_and(|step| !step.is_finite() || step <= 0.0)
    {
        return Err(ProjectionError::InvalidVolumeEvidence {
            reason: "volume step must be finite and positive",
        });
    }
    Ok(())
}

/// Keep publication honest if a descriptor gains mutability before an adapter operation is named.
///
/// Availability is runtime-dependent (fixed volume and source mode are the current examples), so
/// this checks the controls a particular native observation actually advertises rather than assuming
/// that every registry entry is usable on every daemon state.
fn validate_mutable_control_operations(controls: &[Control]) -> Result<(), ProjectionError> {
    for control in controls.iter().filter(|control| control.is_mutable()) {
        if hqp_semantic_operation(&control.id).is_some() {
            continue;
        }
        let matches = hqp_semantic_operation_registry()
            .iter()
            .filter(|entry| entry.control_id == control.id.as_str())
            .count();
        match matches {
            1 => {}
            0 => {
                return Err(ProjectionError::MutableControlHasNoSemanticOperation {
                    control_id: control.id.as_str().to_string(),
                });
            }
            _ => {
                return Err(
                    ProjectionError::MutableControlHasMultipleSemanticOperations {
                        control_id: control.id.as_str().to_string(),
                    },
                );
            }
        }
    }
    Ok(())
}

fn next_revision(current: u64, plane: &'static str) -> Result<u64, RevisionError> {
    // A producer cannot safely continue in the same epoch once the contract counter is
    // exhausted. Refusal is safer than wrapping or publishing changed content at the same
    // revision, either of which destroys the admission ordering invariant.
    current
        .checked_add(1)
        .ok_or(RevisionError::CounterExhausted { plane })
}

fn control_id(id: &str) -> ControlId {
    ControlId::new(id)
}

fn observed(_observation: &HqpNativeObservation, value: ControlValue) -> LaneValue {
    // Native freshness belongs to the one native transport witness. Repeating that volatile
    // timestamp on every value would turn a health refresh into an equal-revision content change,
    // which #324 must reject rather than retain.
    LaneValue::grounded(ValueLane::Observed, value, Provenance::runtime())
}

fn unobserved(_observation: &HqpNativeObservation, reason: ReasonCode) -> LaneValue {
    LaneValue::ungrounded(ValueLane::Observed, reason, Provenance::runtime())
}

fn unavailable(code: ReasonCode, scope: ReasonScope, display_text_key: &str) -> Availability {
    Availability::unavailable(Reason {
        code,
        scope,
        display_text_key: Some(display_text_key.to_string()),
        detail: None,
    })
}

fn read_only(display_text_key: &str) -> Availability {
    Availability {
        state: AvailabilityState::ReadOnly,
        reasons: vec![Reason {
            code: ReasonCode::LockedByProducer,
            scope: ReasonScope::Producer,
            display_text_key: Some(display_text_key.to_string()),
            detail: None,
        }],
    }
}

fn group(id: &str, label_key: &str, members: Vec<ControlId>) -> ControlGroup {
    ControlGroup {
        id: id.to_string(),
        label_key: Some(label_key.to_string()),
        order: None,
        members,
    }
}

fn control(
    id: &str,
    kind: ControlKind,
    label_key: &str,
    group: &str,
    values: Vec<LaneValue>,
    choices: Option<ChoiceSet>,
    range: Option<NumericRange>,
    availability: Availability,
    apply: Option<ApplySemantics>,
    unit: Option<&str>,
) -> Control {
    Control {
        id: control_id(id),
        kind,
        label_key: Some(label_key.to_string()),
        description_key: None,
        group: Some(group.to_string()),
        order: None,
        widget_hint: None,
        unit: unit.map(str::to_string),
        values,
        choices,
        range,
        availability,
        apply,
        divergences: Vec::new(),
        members: Vec::new(),
        extensions: Extensions::default(),
    }
}

fn transport_state_control(observation: &HqpNativeObservation) -> Control {
    control(
        CONTROL_TRANSPORT_STATE,
        ControlKind::Text,
        "control.transport.state",
        GROUP_TRANSPORT,
        vec![observed(
            observation,
            ControlValue::text(match observation.transport {
                HqpNativeTransportState::Stopped => "stopped",
                HqpNativeTransportState::Paused => "paused",
                HqpNativeTransportState::Playing => "playing",
                HqpNativeTransportState::Unknown => "unknown",
            }),
        )],
        None,
        None,
        read_only("reason.transport.state_observed_only"),
        None,
        None,
    )
}

fn transport_action_control(id: &str, label_key: &str, verify_via: &str) -> Control {
    control(
        id,
        ControlKind::Action,
        label_key,
        GROUP_TRANSPORT,
        Vec::new(),
        None,
        None,
        Availability::available(),
        Some(ApplySemantics {
            lane: ApplyLane::Immediate,
            effect: ApplyEffect::VerifiedPending,
            disruption: Disruption::NoDisruption,
            risk: RiskClass::Safe,
            verification: Verification {
                verify_via: Some(control_id(verify_via)),
                verify_lane: ValueLane::Observed,
                provisional_ack: true,
                preserves: Vec::new(),
            },
            invalidates: Vec::new(),
            coupled_with: Vec::new(),
        }),
        None,
    )
}

fn metadata_control(
    id: &str,
    label_key: &str,
    observation: &HqpNativeObservation,
    value: Option<&str>,
) -> Control {
    let lane = match observation.transport {
        HqpNativeTransportState::Stopped => {
            unobserved(observation, ReasonCode::NotApplicableInMode)
        }
        HqpNativeTransportState::Unknown => unobserved(observation, ReasonCode::Unobservable),
        HqpNativeTransportState::Paused | HqpNativeTransportState::Playing => value
            .filter(|value| !value.trim().is_empty())
            .map(|value| observed(observation, ControlValue::text(value)))
            .unwrap_or_else(|| unobserved(observation, ReasonCode::Unobservable)),
    };
    control(
        id,
        ControlKind::Text,
        label_key,
        GROUP_METADATA,
        vec![lane],
        None,
        None,
        read_only("reason.metadata_observed_only"),
        None,
        None,
    )
}

fn volume_control(observation: &HqpNativeObservation) -> Control {
    let availability = if observation.volume.enabled {
        Availability::available()
    } else {
        read_only("reason.volume.fixed")
    };
    let apply = observation.volume.enabled.then(|| ApplySemantics {
        lane: ApplyLane::Immediate,
        effect: ApplyEffect::VerifiedPending,
        disruption: Disruption::NoDisruption,
        risk: RiskClass::Caution,
        verification: Verification {
            verify_via: None,
            verify_lane: ValueLane::Observed,
            provisional_ack: true,
            preserves: Vec::new(),
        },
        invalidates: Vec::new(),
        coupled_with: Vec::new(),
    });
    control(
        CONTROL_VOLUME_LEVEL,
        ControlKind::NumericRange,
        "control.volume.level",
        GROUP_VOLUME,
        vec![observed(
            observation,
            ControlValue::Decimal(observation.volume.value_db),
        )],
        None,
        Some(NumericRange {
            min: Some(observation.volume.min_db),
            max: Some(observation.volume.max_db),
            step: observation.volume.step_db,
            unit: Some("dB".to_string()),
            authority: crate::adaptive::Authority::Runtime,
            revision: None,
        }),
        availability,
        apply,
        Some("dB"),
    )
}

fn selection_control(
    id: &'static str,
    label_key: &str,
    observation: &HqpNativeObservation,
    selection: &HqpNativeSelection,
    apply: Option<ApplySemantics>,
    availability: Availability,
) -> Result<Control, ProjectionError> {
    let choices = choice_set(id, &selection.choices);
    let selected = choice_id(id, &selection.selected);
    if !choices.contains(&selected) {
        return Err(ProjectionError::SelectedChoiceMissing {
            control_id: id,
            selected: selection.selected.clone(),
        });
    }
    let observed_value = if id == CONTROL_PIPELINE_MODE && observation.mode_is_source {
        // `[source]` is HQPlayer's default identity, not an opaque choice. The option remains in
        // the runtime enumeration so it can be rendered and selected, while the observed lane
        // preserves the contract's distinct `Empty` semantic.
        ControlValue::Empty
    } else {
        ControlValue::choice(selected)
    };
    Ok(control(
        id,
        ControlKind::Enumeration,
        label_key,
        GROUP_PIPELINE,
        vec![observed(observation, observed_value)],
        Some(choices),
        None,
        availability,
        apply,
        None,
    ))
}

fn choice_set(control_id: &str, names: &[String]) -> ChoiceSet {
    let mut choices = Vec::with_capacity(names.len());
    for name in names {
        let id = choice_id(control_id, name);
        if choices.iter().any(|choice: &Choice| choice.id == id) {
            // Duplicate names do not identify different semantic choices. Keeping only the
            // first preserves engine order without inventing an index-derived identity.
            continue;
        }
        choices.push(Choice::new(id, name));
    }
    ChoiceSet::runtime(choices)
}

fn choice_id(control_id: &str, engine_name: &str) -> String {
    // Hex is reversible and byte-exact, unlike slugification. It therefore cannot collide for
    // punctuation, Unicode, case, or future engine names, while the name itself remains present
    // as `Choice::engine_name` for rendering and semantic execution.
    let encoded = engine_name
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!("{control_id}.engine.{encoded}")
}

fn pipeline_apply(
    disruption: Disruption,
    risk: RiskClass,
    invalidates: Vec<ControlId>,
    preserves: Vec<ControlId>,
) -> ApplySemantics {
    ApplySemantics {
        lane: ApplyLane::Immediate,
        effect: ApplyEffect::LiveImmediate,
        disruption,
        risk,
        verification: Verification {
            verify_via: None,
            verify_lane: ValueLane::Observed,
            provisional_ack: false,
            preserves,
        },
        invalidates,
        coupled_with: Vec::new(),
    }
}

fn canonical_control_plane(document: &ProducerDocument) -> ProducerDocument {
    let mut canonical = document.clone();
    canonical.revisions = DocumentRevisions::new(0, 0);
    canonical.lanes.clear();
    canonical.change_sets.clear();
    canonical.operations.clear();
    canonical.stale = false;
    for control in &mut canonical.controls {
        control.values.clear();
        control.availability = Availability::available();
        control.divergences.clear();
        if let Some(choices) = &mut control.choices {
            for choice in &mut choices.choices {
                choice.availability = None;
            }
        }
    }
    canonical
}

fn canonical_state(document: &ProducerDocument) -> ProducerDocument {
    let mut canonical = document.clone();
    canonical.revisions = DocumentRevisions::new(0, 0);
    canonical.producer.instance_label = None;
    canonical.producer.product_version = None;
    canonical.producer.epoch = ProducerEpoch(0);
    canonical.target.zone_id = None;
    canonical.lanes.clear();
    canonical.groups.clear();
    canonical.constraints.clear();
    canonical.draft_policy = None;
    canonical.change_sets.clear();
    for control in &mut canonical.controls {
        control.kind = ControlKind::Text;
        control.label_key = None;
        control.description_key = None;
        control.group = None;
        control.order = None;
        control.widget_hint = None;
        control.unit = None;
        control.choices = None;
        control.range = None;
        control.apply = None;
        control.members.clear();
        for value in &mut control.values {
            value.freshness = Freshness::default();
            value.provenance.revision = None;
        }
    }
    canonical
}

/// Composition-root owner for HQPlayer's adaptive publication lifecycle.
///
/// The native adapter reports coherent facts through [`HqpNativeObservationSink`]. This type owns
/// every adaptive concern: projection, per-instance revision history, the shared HQPlayer adapter
/// run, last-known transitions and definitive retirement.
pub struct HqpAdaptivePublisher {
    commands: mpsc::Sender<HqpCoordinatorCommand>,
}

/// Terminal operation audit retained per HQPlayer instance after all unresolved records.
///
/// Correlation tombstones are retained for exactly the same records, so memory use is bounded
/// without silently discarding an operation which may still converge after reconnect.
const MAX_TERMINAL_OPERATIONS: usize = 32;

/// Terminal operation records are compacted from the public document after this many entries,
/// but their correlation fingerprints remain as a bounded no-replay fence for longer. A client
/// must choose a fresh correlation once its original audit record no longer fits in the document;
/// silently treating an old gesture as new would repeat native I/O.
const MAX_CORRELATION_TOMBSTONES: usize = 256;

/// Retired instance identities retain bounded duplicate-correlation truth. Instance names are
/// configuration input, so bounding each retired ledger is insufficient by itself: a stream of
/// distinct removed names must not grow coordinator memory forever.
const MAX_RETIRED_INSTANCES: usize = 64;

/// An unresolved outcome carries real uncertainty and therefore must never be compacted away.
/// The producer applies backpressure instead of allowing a disconnected/indeterminate stream to
/// grow memory forever. One pipeline command may still be Pending in addition to these records.
const MAX_UNRESOLVED_OPERATIONS: usize = 32;

/// Correlations are opaque client input, but cannot be allowed to grow retained coordinator state
/// without bound. This mirrors the command boundary and remains enforced here for internal callers.
const MAX_CORRELATION_ID_BYTES: usize = 256;

/// A terminal publication gets one coordinator-owned retry. Generated terminal documents are
/// deterministic, so retrying cannot duplicate native I/O or allocate another operation.
const TERMINAL_PUBLICATION_RETRIES: usize = 1;

/// One conservative serialization domain for the first immediate HQPlayer slice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HqpConflictSet {
    Pipeline,
}

/// Opaque proof that one Pending operation was admitted by the active producer run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HqpCommandLease {
    producer_id: String,
    producer_epoch: ProducerEpoch,
    run_id: Option<AdapterRunId>,
    conflict_set: HqpConflictSet,
    generation: u64,
    correlation_id: String,
    operation_id: OperationId,
    execution_target: Option<HqpNativeExecutionTarget>,
}

impl HqpCommandLease {
    /// Test-only opaque token for command-actor fakes. A real coordinator always rejects it.
    #[cfg(test)]
    pub(crate) fn for_test(
        producer_id: impl Into<String>,
        producer_epoch: u64,
        generation: u64,
        correlation_id: impl Into<String>,
    ) -> Self {
        let producer_id = producer_id.into();
        let correlation_id = correlation_id.into();
        Self {
            operation_id: OperationId::new(format!("{producer_id}:operation:{generation}")),
            producer_id,
            producer_epoch: ProducerEpoch(producer_epoch),
            run_id: None,
            conflict_set: HqpConflictSet::Pipeline,
            generation,
            correlation_id,
            execution_target: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_with_execution_target(
        producer_id: impl Into<String>,
        producer_epoch: u64,
        generation: u64,
        correlation_id: impl Into<String>,
        execution_target: HqpNativeExecutionTarget,
    ) -> Self {
        let mut lease = Self::for_test(producer_id, producer_epoch, generation, correlation_id);
        lease.execution_target = Some(execution_target);
        lease
    }

    /// Exact coherent native identity captured by the admitted producer observation.
    pub(crate) fn execution_target(&self) -> Option<&HqpNativeExecutionTarget> {
        self.execution_target.as_ref()
    }
}

/// Whether a reservation allocated a new operation or found the caller's exact retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HqpReservationDisposition {
    New,
    Duplicate,
}

/// An admitted Pending operation and the opaque token needed to dispatch it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HqpCommandReservation {
    lease: HqpCommandLease,
    operation: OperationRecord,
    disposition: HqpReservationDisposition,
}

impl HqpCommandReservation {
    #[cfg(test)]
    pub(crate) fn for_test(
        lease: HqpCommandLease,
        operation: OperationRecord,
        is_duplicate: bool,
    ) -> Self {
        Self {
            lease,
            operation,
            disposition: if is_duplicate {
                HqpReservationDisposition::Duplicate
            } else {
                HqpReservationDisposition::New
            },
        }
    }

    pub(crate) fn lease(&self) -> &HqpCommandLease {
        &self.lease
    }

    pub(crate) fn execution_target(&self) -> Option<&HqpNativeExecutionTarget> {
        self.lease.execution_target()
    }

    pub(crate) fn operation(&self) -> &OperationRecord {
        &self.operation
    }

    pub(crate) fn is_duplicate(&self) -> bool {
        self.disposition == HqpReservationDisposition::Duplicate
    }
}

/// Typed evidence supplied by the command actor after native execution.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HqpCommandCompletion {
    outcome: CommandOutcome,
    write_attempt: WriteAttempt,
    at: Timestamp,
    recovery: Option<RecoveryState>,
    reason: Option<Reason>,
}

impl HqpCommandCompletion {
    pub(crate) fn new(
        outcome: CommandOutcome,
        write_attempt: WriteAttempt,
        at: Timestamp,
        recovery: Option<RecoveryState>,
        reason: Option<Reason>,
    ) -> Self {
        Self {
            outcome,
            write_attempt,
            at,
            recovery,
            reason,
        }
    }
}

/// Typed coordinator refusals. They never get classified by parsing an error string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HqpCoordinatorRefusal {
    CoordinatorClosed,
    LifecycleNotRunning,
    ProducerUnknown,
    Preflight(HqpPreflightRefusal),
    CorrelationConflict,
    InvalidCorrelation,
    ConflictBusy,
    UnresolvedRetentionFull,
    StaleLease,
    LeaseStillCurrent,
    IllegalTransition,
    RevisionFailed(String),
    PublicationFailed(String),
    RetirementFailed(String),
}

#[derive(Clone)]
struct HqpCorrelationReservation {
    fingerprint: HqpCommandFingerprint,
    lease: HqpCommandLease,
}

#[derive(Clone, Default)]
struct HqpInstanceCoordinator {
    tracker: RevisionTracker,
    ledger: Vec<OperationRecord>,
    next_generation: u64,
    correlations: HashMap<String, HqpCorrelationReservation>,
    correlation_tombstones: HashMap<String, HqpCommandFingerprint>,
    correlation_tombstone_order: VecDeque<String>,
    active_pipeline: Option<u64>,
    /// Generations whose final lease check passed and whose executor call has begun. This is
    /// coordinator-only causality state: it is not write-attempt evidence and never enters the
    /// producer document. Only the typed receipt may clear it.
    dispatch_in_progress: HashSet<u64>,
    execution_target: Option<HqpNativeExecutionTarget>,
    deferred_commit: Option<HqpDeferredCommit>,
    retirement_requested: Option<ProducerEpoch>,
}

#[derive(Clone)]
struct HqpDeferredCommit {
    tracker: RevisionTracker,
    ledger: Vec<OperationRecord>,
    active_pipeline: Option<u64>,
    dispatch_in_progress: HashSet<u64>,
    correlation_tombstones: HashMap<String, HqpCommandFingerprint>,
    correlation_tombstone_order: VecDeque<String>,
}

/// Retired producers are no longer visible to adaptive consumers, but retain exact terminal
/// correlation truth long enough for an in-flight command actor (or a duplicate gesture) to
/// learn the terminal outcome rather than accidentally treating removal as permission to replay.
#[derive(Clone)]
struct HqpRetiredInstance {
    producer_epoch: ProducerEpoch,
    ledger: Vec<OperationRecord>,
    correlations: HashMap<String, HqpCorrelationReservation>,
}

enum HqpCoordinatorCommand {
    ManagerStarted {
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
    Observed {
        observation: Box<HqpNativeObservation>,
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
    TransientFailure {
        instance_name: String,
        observed_at: SystemTime,
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
    InstanceRemoved {
        instance_name: String,
        producer_epoch: u64,
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
    ManagerStopped {
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
    Reserve {
        command: HqpValidatedCommand,
        reply: oneshot::Sender<Result<HqpCommandReservation, HqpCoordinatorRefusal>>,
    },
    LookupCorrelation {
        request: HqpImmediateCommandRequest,
        reply: oneshot::Sender<Result<Option<HqpCommandReservation>, HqpCoordinatorRefusal>>,
    },
    #[cfg(test)]
    Validate {
        lease: HqpCommandLease,
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
    BeginDispatch {
        lease: HqpCommandLease,
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
    Complete {
        lease: HqpCommandLease,
        completion: HqpCommandCompletion,
        reply: oneshot::Sender<Result<OperationRecord, HqpCoordinatorRefusal>>,
    },
    SupersedeStaleBeforeDispatch {
        lease: HqpCommandLease,
        at: Timestamp,
        reply: oneshot::Sender<Result<OperationRecord, HqpCoordinatorRefusal>>,
    },
    #[cfg(test)]
    RefuseNextPublication {
        reply: oneshot::Sender<Result<(), HqpCoordinatorRefusal>>,
    },
}

struct HqpPublisherCoordinator {
    handle: AdaptiveHandle,
    run: Option<AdapterRun>,
    instances: HashMap<String, HqpInstanceCoordinator>,
    retired_instances: HashMap<String, HqpRetiredInstance>,
    retired_instance_order: VecDeque<String>,
    #[cfg(test)]
    refused_publications: usize,
    stop_requested: bool,
}

impl HqpPublisherCoordinator {
    fn retain_retired_instance(&mut self, instance_name: String, retired: HqpRetiredInstance) {
        self.retired_instance_order
            .retain(|retained| retained != &instance_name);
        self.retired_instances
            .insert(instance_name.clone(), retired);
        self.retired_instance_order.push_back(instance_name);
        while self.retired_instance_order.len() > MAX_RETIRED_INSTANCES {
            if let Some(expired) = self.retired_instance_order.pop_front() {
                self.retired_instances.remove(&expired);
            }
        }
    }

    fn remove_retired_instance(&mut self, instance_name: &str) {
        self.retired_instances.remove(instance_name);
        self.retired_instance_order
            .retain(|retained| retained != instance_name);
    }

    fn has_deferred_commits(&self) -> bool {
        self.instances
            .values()
            .any(|state| state.deferred_commit.is_some())
    }

    fn has_requested_retirements(&self) -> bool {
        self.instances.values().any(|state| {
            state.retirement_requested.is_some()
                && !state
                    .ledger
                    .iter()
                    .any(|operation| operation.outcome == CommandOutcome::Pending)
        })
    }

    fn finish_stop_if_quiescent(&mut self) {
        let quiescent = self.instances.values().all(|state| {
            state.deferred_commit.is_none()
                && state.dispatch_in_progress.is_empty()
                && !has_pending_operations(state)
        });
        if self.stop_requested && quiescent {
            self.run.take();
        }
    }

    async fn retry_deferred_commits(&mut self) {
        let names = self.instances.keys().cloned().collect::<Vec<_>>();
        for name in names {
            let Some(mut state) = self.instances.remove(&name) else {
                continue;
            };
            let admitted = if let Some(commit) = state.deferred_commit.clone() {
                self.publish_deferred(&mut state, commit).await.is_ok()
            } else {
                true
            };
            if !admitted {
                // Retained for the next bounded timer tick. The exact candidate is immutable, so
                // retries cannot allocate a new operation or repeat native I/O.
                self.instances.insert(name, state);
                continue;
            }
            if let Some(epoch) = state.retirement_requested {
                if !has_pending_operations(&state) {
                    // The command actor may have been between native receipt and coordinator
                    // completion when removal arrived. Retire only after that receipt's terminal
                    // publication is admitted, never by guessing from a Pending record.
                    let _ = self.retire_admitted_instance(name, state, epoch).await;
                    continue;
                }
            }
            if self.stop_requested {
                // A deferred completion commit must become admitted before the
                // stop transition can build on its revision. Chain the stop transition now rather
                // than dropping the run with a freshly admitted Pending operation.
                let _ = self
                    .terminalize_pending_without_dispatch(
                        &mut state,
                        contract_timestamp(SystemTime::now()),
                        "HQPlayer manager stopped before native execution",
                    )
                    .await;
            }
            self.instances.insert(name, state);
        }
        self.finish_stop_if_quiescent();
    }

    async fn retire_admitted_instance(
        &mut self,
        instance_name: String,
        state: HqpInstanceCoordinator,
        producer_epoch: ProducerEpoch,
    ) -> Result<(), HqpCoordinatorRefusal> {
        debug_assert!(!has_pending_operations(&state));
        debug_assert!(state.dispatch_in_progress.is_empty());
        if let Some(run) = self.run.as_ref() {
            let producer_id = format!("hqplayer:{instance_name}");
            let retirement = self
                .handle
                .retire(run, &producer_id, producer_epoch)
                .await
                .map_err(|error| HqpCoordinatorRefusal::RetirementFailed(error.to_string()));
            match retirement {
                Ok(RetirementOutcome::Retired { .. } | RetirementOutcome::Committed) => {}
                Ok(outcome) => {
                    self.instances.insert(instance_name, state);
                    return Err(HqpCoordinatorRefusal::RetirementFailed(format!(
                        "{outcome:?}"
                    )));
                }
                Err(error) => {
                    self.instances.insert(instance_name, state);
                    return Err(error);
                }
            }
        }
        self.retain_retired_instance(
            instance_name,
            HqpRetiredInstance {
                producer_epoch,
                ledger: state.ledger,
                correlations: state.correlations,
            },
        );
        Ok(())
    }

    async fn publish_deferred(
        &mut self,
        state: &mut HqpInstanceCoordinator,
        commit: HqpDeferredCommit,
    ) -> Result<(), HqpCoordinatorRefusal> {
        let document = commit
            .tracker
            .last_document
            .clone()
            .ok_or(HqpCoordinatorRefusal::ProducerUnknown)?;
        match self.publish_terminal(document).await {
            Ok(()) => {
                state.tracker = commit.tracker;
                state.ledger = commit.ledger;
                state.active_pipeline = commit.active_pipeline;
                state.dispatch_in_progress = commit.dispatch_in_progress;
                state.correlation_tombstones = commit.correlation_tombstones;
                state.correlation_tombstone_order = commit.correlation_tombstone_order;
                state.deferred_commit = None;
                retain_live_correlations(state);
                Ok(())
            }
            Err(error) => {
                state.deferred_commit = Some(commit);
                Err(error)
            }
        }
    }

    async fn run(mut self, mut commands: mpsc::Receiver<HqpCoordinatorCommand>) {
        let mut retry = tokio::time::interval(std::time::Duration::from_millis(100));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                maybe_command = commands.recv() => {
                    let Some(command) = maybe_command else { break };
                    match command {
                HqpCoordinatorCommand::ManagerStarted { reply } => {
                    let _ = reply.send(self.manager_started());
                }
                HqpCoordinatorCommand::Observed { observation, reply } => {
                    let result = self.observed(*observation).await;
                    let _ = reply.send(result);
                }
                HqpCoordinatorCommand::TransientFailure {
                    instance_name,
                    observed_at,
                    reply,
                } => {
                    let result = self.transient_failure(instance_name, observed_at).await;
                    let _ = reply.send(result);
                }
                HqpCoordinatorCommand::InstanceRemoved {
                    instance_name,
                    producer_epoch,
                    reply,
                } => {
                    let result = self.instance_removed(instance_name, producer_epoch).await;
                    let _ = reply.send(result);
                }
                HqpCoordinatorCommand::ManagerStopped { reply } => {
                    let result = self.manager_stopped().await;
                    let _ = reply.send(result);
                }
                HqpCoordinatorCommand::Reserve { command, reply } => {
                    let result = self.reserve(command).await;
                    let _ = reply.send(result);
                }
                HqpCoordinatorCommand::LookupCorrelation { request, reply } => {
                    let _ = reply.send(self.lookup_correlation(&request));
                }
                #[cfg(test)]
                HqpCoordinatorCommand::Validate { lease, reply } => {
                    let _ = reply.send(self.validate(&lease));
                }
                HqpCoordinatorCommand::BeginDispatch { lease, reply } => {
                    let _ = reply.send(self.begin_dispatch(&lease));
                }
                HqpCoordinatorCommand::Complete {
                    lease,
                    completion,
                    reply,
                } => {
                    let result = self.complete(&lease, completion).await;
                    let _ = reply.send(result);
                }
                HqpCoordinatorCommand::SupersedeStaleBeforeDispatch { lease, at, reply } => {
                    let result = self.supersede_stale_before_dispatch(&lease, at).await;
                    let _ = reply.send(result);
                }
                #[cfg(test)]
                HqpCoordinatorCommand::RefuseNextPublication { reply } => {
                    self.refused_publications += 1;
                    let _ = reply.send(Ok(()));
                }
                    }
                }
                _ = retry.tick(), if self.has_deferred_commits() || self.has_requested_retirements() => {
                    self.retry_deferred_commits().await;
                }
            }
        }
        // Dropping the sole, non-clone run lease synchronously ends publisher liveness.
        self.run.take();
    }

    fn manager_started(&mut self) -> Result<(), HqpCoordinatorRefusal> {
        if self.run.is_none() {
            self.run = Some(self.handle.begin_run("hqplayer"));
            self.stop_requested = false;
        }
        Ok(())
    }

    async fn manager_stopped(&mut self) -> Result<(), HqpCoordinatorRefusal> {
        if self.run.is_none() {
            return Ok(());
        }
        self.stop_requested = true;
        let stopped_at = contract_timestamp(SystemTime::now());
        let instance_names = self.instances.keys().cloned().collect::<Vec<_>>();
        let mut first_failure = None;
        for instance_name in instance_names {
            let Some(mut state) = self.instances.remove(&instance_name) else {
                continue;
            };
            let result = if let Some(commit) = state.deferred_commit.clone() {
                match self.publish_deferred(&mut state, commit).await {
                    Ok(()) => {
                        self.terminalize_pending_without_dispatch(
                            &mut state,
                            stopped_at.clone(),
                            "HQPlayer manager stopped before native execution",
                        )
                        .await
                    }
                    Err(error) => Err(error),
                }
            } else {
                self.terminalize_pending_without_dispatch(
                    &mut state,
                    stopped_at.clone(),
                    "HQPlayer manager stopped before native execution",
                )
                .await
            };
            self.instances.insert(instance_name, state);
            if let Err(error) = result {
                first_failure.get_or_insert(error);
            }
        }
        if let Some(error) = first_failure {
            return Err(error);
        }
        // The sole run is dropped only after every pending audit transition was admitted and no
        // executor-begun fence is awaiting typed receipt truth.
        self.finish_stop_if_quiescent();
        Ok(())
    }

    async fn terminalize_pending_without_dispatch(
        &mut self,
        state: &mut HqpInstanceCoordinator,
        at: Timestamp,
        detail: &str,
    ) -> Result<(), HqpCoordinatorRefusal> {
        let mut candidate_ledger = state.ledger.clone();
        let mut changed = false;
        for operation in &mut candidate_ledger {
            if operation.outcome != CommandOutcome::Pending {
                continue;
            }
            if operation
                .generation
                .is_some_and(|generation| state.dispatch_in_progress.contains(&generation))
            {
                // The executor call began, but its receipt has not reached the coordinator. Stop
                // cannot call this a no-send or discard the operation; the typed completion owns
                // that truth and will clear the fence.
                continue;
            }
            operation.write_attempt = WriteAttempt::NotAttempted;
            operation.recovery = Some(RecoveryState::ReplanRequired);
            operation
                .transition(
                    CommandOutcome::Superseded,
                    at.clone(),
                    None,
                    Some(Reason {
                        code: ReasonCode::Superseded,
                        scope: ReasonScope::Producer,
                        display_text_key: None,
                        detail: Some(detail.to_string()),
                    }),
                )
                .map_err(|_| HqpCoordinatorRefusal::IllegalTransition)?;
            changed = true;
        }
        let dispatch_still_in_progress = candidate_ledger.iter().any(|operation| {
            operation.outcome == CommandOutcome::Pending
                && operation
                    .generation
                    .is_some_and(|generation| state.dispatch_in_progress.contains(&generation))
        });
        if !changed {
            return if dispatch_still_in_progress {
                Err(HqpCoordinatorRefusal::LeaseStillCurrent)
            } else {
                Ok(())
            };
        }

        let mut correlation_tombstones = state.correlation_tombstones.clone();
        let mut correlation_tombstone_order = state.correlation_tombstone_order.clone();
        prune_terminal_history(
            &mut candidate_ledger,
            &state.correlations,
            &mut correlation_tombstones,
            &mut correlation_tombstone_order,
        );
        let mut candidate_document = state
            .tracker
            .last_document
            .clone()
            .ok_or(HqpCoordinatorRefusal::ProducerUnknown)?;
        candidate_document.operations = candidate_ledger.clone();
        let mut candidate_tracker = state.tracker.clone();
        candidate_tracker
            .materialize(candidate_document)
            .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?;
        self.publish_deferred(
            state,
            HqpDeferredCommit {
                tracker: candidate_tracker,
                ledger: candidate_ledger,
                active_pipeline: None,
                dispatch_in_progress: state.dispatch_in_progress.clone(),
                correlation_tombstones,
                correlation_tombstone_order,
            },
        )
        .await?;
        if dispatch_still_in_progress {
            Err(HqpCoordinatorRefusal::LeaseStillCurrent)
        } else {
            Ok(())
        }
    }

    fn lookup_correlation(
        &self,
        request: &HqpImmediateCommandRequest,
    ) -> Result<Option<HqpCommandReservation>, HqpCoordinatorRefusal> {
        validate_correlation(&request.correlation_id)?;
        let instance_name = request
            .key
            .producer_id
            .strip_prefix("hqplayer:")
            .filter(|name| !name.is_empty())
            .ok_or(HqpCoordinatorRefusal::ProducerUnknown)?;
        let expected_fingerprint = fingerprint(request);
        if let Some(state) = self.instances.get(instance_name) {
            if let Some(existing) = state.correlations.get(&request.correlation_id) {
                return reservation_for_correlation(existing, &state.ledger, &expected_fingerprint)
                    .map(Some);
            }
            if state
                .correlation_tombstones
                .contains_key(&request.correlation_id)
            {
                return Err(HqpCoordinatorRefusal::CorrelationConflict);
            }
        }
        if let Some(retired) = self.retired_instances.get(instance_name) {
            if let Some(existing) = retired.correlations.get(&request.correlation_id) {
                return reservation_for_correlation(
                    existing,
                    &retired.ledger,
                    &expected_fingerprint,
                )
                .map(Some);
            }
        }
        Ok(None)
    }

    async fn publish(&mut self, document: ProducerDocument) -> Result<(), HqpCoordinatorRefusal> {
        #[cfg(test)]
        if self.refused_publications > 0 {
            self.refused_publications -= 1;
            return Err(HqpCoordinatorRefusal::PublicationFailed(
                "injected publication refusal".to_string(),
            ));
        }
        let run = self
            .run
            .as_ref()
            .ok_or(HqpCoordinatorRefusal::LifecycleNotRunning)?;
        match self
            .handle
            .publish(run, document)
            .await
            .map_err(|error| HqpCoordinatorRefusal::PublicationFailed(error.to_string()))?
        {
            Admission::Admitted(_) => Ok(()),
            Admission::Refused(refusal) => Err(HqpCoordinatorRefusal::PublicationFailed(format!(
                "{refusal:?}"
            ))),
        }
    }

    async fn publish_terminal(
        &mut self,
        document: ProducerDocument,
    ) -> Result<(), HqpCoordinatorRefusal> {
        let mut last_failure = None;
        for _ in 0..=TERMINAL_PUBLICATION_RETRIES {
            match self.publish(document.clone()).await {
                Ok(()) => return Ok(()),
                Err(error @ HqpCoordinatorRefusal::PublicationFailed(_)) => {
                    last_failure = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
        Err(last_failure.unwrap_or_else(|| {
            HqpCoordinatorRefusal::PublicationFailed(
                "terminal publication retry budget was empty".to_string(),
            )
        }))
    }

    async fn observed(
        &mut self,
        observation: HqpNativeObservation,
    ) -> Result<(), HqpCoordinatorRefusal> {
        if self.run.is_none() || self.stop_requested {
            return Err(HqpCoordinatorRefusal::LifecycleNotRunning);
        }
        let instance_name = observation.instance_name.clone();
        let execution_target = observation.execution_target.clone();
        let mut projected = project_native(&observation).map_err(|error| {
            HqpCoordinatorRefusal::RevisionFailed(format!("projection: {error:?}"))
        })?;
        if let Some(retired) = self.retired_instances.get(&instance_name) {
            if observation.producer_epoch <= retired.producer_epoch.0 {
                return Err(HqpCoordinatorRefusal::StaleLease);
            }
            // A strictly newer producer epoch is the only observation that can supersede an
            // instance retirement. Its correlation namespace is fresh by producer identity.
            self.remove_retired_instance(&instance_name);
        }
        let mut state = self.instances.remove(&instance_name).unwrap_or_default();
        if let Some(commit) = state.deferred_commit.clone() {
            // A deferred terminal commit is a serialization frontier. Materializing this poll
            // from the prior tracker would otherwise offer the aggregator a different document
            // at the terminal candidate's exact revision and strand the operation forever.
            if let Err(error) = self.publish_deferred(&mut state, commit).await {
                self.instances.insert(instance_name, state);
                return Err(error);
            }
        }
        let mut candidate_ledger = state.ledger.clone();
        let converged = reconcile_operations(
            &projected,
            &mut candidate_ledger,
            &state.dispatch_in_progress,
        )?;
        let mut correlation_tombstones = state.correlation_tombstones.clone();
        let mut correlation_tombstone_order = state.correlation_tombstone_order.clone();
        prune_terminal_history(
            &mut candidate_ledger,
            &state.correlations,
            &mut correlation_tombstones,
            &mut correlation_tombstone_order,
        );
        projected.operations = candidate_ledger.clone();
        let mut candidate_tracker = state.tracker.clone();
        let mut candidate = candidate_tracker
            .materialize(projected)
            .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?
            .document;
        if !converged.is_empty() {
            let observed_revision = candidate.position();
            for operation in &mut candidate_ledger {
                if !converged.contains(&operation.id) {
                    continue;
                }
                operation.observed = Some(observed_revision);
                if let Some(transition) = operation.history.last_mut() {
                    transition.observed = Some(observed_revision);
                }
            }
            // Adding the citable revision does not create a second state transition: rematerialize
            // from the admitted base so the observation and operation share one atomic position.
            candidate.operations = candidate_ledger.clone();
            candidate_tracker = state.tracker.clone();
            candidate = candidate_tracker
                .materialize(candidate)
                .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?
                .document;
        }
        let result = self.publish(candidate).await;
        if result.is_ok() {
            state.tracker = candidate_tracker;
            state.ledger = candidate_ledger;
            state.correlation_tombstones = correlation_tombstones;
            state.correlation_tombstone_order = correlation_tombstone_order;
            state.execution_target = Some(execution_target);
            if state.active_pipeline.is_some_and(|generation| {
                !state.ledger.iter().any(|operation| {
                    operation.generation == Some(generation)
                        && operation.outcome == CommandOutcome::Pending
                })
            }) {
                state.active_pipeline = None;
            }
            retain_live_correlations(&mut state);
        }
        self.instances.insert(instance_name, state);
        result
    }

    async fn transient_failure(
        &mut self,
        instance_name: String,
        observed_at: SystemTime,
    ) -> Result<(), HqpCoordinatorRefusal> {
        if self.run.is_none() || self.stop_requested {
            return Err(HqpCoordinatorRefusal::LifecycleNotRunning);
        }
        let mut state = self.instances.remove(&instance_name).unwrap_or_default();
        if let Some(commit) = state.deferred_commit.clone() {
            if let Err(error) = self.publish_deferred(&mut state, commit).await {
                self.instances.insert(instance_name, state);
                return Err(error);
            }
        }
        let mut candidate_tracker = state.tracker.clone();
        let candidate = candidate_tracker
            .mark_transient_failure(contract_timestamp(observed_at))
            .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?
            .map(|outcome| outcome.document);
        let result = if let Some(mut document) = candidate {
            document.operations = state.ledger.clone();
            // Re-materialize after overlaying the ledger. Ordinarily this is content-identical,
            // but it keeps the invariant local if a future projector retains fewer records.
            candidate_tracker = state.tracker.clone();
            let candidate = candidate_tracker
                .materialize(document)
                .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?
                .document;
            let result = self.publish(candidate).await;
            if result.is_ok() {
                state.tracker = candidate_tracker;
            }
            result
        } else {
            Ok(())
        };
        self.instances.insert(instance_name, state);
        result
    }

    async fn instance_removed(
        &mut self,
        instance_name: String,
        producer_epoch: u64,
    ) -> Result<(), HqpCoordinatorRefusal> {
        let Some(mut state) = self.instances.remove(&instance_name) else {
            return Ok(());
        };
        if let Some(commit) = state.deferred_commit.clone() {
            if let Err(error) = self.publish_deferred(&mut state, commit).await {
                self.instances.insert(instance_name, state);
                return Err(error);
            }
        }
        let epoch = ProducerEpoch(producer_epoch);
        if has_pending_operations(&state) {
            // `execute_reserved_native_setting` serializes removal against native I/O, but its
            // typed receipt reaches this actor only after that method returns. Therefore removal
            // may beat `complete` while Pending actually has write evidence in flight. Keep the
            // coordinator and correlation alive, block new work, and retire only after the actor
            // has admitted the receipt's terminal truth.
            state.retirement_requested = Some(epoch);
            self.instances.insert(instance_name, state);
            return Ok(());
        }
        self.retire_admitted_instance(instance_name, state, epoch)
            .await
    }

    async fn reserve(
        &mut self,
        command: HqpValidatedCommand,
    ) -> Result<HqpCommandReservation, HqpCoordinatorRefusal> {
        validate_correlation(&command.request.correlation_id)?;
        if self.stop_requested {
            return Err(HqpCoordinatorRefusal::LifecycleNotRunning);
        }
        let run_id = self
            .run
            .as_ref()
            .map(AdapterRun::id)
            .ok_or(HqpCoordinatorRefusal::LifecycleNotRunning)?;
        let instance_name = command
            .request
            .key
            .producer_id
            .strip_prefix("hqplayer:")
            .filter(|name| !name.is_empty())
            .ok_or(HqpCoordinatorRefusal::ProducerUnknown)?
            .to_string();
        let mut state = self
            .instances
            .remove(&instance_name)
            .ok_or(HqpCoordinatorRefusal::ProducerUnknown)?;
        if let Some(commit) = state.deferred_commit.clone() {
            if let Err(error) = self.publish_deferred(&mut state, commit).await {
                self.instances.insert(instance_name, state);
                return Err(error);
            }
        }

        let result = self.reserve_for_instance(&mut state, command, run_id).await;
        self.instances.insert(instance_name, state);
        result
    }

    async fn reserve_for_instance(
        &mut self,
        state: &mut HqpInstanceCoordinator,
        command: HqpValidatedCommand,
        run_id: AdapterRunId,
    ) -> Result<HqpCommandReservation, HqpCoordinatorRefusal> {
        if state
            .correlation_tombstones
            .contains_key(&command.request.correlation_id)
        {
            // The public audit record was compacted, but replaying an old gesture must never
            // allocate a fresh generation or repeat native I/O.
            return Err(HqpCoordinatorRefusal::CorrelationConflict);
        }
        if let Some(existing) = state.correlations.get(&command.request.correlation_id) {
            if existing.fingerprint != command.fingerprint {
                return Err(HqpCoordinatorRefusal::CorrelationConflict);
            }
            let operation = state
                .ledger
                .iter()
                .find(|operation| operation.id == existing.lease.operation_id)
                .cloned()
                .ok_or(HqpCoordinatorRefusal::StaleLease)?;
            return Ok(HqpCommandReservation {
                lease: existing.lease.clone(),
                operation,
                disposition: HqpReservationDisposition::Duplicate,
            });
        }
        if state.retirement_requested.is_some() {
            return Err(HqpCoordinatorRefusal::LifecycleNotRunning);
        }

        let document = state
            .tracker
            .last_document
            .as_ref()
            .cloned()
            .ok_or(HqpCoordinatorRefusal::ProducerUnknown)?;
        let snapshot = ProducerSnapshot {
            key: ProducerKey::of(&document),
            document: Arc::new(document),
            lanes: BTreeMap::new(),
            presence: ProducerPresence::Live,
            repairs: Vec::new(),
            last_refusal: None,
        };
        let checked =
            preflight(&snapshot, &command.request).map_err(HqpCoordinatorRefusal::Preflight)?;
        if checked.fingerprint != command.fingerprint {
            return Err(HqpCoordinatorRefusal::CorrelationConflict);
        }
        if state.active_pipeline.is_some() {
            return Err(HqpCoordinatorRefusal::ConflictBusy);
        }
        if state
            .ledger
            .iter()
            .filter(|operation| operation_is_unresolved(operation))
            .count()
            >= MAX_UNRESOLVED_OPERATIONS
        {
            return Err(HqpCoordinatorRefusal::UnresolvedRetentionFull);
        }

        let generation = state.next_generation.checked_add(1).ok_or_else(|| {
            HqpCoordinatorRefusal::RevisionFailed("operation generation exhausted".to_string())
        })?;
        let producer_epoch = snapshot.document.producer.epoch;
        let execution_target = state
            .execution_target
            .clone()
            .filter(|target| {
                target.instance_name
                    == instance_name_from_producer_id(&command.request.key.producer_id)
                    && target.producer_epoch == producer_epoch.0
            })
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        let operation_id = OperationId::new(format!(
            "{}:operation:{generation}",
            command.request.key.producer_id
        ));
        let lease = HqpCommandLease {
            producer_id: command.request.key.producer_id.clone(),
            producer_epoch,
            run_id: Some(run_id),
            conflict_set: HqpConflictSet::Pipeline,
            generation,
            correlation_id: command.request.correlation_id.clone(),
            operation_id: operation_id.clone(),
            execution_target: Some(execution_target),
        };
        let operation = OperationRecord {
            id: operation_id,
            correlation_id: command.request.correlation_id.clone(),
            request: Some(OperationRequest {
                control: command.request.control.clone(),
                requested: command.request.requested.clone(),
                lane: command.request.lane.clone(),
                base: command.request.expected,
            }),
            change_set: None,
            generation: Some(generation),
            write_attempt: WriteAttempt::NotAttempted,
            outcome: CommandOutcome::Pending,
            observed: Some(command.request.expected),
            steps: Vec::new(),
            revision_boundaries: Vec::new(),
            history: Vec::new(),
            recovery: None,
            reason: None,
            extensions: Extensions::default(),
        };

        let mut candidate_ledger = state.ledger.clone();
        candidate_ledger.push(operation.clone());
        let mut candidate_document = (*snapshot.document).clone();
        candidate_document.operations = candidate_ledger.clone();
        let mut candidate_tracker = state.tracker.clone();
        let candidate = candidate_tracker
            .materialize(candidate_document)
            .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?
            .document;
        self.publish(candidate).await?;

        state.tracker = candidate_tracker;
        state.ledger = candidate_ledger;
        state.next_generation = generation;
        state.active_pipeline = Some(generation);
        state.correlations.insert(
            command.request.correlation_id,
            HqpCorrelationReservation {
                fingerprint: checked.fingerprint,
                lease: lease.clone(),
            },
        );
        Ok(HqpCommandReservation {
            lease,
            operation,
            disposition: HqpReservationDisposition::New,
        })
    }

    fn validate(&self, lease: &HqpCommandLease) -> Result<(), HqpCoordinatorRefusal> {
        let active_run = self.run.as_ref().map(AdapterRun::id);
        if lease.run_id.is_none() || lease.run_id != active_run {
            return Err(HqpCoordinatorRefusal::StaleLease);
        }
        let instance_name = lease
            .producer_id
            .strip_prefix("hqplayer:")
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        let state = self
            .instances
            .get(instance_name)
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        if state.tracker.epoch != Some(lease.producer_epoch.0)
            || lease.conflict_set != HqpConflictSet::Pipeline
            || state.active_pipeline != Some(lease.generation)
        {
            return Err(HqpCoordinatorRefusal::StaleLease);
        }
        let Some(correlation) = state.correlations.get(&lease.correlation_id) else {
            return Err(HqpCoordinatorRefusal::StaleLease);
        };
        if correlation.lease != *lease {
            return Err(HqpCoordinatorRefusal::StaleLease);
        }
        let pending = state.ledger.iter().any(|operation| {
            operation.id == lease.operation_id
                && operation.generation == Some(lease.generation)
                && operation.outcome == CommandOutcome::Pending
        });
        pending
            .then_some(())
            .ok_or(HqpCoordinatorRefusal::StaleLease)
    }

    fn begin_dispatch(&mut self, lease: &HqpCommandLease) -> Result<(), HqpCoordinatorRefusal> {
        if self.stop_requested {
            return Err(HqpCoordinatorRefusal::LifecycleNotRunning);
        }
        self.validate(lease)?;
        let instance_name = lease
            .producer_id
            .strip_prefix("hqplayer:")
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        let state = self
            .instances
            .get_mut(instance_name)
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        if state.retirement_requested.is_some() {
            return Err(HqpCoordinatorRefusal::StaleLease);
        }
        if !state.dispatch_in_progress.insert(lease.generation) {
            return Err(HqpCoordinatorRefusal::LeaseStillCurrent);
        }
        Ok(())
    }

    /// Return already-published terminal truth for a late actor callback. In particular, instance
    /// retirement removes the document from the public view, but must not make a completion retry
    /// look like a fresh lease or make the accepted operation's audit disappear.
    fn retained_terminal_operation(&self, lease: &HqpCommandLease) -> Option<OperationRecord> {
        let instance_name = lease.producer_id.strip_prefix("hqplayer:")?;
        let retired = self
            .retired_instances
            .get(instance_name)
            .map(|state| (&state.ledger, &state.correlations));
        retired.into_iter().find_map(|(ledger, correlations)| {
            let correlation = correlations.get(&lease.correlation_id)?;
            (correlation.lease == *lease).then_some(())?;
            ledger
                .iter()
                .find(|operation| {
                    operation.id == lease.operation_id
                        && operation.generation == Some(lease.generation)
                        && operation.outcome != CommandOutcome::Pending
                })
                .cloned()
        })
    }

    async fn complete(
        &mut self,
        lease: &HqpCommandLease,
        completion: HqpCommandCompletion,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        if self.validate(lease).is_err() {
            return self
                .retained_terminal_operation(lease)
                .ok_or(HqpCoordinatorRefusal::StaleLease);
        }
        let instance_name = lease
            .producer_id
            .strip_prefix("hqplayer:")
            .ok_or(HqpCoordinatorRefusal::StaleLease)?
            .to_string();
        let mut state = self
            .instances
            .remove(&instance_name)
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        match self
            .complete_for_instance(&mut state, lease, completion)
            .await
        {
            Ok(completed) => {
                if let Some(epoch) = state.retirement_requested {
                    if !has_pending_operations(&state) {
                        self.retire_admitted_instance(instance_name, state, epoch)
                            .await?;
                        self.finish_stop_if_quiescent();
                        return Ok(completed);
                    }
                }
                self.instances.insert(instance_name, state);
                self.finish_stop_if_quiescent();
                Ok(completed)
            }
            Err(error) => {
                self.instances.insert(instance_name, state);
                Err(error)
            }
        }
    }

    async fn complete_for_instance(
        &mut self,
        state: &mut HqpInstanceCoordinator,
        lease: &HqpCommandLease,
        completion: HqpCommandCompletion,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        let mut candidate_ledger = state.ledger.clone();
        let operation = candidate_ledger
            .iter_mut()
            .find(|operation| {
                operation.id == lease.operation_id && operation.generation == Some(lease.generation)
            })
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        operation.write_attempt = completion.write_attempt;
        operation.recovery = completion.recovery;
        operation
            // The native receipt may justify the outcome, but it cannot cite an adaptive
            // observation revision that has not been projected and admitted yet. A later
            // coherent observation can supply that revision without back-dating evidence.
            .transition(completion.outcome, completion.at, None, completion.reason)
            .map_err(|_| HqpCoordinatorRefusal::IllegalTransition)?;
        let completed = operation.clone();

        let mut correlation_tombstones = state.correlation_tombstones.clone();
        let mut correlation_tombstone_order = state.correlation_tombstone_order.clone();
        prune_terminal_history(
            &mut candidate_ledger,
            &state.correlations,
            &mut correlation_tombstones,
            &mut correlation_tombstone_order,
        );

        let mut candidate_document = state
            .tracker
            .last_document
            .clone()
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        candidate_document.operations = candidate_ledger.clone();
        let mut candidate_tracker = state.tracker.clone();
        candidate_tracker
            .materialize(candidate_document)
            .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?;
        let mut dispatch_in_progress = state.dispatch_in_progress.clone();
        dispatch_in_progress.remove(&lease.generation);
        self.publish_deferred(
            state,
            HqpDeferredCommit {
                tracker: candidate_tracker,
                ledger: candidate_ledger,
                active_pipeline: None,
                dispatch_in_progress,
                correlation_tombstones,
                correlation_tombstone_order,
            },
        )
        .await?;
        Ok(completed)
    }

    async fn supersede_stale_before_dispatch(
        &mut self,
        lease: &HqpCommandLease,
        at: Timestamp,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        if let Some(terminal) = self.retained_terminal_operation(lease) {
            return Ok(terminal);
        }
        let active_run = self.run.as_ref().map(AdapterRun::id);
        let instance_name = lease
            .producer_id
            .strip_prefix("hqplayer:")
            .ok_or(HqpCoordinatorRefusal::StaleLease)?
            .to_string();
        let mut state = self
            .instances
            .remove(&instance_name)
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        let result = self
            .supersede_stale_for_instance(&mut state, lease, active_run, self.stop_requested, at)
            .await;
        match result {
            Ok(superseded) => {
                if let Some(epoch) = state.retirement_requested {
                    if !has_pending_operations(&state) {
                        self.retire_admitted_instance(instance_name, state, epoch)
                            .await?;
                        self.finish_stop_if_quiescent();
                        return Ok(superseded);
                    }
                }
                self.instances.insert(instance_name, state);
                self.finish_stop_if_quiescent();
                Ok(superseded)
            }
            Err(error) => {
                self.instances.insert(instance_name, state);
                Err(error)
            }
        }
    }

    async fn supersede_stale_for_instance(
        &mut self,
        state: &mut HqpInstanceCoordinator,
        lease: &HqpCommandLease,
        active_run: Option<AdapterRunId>,
        lifecycle_stopping: bool,
        at: Timestamp,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        if state.dispatch_in_progress.contains(&lease.generation) {
            return Err(HqpCoordinatorRefusal::LeaseStillCurrent);
        }
        let invalidated = lease.run_id != active_run
            || state.tracker.epoch != Some(lease.producer_epoch.0)
            || state.retirement_requested.is_some()
            || lifecycle_stopping;
        if !invalidated {
            return Err(HqpCoordinatorRefusal::LeaseStillCurrent);
        }
        let correlation = state
            .correlations
            .get(&lease.correlation_id)
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        if correlation.lease != *lease {
            return Err(HqpCoordinatorRefusal::StaleLease);
        }
        if let Some(operation) = state.ledger.iter().find(|operation| {
            operation.id == lease.operation_id
                && operation.generation == Some(lease.generation)
                && operation.outcome == CommandOutcome::Superseded
                && operation.write_attempt == WriteAttempt::NotAttempted
        }) {
            return Ok(operation.clone());
        }
        if state.active_pipeline != Some(lease.generation) {
            return Err(HqpCoordinatorRefusal::StaleLease);
        }

        let mut candidate_ledger = state.ledger.clone();
        let operation = candidate_ledger
            .iter_mut()
            .find(|operation| {
                operation.id == lease.operation_id
                    && operation.generation == Some(lease.generation)
                    && operation.outcome == CommandOutcome::Pending
            })
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        operation.write_attempt = WriteAttempt::NotAttempted;
        operation.recovery = Some(RecoveryState::ReplanRequired);
        let reason = Reason {
            code: ReasonCode::Superseded,
            scope: ReasonScope::Producer,
            display_text_key: None,
            detail: Some(
                "producer run or epoch advanced before HQPlayer native dispatch".to_string(),
            ),
        };
        operation
            .transition(CommandOutcome::Superseded, at, None, Some(reason))
            .map_err(|_| HqpCoordinatorRefusal::IllegalTransition)?;
        let superseded = operation.clone();

        let mut correlation_tombstones = state.correlation_tombstones.clone();
        let mut correlation_tombstone_order = state.correlation_tombstone_order.clone();
        prune_terminal_history(
            &mut candidate_ledger,
            &state.correlations,
            &mut correlation_tombstones,
            &mut correlation_tombstone_order,
        );

        let mut candidate_document = state
            .tracker
            .last_document
            .clone()
            .ok_or(HqpCoordinatorRefusal::StaleLease)?;
        candidate_document.operations = candidate_ledger.clone();
        let mut candidate_tracker = state.tracker.clone();
        candidate_tracker
            .materialize(candidate_document)
            .map_err(|error| HqpCoordinatorRefusal::RevisionFailed(format!("{error:?}")))?;
        self.publish_deferred(
            state,
            HqpDeferredCommit {
                tracker: candidate_tracker,
                ledger: candidate_ledger,
                active_pipeline: None,
                dispatch_in_progress: state.dispatch_in_progress.clone(),
                correlation_tombstones,
                correlation_tombstone_order,
            },
        )
        .await?;
        Ok(superseded)
    }
}

fn validate_correlation(correlation_id: &str) -> Result<(), HqpCoordinatorRefusal> {
    if correlation_id.is_empty() || correlation_id.len() > MAX_CORRELATION_ID_BYTES {
        return Err(HqpCoordinatorRefusal::InvalidCorrelation);
    }
    Ok(())
}

fn instance_name_from_producer_id(producer_id: &str) -> &str {
    producer_id.strip_prefix("hqplayer:").unwrap_or_default()
}

fn operation_is_unresolved(operation: &OperationRecord) -> bool {
    operation.outcome.awaits_convergence()
}

fn has_pending_operations(state: &HqpInstanceCoordinator) -> bool {
    state
        .ledger
        .iter()
        .any(|operation| operation.outcome == CommandOutcome::Pending)
}

fn semantic_request_matches(document: &ProducerDocument, request: &OperationRequest) -> bool {
    let Some(control) = document.control(&request.control) else {
        return false;
    };
    if control.observed() == Some(&request.requested) {
        return true;
    }

    // HQPlayer reports its `[source]` mode through the contract's meaningful `Empty` value. It is
    // still selected through the runtime choice identity, so convergence must compare that choice's
    // engine identity rather than treating every `Empty` observation as a match.
    control.id.as_str() == CONTROL_PIPELINE_MODE
        && control.observed() == Some(&ControlValue::Empty)
        && matches!(
            &request.requested,
            ControlValue::Choice(requested)
                if control.choices.as_ref().is_some_and(|choices| {
                    choices.choices.iter().any(|choice| {
                        choice.id == *requested && choice.engine_name.as_deref() == Some("[source]")
                    })
                })
        )
}

fn reconcile_operations(
    document: &ProducerDocument,
    ledger: &mut [OperationRecord],
    dispatch_in_progress: &HashSet<u64>,
) -> Result<HashSet<OperationId>, HqpCoordinatorRefusal> {
    let mut converged = HashSet::new();
    for index in 0..ledger.len() {
        if !ledger[index].outcome.awaits_convergence() {
            continue;
        }
        let Some(request) = ledger[index].request.clone() else {
            continue;
        };
        let matches = semantic_request_matches(document, &request);
        let generation = ledger[index].generation.unwrap_or_default();
        let producer_restarted = request.base.epoch != document.producer.epoch;
        let has_later_generation = ledger.iter().skip(index + 1).any(|later| {
            later.generation.unwrap_or_default() > generation
                && later
                    .request
                    .as_ref()
                    .is_some_and(|later_request| later_request.control == request.control)
        });
        let executor_begun = dispatch_in_progress.contains(&generation);

        let resolution = if executor_begun {
            // The final lease check passed and the executor call began, but no receipt exists yet.
            // Observations may update producer state while that call is in flight; they cannot
            // claim this operation was a no-send or otherwise pre-empt its typed receipt.
            None
        } else if producer_restarted || (!matches && has_later_generation) {
            Some((
                CommandOutcome::Superseded,
                RecoveryState::ReplanRequired,
                Some(Reason {
                    code: ReasonCode::Superseded,
                    scope: ReasonScope::Producer,
                    display_text_key: None,
                    detail: Some(if producer_restarted {
                        "HQPlayer producer epoch advanced before convergence".to_string()
                    } else {
                        "a later HQPlayer operation generation replaced this request".to_string()
                    }),
                }),
            ))
        } else if matches {
            // A matching poll can race a queued command before the native executor runs. It is
            // authoritative evidence that the requested value is already present, but it is not
            // evidence that this operation sent anything. Keep that distinction explicit so the
            // actor cannot later turn a no-send into a claimed write.
            let no_send = ledger[index].outcome == CommandOutcome::Pending
                && ledger[index].write_attempt == WriteAttempt::NotAttempted;
            Some((
                if no_send {
                    CommandOutcome::Ignored
                } else {
                    CommandOutcome::Applied
                },
                RecoveryState::NotRequired,
                None,
            ))
        } else if matches!(
            ledger[index].outcome,
            CommandOutcome::Indeterminate | CommandOutcome::TimedOut
        ) {
            Some((
                CommandOutcome::Divergent,
                RecoveryState::ReplanRequired,
                None,
            ))
        } else {
            None
        };

        let Some((outcome, recovery, reason)) = resolution else {
            continue;
        };
        let operation = &mut ledger[index];
        operation.write_attempt = if outcome == CommandOutcome::Applied {
            WriteAttempt::Confirmed
        } else {
            operation.write_attempt.clone()
        };
        operation.recovery = Some(recovery);
        operation
            .transition(
                outcome,
                document
                    .lanes
                    .iter()
                    .find(|lane| lane.lane == TransportLane::Native)
                    .and_then(|lane| lane.freshness.observed_at.clone())
                    .unwrap_or_else(|| Timestamp::new("unknown")),
                None,
                reason,
            )
            .map_err(|_| HqpCoordinatorRefusal::IllegalTransition)?;
        converged.insert(operation.id.clone());
    }
    Ok(converged)
}

/// Keep every unresolved operation and only the newest bounded terminal audit records. Compacting
/// a document record does not forget its correlation: a separate bounded tombstone prevents an
/// old gesture from becoming a fresh native write merely because its audit fell off the surface.
fn prune_terminal_history(
    ledger: &mut Vec<OperationRecord>,
    correlations: &HashMap<String, HqpCorrelationReservation>,
    tombstones: &mut HashMap<String, HqpCommandFingerprint>,
    tombstone_order: &mut VecDeque<String>,
) {
    let terminal_count = ledger
        .iter()
        .filter(|operation| !operation_is_unresolved(operation))
        .count();
    let mut terminal_to_drop = terminal_count.saturating_sub(MAX_TERMINAL_OPERATIONS);
    if terminal_to_drop == 0 {
        return;
    }
    ledger.retain(|operation| {
        if terminal_to_drop > 0 && !operation_is_unresolved(operation) {
            terminal_to_drop -= 1;
            if let Some(reservation) = correlations.get(&operation.correlation_id) {
                retain_correlation_tombstone(
                    tombstones,
                    tombstone_order,
                    operation.correlation_id.clone(),
                    reservation.fingerprint.clone(),
                );
            }
            false
        } else {
            true
        }
    });
}

fn retain_correlation_tombstone(
    tombstones: &mut HashMap<String, HqpCommandFingerprint>,
    order: &mut VecDeque<String>,
    correlation_id: String,
    fingerprint: HqpCommandFingerprint,
) {
    if tombstones
        .insert(correlation_id.clone(), fingerprint)
        .is_none()
    {
        order.push_back(correlation_id);
    }
    while order.len() > MAX_CORRELATION_TOMBSTONES {
        if let Some(expired) = order.pop_front() {
            tombstones.remove(&expired);
        }
    }
}

fn reservation_for_correlation(
    existing: &HqpCorrelationReservation,
    ledger: &[OperationRecord],
    expected_fingerprint: &HqpCommandFingerprint,
) -> Result<HqpCommandReservation, HqpCoordinatorRefusal> {
    if existing.fingerprint != *expected_fingerprint {
        return Err(HqpCoordinatorRefusal::CorrelationConflict);
    }
    let operation = ledger
        .iter()
        .find(|operation| operation.id == existing.lease.operation_id)
        .cloned()
        .ok_or(HqpCoordinatorRefusal::StaleLease)?;
    Ok(HqpCommandReservation {
        lease: existing.lease.clone(),
        operation,
        disposition: HqpReservationDisposition::Duplicate,
    })
}

fn retain_live_correlations(state: &mut HqpInstanceCoordinator) {
    let retained = state
        .ledger
        .iter()
        .map(|operation| operation.id.clone())
        .collect::<HashSet<_>>();
    state
        .correlations
        .retain(|_, reservation| retained.contains(&reservation.lease.operation_id));
}

impl HqpAdaptivePublisher {
    pub fn new(handle: AdaptiveHandle) -> Self {
        const COORDINATOR_CAPACITY: usize = 32;
        let (commands, receiver) = mpsc::channel(COORDINATOR_CAPACITY);
        tokio::spawn(
            HqpPublisherCoordinator {
                handle,
                run: None,
                instances: HashMap::new(),
                retired_instances: HashMap::new(),
                retired_instance_order: VecDeque::new(),
                #[cfg(test)]
                refused_publications: 0,
                stop_requested: false,
            }
            .run(receiver),
        );
        Self { commands }
    }

    async fn request<T>(
        &self,
        build: impl FnOnce(oneshot::Sender<Result<T, HqpCoordinatorRefusal>>) -> HqpCoordinatorCommand,
    ) -> Result<T, HqpCoordinatorRefusal> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(build(reply))
            .await
            .map_err(|_| HqpCoordinatorRefusal::CoordinatorClosed)?;
        response
            .await
            .map_err(|_| HqpCoordinatorRefusal::CoordinatorClosed)?
    }

    pub(crate) async fn reserve(
        &self,
        command: HqpValidatedCommand,
    ) -> Result<HqpCommandReservation, HqpCoordinatorRefusal> {
        self.request(|reply| HqpCoordinatorCommand::Reserve { command, reply })
            .await
    }

    /// Find an exact prior correlation before checking the caller's now-stale base revision.
    pub(crate) async fn lookup_correlation(
        &self,
        request: &HqpImmediateCommandRequest,
    ) -> Result<Option<HqpCommandReservation>, HqpCoordinatorRefusal> {
        self.request(|reply| HqpCoordinatorCommand::LookupCorrelation {
            request: request.clone(),
            reply,
        })
        .await
    }

    #[cfg(test)]
    pub(crate) async fn validate(
        &self,
        lease: &HqpCommandLease,
    ) -> Result<(), HqpCoordinatorRefusal> {
        self.request(|reply| HqpCoordinatorCommand::Validate {
            lease: lease.clone(),
            reply,
        })
        .await
    }

    /// Atomically perform the final lease check and fence this generation as executing.
    /// The fence is internal ordering state, not evidence that a native write was attempted.
    pub(crate) async fn begin_dispatch(
        &self,
        lease: &HqpCommandLease,
    ) -> Result<(), HqpCoordinatorRefusal> {
        self.request(|reply| HqpCoordinatorCommand::BeginDispatch {
            lease: lease.clone(),
            reply,
        })
        .await
    }

    #[cfg(test)]
    async fn refuse_next_publication(&self) {
        self.request(|reply| HqpCoordinatorCommand::RefuseNextPublication { reply })
            .await
            .expect("test coordinator accepts publication fault");
    }

    pub(crate) async fn complete(
        &self,
        lease: &HqpCommandLease,
        completion: HqpCommandCompletion,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        self.request(|reply| HqpCoordinatorCommand::Complete {
            lease: lease.clone(),
            completion,
            reply,
        })
        .await
    }

    /// Terminalize an exact Pending lease which became stale before native dispatch.
    ///
    /// This is deliberately separate from completion: only the command actor at its
    /// immediately-before-I/O validation point can truthfully assert `NotAttempted`.
    pub(crate) async fn supersede_stale_before_dispatch(
        &self,
        lease: &HqpCommandLease,
        at: Timestamp,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        self.request(
            |reply| HqpCoordinatorCommand::SupersedeStaleBeforeDispatch {
                lease: lease.clone(),
                at,
                reply,
            },
        )
        .await
    }
}

#[async_trait::async_trait]
impl HqpNativeObservationSink for HqpAdaptivePublisher {
    async fn manager_started(&self) -> Result<()> {
        self.request(|reply| HqpCoordinatorCommand::ManagerStarted { reply })
            .await
            .map_err(|error| anyhow!("HQPlayer coordinator start failed: {error:?}"))
    }

    async fn observed(&self, observation: HqpNativeObservation) -> Result<()> {
        self.request(|reply| HqpCoordinatorCommand::Observed {
            observation: Box::new(observation),
            reply,
        })
        .await
        .map_err(|error| anyhow!("HQPlayer coordinator observation failed: {error:?}"))
    }

    async fn transient_failure(&self, instance_name: &str, observed_at: SystemTime) -> Result<()> {
        self.request(|reply| HqpCoordinatorCommand::TransientFailure {
            instance_name: instance_name.to_string(),
            observed_at,
            reply,
        })
        .await
        .map_err(|error| anyhow!("HQPlayer coordinator failure update failed: {error:?}"))
    }

    async fn instance_removed(&self, instance_name: &str, producer_epoch: u64) -> Result<()> {
        self.request(|reply| HqpCoordinatorCommand::InstanceRemoved {
            instance_name: instance_name.to_string(),
            producer_epoch,
            reply,
        })
        .await
        .map_err(|error| anyhow!("HQPlayer coordinator retirement failed: {error:?}"))
    }

    async fn manager_stopped(&self) -> Result<()> {
        // Ending the shared lease after all native workers join makes every retained HQPlayer
        // document last-known. Ordinary shutdown does not retire producer identities.
        self.request(|reply| HqpCoordinatorCommand::ManagerStopped { reply })
            .await
            .map_err(|error| anyhow!("HQPlayer coordinator stop failed: {error:?}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::hqplayer::{HqpNativeExecutionTarget, HqpNativeMetadata, HqpNativeVolume};
    use crate::adaptive::{
        ApplyLane, AvailabilityState, CommandOutcome, ControlId, ControlKind, ControlValue,
        LaneState, OperationId, OperationRecord, OperationRequest, RevisionRef, TargetRole,
        ValueLane, WriteAttempt,
    };
    use crate::producers::{admit, Admission, AdmissionKind};
    use std::time::Duration;

    fn sample() -> HqpNativeObservation {
        HqpNativeObservation {
            instance_name: "main".to_string(),
            instance_label: Some("Listening room".to_string()),
            product_version: Some("6.0.2".to_string()),
            producer_epoch: 7,
            execution_target: HqpNativeExecutionTarget {
                instance_name: "main".to_string(),
                producer_epoch: 7,
                endpoint_generation: 1,
                transport_generation: 1,
            },
            observed_at: SystemTime::UNIX_EPOCH + Duration::from_secs(1_786_016_400),
            transport: HqpNativeTransportState::Playing,
            metadata: HqpNativeMetadata {
                track_id: Some("track-42".to_string()),
                title: Some("Signal".to_string()),
                artist: Some("Artist".to_string()),
                album: Some("Album".to_string()),
            },
            volume: HqpNativeVolume {
                value_db: -18.5,
                min_db: -60.0,
                max_db: 0.0,
                step_db: None,
                enabled: true,
                adaptive: false,
            },
            mode_is_source: true,
            mode: HqpNativeSelection {
                selected: "[source]".to_string(),
                choices: vec!["[source]".to_string(), "PCM".to_string(), "SDM".to_string()],
            },
            filter_1x: HqpNativeSelection {
                selected: "poly-sinc-gauss".to_string(),
                choices: vec![
                    "poly-sinc-gauss".to_string(),
                    "future/filter:alpha".to_string(),
                ],
            },
            filter_nx: HqpNativeSelection {
                selected: "future/filter:alpha".to_string(),
                choices: vec![
                    "poly-sinc-gauss".to_string(),
                    "future/filter:alpha".to_string(),
                ],
            },
            shaper: HqpNativeSelection {
                selected: "NS9".to_string(),
                choices: vec!["NS9".to_string(), "ASDM7EC-super".to_string()],
            },
            rate: HqpNativeSelection {
                selected: "0".to_string(),
                choices: vec!["0".to_string(), "192000".to_string()],
            },
        }
    }

    fn control<'a>(document: &'a ProducerDocument, id: &str) -> &'a crate::adaptive::Control {
        document
            .control(&ControlId::new(id))
            .unwrap_or_else(|| panic!("missing control {id}"))
    }

    fn observed_choice(document: &ProducerDocument, id: &str) -> String {
        match control(document, id).observed() {
            Some(ControlValue::Choice(choice)) => choice.clone(),
            other => panic!("{id} did not carry an observed choice: {other:?}"),
        }
    }

    fn pending_operation(base: RevisionRef) -> OperationRecord {
        OperationRecord {
            id: OperationId::new("hqplayer:main:operation:1"),
            correlation_id: "gesture-1".to_string(),
            request: Some(OperationRequest {
                control: ControlId::new(CONTROL_PIPELINE_MODE),
                requested: ControlValue::choice(choice_id(CONTROL_PIPELINE_MODE, "PCM")),
                lane: ApplyLane::Immediate,
                base,
            }),
            change_set: None,
            generation: Some(1),
            write_attempt: WriteAttempt::NotAttempted,
            outcome: CommandOutcome::Pending,
            observed: Some(base),
            steps: Vec::new(),
            revision_boundaries: Vec::new(),
            history: Vec::new(),
            recovery: None,
            reason: None,
            extensions: Extensions::default(),
        }
    }

    #[test]
    fn operation_only_changes_advance_state_revision_without_advancing_control_plane() {
        let mut tracker = RevisionTracker::default();
        let first = tracker
            .materialize(project_native(&sample()).expect("initial projection"))
            .expect("initial revision");
        let base = first.document.position();

        let mut with_pending = project_native(&sample()).expect("pending projection");
        with_pending.operations.push(pending_operation(base));
        let pending = tracker
            .materialize(with_pending)
            .expect("operation-only revision");

        assert!(!pending.control_plane_advanced);
        assert!(pending.state_advanced);
        assert_eq!(
            pending.document.revisions,
            DocumentRevisions::new(1, 2),
            "admitting Pending must be visible as a producer state change"
        );
        assert_eq!(pending.document.operations.len(), 1);
    }

    #[test]
    fn projects_a_truthful_native_document_without_indexes_or_an_invented_step() {
        let document = project_native(&sample()).expect("coherent sample projects");

        assert_eq!(document.producer.producer_id, "hqplayer:main");
        assert_eq!(document.producer.epoch.0, 7);
        assert_eq!(document.target.role, TargetRole::DspEngine);
        assert_eq!(document.lanes.len(), 1);
        assert_eq!(document.lanes[0].state, LaneState::Connected);
        assert!(document
            .lane_health(&crate::adaptive::TransportLane::Persistent)
            .is_none());
        assert!(document
            .lane_health(&crate::adaptive::TransportLane::Telemetry)
            .is_none());

        let volume = control(&document, "hqplayer.volume.level");
        assert_eq!(volume.kind, ControlKind::NumericRange);
        assert_eq!(volume.range.as_ref().expect("volume range").step, None);
        assert_eq!(volume.observed(), Some(&ControlValue::Decimal(-18.5)));

        for id in [
            "hqplayer.pipeline.filter_1x",
            "hqplayer.pipeline.filter_nx",
            "hqplayer.pipeline.shaper",
            "hqplayer.pipeline.rate",
        ] {
            let setting = control(&document, id);
            assert_eq!(setting.kind, ControlKind::Enumeration);
            let selected = observed_choice(&document, id);
            let choices = setting.choices.as_ref().expect("runtime choices");
            assert!(choices.contains(&selected));
            assert!(choices
                .choices
                .iter()
                .all(|choice| choice.engine_name.is_some()));
            assert!(choices
                .choices
                .iter()
                .all(|choice| !choice.id.chars().all(|c| c.is_ascii_digit())));
        }

        let mode = control(&document, CONTROL_PIPELINE_MODE);
        assert_eq!(mode.observed(), Some(&ControlValue::Empty));
        assert!(
            mode.choices
                .as_ref()
                .expect("mode runtime choices")
                .choices
                .iter()
                .any(|choice| choice.engine_name.as_deref() == Some("[source]")),
            "the source identity stays offered even though its observed value is Empty"
        );
        let rate = control(&document, CONTROL_PIPELINE_RATE);
        assert_eq!(rate.availability.state, AvailabilityState::Unavailable);
        assert!(rate.apply.is_none(), "source-mode SetRate is suppressed");
        assert_eq!(
            rate.availability.reasons[0].code,
            crate::adaptive::ReasonCode::NotApplicableInMode
        );

        let unknown = control(&document, "hqplayer.pipeline.filter_1x")
            .choices
            .as_ref()
            .expect("choices")
            .choices
            .iter()
            .find(|choice| choice.engine_name.as_deref() == Some("future/filter:alpha"))
            .expect("unknown runtime choice survives");
        assert_ne!(unknown.id, "future/filter:alpha");
    }

    #[test]
    fn rejects_a_selection_that_does_not_belong_to_the_verified_runtime_set() {
        let mut observation = sample();
        observation.filter_nx.selected = "not-in-this-chain".to_string();

        assert_eq!(
            project_native(&observation),
            Err(ProjectionError::SelectedChoiceMissing {
                control_id: "hqplayer.pipeline.filter_nx",
                selected: "not-in-this-chain".to_string(),
            })
        );
    }

    #[test]
    fn source_semantics_come_from_the_coherent_mode_family_not_a_display_name() {
        let mut observation = sample();
        observation.mode.selected = "follow-input".to_string();
        observation.mode.choices = vec!["follow-input".to_string(), "PCM".to_string()];
        observation.mode_is_source = true;

        let document = project_native(&observation).expect("source-family alias projects");
        assert_eq!(
            control(&document, CONTROL_PIPELINE_MODE).observed(),
            Some(&ControlValue::Empty)
        );
        let rate = control(&document, CONTROL_PIPELINE_RATE);
        assert_eq!(rate.availability.state, AvailabilityState::Unavailable);
        assert!(rate.apply.is_none());
    }

    #[test]
    fn stopped_retained_metadata_is_not_published_as_now_playing() {
        let mut observation = sample();
        observation.transport = HqpNativeTransportState::Stopped;
        let document = project_native(&observation).expect("stopped observation projects");

        for id in [
            "hqplayer.metadata.track_id",
            "hqplayer.metadata.title",
            "hqplayer.metadata.artist",
            "hqplayer.metadata.album",
        ] {
            let value = control(&document, id)
                .lane(&ValueLane::Observed)
                .expect("metadata has an explicit observed lane");
            assert!(value.grounded_value().is_none());
        }
    }

    #[test]
    fn fixed_volume_remains_visible_but_is_not_advertised_as_mutable() {
        let mut observation = sample();
        observation.volume.enabled = false;
        let document = project_native(&observation).expect("fixed volume projects");
        let volume = control(&document, "hqplayer.volume.level");

        assert_eq!(volume.availability.state, AvailabilityState::ReadOnly);
        assert_eq!(
            volume.availability.reasons[0].code,
            crate::adaptive::ReasonCode::LockedByProducer
        );
        assert!(volume.apply.is_none());
        assert_eq!(volume.observed(), Some(&ControlValue::Decimal(-18.5)));
        assert!(matches!(
            admit(None, document),
            Admission::Admitted(admitted) if admitted.kind == AdmissionKind::Fresh
        ));
    }

    #[test]
    fn transport_actions_are_advertised_with_provisional_sibling_verification() {
        let document = project_native(&sample()).expect("playing observation projects");

        for (id, verify_via) in [
            ("hqplayer.transport.play", "hqplayer.transport.state"),
            ("hqplayer.transport.pause", "hqplayer.transport.state"),
            ("hqplayer.transport.stop", "hqplayer.transport.state"),
            ("hqplayer.transport.previous", "hqplayer.metadata.track_id"),
            ("hqplayer.transport.next", "hqplayer.metadata.track_id"),
        ] {
            let action = control(&document, id);
            assert_eq!(action.kind, ControlKind::Action);
            let apply = action.apply.as_ref().expect("action has an execution path");
            assert_eq!(apply.effect, ApplyEffect::VerifiedPending);
            assert!(apply.verification.provisional_ack);
            assert_eq!(
                apply.verification.verify_via.as_ref(),
                Some(&ControlId::new(verify_via))
            );
        }

        let track_id = control(&document, "hqplayer.metadata.track_id");
        assert_eq!(track_id.kind, ControlKind::Text);
        assert!(!track_id.is_mutable());
    }

    #[test]
    fn every_advertised_mutable_control_has_one_semantic_adapter_operation() {
        let mut observation = sample();
        // Exercise the capability-maximal document: source mode suppresses rate writes, so it
        // cannot prove the rate descriptor stays coupled to its semantic operation.
        observation.mode_is_source = false;
        let document = project_native(&observation).expect("fully mutable observation projects");

        let advertised = document
            .controls
            .iter()
            .filter(|control| control.is_mutable())
            .map(|control| control.id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let registered = hqp_semantic_operation_registry()
            .iter()
            .map(|entry| entry.control_id)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(advertised, registered);
        assert_eq!(
            hqp_semantic_operation_registry().len(),
            registered.len(),
            "a control may not dispatch to more than one semantic operation"
        );
        assert_eq!(
            hqp_semantic_operation_registry()
                .iter()
                .map(|entry| (entry.control_id, entry.operation))
                .collect::<std::collections::BTreeMap<_, _>>(),
            std::collections::BTreeMap::from([
                (CONTROL_TRANSPORT_PLAY, HqpSemanticOperation::Play),
                (CONTROL_TRANSPORT_PAUSE, HqpSemanticOperation::Pause),
                (CONTROL_TRANSPORT_STOP, HqpSemanticOperation::Stop),
                (CONTROL_TRANSPORT_PREVIOUS, HqpSemanticOperation::Previous),
                (CONTROL_TRANSPORT_NEXT, HqpSemanticOperation::Next),
                (CONTROL_VOLUME_LEVEL, HqpSemanticOperation::SetVolumeDb),
                (CONTROL_PIPELINE_MODE, HqpSemanticOperation::SetMode),
                (
                    CONTROL_PIPELINE_FILTER_1X,
                    HqpSemanticOperation::SetFilter1x
                ),
                (
                    CONTROL_PIPELINE_FILTER_NX,
                    HqpSemanticOperation::SetFilterNx
                ),
                (CONTROL_PIPELINE_SHAPER, HqpSemanticOperation::SetShaper),
                (CONTROL_PIPELINE_RATE, HqpSemanticOperation::SetRate),
            ])
        );
    }

    #[test]
    fn engine_choice_ids_are_byte_exact_deterministic_and_never_depend_on_native_indexes() {
        let mut observation = sample();
        observation.filter_1x.choices = vec![
            "future/filter:alpha".to_string(),
            "future filter alpha".to_string(),
            "future/filter:alpha".to_string(),
        ];
        observation.filter_1x.selected = "future/filter:alpha".to_string();

        let first = project_native(&observation).expect("choice projection");
        let second = project_native(&observation).expect("same input projects identically");
        let first_choices = control(&first, CONTROL_PIPELINE_FILTER_1X)
            .choices
            .as_ref()
            .expect("choices");
        let second_choices = control(&second, CONTROL_PIPELINE_FILTER_1X)
            .choices
            .as_ref()
            .expect("choices");
        assert_eq!(first_choices, second_choices);
        assert_eq!(
            first_choices.choices.len(),
            2,
            "exact duplicates collapse safely"
        );
        assert_eq!(
            first_choices
                .choices
                .iter()
                .map(|choice| choice.engine_name.as_deref())
                .collect::<Vec<_>>(),
            vec![Some("future/filter:alpha"), Some("future filter alpha")],
            "the runtime's first-occurrence order is retained"
        );
        assert_ne!(first_choices.choices[0].id, first_choices.choices[1].id);
        assert!(first_choices.contains(&observed_choice(&first, CONTROL_PIPELINE_FILTER_1X)));
    }

    #[test]
    fn revision_planes_ignore_lane_time_but_classify_capability_and_value_changes() {
        let mut tracker = RevisionTracker::default();
        let first = tracker
            .materialize(project_native(&sample()).expect("first projection"))
            .expect("first revision");
        assert!(first.control_plane_advanced);
        assert!(first.state_advanced);
        assert_eq!(
            first.document.revisions,
            crate::adaptive::DocumentRevisions::new(1, 1)
        );

        let mut volatile_only = sample();
        volatile_only.observed_at += Duration::from_secs(2);
        let volatile = tracker
            .materialize(project_native(&volatile_only).expect("volatile refresh"))
            .expect("volatile revision");
        assert!(!volatile.control_plane_advanced);
        assert!(!volatile.state_advanced);
        assert_eq!(
            volatile.document.revisions,
            crate::adaptive::DocumentRevisions::new(1, 1)
        );
        assert!(matches!(
            admit(Some(&first.document), volatile.document.clone()),
            Admission::Admitted(admitted) if admitted.kind == AdmissionKind::HealthRefresh
        ));

        let mut value_only = volatile_only.clone();
        value_only.volume.value_db = -17.5;
        let value = tracker
            .materialize(project_native(&value_only).expect("value projection"))
            .expect("value revision");
        assert!(!value.control_plane_advanced);
        assert!(value.state_advanced);
        assert_eq!(
            value.document.revisions,
            crate::adaptive::DocumentRevisions::new(1, 2)
        );

        let mut capability_only = value_only.clone();
        capability_only.mode.choices.push("PCM-ext".to_string());
        let capability = tracker
            .materialize(project_native(&capability_only).expect("capability projection"))
            .expect("capability revision");
        assert!(capability.control_plane_advanced);
        assert!(!capability.state_advanced);
        assert_eq!(
            capability.document.revisions,
            crate::adaptive::DocumentRevisions::new(2, 2)
        );

        let mut both = capability_only;
        both.mode.selected = "PCM-ext".to_string();
        both.mode_is_source = false;
        both.mode.choices.push("SDM-ext".to_string());
        let combined = tracker
            .materialize(project_native(&both).expect("combined projection"))
            .expect("combined revision");
        assert!(combined.control_plane_advanced);
        assert!(combined.state_advanced);
        assert_eq!(
            combined.document.revisions,
            crate::adaptive::DocumentRevisions::new(3, 3)
        );

        let mut reconnected = both;
        reconnected.producer_epoch = 8;
        let reconnect = tracker
            .materialize(project_native(&reconnected).expect("reconnect"))
            .expect("reconnect revision");
        assert!(reconnect.control_plane_advanced);
        assert!(reconnect.state_advanced);
        assert_eq!(
            reconnect.document.revisions,
            crate::adaptive::DocumentRevisions::new(1, 1),
            "revisions reset because a producer epoch is a new comparison domain"
        );
        let no_op_reconnect = tracker
            .materialize(project_native(&reconnected).expect("same epoch"))
            .expect("same-epoch revision");
        assert!(!no_op_reconnect.control_plane_advanced);
        assert!(!no_op_reconnect.state_advanced);
    }

    #[test]
    fn a_stale_lower_producer_epoch_cannot_replace_the_trackers_current_epoch() {
        let mut tracker = RevisionTracker::default();
        let mut current = sample();
        current.producer_epoch = 8;
        let _current = tracker
            .materialize(project_native(&current).expect("current epoch"))
            .expect("current revision");

        let mut stale = current;
        stale.producer_epoch = 7;
        let stale_result = tracker.materialize(project_native(&stale).expect("stale projection"));

        assert_eq!(
            stale_result.err(),
            Some(RevisionError::EpochRegressed {
                current: 8,
                incoming: 7,
            })
        );
        assert_eq!(
            tracker.epoch,
            Some(8),
            "a late observation from an older producer session must be refused"
        );
    }

    #[test]
    fn refuses_invalid_native_identity_and_volume_evidence_before_serialization() {
        let mut blank_instance = sample();
        blank_instance.instance_name = "  ".to_string();
        assert!(project_native(&blank_instance).is_err());

        for invalid in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut observation = sample();
            observation.volume.value_db = invalid;
            assert!(project_native(&observation).is_err());
        }

        let mut reversed = sample();
        reversed.volume.min_db = 1.0;
        reversed.volume.max_db = -1.0;
        assert!(project_native(&reversed).is_err());

        let mut outside = sample();
        outside.volume.value_db = outside.volume.max_db + 0.5;
        assert!(project_native(&outside).is_err());

        for invalid_step in [0.0, -0.5, f64::INFINITY] {
            let mut observation = sample();
            observation.volume.step_db = Some(invalid_step);
            assert!(project_native(&observation).is_err());
        }
    }

    #[test]
    fn blank_playing_metadata_is_unobserved_not_a_grounded_empty_track() {
        let mut observation = sample();
        observation.metadata.track_id = Some("".to_string());
        observation.metadata.title = Some("   ".to_string());
        observation.metadata.artist = Some("".to_string());
        observation.metadata.album = Some("\t".to_string());

        let document = project_native(&observation).expect("blank metadata projects as absent");
        for id in [
            CONTROL_METADATA_TRACK_ID,
            CONTROL_METADATA_TITLE,
            CONTROL_METADATA_ARTIST,
            CONTROL_METADATA_ALBUM,
        ] {
            assert!(
                control(&document, id)
                    .lane(&ValueLane::Observed)
                    .expect("metadata lane")
                    .grounded_value()
                    .is_none(),
                "{id} must not ground an empty value"
            );
        }
    }

    #[test]
    fn revision_overflow_refuses_atomically_and_a_new_epoch_recovers() {
        let mut tracker = RevisionTracker::default();
        let current = sample();
        tracker
            .materialize(project_native(&current).expect("current projection"))
            .expect("initial revision");

        tracker.control_plane = u64::MAX;
        let before_state = tracker.state;
        let before_control_view = tracker.previous_control_view.clone();
        let before_state_view = tracker.previous_state_view.clone();
        let before_document = tracker.last_document.clone();
        let mut changed_capability = current.clone();
        changed_capability.volume.max_db = -1.0;

        assert_eq!(
            tracker
                .materialize(project_native(&changed_capability).expect("capability projection"))
                .err(),
            Some(RevisionError::CounterExhausted {
                plane: "control_plane"
            })
        );
        assert_eq!(tracker.control_plane, u64::MAX);
        assert_eq!(tracker.state, before_state);
        assert_eq!(tracker.previous_control_view, before_control_view);
        assert_eq!(tracker.previous_state_view, before_state_view);
        assert_eq!(tracker.last_document, before_document);

        let mut next_epoch = changed_capability;
        next_epoch.producer_epoch += 1;
        let recovered = tracker
            .materialize(project_native(&next_epoch).expect("new-epoch projection"))
            .expect("new epoch resets exhausted counters");
        assert_eq!(recovered.document.revisions, DocumentRevisions::new(1, 1));
    }

    async fn coordinator_fixture() -> (
        HqpAdaptivePublisher,
        crate::producers::AdaptiveView,
        tokio_util::sync::CancellationToken,
        tokio::task::JoinHandle<()>,
    ) {
        let shutdown = tokio_util::sync::CancellationToken::new();
        let (handle, actor, view) = crate::producers::AdaptiveRuntime::build(shutdown.clone(), 16);
        let actor_task = tokio::spawn(actor.run());
        let publisher = HqpAdaptivePublisher::new(handle);
        publisher.manager_started().await.expect("manager starts");
        publisher.observed(sample()).await.expect("sample admitted");
        (publisher, view, shutdown, actor_task)
    }

    fn validated_mode_command(
        snapshot: &crate::producers::ProducerSnapshot,
        correlation_id: &str,
        engine_name: &str,
    ) -> crate::producers::hqplayer_command::HqpValidatedCommand {
        let request = crate::producers::hqplayer_command::HqpImmediateCommandRequest {
            key: snapshot.key.clone(),
            expected: snapshot.document.position(),
            control: ControlId::new(CONTROL_PIPELINE_MODE),
            requested: ControlValue::choice(choice_id(CONTROL_PIPELINE_MODE, engine_name)),
            lane: ApplyLane::Immediate,
            correlation_id: correlation_id.to_string(),
        };
        crate::producers::hqplayer_command::preflight(snapshot, &request)
            .expect("mode request passes advisory preflight")
    }

    #[tokio::test]
    async fn coordinator_reserves_pending_before_returning_and_preserves_it_across_observation() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let before = view.snapshot(&key).expect("initial snapshot");

        let reservation = publisher
            .reserve(validated_mode_command(&before, "gesture-1", "PCM"))
            .await
            .expect("pending reservation");

        assert!(!reservation.is_duplicate());
        let pending = view.snapshot(&key).expect("pending snapshot");
        assert_eq!(
            pending.document.revisions.control_plane,
            before.document.revisions.control_plane
        );
        assert_eq!(
            pending.document.revisions.state.0,
            before.document.revisions.state.0 + 1
        );
        assert_eq!(pending.document.operations.len(), 1);
        assert_eq!(
            pending.document.operations[0].outcome,
            CommandOutcome::Pending
        );
        assert_eq!(
            pending.document.operations[0]
                .request
                .as_ref()
                .expect("request")
                .base,
            before.document.position()
        );

        let mut next_observation = sample();
        next_observation.metadata.title = Some("Next signal".to_string());
        publisher
            .observed(next_observation)
            .await
            .expect("later observation admitted");
        let observed = view.snapshot(&key).expect("observed snapshot");
        assert_eq!(observed.document.operations, pending.document.operations);
        assert_eq!(
            observed.document.revisions.state.0,
            pending.document.revisions.state.0 + 1
        );

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_deduplicates_a_correlation_and_refuses_conflicting_reuse() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let snapshot = view.snapshot(&key).expect("initial snapshot");
        let original = validated_mode_command(&snapshot, "gesture-1", "PCM");
        let conflicting = validated_mode_command(&snapshot, "gesture-1", "SDM");

        let first = publisher
            .reserve(original.clone())
            .await
            .expect("first reserve");
        let duplicate = publisher
            .reserve(original)
            .await
            .expect("deduplicated reserve");
        assert!(duplicate.is_duplicate());
        assert_eq!(duplicate.operation().id, first.operation().id);
        assert_eq!(
            view.snapshot(&key)
                .expect("snapshot")
                .document
                .operations
                .len(),
            1
        );

        assert_eq!(
            publisher.reserve(conflicting).await,
            Err(HqpCoordinatorRefusal::CorrelationConflict)
        );

        let pending = view.snapshot(&key).expect("pending snapshot");
        let another_correlation = validated_mode_command(&pending, "gesture-2", "SDM");
        assert_eq!(
            publisher.reserve(another_correlation).await,
            Err(HqpCoordinatorRefusal::ConflictBusy),
            "the five coupled pipeline controls share one conservative conflict set"
        );
        publisher
            .complete(
                first.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Superseded,
                    WriteAttempt::NotAttempted,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    None,
                    None,
                ),
            )
            .await
            .expect("test releases first conflict");

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_rejects_stale_run_lease_and_does_not_publish_its_completion() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let snapshot = view.snapshot(&key).expect("initial snapshot");
        let reservation = publisher
            .reserve(validated_mode_command(&snapshot, "gesture-1", "PCM"))
            .await
            .expect("reserve");
        publisher.manager_stopped().await.expect("old run stops");
        publisher.manager_started().await.expect("new run starts");
        assert_eq!(
            publisher.validate(reservation.lease()).await,
            Err(HqpCoordinatorRefusal::StaleLease)
        );
        assert_eq!(
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Applied,
                        WriteAttempt::Confirmed,
                        Timestamp::new("2026-07-31T00:00:01Z"),
                        None,
                        None,
                    ),
                )
                .await,
            Err(HqpCoordinatorRefusal::StaleLease)
        );
        let stopped = view.snapshot(&key).expect("terminal retained snapshot");
        assert_eq!(
            stopped.document.operations[0].outcome,
            CommandOutcome::Superseded
        );
        assert_eq!(
            stopped.document.operations[0].write_attempt,
            WriteAttempt::NotAttempted
        );

        publisher.manager_stopped().await.expect("new run stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_supersedes_a_pending_generation_invalidated_before_dispatch() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let initial = view.snapshot(&key).expect("initial snapshot");
        let reservation = publisher
            .reserve(validated_mode_command(&initial, "gesture-1", "PCM"))
            .await
            .expect("reserve before producer restart");

        let mut restarted = sample();
        restarted.producer_epoch += 1;
        restarted.execution_target.producer_epoch += 1;
        publisher
            .observed(restarted)
            .await
            .expect("new producer epoch admitted");
        assert_eq!(
            publisher.validate(reservation.lease()).await,
            Err(HqpCoordinatorRefusal::StaleLease)
        );

        let superseded = publisher
            .supersede_stale_before_dispatch(
                reservation.lease(),
                Timestamp::new("2026-07-31T00:00:01Z"),
            )
            .await
            .expect("stale pre-dispatch generation terminalizes");
        assert_eq!(superseded.outcome, CommandOutcome::Superseded);
        assert_eq!(superseded.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(superseded.recovery, Some(RecoveryState::ReplanRequired));
        let reason = superseded.reason.as_ref().expect("supersession reason");
        assert_eq!(reason.code, ReasonCode::Superseded);
        assert_eq!(reason.scope, ReasonScope::Producer);

        let after_invalidation = view.snapshot(&key).expect("superseded snapshot");
        assert_eq!(
            after_invalidation.document.operations[0].outcome,
            CommandOutcome::Superseded
        );
        let next = publisher
            .reserve(validated_mode_command(
                &after_invalidation,
                "gesture-2",
                "SDM",
            ))
            .await
            .expect("released conflict admits later command");
        assert_eq!(
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Applied,
                        WriteAttempt::Confirmed,
                        Timestamp::new("2026-07-31T00:00:02Z"),
                        None,
                        None,
                    ),
                )
                .await,
            Err(HqpCoordinatorRefusal::StaleLease),
            "late completion from the invalidated generation cannot overwrite its successor"
        );
        publisher
            .complete(
                next.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Superseded,
                    WriteAttempt::NotAttempted,
                    Timestamp::new("2026-07-31T00:00:03Z"),
                    None,
                    None,
                ),
            )
            .await
            .expect("test releases successor");

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_terminal_transition_advances_revision_and_releases_conflict_set() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let snapshot = view.snapshot(&key).expect("initial snapshot");
        let reservation = publisher
            .reserve(validated_mode_command(&snapshot, "gesture-1", "PCM"))
            .await
            .expect("reserve");
        let pending = view.snapshot(&key).expect("pending snapshot");

        publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Applied,
                    WriteAttempt::Confirmed,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    None,
                    None,
                ),
            )
            .await
            .expect("terminal completion");

        let complete = view.snapshot(&key).expect("terminal snapshot");
        assert_eq!(
            complete.document.revisions.state.0,
            pending.document.revisions.state.0 + 1
        );
        let operation = &complete.document.operations[0];
        assert_eq!(operation.outcome, CommandOutcome::Applied);
        assert_eq!(operation.write_attempt, WriteAttempt::Confirmed);
        assert_eq!(operation.history.len(), 1);
        assert_eq!(operation.history[0].from, CommandOutcome::Pending);
        assert_eq!(operation.history[0].to, CommandOutcome::Applied);

        let next_snapshot = view.snapshot(&key).expect("completed snapshot");
        let next = publisher
            .reserve(validated_mode_command(&next_snapshot, "gesture-2", "SDM"))
            .await
            .expect("new generation reserves after terminal completion");
        assert_eq!(
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Applied,
                        WriteAttempt::Confirmed,
                        Timestamp::new("2026-07-31T00:00:02Z"),
                        None,
                        None,
                    ),
                )
                .await,
            Err(HqpCoordinatorRefusal::StaleLease),
            "an older generation cannot overwrite the newer Pending operation"
        );
        let indeterminate = publisher
            .complete(
                next.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Indeterminate,
                    WriteAttempt::Attempted,
                    Timestamp::new("2026-07-31T00:00:03Z"),
                    Some(RecoveryState::AwaitingReadback),
                    None,
                ),
            )
            .await
            .expect("native ambiguity is retained truthfully");
        assert_eq!(indeterminate.outcome, CommandOutcome::Indeterminate);
        assert_eq!(indeterminate.write_attempt, WriteAttempt::Attempted);
        assert_eq!(
            indeterminate.recovery,
            Some(RecoveryState::AwaitingReadback)
        );

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_stop_terminalizes_every_pre_receipt_pending_as_no_send_before_run_ends() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let main_key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let mut second_observation = sample();
        second_observation.instance_name = "second".to_string();
        second_observation.execution_target.instance_name = "second".to_string();
        publisher
            .observed(second_observation)
            .await
            .expect("second producer admitted");
        let second_key = crate::producers::ProducerKey {
            producer_id: "hqplayer:second".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };

        let reserved = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&main_key).expect("main snapshot"),
                "reserved-on-stop",
                "PCM",
            ))
            .await
            .expect("reserved operation");
        let second_reserved = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&second_key).expect("second snapshot"),
                "second-reserved-on-stop",
                "PCM",
            ))
            .await
            .expect("second reserved operation");

        publisher.manager_stopped().await.expect("manager stops");

        let reserved_terminal = view.snapshot(&main_key).expect("main retained");
        let reserved_operation = reserved_terminal
            .document
            .operations
            .iter()
            .find(|operation| operation.id == reserved.operation().id)
            .expect("reserved audit retained");
        assert_eq!(reserved_operation.outcome, CommandOutcome::Superseded);
        assert_eq!(reserved_operation.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(
            reserved_operation.recovery,
            Some(RecoveryState::ReplanRequired)
        );
        assert!(reserved_operation.reason.is_some());

        let second_terminal = view.snapshot(&second_key).expect("second retained");
        let second_operation = second_terminal
            .document
            .operations
            .iter()
            .find(|operation| operation.id == second_reserved.operation().id)
            .expect("second audit retained");
        assert_eq!(second_operation.outcome, CommandOutcome::Superseded);
        assert_eq!(second_operation.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(
            second_operation.recovery,
            Some(RecoveryState::ReplanRequired)
        );
        assert!(second_operation.reason.is_some());
        assert_eq!(
            publisher.validate(reserved.lease()).await,
            Err(HqpCoordinatorRefusal::StaleLease)
        );
        assert_eq!(
            publisher.validate(second_reserved.lease()).await,
            Err(HqpCoordinatorRefusal::StaleLease)
        );

        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn instance_removal_defers_retirement_until_inflight_reservation_reports_no_send_truth() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "remove-before-receipt",
                "PCM",
            ))
            .await
            .expect("reservation admitted before removal");

        publisher
            .instance_removed("main", 7)
            .await
            .expect("removal records retirement intent without erasing accepted work");
        assert!(
            view.snapshot(&key).is_some(),
            "the producer remains until the actor reports whether native I/O happened"
        );

        let no_send = publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Superseded,
                    WriteAttempt::NotAttempted,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    Some(RecoveryState::ReplanRequired),
                    None,
                ),
            )
            .await
            .expect("no-send receipt terminalizes before retirement");
        assert_eq!(no_send.outcome, CommandOutcome::Superseded);
        assert_eq!(no_send.write_attempt, WriteAttempt::NotAttempted);
        assert!(
            view.snapshot(&key).is_none(),
            "retirement follows terminal admission"
        );

        let late = publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Applied,
                    WriteAttempt::Confirmed,
                    Timestamp::new("2026-07-31T00:00:02Z"),
                    Some(RecoveryState::NotRequired),
                    None,
                ),
            )
            .await
            .expect("late duplicate reads retained terminal audit rather than disappearing");
        assert_eq!(late.outcome, CommandOutcome::Superseded);
        assert_eq!(late.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(late.recovery, Some(RecoveryState::ReplanRequired));

        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn instance_removal_arriving_before_typed_receipt_completion_preserves_attempt_truth() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "remove-after-receipt",
                "PCM",
            ))
            .await
            .expect("reservation admitted");
        publisher
            .begin_dispatch(reservation.lease())
            .await
            .expect("final lease check fences the begun executor");
        publisher
            .instance_removed("main", 7)
            .await
            .expect("removal waits for the actor's receipt completion");
        assert!(
            view.snapshot(&key).is_some(),
            "removal must not classify a still-Pending write as no-send"
        );
        let retained = publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Indeterminate,
                    WriteAttempt::Attempted,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    Some(RecoveryState::AwaitingReadback),
                    None,
                ),
            )
            .await
            .expect("receipt terminalizes before deferred retirement");
        assert_eq!(retained.outcome, CommandOutcome::Indeterminate);
        assert_eq!(retained.write_attempt, WriteAttempt::Attempted);
        assert!(
            view.snapshot(&key).is_none(),
            "retirement follows attempted receipt truth"
        );

        let duplicate = publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Superseded,
                    WriteAttempt::NotAttempted,
                    Timestamp::new("2026-07-31T00:00:02Z"),
                    Some(RecoveryState::ReplanRequired),
                    None,
                ),
            )
            .await
            .expect("late duplicate preserves the attempted receipt audit");
        assert_eq!(duplicate.outcome, CommandOutcome::Indeterminate);
        assert_eq!(duplicate.write_attempt, WriteAttempt::Attempted);

        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn manager_stop_cannot_terminalize_or_drop_an_executor_begun_operation() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "stop-during-executor",
                "PCM",
            ))
            .await
            .expect("reserve");
        publisher
            .begin_dispatch(reservation.lease())
            .await
            .expect("executor begun fence");

        assert!(publisher.manager_stopped().await.is_err());
        let pending = view.snapshot(&key).expect("in-flight snapshot retained");
        let operation = pending
            .document
            .operations
            .iter()
            .find(|operation| operation.id == reservation.operation().id)
            .expect("in-flight audit retained");
        assert_eq!(operation.outcome, CommandOutcome::Pending);
        assert_eq!(operation.write_attempt, WriteAttempt::NotAttempted);

        publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Indeterminate,
                    WriteAttempt::Attempted,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    Some(RecoveryState::AwaitingReadback),
                    None,
                ),
            )
            .await
            .expect("typed receipt survives stop race");
        let retained = view.snapshot(&key).expect("last-known receipt audit");
        assert_eq!(
            retained.document.operations[0].outcome,
            CommandOutcome::Indeterminate
        );
        assert_eq!(
            retained.document.operations[0].write_attempt,
            WriteAttempt::Attempted
        );

        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_retries_one_terminal_publication_failure_without_stranding_conflict() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "retry-publication",
                "PCM",
            ))
            .await
            .expect("reserve");
        publisher.refuse_next_publication().await;
        publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Applied,
                    WriteAttempt::Confirmed,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    Some(RecoveryState::NotRequired),
                    None,
                ),
            )
            .await
            .expect("coordinator-owned retry admits terminal state");

        let next_snapshot = view.snapshot(&key).expect("terminal snapshot");
        publisher
            .reserve(validated_mode_command(
                &next_snapshot,
                "after-publication-retry",
                "SDM",
            ))
            .await
            .expect("publication retry releases the conflict set");

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_durably_retries_terminal_publication_after_caller_returns() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "durable-publication",
                "PCM",
            ))
            .await
            .expect("reserve");
        for _ in 0..3 {
            publisher.refuse_next_publication().await;
        }
        assert!(matches!(
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Applied,
                        WriteAttempt::Confirmed,
                        Timestamp::new("2026-07-31T00:00:01Z"),
                        Some(RecoveryState::NotRequired),
                        None,
                    ),
                )
                .await,
            Err(HqpCoordinatorRefusal::PublicationFailed(_))
        ));

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if view
                    .snapshot(&key)
                    .expect("snapshot retained")
                    .document
                    .operations[0]
                    .outcome
                    == CommandOutcome::Applied
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("coordinator retries its immutable deferred terminal commit");

        let next_snapshot = view.snapshot(&key).expect("deferred commit admitted");
        publisher
            .reserve(validated_mode_command(
                &next_snapshot,
                "after-durable-retry",
                "SDM",
            ))
            .await
            .expect("admitted deferred commit releases conflict");

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn deferred_terminal_commit_blocks_observation_until_its_revision_is_admitted() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "terminal-frontier",
                "PCM",
            ))
            .await
            .expect("reserve");
        // `complete` owns two immediate terminal attempts; the third fault is consumed by the
        // concurrent observation's frontier flush. It must not publish a competing document at
        // the terminal candidate's same next revision.
        for _ in 0..3 {
            publisher.refuse_next_publication().await;
        }
        assert!(matches!(
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Applied,
                        WriteAttempt::Confirmed,
                        Timestamp::new("2026-07-31T00:00:01Z"),
                        Some(RecoveryState::NotRequired),
                        None,
                    ),
                )
                .await,
            Err(HqpCoordinatorRefusal::PublicationFailed(_))
        ));

        let mut competing_observation = sample();
        competing_observation.metadata.title = Some("must wait behind terminal".to_string());
        publisher
            .observed(competing_observation)
            .await
            .expect("observation either flushes the terminal frontier first or follows it");
        assert_eq!(
            view.snapshot(&key)
                .expect("frontier snapshot")
                .document
                .operations[0]
                .outcome,
            CommandOutcome::Applied,
            "the terminal candidate is admitted before the competing observation can advance"
        );

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if view
                    .snapshot(&key)
                    .expect("terminal snapshot")
                    .document
                    .operations[0]
                    .outcome
                    == CommandOutcome::Applied
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("deferred terminal retry is admitted before a successor");

        let successor = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("terminal snapshot"),
                "successor-after-frontier",
                "SDM",
            ))
            .await
            .expect("successor is usable after terminal frontier clears");
        assert_eq!(successor.operation().outcome, CommandOutcome::Pending);

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coherent_observation_converges_pending_and_source_empty_semantically() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let pcm = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("source snapshot"),
                "to-pcm",
                "PCM",
            ))
            .await
            .expect("PCM reserved");
        let mut pcm_observation = sample();
        pcm_observation.mode_is_source = false;
        pcm_observation.mode.selected = "PCM".to_string();
        publisher
            .observed(pcm_observation.clone())
            .await
            .expect("PCM converged");
        let pcm_snapshot = view.snapshot(&key).expect("PCM snapshot");
        let pcm_operation = pcm_snapshot
            .document
            .operations
            .iter()
            .find(|operation| operation.id == pcm.operation().id)
            .expect("PCM operation");
        assert_eq!(pcm_operation.outcome, CommandOutcome::Ignored);
        assert_eq!(pcm_operation.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(
            pcm_operation.observed,
            Some(pcm_snapshot.document.position())
        );

        let source = publisher
            .reserve(validated_mode_command(
                &pcm_snapshot,
                "to-source",
                "[source]",
            ))
            .await
            .expect("source reserved");
        publisher
            .observed(sample())
            .await
            .expect("source Empty converged");
        let source_snapshot = view.snapshot(&key).expect("source snapshot");
        assert_eq!(
            control(&source_snapshot.document, CONTROL_PIPELINE_MODE).observed(),
            Some(&ControlValue::Empty)
        );
        let source_operation = source_snapshot
            .document
            .operations
            .iter()
            .find(|operation| operation.id == source.operation().id)
            .expect("source operation");
        assert_eq!(source_operation.outcome, CommandOutcome::Ignored);
        assert_eq!(source_operation.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(
            source_operation.observed,
            Some(source_snapshot.document.position())
        );

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn later_generation_supersedes_unresolved_predecessor_during_convergence() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let first = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "first-generation",
                "PCM",
            ))
            .await
            .expect("first reserved");
        publisher
            .complete(
                first.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Indeterminate,
                    WriteAttempt::Attempted,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    Some(RecoveryState::AwaitingReadback),
                    None,
                ),
            )
            .await
            .expect("first unresolved");
        let second = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("unresolved snapshot"),
                "second-generation",
                "SDM",
            ))
            .await
            .expect("second reserved");

        let mut observed = sample();
        observed.mode_is_source = false;
        observed.mode.selected = "SDM".to_string();
        publisher.observed(observed).await.expect("SDM observed");
        let converged = view.snapshot(&key).expect("converged snapshot");
        let first = converged
            .document
            .operations
            .iter()
            .find(|operation| operation.id == first.operation().id)
            .expect("first retained");
        let second = converged
            .document
            .operations
            .iter()
            .find(|operation| operation.id == second.operation().id)
            .expect("second retained");
        assert_eq!(first.outcome, CommandOutcome::Superseded);
        assert_eq!(second.outcome, CommandOutcome::Ignored);
        assert_eq!(second.write_attempt, WriteAttempt::NotAttempted);

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn matching_poll_before_native_execution_resolves_as_ignored_no_send() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "setter-poll-race",
                "PCM",
            ))
            .await
            .expect("reserve");
        let mut racing_poll = sample();
        racing_poll.metadata.title = Some("poll raced setter".to_string());
        publisher
            .observed(racing_poll)
            .await
            .expect("racing observation admitted");
        let raced = view.snapshot(&key).expect("raced snapshot");
        let operation = raced
            .document
            .operations
            .iter()
            .find(|operation| operation.id == reservation.operation().id)
            .expect("operation retained");
        assert_eq!(operation.outcome, CommandOutcome::Pending);
        assert_eq!(operation.write_attempt, WriteAttempt::NotAttempted);

        let mut converged = sample();
        converged.mode_is_source = false;
        converged.mode.selected = "PCM".to_string();
        publisher
            .observed(converged)
            .await
            .expect("later convergence");
        let ignored = view.snapshot(&key).expect("ignored snapshot");
        assert_eq!(
            ignored.document.operations[0].outcome,
            CommandOutcome::Ignored
        );
        assert_eq!(
            ignored.document.operations[0].write_attempt,
            WriteAttempt::NotAttempted
        );

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn executor_begun_matching_observation_waits_for_typed_receipt_and_receipt_wins() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let reservation = publisher
            .reserve(validated_mode_command(
                &view.snapshot(&key).expect("initial"),
                "executor-observation-barrier",
                "PCM",
            ))
            .await
            .expect("reserve");

        publisher
            .begin_dispatch(reservation.lease())
            .await
            .expect("final lease validation and executor-begun fence are atomic");
        let mut matching = sample();
        matching.mode_is_source = false;
        matching.mode.selected = "PCM".to_string();
        publisher
            .observed(matching.clone())
            .await
            .expect("matching producer observation admitted behind fence");
        let while_executing = view.snapshot(&key).expect("executing snapshot");
        let operation = while_executing
            .document
            .operations
            .iter()
            .find(|operation| operation.id == reservation.operation().id)
            .expect("operation retained");
        assert_eq!(operation.outcome, CommandOutcome::Pending);
        assert_eq!(operation.write_attempt, WriteAttempt::NotAttempted);

        publisher
            .complete(
                reservation.lease(),
                HqpCommandCompletion::new(
                    CommandOutcome::Rejected,
                    WriteAttempt::Attempted,
                    Timestamp::new("2026-07-31T00:00:01Z"),
                    Some(RecoveryState::NotRequired),
                    None,
                ),
            )
            .await
            .expect("typed receipt clears fence and establishes operation truth");
        publisher
            .observed(matching)
            .await
            .expect("later matching observation cannot rewrite terminal receipt");
        let terminal = view.snapshot(&key).expect("terminal snapshot");
        let operation = terminal
            .document
            .operations
            .iter()
            .find(|operation| operation.id == reservation.operation().id)
            .expect("terminal audit retained");
        assert_eq!(operation.outcome, CommandOutcome::Rejected);
        assert_eq!(operation.write_attempt, WriteAttempt::Attempted);
        assert_eq!(operation.history.len(), 1);
        assert_eq!(operation.history[0].from, CommandOutcome::Pending);
        assert_eq!(operation.history[0].to, CommandOutcome::Rejected);

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn terminal_ledger_and_correlation_tombstones_are_bounded_but_unresolved_are_retained() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let mut oldest_request = None;
        let mut newest_request = None;
        for index in 0..(MAX_TERMINAL_OPERATIONS + 3) {
            let snapshot = view.snapshot(&key).expect("iteration snapshot");
            let request = crate::producers::hqplayer_command::HqpImmediateCommandRequest {
                key: key.clone(),
                expected: snapshot.document.position(),
                control: ControlId::new(CONTROL_PIPELINE_MODE),
                requested: ControlValue::choice(choice_id(CONTROL_PIPELINE_MODE, "PCM")),
                lane: ApplyLane::Immediate,
                correlation_id: format!("bounded-{index}"),
            };
            if index == 0 {
                oldest_request = Some(request.clone());
            }
            newest_request = Some(request.clone());
            let reservation = publisher
                .reserve(
                    crate::producers::hqplayer_command::preflight(&snapshot, &request)
                        .expect("request preflight"),
                )
                .await
                .expect("reserve");
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Applied,
                        WriteAttempt::Confirmed,
                        Timestamp::new(format!("2026-07-31T00:00:{index:02}Z")),
                        Some(RecoveryState::NotRequired),
                        None,
                    ),
                )
                .await
                .expect("complete");
        }

        let retained = view.snapshot(&key).expect("retained snapshot");
        assert_eq!(retained.document.operations.len(), MAX_TERMINAL_OPERATIONS);
        assert_eq!(
            publisher
                .lookup_correlation(oldest_request.as_ref().expect("oldest"))
                .await,
            Err(HqpCoordinatorRefusal::CorrelationConflict),
            "compacted terminal correlations remain no-replay tombstones"
        );
        assert!(
            publisher
                .lookup_correlation(newest_request.as_ref().expect("newest"))
                .await
                .expect("newest lookup")
                .is_some(),
            "newest terminal correlation remains idempotent"
        );

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn retired_instance_correlation_truth_is_globally_bounded_across_distinct_names() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        publisher
            .instance_removed("main", 7)
            .await
            .expect("fixture instance retires");

        let mut oldest_request = None;
        let mut newest_request = None;
        for index in 0..=MAX_RETIRED_INSTANCES {
            let instance_name = format!("retired-{index}");
            let mut observation = sample();
            observation.instance_name = instance_name.clone();
            observation.instance_label = Some(instance_name.clone());
            observation.execution_target.instance_name = instance_name.clone();
            publisher
                .observed(observation)
                .await
                .expect("distinct instance observation admits");

            let key = crate::producers::ProducerKey {
                producer_id: format!("hqplayer:{instance_name}"),
                role: TargetRole::DspEngine,
                zone_id: None,
            };
            let snapshot = view.snapshot(&key).expect("instance snapshot");
            let request = crate::producers::hqplayer_command::HqpImmediateCommandRequest {
                key,
                expected: snapshot.document.position(),
                control: ControlId::new(CONTROL_PIPELINE_MODE),
                requested: ControlValue::choice(choice_id(CONTROL_PIPELINE_MODE, "PCM")),
                lane: ApplyLane::Immediate,
                correlation_id: format!("retired-correlation-{index}"),
            };
            if index == 0 {
                oldest_request = Some(request.clone());
            }
            newest_request = Some(request.clone());
            let reservation = publisher
                .reserve(
                    crate::producers::hqplayer_command::preflight(&snapshot, &request)
                        .expect("retired request preflight"),
                )
                .await
                .expect("retired request reserves");
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Applied,
                        WriteAttempt::Confirmed,
                        Timestamp::new(format!("2026-07-31T00:02:{:02}Z", index % 60)),
                        Some(RecoveryState::NotRequired),
                        None,
                    ),
                )
                .await
                .expect("retired request completes");
            publisher
                .instance_removed(&instance_name, 7)
                .await
                .expect("completed instance retires");
        }

        assert!(
            publisher
                .lookup_correlation(oldest_request.as_ref().expect("oldest request"))
                .await
                .expect("expired retired lookup is not an error")
                .is_none(),
            "the oldest distinct retired identity is evicted at the global bound"
        );
        assert!(
            publisher
                .lookup_correlation(newest_request.as_ref().expect("newest request"))
                .await
                .expect("newest retired lookup")
                .is_some(),
            "the newest retired identity keeps exact duplicate truth"
        );

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn unresolved_operation_retention_applies_backpressure_instead_of_growing_without_bound()
    {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        for index in 0..MAX_UNRESOLVED_OPERATIONS {
            let reservation = publisher
                .reserve(validated_mode_command(
                    &view.snapshot(&key).expect("current snapshot"),
                    &format!("unresolved-{index}"),
                    "PCM",
                ))
                .await
                .expect("bounded unresolved record admits before cap");
            publisher
                .complete(
                    reservation.lease(),
                    HqpCommandCompletion::new(
                        CommandOutcome::Indeterminate,
                        WriteAttempt::Attempted,
                        Timestamp::new(format!("2026-07-31T00:01:{index:02}Z")),
                        Some(RecoveryState::AwaitingReadback),
                        None,
                    ),
                )
                .await
                .expect("unresolved receipt is retained");
        }
        assert_eq!(
            publisher
                .reserve(validated_mode_command(
                    &view.snapshot(&key).expect("cap snapshot"),
                    "unresolved-over-cap",
                    "PCM",
                ))
                .await,
            Err(HqpCoordinatorRefusal::UnresolvedRetentionFull),
            "the coordinator backpressures rather than silently evicting uncertainty"
        );

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }

    #[tokio::test]
    async fn coordinator_rejects_empty_and_oversized_correlations_even_if_preflight_is_bypassed() {
        let (publisher, view, shutdown, actor_task) = coordinator_fixture().await;
        let key = crate::producers::ProducerKey {
            producer_id: "hqplayer:main".to_string(),
            role: TargetRole::DspEngine,
            zone_id: None,
        };
        let snapshot = view.snapshot(&key).expect("initial");
        for correlation_id in [String::new(), "x".repeat(MAX_CORRELATION_ID_BYTES + 1)] {
            let mut command = validated_mode_command(&snapshot, "valid", "PCM");
            command.request.correlation_id = correlation_id;
            assert_eq!(
                publisher.reserve(command).await,
                Err(HqpCoordinatorRefusal::InvalidCorrelation)
            );
        }

        publisher.manager_stopped().await.expect("manager stops");
        shutdown.cancel();
        actor_task.await.expect("actor joins");
    }
}
