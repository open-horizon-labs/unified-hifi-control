//! MCP contract test harness (issue #394).
//!
//! Pins the MCP surface exactly as it is today so later PRs in epic #392 can be
//! shown to be additive rather than merely claimed to be.
//!
//! # What is pinned, and how
//!
//! Two layers, deliberately:
//!
//! 1. **Wire-level snapshots** driven through the real Axum `/mcp` route via
//!    `tower::ServiceExt::oneshot`. These capture the bytes an MCP client
//!    actually receives for `initialize` and `tools/list`, including the
//!    settings-based HQPlayer tool filtering. They exercise `create_mcp_extension`,
//!    the streamable-HTTP transport, and the `ServerHandler` impl — i.e. every
//!    layer a refactor could disturb. Committed fixtures:
//!      - `tests/fixtures/mcp_tools.json`      (`tools/list`, HQPlayer enabled)
//!      - `tests/fixtures/mcp_initialize.json` (`initialize` result)
//!
//! 2. **In-process assertions** over `unified_hifi_control::mcp` for things a
//!    snapshot cannot express: JSON Schema structural validity, the documented
//!    parameter inventory, the no-orphan-field guardrail, and zone routing.
//!
//! # Regenerating fixtures
//!
//! ```sh
//! UPDATE_MCP_FIXTURES=1 cargo test --test mcp_contract
//! ```
//!
//! Regenerate only when the change to the MCP surface is intended and approved.
//! A fixture diff on an "additive only" PR is a bug report, not a chore.

mod mock_servers;

use axum::{
    body::Body,
    http::{header, Method, Request},
    routing::{delete, get, post},
    Router,
};
use serde_json::{json, Value};
use std::{sync::Arc, time::Instant};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use unified_hifi_control::adapters::hqplayer::{HqpInstanceManager, HqpZoneLinkService};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mcp;
use unified_hifi_control::mcp::types::{McpPipelineStatus, McpPlayResult, McpSearchResult};

use mock_servers::{MockHqpServer, MockLmsServer, MockOpenHomeDevice, MockUpnpRenderer};

// =============================================================================
// Fixture plumbing
// =============================================================================

const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

/// The protocol version the server declares in `create_mcp_extension`.
///
/// The SDK negotiates down to whatever the client asks for, so the test client
/// asks for exactly this: that way the `initialize` snapshot pins the *declared*
/// version, and lowering the declared version turns a client request for this
/// version into an outright error rather than a silent downgrade.
const SERVER_PROTOCOL_VERSION: &str = "2025-11-25";

fn fixture_path(name: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

/// Compare `actual` against a committed fixture, or rewrite the fixture when
/// `UPDATE_MCP_FIXTURES=1`.
fn assert_matches_fixture(name: &str, actual: &Value) {
    let path = fixture_path(name);
    let rendered = format!(
        "{}\n",
        serde_json::to_string_pretty(actual).expect("fixture value must serialize")
    );

    if std::env::var("UPDATE_MCP_FIXTURES").as_deref() == Ok("1") {
        std::fs::create_dir_all(path.parent().expect("fixture dir")).expect("create fixture dir");
        std::fs::write(&path, &rendered).expect("write fixture");
        eprintln!("updated fixture: {}", path.display());
        return;
    }

    let expected = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "missing fixture {}: {e}\n\
             Generate it with: UPDATE_MCP_FIXTURES=1 cargo test --test mcp_contract",
            path.display()
        )
    });

    if expected != rendered {
        // Point at the first differing line; a 400-line JSON diff is unreadable.
        let first_diff = expected
            .lines()
            .zip(rendered.lines())
            .enumerate()
            .find(|(_, (a, b))| a != b)
            .map(|(i, (a, b))| format!("line {}:\n  fixture: {a}\n  actual:  {b}", i + 1))
            .unwrap_or_else(|| "line counts differ".to_string());

        panic!(
            "MCP contract drift in {}\n\n{first_diff}\n\n\
             The MCP surface changed. If that was intentional AND approved, regenerate with:\n\
             \x20 UPDATE_MCP_FIXTURES=1 cargo test --test mcp_contract\n\
             Otherwise this is the bug: epic #392 is additive-only.",
            path.display()
        );
    }
}

/// Scoped env var override. `std::env::set_var` is process-global, so every
/// test that uses this must be `#[serial_test::serial(uhc_config_dir)]`.
struct EnvGuard {
    key: &'static str,
    previous: Option<String>,
}

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let previous = std::env::var(key).ok();
        std::env::set_var(key, value);
        Self { key, previous }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(v) => std::env::set_var(self.key, v),
            None => std::env::remove_var(self.key),
        }
    }
}

/// A config dir containing exactly the app settings we want, so
/// `load_app_settings()` is deterministic instead of reading the developer's
/// real `~/.config`.
struct SettingsFixture {
    _dir: tempfile::TempDir,
    _guard: EnvGuard,
}

impl SettingsFixture {
    fn with_hqplayer(enabled: bool) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let settings = json!({
            "hide_knobs_page": false,
            "hide_hqp_page": false,
            "hide_lms_page": false,
            "adapters": {
                "roon": true,
                "upnp": true,
                "openhome": true,
                "lms": true,
                "hqplayer": enabled,
            }
        });
        std::fs::write(
            dir.path().join("app-settings.json"),
            serde_json::to_vec_pretty(&settings).expect("serialize settings"),
        )
        .expect("write settings");

        let guard = EnvGuard::set("UHC_CONFIG_DIR", &dir.path().to_string_lossy());
        Self {
            _dir: dir,
            _guard: guard,
        }
    }
}

// =============================================================================
// Test app
// =============================================================================

/// Disconnected adapters, no network. Overridable so mock-server round-trip
/// tests can substitute a configured adapter.
struct TestApp {
    router: Router,
}

async fn build_state(lms: Option<Arc<LmsAdapter>>) -> AppState {
    build_state_with_bus(create_bus(), lms).await
}

async fn build_state_with_bus(
    bus: unified_hifi_control::bus::SharedBus,
    lms: Option<Arc<LmsAdapter>>,
) -> AppState {
    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let hqp_instances = Arc::new(HqpInstanceManager::new(bus.clone()));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = lms.unwrap_or_else(|| Arc::new(LmsAdapter::new(bus.clone())));
    let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
    let upnp = Arc::new(UPnPAdapter::new(bus.clone()));
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));

    let startable_adapters: Vec<Arc<dyn Startable>> =
        vec![roon.clone(), lms.clone(), openhome.clone(), upnp.clone()];

    AppState::new(
        roon,
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms,
        openhome,
        upnp,
        KnobStore::new(),
        bus,
        aggregator,
        coordinator,
        startable_adapters,
        Instant::now(),
        CancellationToken::new(),
    )
}

impl TestApp {
    async fn new() -> Self {
        Self::with_state(build_state(None).await)
    }

    fn with_state(state: AppState) -> Self {
        // Exactly the three MCP routes from main.rs, nothing else.
        let router = Router::new()
            .route("/mcp", get(mcp::handle_mcp_get))
            .route("/mcp", post(mcp::handle_mcp_post))
            .route("/mcp", delete(mcp::handle_mcp_delete))
            .layer(mcp::create_mcp_extension(state.clone()))
            .with_state(state);

        Self { router }
    }

    /// Issue a bare GET or DELETE to `/mcp`, as MCP clients do to open the SSE
    /// stream and to tear a session down.
    async fn request(
        &self,
        method: Method,
        session_id: Option<&str>,
    ) -> (axum::http::StatusCode, axum::http::HeaderMap) {
        let mut builder = Request::builder()
            .method(method)
            .uri("/mcp")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(sid) = session_id {
            builder = builder.header(MCP_SESSION_ID_HEADER, sid);
        }
        let response = self
            .router
            .clone()
            .oneshot(builder.body(Body::empty()).expect("build request"))
            .await
            .expect("mcp route must respond");

        (response.status(), response.headers().clone())
    }

    /// POST a JSON-RPC message to `/mcp` and return `(response headers, parsed result)`.
    ///
    /// `enable_json_response` is false in `create_mcp_extension`, so responses
    /// arrive SSE-framed; the `data:` payload is the JSON-RPC envelope.
    async fn post(
        &self,
        session_id: Option<&str>,
        payload: Value,
    ) -> (axum::http::HeaderMap, Option<Value>) {
        let mut builder = Request::builder()
            .method(Method::POST)
            .uri("/mcp")
            .header(header::CONTENT_TYPE, "application/json")
            .header(header::ACCEPT, "application/json, text/event-stream");
        if let Some(sid) = session_id {
            builder = builder.header(MCP_SESSION_ID_HEADER, sid);
        }
        let request = builder
            .body(Body::from(payload.to_string()))
            .expect("build request");

        let response = self
            .router
            .clone()
            .oneshot(request)
            .await
            .expect("mcp route must respond");

        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("read mcp body");
        let body = String::from_utf8_lossy(&body).to_string();

        (headers, parse_sse_json(&body))
    }

    /// `initialize`, then `notifications/initialized`. Returns the session id
    /// and the `result` object of the initialize response.
    async fn initialize(&self) -> (String, Value) {
        self.initialize_with_protocol(SERVER_PROTOCOL_VERSION).await
    }

    async fn initialize_with_protocol(&self, protocol_version: &str) -> (String, Value) {
        let (headers, result) = self
            .post(
                None,
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "protocolVersion": protocol_version,
                        "capabilities": {},
                        "clientInfo": { "name": "mcp-contract-test", "version": "1.0.0" }
                    }
                }),
            )
            .await;

        let session_id = headers
            .get(MCP_SESSION_ID_HEADER)
            .and_then(|v| v.to_str().ok())
            .expect("initialize must return an mcp-session-id header")
            .to_string();

        let result = result.expect("initialize must return a JSON-RPC response");
        let result = result
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("initialize returned no result: {result}"));

        self.post(
            Some(&session_id),
            json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;

        (session_id, result)
    }

    /// `tools/list` against an initialized session.
    async fn list_tools(&self) -> Value {
        let (session_id, _) = self.initialize().await;
        let (_, response) = self
            .post(
                Some(&session_id),
                json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
            )
            .await;
        let response = response.expect("tools/list must return a JSON-RPC response");
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/list returned no result: {response}"))
    }

    /// `tools/call` against an initialized session. Returns the `result` object.
    async fn call_tool(&self, name: &str, arguments: Value) -> Value {
        let (session_id, _) = self.initialize().await;
        let (_, response) = self
            .post(
                Some(&session_id),
                json!({
                    "jsonrpc": "2.0",
                    "id": 3,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": arguments }
                }),
            )
            .await;
        let response = response.expect("tools/call must return a JSON-RPC response");
        response
            .get("result")
            .cloned()
            .unwrap_or_else(|| panic!("tools/call {name} returned no result: {response}"))
    }
}

/// Extract the first `data:` payload from an SSE body, or parse the body as
/// plain JSON if the transport ever switches to `enable_json_response`.
fn parse_sse_json(body: &str) -> Option<Value> {
    for line in body.lines() {
        if let Some(data) = line.strip_prefix("data:") {
            return serde_json::from_str(data.trim()).ok();
        }
    }
    serde_json::from_str(body).ok()
}

/// The concatenated text content of a `tools/call` result.
fn result_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("text").and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

// =============================================================================
// 1. tools/list snapshot
// =============================================================================

/// Each tool's contract — description, input schema, annotation hints — pinned
/// against the committed fixture.
///
/// # Why the fixture is keyed by tool name rather than being the raw array
///
/// Epic #392 is additive-only, so the guardrail has to distinguish two things
/// that a positional array conflates:
///
/// - a tool was **added** — permitted, and should read as a purely additive diff
/// - an existing tool's description or schema **changed** — forbidden, and should
///   read as a loud modification
///
/// Keyed by name, adding a tool adds one key and leaves every existing key byte
/// identical. As an array, appending a tool is also additive, but #399 and #400
/// land in parallel by the epic's own plan and each appends one — so whichever
/// merges second would face a conflict plus a mandatory regeneration, which is
/// exactly how reflexive `UPDATE_MCP_FIXTURES=1` becomes a habit.
///
/// Advertised **order** is pinned separately, in
/// [`tools_list_order_is_pinned`], so order and content fail independently.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn tools_list_matches_fixture() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let result = app.list_tools().await;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools/list result must contain a tools array");

    assert_eq!(
        tools.len(),
        10,
        "expected 10 tools with HQPlayer enabled, got {}: {:?}",
        tools.len(),
        tool_names(tools)
    );

    let mut by_name = serde_json::Map::new();
    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("every tool must have a name")
            .to_string();
        // The name is the key, so drop it from the value: otherwise renaming a
        // tool would show up as one added key and one removed key with identical
        // bodies, which reads as additive when it is not.
        let mut body = tool.clone();
        body.as_object_mut()
            .expect("tool must be an object")
            .remove("name");
        assert!(
            by_name.insert(name.clone(), body).is_none(),
            "duplicate tool name in tools/list: {name}"
        );
    }

    assert_matches_fixture("mcp_tools.json", &Value::Object(by_name));
}

/// The order `tools/list` advertises, pinned separately from tool content.
///
/// Order is part of the wire payload and models do weight earlier tools, so it is
/// worth pinning — but a reordering and a description change are different bugs
/// and should not fail the same assertion.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn tools_list_order_is_pinned() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let result = app.list_tools().await;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");

    assert_eq!(
        tool_names(tools),
        vec![
            "hifi_zones",
            "hifi_now_playing",
            "hifi_control",
            "hifi_search",
            "hifi_play",
            "hifi_status",
            "hifi_hqplayer_status",
            "hifi_hqplayer_profiles",
            "hifi_hqplayer_load_profile",
            "hifi_hqplayer_set_pipeline",
        ],
        "tools/list order follows the tool_box! list in src/mcp/tools/mod.rs. \
         APPEND new tools rather than inserting, so this assertion grows by one \
         line instead of shifting."
    );
}

fn tool_names(tools: &[Value]) -> Vec<String> {
    tools
        .iter()
        .filter_map(|t| t.get("name").and_then(Value::as_str))
        .map(str::to_string)
        .collect()
}

/// The settings-based HQPlayer filter in `handle_list_tools_request` is part of
/// the contract: with the adapter disabled, the four `hifi_hqplayer_*` tools
/// must not be advertised, and nothing else may change.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hqplayer_tools_filtered_when_adapter_disabled() {
    let _settings = SettingsFixture::with_hqplayer(false);
    let app = TestApp::new().await;

    let result = app.list_tools().await;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");
    let names = tool_names(tools);

    assert_eq!(
        names,
        vec![
            "hifi_zones",
            "hifi_now_playing",
            "hifi_control",
            "hifi_search",
            "hifi_play",
            "hifi_status",
        ],
        "HQPlayer disabled must yield exactly the six non-HQPlayer tools, in order"
    );
}

// =============================================================================
// 2. initialize snapshot
// =============================================================================

/// The `initialize` result — server info, declared capabilities, instructions,
/// and protocol version — must match the committed fixture.
///
/// `serverInfo.version` comes from `CARGO_PKG_VERSION`, which release builds
/// inject from the git tag, so it is redacted in the fixture and asserted
/// separately.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn initialize_result_matches_fixture() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let (_, mut result) = app.initialize().await;

    let version = result
        .pointer("/serverInfo/version")
        .and_then(Value::as_str)
        .expect("serverInfo.version must be present")
        .to_string();
    assert_eq!(
        version,
        env!("CARGO_PKG_VERSION"),
        "serverInfo.version must be the crate version"
    );
    *result
        .pointer_mut("/serverInfo/version")
        .expect("serverInfo.version") = json!("<CARGO_PKG_VERSION>");

    assert_matches_fixture("mcp_initialize.json", &result);
}

/// The wire snapshot above cannot see a *raised* declared protocol version,
/// because the SDK negotiates down to whatever the client asked for: declare
/// `2026-06-18`, have the client ask for `2025-11-25`, and the result still says
/// `2025-11-25`. Lowering it is caught (the request becomes an outright error);
/// raising it is not.
///
/// So the declared value is also asserted directly off `server_details()` — the
/// reason that function was extracted from `create_mcp_extension`. This reads
/// production's own value instead of restating it, and it is what makes
/// `SERVER_PROTOCOL_VERSION` above a checked mirror rather than a second source
/// of truth.
#[test]
fn declared_protocol_version_and_capabilities_are_pinned() {
    let details = mcp::server_details();

    // `protocol_version` is already a String on InitializeResult; bound here so
    // the assertion reads against production's value rather than a literal.
    let declared: String = details.protocol_version;
    assert_eq!(
        declared, SERVER_PROTOCOL_VERSION,
        "the declared protocol version changed. The wire snapshot cannot catch a \
         raised version, so this assertion is the one that matters — update \
         SERVER_PROTOCOL_VERSION only when the change is intended."
    );

    // Capabilities gate request dispatch in the SDK, so the declared set decides
    // which methods are reachable at all. #397 adds `resources`; #394 must not.
    assert!(
        details.capabilities.tools.is_some(),
        "tools capability must be declared"
    );
    assert!(
        details.capabilities.resources.is_none(),
        "#394 must not declare a resources capability — that is #397's work"
    );
    assert!(
        details.capabilities.prompts.is_none(),
        "no prompts capability is declared today"
    );
    assert!(
        details.capabilities.logging.is_none(),
        "no logging capability is declared today"
    );

    assert_eq!(details.server_info.version, env!("CARGO_PKG_VERSION"));
}

/// Older clients must still get a session: the SDK negotiates down to the
/// client's protocol version. Pinned so a change to the declared version can't
/// silently start rejecting the clients that are deployed today.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn initialize_negotiates_down_for_older_clients() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for requested in ["2024-11-05", "2025-03-26", "2025-06-18"] {
        let (session_id, result) = app.initialize_with_protocol(requested).await;
        assert!(
            !session_id.is_empty(),
            "client asking for {requested} must get a session"
        );
        assert_eq!(
            result.get("protocolVersion").and_then(Value::as_str),
            Some(requested),
            "server must echo the older client's protocol version"
        );
    }
}

// =============================================================================
// 3. Input schemas are valid JSON Schema, and document every parameter
// =============================================================================

/// Every parameter each tool accepts, in declaration order, paired with whether
/// it is required. Encoded here rather than derived so that adding, renaming, or
/// changing the optionality of a parameter is a visible edit to this table.
const EXPECTED_TOOL_PARAMS: &[(&str, &[(&str, bool)])] = &[
    ("hifi_zones", &[]),
    ("hifi_now_playing", &[("zone_id", true)]),
    (
        "hifi_control",
        &[("zone_id", true), ("action", true), ("value", false)],
    ),
    (
        "hifi_search",
        &[("query", true), ("zone_id", false), ("source", false)],
    ),
    (
        "hifi_play",
        &[
            ("query", true),
            ("zone_id", true),
            ("source", false),
            ("action", false),
        ],
    ),
    ("hifi_status", &[]),
    ("hifi_hqplayer_status", &[]),
    ("hifi_hqplayer_profiles", &[]),
    ("hifi_hqplayer_load_profile", &[("profile", true)]),
    (
        "hifi_hqplayer_set_pipeline",
        &[("setting", true), ("value", true)],
    ),
];

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn every_input_schema_is_structurally_valid_json_schema() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let result = app.list_tools().await;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");

    for tool in tools {
        let name = tool
            .get("name")
            .and_then(Value::as_str)
            .expect("tool must have a name");
        let schema = tool
            .get("inputSchema")
            .unwrap_or_else(|| panic!("{name}: missing inputSchema"));

        assert_eq!(
            schema.get("type").and_then(Value::as_str),
            Some("object"),
            "{name}: inputSchema.type must be \"object\""
        );

        let properties = schema
            .get("properties")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();

        for (prop, spec) in &properties {
            let spec = spec
                .as_object()
                .unwrap_or_else(|| panic!("{name}.{prop}: property spec must be an object"));
            // A property is well-formed if it constrains its value somehow.
            assert!(
                spec.contains_key("type")
                    || spec.contains_key("anyOf")
                    || spec.contains_key("oneOf")
                    || spec.contains_key("allOf")
                    || spec.contains_key("$ref")
                    || spec.contains_key("enum"),
                "{name}.{prop}: property must declare type/enum/$ref/anyOf/oneOf/allOf, got {spec:?}"
            );
            // Descriptions are what the model reads to fill the parameter in.
            assert!(
                spec.get("description")
                    .and_then(Value::as_str)
                    .is_some_and(|d| !d.trim().is_empty()),
                "{name}.{prop}: every parameter must carry a non-empty description"
            );
        }

        let required: Vec<&str> = schema
            .get("required")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for req in &required {
            assert!(
                properties.contains_key(*req),
                "{name}: required lists \"{req}\" which is not in properties"
            );
        }

        // Every documented parameter appears, with the documented optionality.
        let expected = EXPECTED_TOOL_PARAMS
            .iter()
            .find(|(t, _)| *t == name)
            .map(|(_, params)| *params)
            .unwrap_or_else(|| {
                panic!("{name}: not listed in EXPECTED_TOOL_PARAMS — add it deliberately")
            });

        let mut actual: Vec<&str> = properties.keys().map(String::as_str).collect();
        actual.sort_unstable();
        let mut expected_names: Vec<&str> = expected.iter().map(|(p, _)| *p).collect();
        expected_names.sort_unstable();
        assert_eq!(
            actual, expected_names,
            "{name}: input schema properties drifted from the documented parameter list"
        );

        for (param, is_required) in expected {
            assert_eq!(
                required.contains(param),
                *is_required,
                "{name}.{param}: required-ness drifted (schema required = {required:?})"
            );
        }
    }
}

/// Tool descriptions are documentation the model reads (AGENTS.md). An empty or
/// missing description makes a tool unusable.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn every_tool_has_a_nonempty_description() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let result = app.list_tools().await;
    let tools = result
        .get("tools")
        .and_then(Value::as_array)
        .expect("tools array");

    for tool in tools {
        let name = tool.get("name").and_then(Value::as_str).unwrap_or("<?>");
        assert!(
            tool.get("description")
                .and_then(Value::as_str)
                .is_some_and(|d| !d.trim().is_empty()),
            "{name}: tool description must be non-empty"
        );
    }
}

// =============================================================================
// 3b. Transport layer: session recovery, GET, DELETE
// =============================================================================
//
// Everything above POSTs with a valid session. That leaves three of the four
// public functions in src/mcp/mod.rs unexercised — including
// `auto_recover_session`, which is the most intricate code in the module and was
// moved verbatim by the split. It exists because real clients hold stale sessions
// across a server restart, so it is load-bearing rather than glue.

/// A POST carrying a session id the server has never heard of must still
/// succeed: `auto_recover_session` transparently mints a new session and rewrites
/// the request headers, so the client's request goes through instead of failing.
///
/// Without this test the split could have broken stale-session recovery and every
/// other test in this file would still pass.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn a_stale_session_id_is_transparently_recovered() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // Never initialized; this id exists nowhere in the session store.
    let (_, response) = app
        .post(
            Some("stale-session-from-a-previous-server-process"),
            json!({ "jsonrpc": "2.0", "id": 99, "method": "tools/list", "params": {} }),
        )
        .await;

    let response = response.expect("a stale session must still get a JSON-RPC response");
    assert!(
        response.get("error").is_none(),
        "stale session recovery must not surface an error to the client: {response}"
    );
    let tools = response
        .pointer("/result/tools")
        .and_then(Value::as_array)
        .unwrap_or_else(|| panic!("recovered request must return the tool list, got: {response}"));
    assert_eq!(
        tools.len(),
        10,
        "the recovered session must serve the same tool list as a fresh one"
    );
}

/// GET opens the server-to-client SSE stream; DELETE tears a session down. Both
/// are wired in `src/main.rs` and neither had any coverage. Smoked here so a
/// mis-wired route surfaces as a test failure rather than as a client that
/// silently cannot receive notifications.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn get_and_delete_are_wired_and_session_aware() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let (session_id, _) = app.initialize().await;

    // GET without a session is rejected rather than opening a stream.
    let (status, _) = app.request(Method::GET, None).await;
    assert!(
        status.is_client_error(),
        "GET without a session must be refused, got {status}"
    );

    // DELETE without a session likewise.
    let (status, _) = app.request(Method::DELETE, None).await;
    assert!(
        status.is_client_error(),
        "DELETE without a session must be refused, got {status}"
    );

    // DELETE with a live session terminates it.
    let (status, _) = app.request(Method::DELETE, Some(&session_id)).await;
    assert!(
        status.is_success(),
        "DELETE with a valid session must succeed, got {status}"
    );

    // An unknown session id is refused, and GET and DELETE refuse it
    // *differently*. Unlike POST, neither has an auto-recovery path.
    //
    // GET answering 500 rather than 404 is arguably wrong — a stale SSE
    // reconnect is a client-side condition, not a server fault — but it is
    // today's behavior, and #394 records rather than corrects. Recorded here so
    // that if a later issue fixes it, the change is visible instead of incidental.
    let (status, _) = app
        .request(Method::DELETE, Some("never-initialized-session"))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::NOT_FOUND,
        "DELETE with an unknown session returns 404 today"
    );

    let (status, _) = app
        .request(Method::GET, Some("never-initialized-session"))
        .await;
    assert_eq!(
        status,
        axum::http::StatusCode::INTERNAL_SERVER_ERROR,
        "GET with an unknown session returns 500 today (see the note above: \
         recorded, not endorsed)"
    );

    // The methods really are routed: axum answers an unrouted method with 405,
    // and PATCH is the control case.
    let (status, _) = app.request(Method::PATCH, Some(&session_id)).await;
    assert_eq!(
        status,
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        "PATCH is not wired, so 405 here is what proves the GET/DELETE statuses \
         above came from the handlers rather than from axum's router"
    );
}

// =============================================================================
// 4. Capabilities gate: what the server refuses today
// =============================================================================

/// `create_mcp_extension` declares only `tools`, and rust-mcp-sdk 0.8.3 gates
/// request dispatch on the *declared* capability before any handler runs
/// (`ServerCapabilities::can_handle_request`, which rejects all five resource
/// methods when `resources.is_none()`). So the resource methods are not merely
/// unimplemented — they return a concrete JSON-RPC error, and that error is part
/// of today's surface.
///
/// Pinned here so #397 shows up as a visible diff (refusal -> success) rather
/// than as a silent change from one behavior to another. The SDK gate keys only
/// on the *presence* of `resources`; it never inspects `subscribe` or
/// `listChanged`.
///
/// This test records current behavior. It does not endorse it, and #394 must not
/// declare a `resources` capability — that is #397's work.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn resource_methods_are_refused_while_only_tools_is_declared() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let (session_id, init) = app.initialize().await;

    // Precondition: the declared capability set contains tools and not resources.
    assert!(
        init.pointer("/capabilities/tools").is_some(),
        "tools capability must be declared: {init}"
    );
    assert!(
        init.pointer("/capabilities/resources").is_none(),
        "#394 must not declare a resources capability — that is #397's work: {init}"
    );

    for (id, method, params) in [
        (10, "resources/list", json!({})),
        (11, "resources/read", json!({ "uri": "hifi://zones" })),
        (12, "resources/templates/list", json!({})),
    ] {
        let (_, response) = app
            .post(
                Some(&session_id),
                json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
            )
            .await;
        let response =
            response.unwrap_or_else(|| panic!("{method} must return a JSON-RPC response"));

        let error = response
            .get("error")
            .unwrap_or_else(|| panic!("{method} must be refused today, got: {response}"));
        assert_eq!(
            error.get("code").and_then(Value::as_i64),
            Some(-32603),
            "{method}: expected the capability gate's internal-error code, got {error}"
        );
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        assert!(
            message.contains("does not support resources"),
            "{method}: the refusal must name the missing capability, got {message:?}"
        );
    }
}

// =============================================================================
// 5. Every dispatch arm, and the lookup tables tools/list cannot see
// =============================================================================
//
// `handle_call_tool_request` is one match with ten arms and three pure lookup
// tables inside it. None of that is visible in tools/list: inverting the sign of
// `volume_down`, or dropping the `filternx` alias, would leave every snapshot in
// this file green. So each arm is reached and its observable output pinned.
//
// Adapters are disconnected here on purpose. Error text is contract too: an MCP
// client reads these strings and decides what to do next, and they are exactly
// what a careless move of a match arm garbles.

/// A zone id no adapter knows, but which routes to Roon by today's rules.
const UNKNOWN_ROON_ZONE: &str = "roon:does-not-exist";

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_zones_returns_an_empty_array_with_no_zones() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(&app.call_tool("hifi_zones", json!({})).await);
    assert_eq!(text, "[]", "hifi_zones must return a JSON array");
}

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_now_playing_reports_unknown_zones_by_id() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(
        &app.call_tool("hifi_now_playing", json!({ "zone_id": UNKNOWN_ROON_ZONE }))
            .await,
    );
    assert_eq!(
        text,
        format!("Error: Zone not found: {UNKNOWN_ROON_ZONE}"),
        "the refusal must name the zone so a client can correct itself"
    );
}

/// `hifi_status` is one of only two tools whose full payload is deterministic
/// with no backend, so its shape is pinned exactly.
///
/// It is built with a `serde_json::json!` literal rather than a struct. That is
/// deliberate: `serde_json` without `preserve_order` serializes maps as
/// `BTreeMap` (alphabetical), while a struct serializes in declaration order.
/// Converting this response to a typed struct would reorder the keys in the text
/// a client receives, so the ordering is asserted as well as the content.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_status_shape_is_pinned() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(&app.call_tool("hifi_status", json!({})).await);
    let parsed: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("hifi_status must return JSON: {e}\n{text}"));

    assert_eq!(
        parsed,
        json!({
            "roon": { "connected": false, "core_name": null },
            "hqplayer": { "connected": false, "host": null }
        }),
        "hifi_status payload drifted"
    );
    assert!(
        text.find("\"hqplayer\"").unwrap_or(usize::MAX) < text.find("\"roon\"").unwrap_or(0),
        "hifi_status keys must stay alphabetical (BTreeMap order); a typed struct \
         would reorder them and change the text clients receive:\n{text}"
    );
}

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_hqplayer_status_shape_is_pinned() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(&app.call_tool("hifi_hqplayer_status", json!({})).await);
    let parsed: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("hifi_hqplayer_status must return JSON: {e}\n{text}"));

    assert_eq!(
        parsed,
        json!({ "connected": false, "host": null, "pipeline": null }),
        "hifi_hqplayer_status payload drifted"
    );
}

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_hqplayer_profiles_returns_an_array() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(&app.call_tool("hifi_hqplayer_profiles", json!({})).await);
    assert_eq!(
        text, "[]",
        "hifi_hqplayer_profiles must return a JSON array"
    );
}

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_hqplayer_load_profile_reports_failure_prefix() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(
        &app.call_tool(
            "hifi_hqplayer_load_profile",
            json!({ "profile": "4x-Sinc-L" }),
        )
        .await,
    );
    assert!(
        text.starts_with("Error: Failed to load profile: "),
        "unexpected load_profile failure text: {text:?}"
    );
}

/// The HQPlayer setting alias table (`filter1x|filter_1x`,
/// `filterNx|filter_nx|filternx`, `shaper|dither`, `rate|samplerate`, `mode`) is
/// a pure lookup inside one match arm, invisible to `tools/list`. Dropping an
/// alias while moving the arm turns a documented input into "Unknown setting" —
/// the AGENTS.md "no broken tools" failure, with a green snapshot suite. Every
/// alias is therefore checked individually.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hqplayer_set_pipeline_alias_table_is_pinned() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // Recognised aliases reach the adapter, which is disconnected, so they fail
    // with the per-setting failure prefix rather than "Unknown setting".
    for setting in [
        "mode",
        "filter1x",
        "filter_1x",
        "filterNx",
        "filter_nx",
        "filternx",
        "shaper",
        "dither",
    ] {
        let text = result_text(
            &app.call_tool(
                "hifi_hqplayer_set_pipeline",
                json!({ "setting": setting, "value": "poly-sinc-gauss-long" }),
            )
            .await,
        );
        assert!(
            text.starts_with(&format!("Error: Failed to set {setting}: ")),
            "alias {setting:?} must be recognised and reach the adapter, got {text:?}"
        );
    }

    // `rate` and `samplerate` are the only settings that parse their value.
    for setting in ["rate", "samplerate"] {
        let text = result_text(
            &app.call_tool(
                "hifi_hqplayer_set_pipeline",
                json!({ "setting": setting, "value": "96000" }),
            )
            .await,
        );
        assert!(
            text.starts_with(&format!("Error: Failed to set {setting}: ")),
            "alias {setting:?} must be recognised, got {text:?}"
        );

        let text = result_text(
            &app.call_tool(
                "hifi_hqplayer_set_pipeline",
                json!({ "setting": setting, "value": "not-a-number" }),
            )
            .await,
        );
        assert_eq!(
            text, "Error: Invalid rate value (expected Hz like 48000, 96000)",
            "{setting:?} must reject a non-numeric rate before reaching the adapter"
        );
    }

    // Anything else is refused, and the refusal lists the valid settings.
    let text = result_text(
        &app.call_tool(
            "hifi_hqplayer_set_pipeline",
            json!({ "setting": "oversampling", "value": "8x" }),
        )
        .await,
    );
    assert_eq!(
        text,
        "Error: Unknown setting: oversampling. Valid: mode, samplerate, filter1x, filterNx, shaper, dither",
        "the refusal must enumerate the valid settings"
    );
}

/// `hifi_control`'s volume handling: the required-value rule for `volume_set`,
/// the defaulted delta for `volume_up`/`volume_down`, and the fact that volume
/// uses a different routing rule from transport. The exact backend command each
/// action produces is pinned by the LMS mock round-trip below.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_control_volume_argument_handling_is_pinned() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // volume_set with no value is refused before any routing happens.
    let text = result_text(
        &app.call_tool(
            "hifi_control",
            json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": "volume_set" }),
        )
        .await,
    );
    assert_eq!(
        text, "Error: volume_set requires a value (0-100)",
        "volume_set without a value must be refused with its documented range"
    );

    // volume_up / volume_down do not require a value; they default the delta and
    // reach the volume path (Roon here), not the transport path.
    for action in ["volume_up", "volume_down"] {
        let text = result_text(
            &app.call_tool(
                "hifi_control",
                json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": action }),
            )
            .await,
        );
        assert_eq!(
            text, "Error: Volume error: Not connected to Roon",
            "{action} must default its delta and reach the Roon volume path"
        );
    }
}

/// `hifi_play` refuses `action='radio'` for LMS before touching the network. The
/// exact wording is contract: the model is expected to read it and retry with a
/// supported action.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_play_refuses_radio_for_lms() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(
        &app.call_tool(
            "hifi_play",
            json!({
                "query": "Kind of Blue",
                "zone_id": "lms:aa:bb:cc:dd:ee:ff",
                "action": "radio"
            }),
        )
        .await,
    );
    assert_eq!(
        text, "Error: Radio mode not supported for LMS. Use 'play' or 'queue'.",
        "the refusal must name the supported actions"
    );
}

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_search_and_hifi_play_report_errors_with_their_own_prefixes() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(
        &app.call_tool("hifi_search", json!({ "query": "Eagles" }))
            .await,
    );
    assert_eq!(
        text, "Error: Search error: Browse service not available - not connected to Roon",
        "hifi_search must prefix failures with 'Search error'"
    );

    let text = result_text(
        &app.call_tool(
            "hifi_play",
            json!({ "query": "Eagles", "zone_id": UNKNOWN_ROON_ZONE }),
        )
        .await,
    );
    assert_eq!(
        text, "Error: Play error: Browse service not available - not connected to Roon",
        "hifi_play must prefix failures with 'Play error'"
    );
}

// =============================================================================
// 6. Zone-prefix routing
// =============================================================================
//
// Each adapter refuses an unknown id with its own distinctive wording, so the
// error text identifies which adapter a call actually reached. That makes routing
// observable end to end, without mocks and without reading the source.

/// Transport routing: `lms:` / `openhome:` / `upnp:` go to their adapters and
/// EVERYTHING ELSE goes to Roon — including bare ids and unrecognised prefixes.
///
/// The Roon default is a known defect (#398). #394 freezes it rather than fixing
/// it, so a green run here means "unchanged", never "correct".
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn transport_routing_including_the_roon_default_is_pinned() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let cases: &[(&str, &str, &str)] = &[
        ("lms:aa:bb:cc:dd:ee:ff", "not configured", "explicit lms:"),
        (
            "openhome:abc",
            "Device not found: abc",
            "explicit openhome:",
        ),
        ("upnp:abc", "Renderer not found: abc", "explicit upnp:"),
        ("roon:abc", "Not connected to Roon", "explicit roon:"),
        // Today's defaults. Both belong to #398, not #394.
        ("1601a5d4bare", "Not connected to Roon", "bare id -> Roon"),
        (
            "sonos:abc",
            "Not connected to Roon",
            "unknown prefix -> Roon",
        ),
    ];

    for (zone_id, expected_fragment, label) in cases {
        let text = result_text(
            &app.call_tool(
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "play" }),
            )
            .await,
        );
        assert!(
            text.contains(expected_fragment),
            "{label} ({zone_id}): expected the error to contain {expected_fragment:?}, \
             which identifies the adapter reached; got {text:?}"
        );
    }
}

/// Volume routing is a *different* rule from transport routing, and the
/// difference is load-bearing: `sonos:abc` is refused for volume but routed to
/// Roon for transport. Unifying these into one predicate would change behavior.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn volume_routing_differs_from_transport_routing() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let volume_supported: &[(&str, &str)] = &[
        ("lms:aa:bb:cc:dd:ee:ff", "not configured"),
        ("roon:abc", "Not connected to Roon"),
        ("1601a5d4bare", "Not connected to Roon"),
    ];
    for (zone_id, expected_fragment) in volume_supported {
        let text = result_text(
            &app.call_tool(
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "volume_set", "value": 30 }),
            )
            .await,
        );
        assert!(
            text.contains(expected_fragment),
            "{zone_id}: volume must reach an adapter; got {text:?}"
        );
    }

    for zone_id in ["openhome:abc", "upnp:abc", "sonos:abc"] {
        let text = result_text(
            &app.call_tool(
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "volume_set", "value": 30 }),
            )
            .await,
        );
        assert_eq!(
            text, "Error: Volume control not supported for this zone type",
            "{zone_id}: volume must be refused, not silently routed"
        );
    }
}

/// Search and play route on `lms:` only; everything else, including an absent
/// `zone_id`, goes to Roon.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn search_and_play_routing_is_pinned() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // Roon's browse-backed paths word their refusal differently from transport's,
    // so match case-insensitively on the part that names the adapter.
    let roon = "not connected to roon";

    // hifi_search's zone_id is optional. Absent -> Roon.
    let text = result_text(&app.call_tool("hifi_search", json!({ "query": "q" })).await);
    assert!(
        text.to_lowercase().contains(roon),
        "an absent zone_id must route search to Roon; got {text:?}"
    );

    for (zone_id, expected_fragment) in [
        ("lms:aa:bb:cc:dd:ee:ff", "not configured"),
        ("openhome:abc", roon),
        ("upnp:abc", roon),
        ("sonos:abc", roon),
    ] {
        let text = result_text(
            &app.call_tool("hifi_search", json!({ "query": "q", "zone_id": zone_id }))
                .await,
        );
        assert!(
            text.to_lowercase().contains(expected_fragment),
            "search {zone_id}: expected {expected_fragment:?}, got {text:?}"
        );

        let text = result_text(
            &app.call_tool("hifi_play", json!({ "query": "q", "zone_id": zone_id }))
                .await,
        );
        assert!(
            text.to_lowercase().contains(expected_fragment),
            "play {zone_id}: expected {expected_fragment:?}, got {text:?}"
        );
    }
}

// =============================================================================
// 7. Round-trips against tests/mock_servers/
// =============================================================================

/// A `TestApp` wired to a live mock LMS server, with the aggregator running so
/// discovered players actually reach `hifi_zones`.
struct LmsHarness {
    app: TestApp,
    mock: MockLmsServer,
    lms: Arc<LmsAdapter>,
    player_id: &'static str,
    _settings: SettingsFixture,
    _aggregator: tokio::task::JoinHandle<()>,
}

impl LmsHarness {
    const PLAYER_ID: &'static str = "aa:bb:cc:dd:ee:ff";

    fn zone_id(&self) -> String {
        format!("lms:{}", self.player_id)
    }

    /// `SettingsFixture` points `UHC_CONFIG_DIR` at a fresh temp dir, so
    /// `LmsAdapter::configure`'s on-disk config is isolated per test too.
    async fn start() -> Self {
        let settings = SettingsFixture::with_hqplayer(true);

        let mock = MockLmsServer::start().await;
        mock.add_player(Self::PLAYER_ID, "Living Room").await;
        mock.set_mode(Self::PLAYER_ID, "pause").await;
        mock.set_volume(Self::PLAYER_ID, 42).await;
        mock.set_now_playing(Self::PLAYER_ID, "So What", "Miles Davis", "Kind of Blue")
            .await;

        let bus = create_bus();
        let lms = Arc::new(LmsAdapter::new(bus.clone()));
        lms.configure(
            mock.addr().ip().to_string(),
            Some(mock.addr().port()),
            None,
            None,
        )
        .await;

        let state = build_state_with_bus(bus, Some(lms.clone())).await;

        // The aggregator only learns about zones by consuming bus events.
        let aggregator = state.aggregator.clone();
        let aggregator_task = tokio::spawn(async move { aggregator.run().await });

        lms.start().await.expect("LMS adapter must start");

        let app = TestApp::with_state(state.clone());

        // Wait for the player to propagate: adapter connect -> ZoneDiscovered ->
        // aggregator.
        //
        // Budget is 100 x 100ms = 10s, against a local mock that normally responds
        // in well under a second. Generous on purpose, but it is a wall-clock
        // budget: if this ever flakes on a loaded machine, the propagation path is
        // the thing to instrument, not the loop count to raise.
        let zone_id = format!("lms:{}", Self::PLAYER_ID);
        let mut found = false;
        for _ in 0..100 {
            if state.aggregator.get_zone(&zone_id).await.is_some() {
                found = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(
            found,
            "LMS player never reached the aggregator as {zone_id}"
        );

        Self {
            app,
            mock,
            lms,
            player_id: Self::PLAYER_ID,
            _settings: settings,
            _aggregator: aggregator_task,
        }
    }

    /// Stop the adapter before dropping the fixture. Without this its polling
    /// task outlives the test and can write `lms-config.json` into a `TempDir`
    /// that `SettingsFixture` has already removed.
    async fn stop(self) {
        self.lms.stop().await;
        self._aggregator.abort();
        self.mock.stop().await;
    }
}

/// LMS round-trip: a real mock server, a real adapter, real bus events, and the
/// MCP tool call driven over the real `/mcp` route.
///
/// This also pins `hifi_control`'s action map at the point where it is actually
/// observable — the backend command the mock receives. `playpause` must become
/// LMS `pause` (a toggle) and not `play`; `previous` and `prev` must both become
/// `playlist index -1`. `tools/list` says nothing about any of this.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn lms_round_trip_pins_the_action_map_and_the_volume_sign() {
    let h = LmsHarness::start().await;
    let zone_id = h.zone_id();

    // hifi_zones sees the discovered player.
    let text = result_text(&h.app.call_tool("hifi_zones", json!({})).await);
    let zones: Value = serde_json::from_str(&text).expect("hifi_zones must return JSON");
    let zone = zones
        .as_array()
        .and_then(|z| z.iter().find(|z| z.get("zone_id") == Some(&json!(zone_id))))
        .unwrap_or_else(|| panic!("hifi_zones must include {zone_id}: {text}"));
    assert_eq!(zone.get("zone_name"), Some(&json!("Living Room")));
    assert_eq!(zone.get("volume"), Some(&json!(42.0)));

    // hifi_now_playing sees the track.
    let text = result_text(
        &h.app
            .call_tool("hifi_now_playing", json!({ "zone_id": zone_id }))
            .await,
    );
    let np: Value = serde_json::from_str(&text).expect("hifi_now_playing must return JSON");
    assert_eq!(np.get("title"), Some(&json!("So What")));
    assert_eq!(np.get("artist"), Some(&json!("Miles Davis")));
    assert_eq!(np.get("album"), Some(&json!("Kind of Blue")));

    // The MCP action -> backend command map, observed on the wire.
    let expected: &[(&str, Value, &[&str])] = &[
        ("play", Value::Null, &["play"]),
        ("pause", Value::Null, &["pause"]),
        // playpause maps to backend "play_pause", which LMS expresses as a bare
        // "pause" toggle. Mapping it to "play" instead would break the toggle.
        ("playpause", Value::Null, &["pause"]),
        ("next", Value::Null, &["playlist", "index", "+1"]),
        ("previous", Value::Null, &["playlist", "index", "-1"]),
        ("prev", Value::Null, &["playlist", "index", "-1"]),
        // Absolute volume.
        ("volume_set", json!(30), &["mixer", "volume", "30"]),
        // Relative volume, and the sign that no snapshot can see.
        ("volume_up", json!(7), &["mixer", "volume", "+7"]),
        ("volume_down", json!(7), &["mixer", "volume", "-7"]),
        // Defaulted delta of 5.
        ("volume_up", Value::Null, &["mixer", "volume", "+5"]),
        ("volume_down", Value::Null, &["mixer", "volume", "-5"]),
    ];

    for (action, value, expected_command) in expected {
        h.mock.clear_commands().await;

        let mut args = json!({ "zone_id": zone_id, "action": action });
        if !value.is_null() {
            args["value"] = value.clone();
        }
        let text = result_text(&h.app.call_tool("hifi_control", args).await);
        assert!(
            !text.starts_with("Error:"),
            "hifi_control {action} failed: {text}"
        );

        let commands = h.mock.write_commands(h.player_id).await;
        let expected_command: Vec<String> =
            expected_command.iter().map(|s| s.to_string()).collect();
        assert!(
            commands.contains(&expected_command),
            "hifi_control action={action:?} value={value} must send {expected_command:?}; \
             mock received {commands:?}"
        );
    }

    h.stop().await;
}

/// `hifi_control` returns the action name plus the zone's post-command state.
/// The prose framing is what a model reads back to the user.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn lms_control_result_reports_action_and_current_state() {
    let h = LmsHarness::start().await;
    let zone_id = h.zone_id();

    let text = result_text(
        &h.app
            .call_tool(
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "play" }),
            )
            .await,
    );

    assert!(
        text.starts_with("Action 'play' executed.\n\nCurrent state:\n{"),
        "hifi_control must report the action and then the zone state: {text:?}"
    );
    let state_json = text
        .split_once("Current state:\n")
        .map(|(_, json)| json)
        .expect("current state block");
    let parsed: Value = serde_json::from_str(state_json)
        .unwrap_or_else(|e| panic!("state block must be JSON: {e}\n{state_json}"));
    assert_eq!(parsed.get("zone_id"), Some(&json!(zone_id)));

    h.stop().await;
}

/// HQPlayer round-trip against the mock's TCP XML protocol: `hifi_hqplayer_status`
/// must report the live connection rather than the disconnected default.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hqplayer_round_trip_reports_a_live_connection() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let mock = MockHqpServer::start().await;

    let state = build_state(None).await;
    state
        .hqplayer
        .configure(
            mock.addr().ip().to_string(),
            Some(mock.addr().port()),
            None,
            None,
            None,
        )
        .await;
    state
        .hqplayer
        .connect()
        .await
        .expect("HQPlayer must connect to the mock");

    let app = TestApp::with_state(state.clone());

    let mut connected = false;
    for _ in 0..50 {
        if state.hqplayer.get_status().await.connected {
            connected = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    assert!(connected, "HQPlayer adapter never connected to the mock");

    let text = result_text(&app.call_tool("hifi_hqplayer_status", json!({})).await);
    let parsed: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("hifi_hqplayer_status must return JSON: {e}\n{text}"));
    assert_eq!(
        parsed.get("connected"),
        Some(&json!(true)),
        "hifi_hqplayer_status must report the live connection: {text}"
    );
    assert_eq!(
        parsed.get("host"),
        Some(&json!(mock.addr().ip().to_string())),
        "hifi_hqplayer_status must report the configured host: {text}"
    );

    mock.stop().await;
}

/// OpenHome and UPnP have no `configure()` — they are SSDP-discovery only, so a
/// mock cannot be injected without a discovery seam that #394 has no mandate to
/// add. What is verifiable today, and what the refactor could break, is that
/// their prefixes reach *their* adapters: the mock devices answer SSDP-shaped
/// HTTP, and each adapter refuses an unknown id with its own distinctive wording.
///
/// So this asserts the routing edge (already covered above) *and* that the mock
/// devices are reachable, documenting the gap rather than papering over it.
/// Closing it properly belongs with whichever issue first needs OpenHome/UPnP
/// content operations (#399 / #400).
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn openhome_and_upnp_prefixes_reach_their_own_adapters() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let openhome_mock = MockOpenHomeDevice::start().await;
    let upnp_mock = MockUpnpRenderer::start().await;

    // The mock devices are live and serving their descriptions.
    let client = reqwest::Client::new();
    for url in [openhome_mock.description_url(), upnp_mock.description_url()] {
        let body = client
            .get(&url)
            .send()
            .await
            .expect("mock device must respond")
            .text()
            .await
            .expect("mock device body");
        assert!(
            body.contains("MediaRenderer") || body.contains("av-openhome-org"),
            "unexpected device description at {url}: {body}"
        );
    }

    let app = TestApp::new().await;

    // Routing: each prefix reaches its own adapter, identified by the adapter's
    // own refusal wording. `Device not found` is OpenHome's; `Renderer not found`
    // is UPnP's. A mis-wired refactor would surface Roon's wording instead.
    let text = result_text(
        &app.call_tool(
            "hifi_control",
            json!({ "zone_id": "openhome:mock-uuid", "action": "play" }),
        )
        .await,
    );
    assert_eq!(text, "Error: Control error: Device not found: mock-uuid");

    let text = result_text(
        &app.call_tool(
            "hifi_control",
            json!({ "zone_id": "upnp:mock-uuid", "action": "play" }),
        )
        .await,
    );
    assert_eq!(text, "Error: Control error: Renderer not found: mock-uuid");

    openhome_mock.stop().await;
    upnp_mock.stop().await;
}

// =============================================================================
// 8. No orphaned fields
// =============================================================================
//
// AGENTS.md: "No orphaned fields - Don't return data (like item_key) that can't
// be used by any tool." This encodes that guardrail as a test.
//
// Every field name any tool returns must appear below, classified as either
// consumed by some tool's input or explicitly display-only WITH A REASON. The
// inventory is read from real tool responses, so adding a field to a response
// type fails this test until it is classified.

/// Why a returned field is not an input to any tool.
#[derive(Debug, Clone, Copy, PartialEq)]
enum FieldRole {
    /// Consumed by the named tool parameter — the field closes a loop.
    Consumed(&'static str),
    /// Deliberately display-only. The reason is required, not decorative: a bare
    /// allowlist reads as approval, and at least one entry here is a known defect
    /// rather than a design choice.
    DisplayOnly(&'static str),
}

use FieldRole::{Consumed, DisplayOnly};

/// Today's truth, including its defects. `#394` freezes this; it fixes nothing.
const FIELD_ROLES: &[(&str, FieldRole)] = &[
    // Zone identity closes the loop: every zone-scoped tool takes it.
    (
        "zone_id",
        Consumed("hifi_now_playing/hifi_control/hifi_play.zone_id"),
    ),
    (
        "zone_name",
        DisplayOnly("human label; every tool addresses a zone by zone_id, never by name"),
    ),
    (
        "state",
        DisplayOnly("playback state readout; hifi_control.action is the write path"),
    ),
    (
        "volume",
        DisplayOnly("current level readout; hifi_control.value is the write path"),
    ),
    (
        "is_muted",
        DisplayOnly("mute readout with no corresponding write: hifi_control has no mute action"),
    ),
    (
        "title",
        DisplayOnly(
            "hifi_search returns {title, subtitle} and no key, so the only route from \
         'found it' to 'playing it' is handing the title back to hifi_play, which \
         re-searches and takes the first match. That is #392's keystone finding and \
         #396's job — NOT an endorsed design. Listed to freeze today's behavior.",
        ),
    ),
    (
        "artist",
        DisplayOnly("now-playing readout; hifi_play takes a free-text query, not a field"),
    ),
    (
        "album",
        DisplayOnly("now-playing readout; hifi_play takes a free-text query, not a field"),
    ),
    (
        "subtitle",
        DisplayOnly("search-result label, same defect as `title`; see #396"),
    ),
    // hifi_status / hifi_hqplayer_status readouts.
    (
        "connected",
        DisplayOnly("boolean health readout; nothing takes a connection state as input"),
    ),
    (
        "host",
        DisplayOnly(
            "diagnostic address; configuring a host is an HTTP/settings concern, not an MCP tool",
        ),
    ),
    (
        "core_name",
        DisplayOnly("Roon core label for the operator; no tool selects a core"),
    ),
    (
        "roon",
        DisplayOnly("hifi_status grouping key, not a value a client passes anywhere"),
    ),
    (
        "hqplayer",
        DisplayOnly("hifi_status grouping key, not a value a client passes anywhere"),
    ),
    (
        "pipeline",
        DisplayOnly("hifi_hqplayer_status grouping key wrapping the pipeline readout"),
    ),
    (
        "filter",
        DisplayOnly(
            "pipeline readout; hifi_hqplayer_set_pipeline writes it via \
         setting='filter1x'/'filterNx', which is a different name",
        ),
    ),
    (
        "shaper",
        Consumed("hifi_hqplayer_set_pipeline.setting='shaper'"),
    ),
    (
        "rate",
        Consumed("hifi_hqplayer_set_pipeline.setting='rate'"),
    ),
    // -------------------------------------------------------------------------
    // The #395 envelope, carried on `structuredContent`.
    //
    // These are classified here rather than exempted because the guardrail's rule
    // — nothing is returned that a client cannot act on — applies to the
    // structured payload at least as much as to the text. See the extension in
    // `no_tool_returns_an_unclassified_field`.
    // -------------------------------------------------------------------------
    (
        "schema",
        DisplayOnly(
            "envelope version marker (uhc.mcp.envelope/N). A client branches on it to \
         decide how to read the rest; it is not passed back to any tool. The bump \
         rule lives in src/mcp/envelope.rs.",
        ),
    ),
    (
        "outcome",
        DisplayOnly(
            "ok/accepted/unsupported/invalid/error. The whole point of #221: a model \
         branches on it instead of guessing from prose. Not an input anywhere.",
        ),
    ),
    (
        "tool",
        DisplayOnly(
            "echoes which tool answered, so a client correlating several calls (or #397 \
         reading a resource with no request to pair with) does not have to track it",
        ),
    ),
    (
        "operation",
        DisplayOnly(
            "the server's normalized name for what it did — `prev` reported as \
         `previous`. Not a tool input: hifi_control.action is the write path, and \
         this is its resolution.",
        ),
    ),
    (
        "params",
        DisplayOnly(
            "wrapper for the resolved parameters. Its KEYS are always declared inputs \
         of the tool (asserted by envelope_params_use_only_declared_tool_parameters), \
         so everything inside it closes a loop even though the wrapper itself does not.",
        ),
    ),
    (
        "scope",
        DisplayOnly("wrapper for what the call acted on: provider, zone_id, zone_name"),
    ),
    (
        "provider",
        DisplayOnly(
            "which adapter the call was routed to. A client cannot set it — routing is \
         derived from the zone id prefix — but it is what makes a capability refusal \
         checkable rather than a shrug. #398 consumes this concept as data.",
        ),
    ),
    (
        "observed",
        DisplayOnly("wrapper for state read back after a write"),
    ),
    (
        "read_from",
        DisplayOnly(
            "provenance of the read-back, `aggregator` today. Named read_from rather \
         than source so it does not collide with hifi_search.source in this table. \
         #400 adds adapter-sourced queue reads.",
        ),
    ),
    (
        "as_of_ms",
        DisplayOnly(
            "when the aggregator last updated this zone. Present so a client can judge \
         staleness itself, because #395 forbids claiming verification the adapters \
         cannot do — this is the honest alternative to a `verified` flag.",
        ),
    ),
    (
        "zone",
        DisplayOnly(
            "wrapper holding the read-back zone; its fields are the same now-playing \
         readout hifi_now_playing returns",
        ),
    ),
    ("refusal", DisplayOnly("wrapper for why a call was refused")),
    (
        "reason",
        DisplayOnly(
            "provider_limitation / not_implemented / invalid_parameter / unknown_target \
         / backend_error. Each implies a different client action, which is the test \
         for whether a distinction earns its place.",
        ),
    ),
    (
        "detail",
        DisplayOnly(
            "the envelope's own sentence explaining the refusal. Deliberately NOT a \
         copy of the frozen prose: where the prose misleads (OpenHome volume) this \
         says the true thing.",
        ),
    ),
    (
        "alternatives",
        Consumed(
            "each entry is a callable tool invocation, e.g. \"hifi_play action=queue\" \
         (asserted by unsupported_refusals_name_the_operation_and_an_alternative)",
        ),
    ),
    (
        "tracked_by",
        DisplayOnly(
            "the UHC issue that will implement a not_implemented capability. A model \
         cannot act on it, but it is what makes \"not yet\" a claim an operator can \
         check rather than an excuse.",
        ),
    ),
    (
        "parameter",
        Consumed(
            "names the tool input the client must correct; asserted to be a parameter \
         the tool actually declares",
        ),
    ),
    (
        "accepted",
        Consumed("the values the named parameter accepts — the client resends with one"),
    ),
    (
        "discover_with",
        Consumed("names the tool that enumerates valid values, e.g. hifi_zones for zone_id"),
    ),
    (
        "data",
        DisplayOnly(
            "wrapper for the tool's own payload; the same JSON the human text carries. \
         #397 projects this verbatim as resource contents.",
        ),
    ),
    (
        "message",
        DisplayOnly(
            "hifi_play's adapter-authored prose, the only record of WHICH item matched, \
         because search results have no addressable identifier until #396. Prose, \
         not parsed — #396 replaces it with an opaque ref.",
        ),
    ),
    // The keys inside `params`. Every one is a tool input by construction — that
    // is what envelope_params_use_only_declared_tool_parameters enforces — so they
    // are all Consumed, naming the tools that take them.
    ("action", Consumed("hifi_control.action / hifi_play.action")),
    (
        "value",
        Consumed("hifi_control.value / hifi_hqplayer_set_pipeline.value"),
    ),
    ("query", Consumed("hifi_search.query / hifi_play.query")),
    ("source", Consumed("hifi_search.source / hifi_play.source")),
    ("profile", Consumed("hifi_hqplayer_load_profile.profile")),
    ("setting", Consumed("hifi_hqplayer_set_pipeline.setting")),
];

/// Collect every key name reachable in a JSON value, at any depth.
fn collect_keys(value: &Value, into: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            for (k, v) in map {
                into.insert(k.clone());
                collect_keys(v, into);
            }
        }
        Value::Array(items) => {
            for item in items {
                collect_keys(item, into);
            }
        }
        _ => {}
    }
}

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn no_tool_returns_an_unclassified_field() {
    let h = LmsHarness::start().await;
    let zone_id = h.zone_id();

    // Drive every tool that returns structured data, with a live LMS zone so the
    // zone and now-playing payloads are fully populated rather than empty.
    let structured_calls: Vec<(&str, Value)> = vec![
        ("hifi_zones", json!({})),
        ("hifi_now_playing", json!({ "zone_id": zone_id })),
        ("hifi_status", json!({})),
        ("hifi_hqplayer_status", json!({})),
    ];

    let mut returned = std::collections::BTreeSet::new();
    for (tool, args) in structured_calls {
        let result = h.app.call_tool(tool, args).await;
        let text = result_text(&result);
        let parsed: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{tool} must return JSON: {e}\n{text}"));
        collect_keys(&parsed, &mut returned);

        // #395 extension: the envelope is returned data too, so the guardrail
        // applies to it. Without this, every envelope field would be
        // unclassified and this test would still pass — the rule would silently
        // stop covering the newer half of the response.
        if let Some(envelope) = result.get("structuredContent") {
            collect_keys(envelope, &mut returned);
        }
    }

    // Envelope shapes the four calls above cannot reach: every refusal variant's
    // fields, every `params` key, and the write path's `observed`. Driven rather
    // than listed, so a renamed field is caught instead of quietly reclassified.
    for (tool, args) in [
        // One call per Refusal variant.
        ("hifi_now_playing", json!({ "zone_id": "roon:nope" })), // unknown_target
        (
            "hifi_control", // not_implemented
            json!({ "zone_id": "openhome:abc", "action": "volume_set", "value": 1 }),
        ),
        (
            "hifi_play", // provider_limitation
            json!({ "query": "q", "zone_id": "lms:aa:bb:cc:dd:ee:ff", "action": "radio" }),
        ),
        (
            "hifi_hqplayer_set_pipeline", // invalid_parameter
            json!({ "setting": "nope", "value": "x" }),
        ),
        // Remaining `params` keys: query + source (Roon route), profile.
        ("hifi_search", json!({ "query": "q" })),
        ("hifi_hqplayer_load_profile", json!({ "profile": "p" })),
        // A successful write, for `observed`.
        (
            "hifi_control",
            json!({ "zone_id": zone_id.clone(), "action": "play" }),
        ),
    ] {
        let result = h.app.call_tool(tool, args).await;
        if let Some(envelope) = result.get("structuredContent") {
            collect_keys(envelope, &mut returned);
        }
    }

    // Two response shapes cannot be reached over the wire here: hifi_search needs
    // a live music library, and McpPipelineStatus only serializes when HQPlayer is
    // connected with a running pipeline.
    //
    // Their fields are collected by serializing the production types rather than
    // by listing names in a comment. A hand-written list would have a hole exactly
    // where it matters most: renaming McpSearchResult::subtitle would leave
    // `subtitle` classified and the new name unnoticed — and McpSearchResult is
    // the struct #396 exists to change.
    collect_keys(
        &serde_json::to_value(McpSearchResult {
            title: String::new(),
            subtitle: None,
        })
        .expect("McpSearchResult must serialize"),
        &mut returned,
    );
    collect_keys(
        &serde_json::to_value(McpPipelineStatus {
            state: String::new(),
            filter: String::new(),
            shaper: String::new(),
            rate: 0,
        })
        .expect("McpPipelineStatus must serialize"),
        &mut returned,
    );
    // #395: hifi_play's success payload. Same reasoning — its success path needs a
    // live music library, which no mock provides, so serialize the production type
    // rather than trusting a comment.
    collect_keys(
        &serde_json::to_value(McpPlayResult {
            message: String::new(),
        })
        .expect("McpPlayResult must serialize"),
        &mut returned,
    );

    let unclassified: Vec<&String> = returned
        .iter()
        .filter(|f| !FIELD_ROLES.iter().any(|(name, _)| *name == f.as_str()))
        .collect();
    assert!(
        unclassified.is_empty(),
        "MCP tools return field(s) {unclassified:?} that are neither consumed by a \
         tool input nor classified as display-only.\n\n\
         AGENTS.md: \"No orphaned fields - Don't return data (like item_key) that \
         can't be used by any tool.\"\n\n\
         Add each one to FIELD_ROLES in tests/mcp_contract.rs, either as \
         Consumed(\"<tool>.<param>\") or as DisplayOnly with a real reason. If the \
         honest reason is \"a client cannot act on this\", the field is the bug."
    );

    // Every display-only justification must actually say something.
    //
    // This measures length, not content, and a proxy can be gamed by padding. It
    // is kept because it does real work — it caught "Roon core label" (exactly 15
    // characters) and forced the reasons to be rewritten into something a reader
    // can act on. If you are here because this failed, the fix is a better reason,
    // not a longer one.
    for (field, role) in FIELD_ROLES {
        if let DisplayOnly(reason) = role {
            assert!(
                reason.len() > 15,
                "{field}: display-only fields need a real reason, not {reason:?}"
            );
        }
    }

    // Guard against the table rotting into a list of fields nothing returns.
    let stale: Vec<&str> = FIELD_ROLES
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| !returned.contains(*name))
        .collect();
    assert!(
        stale.is_empty(),
        "FIELD_ROLES lists field(s) {stale:?} that no tool returns any more — \
         remove them so the table stays evidence rather than folklore"
    );

    h.stop().await;
}

// =============================================================================
// 9. The human-readable text of all 10 tools, pinned byte for byte (#395)
// =============================================================================
//
// Issue #395 adds a structured result envelope to every tool. Its hard
// constraint is that the envelope is *added* and the existing text is never
// substituted, so this section pins the text before any envelope code exists.
//
// The fixture below is generated at commit 06821cb — the tip of #394, with no
// envelope in the tree. `git log --follow tests/fixtures/mcp_tool_text.json`
// should therefore show exactly one commit, made before the envelope landed. A
// later commit touching it means the additive contract broke and the fixture was
// regenerated to hide it.
//
// Cases are chosen to be deterministic with every adapter disconnected: no
// hosts, ports, timestamps or discovery results appear in any expected string.
// LMS success text is pinned separately, against the mock server.

/// Every tool, with the calls whose text is deterministic offline.
///
/// Read as a coverage matrix: all 10 tools appear, and between them all four
/// refusal classes #395 must distinguish (provider cannot / not yet implemented
/// in UHC / this failed / your input was invalid) are represented.
const TOOL_TEXT_CASES: &[(&str, &str, fn() -> Value)] = &[
    // hifi_zones — empty read.
    ("hifi_zones/empty", "hifi_zones", || json!({})),
    // hifi_now_playing — the zone id names nothing.
    (
        "hifi_now_playing/unknown_zone",
        "hifi_now_playing",
        || json!({ "zone_id": UNKNOWN_ROON_ZONE }),
    ),
    // hifi_control — backend failure, all four routing targets.
    (
        "hifi_control/roon_disconnected",
        "hifi_control",
        || json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": "play" }),
    ),
    (
        "hifi_control/lms_unconfigured",
        "hifi_control",
        || json!({ "zone_id": "lms:aa:bb:cc:dd:ee:ff", "action": "play" }),
    ),
    (
        "hifi_control/openhome_unknown_device",
        "hifi_control",
        || json!({ "zone_id": "openhome:abc", "action": "play" }),
    ),
    (
        "hifi_control/upnp_unknown_renderer",
        "hifi_control",
        || json!({ "zone_id": "upnp:abc", "action": "play" }),
    ),
    // hifi_control — volume_set with no value.
    (
        "hifi_control/volume_set_without_value",
        "hifi_control",
        || json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": "volume_set" }),
    ),
    // hifi_control — volume refused for zone types the MCP volume path skips.
    (
        "hifi_control/volume_openhome",
        "hifi_control",
        || json!({ "zone_id": "openhome:abc", "action": "volume_set", "value": 30 }),
    ),
    (
        "hifi_control/volume_upnp",
        "hifi_control",
        || json!({ "zone_id": "upnp:abc", "action": "volume_set", "value": 30 }),
    ),
    // hifi_control — volume refused for a prefix that names no adapter at all.
    (
        "hifi_control/volume_unknown_prefix",
        "hifi_control",
        || json!({ "zone_id": "sonos:abc", "action": "volume_set", "value": 30 }),
    ),
    // hifi_control — relative volume, defaulted delta, reaching the Roon path.
    (
        "hifi_control/volume_up_defaulted",
        "hifi_control",
        || json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": "volume_up" }),
    ),
    // hifi_control — an action no adapter knows, passed straight through by
    // `other => other`.
    //
    // NOTE the label: the OpenHome adapter resolves the device *before* matching
    // the action, so offline this never reaches action dispatch and the text is
    // "Device not found". Calling this case "unknown_action" would be coverage
    // wearing a label it has not earned. What the pass-through actually does to
    // `operation` is asserted against the LMS mock instead
    // (`envelope_reports_normalized_actions_and_resolved_volume` and the LMS
    // round-trip), where a device does resolve.
    (
        "hifi_control/unknown_action_never_reaches_dispatch",
        "hifi_control",
        || json!({ "zone_id": "openhome:abc", "action": "frobnicate" }),
    ),
    // hifi_search — failure on both routes.
    (
        "hifi_search/roon_disconnected",
        "hifi_search",
        || json!({ "query": "Eagles" }),
    ),
    (
        "hifi_search/lms_unconfigured",
        "hifi_search",
        || json!({ "query": "Eagles", "zone_id": "lms:aa:bb:cc:dd:ee:ff" }),
    ),
    // hifi_play — LMS has no radio mode.
    (
        "hifi_play/lms_radio_unsupported",
        "hifi_play",
        || json!({ "query": "Kind of Blue", "zone_id": "lms:aa:bb:cc:dd:ee:ff", "action": "radio" }),
    ),
    (
        "hifi_play/roon_disconnected",
        "hifi_play",
        || json!({ "query": "Eagles", "zone_id": UNKNOWN_ROON_ZONE }),
    ),
    // hifi_status — deterministic read.
    ("hifi_status/disconnected", "hifi_status", || json!({})),
    // The four HQPlayer tools.
    (
        "hifi_hqplayer_status/disconnected",
        "hifi_hqplayer_status",
        || json!({}),
    ),
    (
        "hifi_hqplayer_profiles/empty",
        "hifi_hqplayer_profiles",
        || json!({}),
    ),
    (
        "hifi_hqplayer_load_profile/disconnected",
        "hifi_hqplayer_load_profile",
        || json!({ "profile": "4x-Sinc-L" }),
    ),
    (
        "hifi_hqplayer_set_pipeline/disconnected",
        "hifi_hqplayer_set_pipeline",
        || json!({ "setting": "mode", "value": "PCM" }),
    ),
    // hifi_hqplayer_set_pipeline — two distinct offending parameters.
    (
        "hifi_hqplayer_set_pipeline/unknown_setting",
        "hifi_hqplayer_set_pipeline",
        || json!({ "setting": "oversampling", "value": "8x" }),
    ),
    (
        "hifi_hqplayer_set_pipeline/invalid_rate",
        "hifi_hqplayer_set_pipeline",
        || json!({ "setting": "samplerate", "value": "not-a-number" }),
    ),
];

/// The exact text every tool returns, pinned against a fixture generated before
/// the envelope existed.
///
/// This is #395's central guarantee. A diff here means the envelope replaced
/// text rather than accompanying it — which is the bug, not a stale fixture.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn tool_text_is_byte_identical_to_the_pre_envelope_fixture() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let mut captured = serde_json::Map::new();
    for (label, tool, args) in TOOL_TEXT_CASES {
        let result = app.call_tool(tool, args()).await;
        captured.insert(label.to_string(), json!(result_text(&result)));
    }

    // Every tool must appear, or the fixture is a partial guarantee dressed up
    // as a total one.
    let covered: std::collections::BTreeSet<&str> =
        TOOL_TEXT_CASES.iter().map(|(_, tool, _)| *tool).collect();
    assert_eq!(
        covered.len(),
        10,
        "all 10 tools must have their text pinned; covered: {covered:?}"
    );

    assert_matches_fixture("mcp_tool_text.json", &Value::Object(captured));
}

/// Exactly one content block per tool result.
///
/// `result_text` in this file concatenates every text block with `\n`, so a
/// second block would change the text a client reads while every `starts_with`
/// assertion above stayed green. #395 chose `structuredContent` over a second
/// JSON text block precisely to avoid that; this asserts the choice rather than
/// trusting it.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn every_tool_result_has_exactly_one_content_block() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for (label, tool, args) in TOOL_TEXT_CASES {
        let result = app.call_tool(tool, args()).await;
        let content = result
            .get("content")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{label}: result must carry a content array: {result}"));
        assert_eq!(
            content.len(),
            1,
            "{label}: exactly one content block, or the human-readable text changes; \
             got {content:?}"
        );
        assert_eq!(
            content[0].get("type"),
            Some(&json!("text")),
            "{label}: the single content block must stay a text block"
        );
    }
}

// =============================================================================
// 10. The envelope itself (#395)
// =============================================================================
//
// The envelope rides `CallToolResult.structured_content` -> wire
// `structuredContent`. Section 9 proves the text did not change; this section
// proves something was actually added, and that what was added is honest.
//
// `EXPECTED_ENVELOPES` below is the coverage matrix. It is a table rather than a
// pile of individual tests so that a missing outcome or an unclassified refusal
// reason is a visible gap in one place.

/// The envelope a client receives from every case in [`TOOL_TEXT_CASES`].
///
/// `(label, expected outcome, expected refusal reason or None)`. Keyed by the same
/// labels as the text fixture, so the two tables describe the same calls and a
/// case cannot be pinned for text but not for structure.
const EXPECTED_ENVELOPES: &[(&str, &str, Option<&str>)] = &[
    ("hifi_zones/empty", "ok", None),
    (
        "hifi_now_playing/unknown_zone",
        "invalid",
        Some("unknown_target"),
    ),
    (
        "hifi_control/roon_disconnected",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_control/lms_unconfigured",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_control/openhome_unknown_device",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_control/upnp_unknown_renderer",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_control/volume_set_without_value",
        "invalid",
        Some("invalid_parameter"),
    ),
    // The two rows this issue exists for: one frozen string, two truths.
    (
        "hifi_control/volume_openhome",
        "unsupported",
        Some("not_implemented"),
    ),
    (
        "hifi_control/volume_upnp",
        "unsupported",
        Some("not_implemented"),
    ),
    (
        "hifi_control/volume_unknown_prefix",
        "invalid",
        Some("invalid_parameter"),
    ),
    (
        "hifi_control/volume_up_defaulted",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_control/unknown_action_never_reaches_dispatch",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_search/roon_disconnected",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_search/lms_unconfigured",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_play/lms_radio_unsupported",
        "unsupported",
        Some("provider_limitation"),
    ),
    (
        "hifi_play/roon_disconnected",
        "error",
        Some("backend_error"),
    ),
    ("hifi_status/disconnected", "ok", None),
    ("hifi_hqplayer_status/disconnected", "ok", None),
    ("hifi_hqplayer_profiles/empty", "ok", None),
    (
        "hifi_hqplayer_load_profile/disconnected",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_hqplayer_set_pipeline/disconnected",
        "error",
        Some("backend_error"),
    ),
    (
        "hifi_hqplayer_set_pipeline/unknown_setting",
        "invalid",
        Some("invalid_parameter"),
    ),
    (
        "hifi_hqplayer_set_pipeline/invalid_rate",
        "invalid",
        Some("invalid_parameter"),
    ),
];

/// Pull the envelope out of a `tools/call` result.
fn envelope(result: &Value, label: &str) -> Value {
    result
        .get("structuredContent")
        .cloned()
        .unwrap_or_else(|| panic!("{label}: every tool must attach an envelope: {result}"))
}

/// Drive every case once and hand back `(label, result)` pairs, so the tests
/// below share one pass over the surface instead of re-driving it each time.
async fn all_cases(app: &TestApp) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    for (label, tool, args) in TOOL_TEXT_CASES {
        out.push((*label, app.call_tool(tool, args()).await));
    }
    out
}

/// Every tool, on every path, carries an envelope with the right outcome and the
/// right refusal reason.
///
/// This is the acceptance criterion "one envelope type used by all tools" made
/// checkable: 10 tools, all four refusal classes, and every case's classification
/// stated rather than inferred.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn every_tool_returns_an_envelope_with_the_expected_outcome() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // The two tables must describe the same calls.
    let text_labels: std::collections::BTreeSet<&str> =
        TOOL_TEXT_CASES.iter().map(|(l, _, _)| *l).collect();
    let env_labels: std::collections::BTreeSet<&str> =
        EXPECTED_ENVELOPES.iter().map(|(l, _, _)| *l).collect();
    assert_eq!(
        text_labels, env_labels,
        "TOOL_TEXT_CASES and EXPECTED_ENVELOPES must cover the same calls"
    );

    for (label, result) in all_cases(&app).await {
        let env = envelope(&result, label);
        let (_, expected_outcome, expected_reason) = EXPECTED_ENVELOPES
            .iter()
            .find(|(l, _, _)| *l == label)
            .expect("labels checked above");

        assert_eq!(
            env.get("schema"),
            Some(&json!("uhc.mcp.envelope/1")),
            "{label}: the envelope must declare its schema"
        );
        assert_eq!(
            env.get("outcome"),
            Some(&json!(expected_outcome)),
            "{label}: unexpected outcome in {env}"
        );
        assert!(
            env.get("tool").and_then(Value::as_str).is_some(),
            "{label}: the envelope must name the tool"
        );
        assert!(
            env.get("operation").and_then(Value::as_str).is_some(),
            "{label}: the envelope must name the operation"
        );

        match expected_reason {
            Some(reason) => {
                let refusal = env
                    .get("refusal")
                    .unwrap_or_else(|| panic!("{label}: a refused outcome needs a refusal: {env}"));
                assert_eq!(
                    refusal.get("reason"),
                    Some(&json!(reason)),
                    "{label}: unexpected refusal reason in {refusal}"
                );
                let detail = refusal
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("{label}: every refusal needs a detail: {refusal}"));
                // A length proxy, with the same weakness FIELD_ROLES's own reason
                // check admits to: it can be gamed by padding. Kept because it
                // does real work catching a detail that is just the reason code
                // spelled out, but it is not a quality gate. If you are here
                // because this failed, write a better sentence, not a longer one.
                assert!(
                    detail.len() > 20,
                    "{label}: a refusal detail must explain itself, not restate the code: \
                     {detail:?}"
                );
            }
            None => assert!(
                env.get("refusal").is_none(),
                "{label}: a successful outcome must carry no refusal: {env}"
            ),
        }
    }
}

/// Outcome and refusal cannot contradict each other.
///
/// `Refusal::outcome()` is the single mapping and `Envelope::refuse` applies it,
/// so this is a check on the wire rather than on the type — it would catch a
/// hand-built envelope that bypassed the builder.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn refusal_presence_matches_the_outcome_on_the_wire() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for (label, result) in all_cases(&app).await {
        let env = envelope(&result, label);
        let outcome = env.get("outcome").and_then(Value::as_str).unwrap_or("");
        let refused = matches!(outcome, "unsupported" | "invalid" | "error");
        assert_eq!(
            env.get("refusal").is_some(),
            refused,
            "{label}: outcome {outcome:?} and refusal presence disagree: {env}"
        );

        // `verified` must never appear: #395 forbids claiming verification the
        // adapters cannot do, and the vocabulary has no way to spell it.
        assert!(
            matches!(
                outcome,
                "ok" | "accepted" | "unsupported" | "invalid" | "error"
            ),
            "{label}: unknown outcome {outcome:?}"
        );
    }
}

/// Every [`Refusal`] variant is exercised by at least one case.
///
/// Without this, a variant could exist in the type, be documented in the PR, and
/// never actually be produced — which is the "advertised but not wired" failure
/// AGENTS.md forbids for tools and which applies equally to a refusal vocabulary.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn every_refusal_reason_is_actually_produced() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let mut seen = std::collections::BTreeSet::new();
    for (label, result) in all_cases(&app).await {
        if let Some(reason) = envelope(&result, label)
            .get("refusal")
            .and_then(|r| r.get("reason"))
            .and_then(Value::as_str)
        {
            seen.insert(reason.to_string());
        }
    }

    for reason in [
        "provider_limitation",
        "not_implemented",
        "invalid_parameter",
        "unknown_target",
        "backend_error",
    ] {
        assert!(
            seen.contains(reason),
            "refusal reason {reason:?} is defined but never produced; seen: {seen:?}"
        );
    }
}

/// `data` and the human text say the same thing.
///
/// **Parsed-value equality, not byte equality.** The text is rendered from the
/// payload struct (declaration order) and `data` from `serde_json::to_value`
/// (a `BTreeMap`, so alphabetical). Asserting strings here would fail, and
/// "fixing" it by rendering the text from the `Value` would reorder the keys of
/// four tools — which is exactly the change #395 promises not to make.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn envelope_data_parses_equal_to_the_text_payload() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // The tools whose human text *is* JSON.
    let json_text_cases = [
        "hifi_zones/empty",
        "hifi_status/disconnected",
        "hifi_hqplayer_status/disconnected",
        "hifi_hqplayer_profiles/empty",
    ];

    let cases = all_cases(&app).await;
    for label in json_text_cases {
        let result = &cases
            .iter()
            .find(|(l, _)| *l == label)
            .unwrap_or_else(|| panic!("missing case {label}"))
            .1;
        let text = result_text(result);
        let from_text: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{label}: text must be JSON: {e}\n{text}"));
        let data = envelope(result, label)
            .get("data")
            .cloned()
            .unwrap_or_else(|| panic!("{label}: a JSON-text tool must set data"));
        assert_eq!(
            data, from_text,
            "{label}: data and the text payload must be the same JSON value"
        );
    }

    // And the tools whose text is prose must NOT invent a data block, except
    // hifi_play, whose adapter message is the only record of what matched.
    for label in [
        "hifi_control/roon_disconnected",
        "hifi_hqplayer_load_profile/disconnected",
    ] {
        let result = &cases
            .iter()
            .find(|(l, _)| *l == label)
            .unwrap_or_else(|| panic!("missing case {label}"))
            .1;
        assert!(
            envelope(result, label).get("data").is_none(),
            "{label}: a prose result must not fabricate a data block"
        );
    }
}

/// `params` keys are always a subset of the tool's own declared input parameters.
///
/// This is what turns a free-form map into a checked contract: a client never
/// meets a key it has not already seen in the tool's input schema. Values are
/// deliberately *resolved* rather than echoed, which is asserted separately.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn envelope_params_use_only_declared_tool_parameters() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for (label, result) in all_cases(&app).await {
        let env = envelope(&result, label);
        let tool = env
            .get("tool")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{label}: envelope must name the tool"));
        let declared: Vec<&str> = EXPECTED_TOOL_PARAMS
            .iter()
            .find(|(name, _)| *name == tool)
            .map(|(_, params)| params.iter().map(|(p, _)| *p).collect())
            .unwrap_or_else(|| panic!("{label}: {tool} is not in EXPECTED_TOOL_PARAMS"));

        let params = env
            .get("params")
            .and_then(Value::as_object)
            .unwrap_or_else(|| panic!("{label}: envelope must carry params: {env}"));
        for key in params.keys() {
            assert!(
                declared.contains(&key.as_str()),
                "{label}: params key {key:?} is not a declared input of {tool} \
                 (declared: {declared:?}). The envelope must not invent parameter names."
            );
        }
    }
}

/// The two lookup tables `tools/list` cannot see are now visible to a client.
///
/// `operation` reports the normalized action and `params.value` the resolved
/// delta, so `prev` -> `previous` and `volume_down` with no value -> `-5.0`.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn envelope_reports_normalized_actions_and_resolved_volume() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // Alias normalization: the client said `prev`, the server did `previous`.
    let result = app
        .call_tool(
            "hifi_control",
            json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": "prev" }),
        )
        .await;
    let env = envelope(&result, "prev");
    assert_eq!(
        env.get("operation"),
        Some(&json!("previous")),
        "`prev` must be reported as the normalized `previous`: {env}"
    );
    assert_eq!(
        env.get("params").and_then(|p| p.get("action")),
        Some(&json!("prev")),
        "params.action keeps the client's spelling; operation carries the resolution"
    );

    let result = app
        .call_tool(
            "hifi_control",
            json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": "playpause" }),
        )
        .await;
    assert_eq!(
        envelope(&result, "playpause").get("operation"),
        Some(&json!("play_pause")),
        "`playpause` must be reported as the backend's `play_pause`"
    );

    // The defaulted delta and the negation, neither of which a client could see
    // before.
    for (action, expected) in [("volume_up", 5.0), ("volume_down", -5.0)] {
        let result = app
            .call_tool(
                "hifi_control",
                json!({ "zone_id": UNKNOWN_ROON_ZONE, "action": action }),
            )
            .await;
        let env = envelope(&result, action);
        assert_eq!(
            env.get("operation"),
            Some(&json!("volume_relative")),
            "{action}: relative volume must be reported as such: {env}"
        );
        assert_eq!(
            env.get("params").and_then(|p| p.get("value")),
            Some(&json!(expected)),
            "{action}: params.value must report the resolved delta, sign included: {env}"
        );
    }

    // The HQPlayer alias table, same idea: `filternx` resolves to `filterNx`.
    let result = app
        .call_tool(
            "hifi_hqplayer_set_pipeline",
            json!({ "setting": "filternx", "value": "poly-sinc-gauss-long" }),
        )
        .await;
    assert_eq!(
        envelope(&result, "filternx")
            .get("params")
            .and_then(|p| p.get("setting")),
        Some(&json!("filterNx")),
        "the canonical setting name must be reported, teaching the alias table"
    );
}

/// An `unsupported` result names the operation and says what *is* available.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn unsupported_refusals_name_the_operation_and_an_alternative() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for (label, result) in all_cases(&app).await {
        let env = envelope(&result, label);
        if env.get("outcome") != Some(&json!("unsupported")) {
            continue;
        }
        let refusal = env.get("refusal").expect("checked elsewhere");

        assert!(
            refusal
                .get("operation")
                .and_then(Value::as_str)
                .is_some_and(|o| !o.is_empty()),
            "{label}: an unsupported refusal must name the operation: {refusal}"
        );
        let alternatives = refusal
            .get("alternatives")
            .and_then(Value::as_array)
            .unwrap_or_else(|| panic!("{label}: unsupported must list alternatives: {refusal}"));
        assert!(
            !alternatives.is_empty(),
            "{label}: #395 requires an unsupported result to say what IS available: {refusal}"
        );
        for alt in alternatives {
            let alt = alt.as_str().unwrap_or("");
            assert!(
                alt.starts_with("hifi_"),
                "{label}: an alternative must be a callable tool invocation, got {alt:?}"
            );
            // One invocation per entry. A comma-joined list reads as guidance and
            // is not callable verbatim, which defeats the point of the field.
            assert!(
                !alt.contains(", "),
                "{label}: each alternative must be one callable invocation, not a \
                 comma-joined list: {alt:?}"
            );
        }

        // The provider must be identifiable, either from the scope or from the
        // refusal itself — an unsupported result that names no provider teaches
        // nothing.
        assert!(
            env.get("scope")
                .and_then(|s| s.get("provider"))
                .and_then(Value::as_str)
                .is_some_and(|p| p != "unknown"),
            "{label}: an unsupported refusal must name the provider: {env}"
        );
    }
}

/// An `invalid` result names the offending parameter and its accepted values.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn invalid_refusals_name_the_parameter_and_its_accepted_values() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for (label, result) in all_cases(&app).await {
        let env = envelope(&result, label);
        if env.get("outcome") != Some(&json!("invalid")) {
            continue;
        }
        let refusal = env.get("refusal").expect("checked elsewhere");

        let parameter = refusal
            .get("parameter")
            .and_then(Value::as_str)
            .unwrap_or_else(|| panic!("{label}: invalid must name the parameter: {refusal}"));

        // And it must be a parameter the tool actually declares, or the client is
        // being pointed at something it cannot set.
        let tool = env.get("tool").and_then(Value::as_str).unwrap_or("");
        let declared: Vec<&str> = EXPECTED_TOOL_PARAMS
            .iter()
            .find(|(name, _)| *name == tool)
            .map(|(_, params)| params.iter().map(|(p, _)| *p).collect())
            .unwrap_or_default();

        // `arguments` is the documented sentinel for "the argument object as a
        // whole", used only when a pre-dispatch deserializer message names no
        // declared parameter. It is not a tool input, and pretending otherwise
        // would be the dishonesty this test exists to prevent.
        if parameter != "arguments" {
            assert!(
                declared.contains(&parameter),
                "{label}: refusal blames {parameter:?}, which {tool} does not declare \
                 (declared: {declared:?})"
            );
        }

        // Either an accepted-values list, or a tool that enumerates them.
        let has_accepted = refusal
            .get("accepted")
            .and_then(Value::as_array)
            .is_some_and(|a| !a.is_empty());
        let has_discovery = refusal
            .get("discover_with")
            .and_then(Value::as_str)
            .is_some_and(|t| t.starts_with("hifi_"));
        assert!(
            has_accepted || has_discovery,
            "{label}: invalid must give accepted values or name a tool that lists \
             them: {refusal}"
        );
    }
}

/// OpenHome and UPnP volume is a UHC gap, not a provider limitation — and the
/// envelope says so even though the frozen prose does not.
///
/// Both adapters implement volume (`src/adapters/openhome.rs` and
/// `src/adapters/upnp.rs`, action `vol_abs`/`vol_rel`) and both expose it over
/// HTTP (`POST /openhome/control`, `POST /upnp/control`). Only the MCP volume
/// path declines to call it. Classifying that as `provider_limitation` is exactly
/// the mislabelling #392 rule 3 forbids, and it is what AGENTS.md's capability
/// matrix currently implies.
///
/// If #398 later proves a genuine provider limit for one of these, this test is
/// the place to change it — deliberately, with the evidence in the commit.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn openhome_and_upnp_volume_is_reported_as_a_uhc_gap_not_a_provider_limit() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for (zone_id, provider) in [("openhome:abc", "openhome"), ("upnp:abc", "upnp")] {
        let result = app
            .call_tool(
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "volume_set", "value": 30 }),
            )
            .await;

        // The text is untouched.
        assert_eq!(
            result_text(&result),
            "Error: Volume control not supported for this zone type",
            "{zone_id}: the frozen prose must not change"
        );

        let env = envelope(&result, zone_id);
        let refusal = env.get("refusal").expect("a refusal is expected");
        assert_eq!(
            refusal.get("reason"),
            Some(&json!("not_implemented")),
            "{zone_id}: the adapter implements volume, so this is UHC's gap: {refusal}"
        );
        assert_eq!(
            refusal.get("tracked_by"),
            Some(&json!("#398")),
            "{zone_id}: 'not yet' must name the issue that makes it checkable"
        );
        assert_eq!(
            env.get("scope").and_then(|s| s.get("provider")),
            Some(&json!(provider)),
            "{zone_id}: the scope must name the provider: {env}"
        );
    }
}

/// A zone id whose prefix names no adapter blames the zone id, not a provider.
///
/// Same frozen string as the case above, different envelope. UHC never
/// identified a provider for `sonos:abc`, so it cannot claim one lacks volume —
/// and telling a model "this zone does not support volume" would teach it never
/// to retry, when the actual fix is a valid zone id.
///
/// This is the one place in #395 where the envelope's outcome deliberately
/// contradicts the accompanying prose. #398 corrects the prose.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn unknown_zone_prefix_volume_blames_the_zone_id_not_the_provider() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let result = app
        .call_tool(
            "hifi_control",
            json!({ "zone_id": "sonos:abc", "action": "volume_set", "value": 30 }),
        )
        .await;

    assert_eq!(
        result_text(&result),
        "Error: Volume control not supported for this zone type",
        "the frozen prose must not change"
    );

    let env = envelope(&result, "sonos");
    assert_eq!(
        env.get("outcome"),
        Some(&json!("invalid")),
        "an unidentifiable provider is the client's zone id, not a provider limit: {env}"
    );
    let refusal = env.get("refusal").expect("a refusal is expected");
    assert_eq!(refusal.get("parameter"), Some(&json!("zone_id")));
    assert_eq!(
        refusal.get("accepted"),
        Some(&json!(["roon:", "lms:", "openhome:", "upnp:"])),
        "the refusal must enumerate the prefixes that name an adapter: {refusal}"
    );

    // And it must NOT claim anything about a provider.
    assert_eq!(
        env.get("scope").and_then(|s| s.get("provider")),
        Some(&json!("unknown")),
        "no provider was identified, so none may be named: {env}"
    );
}

/// An unrecognised zone prefix reports `provider: "unknown"`, never the adapter
/// the call was silently defaulted to.
///
/// `sonos:abc` classifies as `ZoneTarget::Unknown` and transport still sends it to
/// Roon — today's silent default, #398's to fix. An earlier draft of this PR
/// derived `scope.provider` from the *route*, so it reported `roon`, teaching a
/// model two false things: that a Sonos zone is a Roon zone, and that Roon is at
/// fault. #392 rule 3 forbids making a provider look responsible for a UHC
/// decision.
///
/// So `provider: "unknown"` beside a Roon-shaped failure detail is the readable
/// fingerprint of the default, rather than an authoritative-looking lie.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn an_unrecognised_prefix_never_claims_the_adapter_it_was_defaulted_to() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // Transport, search and play all default an unknown prefix to Roon.
    let cases: &[(&str, Value)] = &[
        (
            "hifi_control",
            json!({ "zone_id": "sonos:abc", "action": "play" }),
        ),
        (
            "hifi_search",
            json!({ "query": "q", "zone_id": "sonos:abc" }),
        ),
        ("hifi_play", json!({ "query": "q", "zone_id": "sonos:abc" })),
    ];

    for (tool, args) in cases {
        let result = app.call_tool(tool, args.clone()).await;
        let env = envelope(&result, tool);

        assert_eq!(
            env.get("scope").and_then(|s| s.get("provider")),
            Some(&json!("unknown")),
            "{tool}: an unidentified provider must not be reported as the adapter the \
             call was defaulted to: {env}"
        );

        // And the default is still happening — this test pins the honesty of the
        // report, not a fix. #398 owns the fix.
        let detail = env
            .get("refusal")
            .and_then(|r| r.get("detail"))
            .and_then(Value::as_str)
            .unwrap_or("");
        assert!(
            detail.to_lowercase().contains("roon"),
            "{tool}: the Roon default is expected to remain visible in the detail \
             until #398 removes it; got {detail:?}"
        );
    }

    // A bare id is a *documented* default rather than a silent one, so it does
    // report roon. Pinned to make the difference deliberate.
    let result = app
        .call_tool(
            "hifi_control",
            json!({ "zone_id": "bareid", "action": "play" }),
        )
        .await;
    assert_eq!(
        envelope(&result, "bareid")
            .get("scope")
            .and_then(|s| s.get("provider")),
        Some(&json!("roon")),
        "a bare id classifies as Roon by a rule stated in routing.rs, unlike an \
         unrecognised prefix"
    );
}

/// A call whose arguments do not deserialize still gets an envelope — and an
/// unknown tool name deliberately does not.
///
/// A required parameter left out is the commonest `invalid` a real client
/// produces, and it fails in `handle_call_tool_request` before any tool runs. The
/// SDK turns it into `content: [text], isError: true`, which a client cannot
/// distinguish from a tool's own result, so #395's guarantee has to cover it. The
/// execute-gate dissent blocked on this.
///
/// An unknown tool name is the other side of the line: there is no tool to scope
/// an envelope to, and inventing one would claim a surface that does not exist.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn argument_parse_failures_get_an_envelope_and_unknown_tools_do_not() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let (session_id, _) = app.initialize().await;

    async fn call(app: &TestApp, session_id: &str, id: i32, name: &str, arguments: Value) -> Value {
        let (_, response) = app
            .post(
                Some(session_id),
                json!({
                    "jsonrpc": "2.0", "id": id, "method": "tools/call",
                    "params": { "name": name, "arguments": arguments }
                }),
            )
            .await;
        response
            .and_then(|r| r.get("result").cloned())
            .unwrap_or_else(|| panic!("{name} must return a result"))
    }

    // A known tool, a required parameter omitted.
    let result = call(
        &app,
        &session_id,
        90,
        "hifi_control",
        json!({ "action": "play" }),
    )
    .await;
    assert_eq!(
        result_text(&result),
        "missing field `zone_id`",
        "the SDK's own text must be preserved byte for byte"
    );
    assert_eq!(
        result.get("isError"),
        Some(&json!(true)),
        "isError must survive: it is the only place the surface uses it"
    );
    let env = envelope(&result, "missing zone_id");
    // This is the one envelope built outside the tool modules (in
    // `crate::mcp::handler`), so it is the one whose `schema` presence is least
    // exercised — assert it here rather than relying on the 23 cases above.
    assert_eq!(
        env.get("schema"),
        Some(&json!("uhc.mcp.envelope/1")),
        "the pre-dispatch envelope must declare its schema like every other: {env}"
    );
    assert_eq!(env.get("tool"), Some(&json!("hifi_control")));
    assert_eq!(env.get("outcome"), Some(&json!("invalid")));
    let refusal = env.get("refusal").expect("a refusal is expected");
    assert_eq!(refusal.get("reason"), Some(&json!("invalid_parameter")));
    assert_eq!(
        refusal.get("parameter"),
        Some(&json!("zone_id")),
        "the offending parameter must be named, per #395's acceptance criteria: {refusal}"
    );

    // A wrong type, where the deserializer message may not name the field.
    let result = call(
        &app,
        &session_id,
        91,
        "hifi_control",
        json!({ "zone_id": "roon:x", "action": "play", "value": "loud" }),
    )
    .await;
    let env = envelope(&result, "bad value type");
    assert_eq!(env.get("outcome"), Some(&json!("invalid")));
    let parameter = env
        .get("refusal")
        .and_then(|r| r.get("parameter"))
        .and_then(Value::as_str)
        .unwrap_or("");
    assert!(
        parameter == "value" || parameter == "arguments",
        "either the parameter is identified, or the sentinel says it could not be — \
         never a guess: got {parameter:?}"
    );

    // An unknown tool name: text preserved, no envelope, on purpose.
    let result = call(&app, &session_id, 92, "hifi_frobnicate", json!({})).await;
    assert_eq!(result_text(&result), "Unknown tool: hifi_frobnicate");
    assert_eq!(result.get("isError"), Some(&json!(true)));
    assert!(
        result.get("structuredContent").is_none(),
        "an unknown tool has no tool to scope an envelope to; a present envelope here \
         would claim a surface that does not exist: {result}"
    );
}

/// `scope.zone_name` is read back from the aggregator, never echoed, and its
/// absence next to a present `zone_id` is how a client tells a typo from a real
/// zone.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn scope_zone_name_is_absent_for_a_zone_the_aggregator_does_not_hold() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let result = app
        .call_tool("hifi_now_playing", json!({ "zone_id": UNKNOWN_ROON_ZONE }))
        .await;
    let scope = envelope(&result, "unknown zone")
        .get("scope")
        .cloned()
        .expect("a zone-scoped tool must carry a scope");
    assert_eq!(scope.get("zone_id"), Some(&json!(UNKNOWN_ROON_ZONE)));
    assert!(
        scope.get("zone_name").is_none(),
        "an unknown zone must have no name invented for it: {scope}"
    );
}

// =============================================================================
// 11. The envelope against a live backend (#395)
// =============================================================================
//
// Everything above runs with adapters disconnected, so it covers refusals and
// empty reads. The success path — `accepted`, plus observed state actually read
// back from the aggregator — needs a real backend, and `MockLmsServer` is the one
// mock a full round-trip can be driven through (see #394's notes on why Roon and
// OpenHome/UPnP cannot be).

/// A write reports `accepted`, never `ok`, and carries a read-back from the
/// aggregator.
///
/// The two halves of #221 in one assertion: the model is told the command was
/// accepted (so it stops retrying) and is shown the state (so it does not need a
/// follow-up call), with `as_of_ms` so it can judge staleness rather than being
/// handed a conclusion the server cannot support.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn lms_write_reports_accepted_with_state_read_back_from_the_aggregator() {
    let h = LmsHarness::start().await;
    let zone_id = h.zone_id();

    let result = h
        .app
        .call_tool(
            "hifi_control",
            json!({ "zone_id": zone_id, "action": "play" }),
        )
        .await;
    let env = envelope(&result, "lms play");

    assert_eq!(
        env.get("outcome"),
        Some(&json!("accepted")),
        "a write must report accepted — nothing here confirms the effect: {env}"
    );
    assert!(
        env.get("refusal").is_none(),
        "an accepted write carries no refusal: {env}"
    );

    // Scope is resolved from the aggregator, not echoed.
    let scope = env.get("scope").expect("zone-scoped tool needs a scope");
    assert_eq!(scope.get("provider"), Some(&json!("lms")));
    assert_eq!(scope.get("zone_id"), Some(&json!(zone_id)));
    assert_eq!(
        scope.get("zone_name"),
        Some(&json!("Living Room")),
        "zone_name must be read back from the aggregator, not from the request: {scope}"
    );

    let observed = env
        .get("observed")
        .expect("a write against a known zone must read state back");
    assert_eq!(
        observed.get("read_from"),
        Some(&json!("aggregator")),
        "AGENTS.md: the aggregator owns zone state"
    );
    assert!(
        observed
            .get("as_of_ms")
            .and_then(Value::as_u64)
            .is_some_and(|t| t > 0),
        "the snapshot must carry its own timestamp so staleness is the client's \
         call, not a claim of ours: {observed}"
    );

    // And the read-back is the same zone the text's state block describes —
    // compared as parsed JSON, because the text keeps declaration order while
    // `observed.zone` goes through serde_json's BTreeMap.
    let text = result_text(&result);
    let state_json = text
        .split_once("Current state:\n")
        .map(|(_, json)| json)
        .expect("the text must still embed its state block");
    let from_text: Value =
        serde_json::from_str(state_json).expect("the embedded state block must be JSON");
    assert_eq!(
        observed.get("zone"),
        Some(&from_text),
        "observed.zone and the text's state block must be the same JSON value"
    );
    assert_eq!(from_text.get("zone_id"), Some(&json!(zone_id)));

    h.stop().await;
}

/// A volume write reports the resolved level and reads the zone back.
///
/// `"Volume set"` is #221's other example of prose that states nothing. The
/// envelope turns it into: accepted, on this zone, at this level, with a zone
/// snapshot attached.
///
/// # What this deliberately does not assert
///
/// It does not check `observed.zone.volume` against the level just set. LMS state
/// reaches the aggregator by polling, so the snapshot may well predate the
/// command — asserting the new level would be asserting the verification #395
/// forbids inventing, and would make this test flaky the moment poll timing
/// shifted. `as_of_ms` is there so the client draws that conclusion itself.
///
/// An earlier name for this test claimed "the resulting level", which the body
/// never checked. That is the same mislabelled-coverage defect the design gate's
/// dissent found elsewhere, so the name now says what the body does.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn lms_volume_write_reports_the_resolved_level_and_reads_the_zone_back() {
    let h = LmsHarness::start().await;
    let zone_id = h.zone_id();

    let result = h
        .app
        .call_tool(
            "hifi_control",
            json!({ "zone_id": zone_id, "action": "volume_set", "value": 55 }),
        )
        .await;

    // Text unchanged.
    assert_eq!(result_text(&result), "Volume set");

    let env = envelope(&result, "lms volume_set");
    assert_eq!(env.get("outcome"), Some(&json!("accepted")));
    assert_eq!(env.get("operation"), Some(&json!("volume_absolute")));
    assert_eq!(
        env.get("params").and_then(|p| p.get("value")),
        Some(&json!(55.0))
    );
    assert_eq!(
        env.get("scope").and_then(|s| s.get("provider")),
        Some(&json!("lms"))
    );
    assert!(
        env.get("observed").is_some(),
        "the volume path must read state back too, not just transport: {env}"
    );

    h.stop().await;
}

/// `params` reports what the server *resolved*, which means it drops a parameter
/// the server discarded.
///
/// `hifi_search` on an LMS zone throws `source` away — globalsearch covers every
/// installed provider, so there is nothing to apply it to. Echoing it back would
/// claim UHC honored it. Omitting it is right, but silent, so it is pinned here:
/// the omission is a decision a reader can find, not an accident. The Roon route
/// reports the resolved source for the same reason in the opposite direction —
/// an unrecognised `source` is quietly read as `library`, and reporting the
/// resolution is the only way a client learns that.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn params_omits_a_parameter_the_server_discarded_and_reports_one_it_resolved() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // LMS route: `source` sent, discarded, and therefore absent.
    let result = app
        .call_tool(
            "hifi_search",
            json!({ "query": "q", "zone_id": "lms:aa:bb:cc:dd:ee:ff", "source": "tidal" }),
        )
        .await;
    let params = envelope(&result, "lms search")
        .get("params")
        .cloned()
        .expect("params must be present");
    assert!(
        params.get("source").is_none(),
        "LMS discards `source`; reporting it would claim the server honored it: {params}"
    );

    // Roon route: an unrecognised `source` silently becomes `library`, and the
    // envelope says so.
    let result = app
        .call_tool("hifi_search", json!({ "query": "q", "source": "spotify" }))
        .await;
    assert_eq!(
        envelope(&result, "roon search")
            .get("params")
            .and_then(|p| p.get("source")),
        Some(&json!("library")),
        "an unrecognised source falls back to library, and the client must be able \
         to see that happened"
    );
}

/// A fully-populated multi-tool response survives the SSE framing.
///
/// The envelope roughly doubles response size, and `parse_sse_json` returns only
/// the **first** `data:` line — so a payload split across frames would be silently
/// truncated and every assertion above would still pass on the fragment. Reasoning
/// says JSON escapes newlines and the frame stays single-line; this drives real
/// populated payloads through the real route and checks it.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn populated_payloads_survive_sse_framing() {
    let h = LmsHarness::start().await;
    let zone_id = h.zone_id();

    for (tool, args) in [
        ("hifi_zones", json!({})),
        ("hifi_now_playing", json!({ "zone_id": zone_id })),
        (
            "hifi_control",
            json!({ "zone_id": zone_id, "action": "play" }),
        ),
    ] {
        let result = h.app.call_tool(tool, args).await;
        let env = envelope(&result, tool);

        // A truncated frame would fail to parse as JSON-RPC long before here, so
        // reaching this point with a well-formed envelope is the check. Assert the
        // last-serialized field is present too, since truncation loses the tail.
        assert!(
            env.get("schema").is_some() && env.get("outcome").is_some(),
            "{tool}: envelope head missing: {env}"
        );
        let has_tail = env.get("data").is_some() || env.get("observed").is_some();
        assert!(
            has_tail,
            "{tool}: the envelope's trailing fields are missing, which is what a \
             truncated SSE frame would look like: {env}"
        );

        // And the text is still exactly one block at full size.
        assert_eq!(
            result
                .get("content")
                .and_then(Value::as_array)
                .map(Vec::len)
                .unwrap_or(0),
            1,
            "{tool}: still exactly one content block"
        );
    }

    h.stop().await;
}

/// `hifi_zones` with a real zone: `data` and the text agree, and the text keeps
/// declaration order.
///
/// This is the case the offline fixture cannot cover — `hifi_zones` returns `[]`
/// with no backend, so the key-order trap is invisible there. `McpZone`'s
/// declaration order is `zone_id, zone_name, state, volume, is_muted`, which is
/// *not* alphabetical, so rendering the text from `serde_json::to_value` would
/// change these bytes.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn populated_zone_payload_keeps_declaration_order_in_the_text() {
    let h = LmsHarness::start().await;

    let result = h.app.call_tool("hifi_zones", json!({})).await;
    let text = result_text(&result);

    let zone_id_at = text.find("\"zone_id\"").expect("zone_id must appear");
    let is_muted_at = text.find("\"is_muted\"").expect("is_muted must appear");
    assert!(
        zone_id_at < is_muted_at,
        "the text must keep McpZone's declaration order; alphabetical order would \
         put is_muted first and change the bytes clients receive:\n{text}"
    );

    let from_text: Value = serde_json::from_str(&text).expect("hifi_zones text must be JSON");
    assert_eq!(
        envelope(&result, "hifi_zones").get("data"),
        Some(&from_text),
        "data must be the same JSON value as the text, key order aside"
    );

    h.stop().await;
}

// =============================================================================
// 13. Capability discovery, explicit routing, and the AGENTS.md matrix (#398)
// =============================================================================
//
// Written before any of #398's implementation existed, and every assertion in
// this section failed on the first run. `git log --follow tests/mcp_contract.rs`
// shows a test-only commit for this section preceding the src commit.
//
// Three things are pinned here, in the order they matter:
//
// 1. `hifi_capabilities` answers with three states per capability, and the two
//    states that are not "supported" are never confused with each other.
// 2. An unknown or unprefixed zone id is refused by name instead of being
//    forwarded to Roon, and an unrecognised `hifi_control` action is refused
//    instead of being forwarded to an adapter.
// 3. AGENTS.md's capability matrix is the derived data, rendered — not a
//    hand-maintained table that happens to agree with it.

/// The capability vocabulary, restated here rather than imported.
///
/// This is a contract test, so the expected value must not come from the code
/// under test: importing `Capability::ALL` would make this assert that the
/// vocabulary equals itself, and deleting a capability from production would
/// pass. The cost is that adding one requires an edit here, which is the point.
const EXPECTED_CAPABILITIES: &[&str] = &[
    "transport",
    "transport_skip",
    "volume",
    "search",
    "play_by_query",
    "play_by_ref",
    "browse",
    "queue_read",
    "queue_jump",
    "queue_reorder",
    "queue_remove",
    "queue_clear",
    "play_next",
    "repeat_mode",
    "shuffle_mode",
    "saved_playlists",
    "favorites",
    "multiroom_sync",
];

/// Every provider a zone id can name — five, not four.
///
/// `hqplayer:` is in `PrefixedZoneId`'s own valid-prefix list
/// (`src/bus/events.rs`) and `HqpAdapter` publishes `ZoneDiscovered` with it, so
/// HQPlayer zones appear in `hifi_zones`. A capability report that omitted them
/// would understate the surface in exactly the way #392 rule 3 forbids.
const EXPECTED_PROVIDERS: &[&str] = &["roon", "lms", "openhome", "upnp", "hqplayer"];

/// The three states, spelled as they appear on the wire.
const SUPPORTED: &str = "supported";
const UNSUPPORTED: &str = "unsupported";
const NOT_IMPLEMENTED: &str = "not_implemented";

/// Parse `hifi_capabilities`' payload out of a tool result's text.
fn capability_payload(result: &Value) -> Value {
    let text = result_text(result);
    serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("hifi_capabilities must return JSON: {e}\n{text}"))
}

/// The capability entries for one provider, keyed by capability name.
fn capabilities_of(payload: &Value, provider: &str) -> std::collections::BTreeMap<String, Value> {
    payload
        .get("providers")
        .and_then(Value::as_array)
        .expect("payload must carry a providers array")
        .iter()
        .find(|p| p.get("provider").and_then(Value::as_str) == Some(provider))
        .unwrap_or_else(|| panic!("no capability entry for provider {provider:?}"))
        .get("capabilities")
        .and_then(Value::as_array)
        .expect("each provider must carry a capabilities array")
        .iter()
        .map(|entry| {
            let name = entry
                .get("capability")
                .and_then(Value::as_str)
                .expect("each capability entry must name its capability")
                .to_string();
            (name, entry.clone())
        })
        .collect()
}

fn support_of(entry: &Value) -> &str {
    entry
        .get("support")
        .and_then(Value::as_str)
        .unwrap_or_else(|| panic!("capability entry has no support state: {entry}"))
}

/// The tool is advertised, and it answers with every provider and every
/// capability in the vocabulary, each carrying one of exactly three states.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_capabilities_reports_three_states_for_every_provider() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let advertised = tool_names(
        app.list_tools()
            .await
            .get("tools")
            .and_then(Value::as_array)
            .expect("tools array"),
    );
    assert!(
        advertised.iter().any(|n| n == "hifi_capabilities"),
        "hifi_capabilities must be advertised in tools/list; got {advertised:?}"
    );

    let payload = capability_payload(&app.call_tool("hifi_capabilities", json!({})).await);

    let providers: Vec<&str> = payload
        .get("providers")
        .and_then(Value::as_array)
        .expect("providers array")
        .iter()
        .filter_map(|p| p.get("provider").and_then(Value::as_str))
        .collect();
    assert_eq!(
        providers, EXPECTED_PROVIDERS,
        "every zone prefix UHC recognises must have a capability column, in a stable order"
    );

    for provider in EXPECTED_PROVIDERS {
        let caps = capabilities_of(&payload, provider);
        let names: Vec<&str> = caps.keys().map(String::as_str).collect();
        let mut expected: Vec<&str> = EXPECTED_CAPABILITIES.to_vec();
        expected.sort_unstable();
        assert_eq!(
            names, expected,
            "{provider}: the capability vocabulary drifted from EXPECTED_CAPABILITIES"
        );

        for (name, entry) in &caps {
            let support = support_of(entry);
            assert!(
                [SUPPORTED, UNSUPPORTED, NOT_IMPLEMENTED].contains(&support),
                "{provider}/{name}: {support:?} is not one of the three states"
            );

            // The two non-supported states must be distinguishable by more than
            // their name: `not_implemented` is a claim an operator can check.
            if support == NOT_IMPLEMENTED {
                let tracked = entry
                    .get("tracked_by")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| {
                        panic!("{provider}/{name}: not_implemented needs tracked_by")
                    });
                assert!(
                    tracked.starts_with('#') && tracked[1..].chars().all(|c| c.is_ascii_digit()),
                    "{provider}/{name}: tracked_by must be a UHC issue reference, got {tracked:?}"
                );
            } else {
                assert!(
                    entry.get("tracked_by").is_none(),
                    "{provider}/{name}: only not_implemented may carry tracked_by"
                );
            }

            // Every refusal states its evidence, so the claim is auditable
            // rather than trusted. This is the guardrail against a generated
            // matrix being as unchallengeable as the hand-written one that
            // produced AGENTS.md's error.
            if support != SUPPORTED {
                let detail = entry
                    .get("detail")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| panic!("{provider}/{name}: a refusal must carry detail"));
                assert!(
                    detail.len() > 40,
                    "{provider}/{name}: detail must name a checkable fact, got {detail:?}"
                );
            }
        }
    }
}

/// **The criterion this issue exists for.** LMS's protocol supports browse and
/// genuine queue mutation, so neither may ever read as a provider limitation.
///
/// Backed by the live Lyrion 9.1.2 inventories in #402 and #403: `browselibrary
/// items` and the native taggedlist queries for browse; `playlist move`,
/// `playlist delete`, `playlist clear` and `playlist index` for mutation.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn lms_never_reports_a_uhc_gap_as_a_provider_limitation() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let payload = capability_payload(&app.call_tool("hifi_capabilities", json!({})).await);
    let caps = capabilities_of(&payload, "lms");

    // Not one capability in the vocabulary is beyond LMS's protocol.
    let claimed_limits: Vec<&String> = caps
        .iter()
        .filter(|(_, entry)| support_of(entry) == UNSUPPORTED)
        .map(|(name, _)| name)
        .collect();
    assert!(
        claimed_limits.is_empty(),
        "LMS is reported as protocol-incapable of {claimed_limits:?}. Verified against live \
         Lyrion 9.1.2 in #402/#403: every capability in this vocabulary is reachable over \
         slim.request. A UHC gap wearing a provider limit's clothing is the exact defect \
         #398 exists to end."
    );

    // The named cases, spelled out so a regression names itself.
    for capability in [
        "browse",
        "queue_read",
        "queue_jump",
        "queue_reorder",
        "queue_remove",
        "queue_clear",
        "play_next",
        "repeat_mode",
        "shuffle_mode",
        "saved_playlists",
        "favorites",
        "multiroom_sync",
    ] {
        let entry = caps
            .get(capability)
            .unwrap_or_else(|| panic!("lms/{capability} missing"));
        assert_eq!(
            support_of(entry),
            NOT_IMPLEMENTED,
            "lms/{capability} must read as not-yet-implemented until its issue lands"
        );
    }
}

/// OpenHome and UPnP volume was ❌ in AGENTS.md and `not_implemented` in #395.
/// Both adapters implement `vol_abs`/`vol_rel`, so #398 wires the MCP path and
/// the honest answer is now `supported` — reached, not refused.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn openhome_and_upnp_volume_is_wired_and_reported_supported() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let payload = capability_payload(&app.call_tool("hifi_capabilities", json!({})).await);

    for provider in ["openhome", "upnp"] {
        let caps = capabilities_of(&payload, provider);
        assert_eq!(
            support_of(&caps["volume"]),
            SUPPORTED,
            "{provider}/volume: the adapter implements vol_abs/vol_rel and #398 wires MCP to it"
        );
    }

    // And the wiring is real: the call reaches the adapter, which refuses the
    // unknown device in its own words instead of MCP refusing the zone type.
    for (zone_id, expected) in [
        ("openhome:abc", "Error: Volume error: Device not found: abc"),
        ("upnp:abc", "Error: Volume error: Renderer not found: abc"),
    ] {
        let text = result_text(
            &app.call_tool(
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "volume_set", "value": 30 }),
            )
            .await,
        );
        assert_eq!(
            text, expected,
            "{zone_id}: volume must reach its own adapter now, not be refused by MCP"
        );
    }
}

/// Every `supported` cell is proved by a call that reaches that provider's own
/// adapter, identified by the adapter's own refusal wording.
///
/// This is the criterion "a test fails if a capability is advertised as
/// supported for a provider whose call path does not exist" — as an executed
/// call, not as a type-level claim. A `Supported` produced by a routing arm that
/// points at the wrong adapter passes the type check and fails here.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn every_supported_capability_reaches_that_providers_own_adapter() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let payload = capability_payload(&app.call_tool("hifi_capabilities", json!({})).await);

    // Each adapter's own refusal for an id it does not know. Distinct per
    // adapter, which is what makes the call path observable end to end.
    fn fingerprint(provider: &str) -> &'static str {
        match provider {
            "roon" => "Not connected to Roon",
            "lms" => "LMS host not configured",
            "openhome" => "Device not found",
            "upnp" => "Renderer not found",
            other => panic!("no adapter fingerprint for {other}"),
        }
    }

    // How to exercise each routable capability, and the zone id to use.
    fn probe(provider: &str, capability: &str) -> Option<(&'static str, Value)> {
        let zone_id = format!("{provider}:abc");
        match capability {
            "transport" => Some((
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "play" }),
            )),
            "transport_skip" => Some((
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "next" }),
            )),
            "volume" => Some((
                "hifi_control",
                json!({ "zone_id": zone_id, "action": "volume_set", "value": 30 }),
            )),
            "search" => Some(("hifi_search", json!({ "query": "q", "zone_id": zone_id }))),
            "play_by_query" => Some(("hifi_play", json!({ "query": "q", "zone_id": zone_id }))),
            _ => None,
        }
    }

    let mut proved = 0usize;
    for provider in EXPECTED_PROVIDERS {
        // HQPlayer zones are recognised but nothing is wired, so there is no
        // supported cell to prove; that is asserted separately.
        if *provider == "hqplayer" {
            continue;
        }
        for (capability, entry) in capabilities_of(&payload, provider) {
            if support_of(&entry) != SUPPORTED {
                continue;
            }
            let (tool, args) = probe(provider, &capability).unwrap_or_else(|| {
                panic!(
                    "{provider}/{capability} is reported supported but this test has no way to \
                     exercise it. Either it is not really wired, or the probe table needs an \
                     entry — do not delete the assertion."
                )
            });
            let text = result_text(&app.call_tool(tool, args).await);
            let expected = fingerprint(provider);
            assert!(
                text.to_lowercase().contains(&expected.to_lowercase()),
                "{provider}/{capability} is reported supported, but calling {tool} produced \
                 {text:?}, which does not name the {provider} adapter ({expected:?}). A \
                 supported capability whose call path lands elsewhere is a false claim."
            );
            proved += 1;
        }
    }
    assert!(
        proved >= 16,
        "only {proved} supported cells were proved end to end; the probe table has gone stale"
    );
}

/// An unprefixed zone id no longer means Roon.
///
/// **Behavior change.** Every zone id UHC publishes carries a prefix
/// (`src/bus/events.rs`), so a bare id can only come from outside `hifi_zones` —
/// and routing it to Roon is #360's named anti-pattern. The refusal names the
/// prefixes so a client recovers in one call.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn an_unprefixed_zone_id_is_refused_by_name_instead_of_routed_to_roon() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let calls: &[(&str, Value)] = &[
        (
            "hifi_control",
            json!({ "zone_id": "1601a5d4bare", "action": "play" }),
        ),
        (
            "hifi_control",
            json!({ "zone_id": "1601a5d4bare", "action": "volume_set", "value": 30 }),
        ),
        (
            "hifi_search",
            json!({ "query": "q", "zone_id": "1601a5d4bare" }),
        ),
        (
            "hifi_play",
            json!({ "query": "q", "zone_id": "1601a5d4bare" }),
        ),
        ("hifi_capabilities", json!({ "zone_id": "1601a5d4bare" })),
    ];

    for (tool, args) in calls {
        let result = app.call_tool(tool, args.clone()).await;
        let text = result_text(&result);
        assert!(
            !text.contains("Roon"),
            "{tool}: a bare zone id must not reach Roon any more; got {text:?}"
        );
        assert!(
            text.contains("no provider prefix"),
            "{tool}: the refusal must say the id has no provider prefix; got {text:?}"
        );
        for prefix in ["roon:", "lms:", "openhome:", "upnp:", "hqplayer:"] {
            assert!(
                text.contains(prefix),
                "{tool}: the refusal must name the accepted prefix {prefix:?}; got {text:?}"
            );
        }

        let env = envelope(&result, tool);
        assert_eq!(
            env.get("outcome").and_then(Value::as_str),
            Some("invalid"),
            "{tool}: a malformed zone id is the client's to fix, so `invalid`, not \
             `unsupported`: {env}"
        );
        assert_eq!(
            env.pointer("/refusal/parameter").and_then(Value::as_str),
            Some("zone_id"),
            "{tool}: the refusal must name zone_id: {env}"
        );
        assert_eq!(
            env.pointer("/scope/provider").and_then(Value::as_str),
            Some("unknown"),
            "{tool}: UHC identified no provider, so it must claim none: {env}"
        );
    }
}

/// An unrecognised prefix is refused too, and it says which prefix it did not
/// recognise.
///
/// **Behavior change** for transport, search and play, which forwarded these to
/// Roon. Volume already refused them, with a sentence that blamed the zone type.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn an_unrecognised_zone_prefix_is_refused_instead_of_routed_to_roon() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let calls: &[(&str, Value)] = &[
        (
            "hifi_control",
            json!({ "zone_id": "sonos:abc", "action": "play" }),
        ),
        (
            "hifi_control",
            json!({ "zone_id": "sonos:abc", "action": "volume_set", "value": 30 }),
        ),
        (
            "hifi_search",
            json!({ "query": "q", "zone_id": "sonos:abc" }),
        ),
        ("hifi_play", json!({ "query": "q", "zone_id": "sonos:abc" })),
        ("hifi_capabilities", json!({ "zone_id": "sonos:abc" })),
    ];

    for (tool, args) in calls {
        let result = app.call_tool(tool, args.clone()).await;
        let text = result_text(&result);
        assert!(
            !text.contains("Roon"),
            "{tool}: an unrecognised prefix must not reach Roon; got {text:?}"
        );
        assert!(
            text.contains("sonos:"),
            "{tool}: the refusal must quote the prefix it did not recognise; got {text:?}"
        );
        assert!(
            text.contains("hqplayer:"),
            "{tool}: the refusal must name every accepted prefix; got {text:?}"
        );
    }
}

/// HQPlayer zones are listed by `hifi_zones` and were being forwarded to Roon.
/// They are now recognised, and reported as a UHC gap with a tracking issue.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hqplayer_zones_are_recognised_and_reported_as_not_wired() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    for args in [
        json!({ "zone_id": "hqplayer:desktop", "action": "play" }),
        json!({ "zone_id": "hqplayer:desktop", "action": "volume_set", "value": 30 }),
    ] {
        let result = app.call_tool("hifi_control", args).await;
        let text = result_text(&result);
        assert!(
            !text.contains("Roon"),
            "an hqplayer: zone must not be forwarded to Roon; got {text:?}"
        );

        let env = envelope(&result, "hifi_control");
        assert_eq!(
            env.get("outcome").and_then(Value::as_str),
            Some("unsupported"),
            "the zone id is valid and the operation is not wired: {env}"
        );
        assert_eq!(
            env.pointer("/refusal/reason").and_then(Value::as_str),
            Some("not_implemented"),
            "HQPlayer's adapter has play/pause/next/volume, so this is UHC's gap: {env}"
        );
        assert_eq!(
            env.pointer("/refusal/tracked_by").and_then(Value::as_str),
            Some("#328"),
            "the gap must name the issue that closes it: {env}"
        );
        assert_eq!(
            env.pointer("/scope/provider").and_then(Value::as_str),
            Some("hqplayer"),
            "the prefix identifies the provider even though nothing is wired: {env}"
        );
    }
}

/// `hifi_control` no longer forwards an action it does not know.
///
/// **Behavior change.** `other => other` sent anything through to the adapter,
/// so a typo surfaced as whatever that backend said — or, offline, as a device
/// lookup failure that never mentioned the action at all.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn an_unknown_hifi_control_action_is_refused_with_the_valid_action_list() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    // A well-formed zone id, so nothing but the action can be at fault.
    let result = app
        .call_tool(
            "hifi_control",
            json!({ "zone_id": "openhome:abc", "action": "frobnicate" }),
        )
        .await;
    let text = result_text(&result);
    assert!(
        text.contains("frobnicate"),
        "the refusal must quote the action it rejected; got {text:?}"
    );
    assert!(
        !text.contains("Device not found"),
        "the action must be rejected before the adapter is asked; got {text:?}"
    );
    for action in [
        "play",
        "pause",
        "playpause",
        "next",
        "previous",
        "volume_set",
        "volume_up",
        "volume_down",
    ] {
        assert!(
            text.contains(action),
            "the refusal must list the valid action {action:?}; got {text:?}"
        );
    }

    let env = envelope(&result, "hifi_control");
    assert_eq!(
        env.get("outcome").and_then(Value::as_str),
        Some("invalid"),
        "an unknown action is the client's to fix: {env}"
    );
    assert_eq!(
        env.pointer("/refusal/parameter").and_then(Value::as_str),
        Some("action"),
        "the refusal must name the action parameter: {env}"
    );
    // `operation` was an open set precisely because of `other => other`.
    assert_eq!(
        env.get("operation").and_then(Value::as_str),
        Some("unknown_action"),
        "an unrecognised action must not be echoed into `operation` as if it were one: {env}"
    );
}

/// `hifi_search` with **no** `zone_id` still routes to Roon. Unchanged, and
/// pinned so the additive claim covers it.
///
/// `None` is not a zone id: there is nothing to route by, LMS `globalsearch`
/// requires a player, and the tool's own description documents the default.
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn an_absent_zone_id_still_routes_search_to_roon() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let result = app.call_tool("hifi_search", json!({ "query": "q" })).await;
    let text = result_text(&result);
    assert!(
        text.to_lowercase().contains("not connected to roon"),
        "an absent zone_id must keep routing to Roon; got {text:?}"
    );
    assert_eq!(
        envelope(&result, "hifi_search")
            .pointer("/scope/provider")
            .and_then(Value::as_str),
        Some("roon"),
        "and it must say so, because this default is documented rather than silent"
    );
}

/// Render AGENTS.md's capability matrix **from `hifi_capabilities`' own wire
/// payload**.
///
/// Deliberately not from `mcp::capabilities`' internals. Checking a doc against
/// an internal function proves the doc agrees with a source; checking it against
/// the wire proves the doc agrees with *what a client is actually told*, which is
/// the claim AGENTS.md is making. It also means there is one renderer, here, and
/// no second copy of the table anywhere.
fn render_capability_matrix(payload: &Value) -> String {
    let providers: Vec<String> = payload
        .get("providers")
        .and_then(Value::as_array)
        .expect("providers array")
        .iter()
        .filter_map(|p| p.get("provider").and_then(Value::as_str))
        .map(str::to_string)
        .collect();

    // Header. Capability rows, provider columns — 18 x 5 reads far better tall
    // than wide, and the vocabulary is the part that grows.
    let mut out = String::from("| Capability |");
    for provider in &providers {
        out.push_str(&format!(" {provider} |"));
    }
    out.push_str("\n|---|");
    for _ in &providers {
        out.push_str("---|");
    }
    out.push('\n');

    // One footnote per distinct non-supported cell, so the table states its
    // evidence instead of asserting a symbol.
    let mut footnotes: Vec<String> = Vec::new();

    let tables: Vec<std::collections::BTreeMap<String, Value>> = providers
        .iter()
        .map(|p| capabilities_of(payload, p))
        .collect();

    for capability in EXPECTED_CAPABILITIES {
        out.push_str(&format!("| `{capability}` |"));
        for (provider, caps) in providers.iter().zip(&tables) {
            let entry = caps
                .get(*capability)
                .unwrap_or_else(|| panic!("{provider}/{capability} missing from the payload"));
            let cell = match support_of(entry) {
                SUPPORTED => "✅".to_string(),
                UNSUPPORTED => {
                    footnotes.push(format!(
                        "- ⛔ **{provider} / `{capability}`** — {}",
                        entry
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    ));
                    "⛔".to_string()
                }
                NOT_IMPLEMENTED => {
                    let tracked = entry
                        .get("tracked_by")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    footnotes.push(format!(
                        "- 🚧 **{provider} / `{capability}`** ({tracked}) — {}",
                        entry
                            .get("detail")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                    ));
                    format!("🚧 {tracked}")
                }
                other => panic!("{provider}/{capability}: unknown state {other:?}"),
            };
            out.push_str(&format!(" {cell} |"));
        }
        out.push('\n');
    }

    out.push_str(
        "\n✅ supported · ⛔ the provider's protocol cannot do it · 🚧 the provider can, ",
    );
    out.push_str("UHC has not wired it (issue that will)\n\n");
    out.push_str("Every non-supported cell states the fact it rests on, so the claim can be ");
    out.push_str("checked rather than trusted:\n\n");
    for note in footnotes {
        out.push_str(&note);
        out.push('\n');
    }
    out
}

/// AGENTS.md's capability matrix is generated from the derived data, and this
/// test fails on drift.
///
/// #398 exists because a hand-written ❌ in that table went unchallenged for as
/// long as it took someone to read the adapters. Regenerate with:
///
/// ```sh
/// UPDATE_AGENTS_MATRIX=1 cargo test --test mcp_contract agents_md
/// ```
#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn agents_md_capability_matrix_matches_the_derived_data() {
    const BEGIN: &str = "<!-- BEGIN GENERATED CAPABILITY MATRIX (#398) -->";
    const END: &str = "<!-- END GENERATED CAPABILITY MATRIX (#398) -->";

    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;
    let payload = capability_payload(&app.call_tool("hifi_capabilities", json!({})).await);
    let rendered = render_capability_matrix(&payload);

    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("AGENTS.md");
    let doc = std::fs::read_to_string(&path).expect("AGENTS.md must be readable");

    let start = doc
        .find(BEGIN)
        .unwrap_or_else(|| panic!("AGENTS.md must contain {BEGIN}"));
    let end = doc
        .find(END)
        .unwrap_or_else(|| panic!("AGENTS.md must contain {END}"));
    assert!(start < end, "AGENTS.md matrix markers are out of order");

    let current = &doc[start + BEGIN.len()..end];
    let expected = format!("\n\n{}\n", rendered.trim_end());

    if current != expected {
        if std::env::var("UPDATE_AGENTS_MATRIX").as_deref() == Ok("1") {
            let updated = format!("{}{BEGIN}{expected}{}", &doc[..start], &doc[end..]);
            std::fs::write(&path, updated).expect("AGENTS.md must be writable");
            return;
        }
        panic!(
            "AGENTS.md's capability matrix has drifted from what hifi_capabilities reports.\n\n\
             This table is generated. Do not hand-edit it — a hand-written cell is what \
             produced the ❌ against OpenHome/UPnP volume that #398 was filed to correct.\n\n\
             Regenerate with UPDATE_AGENTS_MATRIX=1 cargo test --test mcp_contract agents_md\n\n\
             --- AGENTS.md has ---\n{current}\n--- the derived data renders ---\n{expected}"
        );
    }
}
