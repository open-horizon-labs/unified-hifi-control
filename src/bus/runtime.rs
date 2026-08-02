//! Reliable, bounded in-process lanes for commands and canonical projection ingress.
//!
//! `crate::bus::EventBus` remains a broadcast *egress* for SSE-compatible notifications. It is
//! intentionally not used here: a lagged broadcast receiver has missed data by definition. This
//! module gives future adapter migrations a private, lossless boundary without changing a public
//! HTTP/MCP route or the existing `BusEvent` wire enum.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use async_trait::async_trait;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::time::{timeout_at, Instant};

use crate::adapters::hqplayer::{HqpAdvancedOptionsSnapshot, HqpNativeObservation, HqpProfile};

use super::{Command, PrefixedZoneId, Zone};

/// Stable internal identity for one admitted command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CommandId(u64);

impl CommandId {
    /// Numeric representation for logs and a future operation-resource projection.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Commands whose native protocol operation has different latency and serialization needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandLane {
    /// Transport/volume interactions expected to respond promptly.
    Interactive,
    /// Slow configuration/profile changes that may trigger daemon recovery.
    Reconfiguration,
}

/// Internal deadlines. They are deliberately Tokio instants rather than a wire field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDeadlines {
    /// The endpoint must call [`EndpointWork::begin_dispatch`] before this instant.
    pub dispatch_by: Instant,
    /// A command started before this instant may still finish later, but becomes indeterminate to
    /// a caller until an observed projection commits.
    pub confirm_by: Instant,
}

/// A provider-neutral control or provider-owned internal command admitted to the reliable runtime.
///
/// This wrapper deliberately lives only on the private reliable-runtime boundary.  The public
/// `bus::Command` keeps its serialized compatibility contract: a provider must not have to add a
/// variant to that wire enum merely because it has a native control plane beyond transport.
#[derive(Debug, Clone, PartialEq)]
pub enum RuntimeCommand {
    /// A normal transport/volume command shared by providers.
    Control(Command),
    /// HQPlayer's semantic pipeline/profile control plane.
    Hqplayer(HqpRuntimeCommand),
}

impl From<Command> for RuntimeCommand {
    fn from(command: Command) -> Self {
        Self::Control(command)
    }
}

/// A semantic HQPlayer operation whose values are intentionally names/values, never list indices.
///
/// The adapter resolves a pipeline name against the exact daemon session that owns the endpoint.
/// Keeping the setting name and its value here lets the reliable lane stay independent from every
/// public HTTP/MCP request shape while ensuring stale enum positions can never cross the boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HqpRuntimeCommand {
    /// Change one pipeline setting. `setting` and `value` use the semantic spellings accepted by
    /// HQPlayer's named-setting layer (for example `mode` / `PCM`, or `rate` / `96000`).
    Pipeline { setting: String, value: String },
    /// Frozen legacy HTTP compatibility: the endpoint resolves this ephemeral list position
    /// against its current daemon session before any native write.
    LegacyPipelineIndex { setting: String, index: u32 },
    /// Refresh advanced native state/options through the exact-instance worker.
    RefreshAdvanced,
    /// Refresh the browser profile inventory through the exact-instance worker.
    RefreshProfiles,
    /// Load one named Embedded profile and wait for native recovery/readback.
    LoadProfile { profile: String },
}

/// A semantic command admitted to the reliable runtime.
#[derive(Debug, Clone, PartialEq)]
pub struct CommandRequest {
    pub target: PrefixedZoneId,
    pub command: RuntimeCommand,
    pub correlation_id: Option<String>,
    pub lane: CommandLane,
    pub deadlines: CommandDeadlines,
}

/// The authoritative lifecycle state for an operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandStatus {
    Queued,
    Dispatched,
    AwaitingProjection,
    /// Native dispatch began, but no matching projection committed by `confirm_by`.
    /// This is intentionally not final: later observed state can still resolve it to Confirmed.
    Indeterminate,
    Confirmed {
        projection_revision: u64,
    },
    Failed {
        detail: String,
    },
    NotDispatched {
        detail: String,
    },
}

impl CommandStatus {
    fn is_final(&self) -> bool {
        matches!(
            self,
            Self::Confirmed { .. } | Self::Failed { .. } | Self::NotDispatched { .. }
        )
    }
}

/// A caller's handle for observing a correlated operation without owning its execution lifetime.
pub struct CommandTicket {
    id: CommandId,
    status: watch::Receiver<CommandStatus>,
}

impl CommandTicket {
    pub fn id(&self) -> CommandId {
        self.id
    }

    pub fn status(&self) -> CommandStatus {
        self.status.borrow().clone()
    }

    /// Wait until the operation reaches a final state or becomes indeterminate.
    ///
    /// `Indeterminate` deliberately wakes the caller: a transport request must not hang merely
    /// because a backend accepted bytes and then took too long to make state observable.
    pub async fn wait_for_observable_result(&mut self) -> CommandStatus {
        loop {
            let current = self.status();
            if current.is_final() || current == CommandStatus::Indeterminate {
                return current;
            }
            if self.status.changed().await.is_err() {
                return self.status();
            }
        }
    }
}

/// An endpoint cannot be registered twice under the same key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointRegistrationError {
    EmptyKey,
    AlreadyRegistered(String),
}

/// Submission can refuse a reused correlation key which names a different command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandSubmissionError {
    CorrelationConflict(String),
}

/// Result an endpoint reports after a native attempt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NativeResult {
    /// The provider rejected or failed the attempted command.
    Failed(String),
    /// Native I/O was accepted. A matching projection is still required for confirmation.
    Accepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum EndpointKey {
    Provider(String),
    Zone(String),
}

impl EndpointKey {
    fn provider(provider: impl Into<String>) -> Result<Self, EndpointRegistrationError> {
        let provider = provider.into();
        if provider.trim().is_empty() {
            return Err(EndpointRegistrationError::EmptyKey);
        }
        Ok(Self::Provider(provider))
    }

    fn zone(zone: PrefixedZoneId) -> Self {
        Self::Zone(zone.into())
    }
}

#[derive(Clone)]
struct EndpointRegistry {
    endpoints: Arc<Mutex<HashMap<EndpointKey, EndpointSlot>>>,
    generation: Arc<AtomicU64>,
}

#[derive(Clone)]
struct EndpointSlot {
    generation: u64,
    sender: mpsc::Sender<EndpointWork>,
    reconfiguring: Arc<AtomicBool>,
}

#[derive(Clone)]
struct EndpointRoute {
    sender: mpsc::Sender<EndpointWork>,
    reconfiguring: Arc<AtomicBool>,
}

impl EndpointRegistry {
    fn register(
        &self,
        key: EndpointKey,
        capacity: usize,
    ) -> Result<CommandEndpoint, EndpointRegistrationError> {
        assert!(capacity > 0, "endpoint capacity must be non-zero");
        let mut endpoints = lock(&self.endpoints);
        if endpoints.contains_key(&key) {
            return Err(EndpointRegistrationError::AlreadyRegistered(
                endpoint_key_text(&key),
            ));
        }
        let (sender, receiver) = mpsc::channel(capacity);
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);
        let reconfiguring = Arc::new(AtomicBool::new(false));
        endpoints.insert(
            key.clone(),
            EndpointSlot {
                generation,
                sender,
                reconfiguring: reconfiguring.clone(),
            },
        );
        Ok(CommandEndpoint {
            receiver,
            key,
            generation,
            registry: Arc::downgrade(&self.endpoints),
            reconfiguring,
        })
    }

    fn route(&self, target: &PrefixedZoneId) -> Option<EndpointRoute> {
        let endpoints = lock(&self.endpoints);
        endpoints
            .get(&EndpointKey::Zone(target.to_string()))
            .or_else(|| endpoints.get(&EndpointKey::Provider(target.source().to_string())))
            .map(|slot| EndpointRoute {
                sender: slot.sender.clone(),
                reconfiguring: slot.reconfiguring.clone(),
            })
    }
}

/// Receiver owned by exactly one adapter worker. Dropping it unregisters the endpoint.
pub struct CommandEndpoint {
    receiver: mpsc::Receiver<EndpointWork>,
    key: EndpointKey,
    generation: u64,
    registry: Weak<Mutex<HashMap<EndpointKey, EndpointSlot>>>,
    reconfiguring: Arc<AtomicBool>,
}

impl CommandEndpoint {
    pub async fn recv(&mut self) -> Option<EndpointWork> {
        self.receiver.recv().await
    }

    /// Mark this logical endpoint as undergoing a slow configuration transition. While the guard
    /// is held, new interactive and competing reconfiguration commands fail admission immediately
    /// instead of waiting behind the slow operation. Other endpoints remain independent.
    pub fn try_begin_reconfiguration(&self) -> Result<ReconfigurationGuard, CommandStatus> {
        self.reconfiguring
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| CommandStatus::NotDispatched {
                detail: "target is already reconfiguring".to_string(),
            })?;
        Ok(ReconfigurationGuard {
            reconfiguring: self.reconfiguring.clone(),
        })
    }
}

pub struct ReconfigurationGuard {
    reconfiguring: Arc<AtomicBool>,
}

impl Drop for ReconfigurationGuard {
    fn drop(&mut self) {
        self.reconfiguring.store(false, Ordering::Release);
    }
}

impl Drop for CommandEndpoint {
    fn drop(&mut self) {
        let Some(registry) = self.registry.upgrade() else {
            return;
        };
        let mut endpoints = lock(&registry);
        if endpoints
            .get(&self.key)
            .is_some_and(|slot| slot.generation == self.generation)
        {
            endpoints.remove(&self.key);
        }
    }
}

/// A queued command delivered to an adapter endpoint. It cannot be executed until `begin_dispatch`
/// wins the deadline gate, which prevents late queue consumption from turning a timeout into an
/// unreported native attempt.
pub struct EndpointWork {
    request: CommandRequest,
    id: CommandId,
    inner: Arc<RuntimeState>,
}

impl EndpointWork {
    pub fn id(&self) -> CommandId {
        self.id
    }

    pub fn request(&self) -> &CommandRequest {
        &self.request
    }

    /// Acquire the irreversible native-dispatch lease. An expired work item must be discarded.
    pub fn begin_dispatch(self) -> Result<EndpointPermit, CommandStatus> {
        if Instant::now() >= self.request.deadlines.dispatch_by {
            self.inner
                .not_dispatched(self.id, "dispatch deadline elapsed before native I/O");
            return Err(CommandStatus::NotDispatched {
                detail: "dispatch deadline elapsed before native I/O".to_string(),
            });
        }
        if !self.inner.transition(self.id, |status| {
            matches!(status, CommandStatus::Queued).then_some(CommandStatus::Dispatched)
        }) {
            return Err(self
                .inner
                .status(self.id)
                .unwrap_or(CommandStatus::NotDispatched {
                    detail: "operation no longer accepts dispatch".to_string(),
                }));
        }
        self.inner
            .schedule_confirmation_timeout(self.id, self.request.deadlines.confirm_by);
        Ok(EndpointPermit {
            request: self.request,
            id: self.id,
            inner: self.inner,
        })
    }

    /// Refuse work before any native attempt (for example a provider is offline).
    pub fn refuse(self, detail: impl Into<String>) {
        self.inner.not_dispatched(self.id, &detail.into());
    }
}

/// The only capability which can report a native result for an admitted endpoint work item.
pub struct EndpointPermit {
    request: CommandRequest,
    id: CommandId,
    inner: Arc<RuntimeState>,
}

impl EndpointPermit {
    pub fn id(&self) -> CommandId {
        self.id
    }

    pub fn request(&self) -> &CommandRequest {
        &self.request
    }

    /// The adapter reports only native fact here. `Accepted` is never confirmation; confirmation
    /// is emitted only by [`ProjectionActor`] after a correlated commit.
    pub fn complete_native(self, result: NativeResult) {
        match result {
            NativeResult::Failed(detail) => self
                .inner
                .set_status(self.id, CommandStatus::Failed { detail }),
            NativeResult::Accepted => {
                let _ = self.inner.transition(self.id, |status| match status {
                    CommandStatus::Dispatched => Some(CommandStatus::AwaitingProjection),
                    // A confirmation timeout is a caller-visible uncertainty, not permission to
                    // discard late native evidence. The eventual projection can still confirm it.
                    CommandStatus::Indeterminate => Some(CommandStatus::Indeterminate),
                    _ => None,
                });
            }
        }
    }
}

#[derive(Clone)]
struct CommandRecord {
    target: PrefixedZoneId,
    command: RuntimeCommand,
    status: CommandStatus,
    updates: watch::Sender<CommandStatus>,
}

/// Surface-facing reliable command ingress.
#[derive(Clone)]
pub struct CommandGateway {
    inner: Arc<RuntimeState>,
    registry: EndpointRegistry,
}

impl CommandGateway {
    /// Whether an exact-zone or provider fallback endpoint currently owns this target. This is
    /// routing metadata, not adapter state; surfaces use it only to distinguish an unknown target
    /// from a temporarily withdrawn projection without reaching into an adapter registry.
    pub fn has_endpoint(&self, target: &PrefixedZoneId) -> bool {
        self.registry.route(target).is_some()
    }

    /// Register a provider-wide endpoint (Roon/LMS/OpenHome/UPnP).
    pub fn register_provider(
        &self,
        provider: impl Into<String>,
        capacity: usize,
    ) -> Result<CommandEndpoint, EndpointRegistrationError> {
        self.registry
            .register(EndpointKey::provider(provider)?, capacity)
    }

    /// Register an exact-zone endpoint (needed for independent HQPlayer instances).
    pub fn register_zone(
        &self,
        zone: PrefixedZoneId,
        capacity: usize,
    ) -> Result<CommandEndpoint, EndpointRegistrationError> {
        self.registry.register(EndpointKey::zone(zone), capacity)
    }

    /// Admit a command to the endpoint's bounded queue. A repeated correlation for the identical
    /// semantic request returns another watcher of the original operation; it never executes twice.
    pub async fn submit(
        &self,
        request: CommandRequest,
    ) -> Result<CommandTicket, CommandSubmissionError> {
        if let Some(correlation) = request.correlation_id.as_deref() {
            if let Some(ticket) = self.inner.ticket_for_correlation(correlation, &request)? {
                return Ok(ticket);
            }
        }

        let id = CommandId(self.inner.next_id.fetch_add(1, Ordering::Relaxed));
        let (updates, receiver) = watch::channel(CommandStatus::Queued);
        self.inner.insert_record(id, &request, updates);
        let ticket = CommandTicket {
            id,
            status: receiver,
        };

        let Some(endpoint) = self.registry.route(&request.target) else {
            self.inner.not_dispatched(
                id,
                "no reliable endpoint is registered for the target provider",
            );
            return Ok(ticket);
        };
        if endpoint.reconfiguring.load(Ordering::Acquire) {
            self.inner.not_dispatched(
                id,
                "target is reconfiguring; retry after its confirmed projection commits",
            );
            return Ok(ticket);
        }

        let work = EndpointWork {
            request: request.clone(),
            id,
            inner: self.inner.clone(),
        };
        match timeout_at(request.deadlines.dispatch_by, endpoint.sender.reserve()).await {
            Ok(Ok(permit)) => {
                let () = permit.send(work);
                self.inner
                    .schedule_dispatch_expiry(id, request.deadlines.dispatch_by);
            }
            Ok(Err(_)) => self
                .inner
                .not_dispatched(id, "target endpoint stopped before queue admission"),
            Err(_) => self.inner.not_dispatched(
                id,
                "dispatch deadline elapsed before endpoint queue admission",
            ),
        }
        Ok(ticket)
    }
}

/// Per-source ordering metadata carried with every canonical projection update.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ProjectionSource {
    pub adapter: String,
    pub instance: Option<String>,
    pub epoch: u64,
}

impl ProjectionSource {
    pub fn identity(&self) -> String {
        match &self.instance {
            Some(instance) => format!("{}/{}", self.adapter, instance),
            None => self.adapter.clone(),
        }
    }
}

/// A private typed payload seam. The future aggregator migration can add a coherent HQPlayer
/// snapshot variant without putting it on the public SSE `BusEvent` enum.
#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionPayload {
    Zone(Box<Zone>),
    /// One generation-fenced HQPlayer observation.  This stays on the private projection lane:
    /// the legacy HTTP payload is still projected by the aggregator from this native fact.
    HqpObservation(Box<HqpNativeObservation>),
    HqpAdvanced {
        instance_name: String,
        snapshot: Box<HqpAdvancedOptionsSnapshot>,
    },
    HqpProfiles {
        instance_name: String,
        result: Result<Vec<HqpProfile>, String>,
    },
    HqpTransientFailure {
        instance_name: String,
        observed_at: std::time::SystemTime,
    },
    HqpRemoved {
        instance_name: String,
        producer_epoch: u64,
    },
    HqpManagerStopped,
    /// Test/control-plane payload used until a provider-specific typed snapshot is wired.
    Marker(String),
}

/// One entry in an atomic projection transaction.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionEntry {
    pub key: String,
    pub payload: ProjectionPayload,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionKind {
    /// A coherent source snapshot can close a detected sequence gap.
    Snapshot,
    /// A partial change requires every preceding sequence to have been admitted.
    Delta,
}

/// A source observation submitted to the single projection actor.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectionUpdate {
    pub source: ProjectionSource,
    pub sequence: u64,
    pub kind: ProjectionKind,
    /// A read-back verified command may nominate itself here. Native acknowledgement alone cannot.
    pub caused_by: Option<CommandId>,
    /// All entries share one revision, so a future direct HQPlayer Zone + native snapshot commit
    /// can be visible atomically.
    pub entries: Vec<ProjectionEntry>,
}

/// The result of a projection ingress submission.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProjectionCommit {
    Committed {
        revision: u64,
    },
    StaleIgnored {
        current_epoch: u64,
        current_sequence: u64,
    },
    GapDetected {
        expected_sequence: u64,
        received_sequence: u64,
    },
}

/// The single canonical projection owner. The reliable runtime serializes ingress and command
/// correlation, but it deliberately does not store projection data, revisions, or source cursors.
/// That prevents it from becoming a second state authority beside `ZoneAggregator`.
#[async_trait]
pub trait ProjectionCommitter: Send + Sync {
    async fn commit_projection(&self, update: ProjectionUpdate) -> ProjectionCommit;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionFreshness {
    Fresh,
    Reconciling,
}

/// A post-commit egress hint. Consumers must reread the aggregator, never treat this as state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuntimeNotification {
    ProjectionCommitted {
        revision: u64,
        source: String,
        caused_by: Option<CommandId>,
    },
    CommandStateChanged {
        id: CommandId,
        status: CommandStatus,
    },
    ProjectionGapDetected {
        source: String,
        expected_sequence: u64,
        received_sequence: u64,
    },
}

/// Adapter-facing bounded projection ingress.
#[derive(Clone)]
pub struct ProjectionIngress {
    submissions: mpsc::Sender<ProjectionSubmission>,
}

impl ProjectionIngress {
    pub async fn submit(
        &self,
        update: ProjectionUpdate,
    ) -> Result<ProjectionCommit, ProjectionIngressClosed> {
        let (reply, response) = oneshot::channel();
        self.submissions
            .send(ProjectionSubmission { update, reply })
            .await
            .map_err(|_| ProjectionIngressClosed)?;
        response.await.map_err(|_| ProjectionIngressClosed)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectionIngressClosed;

struct ProjectionSubmission {
    update: ProjectionUpdate,
    reply: oneshot::Sender<ProjectionCommit>,
}

/// Exclusive projection consumer. Composition starts this once before adapters publish.
pub struct ProjectionActor {
    submissions: mpsc::Receiver<ProjectionSubmission>,
    inner: Arc<RuntimeState>,
    committer: Arc<dyn ProjectionCommitter>,
}

impl ProjectionActor {
    pub async fn run(mut self) {
        while let Some(submission) = self.submissions.recv().await {
            let source = submission.update.source.clone();
            let caused_by = submission.update.caused_by;
            let commit = self.committer.commit_projection(submission.update).await;
            self.inner
                .record_projection_outcome(&source, caused_by, &commit);
            if submission.reply.send(commit).is_err() {
                tracing::trace!("projection submitter dropped before commit acknowledgement");
            }
        }
    }
}

/// Post-commit notification seam. Its events are hints only; all state reads remain owned by the
/// projection committer (currently `ZoneAggregator`).
#[derive(Clone)]
pub struct ProjectionView {
    inner: Arc<RuntimeState>,
}

impl ProjectionView {
    pub fn subscribe(&self) -> broadcast::Receiver<RuntimeNotification> {
        self.inner.notifications.subscribe()
    }
}

/// The four handles needed by composition. The actor is intentionally separate: it makes the
/// ready-before-adapter startup ordering explicit and simple to test.
pub struct RuntimeParts {
    pub commands: CommandGateway,
    pub projection_ingress: ProjectionIngress,
    pub projection_actor: ProjectionActor,
    pub projection_view: ProjectionView,
}

/// Construct the bounded reliable lanes. Neither capacity may be zero.
pub fn build_runtime(
    committer: Arc<dyn ProjectionCommitter>,
    command_capacity: usize,
    projection_capacity: usize,
) -> RuntimeParts {
    assert!(command_capacity > 0, "command capacity must be non-zero");
    assert!(
        projection_capacity > 0,
        "projection capacity must be non-zero"
    );
    let (submissions, receiver) = mpsc::channel(projection_capacity);
    let (notifications, _) = broadcast::channel(256);
    let inner = Arc::new(RuntimeState {
        next_id: AtomicU64::new(1),
        data: Mutex::new(RuntimeData::default()),
        notifications,
    });
    let registry = EndpointRegistry {
        endpoints: Arc::new(Mutex::new(HashMap::new())),
        generation: Arc::new(AtomicU64::new(1)),
    };
    let commands = CommandGateway {
        inner: inner.clone(),
        registry,
    };
    RuntimeParts {
        commands,
        projection_ingress: ProjectionIngress { submissions },
        projection_actor: ProjectionActor {
            submissions: receiver,
            inner: inner.clone(),
            committer,
        },
        projection_view: ProjectionView { inner },
    }
}

struct RuntimeState {
    next_id: AtomicU64,
    data: Mutex<RuntimeData>,
    notifications: broadcast::Sender<RuntimeNotification>,
}

#[derive(Default)]
struct RuntimeData {
    commands: HashMap<CommandId, CommandRecord>,
    correlations: HashMap<String, CommandId>,
}

impl RuntimeState {
    fn insert_record(
        &self,
        id: CommandId,
        request: &CommandRequest,
        updates: watch::Sender<CommandStatus>,
    ) {
        let mut data = lock(&self.data);
        data.commands.insert(
            id,
            CommandRecord {
                target: request.target.clone(),
                command: request.command.clone(),
                status: CommandStatus::Queued,
                updates,
            },
        );
        if let Some(correlation) = &request.correlation_id {
            data.correlations.insert(correlation.clone(), id);
        }
    }

    fn ticket_for_correlation(
        &self,
        correlation: &str,
        request: &CommandRequest,
    ) -> Result<Option<CommandTicket>, CommandSubmissionError> {
        let data = lock(&self.data);
        let Some(id) = data.correlations.get(correlation).copied() else {
            return Ok(None);
        };
        let Some(record) = data.commands.get(&id) else {
            return Ok(None);
        };
        if record.target != request.target || record.command != request.command {
            return Err(CommandSubmissionError::CorrelationConflict(
                correlation.to_string(),
            ));
        }
        Ok(Some(CommandTicket {
            id,
            status: record.updates.subscribe(),
        }))
    }

    fn status(&self, id: CommandId) -> Option<CommandStatus> {
        lock(&self.data)
            .commands
            .get(&id)
            .map(|record| record.status.clone())
    }

    fn set_status(&self, id: CommandId, next: CommandStatus) {
        let changed = {
            let mut data = lock(&self.data);
            let Some(record) = data.commands.get_mut(&id) else {
                return;
            };
            if record.status == next {
                None
            } else {
                record.status = next.clone();
                if record.updates.send(next.clone()).is_err() {
                    tracing::trace!(
                        command_id = id.get(),
                        "command has no active status watcher"
                    );
                }
                Some(next)
            }
        };
        if let Some(status) = changed {
            self.notify(RuntimeNotification::CommandStateChanged { id, status });
        }
    }

    fn transition(
        &self,
        id: CommandId,
        transition: impl FnOnce(&CommandStatus) -> Option<CommandStatus>,
    ) -> bool {
        let changed = {
            let mut data = lock(&self.data);
            let Some(record) = data.commands.get_mut(&id) else {
                return false;
            };
            let Some(next) = transition(&record.status) else {
                return false;
            };
            record.status = next.clone();
            if record.updates.send(next.clone()).is_err() {
                tracing::trace!(
                    command_id = id.get(),
                    "command has no active status watcher"
                );
            }
            Some(next)
        };
        if let Some(status) = changed {
            self.notify(RuntimeNotification::CommandStateChanged { id, status });
        }
        true
    }

    fn not_dispatched(&self, id: CommandId, detail: &str) {
        let _ = self.transition(id, |status| {
            matches!(status, CommandStatus::Queued).then_some(CommandStatus::NotDispatched {
                detail: detail.to_string(),
            })
        });
    }

    fn schedule_dispatch_expiry(self: &Arc<Self>, id: CommandId, deadline: Instant) {
        let inner = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            inner.not_dispatched(id, "dispatch deadline elapsed before native I/O");
        });
    }

    fn schedule_confirmation_timeout(self: &Arc<Self>, id: CommandId, deadline: Instant) {
        let inner = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep_until(deadline).await;
            let _ = inner.transition(id, |status| {
                matches!(
                    status,
                    CommandStatus::Dispatched | CommandStatus::AwaitingProjection
                )
                .then_some(CommandStatus::Indeterminate)
            });
        });
    }

    fn record_projection_outcome(
        &self,
        source: &ProjectionSource,
        caused_by: Option<CommandId>,
        commit: &ProjectionCommit,
    ) {
        match commit {
            ProjectionCommit::Committed { revision } => {
                let command_update = caused_by.and_then(|id| {
                    let mut data = lock(&self.data);
                    let record = data.commands.get_mut(&id)?;
                    if !source_matches_target(source, &record.target) {
                        return None;
                    }
                    match record.status {
                        CommandStatus::AwaitingProjection
                        | CommandStatus::Dispatched
                        | CommandStatus::Indeterminate => {
                            let status = CommandStatus::Confirmed {
                                projection_revision: *revision,
                            };
                            record.status = status.clone();
                            if record.updates.send(status.clone()).is_err() {
                                tracing::trace!(
                                    command_id = id.get(),
                                    "confirmed command has no active status watcher"
                                );
                            }
                            Some((id, status))
                        }
                        _ => None,
                    }
                });
                self.notify(RuntimeNotification::ProjectionCommitted {
                    revision: *revision,
                    source: source.identity(),
                    caused_by,
                });
                if let Some((id, status)) = command_update {
                    self.notify(RuntimeNotification::CommandStateChanged { id, status });
                }
            }
            ProjectionCommit::GapDetected {
                expected_sequence,
                received_sequence,
            } => self.notify(RuntimeNotification::ProjectionGapDetected {
                source: source.identity(),
                expected_sequence: *expected_sequence,
                received_sequence: *received_sequence,
            }),
            ProjectionCommit::StaleIgnored { .. } => {}
        }
    }

    fn notify(&self, event: RuntimeNotification) {
        if self.notifications.send(event).is_err() {
            tracing::trace!("reliable runtime has no post-commit notification subscribers");
        }
    }
}

fn endpoint_key_text(key: &EndpointKey) -> String {
    match key {
        EndpointKey::Provider(provider) | EndpointKey::Zone(provider) => provider.clone(),
    }
}

fn source_matches_target(source: &ProjectionSource, target: &PrefixedZoneId) -> bool {
    if source.adapter != target.source() {
        return false;
    }
    match source.instance.as_deref() {
        Some(instance) => instance == target.raw_id(),
        None => target.source() != "hqplayer",
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Runtime tests intentionally use a minimal external committer. Projection state itself is
    /// covered by `ZoneAggregator` tests: keeping this fake state-free prevents the runtime from
    /// regaining a second projection authority through its test harness.
    #[derive(Default)]
    struct TestCommitter {
        next_revision: AtomicU64,
    }

    #[async_trait::async_trait]
    impl ProjectionCommitter for TestCommitter {
        async fn commit_projection(&self, _update: ProjectionUpdate) -> ProjectionCommit {
            ProjectionCommit::Committed {
                revision: self.next_revision.fetch_add(1, Ordering::Relaxed) + 1,
            }
        }
    }

    fn test_runtime() -> RuntimeParts {
        build_runtime(Arc::new(TestCommitter::default()), 4, 4)
    }

    fn request(target: PrefixedZoneId, correlation: Option<&str>) -> CommandRequest {
        let now = Instant::now();
        CommandRequest {
            target,
            command: RuntimeCommand::Control(Command::Play),
            correlation_id: correlation.map(str::to_string),
            lane: CommandLane::Interactive,
            deadlines: CommandDeadlines {
                dispatch_by: now + Duration::from_secs(5),
                confirm_by: now + Duration::from_secs(10),
            },
        }
    }

    fn projection(
        source: ProjectionSource,
        sequence: u64,
        caused_by: Option<CommandId>,
    ) -> ProjectionUpdate {
        ProjectionUpdate {
            source,
            sequence,
            kind: ProjectionKind::Snapshot,
            caused_by,
            entries: vec![ProjectionEntry {
                key: "zone:one".to_string(),
                payload: ProjectionPayload::Marker("observed".to_string()),
            }],
        }
    }

    #[tokio::test]
    async fn exact_endpoint_receives_once_and_provider_fallback_does_not() {
        let parts = test_runtime();
        let mut provider = match parts.commands.register_provider("hqplayer", 1) {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("provider endpoint: {error:?}"),
        };
        let mut exact = match parts
            .commands
            .register_zone(PrefixedZoneId::hqplayer("office"), 1)
        {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("exact endpoint: {error:?}"),
        };
        let ticket = match parts
            .commands
            .submit(request(PrefixedZoneId::hqplayer("office"), None))
            .await
        {
            Ok(ticket) => ticket,
            Err(error) => panic!("submit: {error:?}"),
        };
        assert_eq!(ticket.status(), CommandStatus::Queued);
        let work = match exact.recv().await {
            Some(work) => work,
            None => panic!("exact endpoint closed"),
        };
        assert_eq!(work.id(), ticket.id());
        assert!(provider.receiver.try_recv().is_err());
    }

    #[tokio::test(start_paused = true)]
    async fn work_expiring_before_dispatch_is_never_permitted_to_execute() {
        let parts = test_runtime();
        let mut endpoint = match parts.commands.register_provider("roon", 1) {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("endpoint: {error:?}"),
        };
        let now = Instant::now();
        let mut late = request(PrefixedZoneId::roon("zone"), None);
        late.deadlines = CommandDeadlines {
            dispatch_by: now + Duration::from_secs(1),
            confirm_by: now + Duration::from_secs(2),
        };
        let ticket = match parts.commands.submit(late).await {
            Ok(ticket) => ticket,
            Err(error) => panic!("submit: {error:?}"),
        };
        let work = match endpoint.recv().await {
            Some(work) => work,
            None => panic!("endpoint closed"),
        };
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(matches!(
            work.begin_dispatch(),
            Err(CommandStatus::NotDispatched { .. })
        ));
        assert!(matches!(
            ticket.status(),
            CommandStatus::NotDispatched { .. }
        ));
    }

    #[tokio::test]
    async fn duplicate_correlation_observes_one_operation_not_two_native_dispatches() {
        let parts = test_runtime();
        let mut endpoint = match parts.commands.register_provider("lms", 2) {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("endpoint: {error:?}"),
        };
        let first = match parts
            .commands
            .submit(request(PrefixedZoneId::lms("player"), Some("gesture-1")))
            .await
        {
            Ok(ticket) => ticket,
            Err(error) => panic!("first submit: {error:?}"),
        };
        let duplicate = match parts
            .commands
            .submit(request(PrefixedZoneId::lms("player"), Some("gesture-1")))
            .await
        {
            Ok(ticket) => ticket,
            Err(error) => panic!("duplicate submit: {error:?}"),
        };
        assert_eq!(first.id(), duplicate.id());
        assert!(endpoint.recv().await.is_some());
        assert!(endpoint.receiver.try_recv().is_err());
    }

    #[tokio::test]
    async fn correlation_identity_includes_hqplayer_semantic_commands() {
        let parts = test_runtime();
        let _endpoint = match parts
            .commands
            .register_zone(PrefixedZoneId::hqplayer("main"), 2)
        {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("endpoint: {error:?}"),
        };
        let first = CommandRequest {
            target: PrefixedZoneId::hqplayer("main"),
            command: RuntimeCommand::Hqplayer(HqpRuntimeCommand::Pipeline {
                setting: "mode".to_string(),
                value: "PCM".to_string(),
            }),
            correlation_id: Some("hqp-change-1".to_string()),
            lane: CommandLane::Reconfiguration,
            deadlines: request(PrefixedZoneId::hqplayer("main"), None).deadlines,
        };
        let duplicate = parts.commands.submit(first.clone()).await;
        assert!(duplicate.is_ok(), "identical semantic request deduplicates");
        let changed = CommandRequest {
            command: RuntimeCommand::Hqplayer(HqpRuntimeCommand::Pipeline {
                setting: "mode".to_string(),
                value: "SDM (DSD)".to_string(),
            }),
            ..first
        };
        assert!(
            matches!(
                parts.commands.submit(changed).await,
                Err(CommandSubmissionError::CorrelationConflict(correlation))
                    if correlation == "hqp-change-1"
            ),
            "a retried correlation cannot silently apply a different pipeline change"
        );
    }

    #[tokio::test]
    async fn reconfiguration_guard_refuses_interactive_work_before_native_dispatch() {
        let parts = test_runtime();
        let mut endpoint = match parts
            .commands
            .register_zone(PrefixedZoneId::hqplayer("main"), 2)
        {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("endpoint: {error:?}"),
        };
        let mut reconfiguration = request(PrefixedZoneId::hqplayer("main"), None);
        reconfiguration.lane = CommandLane::Reconfiguration;
        let _ticket = match parts.commands.submit(reconfiguration).await {
            Ok(ticket) => ticket,
            Err(error) => panic!("submit reconfiguration: {error:?}"),
        };
        let work = match endpoint.recv().await {
            Some(work) => work,
            None => panic!("endpoint closed"),
        };
        let guard = match endpoint.try_begin_reconfiguration() {
            Ok(guard) => guard,
            Err(status) => panic!("guard: {status:?}"),
        };
        let interactive = match parts
            .commands
            .submit(request(PrefixedZoneId::hqplayer("main"), None))
            .await
        {
            Ok(ticket) => ticket,
            Err(error) => panic!("submit interactive: {error:?}"),
        };
        assert!(matches!(
            interactive.status(),
            CommandStatus::NotDispatched { .. }
        ));
        drop(guard);
        work.refuse("test complete");
    }

    #[tokio::test]
    async fn native_acceptance_requires_a_correlated_projection_commit_to_confirm() {
        let parts = test_runtime();
        let mut endpoint = match parts.commands.register_provider("roon", 1) {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("endpoint: {error:?}"),
        };
        let actor = tokio::spawn(parts.projection_actor.run());
        let mut ticket = match parts
            .commands
            .submit(request(PrefixedZoneId::roon("zone"), None))
            .await
        {
            Ok(ticket) => ticket,
            Err(error) => panic!("submit: {error:?}"),
        };
        let work = match endpoint.recv().await {
            Some(work) => work,
            None => panic!("endpoint closed"),
        };
        let permit = match work.begin_dispatch() {
            Ok(permit) => permit,
            Err(status) => panic!("dispatch refused: {status:?}"),
        };
        permit.complete_native(NativeResult::Accepted);
        assert_eq!(ticket.status(), CommandStatus::AwaitingProjection);
        let commit = match parts
            .projection_ingress
            .submit(projection(
                ProjectionSource {
                    adapter: "roon".to_string(),
                    instance: None,
                    epoch: 4,
                },
                1,
                Some(ticket.id()),
            ))
            .await
        {
            Ok(commit) => commit,
            Err(_) => panic!("projection actor closed"),
        };
        assert_eq!(commit, ProjectionCommit::Committed { revision: 1 });
        assert_eq!(
            ticket.wait_for_observable_result().await,
            CommandStatus::Confirmed {
                projection_revision: 1
            }
        );
        drop(parts.projection_ingress);
        match actor.await {
            Ok(()) => {}
            Err(error) => panic!("projection actor join: {error}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn confirmation_timeout_is_indeterminate_and_late_observation_resolves_it() {
        let parts = test_runtime();
        let mut endpoint = match parts
            .commands
            .register_zone(PrefixedZoneId::hqplayer("main"), 1)
        {
            Ok(endpoint) => endpoint,
            Err(error) => panic!("endpoint: {error:?}"),
        };
        let actor = tokio::spawn(parts.projection_actor.run());
        let now = Instant::now();
        let mut slow = request(PrefixedZoneId::hqplayer("main"), Some("profile-load"));
        slow.lane = CommandLane::Reconfiguration;
        slow.deadlines = CommandDeadlines {
            dispatch_by: now + Duration::from_secs(1),
            confirm_by: now + Duration::from_secs(30),
        };
        let mut ticket = match parts.commands.submit(slow).await {
            Ok(ticket) => ticket,
            Err(error) => panic!("submit: {error:?}"),
        };
        let work = match endpoint.recv().await {
            Some(work) => work,
            None => panic!("endpoint closed"),
        };
        let permit = match work.begin_dispatch() {
            Ok(permit) => permit,
            Err(status) => panic!("dispatch refused: {status:?}"),
        };
        permit.complete_native(NativeResult::Accepted);
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(
            ticket.wait_for_observable_result().await,
            CommandStatus::Indeterminate
        );
        let commit = parts
            .projection_ingress
            .submit(projection(
                ProjectionSource {
                    adapter: "hqplayer".to_string(),
                    instance: Some("main".to_string()),
                    epoch: 2,
                },
                1,
                Some(ticket.id()),
            ))
            .await;
        assert_eq!(commit, Ok(ProjectionCommit::Committed { revision: 1 }));
        assert_eq!(
            ticket.status(),
            CommandStatus::Confirmed {
                projection_revision: 1
            }
        );
        drop(parts.projection_ingress);
        match actor.await {
            Ok(()) => {}
            Err(error) => panic!("projection actor join: {error}"),
        }
    }

    #[tokio::test]
    async fn post_commit_notification_is_only_a_reread_hint() {
        let parts = test_runtime();
        let mut notifications = parts.projection_view.subscribe();
        let actor = tokio::spawn(parts.projection_actor.run());
        let commit = parts
            .projection_ingress
            .submit(projection(
                ProjectionSource {
                    adapter: "upnp".to_string(),
                    instance: None,
                    epoch: 1,
                },
                1,
                None,
            ))
            .await;
        assert_eq!(commit, Ok(ProjectionCommit::Committed { revision: 1 }));
        let event = match notifications.recv().await {
            Ok(event) => event,
            Err(error) => panic!("notification: {error}"),
        };
        assert!(matches!(
            event,
            RuntimeNotification::ProjectionCommitted { revision: 1, .. }
        ));
        drop(parts.projection_ingress);
        match actor.await {
            Ok(()) => {}
            Err(error) => panic!("projection actor join: {error}"),
        }
    }

    #[tokio::test]
    async fn unrelated_source_cannot_confirm_a_command() {
        let parts = test_runtime();
        let mut endpoint = parts
            .commands
            .register_provider("roon", 1)
            .expect("roon endpoint");
        let actor = tokio::spawn(parts.projection_actor.run());
        let ticket = parts
            .commands
            .submit(request(PrefixedZoneId::roon("zone"), None))
            .await
            .expect("submit");
        endpoint
            .recv()
            .await
            .expect("work")
            .begin_dispatch()
            .expect("dispatch")
            .complete_native(NativeResult::Accepted);

        parts
            .projection_ingress
            .submit(projection(
                ProjectionSource {
                    adapter: "lms".to_string(),
                    instance: None,
                    epoch: 1,
                },
                1,
                Some(ticket.id()),
            ))
            .await
            .expect("unrelated projection still commits its own state");
        assert_eq!(ticket.status(), CommandStatus::AwaitingProjection);

        drop(parts.projection_ingress);
        actor.await.expect("projection actor");
    }

    #[tokio::test]
    async fn reconfiguration_is_busy_only_for_the_same_endpoint() {
        let parts = test_runtime();
        let hqp_main = parts
            .commands
            .register_zone(PrefixedZoneId::hqplayer("main"), 2)
            .expect("main endpoint");
        let mut hqp_office = parts
            .commands
            .register_zone(PrefixedZoneId::hqplayer("office"), 2)
            .expect("office endpoint");
        let _guard = hqp_main
            .try_begin_reconfiguration()
            .expect("profile lane starts");

        let main = parts
            .commands
            .submit(request(PrefixedZoneId::hqplayer("main"), None))
            .await
            .expect("main submit");
        assert!(matches!(
            main.status(),
            CommandStatus::NotDispatched { ref detail } if detail.contains("reconfiguring")
        ));

        let office = parts
            .commands
            .submit(request(PrefixedZoneId::hqplayer("office"), None))
            .await
            .expect("office submit");
        assert_eq!(
            hqp_office.recv().await.expect("office work").id(),
            office.id()
        );
    }
}
