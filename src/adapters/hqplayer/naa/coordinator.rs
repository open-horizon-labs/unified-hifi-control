//! Per-instance output coordinator: the single owner of output-routing mutations.
//!
//! Executed only from the exact-instance reliable command endpoint (`run_hqplayer_command_endpoint`
//! in `hqplayer.rs`). It owns the [`NaaRelay`] guard, the bounded operation history, the output
//! mutation revision and the one pending select continuation. Native HQPlayer transport work
//! (State/Status/Stop/Play/Seek) is delegated to `HqpAdapter::output_hook_*`, which hold the
//! adapter's operation lease so profile and pipeline operations share it.
//!
//! Cancellation model: every select gets a fresh [`CancellationToken`] and a relay generation.
//! `Stop`, a newer `select`, a reconfiguration-lane command, `configure`, removal and shutdown all
//! call [`HqpOutputCoordinator::supersede`], which cancels the token and bumps the generation
//! before doing their own work. The continuation wraps every await in `select!` against its token
//! and re-checks the generation before every native call, so a cancelled select can never issue a
//! late Play or Seek, and Stop never waits behind it.

use std::collections::VecDeque;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use super::super::{HqpAdapter, HqpNativeWorker, NativeHookFence, NativeHookOutcome};
use super::discovery;
use super::outputs::{
    HqpImportConflict, HqpImportPreview, HqpNativeControlView, HqpOutputAction,
    HqpOutputAvailability, HqpOutputCommandRequest, HqpOutputError, HqpOutputEvidence,
    HqpOutputOperation, HqpOutputOutcome, HqpOutputPhase, HqpOutputProjection, HqpOutputRefusal,
    HqpOutputResult, HqpOutputRoute, HqpSetupPreview, HqpSetupTransaction, NaaRelaySettings,
    OPERATION_HISTORY_LIMIT, VIRTUAL_DEVICE_ID,
};
use super::relay::{NaaRelay, RelayObservation};
use super::setup;
use crate::bus::runtime::CommandId;

/// Budgets for the one-click sequence. Tests shorten them; production mirrors the PoC's live values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HqpOutputTimeouts {
    /// Whole select budget after the native Stop, covering NAA reconnect, Play, audio and Seek.
    pub select_deadline: Duration,
    /// Each individual native round trip.
    pub control_step: Duration,
    /// Polling interval while waiting for relay milestones.
    pub poll: Duration,
}

impl Default for HqpOutputTimeouts {
    fn default() -> Self {
        Self {
            select_deadline: Duration::from_secs(10),
            control_step: Duration::from_secs(3),
            poll: Duration::from_millis(50),
        }
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct Pending {
    token: CancellationToken,
    operation_id: String,
}

#[derive(Default)]
struct Ledger {
    operations: VecDeque<HqpOutputOperation>,
    current_operation_id: Option<String>,
    /// Mutation/configuration revision. Telemetry (bytes, session state) never moves it.
    output_revision: u64,
    native: HqpNativeControlView,
    last_error: Option<HqpOutputError>,
    /// Set while the instance lifecycle has stopped the relay deliberately; overrides the relay's
    /// own `Disabled` so a stopped-but-enabled relay reads as unavailable with its reason.
    lifecycle_stopped: Option<(String, u64)>,
    /// The last derived setup proposal and, once applied, the bytes to compare readbacks against.
    setup: Option<SetupSession>,
}

/// One-time setup transaction state. The pre-apply form and persistent bytes are also persisted
/// to owner-only files so rollback survives a restart of UHC.
#[derive(Clone)]
struct SetupSession {
    preview: HqpSetupPreview,
    /// Identity of the successful controls the preview derived from.
    baseline_fingerprint: String,
    /// The running form as read at preview time: what rollback re-posts.
    baseline_fields: Vec<(String, String)>,
    /// The complete form apply posts.
    proposed_fields: Vec<(String, String)>,
    /// Persistent configuration bytes at preview time: what rollback restores on disk.
    backup: Vec<u8>,
    applied: bool,
}

/// What the running form and persistent configuration must show after a setup step.
struct SetupExpectation {
    backend: String,
    net_device: Option<String>,
    disk: SetupDiskExpectation,
}

enum SetupDiskExpectation {
    /// Persistent output names this relay identity.
    Identity { address: String, device: String },
    /// Persistent bytes equal exactly.
    Bytes(Vec<u8>),
}

impl SetupExpectation {
    fn runtime_matches(&self, form: &setup::ConfigForm) -> bool {
        form.value("backend") == Some(self.backend.as_str())
            && (self.net_device.is_none() || form.value("net_device") == self.net_device.as_deref())
    }

    fn disk_matches(&self, bytes: &[u8]) -> bool {
        match &self.disk {
            SetupDiskExpectation::Bytes(expected) => expected.as_slice() == bytes,
            SetupDiskExpectation::Identity { address, device } => setup::disk_output(bytes)
                .is_some_and(|d| {
                    d.output_type.as_deref() == Some(setup::BACKEND_NETWORK)
                        && d.network_address.as_deref() == Some(address.as_str())
                        && d.network_device.as_deref() == Some(device.as_str())
                }),
        }
    }
}

/// Rollback material persisted with the pre-apply state.
#[derive(serde::Serialize, serde::Deserialize)]
struct SetupRollbackFile {
    baseline_fields: Vec<(String, String)>,
    backup_sha256: String,
}

type RollbackFiles = (Vec<(String, String)>, Vec<u8>);

struct Publisher {
    shutdown: CancellationToken,
    join: tokio::task::JoinHandle<()>,
}

pub struct HqpOutputCoordinator {
    instance: Mutex<Option<String>>,
    settings: Mutex<NaaRelaySettings>,
    relay: Mutex<Option<Arc<NaaRelay>>>,
    ledger: Mutex<Ledger>,
    pending: Mutex<Option<Pending>>,
    /// The in-flight setup transaction: its generation (identity) and cancellation token.
    /// Superseded like a pending select; a stale transaction's cleanup never clears a newer one.
    pending_setup: Mutex<Option<(u64, CancellationToken)>>,
    setup_generation: std::sync::atomic::AtomicU64,
    publisher: tokio::sync::Mutex<Option<Publisher>>,
    worker: Mutex<Option<HqpNativeWorker>>,
    persist_path: Mutex<Option<PathBuf>>,
    timeouts: Mutex<HqpOutputTimeouts>,
}

impl HqpOutputCoordinator {
    pub fn new(settings: NaaRelaySettings) -> Self {
        Self {
            instance: Mutex::new(None),
            settings: Mutex::new(settings),
            relay: Mutex::new(None),
            ledger: Mutex::new(Ledger::default()),
            pending: Mutex::new(None),
            pending_setup: Mutex::new(None),
            setup_generation: std::sync::atomic::AtomicU64::new(0),
            publisher: tokio::sync::Mutex::new(None),
            worker: Mutex::new(None),
            persist_path: Mutex::new(None),
            timeouts: Mutex::new(HqpOutputTimeouts::default()),
        }
    }

    pub fn settings(&self) -> NaaRelaySettings {
        lock(&self.settings).clone()
    }

    /// Replace settings without restarting; the owner restarts the relay when appropriate.
    pub fn set_settings(&self, settings: NaaRelaySettings) {
        *lock(&self.settings) = settings.clone();
        if let Some(relay) = self.relay() {
            relay.set_settings(settings);
        }
    }

    pub fn set_output_timeouts(&self, timeouts: HqpOutputTimeouts) {
        *lock(&self.timeouts) = timeouts;
    }

    fn timeouts(&self) -> HqpOutputTimeouts {
        *lock(&self.timeouts)
    }

    fn relay(&self) -> Option<Arc<NaaRelay>> {
        lock(&self.relay).clone()
    }

    /// Publish the effective HQPlayer-zone metadata to the managed relay.
    pub fn set_metadata(&self, metadata: Option<super::frame::MetadataPayload>) {
        if let Some(relay) = self.relay() {
            relay.set_metadata(metadata);
        }
    }

    fn instance_name(&self) -> String {
        lock(&self.instance)
            .clone()
            .unwrap_or_else(|| "default".to_string())
    }

    // ------------------------------------------------------------------------------------------
    // Lifecycle (called by the manager through the adapter)
    // ------------------------------------------------------------------------------------------

    /// Construct the relay from settings, open the listener when enabled, and start the
    /// publisher. Errors opening the listener are recorded as unavailable, not returned: the
    /// instance still runs and the operator sees why routing is unavailable.
    pub async fn start(
        self: &Arc<Self>,
        instance: &str,
        worker: Option<HqpNativeWorker>,
        persist_path: Option<PathBuf>,
    ) {
        *lock(&self.instance) = Some(instance.to_string());
        *lock(&self.worker) = worker;
        if persist_path.is_some() {
            *lock(&self.persist_path) = persist_path;
        }
        {
            let mut ledger = lock(&self.ledger);
            ledger.lifecycle_stopped = None;
        }
        let settings = self.settings();
        let constructed: Result<Option<Arc<NaaRelay>>, String> = {
            let mut slot = lock(&self.relay);
            if slot.is_none() {
                let persist_path = lock(&self.persist_path).clone();
                match NaaRelay::new(settings.clone(), persist_path) {
                    Ok(relay) => {
                        *slot = Some(Arc::new(relay));
                        Ok(slot.clone())
                    }
                    Err(error) => Err(error),
                }
            } else {
                Ok(slot.clone())
            }
        };
        let relay = match constructed {
            Ok(relay) => relay,
            Err(error) => {
                self.record_error("RELAY_INIT", error);
                self.publish(None).await;
                return;
            }
        };
        if let Some(relay) = relay {
            relay.set_settings(settings.clone());
            if settings.enabled {
                if let Err(error) = relay.start_listener() {
                    tracing::warn!(instance, %error, "HQPlayer NAA relay listener could not start");
                }
            }
        }
        self.start_publisher().await;
        self.publish(None).await;
    }

    async fn start_publisher(self: &Arc<Self>) {
        // Reserve the decision in a short scope. The publisher task below contains awaits; keep
        // its construction outside the mutex guard so the lock lint also reflects the runtime
        // ownership boundary. A concurrent starter may race this check, so the final install
        // below re-checks the slot and aborts the losing task.
        let already_started = {
            let publisher = self.publisher.lock().await;
            publisher.is_some()
        };
        if already_started {
            return;
        }
        let shutdown = CancellationToken::new();
        // The task holds the coordinator weakly: it must never be the reason the coordinator (and
        // its relay guard) outlives the adapter that owns it.
        let me: std::sync::Weak<Self> = Arc::downgrade(self);
        let token = shutdown.clone();
        let join = tokio::spawn(async move {
            loop {
                let Some(coordinator) = me.upgrade() else {
                    break;
                };
                let relay = coordinator.relay();
                let changed = async {
                    match relay.as_ref() {
                        Some(relay) => relay.changed().await,
                        None => std::future::pending::<()>().await,
                    }
                };
                tokio::select! {
                    _ = token.cancelled() => break,
                    _ = changed => {}
                    // Byte counters advance without notifications; refresh them slowly.
                    _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                coordinator.publish(None).await;
                // Release the strong handle before sleeping/waiting again.
                drop(coordinator);
            }
        });
        let mut publisher = self.publisher.lock().await;
        if publisher.is_none() {
            *publisher = Some(Publisher { shutdown, join });
        } else {
            // Another starter installed its task first. Do not leave a duplicate publisher
            // running; dropping a JoinHandle would otherwise detach it permanently.
            join.abort();
            shutdown.cancel();
        }
    }

    /// Stop the relay with the instance lifecycle. Cancels pending work first, closes the pair
    /// and the listener, then publishes the retained observation as unavailable.
    pub async fn stop(&self, reason: &str) {
        self.supersede(&format!("superseded: {reason}"));
        // Take ownership of the join handle before awaiting it. Keeping the mutex guard alive
        // through `join.await` would block a concurrent lifecycle/drop path indefinitely.
        let publisher = {
            let mut slot = self.publisher.lock().await;
            slot.take()
        };
        if let Some(publisher) = publisher {
            publisher.shutdown.cancel();
            let _ = publisher.join.await;
        }
        if let Some(relay) = self.relay() {
            let report = relay.stop_listener();
            if report.workers_detached > 0 {
                tracing::warn!(
                    detached = report.workers_detached,
                    "HQPlayer NAA relay stopped with workers still inside bounded connect/DNS"
                );
            }
            if self.settings().enabled {
                lock(&self.ledger).lifecycle_stopped = Some((reason.to_string(), now()));
            }
        }
        self.publish(None).await;
    }

    /// Cancel any pending select and bump the relay generation so no continuation can act.
    pub fn supersede(&self, reason: &str) {
        if let Some((_, setup)) = lock(&self.pending_setup).take() {
            setup.cancel();
        }
        let pending = lock(&self.pending).take();
        if let Some(relay) = self.relay() {
            relay.bump_generation();
        }
        if let Some(pending) = pending {
            pending.token.cancel();
            self.finish_operation(
                &pending.operation_id,
                HqpOutputOutcome::Cancelled,
                Some(reason.to_string()),
                |_| {},
            );
        }
    }

    // ------------------------------------------------------------------------------------------
    // Projection
    // ------------------------------------------------------------------------------------------

    pub fn projection(&self) -> HqpOutputProjection {
        let instance = self.instance_name();
        let settings = self.settings();
        let observation = self.relay().map(|r| r.observe());
        let ledger = lock(&self.ledger);
        let (
            availability,
            relay_view,
            routes,
            selected,
            generation,
            desired,
            observed,
            session,
            discovery,
            dacs,
            relay_error,
        ) = match observation {
            Some(RelayObservation {
                relay,
                availability,
                routes,
                selected_route_id,
                generation,
                desired_destination,
                observed_forwarding_destination,
                session,
                discovery,
                dac_observations,
                last_error,
                ..
            }) => (
                availability,
                relay,
                routes,
                selected_route_id,
                generation,
                desired_destination,
                observed_forwarding_destination,
                session,
                discovery,
                dac_observations,
                last_error,
            ),
            None => (
                if settings.enabled {
                    HqpOutputAvailability::Unavailable {
                        reason: "relay not started".into(),
                        since: now(),
                    }
                } else {
                    HqpOutputAvailability::Disabled
                },
                super::outputs::HqpRelayConfigView {
                    enabled: settings.enabled,
                    adapter_name: settings.adapter_name.clone(),
                    virtual_device_id: super::outputs::VIRTUAL_DEVICE_ID.into(),
                    bind: Some(settings.bind.clone()),
                    hqp_allow: settings.hqp_allow.clone(),
                    discovery_interface: settings.discovery_interface.clone(),
                    discovery_port: settings.discovery_port,
                    discovery_responder: None,
                },
                vec![],
                None,
                0,
                None,
                None,
                None,
                None,
                vec![],
                None,
            ),
        };
        let availability = match (&ledger.lifecycle_stopped, availability) {
            (Some((reason, since)), HqpOutputAvailability::Disabled) => {
                HqpOutputAvailability::Unavailable {
                    reason: reason.clone(),
                    since: *since,
                }
            }
            (_, availability) => availability,
        };
        let last_error = ledger.last_error.clone().or_else(|| {
            relay_error.map(|message| HqpOutputError {
                code: "RELAY".into(),
                message,
                at: now(),
            })
        });
        HqpOutputProjection {
            zone_id: format!("hqplayer:{instance}"),
            instance,
            source_epoch: 0,
            aggregate_revision: 0,
            output_revision: ledger.output_revision,
            route_generation: generation,
            availability,
            relay: relay_view,
            routes,
            selected_route_id: selected,
            desired_destination: desired,
            observed_forwarding_destination: observed,
            session,
            discovery,
            dac_observations: dacs,
            native: ledger.native.clone(),
            current_operation_id: ledger.current_operation_id.clone(),
            operations: ledger.operations.iter().cloned().collect(),
            last_error,
            observed_at: now(),
        }
    }

    async fn publish(&self, caused_by: Option<CommandId>) {
        let worker = lock(&self.worker).clone();
        let Some(worker) = worker else {
            return;
        };
        let projection = self.projection();
        if let Err(error) = worker
            .sink
            .outputs_observed(&worker.instance_name, projection, caused_by)
            .await
        {
            tracing::warn!(%error, "HQPlayer output projection could not be committed");
        }
    }

    fn record_error(&self, code: &str, message: String) {
        lock(&self.ledger).last_error = Some(HqpOutputError {
            code: code.into(),
            message,
            at: now(),
        });
    }

    fn bump_revision(&self) -> u64 {
        let mut ledger = lock(&self.ledger);
        ledger.output_revision += 1;
        ledger.output_revision
    }

    // ------------------------------------------------------------------------------------------
    // Operations ledger
    // ------------------------------------------------------------------------------------------

    fn admit(
        &self,
        request: &HqpOutputCommandRequest,
        command_id: CommandId,
    ) -> HqpOutputOperation {
        let mut ledger = lock(&self.ledger);
        let operation = HqpOutputOperation {
            operation_id: format!("op-{}", command_id.get()),
            correlation_id: request.correlation_id.clone(),
            request_fingerprint: request.fingerprint(),
            zone_id: request.zone_id.clone(),
            action: request.action.name().to_string(),
            route_id: request.action.route_id().map(str::to_string),
            phase: HqpOutputPhase::Admitted,
            outcome: None,
            source_epoch: request.expected_source_epoch.unwrap_or(0),
            output_revision_at_admission: ledger.output_revision,
            route_generation: None,
            admitted_at: now(),
            updated_at: now(),
            detail: None,
            result: None,
            evidence: HqpOutputEvidence::default(),
        };
        ledger.operations.push_front(operation.clone());
        while ledger.operations.len() > OPERATION_HISTORY_LIMIT {
            ledger.operations.pop_back();
        }
        ledger.current_operation_id = Some(operation.operation_id.clone());
        operation
    }

    fn update_operation(
        &self,
        operation_id: &str,
        f: impl FnOnce(&mut HqpOutputOperation),
    ) -> Option<HqpOutputOperation> {
        let mut ledger = lock(&self.ledger);
        let operation = ledger
            .operations
            .iter_mut()
            .find(|o| o.operation_id == operation_id)?;
        if operation.outcome.is_some() {
            return Some(operation.clone());
        }
        f(operation);
        operation.updated_at = now();
        Some(operation.clone())
    }

    fn set_phase(&self, operation_id: &str, phase: HqpOutputPhase) {
        self.update_operation(operation_id, |o| o.phase = phase);
    }

    fn finish_operation(
        &self,
        operation_id: &str,
        outcome: HqpOutputOutcome,
        detail: Option<String>,
        f: impl FnOnce(&mut HqpOutputOperation),
    ) -> Option<HqpOutputOperation> {
        let finished = self.update_operation(operation_id, |o| {
            o.outcome = Some(outcome);
            o.phase = outcome.phase();
            if detail.is_some() {
                o.detail = detail;
            }
            f(o);
        });
        let mut ledger = lock(&self.ledger);
        if ledger.current_operation_id.as_deref() == Some(operation_id) {
            ledger.current_operation_id = None;
        }
        finished
    }

    pub fn operation(&self, operation_id: &str) -> Option<HqpOutputOperation> {
        lock(&self.ledger)
            .operations
            .iter()
            .find(|o| o.operation_id == operation_id)
            .cloned()
    }

    // ------------------------------------------------------------------------------------------
    // Command execution (endpoint thread; quick)
    // ------------------------------------------------------------------------------------------

    /// Execute one admitted command. Synchronous actions complete here; `select` may hand off to a
    /// continuation. The returned record is the state committed with `caused_by = command_id`.
    pub async fn execute(
        self: &Arc<Self>,
        adapter: Arc<HqpAdapter>,
        request: HqpOutputCommandRequest,
        command_id: CommandId,
    ) -> Result<HqpOutputOperation, HqpOutputRefusal> {
        // Stale fences are checked here, at execution, against the coordinator's own revision;
        // the surface already checked them against the committed projection.
        if request.action.requires_expectations() {
            let current_revision = lock(&self.ledger).output_revision;
            if request.expected_output_revision != Some(current_revision) {
                return Err(HqpOutputRefusal::StaleExpectation {
                    expected_source_epoch: request.expected_source_epoch,
                    expected_output_revision: request.expected_output_revision,
                    current_source_epoch: request.expected_source_epoch.unwrap_or(0),
                    current_output_revision: current_revision,
                });
            }
        }
        let action = request.action.clone();
        let requires_relay = !matches!(action, HqpOutputAction::RelayConfigure { .. });
        let relay = self.relay();
        if requires_relay {
            let Some(relay) = relay.as_ref() else {
                return Err(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                });
            };
            match (relay.availability(), &action) {
                // Route bookkeeping and read-only previews work while disabled; anything that
                // needs the listener does not.
                (
                    HqpOutputAvailability::Disabled,
                    HqpOutputAction::Select { .. } | HqpOutputAction::Discover,
                ) => return Err(HqpOutputRefusal::RelayDisabled),
                (
                    HqpOutputAvailability::Unavailable { reason, .. },
                    HqpOutputAction::Select { .. } | HqpOutputAction::Discover,
                ) => return Err(HqpOutputRefusal::RelayUnavailable { reason }),
                _ => {}
            }
        }
        let operation = self.admit(&request, command_id);
        let operation_id = operation.operation_id.clone();
        match action {
            HqpOutputAction::RouteAdd {
                name,
                host,
                port,
                device_id,
            } => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                match relay.add_route(&name, &host, port, device_id) {
                    Ok(route) => {
                        self.bump_revision();
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Complete,
                            None,
                            |o| {
                                o.route_id = Some(route.route_id.clone());
                                o.route_generation = Some(relay.generation());
                            },
                        );
                    }
                    Err(message) => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Rejected,
                            Some(message),
                            |_| {},
                        );
                    }
                }
            }
            HqpOutputAction::RouteUpdate {
                route_id,
                name,
                host,
                port,
                device_id,
            } => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                // Editing the selected route tears down its session: supersede first.
                if relay.selected_route_id().as_deref() == Some(route_id.as_str()) {
                    self.supersede("route edited");
                }
                match relay.update_route(&route_id, &name, &host, port, device_id) {
                    Ok(_) => {
                        self.bump_revision();
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Complete,
                            None,
                            |o| {
                                o.route_generation = Some(relay.generation());
                            },
                        );
                    }
                    Err(message) => {
                        let outcome = if message.contains("unknown") {
                            HqpOutputOutcome::Rejected
                        } else {
                            HqpOutputOutcome::Failed
                        };
                        self.finish_operation(&operation_id, outcome, Some(message), |_| {});
                    }
                }
            }
            HqpOutputAction::RouteRemove { route_id } => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                if relay.selected_route_id().as_deref() == Some(route_id.as_str()) {
                    self.supersede("route removed");
                }
                match relay.remove_route(&route_id) {
                    Ok(()) => {
                        self.bump_revision();
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Complete,
                            None,
                            |o| {
                                o.route_generation = Some(relay.generation());
                            },
                        );
                    }
                    Err(message) => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Rejected,
                            Some(message),
                            |_| {},
                        );
                    }
                }
            }
            HqpOutputAction::ImportPreview { routes_json } => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                match import_preview(&routes_json, &relay.routes()) {
                    Ok(preview) => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Complete,
                            None,
                            |o| {
                                o.result = Some(HqpOutputResult::ImportPreview(preview));
                            },
                        );
                    }
                    Err(message) => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Rejected,
                            Some(message),
                            |_| {},
                        );
                    }
                }
            }
            HqpOutputAction::ImportApply {
                routes_json,
                preview_id,
            } => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                match import_preview(&routes_json, &relay.routes()) {
                    Ok(preview) if preview.preview_id != preview_id => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Rejected,
                            Some("preview_id does not match this routes_json; run import_preview again".into()),
                            |_| {},
                        );
                    }
                    Ok(preview) => {
                        let added = preview.routes.clone();
                        match relay.add_imported_routes(added.clone()) {
                            Ok(()) => {
                                self.bump_revision();
                                self.finish_operation(
                                    &operation_id,
                                    HqpOutputOutcome::Complete,
                                    None,
                                    |o| {
                                        o.route_generation = Some(relay.generation());
                                        o.result = Some(HqpOutputResult::ImportApplied {
                                            added,
                                            skipped: preview.conflicts.clone(),
                                            ignored_selected_route_id: preview
                                                .ignored_selected_route_id
                                                .clone(),
                                        });
                                    },
                                );
                            }
                            Err(message) => {
                                self.finish_operation(
                                    &operation_id,
                                    HqpOutputOutcome::Failed,
                                    Some(message),
                                    |_| {},
                                );
                            }
                        }
                    }
                    Err(message) => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Rejected,
                            Some(message),
                            |_| {},
                        );
                    }
                }
            }
            HqpOutputAction::RelayConfigure {
                enabled,
                bind,
                hqp_allow,
                discovery_interface,
                discovery_port,
                adapter_name,
            } => {
                let mut settings = self.settings();
                if bind.is_none() && settings == NaaRelaySettings::default() {
                    settings.bind = format!(
                        "{}:0",
                        discovery_interface.as_deref().unwrap_or("127.0.0.1")
                    );
                    settings.adapter_name = format!("UHC {}", self.instance_name());
                }
                settings.enabled = enabled;
                if let Some(port) = discovery_port {
                    settings.discovery_port = port;
                }
                if let Some(bind) = bind {
                    settings.bind = bind;
                }
                settings.hqp_allow = hqp_allow;
                settings.discovery_interface = discovery_interface;
                if let Some(name) = adapter_name.filter(|n| !n.trim().is_empty()) {
                    settings.adapter_name = name;
                }
                if let Err(message) = validate_settings(&settings) {
                    self.finish_operation(
                        &operation_id,
                        HqpOutputOutcome::Rejected,
                        Some(message),
                        |_| {},
                    );
                } else {
                    self.supersede("relay reconfigured");
                    *lock(&self.settings) = settings.clone();
                    if let Some(relay) = relay.as_ref() {
                        relay.stop_listener();
                        relay.set_settings(settings.clone());
                        lock(&self.ledger).lifecycle_stopped = None;
                        if settings.enabled {
                            if let Err(error) = relay.start_listener() {
                                self.record_error("RELAY_START", error);
                            }
                        }
                    } else {
                        let instance = self.instance_name();
                        let worker = lock(&self.worker).clone();
                        self.start(&instance, worker, None).await;
                    }
                    // Persist the allocated port, so automatic setup remains stable on restart.
                    if let Some(active) = self.relay() {
                        if let Some(address) = active.listener_addr() {
                            settings.bind = address.to_string();
                            active.set_settings(settings.clone());
                            *lock(&self.settings) = settings.clone();
                        }
                    }
                    self.bump_revision();
                    // Persist through the adapter's instance configuration. A live-but-not-durable
                    // setting is a partial outcome, never a durable success.
                    match adapter.persist_output_relay_settings(settings).await {
                        Ok(()) => {
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Complete,
                                None,
                                |_| {},
                            );
                        }
                        Err(error) => {
                            self.record_error("PERSIST", error.to_string());
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Partial,
                                Some(format!("relay settings applied for this process but could not be persisted: {error}")),
                                |_| {},
                            );
                        }
                    }
                }
            }
            HqpOutputAction::Discover => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                let interface = self
                    .settings()
                    .discovery_interface
                    .as_deref()
                    .map(str::parse::<Ipv4Addr>);
                match interface {
                    None => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Rejected,
                            Some("discovery needs relay_configure with an explicit discovery_interface; manual destinations remain available".into()),
                            |_| {},
                        );
                    }
                    Some(Err(e)) => {
                        self.finish_operation(
                            &operation_id,
                            HqpOutputOutcome::Rejected,
                            Some(format!("invalid discovery interface: {e}")),
                            |_| {},
                        );
                    }
                    Some(Ok(interface)) => {
                        let own = discovery::own_addresses(relay.listener_addr());
                        let me = Arc::clone(self);
                        let op = operation_id.clone();
                        let relay = Arc::clone(&relay);
                        me.set_phase(&op, HqpOutputPhase::Checking);
                        tokio::spawn(async move {
                            let port = me.settings().discovery_port;
                            let scanned = tokio::task::spawn_blocking(move || {
                                discovery::scan(interface, port, &own)
                            })
                            .await;
                            match scanned {
                                Ok(Ok(observation)) => {
                                    relay.record_discovery(observation.clone());
                                    me.finish_operation(
                                        &op,
                                        HqpOutputOutcome::Complete,
                                        None,
                                        |o| {
                                            o.result =
                                                Some(HqpOutputResult::Discovery(observation));
                                        },
                                    );
                                }
                                Ok(Err(message)) => {
                                    me.finish_operation(
                                        &op,
                                        HqpOutputOutcome::Failed,
                                        Some(message),
                                        |_| {},
                                    );
                                }
                                Err(_) => {
                                    me.finish_operation(
                                        &op,
                                        HqpOutputOutcome::Failed,
                                        Some("discovery task aborted".into()),
                                        |_| {},
                                    );
                                }
                            }
                            me.publish(None).await;
                        });
                    }
                }
            }
            HqpOutputAction::Stop => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                // 1. Local halt with priority: cancel pending, close the pair, clear selection.
                self.supersede("superseded by Stop");
                let outcome = relay.clear_selection();
                self.bump_revision();
                self.update_operation(&operation_id, |o| {
                    o.phase = HqpOutputPhase::Stopping;
                    o.route_generation = Some(outcome.generation);
                });
                if let Some(persist_error) = outcome.persist_error.clone() {
                    self.record_error("PERSIST", persist_error);
                }
                // The admission commit carries the command id so the caller's receipt shows the
                // local halt already happened.
                self.publish(Some(command_id)).await;
                // 2. Bounded native Stop when a session existed (HQPlayer was playing through us)
                //    or the transport is reachable at all.
                let native_needed = outcome.had_session || adapter.output_native_reachable().await;
                if native_needed {
                    // The whole hook, lease wait included, is bounded from here.
                    let deadline = tokio::time::Instant::now() + self.timeouts().control_step;
                    let fence = NativeHookFence::unconditional();
                    match adapter.output_hook_stop_verified(&fence, deadline).await {
                        Ok(NativeHookOutcome::Done(())) => {
                            lock(&self.ledger).native.transport_state = Some("0".into());
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Complete,
                                None,
                                |o| {
                                    o.evidence.native_stop_verified = Some(true);
                                    o.evidence.native_state_after = Some("0".into());
                                },
                            );
                        }
                        Ok(NativeHookOutcome::NotAttempted(reason)) => {
                            lock(&self.ledger).native.transport_state = None;
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Partial,
                                Some(format!("Audio routing stopped locally, but HQPlayer transport Stop was not attempted: {reason}")),
                                |o| o.evidence.native_stop_verified = Some(false),
                            );
                        }
                        Ok(NativeHookOutcome::Indeterminate(reason)) => {
                            lock(&self.ledger).native.transport_state = None;
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Indeterminate,
                                Some(format!("Audio routing stopped locally; HQPlayer transport Stop is indeterminate: {reason}")),
                                |o| o.evidence.native_stop_verified = Some(false),
                            );
                        }
                        Err(error) => {
                            lock(&self.ledger).native.transport_state = None;
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Partial,
                                Some(format!("Audio routing stopped locally, but HQPlayer transport Stop failed: {error}")),
                                |o| o.evidence.native_stop_verified = Some(false),
                            );
                        }
                    }
                } else {
                    self.finish_operation(&operation_id, HqpOutputOutcome::Complete, None, |_| {});
                }
            }
            HqpOutputAction::Select { route_id } => {
                let relay = relay.ok_or(HqpOutputRefusal::RelayUnavailable {
                    reason: "relay not started".into(),
                })?;
                if relay.route(&route_id).is_none() {
                    // Never admitted as an operation: nothing to look up, nothing changed.
                    lock(&self.ledger)
                        .operations
                        .retain(|o| o.operation_id != operation_id);
                    return Err(HqpOutputRefusal::UnknownRoute { route_id });
                }
                // Supersede any pending selection first; its sockets/native waits are cancelled.
                self.supersede("superseded by a newer selection");
                let token = CancellationToken::new();
                *lock(&self.pending) = Some(Pending {
                    token: token.clone(),
                    operation_id: operation_id.clone(),
                });
                let session_exists = relay.observe().session.is_some();
                if !session_exists {
                    // Nothing can be playing through the relay: commit without native traffic.
                    match relay.commit_selection(&route_id) {
                        Ok(generation) => {
                            self.bump_revision();
                            *lock(&self.pending) = None;
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Complete,
                                None,
                                |o| {
                                    o.route_generation = Some(generation);
                                },
                            );
                        }
                        Err(message) => {
                            *lock(&self.pending) = None;
                            self.finish_operation(
                                &operation_id,
                                HqpOutputOutcome::Failed,
                                Some(message),
                                |_| {},
                            );
                        }
                    }
                } else {
                    self.set_phase(&operation_id, HqpOutputPhase::Checking);
                    let me = Arc::clone(self);
                    let op = operation_id.clone();
                    tokio::spawn(async move {
                        me.select_continuation(adapter, relay, token, op, route_id)
                            .await;
                    });
                }
            }
            HqpOutputAction::SetupPreview => {
                self.set_phase(&operation_id, HqpOutputPhase::Checking);
                let me = Arc::clone(self);
                let op = operation_id.clone();
                let settings = self.settings();
                tokio::spawn(async move {
                    let outcome = async {
                        let html = adapter.output_setup_fetch_form().await?;
                        let form = setup::parse_form(&html).map_err(|e| anyhow::anyhow!(e))?;
                        let backup = adapter.output_setup_read_backup().await?;
                        Ok::<_, anyhow::Error>((form, backup))
                    }
                    .await;
                    match outcome {
                        Ok((form, backup)) => {
                            let (preview, proposed) = setup::derive(
                                &form,
                                &backup,
                                &settings.adapter_name,
                                VIRTUAL_DEVICE_ID,
                            );
                            lock(&me.ledger).setup = Some(SetupSession {
                                preview: preview.clone(),
                                baseline_fingerprint: form.fingerprint(),
                                baseline_fields: form.fields.clone(),
                                proposed_fields: proposed,
                                backup,
                                applied: false,
                            });
                            me.finish_operation(&op, HqpOutputOutcome::Complete, None, |o| {
                                o.result = Some(HqpOutputResult::SetupPreview(preview));
                            });
                        }
                        Err(error) => {
                            me.finish_operation(
                                &op,
                                HqpOutputOutcome::Failed,
                                Some(format!("could not read HQPlayer's configuration: {error}")),
                                |_| {},
                            );
                        }
                    }
                    me.publish(None).await;
                });
            }
            HqpOutputAction::SetupApply { preview_id } => {
                let session = lock(&self.ledger).setup.clone();
                let Some(session) = session.filter(|s| s.preview.preview_id == preview_id) else {
                    self.finish_operation(
                        &operation_id,
                        HqpOutputOutcome::Rejected,
                        Some("preview_id does not name the current setup preview; run setup_preview again".into()),
                        |_| {},
                    );
                    self.publish(Some(command_id)).await;
                    return Ok(self.operation(&operation_id).unwrap_or(operation));
                };
                if !session.preview.applicable {
                    self.finish_operation(
                        &operation_id,
                        HqpOutputOutcome::Rejected,
                        session.preview.blocker.clone(),
                        |_| {},
                    );
                    self.publish(Some(command_id)).await;
                    return Ok(self.operation(&operation_id).unwrap_or(operation));
                }
                self.set_phase(&operation_id, HqpOutputPhase::Checking);
                let (generation, token) = self.arm_setup_token();
                let me = Arc::clone(self);
                let op = operation_id.clone();
                let instance = self.instance_name();
                tokio::spawn(async move {
                    me.setup_apply(adapter, &instance, session, generation, token, &op)
                        .await;
                    me.publish(None).await;
                });
            }
            HqpOutputAction::SetupReadback => {
                self.set_phase(&operation_id, HqpOutputPhase::Checking);
                let me = Arc::clone(self);
                let op = operation_id.clone();
                tokio::spawn(async move {
                    let session = lock(&me.ledger).setup.clone();
                    let rollback_available = session.as_ref().is_some_and(|s| s.applied);
                    let expected = session.as_ref().map(|s| me.expected_after(s, s.applied));
                    let (runtime, disk, sha) = me.read_setup_state(&adapter).await;
                    match (runtime, disk) {
                        (Ok(runtime), Ok(disk)) => {
                            let (runtime_matches, disk_matches) = match &expected {
                                Some(expected) => (
                                    Some(expected.runtime_matches(&runtime)),
                                    Some(expected.disk_matches(&disk)),
                                ),
                                None => (None, None),
                            };
                            let readback_matches = match (runtime_matches, disk_matches) {
                                (Some(a), Some(b)) => Some(a && b),
                                _ => None,
                            };
                            me.finish_operation(&op, HqpOutputOutcome::Complete, None, |o| {
                                o.result = Some(HqpOutputResult::Setup(HqpSetupTransaction {
                                    step: "readback".into(),
                                    uploaded: false,
                                    daemon_response: None,
                                    runtime_matches,
                                    disk_matches,
                                    readback_matches,
                                    readback_sha256: sha,
                                    settled_after_ms: None,
                                    rollback_available,
                                }));
                            });
                        }
                        (Err(error), _) | (_, Err(error)) => {
                            me.finish_operation(
                                &op,
                                HqpOutputOutcome::Failed,
                                Some(format!("could not read HQPlayer's configuration: {error}")),
                                |_| {},
                            );
                        }
                    }
                    me.publish(None).await;
                });
            }
            HqpOutputAction::SetupRollback => {
                let instance = self.instance_name();
                let session = lock(&self.ledger).setup.clone();
                let material = match session.as_ref().filter(|s| s.applied) {
                    Some(s) => Some((s.baseline_fields.clone(), s.backup.clone())),
                    None => Self::read_rollback_files(&instance),
                };
                let Some((baseline_fields, backup)) = material else {
                    self.finish_operation(
                        &operation_id,
                        HqpOutputOutcome::Rejected,
                        Some("no pre-apply state is retained for this instance; nothing to roll back".into()),
                        |_| {},
                    );
                    self.publish(Some(command_id)).await;
                    return Ok(self.operation(&operation_id).unwrap_or(operation));
                };
                self.set_phase(&operation_id, HqpOutputPhase::Checking);
                let (generation, token) = self.arm_setup_token();
                let me = Arc::clone(self);
                let op = operation_id.clone();
                tokio::spawn(async move {
                    me.setup_rollback(adapter, baseline_fields, backup, generation, token, &op)
                        .await;
                    me.publish(None).await;
                });
            }
        }
        // Commit the admitted/terminal record under the command id.
        self.publish(Some(command_id)).await;
        Ok(self.operation(&operation_id).unwrap_or(operation))
    }

    // ------------------------------------------------------------------------------------------
    // One-time setup transaction (existing web credential owner; bounded async application)
    // ------------------------------------------------------------------------------------------

    fn setup_file_path(instance: &str, suffix: &str) -> PathBuf {
        let digest = setup::sha256_hex(instance.as_bytes());
        crate::config::get_config_file_path(&format!(
            "hqplayer-naa-setup-{}-{suffix}",
            &digest[..24]
        ))
    }

    fn read_rollback_files(instance: &str) -> Option<RollbackFiles> {
        let backup = std::fs::read(Self::setup_file_path(instance, "backup.xml")).ok()?;
        let material: SetupRollbackFile = serde_json::from_slice(
            &std::fs::read(Self::setup_file_path(instance, "form.json")).ok()?,
        )
        .ok()?;
        if material.backup_sha256 != setup::sha256_hex(&backup) {
            return None;
        }
        Some((material.baseline_fields, backup))
    }

    /// Arm a new setup transaction: a fresh generation and token. Any previous transaction is
    /// cancelled; its later cleanup cannot touch this one because generations differ.
    fn arm_setup_token(&self) -> (u64, CancellationToken) {
        let generation = self
            .setup_generation
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel)
            + 1;
        let token = CancellationToken::new();
        if let Some((_, previous)) = lock(&self.pending_setup).replace((generation, token.clone()))
        {
            previous.cancel();
        }
        (generation, token)
    }

    /// Clear the pending transaction only if it is still the one identified by `generation`.
    fn clear_setup_token(&self, generation: u64) {
        let mut pending = lock(&self.pending_setup);
        if pending.as_ref().is_some_and(|(g, _)| *g == generation) {
            *pending = None;
        }
    }

    /// Test seam: the generation of the pending setup transaction, if any.
    #[doc(hidden)]
    pub fn pending_setup_generation_for_tests(&self) -> Option<u64> {
        lock(&self.pending_setup).as_ref().map(|(g, _)| *g)
    }

    /// A fence that fails once this transaction is no longer the pending one (superseded) or
    /// cancelled, checked by the adapter with the lease held immediately before every post.
    fn setup_fence(
        self: &Arc<Self>,
        generation: u64,
        token: &CancellationToken,
    ) -> NativeHookFence {
        let me = Arc::downgrade(self);
        NativeHookFence {
            token: token.clone(),
            still_current: Arc::new(move || {
                me.upgrade().is_some_and(|c| {
                    lock(&c.pending_setup).as_ref().map(|(g, _)| *g) == Some(generation)
                })
            }),
        }
    }

    /// What the running form and the persistent configuration must report after a step.
    fn expected_after(&self, session: &SetupSession, applied: bool) -> SetupExpectation {
        let settings = self.settings();
        if applied {
            SetupExpectation {
                backend: setup::BACKEND_NETWORK.to_string(),
                net_device: Some(setup::relay_option(
                    &settings.adapter_name,
                    VIRTUAL_DEVICE_ID,
                )),
                disk: SetupDiskExpectation::Identity {
                    address: settings.adapter_name,
                    device: VIRTUAL_DEVICE_ID.to_string(),
                },
            }
        } else {
            let value = |name: &str| {
                session
                    .baseline_fields
                    .iter()
                    .find(|(n, _)| n == name)
                    .map(|(_, v)| v.clone())
            };
            SetupExpectation {
                backend: value("backend").unwrap_or_default(),
                net_device: value("net_device"),
                disk: SetupDiskExpectation::Bytes(session.backup.clone()),
            }
        }
    }

    /// Read the running form and the persistent configuration once.
    async fn read_setup_state(
        &self,
        adapter: &Arc<HqpAdapter>,
    ) -> (
        Result<setup::ConfigForm, String>,
        Result<Vec<u8>, String>,
        Option<String>,
    ) {
        let runtime = match adapter.output_setup_fetch_form().await {
            Ok(html) => setup::parse_form(&html),
            Err(error) => Err(error.to_string()),
        };
        let disk = adapter
            .output_setup_read_backup()
            .await
            .map_err(|e| e.to_string());
        let sha = disk.as_ref().ok().map(|b| setup::sha256_hex(b));
        (runtime, disk, sha)
    }

    /// Poll the running form and the persistent configuration until both match or the settle
    /// budget lapses. Application is asynchronous on the daemon; the post's HTTP 200 proves only
    /// receipt.
    async fn settle(
        &self,
        adapter: &Arc<HqpAdapter>,
        expected: &SetupExpectation,
        deadline: tokio::time::Instant,
        poll: Duration,
    ) -> (Option<bool>, Option<bool>, Option<String>) {
        let mut runtime_ok = None;
        let mut disk_ok = None;
        let mut sha = None;
        loop {
            // The daemon may reload; give the native lane a chance to come back first.
            adapter.output_setup_native_ready(deadline).await;
            let (runtime, disk, latest_sha) = self.read_setup_state(adapter).await;
            if let Ok(form) = &runtime {
                runtime_ok = Some(expected.runtime_matches(form));
            }
            if let Ok(bytes) = &disk {
                disk_ok = Some(expected.disk_matches(bytes));
            }
            if latest_sha.is_some() {
                sha = latest_sha;
            }
            if runtime_ok == Some(true) && disk_ok == Some(true) {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(poll).await;
        }
        (runtime_ok, disk_ok, sha)
    }

    /// Poll only the running form until it reflects the requested selection.  This is used
    /// before `/restore`: Embedded applies `/config` asynchronously, so restoring disk bytes
    /// while that write is still queued can have the later form write overwrite the rollback.
    async fn settle_runtime(
        &self,
        adapter: &Arc<HqpAdapter>,
        expected: &SetupExpectation,
        deadline: tokio::time::Instant,
        poll: Duration,
    ) -> Option<bool> {
        loop {
            adapter.output_setup_native_ready(deadline).await;
            if let Ok(html) = adapter.output_setup_fetch_form().await {
                if let Ok(form) = setup::parse_form(&html) {
                    if expected.runtime_matches(&form) {
                        return Some(true);
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Some(false);
            }
            tokio::time::sleep(poll).await;
        }
    }

    async fn setup_apply(
        self: &Arc<Self>,
        adapter: Arc<HqpAdapter>,
        instance: &str,
        session: SetupSession,
        generation: u64,
        token: CancellationToken,
        op: &str,
    ) {
        let started = tokio::time::Instant::now();
        // Retain the pre-apply state durably before touching HQPlayer.
        let material = SetupRollbackFile {
            baseline_fields: session.baseline_fields.clone(),
            backup_sha256: setup::sha256_hex(&session.backup),
        };
        let persisted = super::super::secure_atomic_write(
            &Self::setup_file_path(instance, "backup.xml"),
            &session.backup,
        )
        .and_then(|()| {
            super::super::secure_atomic_write(
                &Self::setup_file_path(instance, "form.json"),
                &serde_json::to_vec(&material)?,
            )
        });
        if let Err(error) = persisted {
            self.clear_setup_token(generation);
            self.finish_operation(op, HqpOutputOutcome::Failed, Some(format!("could not retain the pre-apply state; refusing to apply without a rollback: {error}")), |_| {});
            return;
        }
        let (settle_budget, poll) = adapter.output_setup_settle().await;
        let fence = self.setup_fence(generation, &token);
        let post_deadline = tokio::time::Instant::now() + adapter.profile_timeouts().await.request;
        let posted = adapter
            .output_setup_apply_form(
                &session.baseline_fingerprint,
                &session.proposed_fields,
                &fence,
                post_deadline,
            )
            .await;
        let response = match posted {
            Ok(NativeHookOutcome::Done(response)) => response,
            Ok(NativeHookOutcome::NotAttempted(reason)) => {
                self.clear_setup_token(generation);
                let outcome = if token.is_cancelled() {
                    HqpOutputOutcome::Cancelled
                } else {
                    HqpOutputOutcome::Rejected
                };
                self.finish_operation(
                    op,
                    outcome,
                    Some(format!("apply not attempted: {reason}")),
                    |o| {
                        o.result = Some(Self::setup_result(
                            "apply", false, None, None, None, None, None, false,
                        ));
                    },
                );
                return;
            }
            Ok(NativeHookOutcome::Indeterminate(reason)) => {
                self.mark_setup_applied();
                self.clear_setup_token(generation);
                self.finish_operation(op, HqpOutputOutcome::Indeterminate, Some(format!("apply: {reason}; verify HQPlayer's output configuration before relying on it")), |o| {
                    o.result = Some(Self::setup_result("apply", true, None, None, None, None, None, true));
                });
                return;
            }
            Err(error) => {
                self.clear_setup_token(generation);
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Failed,
                    Some(format!("apply failed: {error}")),
                    |o| {
                        o.result = Some(Self::setup_result(
                            "apply", false, None, None, None, None, None, false,
                        ));
                    },
                );
                return;
            }
        };
        self.mark_setup_applied();
        self.set_phase(op, HqpOutputPhase::Resuming);
        self.publish(None).await;
        let expected = self.expected_after(&session, true);
        let deadline = tokio::time::Instant::now() + settle_budget;
        let (runtime_ok, disk_ok, sha) = self.settle(&adapter, &expected, deadline, poll).await;
        self.clear_setup_token(generation);
        let settled_after_ms = started.elapsed().as_millis() as u64;
        let both = runtime_ok == Some(true) && disk_ok == Some(true);
        let (outcome, detail) = if both {
            (HqpOutputOutcome::Complete, None)
        } else {
            (
                HqpOutputOutcome::Indeterminate,
                Some(format!(
                    "apply: HQPlayer accepted the form but its readback did not confirm within {settle_budget:?} (running form matches: {runtime_ok:?}, persistent configuration matches: {disk_ok:?}); verify the running output before relying on it"
                )),
            )
        };
        let result = Self::setup_result(
            "apply",
            true,
            Some(response),
            runtime_ok,
            disk_ok,
            sha,
            Some(settled_after_ms),
            true,
        );
        self.finish_operation(op, outcome, detail, |o| {
            o.result = Some(result);
        });
    }

    async fn setup_rollback(
        self: &Arc<Self>,
        adapter: Arc<HqpAdapter>,
        baseline_fields: Vec<(String, String)>,
        backup: Vec<u8>,
        generation: u64,
        token: CancellationToken,
        op: &str,
    ) {
        let started = tokio::time::Instant::now();
        let (settle_budget, poll) = adapter.output_setup_settle().await;
        let request_budget = adapter.profile_timeouts().await.request;
        let fence = self.setup_fence(generation, &token);
        // 1. Running selection first: re-post the previously read form. The fingerprint fence is
        //    the form as it is right now, so a concurrent operator change is refused, not
        //    overwritten.
        let current_fingerprint = match adapter.output_setup_fetch_form().await {
            Ok(html) => match setup::parse_form(&html) {
                Ok(form) => form.fingerprint(),
                Err(error) => {
                    self.clear_setup_token(generation);
                    self.finish_operation(
                        op,
                        HqpOutputOutcome::Failed,
                        Some(format!("rollback: {error}")),
                        |_| {},
                    );
                    return;
                }
            },
            Err(error) => {
                self.clear_setup_token(generation);
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Failed,
                    Some(format!(
                        "rollback: could not read the running form: {error}"
                    )),
                    |_| {},
                );
                return;
            }
        };
        let posted = adapter
            .output_setup_apply_form(
                &current_fingerprint,
                &baseline_fields,
                &fence,
                tokio::time::Instant::now() + request_budget,
            )
            .await;
        let response = match posted {
            Ok(NativeHookOutcome::Done(response)) => response,
            Ok(NativeHookOutcome::NotAttempted(reason)) => {
                self.clear_setup_token(generation);
                let outcome = if token.is_cancelled() {
                    HqpOutputOutcome::Cancelled
                } else {
                    HqpOutputOutcome::Rejected
                };
                self.finish_operation(
                    op,
                    outcome,
                    Some(format!("rollback not attempted: {reason}")),
                    |o| {
                        o.result = Some(Self::setup_result(
                            "rollback", false, None, None, None, None, None, true,
                        ));
                    },
                );
                return;
            }
            Ok(NativeHookOutcome::Indeterminate(reason)) => {
                self.clear_setup_token(generation);
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Indeterminate,
                    Some(format!("rollback: {reason}")),
                    |o| {
                        o.result = Some(Self::setup_result(
                            "rollback", true, None, None, None, None, None, true,
                        ));
                    },
                );
                return;
            }
            Err(error) => {
                self.clear_setup_token(generation);
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Failed,
                    Some(format!("rollback failed: {error}")),
                    |o| {
                        o.result = Some(Self::setup_result(
                            "rollback", false, None, None, None, None, None, true,
                        ));
                    },
                );
                return;
            }
        };
        // 2. Wait until Embedded has actually applied the running form. `/config` is
        // asynchronous; posting `/restore` immediately can otherwise be overwritten by the
        // still-pending form write. Do not touch persistent bytes when the running side never
        // settled, because that would make rollback less safe than leaving it alone.
        let runtime_expected = SetupExpectation {
            backend: baseline_fields
                .iter()
                .find(|(n, _)| n == "backend")
                .map(|(_, v)| v.clone())
                .unwrap_or_default(),
            net_device: baseline_fields
                .iter()
                .find(|(n, _)| n == "net_device")
                .map(|(_, v)| v.clone()),
            disk: SetupDiskExpectation::Bytes(Vec::new()),
        };
        let runtime_deadline = tokio::time::Instant::now() + settle_budget;
        let runtime_settled = self
            .settle_runtime(&adapter, &runtime_expected, runtime_deadline, poll)
            .await;
        if runtime_settled != Some(true) {
            self.clear_setup_token(generation);
            self.finish_operation(op, HqpOutputOutcome::Indeterminate, Some("rollback: running form was accepted but did not settle before persistent restore; verify before retrying".into()), |o| {
                o.result = Some(Self::setup_result("rollback", true, Some(response), None, None, None, Some(started.elapsed().as_millis() as u64), true));
            });
            return;
        }
        // 3. Persistent bytes second: /restore rewrites disk only.
        let restored = adapter
            .output_setup_restore_disk(
                &backup,
                &fence,
                tokio::time::Instant::now() + request_budget,
            )
            .await;
        let disk_response = match restored {
            Ok(NativeHookOutcome::Done(r)) => Some(r),
            Ok(NativeHookOutcome::NotAttempted(reason)) => {
                self.clear_setup_token(generation);
                self.finish_operation(op, HqpOutputOutcome::Partial, Some(format!("rollback: running form restored, persistent configuration not restored: {reason}")), |o| {
                    o.result = Some(Self::setup_result("rollback", true, Some(response.clone()), None, Some(false), None, None, true));
                });
                return;
            }
            Ok(NativeHookOutcome::Indeterminate(reason)) => {
                self.clear_setup_token(generation);
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Indeterminate,
                    Some(format!("rollback: {reason}")),
                    |o| {
                        o.result = Some(Self::setup_result(
                            "rollback",
                            true,
                            Some(response.clone()),
                            None,
                            None,
                            None,
                            None,
                            true,
                        ));
                    },
                );
                return;
            }
            Err(error) => {
                self.clear_setup_token(generation);
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Partial,
                    Some(format!(
                        "rollback: running form restored, persistent restore failed: {error}"
                    )),
                    |o| {
                        o.result = Some(Self::setup_result(
                            "rollback",
                            true,
                            Some(response.clone()),
                            None,
                            Some(false),
                            None,
                            None,
                            true,
                        ));
                    },
                );
                return;
            }
        };
        self.set_phase(op, HqpOutputPhase::Resuming);
        self.publish(None).await;
        let value = |name: &str| {
            baseline_fields
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, v)| v.clone())
        };
        let expected = SetupExpectation {
            backend: value("backend").unwrap_or_default(),
            net_device: value("net_device"),
            disk: SetupDiskExpectation::Bytes(backup.clone()),
        };
        let deadline = tokio::time::Instant::now() + settle_budget;
        let (runtime_ok, disk_ok, sha) = self.settle(&adapter, &expected, deadline, poll).await;
        self.clear_setup_token(generation);
        let both = runtime_ok == Some(true) && disk_ok == Some(true);
        if both {
            if let Some(session) = lock(&self.ledger).setup.as_mut() {
                session.applied = false;
            }
        }
        let rollback_available = !both;
        let (outcome, detail) = if both {
            (HqpOutputOutcome::Complete, None)
        } else {
            (
                HqpOutputOutcome::Indeterminate,
                Some(format!(
                    "rollback: readback did not confirm within {settle_budget:?} (running form matches: {runtime_ok:?}, persistent configuration matches: {disk_ok:?})"
                )),
            )
        };
        let result = Self::setup_result(
            "rollback",
            true,
            Some(format!(
                "{response}; restore {}",
                disk_response.unwrap_or_default()
            )),
            runtime_ok,
            disk_ok,
            sha,
            Some(started.elapsed().as_millis() as u64),
            rollback_available,
        );
        self.finish_operation(op, outcome, detail, |o| {
            o.result = Some(result);
        });
    }

    fn mark_setup_applied(&self) {
        if let Some(session) = lock(&self.ledger).setup.as_mut() {
            session.applied = true;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn setup_result(
        step: &str,
        uploaded: bool,
        daemon_response: Option<String>,
        runtime_matches: Option<bool>,
        disk_matches: Option<bool>,
        readback_sha256: Option<String>,
        settled_after_ms: Option<u64>,
        rollback_available: bool,
    ) -> HqpOutputResult {
        let readback_matches = match (runtime_matches, disk_matches) {
            (Some(a), Some(b)) => Some(a && b),
            _ => None,
        };
        HqpOutputResult::Setup(HqpSetupTransaction {
            step: step.into(),
            uploaded,
            daemon_response,
            runtime_matches,
            disk_matches,
            readback_matches,
            readback_sha256,
            settled_after_ms,
            rollback_available,
        })
    }

    // ------------------------------------------------------------------------------------------
    // Select continuation (background task; every await cancellable)
    // ------------------------------------------------------------------------------------------

    async fn select_continuation(
        self: Arc<Self>,
        adapter: Arc<HqpAdapter>,
        relay: Arc<NaaRelay>,
        token: CancellationToken,
        operation_id: String,
        route_id: String,
    ) {
        let outcome = self
            .select_sequence(&adapter, &relay, &token, &operation_id, &route_id)
            .await;
        if let Err(SelectAbort::Cancelled) = outcome {
            self.finish_operation(
                &operation_id,
                HqpOutputOutcome::Cancelled,
                Some("selection superseded by Stop or a newer request".into()),
                |_| {},
            );
        }
        {
            let mut pending = lock(&self.pending);
            if pending
                .as_ref()
                .is_some_and(|p| p.operation_id == operation_id)
            {
                *pending = None;
            }
        }
        self.publish(None).await;
    }

    fn fence_for(
        relay: &Arc<NaaRelay>,
        token: &CancellationToken,
        generation: u64,
    ) -> NativeHookFence {
        let relay = Arc::clone(relay);
        NativeHookFence {
            token: token.clone(),
            still_current: Arc::new(move || relay.generation() == generation),
        }
    }

    async fn select_sequence(
        &self,
        adapter: &Arc<HqpAdapter>,
        relay: &Arc<NaaRelay>,
        token: &CancellationToken,
        op: &str,
        route_id: &str,
    ) -> Result<(), SelectAbort> {
        let timeouts = self.timeouts();
        let step = |d: Duration| tokio::time::Instant::now() + d;
        // Before the commit the operation owns the generation that `supersede` produced.
        let pre_generation = relay.generation();
        let fence = Self::fence_for(relay, token, pre_generation);
        // 1. HQPlayer must be reachable and its state known before anything changes.
        let transport = cancellable(
            token,
            adapter.output_hook_transport(&fence, step(timeouts.control_step)),
        )
        .await?;
        let transport = match transport {
            Ok(NativeHookOutcome::Done(t)) => t,
            Ok(NativeHookOutcome::NotAttempted(_)) => return Err(SelectAbort::Cancelled),
            Ok(NativeHookOutcome::Indeterminate(reason)) => {
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Failed,
                    Some(format!("Route unchanged. {reason}")),
                    |_| {},
                );
                return Ok(());
            }
            Err(error) => {
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Failed,
                    Some(format!("Route unchanged. {error}")),
                    |_| {},
                );
                return Ok(());
            }
        };
        let state = transport.state.to_string();
        {
            let mut ledger = lock(&self.ledger);
            ledger.native.transport_state = Some(state.clone());
            ledger.native.track = transport.track.clone();
            ledger.native.position = transport.position.map(|p| p.to_string());
            ledger.native.position_restored = None;
        }
        self.update_operation(op, |o| {
            o.evidence.native_state_before = Some(state.clone());
        });
        if !matches!(transport.state, 0..=2) {
            self.finish_operation(
                op,
                HqpOutputOutcome::Failed,
                Some(format!(
                    "Route unchanged. HQPlayer reported unknown state {}",
                    transport.state
                )),
                |_| {},
            );
            return Ok(());
        }
        let playing = transport.state == 2;
        // 2. Stop playing or paused transport before detaching its NAA. Only an originally
        //    playing source resumes.
        if transport.state != 0 {
            self.set_phase(op, HqpOutputPhase::Stopping);
            self.publish(None).await;
            let stopped = cancellable(
                token,
                adapter.output_hook_stop_verified(&fence, step(timeouts.control_step)),
            )
            .await?;
            match stopped {
                Ok(NativeHookOutcome::Done(())) => {}
                Ok(NativeHookOutcome::NotAttempted(_)) => return Err(SelectAbort::Cancelled),
                Ok(NativeHookOutcome::Indeterminate(reason)) => {
                    lock(&self.ledger).native.transport_state = None;
                    self.finish_operation(
                        op,
                        HqpOutputOutcome::Indeterminate,
                        Some(format!(
                            "Route unchanged. HQPlayer transport Stop is indeterminate: {reason}"
                        )),
                        |_| {},
                    );
                    return Ok(());
                }
                Err(error) => {
                    self.finish_operation(
                        op,
                        HqpOutputOutcome::Failed,
                        Some(format!("Route unchanged. {error}")),
                        |_| {},
                    );
                    return Ok(());
                }
            }
            self.update_operation(op, |o| o.evidence.native_stop_verified = Some(true));
            lock(&self.ledger).native.transport_state = Some("0".into());
        }
        if token.is_cancelled() || relay.generation() != pre_generation {
            return Err(SelectAbort::Cancelled);
        }
        // 3. Commit: persist, tear down the old pair, reopen the routing gate.
        let generation = match relay.commit_selection(route_id) {
            Ok(generation) => generation,
            Err(message) => {
                self.finish_operation(op, HqpOutputOutcome::Failed, Some(message), |_| {});
                return Ok(());
            }
        };
        let fence = Self::fence_for(relay, token, generation);
        self.bump_revision();
        self.update_operation(op, |o| {
            o.phase = HqpOutputPhase::Committed;
            o.route_generation = Some(generation);
        });
        self.publish(None).await;
        if !playing {
            self.finish_operation(op, HqpOutputOutcome::Complete, None, |_| {});
            return Ok(());
        }
        let deadline = tokio::time::Instant::now() + timeouts.select_deadline;
        // 4. Wait for a fresh successful initialize on the newly selected route.
        self.set_phase(op, HqpOutputPhase::Connecting);
        let initialized = self
            .await_session(relay, token, generation, deadline, timeouts.poll, |s| {
                s.initialized
            })
            .await?;
        let session_id = match initialized {
            Ok(view) => view.session_id,
            Err(message) => {
                // HQPlayer is stopped; nothing plays. Route stays as selected.
                self.finish_operation(
                    op,
                    HqpOutputOutcome::Failed,
                    Some(format!(
                        "Route selected but playback was not resumed: {message}"
                    )),
                    |_| {},
                );
                return Ok(());
            }
        };
        self.update_operation(op, |o| {
            o.phase = HqpOutputPhase::Initialized;
            o.evidence.session_id = Some(session_id);
            o.evidence.initialized = true;
        });
        // 5. Single Play, then verify accepted start, forwarded audio and state 2.
        self.set_phase(op, HqpOutputPhase::Resuming);
        self.publish(None).await;
        let played = cancellable(
            token,
            adapter.output_hook_play(&fence, step(timeouts.control_step).min(deadline)),
        )
        .await?;
        match played {
            Ok(NativeHookOutcome::Done(())) => {}
            Ok(NativeHookOutcome::NotAttempted(_)) => return Err(SelectAbort::Cancelled),
            Ok(NativeHookOutcome::Indeterminate(reason)) => {
                return self
                    .resume_failed(
                        adapter,
                        relay,
                        &fence,
                        generation,
                        op,
                        true,
                        true,
                        format!("Route selected; Play is indeterminate: {reason}"),
                    )
                    .await;
            }
            Err(error) => {
                return self
                    .resume_failed(
                        adapter,
                        relay,
                        &fence,
                        generation,
                        op,
                        true,
                        false,
                        format!("Route selected but Play failed: {error}"),
                    )
                    .await;
            }
        }
        let started = self
            .await_session(relay, token, generation, deadline, timeouts.poll, |s| {
                s.started && s.current_stream_audio_bytes > 0
            })
            .await?;
        let view = match started {
            Ok(view) => view,
            Err(message) => {
                return self
                    .resume_failed(adapter, relay, &fence, generation, op, true, false, format!("Route selected and Play accepted, but audio did not start on the new route: {message}"))
                    .await;
            }
        };
        self.update_operation(op, |o| {
            o.evidence.started = true;
            o.evidence.current_stream_audio_bytes = view.current_stream_audio_bytes;
        });
        let verified = loop {
            let state = cancellable(
                token,
                adapter.output_hook_state(&fence, step(timeouts.control_step).min(deadline)),
            )
            .await?;
            match state {
                Ok(NativeHookOutcome::Done(2)) => break Ok(()),
                Ok(NativeHookOutcome::Done(other)) if tokio::time::Instant::now() < deadline => {
                    let _ = other;
                    cancellable(token, tokio::time::sleep(Duration::from_millis(200))).await?;
                }
                Ok(NativeHookOutcome::Done(other)) => {
                    break Err(format!("HQPlayer reported state {other} after Play"))
                }
                Ok(NativeHookOutcome::NotAttempted(_)) => return Err(SelectAbort::Cancelled),
                Ok(NativeHookOutcome::Indeterminate(reason)) => break Err(reason),
                Err(error) => break Err(error.to_string()),
            }
        };
        if let Err(message) = verified {
            return self
                .resume_failed(adapter, relay, &fence, generation, op, true, false, format!("Audio reached the new route but HQPlayer did not confirm playing: {message}"))
                .await;
        }
        self.update_operation(op, |o| {
            o.phase = HqpOutputPhase::Forwarding;
            o.evidence.native_state_after = Some("2".into());
        });
        lock(&self.ledger).native.transport_state = Some("2".into());
        // 6. Restore the captured source position once the new stream is actually playing.
        let seconds = transport
            .position
            .filter(|p| p.is_finite() && *p >= 1.0)
            .map(|p| p.floor() as u64);
        let (restored, warning) = match (transport.track.as_deref(), seconds) {
            (Some(track), Some(seconds)) => {
                let restored = cancellable(
                    token,
                    adapter.output_hook_restore_position(
                        &fence,
                        step(timeouts.control_step),
                        track,
                        seconds,
                    ),
                )
                .await?;
                match restored {
                    Ok(NativeHookOutcome::Done(true)) => (Some(true), None),
                    Ok(NativeHookOutcome::Done(false)) => (Some(false), Some(format!("Route switched and playing, but the source position ({seconds}s) could not be confirmed; check playback position."))),
                    Ok(NativeHookOutcome::NotAttempted(_)) => return Err(SelectAbort::Cancelled),
                    Ok(NativeHookOutcome::Indeterminate(reason)) => (Some(false), Some(format!("Route switched and playing, but the Seek to {seconds}s is indeterminate; check playback position. {reason}"))),
                    Err(error) => (Some(false), Some(format!("Route switched and playing, but the source position ({seconds}s) could not be confirmed; check playback position. {error}"))),
                }
            }
            _ => (None, None),
        };
        lock(&self.ledger).native.position_restored = restored;
        let outcome = if warning.is_some() {
            HqpOutputOutcome::Partial
        } else {
            HqpOutputOutcome::Complete
        };
        let latest_bytes = relay
            .session_for(generation)
            .ok()
            .flatten()
            .map(|s| s.current_stream_audio_bytes)
            .unwrap_or(view.current_stream_audio_bytes);
        self.finish_operation(op, outcome, warning, |o| {
            o.evidence.position_restored = restored;
            o.evidence.current_stream_audio_bytes =
                latest_bytes.max(o.evidence.current_stream_audio_bytes);
        });
        Ok(())
    }

    /// Poll the relay's session view for `generation` until `accept` matches, the session fails,
    /// the generation is superseded or the deadline passes. Never holds a lock across an await.
    async fn await_session(
        &self,
        relay: &NaaRelay,
        token: &CancellationToken,
        generation: u64,
        deadline: tokio::time::Instant,
        poll: Duration,
        accept: impl Fn(&super::outputs::HqpRelaySessionView) -> bool,
    ) -> Result<Result<super::outputs::HqpRelaySessionView, String>, SelectAbort> {
        loop {
            match relay.session_for(generation) {
                Err(_) => return Err(SelectAbort::Cancelled),
                Ok(Some(view)) if accept(&view) => return Ok(Ok(view)),
                Ok(Some(_)) => {}
                Ok(None) => {
                    if let Some(error) = relay.last_error() {
                        return Ok(Err(error));
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(Err(format!(
                    "timed out within the {:?} selection budget",
                    self.timeouts().select_deadline
                )));
            }
            cancellable(token, tokio::time::sleep(poll)).await?;
        }
    }

    /// After a failed automatic resume the selected route stays visible; if Play was issued the
    /// local pair is torn down, a bounded native Stop is sent **only while this operation still
    /// owns the generation** (a newer selection or Stop owns the transport otherwise), and the
    /// routing gate is closed so nothing can keep playing unexpectedly until the next selection.
    #[allow(clippy::too_many_arguments)]
    async fn resume_failed(
        &self,
        adapter: &Arc<HqpAdapter>,
        relay: &Arc<NaaRelay>,
        fence: &NativeHookFence,
        generation: u64,
        op: &str,
        play_sent: bool,
        indeterminate: bool,
        mut message: String,
    ) -> Result<(), SelectAbort> {
        let mut native_after = None;
        if play_sent {
            let deadline = tokio::time::Instant::now() + self.timeouts().control_step;
            match adapter.output_hook_stop_verified(fence, deadline).await {
                Ok(NativeHookOutcome::Done(())) => native_after = Some("0".to_string()),
                Ok(NativeHookOutcome::NotAttempted(reason)) => {
                    message.push_str(&format!(" Cleanup Stop not attempted: {reason}."));
                }
                Ok(NativeHookOutcome::Indeterminate(reason)) => {
                    message.push_str(&format!(" HQPlayer transport state is unknown; cleanup Stop is indeterminate: {reason}"));
                }
                Err(error) => {
                    message.push_str(&format!(
                        " HQPlayer transport state is unknown; Stop failed: {error}"
                    ));
                }
            }
            relay.disable_routing_after_failed_resume(generation, message.clone());
        }
        lock(&self.ledger).native.transport_state = native_after.clone();
        self.record_error("RESUME", message.clone());
        let outcome = if indeterminate {
            HqpOutputOutcome::Indeterminate
        } else {
            HqpOutputOutcome::Failed
        };
        self.finish_operation(op, outcome, Some(message), |o| {
            o.evidence.native_state_after = native_after;
        });
        Ok(())
    }
}

impl Drop for HqpOutputCoordinator {
    /// The owning adapter is gone: cancel pending work and the publisher. The relay guard's own
    /// `Drop` closes the listener and the pair; nothing here blocks.
    fn drop(&mut self) {
        if let Some(pending) = lock(&self.pending).take() {
            pending.token.cancel();
        }
        if let Some((_, setup)) = lock(&self.pending_setup).take() {
            setup.cancel();
        }
        if let Ok(mut publisher) = self.publisher.try_lock() {
            if let Some(publisher) = publisher.take() {
                publisher.shutdown.cancel();
                publisher.join.abort();
            }
        }
    }
}

enum SelectAbort {
    Cancelled,
}

async fn cancellable<T>(
    token: &CancellationToken,
    future: impl std::future::Future<Output = T>,
) -> Result<T, SelectAbort> {
    tokio::select! {
        _ = token.cancelled() => Err(SelectAbort::Cancelled),
        value = future => Ok(value),
    }
}

fn validate_settings(settings: &NaaRelaySettings) -> Result<(), String> {
    settings
        .bind
        .parse::<std::net::SocketAddr>()
        .map_err(|e| format!("invalid bind address {:?}: {e}", settings.bind))?;
    for ip in &settings.hqp_allow {
        ip.parse::<std::net::IpAddr>()
            .map_err(|e| format!("invalid hqp_allow entry {ip:?}: {e}"))?;
    }
    if let Some(interface) = &settings.discovery_interface {
        let ip: Ipv4Addr = interface
            .parse()
            .map_err(|e| format!("invalid discovery_interface {interface:?}: {e}"))?;
        if ip.is_unspecified() || ip.is_multicast() {
            return Err("discovery_interface must be an explicit local IPv4 address".into());
        }
        let bind: std::net::SocketAddr = settings
            .bind
            .parse()
            .map_err(|e| format!("invalid bind address {:?}: {e}", settings.bind))?;
        if !bind.ip().is_unspecified() && bind.ip() != std::net::IpAddr::V4(ip) {
            return Err("discovery_interface must match the relay bind address".into());
        }
    }
    if settings.adapter_name.trim().is_empty()
        || settings.adapter_name.len() > 256
        || settings.adapter_name.chars().any(char::is_control)
    {
        return Err("adapter_name must contain 1–256 printable bytes".into());
    }
    Ok(())
}

/// PoC `routes.json` shape (`experiments/naa-router/native/naa-router/src/state.rs::Config`).
#[derive(serde::Deserialize)]
struct PocRoute {
    id: String,
    name: String,
    host: String,
    #[serde(default = "default_poc_port")]
    port: u16,
    #[serde(default)]
    device_id: String,
}

fn default_poc_port() -> u16 {
    super::outputs::DEFAULT_NAA_PORT
}

#[derive(serde::Deserialize)]
struct PocConfig {
    #[serde(default)]
    routes: Vec<PocRoute>,
    #[serde(default)]
    selected_route_id: Option<String>,
}

/// Map a PoC routes file onto stable routes without applying anything. Ids, host, port and
/// device are preserved exactly; the file's selection is reported and never applied.
pub fn import_preview(
    routes_json: &str,
    existing: &[HqpOutputRoute],
) -> Result<HqpImportPreview, String> {
    if routes_json.len() > 1024 * 1024 {
        return Err("routes_json exceeds 1 MiB".into());
    }
    let config: PocConfig = serde_json::from_str(routes_json)
        .map_err(|e| format!("routes_json is not a PoC routes file: {e}"))?;
    let mut routes = Vec::new();
    let mut conflicts = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for route in config.routes {
        if route.id.is_empty() {
            conflicts.push(HqpImportConflict {
                route_id: route.id,
                reason: "empty route id".into(),
            });
            continue;
        }
        if !seen.insert(route.id.clone()) {
            conflicts.push(HqpImportConflict {
                route_id: route.id,
                reason: "duplicate route id in file".into(),
            });
            continue;
        }
        if existing.iter().any(|r| r.route_id == route.id) {
            conflicts.push(HqpImportConflict {
                route_id: route.id,
                reason: "route id already exists".into(),
            });
            continue;
        }
        let device_id = (!route.device_id.is_empty()).then_some(route.device_id.clone());
        if let Err(reason) =
            super::relay::validate_route(&route.name, &route.host, route.port, device_id.as_deref())
        {
            conflicts.push(HqpImportConflict {
                route_id: route.id,
                reason,
            });
            continue;
        }
        routes.push(HqpOutputRoute {
            route_id: route.id,
            name: route.name.trim().to_string(),
            host: route.host,
            port: route.port,
            device_id,
            imported_from: Some("poc-routes-json".into()),
        });
    }
    let preview_id = {
        use sha2::{Digest, Sha256};
        let normalized = serde_json::to_vec(&routes).unwrap_or_default();
        hex::encode(Sha256::digest(normalized))
    };
    Ok(HqpImportPreview {
        preview_id,
        routes,
        conflicts,
        ignored_selected_route_id: config.selected_route_id,
    })
}
