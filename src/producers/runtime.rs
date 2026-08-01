//! Lossless, single-owner runtime for adaptive producer ingress.

use std::fmt;
use std::sync::Arc;

use tokio::sync::{broadcast, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use super::aggregator::{ProducerAggregator, ProducerSnapshot};
use super::event::{create_adaptive_bus, AdaptiveEvent, SharedAdaptiveBus};
use super::run::{AdapterRun, AdapterRuns, PublicationOrigin};
use super::{Admission, AdmissionRefusal, ProducerKey};
use crate::adaptive::{ProducerDocument, ProducerEpoch};

/// The adaptive actor is no longer accepting commands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdaptiveRuntimeClosed;

impl fmt::Display for AdaptiveRuntimeClosed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("adaptive producer runtime is closed")
    }
}

impl std::error::Error for AdaptiveRuntimeClosed {}

/// A detached publication was not accepted for queueing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AdaptivePublishError {
    /// The actor no longer accepts ingress.
    RuntimeClosed,
    /// The supplied lease does not own the document's producer namespace.
    ProducerOwnershipMismatch {
        origin: PublicationOrigin,
        producer_id: String,
    },
}

impl fmt::Display for AdaptivePublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RuntimeClosed => f.write_str("adaptive producer runtime is closed"),
            Self::ProducerOwnershipMismatch {
                origin,
                producer_id,
            } => write!(
                f,
                "adapter {} does not own producer {producer_id}",
                origin.adapter()
            ),
        }
    }
}

impl std::error::Error for AdaptivePublishError {}

/// Result of attempting producer retirement through an adapter-run lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RetirementOutcome {
    /// The bounded inbox committed the retirement; a detached caller need not wait further.
    Committed,
    /// The actor applied the committed retirement and removed this many visible targets.
    Retired { removed: usize },
    /// The supplied lease had already been superseded or ended before queue commitment.
    StaleAdapterRun {
        origin: PublicationOrigin,
        active: Option<super::AdapterRunId>,
    },
    /// The live adapter lease does not own the producer id's namespace.
    ProducerOwnershipMismatch {
        origin: PublicationOrigin,
        producer_id: String,
    },
}

pub(super) enum ProducerCommand {
    Publish {
        origin: PublicationOrigin,
        document: Box<ProducerDocument>,
        verdict: Option<oneshot::Sender<Admission>>,
    },
    Retire {
        origin: PublicationOrigin,
        producer_id: String,
        epoch: ProducerEpoch,
        applied: Option<oneshot::Sender<usize>>,
    },
}

/// Cloneable producer-side handle. It can enqueue commands but cannot read or mutate state.
#[derive(Clone)]
pub struct AdaptiveHandle {
    commands: mpsc::Sender<ProducerCommand>,
    runs: Arc<AdapterRuns>,
}

impl AdaptiveHandle {
    /// Begin a synchronously registered adapter run.
    pub fn begin_run(&self, adapter: &str) -> AdapterRun {
        self.runs.begin(adapter)
    }

    /// Publish and wait for the aggregator's admission verdict.
    pub async fn publish(
        &self,
        run: &AdapterRun,
        document: ProducerDocument,
    ) -> Result<Admission, AdaptiveRuntimeClosed> {
        if !run.origin().owns(&document.producer.producer_id) {
            return Ok(Admission::Refused(
                AdmissionRefusal::ProducerOwnershipMismatch {
                    adapter: run.adapter().to_string(),
                    producer_id: document.producer.producer_id,
                },
            ));
        }
        let (verdict_tx, verdict_rx) = oneshot::channel();
        self.commands
            .send(ProducerCommand::Publish {
                origin: run.origin().clone(),
                document: Box::new(document),
                verdict: Some(verdict_tx),
            })
            .await
            .map_err(|_| AdaptiveRuntimeClosed)?;
        verdict_rx.await.map_err(|_| AdaptiveRuntimeClosed)
    }

    /// Publish losslessly without waiting for a verdict.
    pub async fn publish_detached(
        &self,
        run: &AdapterRun,
        document: ProducerDocument,
    ) -> Result<(), AdaptivePublishError> {
        if !run.origin().owns(&document.producer.producer_id) {
            return Err(AdaptivePublishError::ProducerOwnershipMismatch {
                origin: run.origin().clone(),
                producer_id: document.producer.producer_id,
            });
        }
        self.publish_detached_at(run.origin(), document)
            .await
            .map_err(|_| AdaptivePublishError::RuntimeClosed)
    }

    /// Publish for a recorded origin after the public lease-based method captured it.
    async fn publish_detached_at(
        &self,
        origin: &PublicationOrigin,
        document: ProducerDocument,
    ) -> Result<(), AdaptiveRuntimeClosed> {
        self.commands
            .send(ProducerCommand::Publish {
                origin: origin.clone(),
                document: Box::new(document),
                verdict: None,
            })
            .await
            .map_err(|_| AdaptiveRuntimeClosed)
    }

    /// Retire every target of a producer through `epoch`.
    pub async fn retire_detached(
        &self,
        run: &AdapterRun,
        producer_id: &str,
        epoch: ProducerEpoch,
    ) -> Result<RetirementOutcome, AdaptiveRuntimeClosed> {
        if !run.origin().owns(producer_id) {
            return Ok(RetirementOutcome::ProducerOwnershipMismatch {
                origin: run.origin().clone(),
                producer_id: producer_id.to_string(),
            });
        }
        let permit = self
            .commands
            .reserve()
            .await
            .map_err(|_| AdaptiveRuntimeClosed)?;
        let origin = run.origin().clone();
        match self.runs.with_active(&origin, || {
            permit.send(ProducerCommand::Retire {
                origin: origin.clone(),
                producer_id: producer_id.to_string(),
                epoch,
                applied: None,
            })
        }) {
            Ok(()) => Ok(RetirementOutcome::Committed),
            Err(active) => Ok(RetirementOutcome::StaleAdapterRun { origin, active }),
        }
    }

    /// Retire and wait until the actor has applied the command.
    pub async fn retire(
        &self,
        run: &AdapterRun,
        producer_id: &str,
        epoch: ProducerEpoch,
    ) -> Result<RetirementOutcome, AdaptiveRuntimeClosed> {
        if !run.origin().owns(producer_id) {
            return Ok(RetirementOutcome::ProducerOwnershipMismatch {
                origin: run.origin().clone(),
                producer_id: producer_id.to_string(),
            });
        }
        let (applied_tx, applied_rx) = oneshot::channel();
        let permit = self
            .commands
            .reserve()
            .await
            .map_err(|_| AdaptiveRuntimeClosed)?;
        let origin = run.origin().clone();
        if let Err(active) = self.runs.with_active(&origin, || {
            permit.send(ProducerCommand::Retire {
                origin: origin.clone(),
                producer_id: producer_id.to_string(),
                epoch,
                applied: Some(applied_tx),
            })
        }) {
            return Ok(RetirementOutcome::StaleAdapterRun { origin, active });
        }
        let removed = applied_rx.await.map_err(|_| AdaptiveRuntimeClosed)?;
        Ok(RetirementOutcome::Retired { removed })
    }
}

/// Read-only projection of actor-owned state.
#[derive(Clone)]
pub struct AdaptiveView {
    aggregator: Arc<ProducerAggregator>,
    events: SharedAdaptiveBus,
}

impl AdaptiveView {
    pub fn snapshot(&self, key: &ProducerKey) -> Option<ProducerSnapshot> {
        self.aggregator.snapshot_now(key)
    }

    pub fn snapshots(&self) -> Vec<ProducerSnapshot> {
        self.aggregator.snapshots_now()
    }

    pub fn refusals(&self) -> Vec<(ProducerKey, AdmissionRefusal)> {
        self.aggregator.refusals_now()
    }

    /// Subscribe to admitted/refused hints. Payloads remain readable only through this view.
    pub fn subscribe(&self) -> broadcast::Receiver<AdaptiveEvent> {
        self.events.subscribe()
    }
}

/// Exclusive command consumer. All ingress mutations happen in this task.
pub struct ProducerActor {
    commands: mpsc::Receiver<ProducerCommand>,
    shutdown: CancellationToken,
    aggregator: Arc<ProducerAggregator>,
}

impl ProducerActor {
    pub async fn run(mut self) {
        loop {
            tokio::select! {
                command = self.commands.recv() => match command {
                    Some(command) => self.apply(command).await,
                    None => break,
                },
                () = self.shutdown.cancelled() => {
                    self.drain_accepted().await;
                    break;
                },
            }
        }
    }

    /// Stop new sends and apply everything for which bounded ingress already returned success.
    async fn drain_accepted(&mut self) {
        self.commands.close();
        while let Some(command) = self.commands.recv().await {
            self.apply(command).await;
        }
    }

    async fn apply(&self, command: ProducerCommand) {
        match command {
            ProducerCommand::Publish {
                origin,
                document,
                verdict,
            } => {
                let admission = self.aggregator.ingest_from(&origin, *document).await;
                if let Some(verdict) = verdict {
                    if verdict.send(admission).is_err() {
                        tracing::trace!("adaptive publisher dropped before receiving its verdict");
                    }
                }
            }
            ProducerCommand::Retire {
                origin,
                producer_id,
                epoch,
                applied,
            } => {
                let removed = self
                    .aggregator
                    .retire_committed(&origin, &producer_id, epoch);
                if let Some(applied) = applied {
                    if applied.send(removed).is_err() {
                        tracing::trace!("adaptive publisher dropped before retirement completed");
                    }
                }
            }
        }
    }
}

/// Constructs the handle/actor/view split in one ready-before-publish operation.
pub struct AdaptiveRuntime;

impl AdaptiveRuntime {
    pub fn build(
        shutdown: CancellationToken,
        capacity: usize,
    ) -> (AdaptiveHandle, ProducerActor, AdaptiveView) {
        assert!(capacity > 0, "adaptive ingress capacity must be non-zero");
        let (commands_tx, commands_rx) = mpsc::channel(capacity);
        let runs = Arc::new(AdapterRuns::default());
        let events = create_adaptive_bus();
        let aggregator = Arc::new(ProducerAggregator::detached_with_runs_and_events(
            runs.clone(),
            events.clone(),
        ));
        let handle = AdaptiveHandle {
            commands: commands_tx,
            runs,
        };
        let actor = ProducerActor {
            commands: commands_rx,
            shutdown,
            aggregator: aggregator.clone(),
        };
        let view = AdaptiveView { aggregator, events };
        (handle, actor, view)
    }
}
