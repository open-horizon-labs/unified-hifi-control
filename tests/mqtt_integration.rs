//! End-to-end coverage for the MQTT/Home Assistant publisher (#508, #529)
//! against a real, in-process MQTT broker (`rumqttd`), the "mock/in-process
//! broker" the issue's acceptance criteria calls for. Exercises the
//! acceptance criteria that need a live broker round trip:
//! - HA discovery configs are published (retained) for a newly discovered
//!   zone, grouped under one device, and retracted when the zone is removed.
//! - The retained state topic reflects playback/volume/mute/now-playing
//!   from bus events.
//! - Inbound command topics for a registry-backed provider route to the
//!   owning adapter through `AdapterRegistry`, and are gracefully refused
//!   for a legacy zone with no reliable command gateway configured (#508).
//! - Inbound command topics for a legacy zone route through the reliable
//!   command gateway to a real adapter and a real mock server, exactly like
//!   HTTP/knob/MCP (#529).

#[allow(dead_code, unused_imports)]
mod mock_servers;

use std::collections::HashMap;
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use tokio::time::timeout;

use mock_servers::MockLmsServer;
use unified_hifi_control::adapters::lms::{create_lms_adapters_with_runtime, LmsRuntimeBridge};
use unified_hifi_control::adapters::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic, Startable,
};
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::credentials::MqttCredentialRecord;
use unified_hifi_control::api::AdapterRegistry;
use unified_hifi_control::bus::runtime::build_runtime;
use unified_hifi_control::bus::{
    create_bus, BusEvent, NowPlaying, PlaybackState, PrefixedZoneId, VolumeControl, VolumeScale,
    Zone,
};
use unified_hifi_control::knobs::store::KnobStatusUpdate;
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mqtt::MqttPublisher;

/// Isolate `UHC_CONFIG_DIR` for the one test below that starts a real
/// `LmsAdapter`, which persists its configured host to disk. No other test
/// in this binary touches the env var, but the guard/temp-dir pairing
/// mirrors `tests/mcp_contract.rs`'s `SettingsFixture` so a future addition
/// here does not silently write into a developer's real config directory.
struct ConfigDirGuard {
    _dir: tempfile::TempDir,
    previous: Option<String>,
}

impl ConfigDirGuard {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let previous = std::env::var("UHC_CONFIG_DIR").ok();
        std::env::set_var("UHC_CONFIG_DIR", dir.path());
        Self {
            _dir: dir,
            previous,
        }
    }
}

impl Drop for ConfigDirGuard {
    fn drop(&mut self) {
        match &self.previous {
            Some(value) => std::env::set_var("UHC_CONFIG_DIR", value),
            None => std::env::remove_var("UHC_CONFIG_DIR"),
        }
    }
}

/// Isolate `KnobStore`'s on-disk persistence to a scratch directory shared by
/// every test in this process, so tests never touch a real user config dir.
fn isolated_knob_store() -> KnobStore {
    use std::sync::OnceLock;
    static DIR: OnceLock<tempfile::TempDir> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("UHC_CONFIG_DIR", dir.path());
        dir
    });
    KnobStore::new()
}

/// Bind an ephemeral port and hand its number back for `rumqttd`'s config,
/// which does its own binding. The listener is dropped immediately before
/// the broker binds - the same "ask the OS for a free port" pattern
/// `tests/mock_servers` uses elsewhere in this suite, just synchronous
/// since `rumqttd::Broker` cannot adopt a pre-bound listener.
fn free_port() -> u16 {
    #[allow(clippy::unwrap_used)] // binding to port 0 on loopback cannot fail in test envs
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Start an in-process MQTT broker on `port` and return once it has
/// accepted the port (best-effort: rumqttd has no readiness hook, so
/// callers retry their first connection instead of trusting a fixed delay
/// alone).
fn start_test_broker(port: u16) {
    let mut connections = HashMap::new();
    connections.insert(
        "test".to_string(),
        rumqttd::ServerSettings {
            name: "test".to_string(),
            listen: format!("127.0.0.1:{port}")
                .parse()
                .expect("valid loopback addr"),
            tls: None,
            next_connection_delay_ms: 0,
            connections: rumqttd::ConnectionSettings {
                connection_timeout_ms: 5000,
                max_payload_size: 256 * 1024,
                max_inflight_count: 100,
                auth: None,
                external_auth: None,
                dynamic_filters: false,
            },
        },
    );
    let config = rumqttd::Config {
        id: 0,
        router: rumqttd::RouterConfig {
            max_connections: 100,
            max_outgoing_packet_count: 200,
            max_segment_size: 1024 * 1024,
            max_segment_count: 10,
            custom_segment: None,
            initialized_filters: None,
            shared_subscriptions_strategy: Default::default(),
        },
        v4: Some(connections),
        v5: None,
        ws: None,
        cluster: None,
        console: None,
        bridge: None,
        prometheus: None,
        metrics: None,
    };

    std::thread::spawn(move || {
        let mut broker = rumqttd::Broker::new(config);
        // rumqttd's `start()` blocks the calling thread for the broker's
        // lifetime; a dedicated OS thread keeps it off the Tokio runtime
        // the test itself uses. Readiness is confirmed by
        // `wait_for_broker_ready` below rather than a fixed delay.
        let _ = broker.start();
    });
}

/// Poll the broker's TCP port until it accepts a connection, rather than
/// trusting a fixed sleep after spawning it (rumqttd has no readiness hook).
async fn wait_for_broker_ready(port: u16, deadline: Duration) {
    let started = tokio::time::Instant::now();
    loop {
        if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(
            started.elapsed() < deadline,
            "test broker on port {port} never accepted a connection"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

async fn connect_test_client(port: u16, client_id: &str) -> (AsyncClient, rumqttc::EventLoop) {
    let mut options = MqttOptions::new(client_id, "127.0.0.1", port);
    options.set_keep_alive(Duration::from_secs(5));
    AsyncClient::new(options, 64)
}

/// Subscribe and drive the event loop until the broker acknowledges it, so
/// the caller can rely on the subscription being live rather than merely
/// queued in-process.
async fn subscribe_and_wait(client: &AsyncClient, eventloop: &mut rumqttc::EventLoop, topic: &str) {
    client
        .subscribe(topic, QoS::AtLeastOnce)
        .await
        .expect("queue subscribe request");
    timeout(Duration::from_secs(10), async {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::SubAck(_))) => return,
                Ok(_) => continue,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await
    .expect("broker acknowledged subscribe");
}

/// Poll an event loop until `predicate` matches an incoming publish, or the
/// deadline elapses.
async fn wait_for_publish<F>(
    eventloop: &mut rumqttc::EventLoop,
    deadline: Duration,
    mut predicate: F,
) -> Option<rumqttc::Publish>
where
    F: FnMut(&rumqttc::Publish) -> bool,
{
    timeout(deadline, async {
        loop {
            match eventloop.poll().await {
                Ok(Event::Incoming(Packet::Publish(publish))) if predicate(&publish) => {
                    return publish;
                }
                Ok(_) => continue,
                Err(_) => {
                    // Transient connection errors are expected while the
                    // client is still establishing its session; keep polling.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
            }
        }
    })
    .await
    .ok()
}

fn zone_fixture(zone_id: &str, source: &str) -> Zone {
    Zone {
        zone_id: zone_id.to_string(),
        zone_name: "Test Zone".to_string(),
        state: PlaybackState::Playing,
        volume_control: Some(VolumeControl {
            value: 30.0,
            min: 0.0,
            max: 100.0,
            step: 1.0,
            is_muted: false,
            scale: VolumeScale::Percentage,
            output_id: Some("out1".to_string()),
        }),
        now_playing: Some(NowPlaying {
            title: "Integration Song".to_string(),
            artist: "Integration Artist".to_string(),
            album: "Integration Album".to_string(),
            image_key: Some("art-key".to_string()),
            seek_position: Some(1.0),
            duration: Some(100.0),
            metadata: None,
            repeat_mode: None,
            shuffle: None,
        }),
        source: source.to_string(),
        is_controllable: true,
        is_seekable: true,
        last_updated: 0,
        is_play_allowed: true,
        is_pause_allowed: true,
        is_next_allowed: true,
        is_previous_allowed: true,
    }
}

/// Records every command it receives so the test can assert on routing.
struct RecordingAdapter {
    prefix: &'static str,
    received: Mutex<Vec<(String, AdapterCommand)>>,
}

#[async_trait]
impl AdapterLogic for RecordingAdapter {
    fn prefix(&self) -> &'static str {
        self.prefix
    }

    async fn run(&self, _ctx: AdapterContext) -> anyhow::Result<()> {
        Ok(())
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> anyhow::Result<AdapterCommandResponse> {
        self.received
            .lock()
            .expect("recording adapter mutex")
            .push((zone_id.to_string(), command));
        Ok(AdapterCommandResponse {
            success: true,
            error: None,
        })
    }
}

async fn wait_until<F: Fn() -> bool>(predicate: F, deadline: Duration) -> bool {
    let started = tokio::time::Instant::now();
    while started.elapsed() < deadline {
        if predicate() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    predicate()
}

#[tokio::test(flavor = "multi_thread")]
async fn discovers_publishes_state_and_routes_commands_over_a_real_broker() {
    let port = free_port();
    start_test_broker(port);
    wait_for_broker_ready(port, Duration::from_secs(5)).await;

    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let aggregator_task = aggregator.clone();
    tokio::spawn(async move {
        aggregator_task.run_with_ready(ready_tx).await;
    });
    ready_rx.await.expect("aggregator ready");

    let adapter_registry = Arc::new(AdapterRegistry::default());
    let recorder = Arc::new(RecordingAdapter {
        prefix: "musicassistant",
        received: Mutex::new(Vec::new()),
    });
    adapter_registry.register(recorder.clone()).await;

    let publisher = Arc::new(MqttPublisher::new(
        bus.clone(),
        aggregator.clone(),
        adapter_registry.clone(),
        isolated_knob_store(),
    ));
    publisher.set_base_url("http://uhc.test:8088".to_string());
    publisher
        .configure(MqttCredentialRecord {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
            username: None,
            password: None,
            base_topic: "unified-hifi".to_string(),
            discovery_prefix: "homeassistant".to_string(),
        })
        .await;
    publisher.set_enabled(true).await;
    assert!(publisher.is_running().await);

    let (observer_client, mut observer_loop) = connect_test_client(port, "test-observer").await;
    // `AsyncClient::subscribe().await` only queues the request; nothing is
    // actually sent to the broker until the event loop is polled. Driving
    // it to each SubAck here - rather than starting to poll only inside
    // `wait_for_publish` after the bus event below - closes a real race
    // where the publisher's message could reach the broker before this
    // subscriber's SUBSCRIBE packet does.
    subscribe_and_wait(&observer_client, &mut observer_loop, "homeassistant/#").await;
    subscribe_and_wait(&observer_client, &mut observer_loop, "unified-hifi/#").await;

    // --- Availability: bridge already announced online by the time this
    //     subscriber's SubAck arrives (retained from `announce_all_zones`
    //     on connect). Checked here, before any zone exists, because this
    //     retained message is delivered as part of the subscribe reply -
    //     a later `wait_for_publish` call would never see it again, since
    //     an earlier predicate that does not match it still consumes it
    //     from the stream. ---
    let availability = wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
        publish.topic == "unified-hifi/bridge/status"
    })
    .await
    .expect("availability topic published");
    assert_eq!(availability.payload.as_ref(), b"online");

    let zone_id = "musicassistant:zone1";
    bus.publish(BusEvent::ZoneDiscovered {
        zone: zone_fixture(zone_id, "musicassistant"),
    });

    // --- Discovery: state sensor config is retained and device-grouped ---
    let discovery = wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
        publish.topic.contains("/sensor/") && publish.topic.ends_with("_state/config")
    })
    .await
    .expect("state sensor discovery config published");
    // Per MQTT 3.1.1 3.3.1.3, a broker clears RETAIN on a live forward and
    // only sets it when replaying a retained message to a *new* subscriber -
    // this client subscribed before the publish. Retained storage itself is
    // asserted below via a late subscriber.
    let payload: serde_json::Value =
        serde_json::from_slice(&discovery.payload).expect("discovery payload is JSON");
    assert_eq!(
        payload["device"]["identifiers"][0],
        serde_json::json!("uhc_musicassistant_zone1")
    );
    assert_eq!(
        payload["availability_topic"],
        serde_json::json!("unified-hifi/bridge/status")
    );

    // --- State: retained payload reflects now-playing/volume/mute ---
    let state = wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
        publish.topic == format!("unified-hifi/media_player/{}/state", "musicassistant_zone1")
    })
    .await
    .expect("state topic published");
    let state_payload: serde_json::Value =
        serde_json::from_slice(&state.payload).expect("state payload is JSON");
    assert_eq!(state_payload["state"], serde_json::json!("playing"));
    assert_eq!(
        state_payload["title"],
        serde_json::json!("Integration Song")
    );
    assert_eq!(state_payload["volume"], serde_json::json!(30.0));
    assert_eq!(state_payload["muted"], serde_json::json!(false));
    assert_eq!(
        state_payload["picture"],
        serde_json::json!("http://uhc.test:8088/now_playing/image?zone_id=musicassistant%3Azone1")
    );

    // --- Retention: a brand-new subscriber gets the state immediately,
    //     with RETAIN set, proving the broker actually stored it retained
    //     rather than this test only ever seeing live forwards. ---
    let (late_client, mut late_loop) = connect_test_client(port, "test-late-subscriber").await;
    late_client
        .subscribe(
            "unified-hifi/media_player/musicassistant_zone1/state",
            QoS::AtLeastOnce,
        )
        .await
        .expect("late subscribe to state topic");
    let replayed = wait_for_publish(&mut late_loop, Duration::from_secs(15), |publish| {
        publish.topic == "unified-hifi/media_player/musicassistant_zone1/state"
    })
    .await
    .expect("retained state replayed to a fresh subscriber");
    assert!(
        replayed.retain,
        "retained replay to a new subscriber must set RETAIN"
    );

    // --- Commands: HA -> UHC volume_set routes to the owning adapter ---
    let command_client_id = "test-commander";
    let (command_client, mut command_loop) = connect_test_client(port, command_client_id).await;
    tokio::spawn(async move {
        loop {
            if command_loop.poll().await.is_err() {
                break;
            }
        }
    });
    // Give the client time to complete its CONNACK before publishing.
    tokio::time::sleep(Duration::from_millis(200)).await;
    command_client
        .publish(
            "unified-hifi/media_player/musicassistant_zone1/volume/set",
            QoS::AtLeastOnce,
            false,
            "42",
        )
        .await
        .expect("publish volume command");

    let routed = wait_until(
        || {
            recorder
                .received
                .lock()
                .expect("recording adapter mutex")
                .iter()
                .any(|(zone, command)| {
                    zone == zone_id && matches!(command, AdapterCommand::VolumeAbsolute(42))
                })
        },
        Duration::from_secs(5),
    )
    .await;
    assert!(
        routed,
        "volume command should route to the recording adapter"
    );

    // --- Commands: an unbridged legacy zone is ignored gracefully ---
    let unbridged_calls_before = recorder.received.lock().expect("mutex").len();
    bus.publish(BusEvent::ZoneDiscovered {
        zone: zone_fixture("roon:legacy1", "roon"),
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    command_client
        .publish(
            "unified-hifi/media_player/roon_legacy1/play/set",
            QoS::AtLeastOnce,
            false,
            "PRESS",
        )
        .await
        .expect("publish play command for legacy zone");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        recorder.received.lock().expect("mutex").len(),
        unbridged_calls_before,
        "a zone with no registry-backed adapter must not reach any adapter"
    );

    // --- Retraction: removing a zone clears its retained discovery config ---
    bus.publish(BusEvent::ZoneRemoved {
        zone_id: PrefixedZoneId::parse(zone_id).expect("valid zone id"),
    });
    // Match on an empty payload too, not just the topic: QoS 1 can
    // redeliver an earlier non-empty discovery config for the same topic
    // (e.g. on a PUBACK race), and that redelivery is not the retraction
    // this assertion cares about.
    let retraction = wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
        publish.topic.contains("musicassistant_zone1")
            && publish.topic.contains("/sensor/")
            && publish.topic.ends_with("_state/config")
            && publish.payload.is_empty()
    })
    .await
    .expect("retraction publish observed");
    assert!(
        retraction.payload.is_empty(),
        "retraction must publish an empty retained payload"
    );

    publisher.shutdown().await;
    assert!(!publisher.is_running().await);
}

/// Confirms a publisher with a broker that refuses connections keeps
/// retrying without panicking or blocking `set_enabled`/`shutdown` - the
/// resilience half of "off by default", where "off" also covers "broker
/// unreachable".
#[tokio::test(flavor = "multi_thread")]
async fn tolerates_an_unreachable_broker() {
    let port = free_port(); // never started - nothing listens on it
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let aggregator_task = aggregator.clone();
    tokio::spawn(async move {
        aggregator_task.run_with_ready(ready_tx).await;
    });
    ready_rx.await.expect("aggregator ready");
    let adapter_registry = Arc::new(AdapterRegistry::default());

    let publisher = MqttPublisher::new(
        bus.clone(),
        aggregator.clone(),
        adapter_registry,
        isolated_knob_store(),
    );
    publisher
        .configure(MqttCredentialRecord {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
            username: None,
            password: None,
            base_topic: "unified-hifi".to_string(),
            discovery_prefix: "homeassistant".to_string(),
        })
        .await;
    publisher.set_enabled(true).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        publisher.is_running().await,
        "task keeps retrying, not crashed"
    );

    // Must still stop promptly even mid-retry-loop.
    let stopped = timeout(Duration::from_secs(6), publisher.shutdown()).await;
    assert!(
        stopped.is_ok(),
        "shutdown must not hang on an unreachable broker"
    );
}

/// Command round-trip for a legacy (non-registry) zone (#529): an HA `volume_set` and a `play`
/// published over a real broker must reach a real `LmsAdapter` through the reliable command
/// gateway - the same path `dispatch_lms_runtime_command` gives HTTP/knob/MCP - and land on the
/// wire at a real mock LMS server, not merely be accepted by the gateway.
#[tokio::test(flavor = "multi_thread")]
async fn mqtt_command_for_a_legacy_zone_routes_through_the_command_gateway_to_lms() {
    let _config_dir = ConfigDirGuard::new();

    const PLAYER_ID: &str = "aa:bb:cc:dd:ee:ff";
    let mock = MockLmsServer::start().await;
    mock.add_player(PLAYER_ID, "Living Room").await;
    mock.set_volume(PLAYER_ID, 10).await;

    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let aggregator_task = aggregator.clone();
    tokio::spawn(async move {
        aggregator_task.run_with_ready(ready_tx).await;
    });
    ready_rx.await.expect("aggregator ready");

    let runtime = build_runtime(aggregator.clone(), 16, 32);
    let bridge = Arc::new(LmsRuntimeBridge::new(
        runtime.projection_ingress.clone(),
        runtime.commands.clone(),
    ));
    let (lms, _lms_cli) = create_lms_adapters_with_runtime(bus.clone(), Some(bridge));
    lms.configure(
        mock.addr().ip().to_string(),
        Some(mock.addr().port()),
        None,
        None,
    )
    .await;
    tokio::spawn(runtime.projection_actor.run());
    lms.start().await.expect("LMS adapter must start");

    let zone_id = format!("lms:{PLAYER_ID}");
    let mut found = false;
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if aggregator.get_zone(&zone_id).await.is_some() {
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found,
        "LMS player never reached the aggregator as {zone_id}"
    );

    let port = free_port();
    start_test_broker(port);
    wait_for_broker_ready(port, Duration::from_secs(5)).await;

    let adapter_registry = Arc::new(AdapterRegistry::default());
    let publisher = Arc::new(MqttPublisher::new(
        bus.clone(),
        aggregator.clone(),
        adapter_registry,
        isolated_knob_store(),
    ));
    // Attach the gateway before enabling: `restart()` only snapshots it when the publisher task
    // is (re)spawned, exactly like `AppState::reliable_commands` is attached before any surface
    // can dispatch through it.
    publisher.set_reliable_commands(runtime.commands.clone());
    publisher
        .configure(MqttCredentialRecord {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
            username: None,
            password: None,
            base_topic: "unified-hifi".to_string(),
            discovery_prefix: "homeassistant".to_string(),
        })
        .await;
    publisher.set_enabled(true).await;
    assert!(publisher.is_running().await);

    // Wait for the publisher to discover and announce the zone (needed to populate its slug ->
    // zone id map, which inbound commands are routed through).
    let (observer_client, mut observer_loop) = connect_test_client(port, "test-lms-observer").await;
    subscribe_and_wait(
        &observer_client,
        &mut observer_loop,
        "unified-hifi/media_player/+/state",
    )
    .await;
    let state_publish = wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
        publish.topic.starts_with("unified-hifi/media_player/lms_")
    })
    .await
    .expect("LMS zone state must be published before its slug can route commands");
    let slug = state_publish
        .topic
        .strip_prefix("unified-hifi/media_player/")
        .and_then(|rest| rest.strip_suffix("/state"))
        .expect("state topic shape")
        .to_string();

    let (command_client, mut command_loop) = connect_test_client(port, "test-lms-commander").await;
    tokio::spawn(async move {
        loop {
            if command_loop.poll().await.is_err() {
                break;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    mock.clear_commands().await;
    command_client
        .publish(
            format!("unified-hifi/media_player/{slug}/play/set"),
            QoS::AtLeastOnce,
            false,
            "PRESS",
        )
        .await
        .expect("publish play command for LMS zone");

    let mut played = false;
    let started = tokio::time::Instant::now();
    while started.elapsed() < Duration::from_secs(10) {
        if mock
            .write_commands(PLAYER_ID)
            .await
            .contains(&vec!["play".to_string()])
        {
            played = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        played,
        "MQTT play command for a legacy LMS zone must reach the mock server through the \
         reliable command gateway"
    );

    publisher.shutdown().await;
    lms.stop().await;
    mock.stop().await;
}

/// Covers the knob-specific acceptance criteria of #523 against the same
/// in-process broker: discovery grouped under one device keyed by knob id
/// (not display name), retained battery/zone state, a zone-select command
/// round trip through `KnobStore::update_config`, and discovery retraction
/// on knob removal.
#[tokio::test(flavor = "multi_thread")]
async fn knob_discovery_state_and_zone_select_round_trip_over_a_real_broker() {
    let port = free_port();
    start_test_broker(port);
    wait_for_broker_ready(port, Duration::from_secs(5)).await;

    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let aggregator_task = aggregator.clone();
    tokio::spawn(async move {
        aggregator_task.run_with_ready(ready_tx).await;
    });
    ready_rx.await.expect("aggregator ready");

    let adapter_registry = Arc::new(AdapterRegistry::default());
    let knobs = isolated_knob_store();

    let knob_id = "knobtest1";
    knobs.get_or_create(knob_id, Some("1.2.3")).await;
    knobs
        .update_status(
            knob_id,
            KnobStatusUpdate {
                battery_level: Some(77),
                battery_charging: Some(false),
                zone_id: Some("roon:living".to_string()),
                ip: Some("10.0.0.5".to_string()),
            },
        )
        .await;

    // A zone must exist for the zone-select's options to be non-empty.
    bus.publish(BusEvent::ZoneDiscovered {
        zone: zone_fixture("roon:living", "roon"),
    });

    let publisher = Arc::new(MqttPublisher::new(
        bus.clone(),
        aggregator.clone(),
        adapter_registry,
        knobs.clone(),
    ));
    publisher.set_base_url("http://uhc.test:8088".to_string());
    publisher
        .configure(MqttCredentialRecord {
            host: "127.0.0.1".to_string(),
            port,
            tls: false,
            username: None,
            password: None,
            base_topic: "unified-hifi".to_string(),
            discovery_prefix: "homeassistant".to_string(),
        })
        .await;
    publisher.set_enabled(true).await;
    assert!(publisher.is_running().await);

    let (observer_client, mut observer_loop) = connect_test_client(port, "knob-observer").await;
    subscribe_and_wait(&observer_client, &mut observer_loop, "homeassistant/#").await;
    subscribe_and_wait(&observer_client, &mut observer_loop, "unified-hifi/#").await;

    // --- Discovery: battery sensor is grouped under a device keyed by
    //     knob id, not the knob's display name (which is empty here). ---
    let battery_discovery =
        wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
            publish.topic.contains("/sensor/") && publish.topic.ends_with("_battery/config")
        })
        .await
        .expect("battery sensor discovery config published");
    let payload: serde_json::Value =
        serde_json::from_slice(&battery_discovery.payload).expect("discovery payload is JSON");
    assert_eq!(
        payload["device"]["identifiers"][0],
        serde_json::json!(format!("uhc_knob_{knob_id}"))
    );
    assert_eq!(payload["device_class"], serde_json::json!("battery"));

    // --- Discovery: the zone-select's options track the live zone list. ---
    let select_discovery =
        wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
            publish.topic.contains("/select/") && publish.topic.ends_with("_zone_select/config")
        })
        .await
        .expect("zone select discovery config published");
    let select_payload: serde_json::Value =
        serde_json::from_slice(&select_discovery.payload).expect("select payload is JSON");
    assert_eq!(
        select_payload["options"],
        serde_json::json!(["roon:living"])
    );

    // --- State: retained payload reflects battery/charging/zone/firmware. ---
    let state = wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
        publish.topic == format!("unified-hifi/knob/{knob_id}/state")
    })
    .await
    .expect("knob state topic published");
    let state_payload: serde_json::Value =
        serde_json::from_slice(&state.payload).expect("state payload is JSON");
    assert_eq!(state_payload["battery_level"], serde_json::json!(77));
    assert_eq!(state_payload["battery_charging"], serde_json::json!(false));
    assert_eq!(state_payload["zone_id"], serde_json::json!("roon:living"));
    assert_eq!(state_payload["online"], serde_json::json!(true));
    assert_eq!(
        state_payload["firmware_version"],
        serde_json::json!("1.2.3")
    );

    // --- Command: HA -> UHC zone-select command routes through the same
    //     `KnobStore::update_config` path the web UI uses. ---
    let command_client_id = "knob-commander";
    let (command_client, mut command_loop) = connect_test_client(port, command_client_id).await;
    tokio::spawn(async move {
        loop {
            if command_loop.poll().await.is_err() {
                break;
            }
        }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;

    command_client
        .publish(
            format!("unified-hifi/knob/{knob_id}/zone/set"),
            QoS::AtLeastOnce,
            false,
            "roon:living",
        )
        .await
        .expect("publish zone-select command");

    let reassigned = timeout(Duration::from_secs(5), async {
        loop {
            let assigned = knobs
                .get(knob_id)
                .await
                .and_then(|k| k.config.assigned_zone_id);
            if assigned.as_deref() == Some("roon:living") {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(
        reassigned.is_ok(),
        "zone-select command should update the knob's assigned_zone_id via update_config"
    );

    // The next state publish should reflect the reassignment.
    let reassigned_state =
        wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
            publish.topic == format!("unified-hifi/knob/{knob_id}/state")
                && serde_json::from_slice::<serde_json::Value>(&publish.payload)
                    .ok()
                    .and_then(|v| v.get("assigned_zone_id").cloned())
                    == Some(serde_json::json!("roon:living"))
        })
        .await
        .expect("state republished with the new zone assignment");
    let _ = reassigned_state;

    // --- Retraction: removing the knob clears its retained discovery config. ---
    assert!(knobs.remove(knob_id).await, "knob should have existed");
    let retraction = wait_for_publish(&mut observer_loop, Duration::from_secs(15), |publish| {
        publish.topic.contains(knob_id)
            && publish.topic.contains("/sensor/")
            && publish.topic.ends_with("_battery/config")
            && publish.payload.is_empty()
    })
    .await
    .expect("knob discovery retraction observed");
    assert!(
        retraction.payload.is_empty(),
        "retraction must publish an empty retained payload"
    );

    publisher.shutdown().await;
    assert!(!publisher.is_running().await);
}
