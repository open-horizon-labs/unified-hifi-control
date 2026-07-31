//! Pure HQPlayer native-observation projection into the adaptive producer contract.
//!
//! This module performs no I/O. The adapter owns the generation-fenced native read;
//! the projector accepts exactly one coherent value and cannot reach a socket, cache,
//! clock, or adapter handle.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::SystemTime;

use anyhow::{anyhow, Result};
use tokio::sync::Mutex;

use crate::adapters::hqplayer::{
    HqpNativeObservation, HqpNativeObservationSink, HqpNativeSelection, HqpNativeTransportState,
};
use crate::adaptive::{
    ApplyEffect, ApplyLane, ApplySemantics, Availability, AvailabilityState, Choice, ChoiceSet,
    Control, ControlGroup, ControlId, ControlKind, ControlValue, Disruption, DocumentRevisions,
    Extensions, Freshness, LaneHealth, LaneState, LaneValue, NumericRange, ProducerDocument,
    ProducerEpoch, ProducerIdentity, ProducerTarget, Provenance, Reason, ReasonCode, ReasonScope,
    RiskClass, TargetRole, Timestamp, TransportLane, ValueLane, Verification,
    CONSUMER_SCHEMA_VERSION,
};
use crate::producers::{AdapterRun, AdaptiveHandle, Admission, RetirementOutcome};

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

#[derive(Default)]
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
    canonical.operations.clear();
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
    handle: AdaptiveHandle,
    run: Mutex<Option<Arc<AdapterRun>>>,
    trackers: Mutex<HashMap<String, RevisionTracker>>,
}

impl HqpAdaptivePublisher {
    pub fn new(handle: AdaptiveHandle) -> Self {
        Self {
            handle,
            run: Mutex::new(None),
            trackers: Mutex::new(HashMap::new()),
        }
    }

    async fn current_run(&self) -> Result<Arc<AdapterRun>> {
        self.run
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("HQPlayer producer lifecycle is not running"))
    }

    async fn publish(&self, run: &AdapterRun, document: ProducerDocument) -> Result<()> {
        match self.handle.publish(run, document).await? {
            Admission::Admitted(_) => Ok(()),
            Admission::Refused(refusal) => Err(anyhow!(
                "HQPlayer producer document was refused: {refusal:?}"
            )),
        }
    }
}

#[async_trait::async_trait]
impl HqpNativeObservationSink for HqpAdaptivePublisher {
    async fn manager_started(&self) -> Result<()> {
        let mut run = self.run.lock().await;
        if run.is_none() {
            *run = Some(Arc::new(self.handle.begin_run("hqplayer")));
        }
        Ok(())
    }

    async fn observed(&self, observation: HqpNativeObservation) -> Result<()> {
        let instance_name = observation.instance_name.clone();
        let projected = project_native(&observation).map_err(|error| {
            anyhow!("HQPlayer adaptive projection refused coherent observation: {error:?}")
        })?;
        let document = {
            let mut trackers = self.trackers.lock().await;
            trackers
                .entry(instance_name)
                .or_default()
                .materialize(projected)
                .map_err(|error| {
                    anyhow!("HQPlayer adaptive revision refused observation: {error:?}")
                })?
                .document
        };
        let run = self.current_run().await?;
        self.publish(&run, document).await
    }

    async fn transient_failure(&self, instance_name: &str, observed_at: SystemTime) -> Result<()> {
        let document = {
            let mut trackers = self.trackers.lock().await;
            trackers
                .entry(instance_name.to_string())
                .or_default()
                .mark_transient_failure(contract_timestamp(observed_at))
                .map_err(|error| {
                    anyhow!(
                        "HQPlayer adaptive tracker could not retain failed observation: {error:?}"
                    )
                })?
                .map(|outcome| outcome.document)
        };
        let Some(document) = document else {
            return Ok(());
        };
        let run = self.current_run().await?;
        self.publish(&run, document).await
    }

    async fn instance_removed(&self, instance_name: &str, producer_epoch: u64) -> Result<()> {
        let run = self.run.lock().await.clone();
        if let Some(run) = run {
            let producer_id = format!("hqplayer:{instance_name}");
            match self
                .handle
                .retire(&run, &producer_id, ProducerEpoch(producer_epoch))
                .await?
            {
                RetirementOutcome::Retired { .. } | RetirementOutcome::Committed => {}
                outcome => {
                    return Err(anyhow!(
                        "HQPlayer producer retirement was not applied: {outcome:?}"
                    ));
                }
            }
        }
        self.trackers.lock().await.remove(instance_name);
        Ok(())
    }

    async fn manager_stopped(&self) -> Result<()> {
        // Ending the shared lease after all native workers join makes every retained HQPlayer
        // document last-known. Ordinary shutdown does not retire producer identities.
        self.run.lock().await.take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::hqplayer::{HqpNativeMetadata, HqpNativeVolume};
    use crate::adaptive::{
        AvailabilityState, ControlId, ControlKind, ControlValue, LaneState, TargetRole, ValueLane,
    };
    use crate::producers::{admit, Admission, AdmissionKind};
    use std::time::Duration;

    fn sample() -> HqpNativeObservation {
        HqpNativeObservation {
            instance_name: "main".to_string(),
            instance_label: Some("Listening room".to_string()),
            product_version: Some("6.0.2".to_string()),
            producer_epoch: 7,
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
}
