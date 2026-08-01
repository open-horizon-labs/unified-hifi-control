//! MCP surface validation for the direct HQPlayer zone (#401, HQPlayer-scoped slice).
//!
//! **Written from the client's expectation, not from the implementation.** The client here is an
//! MCP client (Claude, or any other assistant) calling `hifi_control`, `hifi_zones`,
//! `hifi_now_playing` and the `hifi_hqplayer_*` tools exactly as documented in AGENTS.md — the same
//! role the ESP32/iOS harness (`tests/client_harness.rs`) and the knob-first suite
//! (`tests/hqplayer_direct_zone.rs`) play for the other surfaces. This file exists because, before
//! it, **no test exercised `src/mcp/mod.rs` at all**.
//!
//! The full `/mcp` HTTP surface is driven over a real bound TCP port with `rust-mcp-sdk`'s own
//! client runtime (streamable HTTP), not by calling the handler's Rust methods directly — a real
//! MCP client is the thing this surface has to keep working for, and no test proved that until now.
//!
//! No live HQPlayer is contacted. The daemon is the stateful wire/model fake from
//! `tests/mock_servers/hqplayer`, the same fake `tests/hqplayer_direct_zone.rs` and the protocol
//! conformance suite (#322) drive.
//!
//! **RED before this issue:** `HifiTools::HifiControlTool` has its own zone-id routing switch,
//! independent of `knob_control_handler`, and it has the same "falls through to Roon" defect #391
//! fixed in the knob surface — because #391 never touched this file. MCP volume has no `hqplayer:`
//! arm at all, and `seek`/`stop`/`mute` are not recognized action values.

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use axum::routing::{delete, get, post};
use axum::Router;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use mock_servers::hqplayer::corpus::VERIFIED_PROFILE;
use mock_servers::hqplayer::model::{request_attr, DaemonModel, Metadata};
use mock_servers::hqplayer::wire::{WirePolicy, WireServer};

use rust_mcp_sdk::mcp_client::{client_runtime, ClientHandler, ClientRuntime};
use rust_mcp_sdk::schema::{
    CallToolRequestParams, CallToolResult, ClientCapabilities, Implementation,
    InitializeRequestParams, LATEST_PROTOCOL_VERSION,
};
use rust_mcp_sdk::{McpClient, RequestOptions, StreamableTransportOptions};

use unified_hifi_control::adapters::hqplayer::{
    HqpAdapter, HqpInstanceManager, HqpRecoveryConfig, HqpTimeouts, HqpZoneLinkService,
};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::{create_bus, BusEvent, PlaybackState, SharedBus};
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mcp;

// =============================================================================
// Harness
// =============================================================================

/// Isolated from the developer's real config, and from `tests/hqplayer_direct_zone.rs`'s own
/// isolated directory (distinct process-wide path, same reasoning as that file's `isolate_config_dir`).
fn isolate_config_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("uhc-mcp-hqp-direct-{}", std::process::id()));
        let subdir = dir.join("unified-hifi");
        std::fs::create_dir_all(&subdir).expect("create isolated mcp config dir");
        std::fs::write(
            subdir.join("app-settings.json"),
            r#"{"adapters":{"roon":true,"upnp":false,"openhome":false,"lms":false,"hqplayer":true}}"#,
        )
        .expect("write isolated app settings");
        std::env::set_var("UHC_CONFIG_DIR", dir);
    });
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

/// No overrides: every default response is exactly what `ClientHandler`'s trait defaults do
/// (respond to pings, ignore notifications), which is all a `hifi_control` caller needs.
struct TestClientHandler;

#[async_trait::async_trait]
impl ClientHandler for TestClientHandler {}

struct Rig {
    manager: Arc<HqpInstanceManager>,
    aggregator: Arc<ZoneAggregator>,
    bus: SharedBus,
    aggregator_task: tokio::task::JoinHandle<()>,
    server_task: tokio::task::JoinHandle<()>,
    client: Arc<ClientRuntime>,
}

impl Rig {
    /// Build the surface stack over a disconnected Roon adapter, exactly like
    /// `tests/hqplayer_direct_zone.rs::Rig`, but serving MCP over a real bound port instead of
    /// mounting the knob HTTP routes.
    async fn new() -> Self {
        isolate_config_dir();
        let bus = create_bus();
        let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
        let aggregator_task = {
            let aggregator = aggregator.clone();
            tokio::spawn(async move { aggregator.run().await })
        };
        tokio::task::yield_now().await;

        let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
        let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
        let manager = Arc::new(HqpInstanceManager::new(bus.clone()));
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
        );

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
                name: "uhc-mcp-test-client".into(),
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
            server_task,
            client,
        }
    }

    /// Attach a named managed instance to a wire daemon and run it to a published zone.
    async fn attach_named(&self, name: &str, server: &WireServer) -> Arc<HqpAdapter> {
        let adapter = self
            .manager
            .add_instance(
                name.to_string(),
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

    /// Attach the conventional `rig` instance used by the direct-zone tests.
    async fn attach(&self, server: &WireServer) -> Arc<HqpAdapter> {
        self.attach_named("rig", server).await
    }

    /// Attach a wire daemon as the **default** instance — the one MCP's single-instance
    /// `hifi_hqplayer_*` tools (status/profiles/pipeline) always operate on, since none of those
    /// tools take a zone/instance parameter.
    async fn attach_default(&self, server: &WireServer) -> Arc<HqpAdapter> {
        let adapter = self
            .manager
            .add_instance(
                "default".to_string(),
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
        for _ in 0..200 {
            if let Some(zone) = self.aggregator.get_zone("hqplayer:rig").await {
                if condition(&zone) {
                    return zone;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!(
            "zone never satisfied the condition inside the bounded budget; zones={:?}",
            self.aggregator.get_zones().await
        );
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
        self.bus.publish(BusEvent::ShuttingDown { reason: None });
        let _ = self.aggregator_task.await;
        let _ = self.client.shut_down().await;
        self.server_task.abort();
    }
}

fn text_of(result: &CallToolResult) -> String {
    result
        .content
        .first()
        .and_then(|c| c.as_text_content().ok())
        .map(|t| t.text.clone())
        .unwrap_or_default()
}

/// A daemon mid-playback with a loaded, seekable track and a working decimal-dB control.
fn playing_daemon() -> DaemonModel {
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| {
        s.playback = 2;
        s.track = 3;
        s.track_id = "t-3".to_string();
        s.position = 42;
        s.length = 215;
        s.volume_db = -23.5;
        s.volume_range.min_db = -60.0;
        s.volume_range.max_db = 0.0;
        s.metadata = Some(Metadata::sample());
    });
    model
}

async fn start_daemon(model: &DaemonModel) -> WireServer {
    WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await
}

// =============================================================================
// coverage — what already works through the aggregator (not RED; regression pins)
// =============================================================================

/// **Client expectation.** `hifi_zones` and `hifi_now_playing` already read
/// `state.aggregator`, so the direct HQPlayer zone #328 made truthful is visible through MCP with no
/// further work. This test exists because nothing previously proved it — a silent regression here
/// would have been as invisible as the routing defect this file was written to catch.
#[tokio::test]
async fn mcp_zones_and_now_playing_already_report_the_direct_hqplayer_zone() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let zones_result = rig.call_tool("hifi_zones", json!({})).await;
    let zones_json: Value =
        serde_json::from_str(&text_of(&zones_result)).expect("hifi_zones returns JSON");
    let zones = zones_json.as_array().expect("zones array");
    assert!(
        zones
            .iter()
            .any(|z| z["zone_id"] == "hqplayer:rig" && z["state"] == "playing"),
        "hifi_zones must list the direct HQPlayer zone as playing: {zones_json}"
    );

    let now_playing_result = rig
        .call_tool("hifi_now_playing", json!({"zone_id": "hqplayer:rig"}))
        .await;
    let now_playing: Value =
        serde_json::from_str(&text_of(&now_playing_result)).expect("hifi_now_playing returns JSON");
    assert_eq!(now_playing["state"], "playing");
    assert_eq!(now_playing["volume"], -23.5);

    rig.shutdown().await;
    server.stop();
}

// =============================================================================
// routing — the headline defect, and #328's "no fallback to Roon" constraint through MCP
// =============================================================================

/// **Client expectation.** An MCP client calling `hifi_control` with `zone_id: "hqplayer:rig"`
/// expects that daemon to pause — not Roon.
///
/// **RED:** `HifiTools::HifiControlTool` dispatches `lms:`, `openhome:`, `upnp:` and falls through
/// to `self.state.roon.control(...)` for everything else, including `hqplayer:`, with a disconnected
/// Roon adapter that cannot silently succeed.
#[tokio::test]
async fn mcp_transport_reaches_the_selected_hqplayer_instance_and_not_roon() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let before = model.request_count("Pause");
    let result = rig
        .call_tool(
            "hifi_control",
            json!({"zone_id": "hqplayer:rig", "action": "pause"}),
        )
        .await;
    let text = text_of(&result);
    assert!(
        !text.starts_with("Error"),
        "pause on a direct HQPlayer zone must succeed via MCP; got: {text}"
    );
    assert_eq!(
        model.request_count("Pause"),
        before + 1,
        "the selected daemon must have received exactly one Pause"
    );
    rig.zone_when(|z| z.state == PlaybackState::Paused).await;

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** Every transport verb the direct zone advertises routes through MCP, not
/// just play/pause.
///
/// **RED:** `stop`, `seek` are not recognized `hifi_control` action values at all (they fall into
/// the generic pass-through and are sent as an unknown action to Roon); `next`/`previous` fall
/// through to Roon exactly like `pause` above.
#[tokio::test]
async fn mcp_stop_next_previous_and_seek_all_route_to_the_hqplayer_daemon() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    for (action, element, value) in [
        ("next", "Next", None),
        ("previous", "Previous", None),
        ("seek", "Seek", Some(90.0)),
        ("stop", "Stop", None),
    ] {
        let before = model.request_count(element);
        let mut args = json!({"zone_id": "hqplayer:rig", "action": action});
        if let Some(v) = value {
            args["value"] = json!(v);
        }
        let result = rig.call_tool("hifi_control", args).await;
        let text = text_of(&result);
        assert!(
            !text.starts_with("Error"),
            "action `{action}` must route to HQPlayer via MCP; got: {text}"
        );
        assert_eq!(
            model.request_count(element),
            before + 1,
            "action `{action}` must send exactly one `{element}`"
        );
    }

    let seek = model
        .last_request("Seek")
        .expect("the daemon recorded the Seek");
    assert_eq!(
        request_attr(&seek, "position").as_deref(),
        Some("90"),
        "the requested position must reach the wire unmodified: {seek}"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** `playpause` — the MCP tool's documented toggle spelling — resolves
/// against the state most recently observed from the daemon, not a guess.
#[tokio::test]
async fn mcp_playpause_resolves_against_the_published_state() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let result = rig
        .call_tool(
            "hifi_control",
            json!({"zone_id": "hqplayer:rig", "action": "playpause"}),
        )
        .await;
    assert!(!text_of(&result).starts_with("Error"));
    assert_eq!(
        model.request_count("Pause"),
        1,
        "playing + playpause must pause"
    );
    assert_eq!(model.request_count("Play"), 0);
    rig.zone_when(|z| z.state == PlaybackState::Paused).await;

    rig.shutdown().await;
    server.stop();
}

// =============================================================================
// volume — MCP had no HQPlayer arm at all
// =============================================================================

/// **Client expectation.** `volume_set` on a direct HQPlayer zone writes the requested decimal-dB
/// level to *that* daemon, clamped to its observed range.
///
/// **RED:** the `set_volume` helper recognizes only `lms:` and `roon:`/unprefixed zone ids; a
/// `hqplayer:` zone id hits the explicit `else` branch and is refused with "Volume control not
/// supported for this zone type" even though the zone has a working decimal-dB control.
#[tokio::test]
async fn mcp_volume_set_routes_to_the_hqplayer_instance_and_clamps_to_the_observed_range() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.volume_control.is_some()).await;

    let result = rig
        .call_tool(
            "hifi_control",
            json!({"zone_id": "hqplayer:rig", "action": "volume_set", "value": 12.0}),
        )
        .await;
    let text = text_of(&result);
    assert!(
        !text.starts_with("Error"),
        "volume_set on a direct HQPlayer zone must succeed via MCP; got: {text}"
    );
    assert_eq!(
        request_attr(
            &model.last_request("Volume").expect("a Volume write"),
            "value"
        )
        .as_deref(),
        Some("0"),
        "+12 dB must clamp to the observed maximum of 0 dB, not pass through unclamped"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** `volume_up`/`volume_down` on a direct HQPlayer zone move relative to the
/// last observed level via the same `hqplayer:` arm `volume_set` uses — not the old `set_volume`
/// helper, which never had a `hqplayer:` branch for any of the three volume actions.
#[tokio::test]
async fn mcp_volume_up_and_down_route_to_the_hqplayer_instance() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.volume_control.is_some()).await;

    let up = rig
        .call_tool(
            "hifi_control",
            json!({"zone_id": "hqplayer:rig", "action": "volume_up"}),
        )
        .await;
    assert!(
        !text_of(&up).starts_with("Error"),
        "volume_up must succeed via MCP"
    );
    let after_up: f64 = request_attr(
        &model
            .last_request("Volume")
            .expect("a Volume write from volume_up"),
        "value",
    )
    .expect("numeric value")
    .parse()
    .expect("value parses as a number");
    assert!(
        after_up > -23.5,
        "volume_up must move the level up from -23.5 dB, got {after_up}"
    );

    let down = rig
        .call_tool(
            "hifi_control",
            json!({"zone_id": "hqplayer:rig", "action": "volume_down"}),
        )
        .await;
    assert!(
        !text_of(&down).starts_with("Error"),
        "volume_down must succeed via MCP"
    );
    let after_down: f64 = request_attr(
        &model
            .last_request("Volume")
            .expect("a Volume write from volume_down"),
        "value",
    )
    .expect("numeric value")
    .parse()
    .expect("value parses as a number");
    assert!(
        after_down < after_up,
        "volume_down must move the level down from {after_up} dB, got {after_down}"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation, and the safety-critical one.** A volume command through MCP with no
/// usable level is refused — never turned into a number, and never sent to the daemon. Mirrors
/// `tests/hqplayer_direct_zone.rs::a_missing_or_unparseable_level_is_refused_never_defaulted` for
/// the MCP surface, which has its own, independent volume dispatch.
#[tokio::test]
async fn mcp_a_missing_volume_value_is_refused_never_defaulted() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.volume_control.is_some()).await;

    let before = model.request_count("Volume");
    let result = rig
        .call_tool(
            "hifi_control",
            json!({"zone_id": "hqplayer:rig", "action": "volume_set"}),
        )
        .await;
    assert!(
        text_of(&result).starts_with("Error"),
        "an absolute volume with no usable level must be refused, not defaulted"
    );
    assert_eq!(
        model.request_count("Volume"),
        before,
        "no level may be written to the daemon when none was supplied"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** `mute` is a recognized MCP action for a direct HQPlayer zone, routes to
/// that daemon's verified absolute floor move, and is reported back from the observable floor.
///
/// **RED:** `mute` is not a recognized `hifi_control` action value at all before this issue.
#[tokio::test]
async fn mcp_mute_routes_to_the_hqplayer_instance_and_is_reported_from_the_floor() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    let zone = rig.zone_when(|z| z.volume_control.is_some()).await;
    assert!(
        !zone.volume_control.expect("control").is_muted,
        "-23.5 dB is not muted"
    );

    let result = rig
        .call_tool(
            "hifi_control",
            json!({"zone_id": "hqplayer:rig", "action": "mute"}),
        )
        .await;
    assert!(
        !text_of(&result).starts_with("Error"),
        "mute must succeed via MCP"
    );
    assert_eq!(model.request_count("VolumeMute"), 1);

    rig.zone_when(|z| {
        z.volume_control
            .as_ref()
            .map(|v| v.is_muted)
            .unwrap_or(false)
    })
    .await;

    rig.shutdown().await;
    server.stop();
}

// =============================================================================
// routing — HQPlayer-specific management tools, exact managed instance
// =============================================================================

/// **Client expectation.** Omitting `zone_id` preserves the original default-instance behavior.
#[tokio::test]
async fn mcp_hqplayer_status_profiles_and_pipeline_tools_reach_the_default_instance() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach_default(&server).await;

    for _ in 0..200 {
        let status = rig.call_tool("hifi_hqplayer_status", json!({})).await;
        let parsed: Value = serde_json::from_str(&text_of(&status)).unwrap_or(Value::Null);
        if parsed["connected"] == true {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let status = rig.call_tool("hifi_hqplayer_status", json!({})).await;
    let status_json: Value = serde_json::from_str(&text_of(&status)).expect("status JSON");
    assert_eq!(status_json["connected"], true, "status={status_json}");

    let profiles = rig.call_tool("hifi_hqplayer_profiles", json!({})).await;
    let profiles_text = text_of(&profiles);
    assert!(
        profiles_text.contains("Web credentials not configured"),
        "a fresh MCP read must query the selected instance and report why its web lane is unavailable, not return a fabricated empty list: {profiles_text}"
    );

    let before = model.request_count("SetFilter");
    let set_pipeline = rig
        .call_tool(
            "hifi_hqplayer_set_pipeline",
            json!({"setting": "filter1x", "value": "poly-sinc-xtr"}),
        )
        .await;
    let text = text_of(&set_pipeline);
    assert!(
        !text.starts_with("Error"),
        "a valid pipeline setting must be accepted; got: {text}"
    );
    assert!(
        model.request_count("SetFilter") > before,
        "set_pipeline(filter1x) must reach the daemon as a SetFilter write"
    );

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** A zone returned by `hifi_zones` is a complete target for every
/// HQPlayer MCP operation, not just transport and volume. In particular, a pipeline write naming
/// `hqplayer:rig` must not leak to the manager's default instance.
///
/// **RED before #209:** all four `hifi_hqplayer_*` tools had no `zone_id` schema and read/wrote
/// `state.hqplayer`, the default compatibility adapter, unconditionally.
#[tokio::test]
async fn mcp_advanced_hqplayer_tools_target_the_named_instance_not_default() {
    let default_model = playing_daemon();
    default_model.external_change(|s| s.filter_nx_index = 0);
    let rig_model = playing_daemon();
    rig_model.external_change(|s| s.filter_nx_index = 1);
    let default_server = start_daemon(&default_model).await;
    let rig_server = start_daemon(&rig_model).await;
    let rig = Rig::new().await;
    rig.attach_default(&default_server).await;
    rig.attach_named("rig", &rig_server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let default_status = rig.call_tool("hifi_hqplayer_status", json!({})).await;
    let named_status = rig
        .call_tool("hifi_hqplayer_status", json!({"zone_id": "hqplayer:rig"}))
        .await;
    let default_json: Value =
        serde_json::from_str(&text_of(&default_status)).expect("default status JSON");
    let named_json: Value =
        serde_json::from_str(&text_of(&named_status)).expect("named status JSON");
    assert_ne!(
        default_json["pipeline"]["filter"], named_json["pipeline"]["filter"],
        "the named status must be read from hqplayer:rig, not the default daemon"
    );
    let options = &named_json["options"];
    for setting in [
        "mode",
        "samplerate",
        "filter1x",
        "filterNx",
        "shaper",
        "junk_filter",
        "matrix_profile",
    ] {
        assert!(
            options[setting]["current"].is_string(),
            "status must expose the semantic current value for {setting}: {options}"
        );
        assert!(
            options[setting]["choices"]
                .as_array()
                .is_some_and(|choices| !choices.is_empty()),
            "status must expose discover-before-set choices for {setting}: {options}"
        );
    }
    for setting in ["convolution", "adaptive_volume", "random"] {
        assert!(
            options[setting].is_boolean(),
            "status must expose current {setting}: {options}"
        );
    }
    assert!(
        options["repeat"].is_string(),
        "status must expose repeat as off/one/all: {options}"
    );

    let default_before = default_model.request_count("SetFilter");
    let rig_before = rig_model.request_count("SetFilter");
    let result = rig
        .call_tool(
            "hifi_hqplayer_set_pipeline",
            json!({
                "zone_id": "hqplayer:rig",
                "setting": "filter1x",
                "value": "poly-sinc-xtr"
            }),
        )
        .await;
    assert!(
        !text_of(&result).starts_with("Error"),
        "named pipeline write must succeed: {}",
        text_of(&result)
    );
    assert_eq!(
        default_model.request_count("SetFilter"),
        default_before,
        "the default daemon must not receive a write targeted at hqplayer:rig"
    );
    assert!(
        rig_model.request_count("SetFilter") > rig_before,
        "the named daemon must receive the SetFilter write"
    );

    rig.shutdown().await;
    default_server.stop();
    rig_server.stop();
}

/// **Client expectation.** A malformed or missing HQPlayer instance fails explicitly instead of
/// silently falling back to the default adapter.
#[tokio::test]
async fn mcp_advanced_hqplayer_tools_refuse_foreign_and_missing_zone_targets() {
    let rig = Rig::new().await;

    for (zone_id, expected) in [
        ("roon:office", "must start with 'hqplayer:'"),
        ("hqplayer:", "must name an instance"),
        (
            "hqplayer:ghost",
            "HQPlayer instance 'ghost' is not configured",
        ),
    ] {
        let result = rig
            .call_tool(
                "hifi_hqplayer_set_pipeline",
                json!({"zone_id": zone_id, "setting": "mode", "value": "PCM"}),
            )
            .await;
        assert!(
            text_of(&result).contains(expected),
            "target {zone_id:?} must fail explicitly; got {}",
            text_of(&result)
        );
    }

    rig.shutdown().await;
}

/// **Client expectation.** Immediate controls proven by the native adapter are available through
/// the same exact-instance MCP path as the original pipeline fields. These are session controls,
/// not persistent profile edits; playlist/library operations remain outside this claim.
#[tokio::test]
async fn mcp_new_immediate_controls_reach_only_the_named_instance() {
    let default_model = playing_daemon();
    let rig_model = playing_daemon();
    let default_server = start_daemon(&default_model).await;
    let rig_server = start_daemon(&rig_model).await;
    let rig = Rig::new().await;
    rig.attach_default(&default_server).await;
    rig.attach_named("rig", &rig_server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    for (setting, value, element) in [
        ("junk_filter", "20 kHz", "SetJunkFilter"),
        ("convolution", "true", "SetConvolution"),
        ("adaptive_volume", "true", "SetAdaptiveVolume"),
        ("repeat", "all", "SetRepeat"),
        ("random", "true", "SetRandom"),
        ("matrix_profile", "Speakers", "MatrixSetProfile"),
    ] {
        let default_before = default_model.request_count(element);
        let named_before = rig_model.request_count(element);
        let result = rig
            .call_tool(
                "hifi_hqplayer_set_pipeline",
                json!({"zone_id": "hqplayer:rig", "setting": setting, "value": value}),
            )
            .await;
        assert!(
            !text_of(&result).starts_with("Error"),
            "{setting} must be accepted for the named instance: {}",
            text_of(&result)
        );
        assert_eq!(
            default_model.request_count(element),
            default_before,
            "{setting} must not leak to the default daemon"
        );
        assert!(
            rig_model.request_count(element) > named_before,
            "{setting} must emit {element} on the named daemon"
        );
    }

    rig.shutdown().await;
    default_server.stop();
    rig_server.stop();
}

// =============================================================================
// review — defects found by the Opus stacked review, pinned so they stay fixed
// =============================================================================

/// **Client expectation.** An assistant that got `hqplayer:rig` from `hifi_zones` and then asked
/// `hifi_play` to play something on it must be told that this zone has no library to search — not
/// handed a Roon error, and above all not have its request dispatched to Roon at all.
///
/// This is the *same* defect #391 closed in `knob_control_handler` and #401 closed in
/// `hifi_control`: a zone-id switch that recognises one prefix and lets everything else fall
/// through to Roon. `hifi_play` passes `args.zone_id` verbatim into
/// `roon.search_and_play(query, zone_id, …)`, so an `hqplayer:` id becomes Roon's
/// `zone_or_output_id`. On a Roon-plus-HQPlayer install that is a foreign zone id offered to a live
/// Roon core; here Roon is disconnected, which is what makes "did it reach Roon" observable from
/// the response text.
///
/// `hifi_search` carries the same id into Roon's search as context, for the same reason.
///
/// **RED before this fix:** both answered with a Roon failure, proving the call had been routed to
/// Roon rather than refused.
#[tokio::test]
async fn mcp_play_and_search_do_not_hand_an_hqplayer_zone_id_to_roon() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    for (tool, args) in [
        (
            "hifi_play",
            json!({"zone_id":"hqplayer:rig","query":"Kind of Blue"}),
        ),
        (
            "hifi_search",
            json!({"zone_id":"hqplayer:rig","query":"Kind of Blue"}),
        ),
    ] {
        let text = text_of(&rig.call_tool(tool, args).await);
        let lower = text.to_lowercase();
        // A call that was dispatched to Roon comes back as that backend's own failure, wrapped by
        // this surface's `Play error:` / `Search error:` prefix. A call that was refused before
        // dispatch cannot produce either. (Suggesting a `roon:` zone in the refusal is fine and
        // deliberate, so the test keys on the failure shape rather than on the word "Roon".)
        assert!(
            !lower.contains("not connected")
                && !lower.contains("play error")
                && !lower.contains("search error"),
            "`{tool}` with an `hqplayer:` zone id reached Roon; the response is a backend \
             failure rather than a refusal:\n{text}"
        );
        assert!(
            lower.contains("hqplayer"),
            "`{tool}` must name the actual limitation so the assistant can choose another zone; \
             got:\n{text}"
        );
    }

    rig.shutdown().await;
    server.stop();
}

/// **Client expectation.** `hifi_control` must not present an observation that predates the
/// command as the zone's *current* state.
///
/// A direct HQPlayer zone is refreshed only by the producer's status poll (2 s by default), and the
/// transport commands are fire-and-forget — `adapter.pause()` sends `Pause` and returns. So the
/// `aggregator.get_zone` this arm performs immediately afterwards returns the poll from *before*
/// the command, and labelling it "Current state" tells the assistant the pause did not take effect.
/// The observable consequence is an assistant that reports failure, or pauses twice.
///
/// The surface may not read the adapter to close the gap (`docs/ARCHITECTURE.md`,
/// `tests/architecture_lint.rs`), so the fix is to stop making the claim rather than to invent a
/// fresher observation.
///
/// **RED before this fix:** the daemon is paused, `Pause` is in its request log, and the tool
/// answers `Current state:` followed by `"state": "playing"`.
#[tokio::test]
async fn mcp_control_does_not_report_a_pre_command_observation_as_current() {
    let model = playing_daemon();
    let server = start_daemon(&model).await;
    let rig = Rig::new().await;
    rig.attach(&server).await;
    rig.zone_when(|z| z.state == PlaybackState::Playing).await;

    let text = text_of(
        &rig.call_tool(
            "hifi_control",
            json!({"zone_id":"hqplayer:rig","action":"pause"}),
        )
        .await,
    );

    // The command really did land, so any claim about "now" that says `playing` is false.
    assert_eq!(
        model.request_count("Pause"),
        1,
        "the pause must have landed"
    );
    assert_eq!(model.state().playback, 1, "the daemon is paused");

    assert!(
        !(text.contains("Current state") && text.contains("\"state\": \"playing\"")),
        "the daemon is paused; presenting the pre-command poll as the zone's *current* state \
         tells the assistant the pause did not take effect:\n{text}"
    );

    rig.shutdown().await;
    server.stop();
}
