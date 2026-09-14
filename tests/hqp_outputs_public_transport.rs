//! Bounded public-transport acceptance tests for HQPlayer output routing (NAA managed relay).
//!
//! **Both transports are real production bindings**, not fixtures standing in for missing
//! surface:
//! - MCP: `mcp::create_mcp_extension` + `handle_mcp_get/post/delete` (same as
//!   `tests/hqp_outputs_mcp_consumer.rs`), with `hifi_hqplayer_outputs` /
//!   `hifi_hqplayer_output_control` dispatched by `tool_box!` exactly as any other tool.
//! - HTTP: `api::hqp_outputs_http::routes()`, the exact function `src/main.rs` merges into its
//!   own router (`.merge(api::hqp_outputs_http::routes())`, right after `/hqplayer/configure`).
//!   Binding it here is binding the same handlers production binds — this file never defines its
//!   own success handler for anything the contract requires.
//! - Controller auth is the real, unmodified `api::controller_auth::middleware`, layered over the
//!   same merged router, gated live per request by `UHC_REQUIRE_CONTROLLER_AUTH` exactly as
//!   production reads it.
//!
//! Every read/write in every test goes through `api::hqp_outputs::{read_hqp_outputs,
//! read_hqp_output_operation, submit_hqp_output_command}` — the one shared command service both
//! transports call — and `HqpInstanceManager`/`ZoneAggregator`/the reliable runtime/`AppState` are
//! the same production types and `mock_servers::hqplayer`/`mock_servers::naa` fixtures
//! `tests/hqplayer_outputs_integration.rs` uses.
//!
//! What this file does **not** yet cover: the MCP envelope's `no_orphaned_fields` governance list
//! (`tests/mcp_contract.rs::FIELD_ROLES`) has not been extended for the new response fields these
//! two tools return, so `cargo test --test mcp_contract` is expected to fail on that check until a
//! deliberate pass classifies each one — left undone here rather than rushed. `discover` requires
//! a relay bound to the real NAA discovery port (43210), not an ephemeral `:0` bind; no test below
//! exercises `discover` for that reason.

#![cfg(feature = "naa-proxy")]

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::middleware::from_fn_with_state;
use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Value};
use serial_test::serial;
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use mock_servers::hqplayer::corpus::VERIFIED_PROFILE;
use mock_servers::hqplayer::model::{DaemonModel, Metadata};
use mock_servers::hqplayer::wire::{WirePolicy, WireServer};
use mock_servers::naa::FakeNaa;

use rust_mcp_sdk::mcp_client::{client_runtime, ClientHandler, ClientRuntime};
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, Implementation,
    InitializeRequestParams, LATEST_PROTOCOL_VERSION,
};
use rust_mcp_sdk::{McpClient, RequestOptions, StreamableTransportOptions};

use unified_hifi_control::adapters::hqplayer::naa_relay::HqpOutputTimeouts;
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
use unified_hifi_control::api::{self, AppState};
use unified_hifi_control::bus::runtime::build_runtime;
use unified_hifi_control::bus::{create_bus, SharedBus};
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mcp;

// =============================================================================
// Isolation + shared harness
// =============================================================================

/// Isolates `UHC_CONFIG_DIR`/`UHC_DATA_DIR` per test, unrelated adapters explicitly off, no
/// household calls. `std::env::set_var` is process-global, hence `#[serial]` on every test below.
fn isolate_environment(test_name: &str) {
    let dir = std::env::temp_dir().join(format!(
        "uhc-public-transport-{test_name}-{}",
        std::process::id()
    ));
    let subdir = dir.join("unified-hifi");
    std::fs::create_dir_all(&subdir).expect("create isolated config dir");
    std::fs::write(
        subdir.join("app-settings.json"),
        r#"{"adapters":{"roon":false,"upnp":false,"openhome":false,"lms":false,"hqplayer":true,"spotify":false,"applemusic":false,"musicassistant":false}}"#,
    )
    .expect("write isolated app settings");
    std::env::set_var("UHC_CONFIG_DIR", &dir);
    let data_dir = std::env::temp_dir().join(format!(
        "uhc-public-transport-data-{test_name}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&data_dir).expect("create isolated data dir");
    std::env::set_var("UHC_DATA_DIR", &data_dir);
    std::env::remove_var("UHC_REQUIRE_CONTROLLER_AUTH");
}

fn fast_timeouts() -> HqpTimeouts {
    HqpTimeouts {
        connect: Duration::from_millis(200),
        response: Duration::from_millis(1500),
        reconnect_delay: Duration::from_millis(10),
        max_attempts: 1,
    }
}

fn fast_recovery() -> HqpRecoveryConfig {
    HqpRecoveryConfig {
        poll_interval: Duration::from_millis(20),
        short_retry_delay: Duration::from_millis(10),
        restart_window: Duration::from_millis(40),
        backoff_initial: Duration::from_millis(20),
        backoff_cap: Duration::from_millis(40),
        stable_threshold: Duration::from_millis(20),
    }
}

fn fast_output_timeouts() -> HqpOutputTimeouts {
    HqpOutputTimeouts {
        select_deadline: Duration::from_secs(6),
        control_step: Duration::from_secs(2),
        poll: Duration::from_millis(20),
    }
}

/// A daemon mid-playback with a loaded, seekable track (same shape as
/// `hqplayer_outputs_integration.rs::playing_daemon`, duplicated since separate `tests/*.rs`
/// binaries cannot share code).
fn playing_daemon() -> DaemonModel {
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| {
        s.playback = 2;
        s.track = 3;
        s.track_id = "t-3".to_string();
        s.position = 41;
        s.length = 215;
        s.volume_db = -23.5;
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
        s.metadata = Some(Metadata::sample());
    });
    model
}

async fn start_daemon(model: DaemonModel) -> WireServer {
    WireServer::start(Arc::new(model), WirePolicy::default()).await
}

/// RAII wrapper so a panic mid-test never leaves the daemon's accept loop running: `Drop` calls
/// the synchronous `WireServer::stop` (cancel + abort, no await needed) when a test unwinds before
/// reaching the normal `shutdown().await` path.
struct DaemonGuard(Option<WireServer>);

impl DaemonGuard {
    fn new(server: WireServer) -> Self {
        Self(Some(server))
    }

    fn get(&self) -> &WireServer {
        self.0.as_ref().expect("daemon guard used after shutdown")
    }

    async fn shutdown(mut self) {
        if let Some(server) = self.0.take() {
            server.shutdown().await;
        }
    }
}

impl Drop for DaemonGuard {
    fn drop(&mut self) {
        if let Some(server) = self.0.take() {
            server.stop();
        }
    }
}

struct TestClientHandler;

#[async_trait::async_trait]
impl ClientHandler for TestClientHandler {}

/// Real production `AppState`/runtime/manager composition bound to ONE real loopback listener
/// serving both transports: `/mcp` and the merged `/hqplayer/outputs*` routes, with the real
/// `controller_auth::middleware` layered over the whole thing — the same shape `src/main.rs`
/// assembles, just built locally because `main.rs`'s router lives in the binary crate and is
/// never importable from a test.
struct Rig {
    manager: Arc<HqpInstanceManager>,
    aggregator: Arc<ZoneAggregator>,
    bus: SharedBus,
    aggregator_task: tokio::task::JoinHandle<()>,
    projection_task: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
    client: Arc<ClientRuntime>,
    addr: std::net::SocketAddr,
    http: reqwest::Client,
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
        let runtime = build_runtime(aggregator.clone(), 16, 64);
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

        let mcp_extension = mcp::create_mcp_extension(state.clone());
        let app = Router::new()
            .route("/mcp", get(mcp::handle_mcp_get))
            .route("/mcp", post(mcp::handle_mcp_post))
            .route("/mcp", delete(mcp::handle_mcp_delete))
            .layer(mcp_extension)
            // The exact call `src/main.rs` makes — see `tests/api_contract.rs`'s scanner, which
            // only credits this module's routes to the contract when it finds this literal call
            // in main.rs, so this line staying true is load-bearing for more than this file.
            .merge(api::hqp_outputs_http::routes())
            .layer(from_fn_with_state(
                state.controller_auth.clone(),
                api::controller_auth::middleware,
            ))
            .with_state(state);

        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let addr = listener.local_addr().expect("local addr");
        let server_task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client_details = InitializeRequestParams {
            capabilities: ClientCapabilities::default(),
            client_info: Implementation {
                name: "uhc-public-transport-test-client".into(),
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
            manager,
            aggregator,
            bus,
            aggregator_task,
            projection_task,
            server_task,
            client,
            addr,
            http: reqwest::Client::new(),
        }
    }

    fn zone_id(instance: &str) -> String {
        format!("hqplayer:{instance}")
    }

    fn base_url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Attach a real HQPlayer wire-daemon fixture as a managed instance. Does not start the
    /// managed lifecycle — call `start_all` once every instance for a test is attached, so
    /// multi-instance tests bring both instances up together the way a real UHC process would.
    async fn add(&self, instance: &str, server: &WireServer) -> Arc<HqpAdapter> {
        let adapter = self
            .manager
            .add_instance(
                instance.to_string(),
                "127.0.0.1".to_string(),
                Some(server.port()),
                None,
                None,
                None,
            )
            .await;
        adapter.set_timeouts(fast_timeouts()).await;
        adapter.set_recovery_config(fast_recovery()).await;
        adapter.set_output_timeouts(fast_output_timeouts());
        adapter
    }

    async fn start_all(&self) {
        self.manager.start().await.expect("start managed lifecycle");
    }

    async fn zone_when(
        &self,
        instance: &str,
        mut condition: impl FnMut(&unified_hifi_control::bus::Zone) -> bool,
    ) -> unified_hifi_control::bus::Zone {
        let zone_id = Self::zone_id(instance);
        for _ in 0..200 {
            if let Some(zone) = self.aggregator.get_zone(&zone_id).await {
                if condition(&zone) {
                    return zone;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("zone {zone_id} did not settle in the bounded polling budget");
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
/// (`src/mcp/envelope.rs::Envelope`) — the machine-readable surface this file asserts against,
/// never the human-readable text content.
fn envelope_of(result: &CallToolResult) -> Value {
    result
        .structured_content
        .clone()
        .map(Value::Object)
        .expect("every UHC MCP tool result carries the structured envelope")
}

/// Calls a tool and requires its envelope to report the given `outcome`, then returns `data`.
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

async fn read_outputs(rig: &Rig, instance: &str) -> Value {
    expect_envelope_data(
        rig,
        "hifi_hqplayer_outputs",
        json!({"zone_id": Rig::zone_id(instance)}),
        "ok",
    )
    .await
}

/// Polls the projection until `condition` holds. The projection commit pipeline (adapter -> bus
/// -> aggregator -> projection actor) is asynchronous, so a single read right after a relay-side
/// event (audio observed at the endpoint fixture) can still race the committed document — this is
/// what `outputs_when` in `hqplayer_outputs_integration.rs` exists to avoid, reused here for the
/// same reason.
async fn read_outputs_when(
    rig: &Rig,
    instance: &str,
    mut condition: impl FnMut(&Value) -> bool,
) -> Value {
    for _ in 0..80 {
        let projection = read_outputs(rig, instance).await;
        if condition(&projection) {
            return projection;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("projection for {instance} never satisfied the condition within the bounded budget");
}

fn operation_id(receipt: &Value) -> String {
    receipt["operation"]["operation_id"]
        .as_str()
        .unwrap_or_else(|| panic!("receipt carries an operation_id: {receipt}"))
        .to_string()
}

/// Polls `hifi_hqplayer_outputs{zone_id, operation_id}` until `outcome` is set. An accepted
/// receipt is only an admission; this is what "await terminal" means in every test below.
async fn await_terminal_operation(rig: &Rig, instance: &str, operation_id: &str) -> Value {
    for _ in 0..80 {
        let operation = expect_envelope_data(
            rig,
            "hifi_hqplayer_outputs",
            json!({"zone_id": Rig::zone_id(instance), "operation_id": operation_id}),
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

/// Reads the current `(source_epoch, output_revision)` fence pair the next mutation must echo.
async fn current_fence(rig: &Rig, instance: &str) -> (u64, u64) {
    let projection = read_outputs(rig, instance).await;
    (
        projection["source_epoch"].as_u64().expect("source_epoch"),
        projection["output_revision"]
            .as_u64()
            .expect("output_revision"),
    )
}

// =============================================================================
// 1. configure -> await -> route add -> select -> real NAA stream (exact payload/side/feedback)
//    -> Stop, over the real bound MCP transport.
// =============================================================================

#[tokio::test]
#[serial]
async fn mcp_configure_route_add_select_streams_real_naa_audio_then_stop() {
    let model = playing_daemon();
    let daemon = DaemonGuard::new(start_daemon(model.clone()).await);
    let rig = Rig::new("configure-select-stream").await;
    rig.add("living", daemon.get()).await;
    rig.start_all().await;
    rig.zone_when("living", |_| true).await;

    // 1. Configure the opt-in relay on a known loopback bind.
    let (epoch, revision) = current_fence(&rig, "living").await;
    let configure_receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({
            "zone_id": Rig::zone_id("living"),
            "action": "relay_configure",
            "enabled": true,
            "bind": "127.0.0.1:0",
            "hqp_allow": [],
            "discovery_interface": null,
            "expected_source_epoch": epoch,
            "expected_output_revision": revision,
        }),
        "accepted",
    )
    .await;
    let configured =
        await_terminal_operation(&rig, "living", &operation_id(&configure_receipt)).await;
    assert_eq!(
        configured["outcome"], "complete",
        "binding a fresh loopback listener must complete: {configured}"
    );

    let after_configure = read_outputs(&rig, "living").await;
    assert_eq!(after_configure["availability"], "available");
    let bind = after_configure["relay"]["bind"]
        .as_str()
        .expect("bound relay reports its actual bind address")
        .to_string();
    assert!(
        bind.starts_with("127.0.0.1:"),
        "expected a loopback bind, got {bind}"
    );
    let relay_addr: std::net::SocketAddr = bind.parse().expect("bind address parses");

    // 2. Add a route to a real software NAA endpoint fixture.
    let naa = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let (epoch, revision) = current_fence(&rig, "living").await;
    let add_receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({
            "zone_id": Rig::zone_id("living"),
            "action": "route_add",
            "name": "A",
            "host": naa.host(),
            "port": naa.port(),
            "device_id": null,
            "expected_source_epoch": epoch,
            "expected_output_revision": revision,
        }),
        "accepted",
    )
    .await;
    let added = await_terminal_operation(&rig, "living", &operation_id(&add_receipt)).await;
    assert_eq!(
        added["outcome"], "complete",
        "route_add must complete: {added}"
    );
    let route_id = added["route_id"]
        .as_str()
        .expect("route_add reports the stable route id")
        .to_string();

    // 3. Select the route BEFORE any relay session exists — intentionally, even with a
    // mid-playback daemon: with nothing connected through the relay yet there is nothing to stop,
    // so this must complete with no native traffic at all.
    let (epoch, revision) = current_fence(&rig, "living").await;
    let select_receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({
            "zone_id": Rig::zone_id("living"),
            "action": "select",
            "route_id": route_id,
            "expected_source_epoch": epoch,
            "expected_output_revision": revision,
        }),
        "accepted",
    )
    .await;
    let selected = await_terminal_operation(&rig, "living", &operation_id(&select_receipt)).await;
    assert_eq!(
        selected["outcome"], "complete",
        "select must complete: {selected}"
    );
    assert_eq!(
        model.request_count("Stop"),
        0,
        "selecting onto an idle relay issues no native Stop"
    );
    assert_eq!(
        model.request_count("Play"),
        0,
        "selecting onto an idle relay issues no native Play"
    );

    // 4. A real HQPlayer-side NAA client (`mock_servers::naa::HqpNaaClient`, the same driver
    // `hqplayer_outputs_integration.rs` uses) handshakes through the bound relay and streams one
    // record with nonempty side sections, so the exact PCM payload, the exact nonempty
    // position/metadata side bytes at the endpoint, and the exact feedback bytes relayed back to
    // the client are all verifiable byte-for-byte. Run on a blocking thread: this is synchronous
    // `std::net` I/O and must not block the runtime the rig's own async tasks share. The client is
    // returned out of the blocking closure and held alive here, deliberately — dropping it (as an
    // earlier version of this test did) closes its socket immediately after the one send, which
    // races (and can lose) the relay's own projection commit of that session; keeping the same
    // connection open across the polling read below, and through Stop, is what proves Stop closes
    // a session that was genuinely still live.
    let payload = mock_servers::naa::auto_client_payload();
    let position: Vec<u8> = b"position-41".to_vec();
    let metadata: Vec<u8> = b"track-3-metadata".to_vec();
    let (payload_in, position_in, metadata_in) =
        (payload.clone(), position.clone(), metadata.clone());
    let (mut client, start_reply, feedback_received) = tokio::task::spawn_blocking(move || {
        let mut client =
            mock_servers::naa::HqpNaaClient::connect(relay_addr, "public-transport-nonce")
                .expect("connect to the bound relay");
        client
            .handshake()
            .expect("auth/getdevices/initialize/getformats through the relay");
        let start_reply = client.start(44100).expect("start through the relay");
        let feedback = client
            .send_audio_with_sections(&payload_in, &position_in, &metadata_in)
            .expect("send one audio record with side sections through the relay");
        (client, start_reply, feedback)
    })
    .await
    .expect("blocking NAA client task must not panic");
    assert_eq!(
        mock_servers::naa::attribute(&start_reply, "result").as_deref(),
        Some("1"),
        "start must be accepted: {}",
        String::from_utf8_lossy(&start_reply)
    );

    assert!(
        naa.wait_until(|n| !n.audio_records().is_empty(), Duration::from_secs(5)),
        "the selected endpoint must observe the forwarded record"
    );
    let records = naa.audio_records();
    assert_eq!(records.len(), 1, "exactly one record was sent");
    let record = &records[0];
    assert_eq!(
        record.payload, payload,
        "exact PCM payload, byte-for-byte, no rewrite"
    );
    assert_eq!(
        record.position, position,
        "exact nonempty position side section, byte-for-byte"
    );
    assert_eq!(
        record.metadata, metadata,
        "exact nonempty metadata side section, byte-for-byte"
    );
    assert!(
        record.picture.is_empty(),
        "HqpNaaClient::send_audio_with_sections never sets a picture section; none may appear: {record:?}"
    );
    assert_eq!(
        Some(feedback_received),
        naa.last_feedback_bytes(),
        "the feedback the client actually received must equal exactly what the endpoint sent, relayed with no rewrite"
    );

    let forwarding = read_outputs_when(&rig, "living", |p| {
        p["session"]["current_stream_audio_bytes"]
            .as_u64()
            .unwrap_or(0)
            > 0
    })
    .await;
    assert_eq!(forwarding["selected_route_id"], route_id);
    assert_eq!(
        forwarding["session"]["started"], true,
        "the committed projection's own session must confirm audio, not just the fixture: {forwarding}"
    );

    // 5. Stop.
    let stop_receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({"zone_id": Rig::zone_id("living"), "action": "stop"}),
        "accepted",
    )
    .await;
    let stopped = await_terminal_operation(&rig, "living", &operation_id(&stop_receipt)).await;
    assert!(
        matches!(stopped["outcome"].as_str(), Some("complete" | "partial")),
        "stop must complete (or be partial on native confirmation only): {stopped}"
    );
    let after_stop = read_outputs(&rig, "living").await;
    assert_eq!(
        after_stop["selected_route_id"],
        Value::Null,
        "Stop clears the selection"
    );
    assert_eq!(
        after_stop["session"],
        Value::Null,
        "Stop closes the relay pair"
    );

    // Stop must close the pair: the still-live downstream client observes its socket closed,
    // rather than this test only inferring it from the projection.
    let closed = tokio::task::spawn_blocking(move || {
        client.set_read_timeout(Duration::from_secs(2));
        client.is_closed()
    })
    .await
    .expect("blocking close-check task must not panic");
    assert!(
        closed,
        "Stop must close the relay pair; the downstream client's socket must observe it"
    );

    naa.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

// =============================================================================
// 2. Exact two-instance targeting: a command aimed at one instance never touches another, and an
//    unattached instance is refused rather than silently routed.
// =============================================================================

#[tokio::test]
#[serial]
async fn mcp_targets_the_exact_instance_across_two_real_instances() {
    let alpha_daemon = DaemonGuard::new(start_daemon(playing_daemon()).await);
    let beta_daemon = DaemonGuard::new(start_daemon(playing_daemon()).await);
    let rig = Rig::new("two-instance-targeting").await;
    rig.add("alpha", alpha_daemon.get()).await;
    rig.add("beta", beta_daemon.get()).await;
    rig.start_all().await;
    rig.zone_when("alpha", |_| true).await;
    rig.zone_when("beta", |_| true).await;

    // An instance that was never attached is refused as unknown, never silently routed to
    // whichever real instance happens to exist.
    let ghost = rig
        .call_tool(
            "hifi_hqplayer_output_control",
            json!({"zone_id": "hqplayer:ghost", "action": "stop"}),
        )
        .await;
    let ghost_envelope = envelope_of(&ghost);
    assert_eq!(
        ghost_envelope["outcome"], "invalid",
        "an unknown instance must be refused, not routed to a real one: {ghost_envelope}"
    );
    assert_eq!(ghost_envelope["refusal"]["reason"], "unknown_target");
    assert_eq!(ghost_envelope["data"]["error_code"], "UNKNOWN_INSTANCE");

    // Configure ONLY alpha's relay.
    let (epoch, revision) = current_fence(&rig, "alpha").await;
    let configure_receipt = expect_envelope_data(
        &rig,
        "hifi_hqplayer_output_control",
        json!({
            "zone_id": Rig::zone_id("alpha"),
            "action": "relay_configure",
            "enabled": true,
            "bind": "127.0.0.1:0",
            "hqp_allow": [],
            "discovery_interface": null,
            "expected_source_epoch": epoch,
            "expected_output_revision": revision,
        }),
        "accepted",
    )
    .await;
    let configured =
        await_terminal_operation(&rig, "alpha", &operation_id(&configure_receipt)).await;
    assert_eq!(configured["outcome"], "complete", "{configured}");

    let alpha_after = read_outputs(&rig, "alpha").await;
    assert_eq!(alpha_after["availability"], "available");

    // beta must be completely untouched: it was never configured, and it recorded nothing from a
    // command that only ever named alpha.
    let beta_after = read_outputs(&rig, "beta").await;
    assert_ne!(
        beta_after["availability"],
        json!("available"),
        "beta's own relay was never configured; it must not have picked up alpha's: {beta_after}"
    );
    assert!(
        beta_after["operations"]
            .as_array()
            .is_some_and(|ops| ops.is_empty()),
        "a command aimed only at alpha must record nothing on beta: {beta_after}"
    );

    alpha_daemon.shutdown().await;
    beta_daemon.shutdown().await;
    rig.shutdown().await;
}

// =============================================================================
// 3. Correlation retry produces no second effect; a changed request under the same id is refused.
// =============================================================================

#[tokio::test]
#[serial]
async fn mcp_correlation_retry_produces_no_second_effect_and_a_changed_request_is_refused() {
    let daemon = DaemonGuard::new(start_daemon(playing_daemon()).await);
    let rig = Rig::new("correlation-retry").await;
    rig.add("living", daemon.get()).await;
    rig.start_all().await;
    rig.zone_when("living", |_| true).await;

    let naa = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let (epoch, revision) = current_fence(&rig, "living").await;
    let add = |name: &str| {
        json!({
            "zone_id": Rig::zone_id("living"),
            "action": "route_add",
            "name": name,
            "host": naa.host(),
            "port": naa.port(),
            "device_id": null,
            "correlation_id": "ui-add-1",
            "expected_source_epoch": epoch,
            "expected_output_revision": revision,
        })
    };

    let first =
        expect_envelope_data(&rig, "hifi_hqplayer_output_control", add("A"), "accepted").await;
    let first_op = operation_id(&first);

    // Same correlation id, identical fingerprint: returns the SAME operation, no second route.
    let repeat =
        expect_envelope_data(&rig, "hifi_hqplayer_output_control", add("A"), "accepted").await;
    assert_eq!(
        operation_id(&repeat),
        first_op,
        "an identical retry must return the original operation, not admit a new one"
    );
    let after_repeat = read_outputs(&rig, "living").await;
    assert_eq!(
        after_repeat["routes"].as_array().map(Vec::len),
        Some(1),
        "a retried route_add must not create a second route: {after_repeat}"
    );

    // Same correlation id, DIFFERENT fingerprint (name changed): refused, no side effect, and the
    // refusal's structured `data` — not the frozen prose — carries the real code so a client never
    // has to parse text to know this was CORRELATION_CONFLICT.
    let conflict = rig
        .call_tool("hifi_hqplayer_output_control", add("B"))
        .await;
    let conflict_envelope = envelope_of(&conflict);
    assert_eq!(
        conflict_envelope["outcome"], "error",
        "a changed request under a reused correlation_id must be refused: {conflict_envelope}"
    );
    assert_eq!(
        conflict_envelope["data"]["error_code"],
        "CORRELATION_CONFLICT"
    );

    let after_conflict = read_outputs(&rig, "living").await;
    assert_eq!(
        after_conflict["routes"].as_array().map(Vec::len),
        Some(1),
        "a refused conflicting request must leave the route inventory unchanged: {after_conflict}"
    );

    naa.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

// =============================================================================
// 4. The real HTTP routes (`api::hqp_outputs_http::routes()`, merged the same way main.rs merges
//    them) serve the same shared service, reject malformed requests with structured errors
//    instead of Axum's bare-text default, and the real controller-auth middleware gates the
//    mutation.
// =============================================================================

#[tokio::test]
#[serial]
async fn http_real_routes_serve_the_shared_service_and_enforce_controller_auth() {
    let daemon = DaemonGuard::new(start_daemon(playing_daemon()).await);
    let rig = Rig::new("http-transport").await;
    rig.add("living", daemon.get()).await;
    rig.start_all().await;
    rig.zone_when("living", |_| true).await;

    let outputs_url = format!(
        "{}/hqplayer/outputs?zone_id=hqplayer:living",
        rig.base_url()
    );
    let get_response = rig
        .http
        .get(&outputs_url)
        .send()
        .await
        .expect("GET /hqplayer/outputs");
    assert_eq!(get_response.status(), reqwest::StatusCode::OK);
    let projection: Value = get_response.json().await.expect("projection body");
    let epoch = projection["source_epoch"].as_u64().expect("source_epoch");
    let revision = projection["output_revision"]
        .as_u64()
        .expect("output_revision");

    // A missing required query parameter gets a structured contract error, not Axum's bare-text
    // rejection body.
    let malformed = rig
        .http
        .get(format!("{}/hqplayer/outputs", rig.base_url()))
        .send()
        .await
        .expect("GET without zone_id");
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
    let malformed_body: Value = malformed
        .json()
        .await
        .expect("a structured JSON error body, not plain text");
    assert_eq!(malformed_body["error_code"], "INVALID_ZONE_ID");

    let command_url = format!("{}/hqplayer/outputs/command", rig.base_url());

    // A body that isn't a valid command gets INVALID_COMMAND, structured the same way.
    let malformed_command = rig
        .http
        .post(&command_url)
        .json(&json!({"not": "a command"}))
        .send()
        .await
        .expect("POST malformed body");
    assert_eq!(malformed_command.status(), reqwest::StatusCode::BAD_REQUEST);
    let malformed_command_body: Value = malformed_command
        .json()
        .await
        .expect("a structured JSON error body, not plain text");
    assert_eq!(malformed_command_body["error_code"], "INVALID_COMMAND");

    // The real mutation, over real HTTP, through the real merged route — the same shared service
    // the MCP tests call, proving one backend behind two transports rather than two.
    let command_body = json!({
        "zone_id": "hqplayer:living",
        "action": "relay_configure",
        "enabled": true,
        "bind": "127.0.0.1:0",
        "hqp_allow": [],
        "discovery_interface": null,
        "expected_source_epoch": epoch,
        "expected_output_revision": revision,
    });
    let accepted = rig
        .http
        .post(&command_url)
        .json(&command_body)
        .send()
        .await
        .expect("POST relay_configure");
    assert_eq!(accepted.status(), reqwest::StatusCode::OK);
    let receipt: Value = accepted.json().await.expect("receipt body");
    assert_eq!(receipt["accepted"], true);
    let op_id = operation_id(&receipt);

    let operation_url = format!(
        "{}/hqplayer/outputs/operation?zone_id=hqplayer:living&operation_id={op_id}",
        rig.base_url()
    );
    let mut terminal = None;
    for _ in 0..80 {
        let response = rig
            .http
            .get(&operation_url)
            .send()
            .await
            .expect("GET operation");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body: Value = response.json().await.expect("operation body");
        if body.get("outcome").is_some_and(|o| !o.is_null()) {
            terminal = Some(body);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let terminal =
        terminal.expect("operation reached a terminal outcome within the bounded budget");
    assert_eq!(terminal["outcome"], "complete", "{terminal}");

    // Controller auth: with the gate on and no session, the exact same protected mutation route
    // is refused before touching the relay — the real `controller_auth::middleware`/`is_protected`
    // layered over the exact same `routes()` merge main.rs uses, so this proves the production
    // gate, not a stand-in.
    std::env::set_var("UHC_REQUIRE_CONTROLLER_AUTH", "1");
    let refused = rig.http.post(&command_url).json(&command_body).send().await;
    std::env::remove_var("UHC_REQUIRE_CONTROLLER_AUTH");
    let refused = refused.expect("POST while auth is required");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "an unauthenticated protected mutation must be refused"
    );

    daemon.shutdown().await;
    rig.shutdown().await;
}
