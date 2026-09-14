//! Public wire types for HQPlayer output routing (NAA managed relay).
//!
//! These shapes are the contract shared by `GET /hqplayer/outputs`, `POST /hqplayer/outputs/command`,
//! `GET /hqplayer/outputs/operation`, the `hifi_hqplayer_outputs` / `hifi_hqplayer_output_control`
//! MCP tools and the Dioxus HQPlayer page. They are deliberately plain data: every surface reads
//! the aggregator-committed [`HqpOutputProjection`] and submits the same typed
//! [`HqpOutputCommandRequest`] through one command service. Nothing here performs I/O.
//!
//! Two revisions are carried on purpose and must not be confused:
//! * `output_revision` is the per-instance mutation revision of this document. Every mutation
//!   except `stop`, `discover` and the read-only previews must echo it.
//! * `aggregate_revision` is telemetry: the aggregator's global projection revision at commit.
//!
//! Freshness vocabulary: `discovery: None` means *never scanned*, `discovery.endpoints == []` means
//! *scanned and nothing answered*. An endpoint absent from `dac_observations` is *unknown*; an entry
//! with `devices == []` is *observed empty*. A DAC observation never claims current attachment.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// The stable virtual device id HQPlayer sees for the managed relay.
pub const VIRTUAL_DEVICE_ID: &str = "hiphi:router";
/// Default adapter/device display name HQPlayer sees for the managed relay.
pub const DEFAULT_ADAPTER_NAME: &str = "HiPhi Router";
/// Standard NAA TCP/UDP port.
pub const DEFAULT_NAA_PORT: u16 = 43210;
/// Bounded operation history retained in the projection.
pub const OPERATION_HISTORY_LIMIT: usize = 32;

/// Persisted per-instance relay settings. `enabled: false` opens no listener at all.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NaaRelaySettings {
    #[serde(default)]
    pub enabled: bool,
    /// NAA TCP listener address. Loopback by default; a LAN address requires `hqp_allow`.
    #[serde(default = "default_bind")]
    pub bind: String,
    /// Accept NAA/discovery only from these HQPlayer IPs. Mandatory for a non-loopback bind.
    #[serde(default)]
    pub hqp_allow: Vec<String>,
    /// Opt-in NAA multicast discovery on this explicit local IPv4 interface.
    #[serde(default)]
    pub discovery_interface: Option<String>,
    /// UDP port NAA discovery uses; HQPlayer's scanner asks the standard 43210. The relay must
    /// bind its TCP listener on the same port for discovery to name a reachable endpoint.
    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,
    #[serde(default = "default_adapter_name")]
    pub adapter_name: String,
}

fn default_discovery_port() -> u16 {
    DEFAULT_NAA_PORT
}

fn default_bind() -> String {
    format!("127.0.0.1:{DEFAULT_NAA_PORT}")
}

fn default_adapter_name() -> String {
    DEFAULT_ADAPTER_NAME.to_string()
}

impl Default for NaaRelaySettings {
    fn default() -> Self {
        Self {
            enabled: false,
            bind: default_bind(),
            hqp_allow: Vec::new(),
            discovery_interface: None,
            discovery_port: default_discovery_port(),
            adapter_name: default_adapter_name(),
        }
    }
}

/// Whether the managed relay listener exists and is owned by UHC.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HqpOutputAvailability {
    /// Relay disabled in this instance's settings: no listener exists. Routes stay readable.
    Disabled,
    /// Relay listener bound and owned by UHC.
    Available,
    /// The owned listener failed or exited. The last observation is retained; this is not an
    /// empty inventory.
    Unavailable { reason: String, since: u64 },
}

/// Non-secret relay configuration as published.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpRelayConfigView {
    pub enabled: bool,
    pub adapter_name: String,
    pub virtual_device_id: String,
    /// Bound address once listening, otherwise the configured value.
    pub bind: Option<String>,
    pub hqp_allow: Vec<String>,
    pub discovery_interface: Option<String>,
    /// UDP discovery port the relay answers on (and the scanner asks).
    #[serde(default = "default_discovery_port")]
    pub discovery_port: u16,
    /// Bound UDP address while the relay is advertising itself; null when discovery is off or
    /// the responder could not bind.
    #[serde(default)]
    pub discovery_responder: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpEndpointRef {
    pub host: String,
    pub port: u16,
    pub device_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpOutputRoute {
    /// Stable identifier: UUIDv4 for new routes, the PoC id kept verbatim on import.
    pub route_id: String,
    pub name: String,
    pub host: String,
    pub port: u16,
    /// `None` resolves the endpoint's sole output on a fresh authenticated connection.
    pub device_id: Option<String>,
    pub imported_from: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpRelaySessionView {
    pub session_id: u64,
    pub route_id: String,
    /// Relay generation the session was reserved under.
    pub route_generation: u64,
    pub state: String,
    pub peer: String,
    pub connected_at: u64,
    pub bytes_to_naa: u64,
    pub bytes_from_naa: u64,
    /// Downstream initialize reply with explicit `result="1"`.
    pub initialized: bool,
    /// Downstream start reply with explicit `result="1"`; reset on every start.
    pub started: bool,
    /// Current-stream audio-section payload bytes only; reset on each start.
    pub current_stream_audio_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpDiscoveredEndpoint {
    pub name: String,
    pub host: String,
    pub port: u16,
    pub protocol: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpDiscoveryObservation {
    pub scanned_at: u64,
    pub interface: String,
    pub duration_ms: u64,
    pub provenance: String,
    /// `[]` means the scan completed and nothing answered. Self and other routers are excluded.
    pub endpoints: Vec<HqpDiscoveredEndpoint>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpDacDevice {
    pub id: String,
    pub description: String,
}

/// One endpoint's last relayed `getdevices` reply. Identity is host+port, never the DAC id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpDacObservation {
    pub host: String,
    pub port: u16,
    pub observed_at: u64,
    pub session_id: u64,
    pub provenance: String,
    /// Whole-list replacement. `[]` means the endpoint replied with zero outputs.
    pub devices: Vec<HqpDacDevice>,
}

/// Cached native-control view. Reading it never issues native traffic.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpNativeControlView {
    pub transport_state: Option<String>,
    pub track: Option<String>,
    pub position: Option<String>,
    /// `None` not attempted; `Some(true)` confirmed by readback; `Some(false)` attempted but
    /// unconfirmed, refused or mismatched.
    pub position_restored: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpOutputError {
    pub code: String,
    pub message: String,
    pub at: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HqpOutputPhase {
    Admitted,
    Checking,
    Stopping,
    Committed,
    Connecting,
    Initialized,
    Resuming,
    Forwarding,
    Complete,
    Cancelled,
    Rejected,
    Failed,
    Partial,
    Indeterminate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HqpOutputOutcome {
    Complete,
    Cancelled,
    Rejected,
    Failed,
    Partial,
    Indeterminate,
}

impl HqpOutputOutcome {
    pub fn phase(self) -> HqpOutputPhase {
        match self {
            Self::Complete => HqpOutputPhase::Complete,
            Self::Cancelled => HqpOutputPhase::Cancelled,
            Self::Rejected => HqpOutputPhase::Rejected,
            Self::Failed => HqpOutputPhase::Failed,
            Self::Partial => HqpOutputPhase::Partial,
            Self::Indeterminate => HqpOutputPhase::Indeterminate,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpOutputEvidence {
    pub native_state_before: Option<String>,
    pub native_stop_verified: Option<bool>,
    /// Fresh relay session on the new route.
    pub session_id: Option<u64>,
    pub initialized: bool,
    pub started: bool,
    /// Audio-section payload bytes from the same `session_id` / `route_generation`.
    pub current_stream_audio_bytes: u64,
    pub native_state_after: Option<String>,
    pub position_restored: Option<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpImportConflict {
    pub route_id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpImportPreview {
    /// Bound to the normalized `routes_json` text this preview read.
    pub preview_id: String,
    pub routes: Vec<HqpOutputRoute>,
    pub conflicts: Vec<HqpImportConflict>,
    /// Reported when the PoC file named a selection. It is never applied.
    pub ignored_selected_route_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpSetupAttribute {
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpSetupChange {
    pub attribute: String,
    pub from: Option<String>,
    pub to: String,
}

/// The derived one-time setup proposal: switch HQPlayer's running output to the managed relay
/// through its own authenticated `/config` form, preserving every other successful control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpSetupPreview {
    /// Bound to the exact form controls and persistent bytes the preview read.
    pub preview_id: String,
    pub applicable: bool,
    pub blocker: Option<String>,
    /// The output-related controls as HQPlayer shows them now (backend, net_device, alsa_device,
    /// net_bits, net_period, mode).
    pub current: Vec<HqpSetupAttribute>,
    /// Exactly what apply changes; everything else is resubmitted unchanged.
    pub changes: Vec<HqpSetupChange>,
    /// Number of successful form controls resubmitted unchanged (DSP settings included).
    #[serde(default)]
    pub preserved_controls: usize,
    /// The exact `net_device` option HQPlayer offers for the relay, when it has discovered it.
    #[serde(default)]
    pub relay_option: Option<String>,
    /// SHA-256 of the persistent configuration (`/backup`) at preview time.
    pub backup_sha256: String,
    /// SHA-256 of the form body apply would post.
    pub proposed_sha256: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpSetupTransaction {
    /// `apply` | `readback` | `rollback`.
    pub step: String,
    /// Whether the authenticated form (apply/rollback) reached HQPlayer.
    pub uploaded: bool,
    pub daemon_response: Option<String>,
    /// Running selection as HQPlayer's own form reports it after settling.
    #[serde(default)]
    pub runtime_matches: Option<bool>,
    /// Persistent configuration as `/backup` reports it after settling.
    #[serde(default)]
    pub disk_matches: Option<bool>,
    /// `runtime_matches && disk_matches`.
    pub readback_matches: Option<bool>,
    /// SHA-256 of the persistent configuration read back.
    pub readback_sha256: Option<String>,
    pub settled_after_ms: Option<u64>,
    pub rollback_available: bool,
}

/// Typed result of read-like and transactional actions. Never JSON inside a string.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum HqpOutputResult {
    Discovery(HqpDiscoveryObservation),
    ImportPreview(HqpImportPreview),
    ImportApplied {
        added: Vec<HqpOutputRoute>,
        skipped: Vec<HqpImportConflict>,
        ignored_selected_route_id: Option<String>,
    },
    SetupPreview(HqpSetupPreview),
    Setup(HqpSetupTransaction),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpOutputOperation {
    /// UHC-minted `op-<command id>`.
    pub operation_id: String,
    pub correlation_id: Option<String>,
    /// sha256 of the canonical `(zone_id, action)` JSON.
    pub request_fingerprint: String,
    pub zone_id: String,
    pub action: String,
    pub route_id: Option<String>,
    pub phase: HqpOutputPhase,
    pub outcome: Option<HqpOutputOutcome>,
    pub source_epoch: u64,
    pub output_revision_at_admission: u64,
    pub route_generation: Option<u64>,
    pub admitted_at: u64,
    pub updated_at: u64,
    /// Human-readable only; never structured data.
    pub detail: Option<String>,
    pub result: Option<HqpOutputResult>,
    pub evidence: HqpOutputEvidence,
}

impl HqpOutputOperation {
    pub fn is_terminal(&self) -> bool {
        self.outcome.is_some()
    }

    /// Whether this record carries **historical** evidence that audio reached the new route while
    /// the operation ran: an accepted start plus positive current-stream payload on the fresh
    /// session. It says nothing about now; use
    /// [`HqpOutputProjection::operation_confirms_current_audio`] for a present-tense claim.
    pub fn historical_audio_evidence(&self) -> bool {
        self.evidence.started
            && self.evidence.current_stream_audio_bytes > 0
            && self.evidence.session_id.is_some()
            && matches!(
                self.phase,
                HqpOutputPhase::Forwarding | HqpOutputPhase::Complete | HqpOutputPhase::Partial
            )
    }
}

/// The aggregator-committed output document for one exact HQPlayer instance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpOutputProjection {
    pub zone_id: String,
    pub instance: String,
    /// Reliable-projection source epoch stamped by the aggregator at commit.
    pub source_epoch: u64,
    /// Aggregator projection revision at commit (telemetry, never a mutation precondition).
    pub aggregate_revision: u64,
    /// Per-instance mutation revision of this document.
    pub output_revision: u64,
    /// Relay generation: bumps on every route/selection change, Stop and disconnect.
    pub route_generation: u64,
    pub availability: HqpOutputAvailability,
    pub relay: HqpRelayConfigView,
    pub routes: Vec<HqpOutputRoute>,
    pub selected_route_id: Option<String>,
    pub desired_destination: Option<HqpEndpointRef>,
    pub observed_forwarding_destination: Option<HqpEndpointRef>,
    pub session: Option<HqpRelaySessionView>,
    pub discovery: Option<HqpDiscoveryObservation>,
    pub dac_observations: Vec<HqpDacObservation>,
    pub native: HqpNativeControlView,
    pub current_operation_id: Option<String>,
    /// Newest first, bounded by [`OPERATION_HISTORY_LIMIT`].
    pub operations: Vec<HqpOutputOperation>,
    pub last_error: Option<HqpOutputError>,
    pub observed_at: u64,
}

impl HqpOutputProjection {
    pub fn operation(&self, operation_id: &str) -> Option<&HqpOutputOperation> {
        self.operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
    }

    /// The only present-tense "audio is flowing" claim any surface may make from this document.
    ///
    /// Requires all of: the relay is available (a retained snapshot behind an unavailable listener
    /// proves nothing), a route is selected and the live session belongs to it, the session was
    /// reserved under the current route generation, and it reports an accepted start with positive
    /// current-stream payload while forwarding.
    pub fn session_confirms_audio(&self) -> bool {
        self.availability == HqpOutputAvailability::Available
            && self.session.as_ref().is_some_and(|session| {
                Some(&session.route_id) == self.selected_route_id.as_ref()
                    && session.route_generation == self.route_generation
                    && session.initialized
                    && session.started
                    && session.current_stream_audio_bytes > 0
                    && session.state == "forwarding"
            })
    }

    /// Whether `operation_id`'s route is the one audibly forwarding **now**: the operation's
    /// historical evidence, its route generation and session must all match the live session, and
    /// [`Self::session_confirms_audio`] must hold. An operation that finished before a Stop or a
    /// newer selection can never satisfy this.
    pub fn operation_confirms_current_audio(&self, operation_id: &str) -> bool {
        let Some(operation) = self.operation(operation_id) else {
            return false;
        };
        let Some(session) = self.session.as_ref() else {
            return false;
        };
        operation.historical_audio_evidence()
            && operation.route_generation == Some(self.route_generation)
            && operation.evidence.session_id == Some(session.session_id)
            && operation.route_id.as_ref() == self.selected_route_id.as_ref()
            && self.session_confirms_audio()
    }
}

/// Typed command actions. Flattened into the request object on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HqpOutputAction {
    Discover,
    RouteAdd {
        name: String,
        host: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        device_id: Option<String>,
    },
    RouteUpdate {
        route_id: String,
        name: String,
        host: String,
        #[serde(default)]
        port: Option<u16>,
        #[serde(default)]
        device_id: Option<String>,
    },
    RouteRemove {
        route_id: String,
    },
    Select {
        route_id: String,
    },
    Stop,
    ImportPreview {
        routes_json: String,
    },
    ImportApply {
        routes_json: String,
        preview_id: String,
    },
    RelayConfigure {
        enabled: bool,
        #[serde(default)]
        bind: Option<String>,
        #[serde(default)]
        hqp_allow: Vec<String>,
        #[serde(default)]
        discovery_interface: Option<String>,
        /// Defaults to the standard NAA port 43210 when omitted.
        #[serde(default)]
        discovery_port: Option<u16>,
        #[serde(default)]
        adapter_name: Option<String>,
    },
    SetupPreview,
    SetupApply {
        preview_id: String,
    },
    SetupReadback,
    SetupRollback,
}

impl HqpOutputAction {
    /// Snake-case action name as it appears on the wire.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Discover => "discover",
            Self::RouteAdd { .. } => "route_add",
            Self::RouteUpdate { .. } => "route_update",
            Self::RouteRemove { .. } => "route_remove",
            Self::Select { .. } => "select",
            Self::Stop => "stop",
            Self::ImportPreview { .. } => "import_preview",
            Self::ImportApply { .. } => "import_apply",
            Self::RelayConfigure { .. } => "relay_configure",
            Self::SetupPreview => "setup_preview",
            Self::SetupApply { .. } => "setup_apply",
            Self::SetupReadback => "setup_readback",
            Self::SetupRollback => "setup_rollback",
        }
    }

    /// Whether the caller must echo `expected_source_epoch` and `expected_output_revision`.
    pub fn requires_expectations(&self) -> bool {
        !matches!(
            self,
            Self::Stop
                | Self::Discover
                | Self::ImportPreview { .. }
                | Self::SetupPreview
                | Self::SetupReadback
        )
    }

    /// Whether the action changes routing, configuration or HQPlayer state.
    pub fn is_mutation(&self) -> bool {
        !matches!(
            self,
            Self::Discover | Self::ImportPreview { .. } | Self::SetupPreview | Self::SetupReadback
        )
    }

    pub fn route_id(&self) -> Option<&str> {
        match self {
            Self::RouteUpdate { route_id, .. }
            | Self::RouteRemove { route_id }
            | Self::Select { route_id } => Some(route_id),
            _ => None,
        }
    }
}

/// One typed command from any surface. Flat on the wire.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpOutputCommandRequest {
    pub zone_id: String,
    #[serde(default)]
    pub correlation_id: Option<String>,
    #[serde(default)]
    pub expected_source_epoch: Option<u64>,
    #[serde(default)]
    pub expected_output_revision: Option<u64>,
    #[serde(flatten)]
    pub action: HqpOutputAction,
}

impl HqpOutputCommandRequest {
    /// Canonical request fingerprint: sha256 over the ordered `(zone_id, action)` document.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut document = BTreeMap::new();
        document.insert("zone_id", serde_json::Value::String(self.zone_id.clone()));
        document.insert(
            "action",
            serde_json::to_value(&self.action).unwrap_or(serde_json::Value::Null),
        );
        let canonical = serde_json::to_vec(&document).unwrap_or_default();
        hex::encode(Sha256::digest(canonical))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HqpOutputCommandReceipt {
    pub accepted: bool,
    pub operation: HqpOutputOperation,
    pub projection: HqpOutputProjection,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_request_is_flat_on_the_wire() {
        let request: HqpOutputCommandRequest = serde_json::from_value(serde_json::json!({
            "zone_id": "hqplayer:living",
            "correlation_id": "ui-1",
            "expected_source_epoch": 3,
            "expected_output_revision": 12,
            "action": "select",
            "route_id": "r-1"
        }))
        .expect("flat select request parses");
        assert_eq!(
            request.action,
            HqpOutputAction::Select {
                route_id: "r-1".to_string()
            }
        );
        let wire = serde_json::to_value(&request).expect("serializes");
        assert_eq!(wire["action"], "select");
        assert_eq!(wire["route_id"], "r-1");
        assert!(wire.get("Select").is_none());
    }

    #[test]
    fn stop_and_previews_do_not_require_expectations() {
        assert!(!HqpOutputAction::Stop.requires_expectations());
        assert!(!HqpOutputAction::Discover.requires_expectations());
        assert!(!HqpOutputAction::SetupPreview.requires_expectations());
        assert!(HqpOutputAction::Select {
            route_id: "r".into()
        }
        .requires_expectations());
        assert!(HqpOutputAction::RelayConfigure {
            enabled: true,
            bind: None,
            hqp_allow: vec![],
            discovery_interface: None,
            discovery_port: None,
            adapter_name: None
        }
        .requires_expectations());
    }

    #[test]
    fn fingerprint_ignores_correlation_but_not_parameters() {
        let base = HqpOutputCommandRequest {
            zone_id: "hqplayer:a".into(),
            correlation_id: Some("x".into()),
            expected_source_epoch: Some(1),
            expected_output_revision: Some(2),
            action: HqpOutputAction::Select {
                route_id: "r-1".into(),
            },
        };
        let mut same = base.clone();
        same.correlation_id = Some("y".into());
        same.expected_output_revision = Some(9);
        assert_eq!(base.fingerprint(), same.fingerprint());
        let mut other = base.clone();
        other.action = HqpOutputAction::Select {
            route_id: "r-2".into(),
        };
        assert_ne!(base.fingerprint(), other.fingerprint());
    }

    fn operation(route_generation: u64, session_id: u64, route_id: &str) -> HqpOutputOperation {
        HqpOutputOperation {
            operation_id: "op-1".into(),
            correlation_id: None,
            request_fingerprint: String::new(),
            zone_id: "hqplayer:a".into(),
            action: "select".into(),
            route_id: Some(route_id.into()),
            phase: HqpOutputPhase::Complete,
            outcome: Some(HqpOutputOutcome::Complete),
            source_epoch: 0,
            output_revision_at_admission: 0,
            route_generation: Some(route_generation),
            admitted_at: 0,
            updated_at: 0,
            detail: None,
            result: None,
            evidence: HqpOutputEvidence {
                session_id: Some(session_id),
                initialized: true,
                started: true,
                current_stream_audio_bytes: 4096,
                ..HqpOutputEvidence::default()
            },
        }
    }

    fn session(route_id: &str, route_generation: u64, session_id: u64) -> HqpRelaySessionView {
        HqpRelaySessionView {
            session_id,
            route_id: route_id.into(),
            route_generation,
            state: "forwarding".into(),
            peer: String::new(),
            connected_at: 0,
            bytes_to_naa: 1,
            bytes_from_naa: 1,
            initialized: true,
            started: true,
            current_stream_audio_bytes: 4096,
        }
    }

    fn projection() -> HqpOutputProjection {
        HqpOutputProjection {
            zone_id: "hqplayer:a".into(),
            instance: "a".into(),
            source_epoch: 3,
            aggregate_revision: 10,
            output_revision: 5,
            route_generation: 7,
            availability: HqpOutputAvailability::Available,
            relay: HqpRelayConfigView {
                enabled: true,
                adapter_name: DEFAULT_ADAPTER_NAME.into(),
                virtual_device_id: VIRTUAL_DEVICE_ID.into(),
                bind: None,
                hqp_allow: vec![],
                discovery_interface: None,
                discovery_port: DEFAULT_NAA_PORT,
                discovery_responder: None,
            },
            routes: vec![],
            selected_route_id: Some("r-1".into()),
            desired_destination: None,
            observed_forwarding_destination: None,
            session: Some(session("r-1", 7, 4)),
            discovery: None,
            dac_observations: vec![],
            native: HqpNativeControlView::default(),
            current_operation_id: None,
            operations: vec![operation(7, 4, "r-1")],
            last_error: None,
            observed_at: 0,
        }
    }

    #[test]
    fn a_live_matching_session_confirms_current_audio() {
        let live = projection();
        assert!(live.session_confirms_audio());
        assert!(live.operation_confirms_current_audio("op-1"));
        assert!(live.operations[0].historical_audio_evidence());
    }

    #[test]
    fn a_retained_snapshot_behind_an_unavailable_relay_never_claims_audio() {
        let mut retained = projection();
        retained.availability = HqpOutputAvailability::Unavailable {
            reason: "listener exited".into(),
            since: 1,
        };
        assert!(!retained.session_confirms_audio());
        assert!(!retained.operation_confirms_current_audio("op-1"));
        // The record itself still carries its historical evidence, honestly named.
        assert!(retained.operations[0].historical_audio_evidence());
    }

    #[test]
    fn an_old_operation_after_stop_or_a_newer_selection_never_claims_current_audio() {
        let mut stopped = projection();
        stopped.selected_route_id = None;
        stopped.session = None;
        stopped.route_generation = 8;
        assert!(!stopped.session_confirms_audio());
        assert!(!stopped.operation_confirms_current_audio("op-1"));

        let mut reselected = projection();
        reselected.selected_route_id = Some("r-2".into());
        reselected.route_generation = 9;
        reselected.session = Some(session("r-2", 9, 5));
        assert!(
            reselected.session_confirms_audio(),
            "the new route is forwarding"
        );
        assert!(
            !reselected.operation_confirms_current_audio("op-1"),
            "the old operation's evidence belongs to session 4 / generation 7"
        );
    }

    #[test]
    fn a_session_for_a_route_other_than_the_selected_one_never_claims_audio() {
        let mut mismatched = projection();
        mismatched.selected_route_id = Some("r-9".into());
        assert!(!mismatched.session_confirms_audio());
        assert!(!mismatched.operation_confirms_current_audio("op-1"));
    }

    #[test]
    fn audio_requires_started_initialized_positive_bytes_and_forwarding_state() {
        let mut p = projection();
        p.session.as_mut().map(|s| s.current_stream_audio_bytes = 0);
        assert!(!p.session_confirms_audio());
        let mut p = projection();
        p.session.as_mut().map(|s| s.started = false);
        assert!(!p.session_confirms_audio());
        let mut p = projection();
        p.session.as_mut().map(|s| s.initialized = false);
        assert!(!p.session_confirms_audio());
        let mut p = projection();
        p.session.as_mut().map(|s| s.state = "stopped".into());
        assert!(!p.session_confirms_audio());
        let mut p = projection();
        p.session.as_mut().map(|s| s.route_generation = 6);
        assert!(
            !p.session_confirms_audio(),
            "a session from an older generation is stale"
        );
    }
}

/// Why the command service or the exact-instance coordinator refused a command before or at
/// execution. Encoded into the reliable ticket's failure detail as `CODE|message` so the surface
/// that submitted the command can classify it without a second channel.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "SCREAMING_SNAKE_CASE")]
pub enum HqpOutputRefusal {
    /// The `naa-proxy` feature is not compiled into this build.
    FeatureUnavailable,
    /// The relay is disabled in this instance's settings; enable it with `relay_configure`.
    RelayDisabled,
    /// The owned listener failed or is stopped; last observation retained.
    RelayUnavailable {
        reason: String,
    },
    UnknownRoute {
        route_id: String,
    },
    UnknownOperation {
        operation_id: String,
    },
    StaleExpectation {
        expected_source_epoch: Option<u64>,
        expected_output_revision: Option<u64>,
        current_source_epoch: u64,
        current_output_revision: u64,
    },
    /// Same correlation id reused for a different request fingerprint.
    CorrelationConflict {
        correlation_id: String,
    },
    InvalidCommand {
        message: String,
    },
    /// Retained for wire compatibility only: every action is implemented and no backend path
    /// produces this value any more.
    NotYetImplemented {
        action: String,
    },
    Backend {
        message: String,
    },
}

impl HqpOutputRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::FeatureUnavailable => "FEATURE_UNAVAILABLE",
            Self::RelayDisabled => "RELAY_DISABLED",
            Self::RelayUnavailable { .. } => "RELAY_UNAVAILABLE",
            Self::UnknownRoute { .. } => "UNKNOWN_ROUTE",
            Self::UnknownOperation { .. } => "UNKNOWN_OPERATION",
            Self::StaleExpectation { .. } => "STALE_EXPECTATION",
            Self::CorrelationConflict { .. } => "CORRELATION_CONFLICT",
            Self::InvalidCommand { .. } => "INVALID_COMMAND",
            Self::NotYetImplemented { .. } => "NOT_YET_IMPLEMENTED",
            Self::Backend { .. } => "BACKEND",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::FeatureUnavailable => {
                "the UHC-owned NAA relay (naa-proxy feature) is not compiled into this build".into()
            }
            Self::RelayDisabled => {
                "the NAA relay is disabled for this instance; enable it with relay_configure".into()
            }
            Self::RelayUnavailable { reason } => format!("the NAA relay is unavailable: {reason}"),
            Self::UnknownRoute { route_id } => format!("unknown route_id {route_id:?}"),
            Self::UnknownOperation { operation_id } => {
                format!("unknown operation_id {operation_id:?}")
            }
            Self::StaleExpectation {
                expected_source_epoch,
                expected_output_revision,
                current_source_epoch,
                current_output_revision,
            } => format!(
                "stale expectation: expected source_epoch {expected_source_epoch:?} / output_revision {expected_output_revision:?}, current {current_source_epoch} / {current_output_revision}; re-read GET /hqplayer/outputs and retry"
            ),
            Self::CorrelationConflict { correlation_id } => format!(
                "correlation_id {correlation_id:?} was already used for a different request"
            ),
            Self::InvalidCommand { message } => message.clone(),
            Self::NotYetImplemented { action } => {
                format!("action {action:?} is not implemented in this build")
            }
            Self::Backend { message } => message.clone(),
        }
    }

    /// Wire form for the reliable ticket's failure detail.
    pub fn encode(&self) -> String {
        format!(
            "{}|{}",
            self.code(),
            serde_json::to_string(self).unwrap_or_default()
        )
    }

    /// Recover a refusal from a ticket failure detail, if it carries one.
    pub fn decode(detail: &str) -> Option<Self> {
        let (_, json) = detail.split_once('|')?;
        serde_json::from_str(json).ok()
    }
}
