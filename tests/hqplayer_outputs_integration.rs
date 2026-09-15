//! HQPlayer output routing through the production path (P2–P5 backend slice).
//!
//! The objects under test are the real ones: `ZoneAggregator` as projection committer, the
//! reliable runtime, `HqpRuntimeBridge`, `HqpInstanceManager::new_with_runtime` with its
//! exact-instance command endpoint, the adapter-owned NAA relay, and the shared command service in
//! `api::hqp_outputs` that HTTP and MCP call. Peers are loopback software fixtures only: the
//! stateful HQPlayer wire daemon (`tests/mock_servers/hqplayer`), software NAA endpoints and an
//! auto-reconnecting HQPlayer-side NAA client (`tests/mock_servers/naa`). No live HQPlayer, no
//! physical DAC, no listening claim.

#![cfg(feature = "naa-proxy")]

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::post;
use axum::Router;
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

use mock_servers::hqplayer::corpus::VERIFIED_PROFILE;
use mock_servers::hqplayer::model::{DaemonModel, Metadata};
use mock_servers::hqplayer::wire::{ReplyGate, WirePolicy, WireServer};
use mock_servers::naa::{port_is_listening, AutoHqpClient, FakeNaa};

use unified_hifi_control::adapters::hqplayer::naa_relay::HqpOutputTimeouts;
use unified_hifi_control::adapters::hqplayer::outputs::{
    HqpOutputAction, HqpOutputAvailability, HqpOutputCommandReceipt, HqpOutputCommandRequest,
    HqpOutputOperation, HqpOutputOutcome, HqpOutputPhase, HqpOutputProjection, HqpOutputRefusal,
    NaaRelaySettings,
};
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
use unified_hifi_control::api::hqp_outputs::{
    read_hqp_output_operation, read_hqp_outputs, submit_hqp_output_command, HqpOutputServiceError,
};
use unified_hifi_control::api::{self, AppState};
use unified_hifi_control::bus::runtime::build_runtime;
use unified_hifi_control::bus::{create_bus, BusEvent, SharedBus};
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::knobs::KnobStore;

// =============================================================================
// Harness
// =============================================================================

// Every async scenario below writes the same process-wide configuration file.
// Serialize these scenarios so another manager cannot replace a persistence assertion
// with its own instance array while the first scenario is still running.

/// Isolated from the developer's real config; every unrelated adapter is explicitly off (Roon
/// defaults to on), so nothing but the HQPlayer lifecycle under test can run.
fn isolate_config_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("uhc-hqp-outputs-{}", std::process::id()));
        let subdir = dir.join("unified-hifi");
        std::fs::create_dir_all(&subdir).expect("create isolated config dir");
        std::fs::write(
            subdir.join("app-settings.json"),
            r#"{"adapters":{"roon":false,"upnp":false,"openhome":false,"lms":false,"hqplayer":true,"spotify":false,"applemusic":false,"musicassistant":false}}"#,
        )
        .expect("write isolated app settings");
        let data_dir = dir.join("isolated-data");
        std::fs::create_dir_all(&data_dir).expect("create isolated data dir");
        std::env::set_var("UHC_DATA_DIR", data_dir);
        std::env::set_var("FIRMWARE_AUTO_UPDATE", "false");
        std::env::set_var("UHC_CONFIG_DIR", dir);
    });
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

/// A daemon mid-playback with a loaded, seekable track.
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

struct Rig {
    state: AppState,
    manager: Arc<HqpInstanceManager>,
    aggregator: Arc<ZoneAggregator>,
    bus: SharedBus,
    aggregator_task: tokio::task::JoinHandle<()>,
    projection_task: tokio::task::JoinHandle<()>,
    instance: String,
}

impl Rig {
    async fn new(instance: &str) -> Self {
        isolate_config_dir();
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
        Self {
            state,
            manager,
            aggregator,
            bus,
            aggregator_task,
            projection_task,
            instance: instance.to_string(),
        }
    }

    fn zone_id(&self) -> String {
        format!("hqplayer:{}", self.instance)
    }

    /// Attach the managed instance to a wire daemon, enable its relay on an ephemeral loopback
    /// port and start the lifecycle. Returns the adapter and the bound relay address.
    async fn attach(&self, daemon: &WireServer) -> (Arc<HqpAdapter>, SocketAddr) {
        let adapter = self
            .manager
            .add_instance(
                self.instance.clone(),
                "127.0.0.1".to_string(),
                Some(daemon.port()),
                None,
                None,
                None,
            )
            .await;
        adapter.set_timeouts(fast_timeouts()).await;
        adapter.set_recovery_config(fast_recovery()).await;
        adapter.set_output_timeouts(fast_output_timeouts());
        self.manager
            .set_instance_relay_settings(
                &self.instance,
                NaaRelaySettings {
                    enabled: true,
                    bind: "127.0.0.1:0".to_string(),
                    ..NaaRelaySettings::default()
                },
            )
            .await
            .expect("relay settings persist");
        self.manager.start().await.expect("start managed lifecycle");
        let projection = self
            .outputs_when(|p| p.availability == HqpOutputAvailability::Available)
            .await;
        let bind = projection
            .relay
            .bind
            .expect("bound address published")
            .parse()
            .expect("bound address parses");
        // The direct zone must be published too: the command service gates on a known instance.
        self.wait_for(|| async { self.aggregator.get_zone(&self.zone_id()).await.is_some() })
            .await;
        (adapter, bind)
    }

    async fn wait_for<F, Fut>(&self, mut condition: F)
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if condition().await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within budget");
    }

    async fn outputs(&self) -> HqpOutputProjection {
        read_hqp_outputs(&self.state, &self.zone_id())
            .await
            .expect("outputs readable")
    }

    async fn outputs_when(
        &self,
        mut condition: impl FnMut(&HqpOutputProjection) -> bool,
    ) -> HqpOutputProjection {
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Ok(projection) = read_hqp_outputs(&self.state, &self.zone_id()).await {
                if condition(&projection) {
                    return projection;
                }
                if Instant::now() >= deadline {
                    panic!("output projection never satisfied the condition: {projection:#?}");
                }
            } else if Instant::now() >= deadline {
                panic!("output projection unreadable within budget");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn operation_when(
        &self,
        operation_id: &str,
        mut condition: impl FnMut(&HqpOutputOperation) -> bool,
    ) -> HqpOutputOperation {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let operation = read_hqp_output_operation(&self.state, &self.zone_id(), operation_id)
                .await
                .expect("operation lookup works after the request returned");
            if condition(&operation) {
                return operation;
            }
            if Instant::now() >= deadline {
                panic!("operation never satisfied the condition: {operation:#?}");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn command(
        &self,
        action: HqpOutputAction,
        correlation: Option<&str>,
    ) -> Result<HqpOutputCommandReceipt, HqpOutputServiceError> {
        let current = self.outputs().await;
        let (epoch, revision) = if action.requires_expectations() {
            (Some(current.source_epoch), Some(current.output_revision))
        } else {
            (None, None)
        };
        submit_hqp_output_command(
            &self.state,
            HqpOutputCommandRequest {
                zone_id: self.zone_id(),
                correlation_id: correlation.map(str::to_string),
                expected_source_epoch: epoch,
                expected_output_revision: revision,
                action,
            },
        )
        .await
    }

    async fn add_route(&self, name: &str, naa: &FakeNaa, device_id: Option<&str>) -> String {
        let receipt = self
            .command(
                HqpOutputAction::RouteAdd {
                    name: name.into(),
                    host: naa.host(),
                    port: Some(naa.port()),
                    device_id: device_id.map(str::to_string),
                },
                None,
            )
            .await
            .expect("route added");
        assert_eq!(receipt.operation.outcome, Some(HqpOutputOutcome::Complete));
        receipt
            .operation
            .route_id
            .expect("route_add reports the stable route id")
    }

    /// Mirror the control plane into the NAA client: streaming while the daemon says playing.
    fn mirror_transport(
        &self,
        model: &DaemonModel,
        client: &Arc<AutoHqpClient>,
    ) -> Arc<AtomicBool> {
        let stop = Arc::new(AtomicBool::new(false));
        let model = model.clone();
        let client = client.clone();
        let flag = stop.clone();
        std::thread::spawn(move || {
            while !flag.load(Ordering::Acquire) {
                client.set_playing(model.state().playback == 2);
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        stop
    }

    async fn shutdown(self) {
        self.manager.stop().await;
        self.bus.publish(BusEvent::ShuttingDown { reason: None });
        let _ = self.aggregator_task.await;
        self.projection_task.abort();
    }
}

fn profile_router(state: AppState) -> Router {
    Router::new()
        .route(
            "/hqp/instances/{name}/profile",
            post(api::hqp_instance_load_profile_handler),
        )
        .with_state(state)
}

// =============================================================================
// Lifecycle through the manager
// =============================================================================

/// The relay follows the exact managed instance: enabled and bound while managed, unavailable with
/// its reason when the lifecycle stops (retaining routes), gone when the instance is removed.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn relay_lifecycle_follows_the_managed_instance() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("lifecycle").await;
    let (_adapter, bind) = rig.attach(&daemon).await;
    assert!(port_is_listening(bind));
    let naa = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let route = rig.add_route("A", &naa, None).await;

    rig.manager.stop().await;
    let stopped = rig
        .outputs_when(|p| matches!(p.availability, HqpOutputAvailability::Unavailable { .. }))
        .await;
    match &stopped.availability {
        HqpOutputAvailability::Unavailable { reason, .. } => {
            assert!(reason.contains("lifecycle"), "{reason}")
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        stopped.routes.len(),
        1,
        "unavailable retains the route inventory"
    );
    assert_eq!(stopped.routes[0].route_id, route);
    assert!(
        !port_is_listening(bind),
        "no listener may outlive the managed instance"
    );

    rig.manager.start().await.expect("restart");
    let again = rig
        .outputs_when(|p| p.availability == HqpOutputAvailability::Available)
        .await;
    assert_eq!(
        again.relay.bind.as_deref(),
        Some(bind.to_string().as_str()),
        "restart must retain the allocated listener port"
    );
    assert_eq!(
        again.routes[0].route_id, route,
        "route ids are stable across restarts"
    );
    let rebind: SocketAddr = again.relay.bind.expect("bind").parse().expect("addr");
    assert!(port_is_listening(rebind));

    assert!(rig.manager.remove_instance("lifecycle").await);
    rig.wait_for(|| async {
        rig.aggregator
            .get_hqplayer_outputs("lifecycle")
            .await
            .is_none()
    })
    .await;
    assert!(!port_is_listening(rebind));
    assert!(matches!(
        read_hqp_outputs(&rig.state, "hqplayer:lifecycle").await,
        Err(HqpOutputServiceError::UnknownInstance(_))
    ));
    naa.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

// =============================================================================
// Exact instance, fences, correlation
// =============================================================================

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn commands_target_the_exact_instance_and_never_fall_back() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("exact").await;
    let (_adapter, _bind) = rig.attach(&daemon).await;
    let stop = HqpOutputCommandRequest {
        zone_id: "hqplayer:other".into(),
        correlation_id: None,
        expected_source_epoch: None,
        expected_output_revision: None,
        action: HqpOutputAction::Stop,
    };
    assert!(matches!(
        submit_hqp_output_command(&rig.state, stop.clone()).await,
        Err(HqpOutputServiceError::UnknownInstance(ref i)) if i == "other"
    ));
    let bare = HqpOutputCommandRequest {
        zone_id: "exact".into(),
        ..stop.clone()
    };
    assert!(matches!(
        submit_hqp_output_command(&rig.state, bare).await,
        Err(HqpOutputServiceError::InvalidZone(_))
    ));
    assert!(matches!(
        read_hqp_outputs(&rig.state, "hqplayer:other").await,
        Err(HqpOutputServiceError::UnknownInstance(_))
    ));
    // The one real instance recorded nothing for the misaddressed requests.
    assert!(rig.outputs().await.operations.is_empty());
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn stale_expectations_are_refused_before_any_side_effect() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("stale").await;
    let (_adapter, _bind) = rig.attach(&daemon).await;
    let naa = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let route = rig.add_route("A", &naa, None).await;
    let before = rig.outputs().await;
    let stale = submit_hqp_output_command(
        &rig.state,
        HqpOutputCommandRequest {
            zone_id: rig.zone_id(),
            correlation_id: None,
            expected_source_epoch: Some(before.source_epoch),
            expected_output_revision: Some(before.output_revision + 7),
            action: HqpOutputAction::Select {
                route_id: route.clone(),
            },
        },
    )
    .await;
    match stale {
        Err(HqpOutputServiceError::Refused(HqpOutputRefusal::StaleExpectation {
            current_output_revision,
            ..
        })) => assert_eq!(current_output_revision, before.output_revision),
        other => panic!("stale revision must be refused, got {other:?}"),
    }
    let missing = submit_hqp_output_command(
        &rig.state,
        HqpOutputCommandRequest {
            zone_id: rig.zone_id(),
            correlation_id: None,
            expected_source_epoch: None,
            expected_output_revision: None,
            action: HqpOutputAction::Select {
                route_id: route.clone(),
            },
        },
    )
    .await;
    assert!(matches!(
        missing,
        Err(HqpOutputServiceError::Refused(
            HqpOutputRefusal::StaleExpectation { .. }
        ))
    ));
    let after = rig.outputs().await;
    assert_eq!(
        after.selected_route_id, None,
        "a refused select selects nothing"
    );
    assert_eq!(after.output_revision, before.output_revision);
    assert_eq!(
        after.operations.len(),
        before.operations.len(),
        "refusals leave no record"
    );
    assert_eq!(
        model.request_count("Stop"),
        0,
        "no native traffic for a refused command"
    );
    naa.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn correlation_dedups_identical_requests_and_rejects_conflicts() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("corr").await;
    let (_adapter, _bind) = rig.attach(&daemon).await;
    let naa = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let current = rig.outputs().await;
    let add = |name: &str| HqpOutputCommandRequest {
        zone_id: rig.zone_id(),
        correlation_id: Some("ui-add-1".into()),
        expected_source_epoch: Some(current.source_epoch),
        expected_output_revision: Some(current.output_revision),
        action: HqpOutputAction::RouteAdd {
            name: name.into(),
            host: naa.host(),
            port: Some(naa.port()),
            device_id: None,
        },
    };
    let first = submit_hqp_output_command(&rig.state, add("A"))
        .await
        .expect("first add accepted");
    assert!(
        rig.outputs().await.output_revision > current.output_revision,
        "the add advanced the mutation revision"
    );
    // An exact retry carries the OLD expectations. Dedup by correlation + fingerprint must win
    // before any stale-revision check: the original operation comes back and nothing re-executes.
    let repeat = submit_hqp_output_command(&rig.state, add("A"))
        .await
        .expect("exact retry returns the original operation despite the advanced revision");
    assert_eq!(repeat.operation.operation_id, first.operation.operation_id);
    assert_eq!(rig.outputs().await.routes.len(), 1, "no second side effect");
    // Same correlation, different fingerprint: refused, no side effect, regardless of revision.
    let conflict = submit_hqp_output_command(&rig.state, add("B")).await;
    assert!(matches!(
        conflict,
        Err(HqpOutputServiceError::Refused(
            HqpOutputRefusal::CorrelationConflict { .. }
        ))
    ));
    assert_eq!(rig.outputs().await.routes.len(), 1);
    // A supplied but invalid correlation id is refused, never silently dropped.
    for bad in ["", "   ", &"x".repeat(129), "tab\there"] {
        let mut request = add("C");
        request.correlation_id = Some(bad.to_string());
        let refused = submit_hqp_output_command(&rig.state, request).await;
        assert!(
            matches!(
                refused,
                Err(HqpOutputServiceError::Refused(
                    HqpOutputRefusal::InvalidCommand { .. }
                ))
            ),
            "{bad:?} must be refused, got {refused:?}"
        );
    }
    assert_eq!(rig.outputs().await.routes.len(), 1);
    naa.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

// =============================================================================
// The one-click switch through the production path
// =============================================================================

/// A → B with a playing source: verified native Stop, route commit, fresh authenticated session
/// on B, one Play, accepted start with real payload, native state 2, position restored. The
/// receipt is accepted first; the operation GET carries the outcome and evidence.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn select_switches_a_playing_source_with_stop_fresh_session_play_and_seek() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("switch").await;
    let (_adapter, bind) = rig.attach(&daemon).await;
    let a = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let b = FakeNaa::start("B", "hw:CARD=B,DEV=0", 44100);
    let route_a = rig.add_route("A", &a, None).await;
    let route_b = rig.add_route("B", &b, None).await;

    // Selecting with no HQPlayer session through the relay needs no native traffic.
    let first = rig
        .command(
            HqpOutputAction::Select {
                route_id: route_a.clone(),
            },
            Some("sel-a"),
        )
        .await
        .expect("select A");
    assert_eq!(first.operation.outcome, Some(HqpOutputOutcome::Complete));
    assert_eq!(model.request_count("Stop"), 0);
    assert_eq!(model.request_count("Play"), 0);

    // HQPlayer (the fixture client) connects through the relay and streams to A.
    let client = Arc::new(AutoHqpClient::start(bind, 44100));
    let mirror = rig.mirror_transport(&model, &client);
    let forwarding = rig
        .outputs_when(|p| {
            p.session_confirms_audio() && p.selected_route_id.as_deref() == Some(&route_a)
        })
        .await;
    assert!(forwarding
        .session
        .as_ref()
        .is_some_and(|s| s.current_stream_audio_bytes > 0));
    assert!(a.wait_until(|n| n.audio_bytes() > 0, Duration::from_secs(3)));
    let revision_before = forwarding.output_revision;

    // Telemetry (bytes) grows without moving the mutation revision. The publisher refreshes byte
    // counters on its own period, so wait (bounded) for an actual increase rather than sleeping.
    let bytes_before = forwarding
        .session
        .as_ref()
        .map(|s| s.current_stream_audio_bytes)
        .expect("forwarding session");
    let later = rig
        .outputs_when(|p| {
            p.session
                .as_ref()
                .is_some_and(|s| s.current_stream_audio_bytes > bytes_before)
        })
        .await;
    assert_eq!(
        later.output_revision, revision_before,
        "bytes are telemetry, not a mutation"
    );
    // The endpoint received exactly the bytes the client sent, and the client received exactly
    // the feedback the endpoint wrote: measured, not inferred from counters.
    let expected_payload = mock_servers::naa::auto_client_payload();
    let records = a.audio_records();
    assert!(!records.is_empty());
    assert!(
        records.iter().all(|r| r.payload == expected_payload),
        "payload bytes must be exact"
    );
    assert!(records
        .iter()
        .all(|r| r.payload_sha256 == mock_servers::naa::sha256_hex(&expected_payload)));
    let mut corrupted = expected_payload.clone();
    corrupted[7] ^= 0x80;
    assert!(
        records.iter().all(|r| r.payload != corrupted),
        "same-length corruption is detectable"
    );
    let client_feedback = client.feedback();
    assert!(!client_feedback.is_empty());
    assert!(client_feedback.iter().all(|f| f.len() == 16));
    assert_eq!(
        client_feedback.last().cloned(),
        a.last_feedback_bytes(),
        "feedback bytes travel downstream byte-for-byte"
    );

    // The switch.
    let stops_before = model.request_count("Stop");
    let plays_before = model.request_count("Play");
    let receipt = rig
        .command(
            HqpOutputAction::Select {
                route_id: route_b.clone(),
            },
            Some("sel-b"),
        )
        .await
        .expect("select B accepted");
    assert!(receipt.accepted);
    assert!(
        !receipt.operation.is_terminal() || receipt.operation.outcome.is_some(),
        "receipt is the admission, not a playback claim: {:?}",
        receipt.operation
    );
    assert!(
        !receipt
            .projection
            .operation_confirms_current_audio(&receipt.operation.operation_id),
        "HTTP acceptance is never called playback"
    );
    let done = rig
        .operation_when(&receipt.operation.operation_id, |o| o.is_terminal())
        .await;
    assert!(
        matches!(
            done.outcome,
            Some(HqpOutputOutcome::Complete | HqpOutputOutcome::Partial)
        ),
        "switch must complete (or be partial on position only): {done:#?}"
    );
    assert_eq!(done.evidence.native_state_before.as_deref(), Some("2"));
    assert_eq!(done.evidence.native_stop_verified, Some(true));
    assert!(done.evidence.initialized && done.evidence.started);
    assert!(done.evidence.current_stream_audio_bytes > 0, "{done:#?}");
    assert_eq!(done.evidence.native_state_after.as_deref(), Some("2"));
    assert_eq!(done.evidence.position_restored, Some(true), "{done:#?}");
    assert_eq!(
        model.request_count("Stop"),
        stops_before + 1,
        "exactly one native Stop"
    );
    assert_eq!(
        model.request_count("Play"),
        plays_before + 1,
        "exactly one native Play"
    );
    assert_eq!(model.request_count("Seek"), 1);
    assert_eq!(
        model.state().position,
        41,
        "position restored on the same track"
    );
    // Order: Stop was applied before Play.
    let requests = model.requests();
    let stop_at = requests
        .iter()
        .position(|r| r.contains("<Stop"))
        .expect("Stop seen");
    let play_at = requests
        .iter()
        .rposition(|r| r.contains("<Play"))
        .expect("Play seen");
    assert!(stop_at < play_at);
    // B received a fresh authenticated session and its own device id; A was closed.
    assert!(b.wait_until(|n| n.audio_bytes() > 0, Duration::from_secs(3)));
    assert_eq!(b.initialize_devices(), vec!["hw:CARD=B,DEV=0".to_string()]);
    assert!(a.closed_sessions() >= 1);
    let after = rig.outputs().await;
    assert_eq!(after.selected_route_id.as_deref(), Some(route_b.as_str()));
    assert!(after.operation_confirms_current_audio(&done.operation_id));
    assert!(after.session_confirms_audio());
    assert_eq!(
        after
            .observed_forwarding_destination
            .as_ref()
            .map(|d| d.port),
        Some(b.port()),
        "observed destination follows the fresh session, not the desire"
    );
    assert!(
        after.output_revision > revision_before,
        "a switch is a mutation"
    );

    mirror.store(true, Ordering::Release);
    Arc::try_unwrap(client).ok().map(AutoHqpClient::close);
    a.close();
    b.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

// =============================================================================
// Cancellation priority
// =============================================================================

/// Stop must overtake a select that is blocked behind a held native reply: the pair closes and the
/// receipt returns immediately, the select ends cancelled, and no late Play is ever issued.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn stop_overtakes_a_select_held_on_native_stop_and_prevents_late_play() {
    let model = playing_daemon();
    let gate = ReplyGate::new("Stop");
    let daemon = WireServer::start(
        Arc::new(model.clone()),
        WirePolicy {
            reply_gate: Some(gate.clone()),
            ..WirePolicy::default()
        },
    )
    .await;
    let rig = Rig::new("stopwin").await;
    let (adapter, bind) = rig.attach(&daemon).await;
    // Long native response budget so the held reply blocks the select instead of timing out.
    adapter
        .set_timeouts(HqpTimeouts {
            response: Duration::from_secs(8),
            ..fast_timeouts()
        })
        .await;
    let a = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let b = FakeNaa::start("B", "hw:CARD=B,DEV=0", 44100);
    let route_a = rig.add_route("A", &a, None).await;
    let route_b = rig.add_route("B", &b, None).await;
    rig.command(
        HqpOutputAction::Select {
            route_id: route_a.clone(),
        },
        None,
    )
    .await
    .expect("select A");
    let client = Arc::new(AutoHqpClient::start(bind, 44100));
    let mirror = rig.mirror_transport(&model, &client);
    rig.outputs_when(|p| p.session_confirms_audio()).await;

    let held = rig
        .command(
            HqpOutputAction::Select {
                route_id: route_b.clone(),
            },
            Some("held"),
        )
        .await
        .expect("select B admitted");
    // The daemon applied Stop and is now holding its reply; the select is blocked inside it.
    tokio::time::timeout(Duration::from_secs(3), gate.wait_until_reached())
        .await
        .expect("select reached the gated native Stop");
    assert_eq!(model.state().playback, 0);
    let plays_before = model.request_count("Play");

    let started = Instant::now();
    let stop = rig
        .command(HqpOutputAction::Stop, Some("stop-now"))
        .await
        .expect("Stop accepted");
    let latency = started.elapsed();
    assert!(
        latency < Duration::from_secs(2),
        "Stop waited behind the held select: {latency:?}"
    );
    assert!(matches!(
        stop.operation.phase,
        HqpOutputPhase::Stopping | HqpOutputPhase::Complete | HqpOutputPhase::Partial
    ));
    assert_eq!(
        stop.projection.selected_route_id, None,
        "Stop clears the selection first"
    );
    assert!(
        stop.projection.session.is_none(),
        "Stop closes the pair first"
    );
    let cancelled = rig
        .operation_when(&held.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(
        cancelled.outcome,
        Some(HqpOutputOutcome::Cancelled),
        "{cancelled:#?}"
    );
    gate.release();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        model.request_count("Play"),
        plays_before,
        "no late Play after Stop"
    );
    assert_eq!(model.request_count("Seek"), 0);
    assert!(
        b.auth_nonces().is_empty(),
        "the cancelled route was never contacted"
    );
    let stop_done = rig
        .operation_when(&stop.operation.operation_id, |o| o.is_terminal())
        .await;
    assert!(
        matches!(
            stop_done.outcome,
            Some(HqpOutputOutcome::Complete | HqpOutputOutcome::Partial)
        ),
        "{stop_done:#?}"
    );
    // The daemon is stopped and stays stopped; a reconnecting client is refused (no route).
    assert_eq!(model.state().playback, 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(rig.outputs().await.session.is_none());
    mirror.store(true, Ordering::Release);
    Arc::try_unwrap(client).ok().map(AutoHqpClient::close);
    a.close();
    b.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// A profile load (reconfiguration lane) shares the lease and invalidates pending output work: the
/// held select is cancelled and never issues Play.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn a_reconfiguration_command_supersedes_a_pending_select() {
    let model = playing_daemon();
    let gate = ReplyGate::new("Stop");
    let daemon = WireServer::start(
        Arc::new(model.clone()),
        WirePolicy {
            reply_gate: Some(gate.clone()),
            ..WirePolicy::default()
        },
    )
    .await;
    let rig = Rig::new("reconf").await;
    let (adapter, bind) = rig.attach(&daemon).await;
    adapter
        .set_timeouts(HqpTimeouts {
            response: Duration::from_secs(8),
            ..fast_timeouts()
        })
        .await;
    let a = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let b = FakeNaa::start("B", "hw:CARD=B,DEV=0", 44100);
    let route_a = rig.add_route("A", &a, None).await;
    let route_b = rig.add_route("B", &b, None).await;
    rig.command(HqpOutputAction::Select { route_id: route_a }, None)
        .await
        .expect("select A");
    let client = Arc::new(AutoHqpClient::start(bind, 44100));
    let mirror = rig.mirror_transport(&model, &client);
    rig.outputs_when(|p| p.session_confirms_audio()).await;
    let held = rig
        .command(HqpOutputAction::Select { route_id: route_b }, Some("held"))
        .await
        .expect("select B admitted");
    tokio::time::timeout(Duration::from_secs(3), gate.wait_until_reached())
        .await
        .expect("select reached the gated native Stop");
    let plays_before = model.request_count("Play");

    // A user-requested profile load through the existing HTTP route. It fails (no web
    // credentials in this rig) but its admission on the reconfiguration lane is what matters.
    let response = profile_router(rig.state.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/hqp/instances/reconf/profile")
                .header("content-type", "application/json")
                .body(Body::from(json!({"profile": "any"}).to_string()))
                .expect("request"),
        )
        .await
        .expect("handler responds");
    assert_ne!(response.status(), StatusCode::NOT_FOUND);
    let cancelled = rig
        .operation_when(&held.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(
        cancelled.outcome,
        Some(HqpOutputOutcome::Cancelled),
        "{cancelled:#?}"
    );
    assert!(cancelled
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("reconfiguration")));
    gate.release();
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        model.request_count("Play"),
        plays_before,
        "no late Play after supersede"
    );
    mirror.store(true, Ordering::Release);
    Arc::try_unwrap(client).ok().map(AutoHqpClient::close);
    a.close();
    b.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// Import preserves PoC route ids, host, port and device, and never applies the file's selection.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn import_preview_and_apply_preserve_ids_and_ignore_the_files_selection() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("import").await;
    let (_adapter, _bind) = rig.attach(&daemon).await;
    let routes_json = json!({
        "routes": [
            {"id": "route-1", "name": "Old A", "host": "192.0.2.30", "port": 43210, "device_id": "hw:CARD=A,DEV=0"},
            {"id": "route-2", "name": "Old B", "host": "192.0.2.31", "device_id": ""},
            {"id": "", "name": "broken", "host": "192.0.2.32"}
        ],
        "selected_route_id": "route-2"
    })
    .to_string();
    let preview = rig
        .command(
            HqpOutputAction::ImportPreview {
                routes_json: routes_json.clone(),
            },
            None,
        )
        .await
        .expect("preview");
    let Some(unified_hifi_control::adapters::hqplayer::outputs::HqpOutputResult::ImportPreview(p)) =
        preview.operation.result.clone()
    else {
        panic!("typed preview result expected: {:?}", preview.operation);
    };
    assert_eq!(p.routes.len(), 2);
    assert_eq!(p.routes[0].route_id, "route-1");
    assert_eq!(p.routes[0].device_id.as_deref(), Some("hw:CARD=A,DEV=0"));
    assert_eq!(p.routes[1].port, 43210);
    assert_eq!(p.routes[1].device_id, None);
    assert_eq!(p.conflicts.len(), 1);
    assert_eq!(p.ignored_selected_route_id.as_deref(), Some("route-2"));
    assert!(
        rig.outputs().await.routes.is_empty(),
        "preview applies nothing"
    );

    let wrong = rig
        .command(
            HqpOutputAction::ImportApply {
                routes_json: routes_json.clone(),
                preview_id: "not-the-preview".into(),
            },
            None,
        )
        .await
        .expect("admitted");
    assert_eq!(wrong.operation.outcome, Some(HqpOutputOutcome::Rejected));
    let applied = rig
        .command(
            HqpOutputAction::ImportApply {
                routes_json,
                preview_id: p.preview_id,
            },
            None,
        )
        .await
        .expect("apply");
    assert_eq!(applied.operation.outcome, Some(HqpOutputOutcome::Complete));
    let after = rig.outputs().await;
    assert_eq!(
        after
            .routes
            .iter()
            .map(|r| r.route_id.as_str())
            .collect::<Vec<_>>(),
        vec!["route-1", "route-2"]
    );
    assert_eq!(
        after.selected_route_id, None,
        "the file's selection is never applied"
    );
    assert!(after
        .routes
        .iter()
        .all(|r| r.imported_from.as_deref() == Some("poc-routes-json")));
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// The UI and MCP may omit ports for first setup: allocate and persist the concrete endpoint.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn first_relay_configuration_allocates_and_persists_its_own_endpoint() {
    let daemon = WireServer::start(Arc::new(playing_daemon()), WirePolicy::default()).await;
    let rig = Rig::new("automatic-relay").await;
    let (adapter, _) = rig.attach(&daemon).await;
    adapter
        .set_output_relay_settings(NaaRelaySettings::default())
        .await;
    let receipt = rig
        .command(
            HqpOutputAction::RelayConfigure {
                enabled: true,
                bind: None,
                hqp_allow: vec![],
                discovery_interface: Some("127.0.0.1".into()),
                discovery_port: Some(mock_servers::naa::reserved_port()),
                adapter_name: None,
            },
            None,
        )
        .await
        .expect("configure through command service");
    assert_eq!(receipt.operation.outcome, Some(HqpOutputOutcome::Complete));
    let saved = adapter.output_relay_settings().await;
    let address: SocketAddr = saved.bind.parse().unwrap();
    assert_ne!(address.port(), 0, "allocated port must survive restart");
    assert_eq!(saved.adapter_name, "UHC automatic-relay");
    assert!(port_is_listening(address));
    let projection = rig
        .outputs_when(|p| p.relay.adapter_name == "UHC automatic-relay")
        .await;
    assert_eq!(
        projection.relay.discovery_responder.as_deref(),
        Some(saved.bind.as_str())
    );
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// Settings arrive through the same typed command; disabling closes the listener and Stop-like
/// reads stay possible; re-enabling binds again and the change persists in the instance file.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn relay_configure_toggles_the_listener_and_persists() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("configure").await;
    let (_adapter, bind) = rig.attach(&daemon).await;
    let disabled = rig
        .command(
            HqpOutputAction::RelayConfigure {
                enabled: false,
                bind: None,
                hqp_allow: vec![],
                discovery_interface: None,
                discovery_port: None,
                adapter_name: Some("Living Room Router".into()),
            },
            None,
        )
        .await
        .expect("configure");
    assert_eq!(disabled.operation.outcome, Some(HqpOutputOutcome::Complete));
    let off = rig
        .outputs_when(|p| p.availability == HqpOutputAvailability::Disabled)
        .await;
    assert!(!port_is_listening(bind));
    assert_eq!(off.relay.adapter_name, "Living Room Router");
    let saved = unified_hifi_control::adapters::hqplayer::load_hqp_configs()
        .into_iter()
        .find(|c| c.name == "configure")
        .expect("instance saved");
    let relay = saved
        .naa_relay
        .expect("relay settings persisted with the instance");
    assert!(!relay.enabled);
    assert_eq!(relay.adapter_name, "Living Room Router");
    let unavailable = rig
        .command(HqpOutputAction::Discover, None)
        .await
        .expect_err("discover needs a listener");
    assert!(matches!(
        unavailable,
        HqpOutputServiceError::Refused(HqpOutputRefusal::RelayDisabled)
    ));
    let enabled = rig
        .command(
            HqpOutputAction::RelayConfigure {
                enabled: true,
                bind: Some("127.0.0.1:0".into()),
                hqp_allow: vec![],
                discovery_interface: None,
                discovery_port: None,
                adapter_name: None,
            },
            None,
        )
        .await
        .expect("configure");
    assert_eq!(enabled.operation.outcome, Some(HqpOutputOutcome::Complete));
    let on = rig
        .outputs_when(|p| p.availability == HqpOutputAvailability::Available)
        .await;
    let rebind: SocketAddr = on.relay.bind.expect("bind").parse().expect("addr");
    assert!(port_is_listening(rebind));
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// Finding 19: the manager's instance map must not be kept alive by its own adapters through the
/// config persister. Dropping the manager (without `stop`) cancels its workers, the owned relay
/// listener closes, and an adapter that outlives it reports a retired persister instead of
/// claiming durable success.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn dropping_the_manager_retires_persistence_and_closes_the_relay() {
    isolate_config_dir();
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let runtime = build_runtime(aggregator.clone(), 16, 64);
    let bridge = Arc::new(HqpRuntimeBridge::new(
        runtime.projection_ingress.clone(),
        runtime.commands.clone(),
    ));
    let projection_task = tokio::spawn(runtime.projection_actor.run());
    let manager = HqpInstanceManager::new_with_runtime(bus.clone(), bridge);
    let adapter = manager
        .add_instance(
            "dropme".into(),
            "127.0.0.1".into(),
            Some(daemon.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.set_recovery_config(fast_recovery()).await;
    manager
        .set_instance_relay_settings(
            "dropme",
            NaaRelaySettings {
                enabled: true,
                bind: "127.0.0.1:0".into(),
                ..NaaRelaySettings::default()
            },
        )
        .await
        .expect("settings");
    manager.start().await.expect("start");
    let deadline = Instant::now() + Duration::from_secs(5);
    let bind = loop {
        let projection = adapter.output_projection();
        if projection.availability == HqpOutputAvailability::Available {
            break projection
                .relay
                .bind
                .expect("bind")
                .parse::<SocketAddr>()
                .expect("addr");
        }
        assert!(
            Instant::now() < deadline,
            "relay never became available: {projection:#?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert!(port_is_listening(bind));
    // Persistence works while the manager lives.
    adapter
        .set_output_relay_settings(adapter.output_relay_settings().await)
        .await;

    drop(manager);
    let deadline = Instant::now() + Duration::from_secs(5);
    while port_is_listening(bind) && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        !port_is_listening(bind),
        "a dropped manager must not leave its relay listener behind"
    );
    let retired = adapter
        .persist_output_relay_settings_for_tests(adapter.output_relay_settings().await)
        .await;
    assert!(
        retired.is_err(),
        "a retired manager must be reported, not treated as durable success"
    );
    // Finding 24: once the test's own handle is gone, nothing (publisher task, relay, persister)
    // may keep the adapter and its coordinator alive.
    let weak = Arc::downgrade(&adapter);
    drop(adapter);
    let deadline = Instant::now() + Duration::from_secs(5);
    while weak.upgrade().is_some() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        weak.upgrade().is_none(),
        "adapter/coordinator must be released after the manager and the last handle are dropped"
    );
    projection_task.abort();
    daemon.shutdown().await;
}

/// Finding 21: a select blocked inside the native conversation (held `State` reply) that is then
/// superseded by Stop must never write Stop/Play/Seek once the reply is released: the write
/// admission fence sits inside the conversation lease, after every await.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn a_select_held_inside_the_native_conversation_never_writes_after_stop() {
    let model = playing_daemon();
    let gate = ReplyGate::new("State");
    let daemon = WireServer::start(
        Arc::new(model.clone()),
        WirePolicy {
            reply_gate: Some(gate.clone()),
            ..WirePolicy::default()
        },
    )
    .await;
    let rig = Rig::new("heldstate").await;
    let (adapter, bind) = rig.attach(&daemon).await;
    adapter
        .set_timeouts(HqpTimeouts {
            response: Duration::from_secs(8),
            ..fast_timeouts()
        })
        .await;
    let a = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let b = FakeNaa::start("B", "hw:CARD=B,DEV=0", 44100);
    let route_a = rig.add_route("A", &a, None).await;
    let route_b = rig.add_route("B", &b, None).await;
    rig.command(HqpOutputAction::Select { route_id: route_a }, None)
        .await
        .expect("select A");
    let client = Arc::new(AutoHqpClient::start(bind, 44100));
    let mirror = rig.mirror_transport(&model, &client);
    rig.outputs_when(|p| p.session_confirms_audio()).await;
    // The managed poller also issues State; consume the one-shot gate with the select's own read
    // by arming it only now and waiting for the select to reach it.
    let stops_before = model.request_count("Stop");
    let plays_before = model.request_count("Play");
    let held = rig
        .command(
            HqpOutputAction::Select { route_id: route_b },
            Some("held-state"),
        )
        .await
        .expect("select B admitted");
    tokio::time::timeout(Duration::from_secs(4), gate.wait_until_reached())
        .await
        .expect("a State reply is being held");
    let stop = rig
        .command(HqpOutputAction::Stop, Some("stop-now"))
        .await
        .expect("Stop accepted");
    assert_eq!(stop.projection.selected_route_id, None);
    let cancelled = rig
        .operation_when(&held.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(
        cancelled.outcome,
        Some(HqpOutputOutcome::Cancelled),
        "{cancelled:#?}"
    );
    gate.release();
    tokio::time::sleep(Duration::from_millis(600)).await;
    // The Stop command's own native Stop may have been written (at most one); the cancelled
    // select wrote nothing.
    assert!(
        model.request_count("Stop") <= stops_before + 1,
        "cancelled select must not write Stop"
    );
    assert_eq!(model.request_count("Play"), plays_before, "no late Play");
    assert_eq!(model.request_count("Seek"), 0);
    assert!(b.auth_nonces().is_empty());
    mirror.store(true, Ordering::Release);
    Arc::try_unwrap(client).ok().map(AutoHqpClient::close);
    a.close();
    b.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// Finding 22: an already-expired hook deadline or a cancelled token produces zero native writes,
/// even though the future is immediately ready.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn expired_or_cancelled_hooks_never_write() {
    use unified_hifi_control::adapters::hqplayer::naa_relay::{NativeHookFence, NativeHookOutcome};
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("expired").await;
    let (adapter, _bind) = rig.attach(&daemon).await;
    let plays_before = model.request_count("Play");
    let stops_before = model.request_count("Stop");
    let expired = tokio::time::Instant::now() - Duration::from_millis(1);
    let fence = NativeHookFence::unconditional();
    let play = adapter
        .output_hook_play(&fence, expired)
        .await
        .expect("no wire error");
    assert!(
        matches!(play, NativeHookOutcome::NotAttempted(_)),
        "{play:?}"
    );
    let stop = adapter
        .output_hook_stop_verified(&fence, expired)
        .await
        .expect("no wire error");
    assert!(
        matches!(stop, NativeHookOutcome::NotAttempted(_)),
        "{stop:?}"
    );
    let cancelled = NativeHookFence::unconditional();
    cancelled.token.cancel();
    let far = tokio::time::Instant::now() + Duration::from_secs(5);
    let play = adapter
        .output_hook_play(&cancelled, far)
        .await
        .expect("no wire error");
    assert!(
        matches!(play, NativeHookOutcome::NotAttempted(_)),
        "{play:?}"
    );
    let superseded = NativeHookFence {
        token: CancellationToken::new(),
        still_current: Arc::new(|| false),
    };
    let seek = adapter
        .output_hook_restore_position(&superseded, far, "3", 41)
        .await
        .expect("no wire error");
    assert!(
        matches!(seek, NativeHookOutcome::NotAttempted(_)),
        "{seek:?}"
    );
    assert_eq!(model.request_count("Play"), plays_before);
    assert_eq!(model.request_count("Stop"), stops_before);
    assert_eq!(model.request_count("Seek"), 0);
    // An explicit daemon rejection is a failure, not "indeterminate".
    model.arm(|f| {
        f.reject_next
            .push(("Play".into(), "fixture refuses".into()))
    });
    let rejected = adapter
        .output_hook_play(&NativeHookFence::unconditional(), far)
        .await;
    assert!(
        rejected.is_err(),
        "rejection must surface as an error: {rejected:?}"
    );
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// Finding 28: the write-admission guard runs after the inner conversation's own awaits. A hook
/// parked between those awaits and the guard, cancelled there, then released, writes nothing.
/// (Before the fix the fence was checked before those awaits, so this release would have written
/// Play.) Also exercised with the connection slot and the state lock held across the cancel.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn a_hook_cancelled_between_the_pre_write_awaits_and_admission_never_writes() {
    use unified_hifi_control::adapters::hqplayer::naa_relay::{NativeHookFence, NativeHookOutcome};
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("prewrite").await;
    let (adapter, _bind) = rig.attach(&daemon).await;
    let plays_before = model.request_count("Play");
    let far = tokio::time::Instant::now() + Duration::from_secs(8);

    // 1. Park at the seam, cancel, release.
    let (reached, release) = adapter.arm_pre_write_gate_for_tests();
    let fence = NativeHookFence::unconditional();
    let token = fence.token.clone();
    let hook = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.output_hook_play(&fence, far).await })
    };
    tokio::time::timeout(Duration::from_secs(4), reached.notified())
        .await
        .expect("hook reached the pre-write seam");
    token.cancel();
    release.notify_one();
    let outcome = hook.await.expect("hook task").expect("no wire error");
    assert!(
        matches!(outcome, NativeHookOutcome::NotAttempted(_)),
        "{outcome:?}"
    );
    adapter.disarm_pre_write_gate_for_tests();
    assert_eq!(
        model.request_count("Play"),
        plays_before,
        "no Play after cancel at the seam"
    );

    // 2. Superseded (generation moved) while parked: same guarantee.
    let (reached, release) = adapter.arm_pre_write_gate_for_tests();
    let current = Arc::new(AtomicBool::new(true));
    let fence = NativeHookFence {
        token: CancellationToken::new(),
        still_current: {
            let current = current.clone();
            Arc::new(move || current.load(Ordering::Acquire))
        },
    };
    let hook = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.output_hook_play(&fence, far).await })
    };
    tokio::time::timeout(Duration::from_secs(4), reached.notified())
        .await
        .expect("hook reached the pre-write seam");
    current.store(false, Ordering::Release);
    release.notify_one();
    let outcome = hook.await.expect("hook task").expect("no wire error");
    assert!(
        matches!(outcome, NativeHookOutcome::NotAttempted(_)),
        "{outcome:?}"
    );
    adapter.disarm_pre_write_gate_for_tests();
    assert_eq!(model.request_count("Play"), plays_before);

    // 3. The connection slot held across the cancel: the hook blocks at its pre-write await and is
    //    refused when released.
    let hold = adapter.hold_native_connection_for_tests().await;
    let fence = NativeHookFence::unconditional();
    let token = fence.token.clone();
    let hook = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.output_hook_play(&fence, far).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !hook.is_finished(),
        "hook must be blocked behind the held connection slot"
    );
    token.cancel();
    drop(hold);
    let outcome = hook.await.expect("hook task").expect("no wire error");
    assert!(
        matches!(outcome, NativeHookOutcome::NotAttempted(_)),
        "{outcome:?}"
    );
    assert_eq!(model.request_count("Play"), plays_before);

    // 4. The state lock held across the cancel (blocks the `timeouts()` read).
    let hold = adapter.hold_native_state_for_tests().await;
    let fence = NativeHookFence::unconditional();
    let token = fence.token.clone();
    let hook = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.output_hook_play(&fence, far).await })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        !hook.is_finished(),
        "hook must be blocked behind the held state lock"
    );
    token.cancel();
    drop(hold);
    let outcome = hook.await.expect("hook task").expect("no wire error");
    assert!(
        matches!(outcome, NativeHookOutcome::NotAttempted(_)),
        "{outcome:?}"
    );
    assert_eq!(model.request_count("Play"), plays_before);
    assert_eq!(model.request_count("Seek"), 0);
    daemon.shutdown().await;
    rig.shutdown().await;
}

// =============================================================================
// One-time setup through the existing web credential owner
// =============================================================================

/// A loopback stand-in for HQPlayer Embedded's persistent web lane (port 8088), shaped after the
/// private 6.0.4 evidence without copying it: Digest-protected `GET /config` renders the running
/// configuration form (successful controls: text/number inputs, checked checkboxes, selects with
/// `net_device` options like `"<name>/<device>"`); `POST /config` applies the posted controls to
/// the running selection after `apply_delay` and regenerates the on-disk XML
/// (`<hqplayerd><output type="…"/><network address="…" device="…"/>…`); `GET /backup` returns the
/// raw disk bytes; `POST /restore` (multipart `scope`, `cfgfile`) rewrites the disk bytes only and
/// never touches the running selection. Credentials here are fake test values.
#[derive(Clone)]
enum Control {
    Text(String),
    Number(String),
    Checkbox {
        value: String,
        checked: bool,
    },
    Select {
        options: Vec<(String, String)>,
        selected: Option<String>,
    },
}

struct WebModel {
    controls: Vec<(String, Control)>,
    disk: Vec<u8>,
    posted: Vec<Vec<(String, String)>>,
    /// The running `backend` selection observed when each `/restore` upload arrived.
    restore_seen_running_backend: Vec<Option<String>>,
}

struct FakeHqpWeb {
    addr: SocketAddr,
    model: Arc<std::sync::Mutex<WebModel>>,
    requests: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
    gate: Arc<std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>>,
    task: tokio::task::JoinHandle<()>,
}

const ORIGINAL_DISK: &[u8] = b"<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<hqplayerd>\n\t<!-- fixture: raw bytes as the daemon wrote them -->\n\t<output type=\"alsa\"/>\n\t<alsa device=\"hw:CARD=null\" bits=\"24\" period_time=\"100\"/>\n\t<network address=\"\" device=\"\" dac_bits=\"0\" period_time=\"0\"/>\n\t<engine mode=\"sdm\" filter=\"40\"/>\n</hqplayerd>\n";

fn setup_controls(relay_offered: bool) -> Vec<(String, Control)> {
    let mut net_devices = vec![("rpi/hw:CARD=C20,DEV=0".to_string(), "rpi: usb".to_string())];
    if relay_offered {
        net_devices.insert(
            0,
            (
                "HiPhi Router/hiphi:router".to_string(),
                "HiPhi Router: HiPhi Router".to_string(),
            ),
        );
    }
    vec![
        ("title".into(), Control::Text("HQPlayerEmbedded".into())),
        (
            "backend".into(),
            Control::Select {
                options: vec![
                    ("alsa".into(), "ALSA".into()),
                    ("network".into(), "Network Audio".into()),
                    ("combo".into(), "Combo".into()),
                ],
                selected: Some("alsa".into()),
            },
        ),
        (
            "mode".into(),
            Control::Select {
                options: vec![
                    ("auto".into(), "Auto".into()),
                    ("pcm".into(), "PCM".into()),
                    ("sdm".into(), "SDM".into()),
                ],
                selected: Some("sdm".into()),
            },
        ),
        (
            "filter".into(),
            Control::Select {
                options: vec![
                    ("40".into(), "poly-sinc-gauss-hires-lp".into()),
                    ("41".into(), "other".into()),
                ],
                selected: Some("40".into()),
            },
        ),
        (
            "alsa_device".into(),
            Control::Select {
                options: vec![("hw:CARD=null".into(), "null".into())],
                selected: None,
            },
        ),
        (
            "net_device".into(),
            Control::Select {
                options: net_devices,
                selected: None,
            },
        ),
        ("net_bits".into(), Control::Number("0".into())),
        ("net_period".into(), Control::Number("0".into())),
        (
            "dsd_6db".into(),
            Control::Checkbox {
                value: "1".into(),
                checked: true,
            },
        ),
        (
            "net_dop".into(),
            Control::Checkbox {
                value: "1".into(),
                checked: false,
            },
        ),
        (
            "log_enabled".into(),
            Control::Checkbox {
                value: "1".into(),
                checked: true,
            },
        ),
    ]
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl WebModel {
    fn render(&self) -> String {
        let mut html =
            String::from("<!DOCTYPE html><html><body><h1>Config</h1><form method=\"post\">\n");
        for (name, control) in &self.controls {
            match control {
                Control::Text(v) => html.push_str(&format!(
                    "<input type=\"text\" name=\"{name}\" value=\"{}\"/>\n",
                    html_escape(v)
                )),
                Control::Number(v) => html.push_str(&format!(
                    "<input type=\"number\" name=\"{name}\" value=\"{}\"/>\n",
                    html_escape(v)
                )),
                Control::Checkbox { value, checked } => html.push_str(&format!(
                    "<input type=\"checkbox\" name=\"{name}\" value=\"{}\"{}/>\n",
                    html_escape(value),
                    if *checked { " checked" } else { "" }
                )),
                Control::Select { options, selected } => {
                    html.push_str(&format!("<select name=\"{name}\">\n"));
                    for (value, text) in options {
                        html.push_str(&format!(
                            "<option value=\"{}\"{}>{}</option>\n",
                            html_escape(value),
                            if selected.as_deref() == Some(value.as_str()) {
                                " selected"
                            } else {
                                ""
                            },
                            html_escape(text)
                        ));
                    }
                    html.push_str("</select>\n");
                }
            }
        }
        html.push_str("<input type=\"submit\" value=\"Apply\"/><input type=\"submit\" value=\"Refresh devices\"/>\n</form>\n");
        html.push_str("<form method=\"get\"><input type=\"text\" name=\"profile_name\"/><input type=\"submit\" value=\"Load\"/></form></body></html>");
        html
    }

    fn value(&self, name: &str) -> Option<String> {
        self.controls
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, c)| match c {
                Control::Text(v) | Control::Number(v) => Some(v.clone()),
                Control::Checkbox { value, checked } => checked.then(|| value.clone()),
                Control::Select { options, selected } => selected
                    .clone()
                    .or_else(|| options.first().map(|(v, _)| v.clone())),
            })
    }

    /// Apply a posted form to the running selection and regenerate the on-disk XML, as the
    /// daemon does when the operator presses Apply.
    fn apply_form(&mut self, fields: &[(String, String)]) {
        for (name, control) in &mut self.controls {
            let posted: Vec<&String> = fields
                .iter()
                .filter(|(n, _)| n == name)
                .map(|(_, v)| v)
                .collect();
            match control {
                Control::Text(v) | Control::Number(v) => {
                    if let Some(p) = posted.first() {
                        *v = (*p).clone();
                    }
                }
                Control::Checkbox { checked, .. } => *checked = !posted.is_empty(),
                Control::Select { selected, .. } => {
                    if let Some(p) = posted.first() {
                        *selected = Some((*p).clone());
                    }
                }
            }
        }
        let backend = self.value("backend").unwrap_or_default();
        let net_device = self.value("net_device").unwrap_or_default();
        let (address, device) = net_device
            .split_once('/')
            .map(|(a, d)| (a.to_string(), d.to_string()))
            .unwrap_or_default();
        self.disk = format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n<hqplayerd>\n\t<output type=\"{}\"/>\n\t<alsa device=\"{}\" bits=\"24\" period_time=\"100\"/>\n\t<network address=\"{}\" device=\"{}\" dac_bits=\"{}\" period_time=\"{}\" friendly_name=\"{}: {}\"/>\n\t<engine mode=\"{}\" filter=\"{}\"/>\n</hqplayerd>\n",
            backend,
            self.value("alsa_device").unwrap_or_default(),
            address,
            device,
            self.value("net_bits").unwrap_or_default(),
            self.value("net_period").unwrap_or_default(),
            address,
            address,
            self.value("mode").unwrap_or_default(),
            self.value("filter").unwrap_or_default(),
        )
        .into_bytes();
    }
}

impl FakeHqpWeb {
    async fn start(relay_offered: bool, apply_delay: Duration) -> Self {
        Self::start_with_delays(relay_offered, apply_delay, apply_delay).await
    }

    /// `config_delay`: how long the daemon takes to apply a posted form (running selection AND
    /// the regenerated disk file, written after the HTTP acknowledgement). `restore_delay`: how
    /// long `/restore` takes to rewrite the disk bytes.
    async fn start_with_delays(
        relay_offered: bool,
        config_delay: Duration,
        restore_delay: Duration,
    ) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake web");
        let addr = listener.local_addr().expect("addr");
        let model = Arc::new(std::sync::Mutex::new(WebModel {
            controls: setup_controls(relay_offered),
            disk: ORIGINAL_DISK.to_vec(),
            posted: Vec::new(),
            restore_seen_running_backend: Vec::new(),
        }));
        let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gate: Arc<
            std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>,
        > = Arc::new(std::sync::Mutex::new(None));
        let task = {
            let model = model.clone();
            let requests = requests.clone();
            let gate = gate.clone();
            tokio::spawn(async move {
                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        break;
                    };
                    let model = model.clone();
                    let requests = requests.clone();
                    let gate = gate.clone();
                    tokio::spawn(async move {
                        let _ =
                            Self::serve(stream, model, requests, gate, config_delay, restore_delay)
                                .await;
                    });
                }
            })
        };
        Self {
            addr,
            model,
            requests,
            gate,
            task,
        }
    }

    async fn serve(
        mut stream: tokio::net::TcpStream,
        model: Arc<std::sync::Mutex<WebModel>>,
        requests: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
        gate: Arc<std::sync::Mutex<Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>>>,
        config_delay: Duration,
        restore_delay: Duration,
    ) -> std::io::Result<()> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        let head_end = loop {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                return Ok(());
            }
            buf.extend_from_slice(&chunk[..n]);
            if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                break pos + 4;
            }
        };
        let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or_default().to_string();
        let mut content_length = 0usize;
        let mut authorized = false;
        let mut content_type = String::new();
        for line in lines {
            let lower = line.to_ascii_lowercase();
            if let Some(v) = lower.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
            if lower.starts_with("authorization:")
                && lower.contains("digest")
                && line.contains("username=\"admin\"")
            {
                authorized = true;
            }
            if let Some(v) = lower.strip_prefix("content-type:") {
                content_type = v.trim().to_string();
            }
        }
        while buf.len() < head_end + content_length {
            let n = stream.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&chunk[..n]);
        }
        let body = buf[head_end..(head_end + content_length).min(buf.len())].to_vec();
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or_default().to_string();
        let path = parts.next().unwrap_or_default().to_string();
        requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((format!("{method} {path}"), authorized));
        if !authorized {
            let challenge = "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"hqplayer\", nonce=\"fixture-nonce\", qop=\"auth\", algorithm=MD5\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
            stream.write_all(challenge.as_bytes()).await?;
            return Ok(());
        }
        if method == "GET" && path == "/config" {
            let held = gate.lock().unwrap_or_else(|e| e.into_inner()).clone();
            if let Some((reached, release)) = held {
                reached.notify_one();
                release.notified().await;
            }
        }
        let (status, ctype, payload): (&str, &str, Vec<u8>) = match (method.as_str(), path.as_str())
        {
            ("GET", "/config") => (
                "200 OK",
                "text/html",
                model
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .render()
                    .into_bytes(),
            ),
            ("POST", "/config") => {
                let fields: Vec<(String, String)> = url::form_urlencoded::parse(&body)
                    .map(|(k, v)| (k.into_owned(), v.into_owned()))
                    .collect();
                if fields.is_empty() {
                    (
                        "200 OK",
                        "text/html",
                        b"<html><body>Failed!</body></html>".to_vec(),
                    )
                } else {
                    model
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .posted
                        .push(fields.clone());
                    let model = model.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(config_delay).await;
                        model
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .apply_form(&fields);
                    });
                    (
                        "200 OK",
                        "text/html",
                        b"<html><body>Configuration page</body></html>".to_vec(),
                    )
                }
            }
            ("GET", "/backup") => (
                "200 OK",
                "application/xml",
                model.lock().unwrap_or_else(|e| e.into_inner()).disk.clone(),
            ),
            ("POST", "/restore") => {
                let boundary = content_type
                    .split("boundary=")
                    .nth(1)
                    .map(|b| b.trim().to_string())
                    .unwrap_or_default();
                let marker = format!("--{boundary}");
                let text = String::from_utf8_lossy(&body).to_string();
                let has_scope = text.contains("name=\"scope\"") && text.contains("system");
                let file_start = text
                    .find("name=\"cfgfile\"")
                    .and_then(|p| text[p..].find("\r\n\r\n").map(|q| p + q + 4));
                let uploaded = file_start.and_then(|start| {
                    text[start..]
                        .find(&format!("\r\n{marker}"))
                        .map(|end| body[start..start + end].to_vec())
                });
                match (has_scope, uploaded) {
                    (true, Some(bytes)) if !bytes.is_empty() => {
                        {
                            let mut m = model.lock().unwrap_or_else(|e| e.into_inner());
                            let seen = m.value("backend");
                            m.restore_seen_running_backend.push(seen);
                        }
                        let model = model.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(restore_delay).await;
                            // Disk only: the running selection is not reloaded by /restore.
                            model.lock().unwrap_or_else(|e| e.into_inner()).disk = bytes;
                        });
                        (
                            "200 OK",
                            "text/html",
                            b"<html><body>restore</body></html>".to_vec(),
                        )
                    }
                    _ => (
                        "200 OK",
                        "text/html",
                        b"<html><body>Failed!</body></html>".to_vec(),
                    ),
                }
            }
            _ => ("404 Not Found", "text/plain", b"not found".to_vec()),
        };
        let head = format!("HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", payload.len());
        stream.write_all(head.as_bytes()).await?;
        stream.write_all(&payload).await?;
        Ok(())
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    fn disk(&self) -> Vec<u8> {
        self.model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .disk
            .clone()
    }

    fn running(&self, name: &str) -> Option<String> {
        self.model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .value(name)
    }

    fn posted(&self) -> Vec<Vec<(String, String)>> {
        self.model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .posted
            .clone()
    }

    fn restore_seen_running_backend(&self) -> Vec<Option<String>> {
        self.model
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .restore_seen_running_backend
            .clone()
    }

    fn requests(&self) -> Vec<(String, bool)> {
        self.requests
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Change a running control out of band (an operator edit between preview and apply).
    fn select(&self, name: &str, value: &str) {
        let mut model = self.model.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, Control::Select { selected, .. })) =
            model.controls.iter_mut().find(|(n, _)| n == name)
        {
            *selected = Some(value.to_string());
        }
    }

    /// Hold every `GET /config` until released (after a Digest-authenticated request).
    fn hold_config_reads(&self) -> (Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>) {
        let reached = Arc::new(tokio::sync::Notify::new());
        let release = Arc::new(tokio::sync::Notify::new());
        *self.gate.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((reached.clone(), release.clone()));
        (reached, release)
    }

    fn release_config_reads(&self, release: &Arc<tokio::sync::Notify>) {
        *self.gate.lock().unwrap_or_else(|e| e.into_inner()) = None;
        release.notify_waiters();
    }

    fn stop(self) {
        self.task.abort();
    }
}

impl Rig {
    /// Attach with web credentials pointing at a fake persistent web lane.
    async fn attach_with_web(&self, daemon: &WireServer, web: &FakeHqpWeb) -> Arc<HqpAdapter> {
        let adapter = self
            .manager
            .add_instance(
                self.instance.clone(),
                "127.0.0.1".to_string(),
                Some(daemon.port()),
                Some(web.port()),
                Some("admin".to_string()),
                Some("fixture-only-password".to_string()),
            )
            .await;
        adapter.set_timeouts(fast_timeouts()).await;
        adapter.set_recovery_config(fast_recovery()).await;
        adapter.set_output_timeouts(fast_output_timeouts());
        adapter
            .set_profile_timeouts(
                unified_hifi_control::adapters::hqplayer::HqpProfileTimeouts {
                    request: Duration::from_secs(5),
                    settle_deadline: Duration::from_secs(4),
                    poll_interval: Duration::from_millis(50),
                },
            )
            .await;
        self.manager
            .set_instance_relay_settings(
                &self.instance,
                NaaRelaySettings {
                    enabled: true,
                    bind: "127.0.0.1:0".to_string(),
                    ..NaaRelaySettings::default()
                },
            )
            .await
            .expect("relay settings persist");
        self.manager.start().await.expect("start managed lifecycle");
        self.outputs_when(|p| p.availability == HqpOutputAvailability::Available)
            .await;
        self.wait_for(|| async { self.aggregator.get_zone(&self.zone_id()).await.is_some() })
            .await;
        adapter
    }

    async fn setup_preview(
        &self,
    ) -> unified_hifi_control::adapters::hqplayer::outputs::HqpSetupPreview {
        let receipt = self
            .command(HqpOutputAction::SetupPreview, None)
            .await
            .expect("preview admitted");
        let operation = self
            .operation_when(&receipt.operation.operation_id, |o| o.is_terminal())
            .await;
        assert_eq!(
            operation.outcome,
            Some(HqpOutputOutcome::Complete),
            "{operation:#?}"
        );
        match operation.result {
            Some(
                unified_hifi_control::adapters::hqplayer::outputs::HqpOutputResult::SetupPreview(p),
            ) => p,
            other => panic!("typed setup preview expected, got {other:?}"),
        }
    }
}

fn setup_result(
    operation: &HqpOutputOperation,
) -> Option<unified_hifi_control::adapters::hqplayer::outputs::HqpSetupTransaction> {
    match &operation.result {
        Some(unified_hifi_control::adapters::hqplayer::outputs::HqpOutputResult::Setup(t)) => {
            Some(t.clone())
        }
        _ => None,
    }
}

fn posted_value<'a>(fields: &'a [(String, String)], name: &str) -> Option<&'a str> {
    fields
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_str())
}

/// Preview derives the running-form change from HQPlayer's own form and the relay identity;
/// apply re-posts the complete form through the Digest-authenticated lane (DSP controls untouched)
/// and is confirmed only once the running form AND the persistent configuration read back after
/// the daemon's asynchronous application; rollback re-posts the previous running form first and
/// restores the raw persistent bytes second. No XML, attribute map or credential ever appears in a
/// request parameter or an operation record.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn setup_preview_apply_readback_and_rollback_through_the_credential_owner() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    // The daemon applies a posted form asynchronously: the running selection and the regenerated
    // disk file land 300 ms AFTER the HTTP acknowledgement, while /restore rewrites disk in 10 ms.
    // A rollback that uploads the raw backup before the baseline form has settled would see its
    // exact bytes overwritten by the daemon's own delayed write.
    let web =
        FakeHqpWeb::start_with_delays(true, Duration::from_millis(300), Duration::from_millis(10))
            .await;
    let rig = Rig::new("setup").await;
    let _adapter = rig.attach_with_web(&daemon, &web).await;

    let p = rig.setup_preview().await;
    assert!(p.applicable, "{p:#?}");
    assert_eq!(p.relay_option.as_deref(), Some("HiPhi Router/hiphi:router"));
    let changed: Vec<(&str, Option<&str>, &str)> = p
        .changes
        .iter()
        .map(|c| (c.attribute.as_str(), c.from.as_deref(), c.to.as_str()))
        .collect();
    assert_eq!(
        changed,
        vec![("backend", Some("alsa"), "network"), ("net_bits", Some("0"), "32"), ("net_period", Some("0"), "250")],
        "only the output selection and unset network framing change; net_device already names the relay"
    );
    assert!(p
        .current
        .iter()
        .any(|a| a.name == "mode" && a.value == "sdm"));
    assert_eq!(
        p.preserved_controls, 7,
        "ten successful controls minus the three changed ones"
    );
    assert_eq!(
        p.backup_sha256,
        mock_servers::naa::sha256_hex(ORIGINAL_DISK)
    );
    let projection = rig.outputs().await;
    let record = serde_json::to_string(&projection).expect("serializes");
    assert!(
        !record.contains("fixture-only-password"),
        "credentials never appear in records"
    );
    assert!(!record.contains("<hqplayerd"), "no raw XML in records");
    let requests = web.requests();
    assert!(
        requests.iter().any(|(r, auth)| r == "GET /config" && !auth),
        "first attempt drew the Digest challenge"
    );
    assert!(
        requests.iter().any(|(r, auth)| r == "GET /config" && *auth),
        "the instance's stored credentials authenticated"
    );
    assert_eq!(
        web.running("backend").as_deref(),
        Some("alsa"),
        "preview changes nothing"
    );
    assert_eq!(web.disk(), ORIGINAL_DISK);

    // A stale preview id is refused without a post.
    let wrong = rig
        .command(
            HqpOutputAction::SetupApply {
                preview_id: "stale".into(),
            },
            None,
        )
        .await
        .expect("admitted");
    assert_eq!(wrong.operation.outcome, Some(HqpOutputOutcome::Rejected));
    assert!(web.posted().is_empty());

    // Apply.
    let apply = rig
        .command(
            HqpOutputAction::SetupApply {
                preview_id: p.preview_id.clone(),
            },
            Some("setup-apply"),
        )
        .await
        .expect("apply admitted");
    assert!(
        !apply.projection.session_confirms_audio() && apply.operation.outcome.is_none()
            || apply.operation.outcome.is_some(),
        "the receipt is the admission"
    );
    let apply = rig
        .operation_when(&apply.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(
        apply.outcome,
        Some(HqpOutputOutcome::Complete),
        "{apply:#?}"
    );
    let t = setup_result(&apply).expect("typed setup transaction");
    assert_eq!(t.step, "apply");
    assert!(t.uploaded);
    assert_eq!(t.daemon_response.as_deref(), Some("HTTP 200 OK"));
    assert_eq!(
        (t.runtime_matches, t.disk_matches, t.readback_matches),
        (Some(true), Some(true), Some(true)),
        "{t:#?}"
    );
    assert!(
        t.settled_after_ms.is_some_and(|ms| ms >= 250),
        "confirmed only after the daemon applied it: {t:#?}"
    );
    assert!(t.rollback_available);
    let posted = web.posted();
    assert_eq!(posted.len(), 1);
    let form = &posted[0];
    assert_eq!(posted_value(form, "backend"), Some("network"));
    assert_eq!(
        posted_value(form, "net_device"),
        Some("HiPhi Router/hiphi:router")
    );
    assert_eq!(posted_value(form, "net_bits"), Some("32"));
    assert_eq!(posted_value(form, "net_period"), Some("250"));
    assert_eq!(
        posted_value(form, "mode"),
        Some("sdm"),
        "DSP controls are resubmitted unchanged"
    );
    assert_eq!(posted_value(form, "filter"), Some("40"));
    assert_eq!(posted_value(form, "dsd_6db"), Some("1"));
    assert_eq!(posted_value(form, "log_enabled"), Some("1"));
    assert_eq!(posted_value(form, "title"), Some("HQPlayerEmbedded"));
    assert_eq!(
        posted_value(form, "net_dop"),
        None,
        "an unchecked box stays absent"
    );
    assert!(web
        .requests()
        .iter()
        .any(|(r, auth)| r == "POST /config" && *auth));
    assert_eq!(web.running("backend").as_deref(), Some("network"));
    assert_eq!(web.running("mode").as_deref(), Some("sdm"));
    let disk = String::from_utf8_lossy(&web.disk()).to_string();
    assert!(
        disk.contains("<output type=\"network\"/>")
            && disk.contains("address=\"HiPhi Router\" device=\"hiphi:router\""),
        "{disk}"
    );
    assert_eq!(
        t.readback_sha256.as_deref(),
        Some(mock_servers::naa::sha256_hex(&web.disk()).as_str())
    );
    assert_eq!(
        model.request_count("Stop") + model.request_count("Play"),
        0,
        "setup never touches transport"
    );

    // Readback is a separate, honest read.
    let readback = rig
        .command(HqpOutputAction::SetupReadback, None)
        .await
        .expect("readback admitted");
    let readback = rig
        .operation_when(&readback.operation.operation_id, |o| o.is_terminal())
        .await;
    let t = setup_result(&readback).expect("typed");
    assert_eq!(
        (
            t.step.as_str(),
            t.readback_matches,
            t.uploaded,
            t.rollback_available
        ),
        ("readback", Some(true), false, true)
    );

    // Rollback: running form first, then raw persistent bytes, both read back.
    let rollback = rig
        .command(HqpOutputAction::SetupRollback, Some("setup-rollback"))
        .await
        .expect("rollback admitted");
    let rollback = rig
        .operation_when(&rollback.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(
        rollback.outcome,
        Some(HqpOutputOutcome::Complete),
        "{rollback:#?}"
    );
    let t = setup_result(&rollback).expect("typed");
    assert_eq!(t.step, "rollback");
    assert_eq!(
        (t.runtime_matches, t.disk_matches),
        (Some(true), Some(true))
    );
    assert!(!t.rollback_available, "nothing left to roll back");
    let posted = web.posted();
    assert_eq!(
        posted.len(),
        2,
        "rollback re-posted the previous running form"
    );
    assert_eq!(posted_value(&posted[1], "backend"), Some("alsa"));
    assert_eq!(posted_value(&posted[1], "net_bits"), Some("0"));
    assert_eq!(posted_value(&posted[1], "mode"), Some("sdm"));
    assert_eq!(web.running("backend").as_deref(), Some("alsa"));
    let order: Vec<String> = web
        .requests()
        .into_iter()
        .filter(|(r, auth)| *auth && r.starts_with("POST"))
        .map(|(r, _)| r)
        .collect();
    assert_eq!(
        order,
        vec!["POST /config", "POST /config", "POST /restore"],
        "form before raw restore"
    );
    assert_eq!(
        web.restore_seen_running_backend(),
        vec![Some("alsa".to_string())],
        "the raw restore was uploaded only once the daemon had applied the baseline form"
    );
    assert_eq!(web.disk(), ORIGINAL_DISK, "raw bytes restored exactly");
    // Long after any delayed daemon write could have fired, the exact bytes are still there.
    tokio::time::sleep(Duration::from_millis(450)).await;
    assert_eq!(
        web.disk(),
        ORIGINAL_DISK,
        "no delayed daemon write overwrote the restored bytes"
    );
    assert_eq!(web.running("backend").as_deref(), Some("alsa"));
    web.stop();
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// HQPlayer has not discovered the relay: the preview reports a blocker and apply is refused
/// instead of inventing an option.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn setup_preview_reports_a_blocker_when_hqplayer_has_not_discovered_the_relay() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let web = FakeHqpWeb::start(false, Duration::from_millis(10)).await;
    let rig = Rig::new("setupblock").await;
    let _adapter = rig.attach_with_web(&daemon, &web).await;
    let p = rig.setup_preview().await;
    assert!(!p.applicable);
    assert!(
        p.blocker
            .as_deref()
            .is_some_and(|b| b.contains("not discovered")),
        "{p:#?}"
    );
    assert_eq!(p.relay_option, None);
    assert!(p.changes.is_empty());
    let apply = rig
        .command(
            HqpOutputAction::SetupApply {
                preview_id: p.preview_id,
            },
            None,
        )
        .await
        .expect("admitted");
    assert_eq!(apply.operation.outcome, Some(HqpOutputOutcome::Rejected));
    assert!(web.posted().is_empty());
    web.stop();
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// Apply is bound to the successful controls the preview read: a running form that changed since
/// (an operator edit or a profile load) is refused under the lease before any post.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn setup_apply_is_refused_when_the_running_form_changed_since_the_preview() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let web = FakeHqpWeb::start(true, Duration::from_millis(10)).await;
    let rig = Rig::new("setupstale").await;
    let _adapter = rig.attach_with_web(&daemon, &web).await;
    let p = rig.setup_preview().await;
    web.select("filter", "41");
    let apply = rig
        .command(
            HqpOutputAction::SetupApply {
                preview_id: p.preview_id,
            },
            None,
        )
        .await
        .expect("admitted");
    let apply = rig
        .operation_when(&apply.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(
        apply.outcome,
        Some(HqpOutputOutcome::Rejected),
        "{apply:#?}"
    );
    assert!(apply
        .detail
        .as_deref()
        .is_some_and(|d| d.contains("changed since the preview")));
    assert!(web.posted().is_empty(), "no post on a changed form");
    web.stop();
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// A setup apply parked on its lease-held re-read is superseded by Stop: it is cancelled and never
/// posts. Two transactions armed in turn keep their identities: the older one's cleanup never
/// clears the newer one's, whether or not the newer one was itself cancelled.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn superseded_setup_transactions_never_post_and_never_clear_a_newer_one() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let web = FakeHqpWeb::start(true, Duration::from_millis(10)).await;
    let rig = Rig::new("setupsuper").await;
    let adapter = rig.attach_with_web(&daemon, &web).await;
    let p = rig.setup_preview().await;

    // A: held on its re-read.
    let (reached, release) = web.hold_config_reads();
    let a = rig
        .command(
            HqpOutputAction::SetupApply {
                preview_id: p.preview_id.clone(),
            },
            Some("apply-a"),
        )
        .await
        .expect("A admitted");
    tokio::time::timeout(Duration::from_secs(4), reached.notified())
        .await
        .expect("A reached its re-read");
    let generation_a = adapter
        .pending_setup_generation_for_tests()
        .expect("A pending");
    // B: arms a newer transaction (A is cancelled); B is held too.
    let b = rig
        .command(
            HqpOutputAction::SetupApply {
                preview_id: p.preview_id.clone(),
            },
            Some("apply-b"),
        )
        .await
        .expect("B admitted");
    tokio::time::timeout(Duration::from_secs(4), reached.notified())
        .await
        .expect("B reached its re-read");
    let generation_b = adapter
        .pending_setup_generation_for_tests()
        .expect("B pending");
    assert!(generation_b > generation_a);
    // Stop supersedes B as well while both are held.
    rig.command(HqpOutputAction::Stop, Some("stop-all"))
        .await
        .expect("Stop accepted");
    web.release_config_reads(&release);
    let a = rig
        .operation_when(&a.operation.operation_id, |o| o.is_terminal())
        .await;
    let b = rig
        .operation_when(&b.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(a.outcome, Some(HqpOutputOutcome::Cancelled), "{a:#?}");
    assert_eq!(b.outcome, Some(HqpOutputOutcome::Cancelled), "{b:#?}");
    assert!(
        web.posted().is_empty(),
        "a superseded transaction never posts"
    );
    assert_eq!(adapter.pending_setup_generation_for_tests(), None);

    // Now a fresh C completes and A's stale cleanup (already ran) did not disturb it.
    let c = rig
        .command(
            HqpOutputAction::SetupApply {
                preview_id: p.preview_id,
            },
            Some("apply-c"),
        )
        .await
        .expect("C admitted");
    let c = rig
        .operation_when(&c.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(c.outcome, Some(HqpOutputOutcome::Complete), "{c:#?}");
    assert_eq!(web.posted().len(), 1);
    assert_eq!(adapter.pending_setup_generation_for_tests(), None);
    web.stop();
    daemon.shutdown().await;
    rig.shutdown().await;
}

/// Finding 29: with the native connection or the adapter state lock held, a hook with a short
/// deadline returns "not attempted" within its bound while the lock is STILL held.
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn native_hooks_return_within_their_deadline_while_a_lock_is_still_held() {
    use unified_hifi_control::adapters::hqplayer::naa_relay::{NativeHookFence, NativeHookOutcome};
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("bounded").await;
    let (adapter, _bind) = rig.attach(&daemon).await;
    let plays_before = model.request_count("Play");
    for hold_state in [false, true] {
        let connection_hold = if hold_state {
            None
        } else {
            Some(adapter.hold_native_connection_for_tests().await)
        };
        let state_hold = if hold_state {
            Some(adapter.hold_native_state_for_tests().await)
        } else {
            None
        };
        let fence = NativeHookFence::unconditional();
        let deadline = tokio::time::Instant::now() + Duration::from_millis(400);
        let started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            adapter.output_hook_play(&fence, deadline),
        )
        .await
        .expect("hook returned within the bound while the lock is still held")
        .expect("no wire error");
        assert!(
            matches!(outcome, NativeHookOutcome::NotAttempted(_)),
            "{outcome:?}"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let outcome = tokio::time::timeout(
            Duration::from_secs(2),
            adapter.output_hook_stop_verified(
                &fence,
                tokio::time::Instant::now() + Duration::from_millis(400),
            ),
        )
        .await
        .expect("stop hook returned within the bound")
        .expect("no wire error");
        assert!(
            matches!(outcome, NativeHookOutcome::NotAttempted(_)),
            "{outcome:?}"
        );
        drop(connection_hold);
        drop(state_hold);
    }
    assert_eq!(model.request_count("Play"), plays_before);
    assert_eq!(model.request_count("Stop"), 0);
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[allow(dead_code)]
fn unused(_: Value) {}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn instance_pipeline_controls_never_fall_back_to_default() {
    use axum::{
        extract::{Path, State},
        Json,
    };
    use unified_hifi_control::api::{
        hqp_instance_pipeline_handler, hqp_instance_pipeline_update_handler, HqpPipelineRequest,
    };
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("office-dsp").await;
    rig.attach(&daemon).await;
    let other_model = playing_daemon();
    let other = WireServer::start(Arc::new(other_model.clone()), WirePolicy::default()).await;
    rig.manager
        .add_instance(
            "default".into(),
            "127.0.0.1".into(),
            Some(other.port()),
            None,
            None,
            None,
        )
        .await;

    let read =
        hqp_instance_pipeline_handler(State(rig.state.clone()), Path("office-dsp".into())).await;
    assert_eq!(
        read.status(),
        axum::http::StatusCode::OK,
        "read must target the named instance"
    );
    let result = hqp_instance_pipeline_update_handler(
        State(rig.state.clone()),
        Path("office-dsp".into()),
        Json(HqpPipelineRequest {
            setting: "repeat".into(),
            value: serde_json::json!("all"),
        }),
    )
    .await;
    assert_eq!(
        result.status(),
        axum::http::StatusCode::OK,
        "mutation must target the named instance"
    );
    assert_eq!(model.state().repeat, 2);
    assert_eq!(
        other_model.state().repeat,
        0,
        "default instance must remain unchanged"
    );
    let missing = hqp_instance_pipeline_update_handler(
        State(rig.state.clone()),
        Path("missing".into()),
        Json(HqpPipelineRequest {
            setting: "repeat".into(),
            value: serde_json::json!("off"),
        }),
    )
    .await;
    assert!(!missing.status().is_success());
    assert_eq!(
        model.state().repeat,
        2,
        "unknown instance cannot change another engine"
    );
    other.shutdown().await;
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn renaming_hqplayer_preserves_identity_pairings_and_running_relay() {
    use axum::{extract::State, response::IntoResponse, Json};
    use unified_hifi_control::api::{hqp_instances_handler, zone_name_post, ZoneNameRequest};
    let daemon = WireServer::start(Arc::new(playing_daemon()), WirePolicy::default()).await;
    let rig = Rig::new("rename-target").await;
    let (adapter, bind) = rig.attach(&daemon).await;
    rig.state
        .hqp_zone_links
        .link_zone("roon:source".into(), rig.instance.clone())
        .await
        .unwrap();
    let before = rig.manager.instance_count().await;
    let response = zone_name_post(Json(ZoneNameRequest {
        zone_id: rig.zone_id(),
        name: Some("Listening Room".into()),
    }))
    .await;
    assert_eq!(
        response.into_response().status(),
        axum::http::StatusCode::OK
    );
    let response = hqp_instances_handler(State(rig.state.clone()))
        .await
        .into_response();
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    let row = body["instances"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == "rename-target")
        .unwrap();
    assert_eq!(row["display_name"], "Listening Room");
    assert_eq!(rig.manager.instance_count().await, before);
    assert!(Arc::ptr_eq(
        &adapter,
        &rig.manager.get(&rig.instance).await.unwrap()
    ));
    assert_eq!(
        rig.state
            .hqp_zone_links
            .get_instance_for_zone("roon:source")
            .await
            .as_deref(),
        Some("rename-target")
    );
    let projection = rig
        .outputs_when(|p| p.availability == HqpOutputAvailability::Available)
        .await;
    assert_eq!(
        projection.relay.bind.as_deref(),
        Some(bind.to_string().as_str())
    );
    assert_eq!(
        unified_hifi_control::api::load_app_settings().custom_zone_name(&rig.zone_id()),
        Some("Listening Room")
    );
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn optional_unknown_junk_filter_does_not_break_advanced_readback() {
    use axum::{
        extract::{Path, State},
        response::IntoResponse,
    };
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("optional-junk").await;
    rig.attach(&daemon).await;
    model.arm(|faults| {
        faults
            .reject_next
            .push(("GetJunkFilters".into(), "Unknown command".into()))
    });
    let response = api::hqp_instance_matrix_profiles_handler(
        State(rig.state.clone()),
        Path(rig.instance.clone()),
    )
    .await
    .into_response();
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let bytes = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let value: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(value["junk_filters_supported"], false);
    assert_eq!(value["junk_filters"], serde_json::json!([]));
    assert!(value["profiles"].is_array());
    let adapter = rig.manager.get(&rig.instance).await.unwrap();
    model.arm(|faults| {
        faults
            .reject_next
            .push(("GetJunkFilters".into(), "Internal failure".into()))
    });
    assert!(
        adapter.get_advanced_options_snapshot().await.is_err(),
        "Only an explicit unknown-command rejection is optional"
    );
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn create_only_configure_cannot_replace_an_existing_instance() {
    use axum::{extract::State, response::IntoResponse, Json};
    let rig = Rig::new("create-only").await;
    rig.manager
        .add_instance(
            rig.instance.clone(),
            "original.invalid".into(),
            Some(4321),
            None,
            None,
            None,
        )
        .await;
    let response = api::hqp_configure_handler(
        State(rig.state.clone()),
        Json(
            serde_json::from_value(serde_json::json!({
                "name": rig.instance, "host":"replacement.invalid", "create_only":true
            }))
            .unwrap(),
        ),
    )
    .await
    .into_response();
    assert_eq!(response.status(), axum::http::StatusCode::CONFLICT);
    assert_eq!(
        rig.manager
            .get(&rig.instance)
            .await
            .unwrap()
            .get_status()
            .await
            .host
            .as_deref(),
        Some("original.invalid")
    );
    rig.shutdown().await;
}

#[tokio::test]
#[ignore = "read-only verification against an explicitly supplied HQPlayer host"]
#[serial_test::serial(hqp_output_config)]
async fn live_desktop_advanced_readback() {
    isolate_config_dir();
    let host = std::env::var("UHC_HQP_TEST_HOST").expect("explicit test host required");
    let adapter = HqpAdapter::new(create_bus());
    adapter.configure(host, Some(4321), None, None, None).await;
    adapter
        .connect()
        .await
        .expect("connect to requested HQPlayer");
    let snapshot = adapter
        .get_advanced_options_snapshot()
        .await
        .expect("advanced readback");
    println!(
        "Advanced readback: junk supported={}, matrix profiles={}",
        snapshot.junk_filters_supported,
        snapshot.matrix_profiles.len()
    );
    assert!(
        !snapshot.junk_filters_supported,
        "this probe targets Desktop's known unsupported enumeration"
    );
    let error = adapter
        .fetch_profiles()
        .await
        .expect_err("Desktop must not use the Embedded profile endpoint");
    assert!(error.to_string().contains("require HQPlayer Embedded"));
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn relay_metadata_uses_declared_source_projection() {
    let rig = Rig::new("bound-metadata").await;
    let daemon = WireServer::start(Arc::new(playing_daemon()), WirePolicy::default()).await;
    rig.attach(&daemon).await;
    rig.state
        .hqp_zone_links
        .link_zone("roon:metadata-source".into(), rig.instance.clone())
        .await
        .unwrap();
    let mut zone = rig
        .state
        .aggregator
        .get_zone(&format!("hqplayer:{}", rig.instance))
        .await
        .unwrap();
    zone.zone_id = "roon:metadata-source".into();
    zone.source = "roon".into();
    let np = zone.now_playing.as_mut().unwrap();
    np.title = "Bound source title".into();
    np.artist = "Bound source artist".into();
    np.album = "Bound source album".into();
    np.image_key = Some("bound-artwork".into());
    rig.bus.publish(BusEvent::ZoneDiscovered { zone });
    tokio::time::timeout(Duration::from_secs(2), async {
        while rig
            .state
            .aggregator
            .get_zone("roon:metadata-source")
            .await
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (source, np) =
        unified_hifi_control::coordinator::relay_metadata_source(&rig.state, &rig.instance)
            .await
            .unwrap();
    assert_eq!(source, "roon:metadata-source");
    assert_eq!(np.title, "Bound source title");
    assert_eq!(np.image_key.as_deref(), Some("bound-artwork"));
    rig.state
        .hqp_zone_links
        .unlink_zone("roon:metadata-source")
        .await;
    assert!(
        unified_hifi_control::coordinator::relay_metadata_source(&rig.state, &rig.instance)
            .await
            .is_none(),
        "Unlinking must clear fallback metadata"
    );
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn bound_source_text_and_artwork_reach_naa_without_changing_audio() {
    let rig = Rig::new("metadata-wire").await;
    let daemon = WireServer::start(Arc::new(playing_daemon()), WirePolicy::default()).await;
    let (_, bind) = rig.attach(&daemon).await;
    let naa = FakeNaa::start("metadata-dac", "hw:metadata", 44100);
    let route = rig.add_route("metadata-dac", &naa, None).await;
    rig.command(HqpOutputAction::Select { route_id: route }, None)
        .await
        .unwrap();
    // The image service returns provider bytes unchanged for NAA, not a UI placeholder.
    let picture = vec![0xff, 0xd8, 0xff, 0xd9];
    let served = picture.clone();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let image_server = tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().route(
                "/art",
                axum::routing::get(move || {
                    let bytes = served.clone();
                    async move { ([("content-type", "image/jpeg")], bytes) }
                }),
            ),
        )
        .await
        .unwrap();
    });
    let mut zone = rig.aggregator.get_zone(&rig.zone_id()).await.unwrap();
    zone.zone_id = "openhome:bound-source".into();
    zone.source = "openhome".into();
    let np = zone.now_playing.as_mut().unwrap();
    np.title = "Source track one".into();
    np.artist = "Source artist".into();
    np.album = "Source album".into();
    np.image_key = Some(format!("http://{address}/art"));
    rig.bus
        .publish(BusEvent::ZoneDiscovered { zone: zone.clone() });
    rig.state
        .hqp_zone_links
        .link_zone(zone.zone_id.clone(), rig.instance.clone())
        .await
        .unwrap();
    let worker = tokio::spawn(unified_hifi_control::coordinator::run_relay_metadata(
        rig.state.clone(),
    ));
    let client = AutoHqpClient::start(bind, 44100);
    client.set_playing(true);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !naa.audio_records().iter().any(|r| r.picture == picture) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("bound artwork must reach NAA");
    assert!(naa
        .audio_records()
        .iter()
        .any(|r| String::from_utf8_lossy(&r.metadata).contains("song=Source track one")));
    zone.now_playing.as_mut().unwrap().title = "Source track two".into();
    rig.bus.publish(BusEvent::ZoneDiscovered { zone });
    tokio::time::timeout(Duration::from_secs(5), async {
        while !naa
            .audio_records()
            .iter()
            .any(|r| String::from_utf8_lossy(&r.metadata).contains("song=Source track two"))
        {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("track changes must update metadata in the same stream");
    tokio::time::sleep(Duration::from_millis(600)).await;
    let records = naa.audio_records();
    assert!(records
        .iter()
        .all(|r| r.payload == mock_servers::naa::auto_client_payload()));
    assert!(
        records.iter().filter(|r| !r.picture.is_empty()).count() <= 3,
        "artwork must not repeat on every audio frame"
    );
    assert!(records.len() > 3);
    rig.state.shutdown.cancel();
    worker.await.unwrap();
    client.close();
    image_server.abort();
    naa.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn saving_unchanged_relay_settings_preserves_port_and_audio_session() {
    let rig = Rig::new("save-live-relay").await;
    let daemon = WireServer::start(Arc::new(playing_daemon()), WirePolicy::default()).await;
    let (_, bind) = rig.attach(&daemon).await;
    let naa = FakeNaa::start("save-dac", "hw:save", 44100);
    let route = rig.add_route("save-dac", &naa, None).await;
    rig.command(HqpOutputAction::Select { route_id: route }, None)
        .await
        .unwrap();
    let client = AutoHqpClient::start(bind, 44100);
    client.set_playing(true);
    let before = rig.outputs_when(|p| p.session_confirms_audio()).await;
    let result = rig
        .command(
            HqpOutputAction::RelayConfigure {
                enabled: true,
                bind: Some("127.0.0.1:0".into()),
                hqp_allow: vec![],
                discovery_interface: None,
                discovery_port: Some(43210),
                adapter_name: Some(before.relay.adapter_name.clone()),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(result.operation.outcome, Some(HqpOutputOutcome::Complete));
    assert_eq!(
        result.projection.relay.bind, before.relay.bind,
        "automatic port must be reused"
    );
    assert_eq!(
        result.projection.session.as_ref().map(|s| s.session_id),
        before.session.as_ref().map(|s| s.session_id),
        "saving must not disconnect audio"
    );
    client.close();
    naa.close();
    daemon.shutdown().await;
    rig.shutdown().await;
}

struct PairedSourceFixture {
    client: Arc<AutoHqpClient>,
    paused: AtomicBool,
    resumed: AtomicBool,
    was_playing: AtomicBool,
    fail_pause: AtomicBool,
    hold_pause: AtomicBool,
}
#[async_trait::async_trait]
impl unified_hifi_control::adapters::hqplayer::naa_relay::RelaySourceControl
    for PairedSourceFixture
{
    async fn pause_for_switch(&self) -> Result<Option<(String, bool)>, String> {
        self.paused.store(true, Ordering::Release);
        while self.hold_pause.load(Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        if self.fail_pause.load(Ordering::Acquire) {
            return Err("pause rejected".into());
        }
        self.client.set_playing(false);
        Ok(Some((
            "roon:paired".into(),
            self.was_playing.load(Ordering::Acquire),
        )))
    }
    async fn pause_after_failed_resume(&self, source: &str) -> Result<(), String> {
        assert_eq!(source, "roon:paired");
        self.client.set_playing(false);
        Ok(())
    }
    async fn resume_after_switch(&self, source: &str) -> Result<(), String> {
        assert_eq!(source, "roon:paired");
        assert!(self.paused.load(Ordering::Acquire));
        self.client.set_playing(true);
        self.resumed.store(true, Ordering::Release);
        Ok(())
    }
}
#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn paired_source_switch_uses_source_transport_without_native_stop_or_play() {
    let model = playing_daemon();
    let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let rig = Rig::new("paired").await;
    let (adapter, bind) = rig.attach(&daemon).await;
    let a = FakeNaa::start("A", "hw:CARD=A,DEV=0", 44100);
    let b = FakeNaa::start("B", "hw:CARD=B,DEV=0", 44100);
    let route_a = rig.add_route("A", &a, None).await;
    let route_b = rig.add_route("B", &b, None).await;
    rig.command(
        HqpOutputAction::Select {
            route_id: route_a.clone(),
        },
        None,
    )
    .await
    .unwrap();
    let client = Arc::new(AutoHqpClient::start(bind, 44100));
    client.set_playing(true);
    rig.outputs_when(|p| p.session_confirms_audio()).await;
    let source = Arc::new(PairedSourceFixture {
        client: client.clone(),
        paused: AtomicBool::new(false),
        resumed: AtomicBool::new(false),
        was_playing: AtomicBool::new(true),
        fail_pause: AtomicBool::new(false),
        hold_pause: AtomicBool::new(false),
    });
    adapter.set_relay_source_control(source.clone());
    let receipt = rig
        .command(
            HqpOutputAction::Select {
                route_id: route_b.clone(),
            },
            Some("paired-switch"),
        )
        .await
        .unwrap();
    let done = rig
        .operation_when(&receipt.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(done.outcome, Some(HqpOutputOutcome::Complete), "{done:?}");
    assert!(
        source.paused.load(Ordering::Acquire),
        "must pause the paired source"
    );
    assert!(
        source.resumed.load(Ordering::Acquire),
        "must resume the paired source"
    );
    assert_eq!(model.request_count("Stop"), 0);
    assert_eq!(model.request_count("Play"), 0);
    assert!(b.audio_bytes() > 0);
    // An idempotent retry must not pause/play a second time.
    source.paused.store(false, Ordering::Release);
    let retry = rig
        .command(
            HqpOutputAction::Select {
                route_id: route_b.clone(),
            },
            Some("paired-switch"),
        )
        .await
        .unwrap();
    assert_eq!(retry.operation.operation_id, receipt.operation.operation_id);
    assert!(!source.paused.load(Ordering::Acquire));

    source.was_playing.store(false, Ordering::Release);
    source.resumed.store(false, Ordering::Release);
    client.set_playing(false);
    let paused = rig
        .command(
            HqpOutputAction::Select {
                route_id: route_a.clone(),
            },
            None,
        )
        .await
        .unwrap();
    let paused = rig
        .operation_when(&paused.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(paused.outcome, Some(HqpOutputOutcome::Complete));
    assert!(
        !source.resumed.load(Ordering::Acquire),
        "paused source must stay paused"
    );

    source.fail_pause.store(true, Ordering::Release);
    let failed = rig
        .command(
            HqpOutputAction::Select {
                route_id: route_b.clone(),
            },
            None,
        )
        .await
        .unwrap();
    let failed = rig
        .operation_when(&failed.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(failed.outcome, Some(HqpOutputOutcome::Failed));
    let unchanged = read_hqp_outputs(&rig.state, &rig.zone_id()).await.unwrap();
    assert_eq!(
        unchanged.selected_route_id.as_deref(),
        Some(route_a.as_str())
    );
    assert!(!source.resumed.load(Ordering::Acquire));

    // Stop cancels an in-flight source pause; completing it later cannot resume audio.
    source.fail_pause.store(false, Ordering::Release);
    source.hold_pause.store(true, Ordering::Release);
    source.paused.store(false, Ordering::Release);
    let held = rig
        .command(HqpOutputAction::Select { route_id: route_b }, None)
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        while !source.paused.load(Ordering::Acquire) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    rig.command(HqpOutputAction::Stop, None).await.unwrap();
    source.hold_pause.store(false, Ordering::Release);
    let cancelled = rig
        .operation_when(&held.operation.operation_id, |o| o.is_terminal())
        .await;
    assert_eq!(cancelled.outcome, Some(HqpOutputOutcome::Cancelled));
    assert!(!source.resumed.load(Ordering::Acquire));
    drop(source);
    drop(adapter);
    rig.shutdown().await;
    drop(client);
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn roon_source_bridge_requires_one_exact_binding_and_preserves_paused_state() {
    use unified_hifi_control::coordinator::relay_source_control;
    let rig = Rig::new("paired-resolution").await;
    let daemon = WireServer::start(Arc::new(playing_daemon()), WirePolicy::default()).await;
    rig.attach(&daemon).await;
    let control = relay_source_control(&rig.state, &rig.instance);
    assert_eq!(control.pause_for_switch().await.unwrap(), None);
    rig.state
        .hqp_zone_links
        .link_zone("roon:source".into(), rig.instance.clone())
        .await
        .unwrap();
    assert!(
        control.pause_for_switch().await.is_err(),
        "missing source must fail closed"
    );
    let mut zone = rig.state.aggregator.get_zone(&rig.zone_id()).await.unwrap();
    zone.zone_id = "roon:source".into();
    zone.source = "roon".into();
    zone.state = unified_hifi_control::bus::PlaybackState::Paused;
    rig.bus.publish(BusEvent::ZoneDiscovered { zone });
    tokio::time::timeout(Duration::from_secs(2), async {
        while rig.state.aggregator.get_zone("roon:source").await.is_none() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    // There is no Roon command endpoint in this fixture: paused must require no command.
    assert_eq!(
        control.pause_for_switch().await.unwrap(),
        Some(("roon:source".into(), false))
    );
    rig.state
        .hqp_zone_links
        .link_zone("roon:other".into(), rig.instance.clone())
        .await
        .unwrap();
    assert!(
        control.pause_for_switch().await.is_err(),
        "ambiguous pairing must not choose a zone"
    );
    rig.state.hqp_zone_links.unlink_zone("roon:source").await;
    assert!(
        control.resume_after_switch("roon:source").await.is_err(),
        "changed binding must not resume old source"
    );
    assert!(
        control
            .pause_after_failed_resume("roon:source")
            .await
            .is_err(),
        "cleanup must not pause a newly bound zone"
    );
    daemon.shutdown().await;
    rig.shutdown().await;
}

#[tokio::test]
#[serial_test::serial(hqp_output_config)]
async fn repeated_config_load_preserves_all_instance_relay_settings() {
    use unified_hifi_control::adapters::hqplayer::{
        load_hqp_configs, save_hqp_configs, HqpInstanceConfig,
    };
    isolate_config_dir();
    let configs: Vec<_> = ["default", "office"]
        .into_iter()
        .enumerate()
        .map(|(index, name)| HqpInstanceConfig {
            name: name.into(),
            host: "127.0.0.1".into(),
            port: 45000 + index as u16,
            web_port: 8080,
            username: None,
            password: None,
            naa_relay: Some(NaaRelaySettings {
                enabled: true,
                adapter_name: format!("My {name} relay"),
                bind: format!("127.0.0.1:{}", 46000 + index),
                ..Default::default()
            }),
        })
        .collect();
    assert!(save_hqp_configs(&configs));
    for _ in 0..2 {
        let manager = HqpInstanceManager::new(create_bus());
        manager.load_from_config().await;
        let saved = load_hqp_configs();
        assert_eq!(
            saved.len(),
            configs.len(),
            "loading must not rewrite the instance array as a legacy object"
        );
        for expected in &configs {
            let actual = saved.iter().find(|c| c.name == expected.name).unwrap();
            assert_eq!(
                actual.naa_relay, expected.naa_relay,
                "relay settings must survive every startup"
            );
            let adapter = manager.get(&expected.name).await.unwrap();
            assert_eq!(
                adapter.output_relay_settings().await,
                expected.naa_relay.clone().unwrap()
            );
        }
    }
}
