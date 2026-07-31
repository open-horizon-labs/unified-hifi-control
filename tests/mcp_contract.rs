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
use unified_hifi_control::mcp::types::{McpPipelineStatus, McpSearchResult};

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
        std::fs::create_dir_all(path.parent().expect("fixture dir"))
            .expect("create fixture dir");
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
        body.as_object_mut().expect("tool must be an object").remove("name");
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
        .unwrap_or_else(|| {
            panic!("recovered request must return the tool list, got: {response}")
        });
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
    assert_eq!(text, "[]", "hifi_hqplayer_profiles must return a JSON array");
}

#[tokio::test]
#[serial_test::serial(uhc_config_dir)]
async fn hifi_hqplayer_load_profile_reports_failure_prefix() {
    let _settings = SettingsFixture::with_hqplayer(true);
    let app = TestApp::new().await;

    let text = result_text(
        &app.call_tool("hifi_hqplayer_load_profile", json!({ "profile": "4x-Sinc-L" }))
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

    let text = result_text(&app.call_tool("hifi_search", json!({ "query": "Eagles" })).await);
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
        ("openhome:abc", "Device not found: abc", "explicit openhome:"),
        ("upnp:abc", "Renderer not found: abc", "explicit upnp:"),
        ("roon:abc", "Not connected to Roon", "explicit roon:"),
        // Today's defaults. Both belong to #398, not #394.
        ("1601a5d4bare", "Not connected to Roon", "bare id -> Roon"),
        ("sonos:abc", "Not connected to Roon", "unknown prefix -> Roon"),
    ];

    for (zone_id, expected_fragment, label) in cases {
        let text = result_text(
            &app.call_tool("hifi_control", json!({ "zone_id": zone_id, "action": "play" }))
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
        assert!(found, "LMS player never reached the aggregator as {zone_id}");

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
            .call_tool("hifi_control", json!({ "zone_id": zone_id, "action": "play" }))
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
    for url in [
        openhome_mock.description_url(),
        upnp_mock.description_url(),
    ] {
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
    ("zone_id", Consumed("hifi_now_playing/hifi_control/hifi_play.zone_id")),
    ("zone_name", DisplayOnly(
        "human label; every tool addresses a zone by zone_id, never by name",
    )),
    ("state", DisplayOnly(
        "playback state readout; hifi_control.action is the write path",
    )),
    ("volume", DisplayOnly(
        "current level readout; hifi_control.value is the write path",
    )),
    ("is_muted", DisplayOnly(
        "mute readout with no corresponding write: hifi_control has no mute action",
    )),
    ("title", DisplayOnly(
        "hifi_search returns {title, subtitle} and no key, so the only route from \
         'found it' to 'playing it' is handing the title back to hifi_play, which \
         re-searches and takes the first match. That is #392's keystone finding and \
         #396's job — NOT an endorsed design. Listed to freeze today's behavior.",
    )),
    ("artist", DisplayOnly(
        "now-playing readout; hifi_play takes a free-text query, not a field",
    )),
    ("album", DisplayOnly(
        "now-playing readout; hifi_play takes a free-text query, not a field",
    )),
    ("subtitle", DisplayOnly(
        "search-result label, same defect as `title`; see #396",
    )),
    // hifi_status / hifi_hqplayer_status readouts.
    ("connected", DisplayOnly(
        "boolean health readout; nothing takes a connection state as input",
    )),
    ("host", DisplayOnly(
        "diagnostic address; configuring a host is an HTTP/settings concern, not an MCP tool",
    )),
    ("core_name", DisplayOnly(
        "Roon core label for the operator; no tool selects a core",
    )),
    ("roon", DisplayOnly(
        "hifi_status grouping key, not a value a client passes anywhere",
    )),
    ("hqplayer", DisplayOnly(
        "hifi_status grouping key, not a value a client passes anywhere",
    )),
    ("pipeline", DisplayOnly(
        "hifi_hqplayer_status grouping key wrapping the pipeline readout",
    )),
    ("filter", DisplayOnly(
        "pipeline readout; hifi_hqplayer_set_pipeline writes it via \
         setting='filter1x'/'filterNx', which is a different name",
    )),
    ("shaper", Consumed("hifi_hqplayer_set_pipeline.setting='shaper'")),
    ("rate", Consumed("hifi_hqplayer_set_pipeline.setting='rate'")),
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
        let text = result_text(&h.app.call_tool(tool, args).await);
        let parsed: Value = serde_json::from_str(&text)
            .unwrap_or_else(|e| panic!("{tool} must return JSON: {e}\n{text}"));
        collect_keys(&parsed, &mut returned);
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
