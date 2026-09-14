//! HQPlayer output-routing (NAA managed relay) MCP tools.
//!
//! Both tools build the same request the HTTP surface
//! (`api::hqp_outputs_http`) deserializes and call the exact same shared
//! command service (`api::hqp_outputs::{read_hqp_outputs, read_hqp_output_operation,
//! submit_hqp_output_command}`) — no adapter orchestration and no duplicated fencing,
//! correlation or targeting logic live here. `zone_id` is required on both tools; there is no
//! default-instance fallback, unlike the other `hifi_hqplayer_*` tools.

use crate::adapters::hqplayer::outputs::{
    HqpOutputAction, HqpOutputCommandRequest, HqpOutputRefusal,
};
use crate::api::hqp_outputs::{
    read_hqp_output_operation, read_hqp_outputs, submit_hqp_output_command, HqpOutputServiceError,
};
use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Provider, Refusal, Scope};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};

/// Read the committed HQPlayer output-routing document
#[mcp_tool(
    name = "hifi_hqplayer_outputs",
    description = "Read the committed HQPlayer output-routing document for an exact instance: relay availability, saved routes, the selected route, the current relay session and recent operations. Pass operation_id to read one operation record instead of the whole document.",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerOutputsTool {
    /// Exact HQPlayer zone id, e.g. "hqplayer:living". Required — there is no default-instance
    /// fallback for output routing.
    pub zone_id: String,
    /// Read one operation record by id (from a prior write's receipt) instead of the projection.
    pub operation_id: Option<String>,
}

/// Mutate HQPlayer output routing
#[mcp_tool(
    name = "hifi_hqplayer_output_control",
    description = "Mutate HQPlayer output routing on an exact instance: relay_configure, route_add, route_update, route_remove, select, stop, discover, import_preview, import_apply, setup_preview, setup_apply, setup_readback, or setup_rollback. Call hifi_hqplayer_outputs first for the current source_epoch/output_revision to echo back — every mutation except stop/discover/import_preview/setup_preview/setup_readback requires both."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerOutputControlTool {
    /// Exact HQPlayer instance zone id; no implicit default is used.
    pub zone_id: String,
    /// One of: relay_configure, route_add, route_update, route_remove, select, stop, discover,
    /// import_preview, import_apply, setup_preview, setup_apply, setup_readback, setup_rollback.
    pub action: String,
    /// Caller-owned id. Reused with an identical request returns the same operation (no second
    /// side effect); reused with a different request is refused.
    pub correlation_id: Option<String>,
    /// Expected source epoch from the latest projection, used to reject stale commands.
    pub expected_source_epoch: Option<u64>,
    /// Expected output revision from the latest projection, used to reject stale commands.
    pub expected_output_revision: Option<u64>,
    /// Existing route id for update, remove, or select.
    pub route_id: Option<String>,
    /// Friendly route or relay name.
    pub name: Option<String>,
    /// NAA destination host or address.
    pub host: Option<String>,
    /// NAA destination TCP port.
    pub port: Option<u16>,
    /// Optional DAC device identifier returned by read-through discovery.
    pub device_id: Option<String>,
    /// JSON route document for import preview/apply.
    pub routes_json: Option<String>,
    /// Preview identity returned by a preview operation.
    pub preview_id: Option<String>,
    /// Enable or disable the managed relay.
    pub enabled: Option<bool>,
    /// Explicit relay bind address; loopback is required unless an allowlist is supplied.
    pub bind: Option<String>,
    /// Optional HQPlayer source allowlist; empty accepts any reachable NAA peer.
    pub hqp_allow: Option<Vec<String>>,
    /// IPv4 interface used for NAA multicast discovery.
    pub discovery_interface: Option<String>,
    /// NAA discovery answers on the port it advertises; omit to default to 43210.
    pub discovery_port: Option<u16>,
    /// Name presented by this proxy as the NAA endpoint/DAC.
    pub adapter_name: Option<String>,
}

/// The `action` values this tool accepts, for the refusal's `accepted` list.
const VALID_ACTIONS: &[&str] = &[
    "discover",
    "route_add",
    "route_update",
    "route_remove",
    "select",
    "stop",
    "import_preview",
    "import_apply",
    "relay_configure",
    "setup_preview",
    "setup_apply",
    "setup_readback",
    "setup_rollback",
];

pub async fn handle_outputs(
    state: &AppState,
    args: HifiHqplayerOutputsTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::read("hifi_hqplayer_outputs", "get_outputs")
        .param("zone_id", args.zone_id.clone())
        .param_opt("operation_id", args.operation_id.clone())
        .scope(Scope::for_zone(state, &args.zone_id, Provider::HqPlayer).await);
    if let Some(operation_id) = args.operation_id.as_deref() {
        match read_hqp_output_operation(state, &args.zone_id, operation_id).await {
            Ok(operation) => Ok(env.json_result(&operation)),
            Err(error) => refused(state, env, &args.zone_id, error).await,
        }
    } else {
        match read_hqp_outputs(state, &args.zone_id).await {
            Ok(projection) => Ok(env.json_result(&projection)),
            Err(error) => refused(state, env, &args.zone_id, error).await,
        }
    }
}

pub async fn handle_output_control(
    state: &AppState,
    args: HifiHqplayerOutputControlTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::write("hifi_hqplayer_output_control", args.action.clone())
        .param("zone_id", args.zone_id.clone())
        .param_opt("correlation_id", args.correlation_id.clone())
        .param_opt("expected_source_epoch", args.expected_source_epoch)
        .param_opt("expected_output_revision", args.expected_output_revision)
        .param_opt("route_id", args.route_id.clone())
        .scope(Scope::for_zone(state, &args.zone_id, Provider::HqPlayer).await);

    let action: HqpOutputAction = match serde_json::from_value(action_json(&args)) {
        Ok(action) => action,
        Err(parse_error) => {
            let detail = format!("action {:?} rejected: {parse_error}", args.action);
            let refusal = Refusal::InvalidParameter {
                parameter: "action",
                accepted: VALID_ACTIONS.iter().map(|a| (*a).to_string()).collect(),
                detail: detail.clone(),
            };
            let data = json!({"error_code": "INVALID_COMMAND", "status": 400});
            return env.data(&data).refused(detail, refusal);
        }
    };

    let request = HqpOutputCommandRequest {
        zone_id: args.zone_id.clone(),
        correlation_id: args.correlation_id.clone(),
        expected_source_epoch: args.expected_source_epoch,
        expected_output_revision: args.expected_output_revision,
        action,
    };
    let zone_id = args.zone_id.clone();
    match submit_hqp_output_command(state, request).await {
        Ok(receipt) => Ok(env.json_result(&receipt)),
        Err(error) => refused(state, env, &zone_id, error).await,
    }
}

/// Every refusal path funnels through here so `data` always carries the same structured
/// `{error_code, status, projection?}` the HTTP surface's error body carries — clients must not
/// have to parse the frozen `Error: {text}` prose to learn what actually happened, especially for
/// `stale_expectation`/`correlation_conflict`/indeterminate outcomes where the caller needs the
/// current committed state to retry correctly.
async fn refused(
    state: &AppState,
    env: Envelope,
    zone_id: &str,
    error: HqpOutputServiceError,
) -> Result<CallToolResult, CallToolError> {
    let mut data = json!({
        "error_code": error.code(),
        "status": error.status(),
    });
    if let Ok(projection) = read_hqp_outputs(state, zone_id).await {
        if let Ok(projection) = serde_json::to_value(projection) {
            data["projection"] = projection;
        }
    }
    let refusal = refusal_for(&error);
    env.data(&data).refused(error.message(), refusal)
}

/// Build the flat `{"action": ..., ...}` object the real `HqpOutputAction` deserializer (an
/// internally tagged enum, `#[serde(tag = "action")]`) expects — the same shape the HTTP body
/// carries — so this tool reuses that one real deserializer instead of hand-mapping each action's
/// fields into a duplicate match.
fn action_json(args: &HifiHqplayerOutputControlTool) -> Value {
    let mut map = Map::new();
    map.insert("action".to_string(), json!(args.action));
    macro_rules! put_opt {
        ($key:literal, $value:expr) => {
            if let Some(v) = $value.clone() {
                map.insert($key.to_string(), json!(v));
            }
        };
    }
    put_opt!("route_id", args.route_id);
    put_opt!("name", args.name);
    put_opt!("host", args.host);
    put_opt!("port", args.port);
    put_opt!("device_id", args.device_id);
    put_opt!("routes_json", args.routes_json);
    put_opt!("preview_id", args.preview_id);
    put_opt!("enabled", args.enabled);
    put_opt!("bind", args.bind);
    put_opt!("hqp_allow", args.hqp_allow);
    put_opt!("discovery_interface", args.discovery_interface);
    put_opt!("discovery_port", args.discovery_port);
    put_opt!("adapter_name", args.adapter_name);
    Value::Object(map)
}

/// Classify a shared-service error into the envelope's refusal vocabulary. The status/code/message
/// mapping itself stays owned by `HqpOutputServiceError` and `HqpOutputRefusal` (`api::hqp_outputs`,
/// `adapters::hqplayer::outputs`) — this only picks which `Refusal` shape best matches, so the HTTP
/// and MCP surfaces can never classify the same error two different ways by accident... except
/// where MCP's richer vocabulary (`unsupported` vs `invalid` vs `error`) has no HTTP status
/// equivalent to derive from, so the split below is this tool's own judgment call.
fn refusal_for(error: &HqpOutputServiceError) -> Refusal {
    match error {
        HqpOutputServiceError::InvalidZone(_) => Refusal::InvalidParameter {
            parameter: "zone_id",
            accepted: vec!["hqplayer:<instance>".to_string()],
            detail: error.message(),
        },
        HqpOutputServiceError::UnknownInstance(_) => Refusal::UnknownTarget {
            parameter: "zone_id",
            discover_with: "hifi_zones",
            detail: error.message(),
        },
        HqpOutputServiceError::Refused(refusal) => match refusal {
            // `FeatureUnavailable` means the `naa-proxy` cargo feature is not compiled into this
            // particular server binary — a local build configuration, not a limit of HQPlayer's
            // own protocol. `ProviderLimitation` is reserved for the latter (see its own doc
            // comment); this is a backend condition instead.
            HqpOutputRefusal::FeatureUnavailable => Refusal::backend_error(error.message()),
            HqpOutputRefusal::UnknownRoute { .. } => Refusal::InvalidParameter {
                parameter: "route_id",
                accepted: vec![],
                detail: error.message(),
            },
            HqpOutputRefusal::UnknownOperation { .. } => Refusal::InvalidParameter {
                parameter: "operation_id",
                accepted: vec![],
                detail: error.message(),
            },
            HqpOutputRefusal::InvalidCommand { .. } => Refusal::InvalidParameter {
                parameter: "action",
                accepted: VALID_ACTIONS.iter().map(|a| (*a).to_string()).collect(),
                detail: error.message(),
            },
            HqpOutputRefusal::RelayDisabled
            | HqpOutputRefusal::RelayUnavailable { .. }
            | HqpOutputRefusal::StaleExpectation { .. }
            | HqpOutputRefusal::CorrelationConflict { .. }
            // Retained on `HqpOutputRefusal` for wire compatibility only (see its own doc
            // comment); no backend path produces it any more, so there is nothing more specific
            // to classify it as here either.
            | HqpOutputRefusal::NotYetImplemented { .. }
            | HqpOutputRefusal::Backend { .. } => Refusal::backend_error(error.message()),
        },
        HqpOutputServiceError::RuntimeUnavailable | HqpOutputServiceError::Indeterminate(_) => {
            Refusal::backend_error(error.message())
        }
    }
}
