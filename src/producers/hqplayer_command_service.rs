//! Bounded, actor-owned execution for HQPlayer immediate adaptive commands.
//!
//! Advisory reads and producer reservation deliberately happen before native I/O. Once the
//! bounded actor accepts a message, it owns the reserved operation through completion even if the
//! submitting task disappears.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use crate::adapters::hqplayer::{
    HqpInstanceManager, HqpNativeExecutionTarget, HqpNativeSetting, NativeSettingReadback,
    NativeSettingReceipt,
};
use crate::adaptive::{
    CommandOutcome, OperationRecord, Reason, ReasonCode, ReasonScope, RecoveryState, Timestamp,
    WriteAttempt,
};

use super::control_plane::{
    CommandAcceptance, CommandRefusal, SemanticCommand, SemanticCommandExecutor, SemanticRefusal,
};
use super::hqplayer::{
    HqpAdaptivePublisher, HqpCommandCompletion, HqpCommandLease, HqpCommandReservation,
    HqpCoordinatorRefusal, HqpSemanticOperation,
};
use super::hqplayer_command::{
    preflight, validate_correlation_id, HqpEngineValue, HqpImmediateCommandRequest,
    HqpPreflightRefusal, HqpValidatedCommand,
};
use super::{AdaptiveView, ProducerKey, ProducerSnapshot};

/// A command accepted only after its `Pending` operation has been admitted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HqpCommandAcknowledgement {
    pub operation: OperationRecord,
    pub duplicate: bool,
}

/// Refusals returned before a new native execution is admitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HqpCommandServiceRefusal {
    ProducerUnknown,
    InvalidProducerIdentity,
    Preflight(HqpPreflightRefusal),
    Coordinator(HqpCoordinatorRefusal),
    /// The actor closed before accepting the message. Reached by `HqpSemanticExecutor` (#331).
    ServiceClosed,
}

trait HqpCommandSnapshotView: Send + Sync {
    fn snapshot(&self, key: &ProducerKey) -> Option<ProducerSnapshot>;
}

impl HqpCommandSnapshotView for AdaptiveView {
    fn snapshot(&self, key: &ProducerKey) -> Option<ProducerSnapshot> {
        AdaptiveView::snapshot(self, key)
    }
}

#[async_trait]
trait HqpCommandCoordinator: Send + Sync {
    async fn lookup_correlation(
        &self,
        request: &HqpImmediateCommandRequest,
    ) -> Result<Option<HqpCommandReservation>, HqpCoordinatorRefusal>;

    async fn reserve(
        &self,
        command: HqpValidatedCommand,
    ) -> Result<HqpCommandReservation, HqpCoordinatorRefusal>;

    async fn begin_dispatch(&self, lease: &HqpCommandLease) -> Result<(), HqpCoordinatorRefusal>;

    async fn complete(
        &self,
        lease: &HqpCommandLease,
        completion: HqpCommandCompletion,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal>;

    async fn supersede_stale_before_dispatch(
        &self,
        lease: &HqpCommandLease,
        at: Timestamp,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal>;
}

#[async_trait]
impl HqpCommandCoordinator for HqpAdaptivePublisher {
    async fn lookup_correlation(
        &self,
        request: &HqpImmediateCommandRequest,
    ) -> Result<Option<HqpCommandReservation>, HqpCoordinatorRefusal> {
        HqpAdaptivePublisher::lookup_correlation(self, request).await
    }

    async fn reserve(
        &self,
        command: HqpValidatedCommand,
    ) -> Result<HqpCommandReservation, HqpCoordinatorRefusal> {
        HqpAdaptivePublisher::reserve(self, command).await
    }

    async fn begin_dispatch(&self, lease: &HqpCommandLease) -> Result<(), HqpCoordinatorRefusal> {
        HqpAdaptivePublisher::begin_dispatch(self, lease).await
    }

    async fn complete(
        &self,
        lease: &HqpCommandLease,
        completion: HqpCommandCompletion,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        HqpAdaptivePublisher::complete(self, lease, completion).await
    }

    async fn supersede_stale_before_dispatch(
        &self,
        lease: &HqpCommandLease,
        at: Timestamp,
    ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
        HqpAdaptivePublisher::supersede_stale_before_dispatch(self, lease, at).await
    }
}

#[async_trait]
trait HqpNativeSettingExecutor: Send + Sync {
    async fn execute(
        &self,
        target: &HqpNativeExecutionTarget,
        setting: HqpNativeSetting,
    ) -> anyhow::Result<NativeSettingReceipt>;
}

#[async_trait]
impl HqpNativeSettingExecutor for HqpInstanceManager {
    async fn execute(
        &self,
        target: &HqpNativeExecutionTarget,
        setting: HqpNativeSetting,
    ) -> anyhow::Result<NativeSettingReceipt> {
        self.execute_reserved_native_setting(target, setting).await
    }
}

enum HqpCommandActorMessage {
    // The public consumer landed in #331: `HqpSemanticExecutor` below is the only sender.
    Submit {
        request: Box<HqpImmediateCommandRequest>,
        reply: oneshot::Sender<Result<HqpCommandAcknowledgement, HqpCommandServiceRefusal>>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Cloneable ingress for the bounded immediate-command actor.
#[derive(Clone)]
#[doc(hidden)]
pub struct HqpImmediateCommandHandle {
    commands: mpsc::Sender<HqpCommandActorMessage>,
}

impl HqpImmediateCommandHandle {
    pub(crate) async fn submit(
        &self,
        request: HqpImmediateCommandRequest,
    ) -> Result<HqpCommandAcknowledgement, HqpCommandServiceRefusal> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(HqpCommandActorMessage::Submit {
                request: Box::new(request),
                reply,
            })
            .await
            .map_err(|_| HqpCommandServiceRefusal::ServiceClosed)?;
        response
            .await
            .map_err(|_| HqpCommandServiceRefusal::ServiceClosed)?
    }

    /// Close ingress and wait until every message accepted before closure has terminalized.
    pub async fn shutdown(&self) -> Result<(), HqpCommandShutdownError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(HqpCommandActorMessage::Shutdown { reply })
            .await
            .map_err(|_| HqpCommandShutdownError)?;
        response.await.map_err(|_| HqpCommandShutdownError)
    }
}

/// The actor ended before acknowledging a requested graceful drain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[doc(hidden)]
pub struct HqpCommandShutdownError;

impl std::fmt::Display for HqpCommandShutdownError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HQPlayer command actor closed before completing its drain")
    }
}

impl std::error::Error for HqpCommandShutdownError {}

/// The single owner of accepted command jobs.
#[doc(hidden)]
pub struct HqpImmediateCommandActor {
    commands: mpsc::Receiver<HqpCommandActorMessage>,
    view: Arc<dyn HqpCommandSnapshotView>,
    coordinator: Arc<dyn HqpCommandCoordinator>,
    executor: Arc<dyn HqpNativeSettingExecutor>,
}

fn send_submit_reply(
    reply: oneshot::Sender<Result<HqpCommandAcknowledgement, HqpCommandServiceRefusal>>,
    response: Result<HqpCommandAcknowledgement, HqpCommandServiceRefusal>,
) {
    if reply.send(response).is_err() {
        // The actor owns an accepted reservation independently of caller lifetime. A dropped
        // acknowledgement receiver is observable cancellation, not permission to abandon work.
        tracing::debug!("HQPlayer command submitter disconnected before acknowledgement");
    }
}

impl HqpImmediateCommandActor {
    /// Build a bounded service. Capacity must be non-zero (the same requirement as Tokio's
    /// bounded channel).
    pub fn build(
        view: AdaptiveView,
        coordinator: Arc<HqpAdaptivePublisher>,
        executor: Arc<HqpInstanceManager>,
        capacity: usize,
    ) -> (HqpImmediateCommandHandle, Self) {
        Self::build_with(Arc::new(view), coordinator, executor, capacity)
    }

    fn build_with(
        view: Arc<dyn HqpCommandSnapshotView>,
        coordinator: Arc<dyn HqpCommandCoordinator>,
        executor: Arc<dyn HqpNativeSettingExecutor>,
        capacity: usize,
    ) -> (HqpImmediateCommandHandle, Self) {
        let (commands, receiver) = mpsc::channel(capacity);
        (
            HqpImmediateCommandHandle { commands },
            Self {
                commands: receiver,
                view,
                coordinator,
                executor,
            },
        )
    }

    pub async fn run(mut self) {
        while let Some(message) = self.commands.recv().await {
            match message {
                HqpCommandActorMessage::Submit { request, reply } => {
                    self.process(*request, reply).await;
                }
                HqpCommandActorMessage::Shutdown { reply } => {
                    // Closing first prevents new sends. Messages which already acquired capacity
                    // remain in the receiver and are owned by this actor, so drain them too.
                    self.commands.close();
                    let mut shutdown_replies = vec![reply];
                    while let Some(accepted) = self.commands.recv().await {
                        match accepted {
                            HqpCommandActorMessage::Submit { request, reply } => {
                                self.process(*request, reply).await;
                            }
                            HqpCommandActorMessage::Shutdown { reply } => {
                                shutdown_replies.push(reply);
                            }
                        }
                    }
                    for waiter in shutdown_replies {
                        if waiter.send(()).is_err() {
                            tracing::debug!(
                                "HQPlayer command shutdown waiter disconnected before drain"
                            );
                        }
                    }
                    return;
                }
            }
        }
    }

    async fn process(
        &self,
        request: HqpImmediateCommandRequest,
        reply: oneshot::Sender<Result<HqpCommandAcknowledgement, HqpCommandServiceRefusal>>,
    ) {
        if let Err(refusal) = validate_correlation_id(&request.correlation_id) {
            send_submit_reply(reply, Err(HqpCommandServiceRefusal::Preflight(refusal)));
            return;
        }
        match self.coordinator.lookup_correlation(&request).await {
            Ok(Some(reservation)) => {
                send_submit_reply(
                    reply,
                    Ok(HqpCommandAcknowledgement {
                        operation: reservation.operation().clone(),
                        duplicate: true,
                    }),
                );
                return;
            }
            Ok(None) => {}
            Err(refusal) => {
                send_submit_reply(reply, Err(HqpCommandServiceRefusal::Coordinator(refusal)));
                return;
            }
        }
        let Some(snapshot) = self.view.snapshot(&request.key) else {
            send_submit_reply(reply, Err(HqpCommandServiceRefusal::ProducerUnknown));
            return;
        };
        let validated = match preflight(&snapshot, &request) {
            Ok(validated) => validated,
            Err(refusal) => {
                send_submit_reply(reply, Err(HqpCommandServiceRefusal::Preflight(refusal)));
                return;
            }
        };
        if instance_name(&validated).is_none() {
            send_submit_reply(
                reply,
                Err(HqpCommandServiceRefusal::InvalidProducerIdentity),
            );
            return;
        }
        let setting = native_setting(&validated);
        let reservation = match self.coordinator.reserve(validated).await {
            Ok(reservation) => reservation,
            Err(refusal) => {
                send_submit_reply(reply, Err(HqpCommandServiceRefusal::Coordinator(refusal)));
                return;
            }
        };
        let lease = reservation.lease().clone();
        let execution_target = reservation.execution_target().cloned();
        let duplicate = reservation.is_duplicate();
        let acknowledgement = HqpCommandAcknowledgement {
            operation: reservation.operation().clone(),
            duplicate,
        };
        // The accepted job no longer depends on this send succeeding. A disconnected submitter
        // drops only its acknowledgement receiver, never the actor-owned reservation.
        send_submit_reply(reply, Ok(acknowledgement));
        if duplicate {
            return;
        }

        if let Err(refusal) = self.coordinator.begin_dispatch(&lease).await {
            if let Err(error) = self
                .coordinator
                .supersede_stale_before_dispatch(&lease, now())
                .await
            {
                tracing::debug!(
                    ?refusal,
                    ?error,
                    "stale HQPlayer operation could not publish supersession"
                );
            }
            return;
        }

        let Some(execution_target) = execution_target else {
            let evidence = CompletionEvidence {
                outcome: CommandOutcome::Superseded,
                write_attempt: WriteAttempt::NotAttempted,
                recovery: RecoveryState::ReplanRequired,
                reason: Some(reason(
                    ReasonCode::Superseded,
                    ReasonScope::Producer,
                    "the admitted HQPlayer command did not retain its native execution target"
                        .to_string(),
                )),
            };
            if let Err(error) = self
                .coordinator
                .complete(&lease, completion(evidence))
                .await
            {
                tracing::warn!(?error, "targetless HQPlayer command was not terminalized");
            }
            return;
        };

        // The coordinator fence says only that the executor call began; it remains deliberately
        // outside the producer document. The executor's typed receipt is still the sole source of
        // native-attempt evidence. A moved/removed reserved target returns `NotAttempted`, while
        // observations and lifecycle teardown wait rather than guessing before this receipt.
        let evidence = match setting {
            Some(setting) => match self.executor.execute(&execution_target, setting).await {
                Ok(receipt) => receipt_evidence(receipt),
                Err(error) => execution_error_evidence(error),
            },
            None => CompletionEvidence {
                outcome: CommandOutcome::Superseded,
                write_attempt: WriteAttempt::NotAttempted,
                recovery: RecoveryState::ReplanRequired,
                reason: Some(reason(
                    ReasonCode::Superseded,
                    ReasonScope::Producer,
                    "the validated command had no #329 native setting arm".to_string(),
                )),
            },
        };
        let completion = completion(evidence);
        if let Err(error) = self.coordinator.complete(&lease, completion).await {
            tracing::warn!(?error, "HQPlayer command completion was not published");
        }
    }
}

fn instance_name(command: &HqpValidatedCommand) -> Option<String> {
    command
        .request
        .key
        .producer_id
        .strip_prefix("hqplayer:")
        .filter(|name| !name.is_empty())
        .map(str::to_string)
}

fn native_setting(command: &HqpValidatedCommand) -> Option<HqpNativeSetting> {
    match (&command.operation, &command.engine_value) {
        (HqpSemanticOperation::SetMode, HqpEngineValue::Name(value)) => {
            Some(HqpNativeSetting::Mode(value.clone()))
        }
        (HqpSemanticOperation::SetFilter1x, HqpEngineValue::Name(value)) => {
            Some(HqpNativeSetting::Filter1x(value.clone()))
        }
        (HqpSemanticOperation::SetFilterNx, HqpEngineValue::Name(value)) => {
            Some(HqpNativeSetting::FilterNx(value.clone()))
        }
        (HqpSemanticOperation::SetShaper, HqpEngineValue::Name(value)) => {
            Some(HqpNativeSetting::Shaper(value.clone()))
        }
        (HqpSemanticOperation::SetRate, HqpEngineValue::Rate(value)) => {
            Some(HqpNativeSetting::Rate(*value))
        }
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq)]
struct CompletionEvidence {
    outcome: CommandOutcome,
    write_attempt: WriteAttempt,
    recovery: RecoveryState,
    reason: Option<Reason>,
}

fn receipt_evidence(receipt: NativeSettingReceipt) -> CompletionEvidence {
    match receipt {
        NativeSettingReceipt::NotAttempted {
            reason: detail,
            readback: NativeSettingReadback::Noop | NativeSettingReadback::Verified,
        } => CompletionEvidence {
            outcome: CommandOutcome::Ignored,
            write_attempt: WriteAttempt::NotAttempted,
            recovery: RecoveryState::NotRequired,
            reason: Some(reason(
                ReasonCode::LockedByProducer,
                ReasonScope::Observed,
                detail,
            )),
        },
        NativeSettingReceipt::NotAttempted {
            reason: detail,
            readback,
        } => CompletionEvidence {
            outcome: CommandOutcome::Superseded,
            write_attempt: WriteAttempt::NotAttempted,
            recovery: RecoveryState::ReplanRequired,
            reason: Some(reason(
                ReasonCode::Superseded,
                ReasonScope::Observed,
                format!("{detail}; native readback: {readback:?}"),
            )),
        },
        NativeSettingReceipt::Suppressed { reason: detail } => CompletionEvidence {
            outcome: CommandOutcome::Superseded,
            write_attempt: WriteAttempt::NotAttempted,
            recovery: RecoveryState::ReplanRequired,
            reason: Some(reason(
                ReasonCode::Superseded,
                ReasonScope::Observed,
                detail,
            )),
        },
        NativeSettingReceipt::DaemonRejected(rejected) => CompletionEvidence {
            outcome: CommandOutcome::Rejected,
            write_attempt: WriteAttempt::AcknowledgedProvisional,
            recovery: RecoveryState::NotRequired,
            reason: Some(reason(
                ReasonCode::LockedByProducer,
                ReasonScope::Producer,
                rejected.to_string(),
            )),
        },
        NativeSettingReceipt::DaemonAcknowledged {
            readback: NativeSettingReadback::Verified,
        } => CompletionEvidence {
            outcome: CommandOutcome::Applied,
            write_attempt: WriteAttempt::Confirmed,
            recovery: RecoveryState::NotRequired,
            reason: None,
        },
        NativeSettingReceipt::DaemonAcknowledged {
            readback: NativeSettingReadback::Noop,
        } => CompletionEvidence {
            outcome: CommandOutcome::Ignored,
            write_attempt: WriteAttempt::Confirmed,
            recovery: RecoveryState::NotRequired,
            reason: None,
        },
        NativeSettingReceipt::DaemonAcknowledged {
            readback:
                NativeSettingReadback::Divergent {
                    requested,
                    observed,
                },
        } => divergent_evidence(WriteAttempt::AcknowledgedProvisional, requested, observed),
        NativeSettingReceipt::DaemonAcknowledged {
            readback: NativeSettingReadback::Unavailable { reason: detail },
        } => indeterminate_evidence(
            WriteAttempt::AcknowledgedProvisional,
            RecoveryState::AwaitingReadback,
            detail,
        ),
        NativeSettingReceipt::PossibleAttempt {
            readback: NativeSettingReadback::Verified | NativeSettingReadback::Noop,
            ..
        } => CompletionEvidence {
            outcome: CommandOutcome::Applied,
            write_attempt: WriteAttempt::Confirmed,
            recovery: RecoveryState::NotRequired,
            reason: None,
        },
        NativeSettingReceipt::PossibleAttempt {
            readback:
                NativeSettingReadback::Divergent {
                    requested,
                    observed,
                },
            ..
        } => divergent_evidence(WriteAttempt::Attempted, requested, observed),
        NativeSettingReceipt::PossibleAttempt {
            reason: delivery,
            readback: NativeSettingReadback::Unavailable { reason: readback },
        } => indeterminate_evidence(
            WriteAttempt::Attempted,
            RecoveryState::AwaitingReconnect,
            format!("{delivery}; {readback}"),
        ),
    }
}

fn execution_error_evidence(error: anyhow::Error) -> CompletionEvidence {
    // The manager's legacy `anyhow` boundary does not prove whether a write left. Preserve the
    // ambiguity without parsing its display text or replaying the request.
    indeterminate_evidence(
        WriteAttempt::Attempted,
        RecoveryState::AwaitingReconnect,
        error.to_string(),
    )
}

fn divergent_evidence(
    write_attempt: WriteAttempt,
    requested: String,
    observed: String,
) -> CompletionEvidence {
    CompletionEvidence {
        outcome: CommandOutcome::Divergent,
        write_attempt,
        recovery: RecoveryState::ReplanRequired,
        reason: Some(reason(
            ReasonCode::Unobservable,
            ReasonScope::Observed,
            format!("requested {requested}; authoritative readback reported {observed}"),
        )),
    }
}

fn indeterminate_evidence(
    write_attempt: WriteAttempt,
    recovery: RecoveryState,
    detail: String,
) -> CompletionEvidence {
    CompletionEvidence {
        outcome: CommandOutcome::Indeterminate,
        write_attempt,
        recovery,
        reason: Some(reason(ReasonCode::Unobservable, ReasonScope::Lane, detail)),
    }
}

fn completion(evidence: CompletionEvidence) -> HqpCommandCompletion {
    HqpCommandCompletion::new(
        evidence.outcome,
        evidence.write_attempt,
        now(),
        Some(evidence.recovery),
        evidence.reason,
    )
}

fn reason(code: ReasonCode, scope: ReasonScope, detail: String) -> Reason {
    Reason {
        code,
        scope,
        display_text_key: None,
        detail: Some(detail),
    }
}

fn now() -> Timestamp {
    Timestamp::new(chrono::Utc::now().to_rfc3339())
}

// =============================================================================
// #331: the surface-facing executor
// =============================================================================

/// The HQPlayer implementation of the control plane's producer-neutral executor (#331).
///
/// It exists so `ControlPlaneService` never names a producer family, and so the twenty-two typed
/// refusals this module can produce are translated once, here, into the ten a surface can act on.
/// The translation is an exhaustive `match`: a refusal added later fails to compile rather than
/// silently inheriting an existing surface verdict.
pub struct HqpSemanticExecutor {
    commands: HqpImmediateCommandHandle,
}

impl HqpSemanticExecutor {
    /// The `producer_type` this executor owns, as published by the HQPlayer producer document.
    pub const PRODUCER_TYPE: &'static str = "hqplayer";

    /// Wrap the bounded immediate-command actor's ingress.
    pub fn new(commands: HqpImmediateCommandHandle) -> Self {
        Self { commands }
    }
}

#[async_trait]
impl SemanticCommandExecutor for HqpSemanticExecutor {
    fn producer_type(&self) -> &str {
        Self::PRODUCER_TYPE
    }

    async fn submit(&self, command: SemanticCommand) -> Result<CommandAcceptance, CommandRefusal> {
        let request = HqpImmediateCommandRequest {
            key: command.key,
            expected: command.expected,
            control: command.control,
            requested: command.requested,
            lane: command.lane,
            correlation_id: command.correlation_id,
        };
        match self.commands.submit(request).await {
            Ok(acknowledgement) => Ok(CommandAcceptance {
                operation: acknowledgement.operation,
                duplicate: acknowledgement.duplicate,
            }),
            Err(refusal) => Err(surface_refusal(refusal)),
        }
    }
}

fn surface_refusal(refusal: HqpCommandServiceRefusal) -> CommandRefusal {
    match refusal {
        HqpCommandServiceRefusal::ServiceClosed => CommandRefusal::ServiceClosed,
        HqpCommandServiceRefusal::ProducerUnknown
        | HqpCommandServiceRefusal::InvalidProducerIdentity => {
            CommandRefusal::Refused(SemanticRefusal::ProducerMismatch)
        }
        HqpCommandServiceRefusal::Preflight(preflight) => {
            CommandRefusal::Refused(semantic_preflight(preflight))
        }
        HqpCommandServiceRefusal::Coordinator(coordinator) => match coordinator {
            HqpCoordinatorRefusal::Preflight(preflight) => {
                CommandRefusal::Refused(semantic_preflight(preflight))
            }
            HqpCoordinatorRefusal::CoordinatorClosed
            | HqpCoordinatorRefusal::LifecycleNotRunning => CommandRefusal::ServiceClosed,
            HqpCoordinatorRefusal::ProducerUnknown => {
                CommandRefusal::Refused(SemanticRefusal::ProducerMismatch)
            }
            HqpCoordinatorRefusal::InvalidCorrelation => {
                CommandRefusal::Refused(SemanticRefusal::CorrelationUnusable)
            }
            // Everything remaining is one fact for a surface: nothing was written, and the reason
            // is contention or bookkeeping rather than the request. A surface re-reads and retries.
            HqpCoordinatorRefusal::CorrelationConflict
            | HqpCoordinatorRefusal::ConflictBusy
            | HqpCoordinatorRefusal::UnresolvedRetentionFull
            | HqpCoordinatorRefusal::StaleLease
            | HqpCoordinatorRefusal::LeaseStillCurrent
            | HqpCoordinatorRefusal::IllegalTransition
            | HqpCoordinatorRefusal::RevisionFailed(_)
            | HqpCoordinatorRefusal::PublicationFailed(_)
            | HqpCoordinatorRefusal::RetirementFailed(_) => {
                CommandRefusal::Refused(SemanticRefusal::NotReserved)
            }
        },
    }
}

fn semantic_preflight(refusal: HqpPreflightRefusal) -> SemanticRefusal {
    match refusal {
        HqpPreflightRefusal::CorrelationIdEmpty
        | HqpPreflightRefusal::CorrelationIdTooLong { .. } => SemanticRefusal::CorrelationUnusable,
        HqpPreflightRefusal::ProducerMismatch => SemanticRefusal::ProducerMismatch,
        HqpPreflightRefusal::RevisionMismatch { expected, actual } => {
            SemanticRefusal::PositionStale { expected, actual }
        }
        HqpPreflightRefusal::ProducerNotLive | HqpPreflightRefusal::ProducerStale => {
            SemanticRefusal::ProducerNotWritable
        }
        HqpPreflightRefusal::NativeLaneMissing
        | HqpPreflightRefusal::NativeLaneUnavailable
        | HqpPreflightRefusal::NativeLaneStale => SemanticRefusal::LaneUnusable,
        HqpPreflightRefusal::ControlUnknown
        | HqpPreflightRefusal::ControlUnavailable
        | HqpPreflightRefusal::ControlReadOnly => SemanticRefusal::ControlNotWritable,
        HqpPreflightRefusal::WrongApplyLane
        | HqpPreflightRefusal::NonImmediateRequest
        | HqpPreflightRefusal::WrongControlKind
        | HqpPreflightRefusal::WrongValueKind => SemanticRefusal::RequestMismatch,
        HqpPreflightRefusal::UnknownChoice
        | HqpPreflightRefusal::ChoiceUnavailable
        | HqpPreflightRefusal::ChoiceMissingEngineName
        | HqpPreflightRefusal::InvalidRateEngineName => SemanticRefusal::ValueNotSelectable,
        // #375 registered these descriptors; #347 owes them a verified receipt. Advertising them as
        // invokable before that is how a tool description stops being true.
        HqpPreflightRefusal::DeferredToIssue328 { .. }
        | HqpPreflightRefusal::UnsupportedControl => SemanticRefusal::NoVerifiedOperation,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;
    use std::time::Duration;

    use tokio::sync::{Notify, Semaphore};

    use super::*;
    use crate::adapters::hqplayer::HqpRejected;
    use crate::adaptive::{
        ApplyEffect, ApplyLane, ApplySemantics, Availability, Choice, ChoiceSet, Control,
        ControlId, ControlKind, ControlValue, Disruption, DocumentRevisions, Extensions, Freshness,
        LaneHealth, LaneState, OperationId, OperationRequest, ProducerDocument, ProducerEpoch,
        ProducerIdentity, ProducerTarget, RevisionRef, RiskClass, TargetRole, TransportLane,
        Verification, CONSUMER_SCHEMA_VERSION,
    };
    use crate::producers::hqplayer_command::{fingerprint, HqpCommandFingerprint};
    use crate::producers::ProducerPresence;

    const MODE: &str = "hqplayer.pipeline.mode";
    const MODE_CHOICE: &str = "hqplayer.pipeline.mode.engine.50434d";
    const FILTER_1X: &str = "hqplayer.pipeline.filter_1x";
    const FILTER_1X_CHOICE: &str = "hqplayer.pipeline.filter_1x.engine.706f6c79";
    const FILTER_NX: &str = "hqplayer.pipeline.filter_nx";
    const FILTER_NX_CHOICE: &str = "hqplayer.pipeline.filter_nx.engine.73696e63";
    const SHAPER: &str = "hqplayer.pipeline.shaper";
    const SHAPER_CHOICE: &str = "hqplayer.pipeline.shaper.engine.4e5339";
    const RATE: &str = "hqplayer.pipeline.rate";
    const RATE_CHOICE: &str = "hqplayer.pipeline.rate.engine.313932303030";

    struct FakeView {
        snapshot: StdMutex<ProducerSnapshot>,
    }

    impl HqpCommandSnapshotView for FakeView {
        fn snapshot(&self, key: &ProducerKey) -> Option<ProducerSnapshot> {
            let snapshot = lock(&self.snapshot);
            (snapshot.key == *key).then(|| snapshot.clone())
        }
    }

    #[derive(Default)]
    struct FakeCoordinator {
        reserves: AtomicUsize,
        validations: AtomicUsize,
        completions: AtomicUsize,
        supersessions: AtomicUsize,
        lookups: AtomicUsize,
        validation_fails: AtomicBool,
        block_reserve: AtomicBool,
        targetless_lease: AtomicBool,
        reserve_started: Notify,
        reserve_release: Notify,
        correlations: StdMutex<HashMap<String, (HqpCommandFingerprint, HqpCommandReservation)>>,
        sequence: Arc<StdMutex<Vec<&'static str>>>,
    }

    #[async_trait]
    impl HqpCommandCoordinator for FakeCoordinator {
        async fn lookup_correlation(
            &self,
            request: &HqpImmediateCommandRequest,
        ) -> Result<Option<HqpCommandReservation>, HqpCoordinatorRefusal> {
            self.lookups.fetch_add(1, Ordering::SeqCst);
            let correlations = lock(&self.correlations);
            let Some((reserved_fingerprint, reservation)) =
                correlations.get(&request.correlation_id)
            else {
                return Ok(None);
            };
            if *reserved_fingerprint != fingerprint(request) {
                return Err(HqpCoordinatorRefusal::CorrelationConflict);
            }
            Ok(Some(HqpCommandReservation::for_test(
                reservation.lease().clone(),
                reservation.operation().clone(),
                true,
            )))
        }

        async fn reserve(
            &self,
            command: HqpValidatedCommand,
        ) -> Result<HqpCommandReservation, HqpCoordinatorRefusal> {
            self.reserves.fetch_add(1, Ordering::SeqCst);
            push(&self.sequence, "reserve");
            if self.block_reserve.load(Ordering::SeqCst) {
                self.reserve_started.notify_one();
                self.reserve_release.notified().await;
            }
            let mut correlations = lock(&self.correlations);
            if let Some((reserved_fingerprint, existing)) =
                correlations.get(&command.request.correlation_id)
            {
                if *reserved_fingerprint != command.fingerprint {
                    return Err(HqpCoordinatorRefusal::CorrelationConflict);
                }
                return Ok(HqpCommandReservation::for_test(
                    existing.lease().clone(),
                    existing.operation().clone(),
                    true,
                ));
            }
            let generation = self.reserves.load(Ordering::SeqCst) as u64;
            let execution_target = HqpNativeExecutionTarget {
                instance_name: command
                    .request
                    .key
                    .producer_id
                    .strip_prefix("hqplayer:")
                    .unwrap_or_default()
                    .to_string(),
                producer_epoch: command.request.expected.epoch.0,
                endpoint_generation: 17,
                transport_generation: 23,
            };
            let lease = if self.targetless_lease.load(Ordering::SeqCst) {
                HqpCommandLease::for_test(
                    command.request.key.producer_id.clone(),
                    command.request.expected.epoch.0,
                    generation,
                    command.request.correlation_id.clone(),
                )
            } else {
                HqpCommandLease::for_test_with_execution_target(
                    command.request.key.producer_id.clone(),
                    command.request.expected.epoch.0,
                    generation,
                    command.request.correlation_id.clone(),
                    execution_target,
                )
            };
            let operation = pending_operation(&command, generation);
            let reservation = HqpCommandReservation::for_test(lease, operation, false);
            correlations.insert(
                command.request.correlation_id,
                (command.fingerprint, reservation.clone()),
            );
            Ok(reservation)
        }

        async fn begin_dispatch(
            &self,
            _lease: &HqpCommandLease,
        ) -> Result<(), HqpCoordinatorRefusal> {
            self.validations.fetch_add(1, Ordering::SeqCst);
            push(&self.sequence, "begin_dispatch");
            if self.validation_fails.load(Ordering::SeqCst) {
                Err(HqpCoordinatorRefusal::StaleLease)
            } else {
                Ok(())
            }
        }

        async fn complete(
            &self,
            lease: &HqpCommandLease,
            _completion: HqpCommandCompletion,
        ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
            self.completions.fetch_add(1, Ordering::SeqCst);
            push(&self.sequence, "complete");
            lock(&self.correlations)
                .values()
                .find(|(_, reservation)| reservation.lease() == lease)
                .map(|(_, reservation)| reservation.operation().clone())
                .ok_or(HqpCoordinatorRefusal::StaleLease)
        }

        async fn supersede_stale_before_dispatch(
            &self,
            lease: &HqpCommandLease,
            _at: Timestamp,
        ) -> Result<OperationRecord, HqpCoordinatorRefusal> {
            self.supersessions.fetch_add(1, Ordering::SeqCst);
            push(&self.sequence, "supersede");
            lock(&self.correlations)
                .values()
                .find(|(_, reservation)| reservation.lease() == lease)
                .map(|(_, reservation)| reservation.operation().clone())
                .ok_or(HqpCoordinatorRefusal::StaleLease)
        }
    }

    struct FakeExecutor {
        calls: AtomicUsize,
        block: AtomicBool,
        permits: Semaphore,
        called: Notify,
        last_target: StdMutex<Option<HqpNativeExecutionTarget>>,
        receipt: StdMutex<NativeSettingReceipt>,
        sequence: Arc<StdMutex<Vec<&'static str>>>,
    }

    impl FakeExecutor {
        fn new(sequence: Arc<StdMutex<Vec<&'static str>>>) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                block: AtomicBool::new(false),
                permits: Semaphore::new(0),
                called: Notify::new(),
                last_target: StdMutex::new(None),
                receipt: StdMutex::new(NativeSettingReceipt::DaemonAcknowledged {
                    readback: NativeSettingReadback::Verified,
                }),
                sequence,
            }
        }
    }

    #[async_trait]
    impl HqpNativeSettingExecutor for FakeExecutor {
        async fn execute(
            &self,
            target: &HqpNativeExecutionTarget,
            _setting: HqpNativeSetting,
        ) -> anyhow::Result<NativeSettingReceipt> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *lock(&self.last_target) = Some(target.clone());
            push(&self.sequence, "execute");
            self.called.notify_waiters();
            if self.block.load(Ordering::SeqCst) {
                let permit = self.permits.acquire().await;
                if let Ok(permit) = permit {
                    permit.forget();
                }
            }
            Ok(lock(&self.receipt).clone())
        }
    }

    fn build_test_service(
        snapshot: ProducerSnapshot,
        coordinator: Arc<FakeCoordinator>,
        executor: Arc<FakeExecutor>,
        capacity: usize,
    ) -> (HqpImmediateCommandHandle, tokio::task::JoinHandle<()>) {
        build_test_service_with_view(
            Arc::new(FakeView {
                snapshot: StdMutex::new(snapshot),
            }),
            coordinator,
            executor,
            capacity,
        )
    }

    fn build_test_service_with_view(
        view: Arc<FakeView>,
        coordinator: Arc<FakeCoordinator>,
        executor: Arc<FakeExecutor>,
        capacity: usize,
    ) -> (HqpImmediateCommandHandle, tokio::task::JoinHandle<()>) {
        let (handle, actor) =
            HqpImmediateCommandActor::build_with(view, coordinator, executor, capacity);
        let task = tokio::spawn(actor.run());
        (handle, task)
    }

    // =========================================================================
    // #331: the surface-facing executor
    // =========================================================================

    /// The `ServiceClosed` case #329 predicted for this issue, now reachable.
    ///
    /// It matters because a surface must be able to distinguish "the request was wrong" from "the
    /// server is going away": the first is worth showing the user, the second is worth retrying.
    #[tokio::test]
    async fn a_command_submitted_after_shutdown_is_service_closed_rather_than_a_refusal() {
        let live = snapshot();
        let key = live.key.clone();
        let expected = live.document.position();
        let (handle, task) = build_test_service(
            live,
            Arc::new(FakeCoordinator::default()),
            Arc::new(FakeExecutor::new(Arc::new(StdMutex::new(Vec::new())))),
            4,
        );
        handle.shutdown().await.expect("the actor drains");
        task.await.expect("the actor task ends");

        let executor = HqpSemanticExecutor::new(handle);
        assert_eq!(executor.producer_type(), "hqplayer");
        let refused = executor
            .submit(SemanticCommand {
                key,
                expected,
                control: ControlId::new(MODE),
                requested: ControlValue::Choice(MODE_CHOICE.to_string()),
                lane: ApplyLane::Immediate,
                correlation_id: "corr-after-shutdown".to_string(),
                surface_id: "web".to_string(),
            })
            .await
            .expect_err("a closed actor accepts nothing");
        assert_eq!(refused, CommandRefusal::ServiceClosed);
    }

    /// Every internal refusal has to land on exactly one surface verdict, and the collapse must not
    /// erase the distinction a surface acts on. Revision staleness in particular must survive:
    /// "re-read and retry" is a different instruction from "your request was wrong".
    #[test]
    fn internal_refusals_collapse_onto_surface_verdicts_without_losing_what_to_do_next() {
        assert_eq!(
            surface_refusal(HqpCommandServiceRefusal::Preflight(
                HqpPreflightRefusal::RevisionMismatch {
                    expected: stale_position(),
                    actual: current_position(),
                }
            )),
            CommandRefusal::Refused(SemanticRefusal::PositionStale {
                expected: stale_position(),
                actual: current_position(),
            })
        );
        assert_eq!(
            surface_refusal(HqpCommandServiceRefusal::Preflight(
                HqpPreflightRefusal::UnknownChoice
            )),
            CommandRefusal::Refused(SemanticRefusal::ValueNotSelectable)
        );
        assert_eq!(
            surface_refusal(HqpCommandServiceRefusal::Preflight(
                HqpPreflightRefusal::ControlReadOnly
            )),
            CommandRefusal::Refused(SemanticRefusal::ControlNotWritable)
        );
        assert_eq!(
            surface_refusal(HqpCommandServiceRefusal::Preflight(
                HqpPreflightRefusal::UnsupportedControl
            )),
            CommandRefusal::Refused(SemanticRefusal::NoVerifiedOperation)
        );
        assert_eq!(
            surface_refusal(HqpCommandServiceRefusal::Coordinator(
                HqpCoordinatorRefusal::CoordinatorClosed
            )),
            CommandRefusal::ServiceClosed,
            "a closing coordinator is a retry, not a rejected request"
        );
        assert_eq!(
            surface_refusal(HqpCommandServiceRefusal::Coordinator(
                HqpCoordinatorRefusal::ConflictBusy
            )),
            CommandRefusal::Refused(SemanticRefusal::NotReserved)
        );
        assert_eq!(
            surface_refusal(HqpCommandServiceRefusal::Coordinator(
                HqpCoordinatorRefusal::Preflight(HqpPreflightRefusal::ProducerNotLive)
            )),
            CommandRefusal::Refused(SemanticRefusal::ProducerNotWritable),
            "a preflight refusal repeated by the coordinator must read the same to a surface"
        );
    }

    fn stale_position() -> RevisionRef {
        RevisionRef {
            epoch: ProducerEpoch(7),
            control_plane: crate::adaptive::Revision(3),
            state: crate::adaptive::Revision(8),
        }
    }

    fn current_position() -> RevisionRef {
        RevisionRef {
            epoch: ProducerEpoch(7),
            control_plane: crate::adaptive::Revision(3),
            state: crate::adaptive::Revision(9),
        }
    }

    #[test]
    fn verified_no_send_is_a_confirmed_noop_not_an_indeterminate_write() {
        let evidence = receipt_evidence(NativeSettingReceipt::NotAttempted {
            reason: "already selected".to_string(),
            readback: NativeSettingReadback::Noop,
        });

        assert_eq!(evidence.outcome, CommandOutcome::Ignored);
        assert_eq!(evidence.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(evidence.recovery, RecoveryState::NotRequired);
    }

    #[test]
    fn reserved_target_mismatch_is_terminal_no_write_evidence() {
        let evidence = receipt_evidence(NativeSettingReceipt::NotAttempted {
            reason: "reserved native producer identity moved before execution".to_string(),
            readback: NativeSettingReadback::Unavailable {
                reason: "reserved producer no longer exists".to_string(),
            },
        });

        assert_eq!(evidence.outcome, CommandOutcome::Superseded);
        assert_eq!(evidence.write_attempt, WriteAttempt::NotAttempted);
        assert_eq!(evidence.recovery, RecoveryState::ReplanRequired);
    }

    #[test]
    fn typed_native_receipts_keep_attempt_application_and_recovery_evidence_separate() {
        let acknowledged = receipt_evidence(NativeSettingReceipt::DaemonAcknowledged {
            readback: NativeSettingReadback::Verified,
        });
        assert_eq!(acknowledged.outcome, CommandOutcome::Applied);
        assert_eq!(acknowledged.write_attempt, WriteAttempt::Confirmed);
        assert_eq!(acknowledged.recovery, RecoveryState::NotRequired);

        let rejected = receipt_evidence(NativeSettingReceipt::DaemonRejected(HqpRejected {
            element: "SetMode".to_string(),
            reason: Some("locked".to_string()),
        }));
        assert_eq!(rejected.outcome, CommandOutcome::Rejected);
        assert_eq!(
            rejected.write_attempt,
            WriteAttempt::AcknowledgedProvisional
        );
        assert_eq!(rejected.recovery, RecoveryState::NotRequired);

        let divergent = receipt_evidence(NativeSettingReceipt::DaemonAcknowledged {
            readback: NativeSettingReadback::Divergent {
                requested: "PCM".to_string(),
                observed: "SDM".to_string(),
            },
        });
        assert_eq!(divergent.outcome, CommandOutcome::Divergent);
        assert_eq!(
            divergent.write_attempt,
            WriteAttempt::AcknowledgedProvisional
        );
        assert_eq!(divergent.recovery, RecoveryState::ReplanRequired);

        let ambiguous = receipt_evidence(NativeSettingReceipt::PossibleAttempt {
            reason: "reply was unavailable".to_string(),
            readback: NativeSettingReadback::Unavailable {
                reason: "session ended".to_string(),
            },
        });
        assert_eq!(ambiguous.outcome, CommandOutcome::Indeterminate);
        assert_eq!(ambiguous.write_attempt, WriteAttempt::Attempted);
        assert_eq!(ambiguous.recovery, RecoveryState::AwaitingReconnect);
    }

    #[test]
    fn all_five_validated_settings_have_one_owned_native_dispatch_arm() {
        let snapshot = snapshot();
        for (control, choice, expected) in [
            (MODE, MODE_CHOICE, HqpNativeSetting::Mode("PCM".to_string())),
            (
                FILTER_1X,
                FILTER_1X_CHOICE,
                HqpNativeSetting::Filter1x("poly".to_string()),
            ),
            (
                FILTER_NX,
                FILTER_NX_CHOICE,
                HqpNativeSetting::FilterNx("sinc".to_string()),
            ),
            (
                SHAPER,
                SHAPER_CHOICE,
                HqpNativeSetting::Shaper("NS9".to_string()),
            ),
            (RATE, RATE_CHOICE, HqpNativeSetting::Rate(192_000)),
        ] {
            let request = request_for(&snapshot, "dispatch", control, choice);
            let validated = preflight(&snapshot, &request);
            assert!(
                matches!(validated, Ok(ref command) if native_setting(command) == Some(expected)),
                "{control} must dispatch only through its typed owned setting"
            );
        }
    }

    #[tokio::test]
    async fn stale_advisory_preflight_invokes_neither_reservation_nor_executor() {
        let snapshot = snapshot();
        let mut request = request(&snapshot, "stale");
        request.expected.state.0 -= 1;
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence));
        let (handle, task) = build_test_service(snapshot, coordinator.clone(), executor.clone(), 4);

        let result = handle.submit(request).await;
        assert!(matches!(
            result,
            Err(HqpCommandServiceRefusal::Preflight(
                HqpPreflightRefusal::RevisionMismatch { .. }
            ))
        ));
        assert_eq!(coordinator.reserves.load(Ordering::SeqCst), 0);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn actor_owns_an_accepted_job_after_the_submitter_is_cancelled() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            block_reserve: AtomicBool::new(true),
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence));
        let (handle, task) =
            build_test_service(snapshot.clone(), coordinator.clone(), executor.clone(), 4);

        let submit = tokio::spawn({
            let handle = handle.clone();
            async move { handle.submit(request(&snapshot, "cancelled-caller")).await }
        });
        coordinator.reserve_started.notified().await;
        submit.abort();
        coordinator.reserve_release.notify_one();

        let called = tokio::time::timeout(Duration::from_secs(1), executor.called.notified()).await;
        assert!(
            called.is_ok(),
            "the actor must continue after the caller disappears"
        );
        assert!(
            wait_for_count(&coordinator.completions, 1).await,
            "the abandoned acknowledgement must still terminalize"
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.completions.load(Ordering::SeqCst), 1);
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn duplicate_correlation_is_acknowledged_without_a_second_send() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence));
        let (handle, task) =
            build_test_service(snapshot.clone(), coordinator.clone(), executor.clone(), 4);
        let command = request(&snapshot, "same-gesture");

        let first = handle.submit(command.clone()).await;
        assert!(matches!(first, Ok(ref acknowledgement) if !acknowledgement.duplicate));
        assert!(wait_for_count(&coordinator.completions, 1).await);
        let duplicate = handle.submit(command).await;
        assert!(matches!(duplicate, Ok(ref acknowledgement) if acknowledgement.duplicate));
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.reserves.load(Ordering::SeqCst), 1);
        assert_eq!(coordinator.lookups.load(Ordering::SeqCst), 2);
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn exact_retry_bypasses_stale_advisory_view_after_pending_advances_revision() {
        let snapshot = snapshot();
        let view = Arc::new(FakeView {
            snapshot: StdMutex::new(snapshot.clone()),
        });
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence));
        let (handle, task) =
            build_test_service_with_view(view.clone(), coordinator.clone(), executor.clone(), 4);
        let command = request(&snapshot, "pending-retry");

        let first = handle.submit(command.clone()).await;
        assert!(matches!(first, Ok(ref acknowledgement) if !acknowledgement.duplicate));
        assert!(wait_for_count(&coordinator.completions, 1).await);
        Arc::make_mut(&mut lock(&view.snapshot).document)
            .revisions
            .state
            .0 += 1;

        let retry = handle.submit(command).await;
        assert!(matches!(retry, Ok(ref acknowledgement) if acknowledgement.duplicate));
        assert_eq!(coordinator.reserves.load(Ordering::SeqCst), 1);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn correlation_reuse_with_changed_semantics_refuses_before_advisory_preflight() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence));
        let (handle, task) =
            build_test_service(snapshot.clone(), coordinator.clone(), executor.clone(), 8);
        let command = request(&snapshot, "conflicting-reuse");
        assert!(handle.submit(command.clone()).await.is_ok());
        assert!(wait_for_count(&coordinator.completions, 1).await);

        let conflicts = vec![
            HqpImmediateCommandRequest {
                expected: RevisionRef {
                    state: crate::adaptive::Revision(command.expected.state.0 + 1),
                    ..command.expected
                },
                ..command.clone()
            },
            HqpImmediateCommandRequest {
                key: ProducerKey {
                    role: TargetRole::Instance,
                    ..command.key.clone()
                },
                ..command.clone()
            },
            HqpImmediateCommandRequest {
                lane: ApplyLane::Persistent,
                ..command.clone()
            },
            HqpImmediateCommandRequest {
                requested: ControlValue::choice("hqplayer.pipeline.mode.engine.53444d"),
                ..command
            },
        ];
        for conflict in conflicts {
            assert_eq!(
                handle.submit(conflict).await,
                Err(HqpCommandServiceRefusal::Coordinator(
                    HqpCoordinatorRefusal::CorrelationConflict,
                ))
            );
        }

        assert_eq!(coordinator.reserves.load(Ordering::SeqCst), 1);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn lease_validation_happens_immediately_before_native_dispatch() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence.clone()));
        let (handle, task) =
            build_test_service(snapshot.clone(), coordinator.clone(), executor.clone(), 4);

        assert!(handle.submit(request(&snapshot, "ordered")).await.is_ok());
        assert!(wait_for_count(&coordinator.completions, 1).await);
        assert_eq!(
            *lock(&sequence),
            vec!["reserve", "begin_dispatch", "execute", "complete"]
        );
        assert_eq!(
            *lock(&executor.last_target),
            Some(HqpNativeExecutionTarget {
                instance_name: "main".to_string(),
                producer_epoch: 7,
                endpoint_generation: 17,
                transport_generation: 23,
            }),
            "the native executor must receive the exact identity retained by reservation"
        );
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn native_no_write_receipt_is_not_upgraded_to_attempted_before_execution() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence.clone()));
        *lock(&executor.receipt) = NativeSettingReceipt::NotAttempted {
            reason: "the exact reserved endpoint moved before the native write path".to_string(),
            readback: NativeSettingReadback::Unavailable {
                reason: "the old native session is gone".to_string(),
            },
        };
        let (handle, task) = build_test_service(snapshot.clone(), coordinator.clone(), executor, 4);

        assert!(handle
            .submit(request(&snapshot, "no-write-boundary"))
            .await
            .is_ok());
        assert!(wait_for_count(&coordinator.completions, 1).await);
        assert_eq!(
            *lock(&sequence),
            vec!["reserve", "begin_dispatch", "execute", "complete"],
            "attempt evidence must come from the executor receipt, not a pre-execution marker"
        );
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn failed_dispatch_validation_terminalizes_without_native_io() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            validation_fails: AtomicBool::new(true),
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence.clone()));
        let (handle, task) =
            build_test_service(snapshot.clone(), coordinator.clone(), executor.clone(), 4);

        assert!(handle
            .submit(request(&snapshot, "stale-lease"))
            .await
            .is_ok());
        assert!(wait_for_count(&coordinator.supersessions, 1).await);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            *lock(&sequence),
            vec!["reserve", "begin_dispatch", "supersede"]
        );
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn missing_reserved_target_terminalizes_without_native_io() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            targetless_lease: AtomicBool::new(true),
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence.clone()));
        let (handle, task) =
            build_test_service(snapshot.clone(), coordinator.clone(), executor.clone(), 4);

        assert!(handle
            .submit(request(&snapshot, "targetless"))
            .await
            .is_ok());
        assert!(wait_for_count(&coordinator.completions, 1).await);
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        assert_eq!(
            *lock(&sequence),
            vec!["reserve", "begin_dispatch", "complete"]
        );
        assert!(handle.shutdown().await.is_ok());
        assert!(task.await.is_ok());
    }

    #[tokio::test]
    async fn graceful_shutdown_closes_ingress_and_drains_every_accepted_job() {
        let snapshot = snapshot();
        let sequence = Arc::new(StdMutex::new(Vec::new()));
        let coordinator = Arc::new(FakeCoordinator {
            sequence: sequence.clone(),
            ..FakeCoordinator::default()
        });
        let executor = Arc::new(FakeExecutor::new(sequence));
        executor.block.store(true, Ordering::SeqCst);
        let (handle, task) =
            build_test_service(snapshot.clone(), coordinator.clone(), executor.clone(), 4);

        let first = tokio::spawn({
            let handle = handle.clone();
            let snapshot = snapshot.clone();
            async move { handle.submit(request(&snapshot, "first")).await }
        });
        executor.called.notified().await;
        let second = tokio::spawn({
            let handle = handle.clone();
            async move { handle.submit(request(&snapshot, "second")).await }
        });
        let shutdown = tokio::spawn({
            let handle = handle.clone();
            async move { handle.shutdown().await }
        });
        executor.permits.add_permits(2);

        assert!(first.await.is_ok());
        assert!(second.await.is_ok());
        assert!(matches!(shutdown.await, Ok(Ok(()))));
        assert!(task.await.is_ok());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 2);
        assert_eq!(coordinator.completions.load(Ordering::SeqCst), 2);
        assert!(matches!(
            handle
                .submit(request(&snapshot_for_closed(), "closed"))
                .await,
            Err(HqpCommandServiceRefusal::ServiceClosed)
        ));
    }

    fn snapshot_for_closed() -> ProducerSnapshot {
        snapshot()
    }

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
                last_success: None,
                last_error: None,
                freshness: Freshness::default(),
                extensions: Extensions::default(),
            }],
            groups: Vec::new(),
            controls: vec![
                selection(MODE, MODE_CHOICE, "PCM"),
                selection(FILTER_1X, FILTER_1X_CHOICE, "poly"),
                selection(FILTER_NX, FILTER_NX_CHOICE, "sinc"),
                selection(SHAPER, SHAPER_CHOICE, "NS9"),
                selection(RATE, RATE_CHOICE, "192000"),
            ],
            constraints: Vec::new(),
            draft_policy: None,
            change_sets: Vec::new(),
            operations: Vec::new(),
            stale: false,
            extensions: Extensions::default(),
        };
        ProducerSnapshot {
            key: ProducerKey::of(&document),
            document: Arc::new(document),
            lanes: BTreeMap::new(),
            presence: ProducerPresence::Live,
            repairs: Vec::new(),
            last_refusal: None,
        }
    }

    fn request(snapshot: &ProducerSnapshot, correlation_id: &str) -> HqpImmediateCommandRequest {
        request_for(snapshot, correlation_id, MODE, MODE_CHOICE)
    }

    fn request_for(
        snapshot: &ProducerSnapshot,
        correlation_id: &str,
        control: &str,
        choice: &str,
    ) -> HqpImmediateCommandRequest {
        HqpImmediateCommandRequest {
            key: snapshot.key.clone(),
            expected: snapshot.document.position(),
            control: ControlId::new(control),
            requested: ControlValue::choice(choice),
            lane: ApplyLane::Immediate,
            correlation_id: correlation_id.to_string(),
        }
    }

    fn selection(control: &str, choice: &str, engine_name: &str) -> Control {
        Control {
            id: ControlId::new(control),
            kind: ControlKind::Enumeration,
            label_key: None,
            description_key: None,
            group: None,
            order: None,
            widget_hint: None,
            unit: None,
            values: Vec::new(),
            choices: Some(ChoiceSet::runtime(vec![Choice::new(choice, engine_name)])),
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

    fn pending_operation(command: &HqpValidatedCommand, generation: u64) -> OperationRecord {
        OperationRecord {
            id: OperationId::new(format!(
                "{}:operation:{generation}",
                command.request.key.producer_id
            )),
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
        }
    }

    fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn push(sequence: &StdMutex<Vec<&'static str>>, event: &'static str) {
        lock(sequence).push(event);
    }

    async fn wait_for_count(counter: &AtomicUsize, expected: usize) -> bool {
        tokio::time::timeout(Duration::from_secs(1), async {
            while counter.load(Ordering::SeqCst) < expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .is_ok()
    }
}
