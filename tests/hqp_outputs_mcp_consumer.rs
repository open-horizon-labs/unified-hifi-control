//! Consumer-level MCP surface for HQPlayer output routing (NAA managed relay).
//!
//! **Written from the client's expectation, against the backend's published contract**:
//! `hifi_hqplayer_outputs` (read) and `hifi_hqplayer_output_control` (write) share the same
//! command service as the HTTP surface (`GET /hqplayer/outputs`, `POST
//! /hqplayer/outputs/command`). This file is owned independently of
//! `tests/hqplayer_naa_relay_lifecycle.rs` and `tests/mock_servers/naa.rs` (the relay/protocol
//! test slice) — it exercises the real bound MCP transport the same way
//! `tests/mcp_hqplayer_control.rs` does for the existing HQPlayer tools, using only the
//! already-public `mock_servers::hqplayer` wire fixture, so it stays buildable independent of
//! that other slice's churn.
//!
//! Every UHC MCP tool result carries a structured envelope on `structuredContent`
//! (`src/mcp/envelope.rs`): `outcome` (`"ok"` for reads, `"accepted"` for writes) and `data` (the
//! actual payload — a projection or an operation, per tool). These tests read `data` from that
//! envelope, not from the human-readable text content, per the MCP contract.
//!
//! Each test asserts the **intended final passing behavior**: configure the opt-in relay with a
//! known loopback bind through the real write tool and poll until that operation reaches a
//! terminal outcome, then separately confirm the plain projection reflects it — an accepted
//! receipt is an admission, not a completion, so nothing here treats `accepted` as done. Neither
//! tool is registered yet, so every test currently fails at its first real MCP call — that is the
//! correct red: these assertions describe success and must flip to passing only once the backend
//! actually implements the tools, never the other way around.
//!
//! Both tests attach their own instance and isolate their own config/data directories, but
//! `std::env::set_var` is process-global — running them concurrently in the same test binary
//! would race on `UHC_CONFIG_DIR`/`UHC_DATA_DIR`. `#[serial]` (the `serial_test` dev-dependency
//! already used elsewhere in this crate) forces them to run one at a time regardless of
//! `--test-threads`, rather than relying on the caller to remember `--test-threads=1`.

#![cfg(feature = "naa-proxy")]

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Value};
use serial_test::serial;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use mock_servers::hqplayer::corpus::VERIFIED_PROFILE;
use mock_servers::hqplayer::model::DaemonModel;
use mock_servers::hqplayer::wire::{WirePolicy, WireServer};

use rust_mcp_sdk::mcp_client::{client_runtime, ClientHandler, ClientRuntime};
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, Implementation,
    InitializeRequestParams, LATEST_PROTOCOL_VERSION,
};
use rust_mcp_sdk::{McpClient, RequestOptions, StreamableTransportOptions};

use unified_hifi_control::adapters::hqplayer::{
    HqpAdapter, HqpInstanceManager, HqpRecoveryConfig, HqpRuntimeBridge, HqpTimeouts,
    HqpZoneLinkService,
};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::runtime::build_runtime;
use unified_hifi_control::bus::{create_bus, SharedBus};
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mcp;

/// Isolates both `UHC_CONFIG_DIR` and `UHC_DATA_DIR` (separate lookups — see
/// `src/config/mod.rs::get_data_dir`) under a directory keyed by `test_name`, and disables every
/// adapter except the one HQPlayer instance each test attaches. Unconditional (no `Once`): with
/// `#[serial]` there is no concurrent access to race, and each test wants its own fresh
/// directories rather than sharing one across the process.
fn isolate_environment(test_name: &str) {
    let dir = std::env::temp_dir().join(format!(
        "uhc-mcp-hqp-outputs-{test_name}-{}",
        std::process::id()
    ));
    let subdir = dir.join("unified-hifi");
    std::fs::create_dir_all(&subdir).expect("create isolated mcp config dir");
    std::fs::write(
        subdir.join("app-settings.json"),
        r#"{"adapters":{"roon":false,"upnp":false,"openhome":false,"lms":false,"hqplayer":true}}"#,
    )
    .expect("write isolated app settings");
    std::env::set_var("UHC_CONFIG_DIR", &dir);
    let data_dir = std::env::temp_dir().join(format!(
        "uhc-mcp-hqp-outputs-data-{test_name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&data_dir).expect("create isolated data dir");
    std::env::set_var("UHC_DATA_DIR", &data_dir);
}

fn fast_timeouts() -> HqpTimeouts {
    HqpTimeouts {
        connect: Duration::from_millis(100),
        response: Duration::from_millis(500),
        reconnect_delay: Duration::from_millis(10),
        max_attempts: 1,
    }
}

fn fast_recovery() -> HqpRecoveryConfig {
    HqpRecoveryConfig {
        poll_interval: Duration::from_millis(10),
        short_retry_delay: Duration::from_millis(10),
        restart_window: Duration::from_millis(40),
        backoff_initial: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(40),
        stable_threshold: Duration::from_millis(20),
    }
}

struct TestClientHandler;

#[async_trait::async_trait]
impl ClientHandler for TestClientHandler {}

/// Real MCP server + client over a bound loopback port, with one attached HQPlayer instance.
/// Deliberately does not attach any NAA relay peer fixture — these tests exercise the
/// read/write output-routing MCP surface itself (registration, projection shape, command
/// acceptance and completion), not yet the relay byte-transparency path, which needs the command
/// service this file currently finds absent.
struct Rig {
    instance_name: String,
    manager: Arc<HqpInstanceManager>,
    aggregator: Arc<ZoneAggregator>,
    bus: SharedBus,
    aggregator_task: tokio::task::JoinHandle<()>,
    projection_task: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
    client: Arc<ClientRuntime>,
}

impl Rig {
    async fn new(test_name: &str) -> Self {
        isolate_environment(test_name);
        let bus = create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let aggregator_task = {
            let aggregator = aggregator.clone();
            tokio::spawn(async move { aggregator.run().await })
        };
        tokio::task::yield_now().await;
        let runtime = build_runtime(aggregator.clone(), 16, 32);
        let bridge = Arc::new(HqpRuntimeBridge::new(
            runtime.projection_ingress.clone(),
            runtime.commands.clone(),
        ));
        let reliable_commands = runtime.commands.clone();
        let projection_task = tokio::spawn(runtime.projection_actor.run());

        let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
        let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
        let manager = Arc::new(HqpInstanceManager::new_with_runtime(bus.clone(), bridge));
        let hqplayer = manager.get_default().await;
        let hqp_zone_links = Arc::new(HqpZoneLinkService::new(manager.clone()));
        let lms = Arc::new(LmsAdapter::new(bus.clone()));
        let openhome = Arc::new(OpenHomeAdapter::new(bus.clone()));
        let upnp = Arc::new(UPnPAdapter::new(bus.clone()));

        let startable: Vec<Arc<dyn Startable>> = vec![];
        let state = AppState::new(
            roon,
            hqplayer,
            manager.clone(),
            hqp_zone_links,
            lms,
            openhome,
            upnp,
            KnobStore::new(),
            bus.clone(),
            aggregator.clone(),
            coordinator,
            startable,
            Instant::now(),
            CancellationToken::new(),
        )
        .with_reliable_commands(reliable_commands);

        let mcp_extension = mcp::create_mcp_extension(state);
        let app = Router::new()
            .route("/mcp", get(mcp::handle_mcp_get))
            .route("/mcp", post(mcp::handle_mcp_post))
            .route("/mcp", delete(mcp::handle_mcp_delete))
            .layer(mcp_extension);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind mcp test listener");
        let addr = listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client_details = InitializeRequestParams {
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "uhc-mcp-outputs-test-client".into(),
                version: "0.0.0".into(),
                title: None,
                description: None,
                icons: vec![],
                website_url: None,
            },
            protocol_version: LATEST_PROTOCOL_VERSION.into(),
            meta: None,
        };
        let transport_options = StreamableTransportOptions {
            mcp_url: format!("http://{addr}/mcp"),
            request_options: RequestOptions::default(),
        };
        let client = client_runtime::with_transport_options(
            client_details,
            transport_options,
            TestClientHandler,
            None,
            None,
        );
        client.clone().start().await.expect("start mcp test client");

        Self {
            instance_name: test_name.to_string(),
            manager,
            aggregator,
            bus,
            aggregator_task,
            projection_task,
            server_task,
            client,
        }
    }

    fn zone_id(&self) -> String {
        format!("hqplayer:{}", self.instance_name)
    }

    async fn attach(&self, server: &WireServer) -> Arc<HqpAdapter> {
        let adapter = self
            .manager
            .add_instance(
                self.instance_name.clone(),
                "127.0.0.1".to_string(),
                Some(server.port()),
                None,
                None,
                None,
            )
            .await;
        adapter.set_timeouts(fast_timeouts()).await;
        adapter.set_recovery_config(fast_recovery()).await;
        self.manager.start().await.expect("start managed lifecycle");
        adapter
    }

    async fn zone_when(
        &self,
        mut condition: impl FnMut(&unified_hifi_control::bus::Zone) -> bool,
    ) -> unified_hifi_control::bus::Zone {
        let zone_id = self.zone_id();
        for _ in 0..200 {
            if let Some(zone) = self.aggregator.get_zone(&zone_id).await {
                if condition(&zone) {
                    return zone;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("zone did not settle in the bounded polling budget");
    }

    async fn call_tool(&self, name: &str, args: Value) -> CallToolResult {
        let arguments = args.as_object().cloned();
        self.client
            .request_tool_call(CallToolRequestParams {
                name: name.to_string(),
                arguments,
                meta: None,
                task: None,
            })
            .await
            .unwrap_or_else(|e| panic!("tool call `{name}` failed at the protocol level: {e}"))
    }

    async fn shutdown(self) {
        self.manager.stop().await;
        self.bus
            .publish(unified_hifi_control::bus::BusEvent::ShuttingDown { reason: None });
        let _ = self.aggregator_task.await;
        self.projection_task.abort();
        let _ = self.client.shut_down().await;
        self.server_task.abort();
    }
}

/// Every UHC MCP tool result carries the structured envelope on `structuredContent`
/// (`src/mcp/envelope.rs::Envelope`) — this is the contract-defined machine-readable surface,
/// not the human-readable text content.
fn envelope_of(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .map(Value::Object)
        .expect("every UHC MCP tool result carries the structured envelope")
}

/// Calls a tool and requires its envelope to report the given `outcome` (`"ok"` for reads,
/// `"accepted"` for writes — see `Envelope::read`/`Envelope::write`), then returns `data`. A
/// missing tool, a protocol-level error, or a mismatched outcome all fail this with a message
/// identifying which step of the intended flow could not get as far as returning real data.
async fn expect_envelope_data(rig: &Rig, name: &str, args: Value, expected_outcome: &str) -> Value {
    let result = rig.call_tool(name, args).await;
    let envelope = envelope_of(&result);
    assert_eq!(
        envelope["outcome"], expected_outcome,
        "`{name}` must report outcome=\"{expected_outcome}\": {envelope}"
    );
    envelope
        .get("data")
        .cloned()
        .unwrap_or_else(|| panic!("`{name}` envelope carries no `data`: {envelope}"))
}

async fn start_daemon() -> WireServer {
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    WireServer::start(Arc::new(model), WirePolicy::default()).await
}

/// Polls `hifi_hqplayer_outputs{zone_id, operation_id}` (envelope `data` is the operation, per
/// contract — not the projection) until `outcome` is set, and returns that terminal operation.
/// An accepted receipt is only an admission; this is what "await terminal" means in practice.
async fn await_terminal_operation(rig: &Rig, operation_id: &str) -> Value {
    for _ in 0..40 {
        let operation = expect_envelope_data(
            rig,
            "hifi_hqplayer_outputs",
            json!({"zone_id": rig.zone_id(), "operation_id": operation_id}),
            "ok",
        )
        .await;
        if operation.get("outcome").is_some_and(|o| !o.is_null()) {
            return operation;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("operation {operation_id} did not reach a terminal outcome within the bounded budget");
}

/// Configure the opt-in relay through the production MCP command tool, await its terminal
/// outcome, then read the committed projection to verify the available loopback listener.
/// An accepted receipt alone does not prove completion.
#[tokio::test]
#[serial]
async fn relay_configure_reaches_a_terminal_outcome_and_the_projection_reflects_it() {
    let server = start_daemon().await;
    let rig = Rig::new("relay-configure").await;
    rig.attach(&server).await;
    rig.zone_when(|_| true).await;

    let initial = expect_envelope_data(
        &rig,
        "hifi_hqplayer_outputs",
        json!({"zone_id": rig.zone_id()}),
        "ok",
    )
    .await;
    assert_eq!(initial["zone_id"], rig.zone_id());
    let source_epoch = initial["source_epoch"]
        .as_u64()
        .expect("projection carries source_epoch");
    let output_revision = initial["output_revision"]
        .as_u64()
        .expect("projection carries output_revision");

    let receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({
            "zone_id": rig.zone_id(),
            "action": "relay_configure",
            "enabled": true,
            "bind": "127.0.0.1:0",
            "hqp_allow": [],
            "discovery_interface": null,
            "expected_source_epoch": source_epoch,
            "expected_output_revision": output_revision,
        }),
        "accepted",
    )
    .await;
    assert_eq!(
        receipt["accepted"], true,
        "relay_configure must be accepted: {receipt}"
    );
    let operation_id = receipt["operation"]["operation_id"]
        .as_str()
        .expect("accepted receipt carries an operation_id")
        .to_string();

    // `accepted` is an admission, not a completion — wait for the operation to actually settle.
    let terminal = await_terminal_operation(&rig, &operation_id).await;
    assert_eq!(
        terminal["outcome"], "complete",
        "binding a fresh loopback listener must complete: {terminal}"
    );

    // Only now check the plain projection — a single immediate GET right after the accepted
    // receipt would race the relay's own bind, which is exactly the bug this ordering avoids.
    let after = expect_envelope_data(
        &rig,
        "hifi_hqplayer_outputs",
        json!({"zone_id": rig.zone_id()}),
        "ok",
    )
    .await;
    assert_eq!(
        after["availability"], "available",
        "a completed, successfully bound relay_configure must report availability=available: {after}"
    );
    assert_eq!(after["relay"]["enabled"], true);
    let bind = after["relay"]["bind"]
        .as_str()
        .expect("bound relay reports its actual bind address");
    assert!(
        bind.starts_with("127.0.0.1:"),
        "expected a loopback bind, got {bind}"
    );

    rig.shutdown().await;
}

/// Configure a loopback relay with an explicit discovery interface and matching discovery
/// port, await completion, then request discovery through MCP and verify its typed terminal
/// result.
#[tokio::test]
#[serial]
async fn discover_after_relay_configure_reaches_a_terminal_discovery_result() {
    let server = start_daemon().await;
    let rig = Rig::new("discover").await;
    rig.attach(&server).await;
    rig.zone_when(|_| true).await;

    let initial = expect_envelope_data(
        &rig,
        "hifi_hqplayer_outputs",
        json!({"zone_id": rig.zone_id()}),
        "ok",
    )
    .await;
    let source_epoch = initial["source_epoch"].as_u64().unwrap();
    let output_revision = initial["output_revision"].as_u64().unwrap();

    // NAA discovery answers on the TCP port it advertises: with discovery enabled the relay must
    // actually be bound to that same port, not an ephemeral `:0` bind (which the daemon cannot
    // answer discovery on). Reserve one real, explicitly loopback ephemeral port and use it for
    // both `bind` and `discovery_port`, so the two agree the way production requires.
    let discovery_port = mock_servers::naa::reserved_port();
    let configure_receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({
            "zone_id": rig.zone_id(),
            "action": "relay_configure",
            "enabled": true,
            "bind": format!("127.0.0.1:{discovery_port}"),
            "hqp_allow": [],
            "discovery_interface": "127.0.0.1",
            "discovery_port": discovery_port,
            "expected_source_epoch": source_epoch,
            "expected_output_revision": output_revision,
        }),
        "accepted",
    )
    .await;
    let configure_operation_id = configure_receipt["operation"]["operation_id"]
        .as_str()
        .expect("accepted receipt carries an operation_id")
        .to_string();
    let configured = await_terminal_operation(&rig, &configure_operation_id).await;
    assert_eq!(
        configured["outcome"], "complete",
        "relay_configure must complete before discovery has anything to scan from: {configured}"
    );

    // discover requires no epoch/revision echo (contract: exempt actions are
    // stop/discover/import_preview/setup_preview/setup_readback).
    let discover_receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({"zone_id": rig.zone_id(), "action": "discover"}),
        "accepted",
    )
    .await;
    assert_eq!(
        discover_receipt["accepted"], true,
        "discover must be accepted: {discover_receipt}"
    );
    let discover_operation_id = discover_receipt["operation"]["operation_id"]
        .as_str()
        .expect("accepted receipt carries an operation_id")
        .to_string();

    let terminal = await_terminal_operation(&rig, &discover_operation_id).await;
    assert_eq!(
        terminal["outcome"], "complete",
        "a discovery scan against a reachable loopback interface must complete: {terminal}"
    );
    assert_eq!(
        terminal["result"]["kind"], "discovery",
        "discover's typed result must be the discovery observation, not JSON-in-a-string: {terminal}"
    );

    rig.shutdown().await;
}
