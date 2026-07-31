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
    let bus = create_bus();
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

/// The `tools/list` wire payload — names, descriptions, input schemas, and
/// annotation hints — must match the committed fixture byte for byte.
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

    assert_matches_fixture("mcp_tools.json", result.get("tools").expect("tools"));
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
