//! Device control safety and behaviour (#774, alpha) — end to end through the real surfaces.
//!
//! Every test drives the code a client hits: `/knob/control` (knob, Watch, Home Assistant),
//! `hifi_control` and `hifi_devices` (MCP), and the aggregator's zone projection. Receivers are
//! `MockMarantz` instances shaped like the SR6008 captures; **two of them share one model**, so a
//! test that routes by model, discovery order or "the only Marantz" fails here.
//!
//! Tests named `critical_*` guard a physical failure mode (docs/AGENTS.md hard constraint 1): a
//! level sent to the wrong amplifier, a level above the user's ceiling, a level sent on connect.
//! Each says which tempting shortcut it is built to fail.

mod mock_servers;

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode};
use mock_servers::marantz::{MockMarantz, MockMarantzState};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::denonavr::protocol::Description;
use unified_hifi_control::adapters::denonavr::DenonAvrAdapter;
use unified_hifi_control::adapters::devices::DeviceFamily;
use unified_hifi_control::adapters::hqplayer::{HqpInstanceManager, HqpZoneLinkService};
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::openhome::OpenHomeAdapter;
use unified_hifi_control::adapters::roon::RoonAdapter;
use unified_hifi_control::adapters::upnp::UPnPAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::api::AppState;
use unified_hifi_control::bus::{
    create_bus, BusEvent, PlaybackState, SharedBus, VolumeScale, Zone,
};
use unified_hifi_control::coordinator::AdapterCoordinator;
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::DeviceService;
use unified_hifi_control::devices::{DeviceId, DevicePower};
use unified_hifi_control::knobs::routes::{knob_control_handler, KnobControlRequest};
use unified_hifi_control::knobs::KnobStore;
use unified_hifi_control::mcp::tools::devices::{handle_devices, HifiDevicesTool};
use unified_hifi_control::mcp::tools::transport::{handle_control, HifiControlTool};

const FRONT: &str = "0006781E0AB8";
const BEDROOM: &str = "0006781E2AB9";

struct Harness {
    state: AppState,
    bus: SharedBus,
    adapter: Arc<DenonAvrAdapter>,
    _dir: tempfile::TempDir,
}

fn description(serial: &str, name: &str) -> Description {
    Description {
        manufacturer: "Marantz".to_string(),
        model: "SR6008".to_string(),
        friendly_name: name.to_string(),
        serial: Some(serial.to_string()),
        udn: None,
    }
}

fn id(serial: &str) -> DeviceId {
    DeviceId::new("marantz", serial).unwrap()
}

async fn harness() -> Harness {
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run_with_ready(ready_tx).await });
    }
    ready_rx.await.unwrap();

    let coordinator = Arc::new(AdapterCoordinator::new(bus.clone()));
    let roon = Arc::new(RoonAdapter::new_disconnected(bus.clone()));
    let hqp_instances = Arc::new(HqpInstanceManager::new_with_native_sink(
        bus.clone(),
        aggregator.clone(),
    ));
    let hqplayer = hqp_instances.get_default().await;
    let hqp_zone_links = Arc::new(HqpZoneLinkService::new(hqp_instances.clone()));
    let lms = Arc::new(LmsAdapter::new(bus.clone()));
    let mut state = AppState::new(
        roon,
        hqplayer,
        hqp_instances,
        hqp_zone_links,
        lms,
        Arc::new(OpenHomeAdapter::new(bus.clone())),
        Arc::new(UPnPAdapter::new(bus.clone())),
        KnobStore::new(),
        bus.clone(),
        aggregator.clone(),
        coordinator,
        Vec::new(),
        tokio::time::Instant::now().into_std(),
        CancellationToken::new(),
    );
    let dir = tempfile::tempdir().unwrap();
    let devices = Arc::new(
        DeviceService::new(
            aggregator,
            bus.clone(),
            DeviceLinkStore::open(dir.path().join("device-links.json")),
        )
        .with_confirm_timeout(Duration::from_millis(1500)),
    );
    let adapter = Arc::new(DenonAvrAdapter::new(bus.clone()));
    devices.register_driver(adapter.clone()).await;
    state.devices = devices;
    Harness {
        state,
        bus,
        adapter,
        _dir: dir,
    }
}

impl Harness {
    async fn attach(&self, serial: &str, name: &str, mock: &MockMarantz) {
        self.adapter
            .attach_unit(
                &id(serial),
                "127.0.0.1",
                mock.ports(),
                description(serial, name),
            )
            .await;
        // Ready means the push session reported a level *and* the unit's maximum.
        self.wait_for(|state| {
            let serial = serial.to_string();
            async move {
                state
                    .aggregator
                    .get_device(&id(&serial))
                    .await
                    .is_some_and(|device| device.volume_control().is_some())
            }
        })
        .await;
    }

    /// Publish a provider zone that has **no volume of its own**, like a fixed-output streamer.
    fn publish_zone(&self, zone_id: &str) {
        self.bus.publish(BusEvent::ZoneDiscovered {
            zone: Zone {
                zone_id: zone_id.to_string(),
                zone_name: zone_id.to_string(),
                state: PlaybackState::Playing,
                volume_control: None,
                now_playing: None,
                source: zone_id.split(':').next().unwrap().to_string(),
                is_controllable: true,
                is_seekable: false,
                last_updated: 0,
                is_play_allowed: false,
                is_pause_allowed: true,
                is_next_allowed: true,
                is_previous_allowed: true,
            },
        });
    }

    async fn zone_ready(&self, zone_id: &str) {
        self.publish_zone(zone_id);
        let zone_id = zone_id.to_string();
        self.wait_for(|state| {
            let zone_id = zone_id.clone();
            async move { state.aggregator.get_zone(&zone_id).await.is_some() }
        })
        .await;
    }

    async fn link(&self, zone_id: &str, serial: &str) {
        self.state
            .devices
            .link(zone_id, id(serial).as_str(), None)
            .await
            .expect("link");
    }

    async fn knob(
        &self,
        zone_id: &str,
        action: &str,
        value: Option<Value>,
    ) -> Result<Value, (StatusCode, Value)> {
        knob_control_handler(
            State(self.state.clone()),
            HeaderMap::new(),
            Json(KnobControlRequest {
                zone_id: zone_id.to_string(),
                action: action.to_string(),
                value,
            }),
        )
        .await
        .map(|Json(body)| body)
        .map_err(|(status, Json(body))| (status, body))
    }

    async fn wait_for<F, Fut>(&self, condition: F)
    where
        F: Fn(AppState) -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition(self.state.clone()).await {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within 5s");
    }
}

/// The highest absolute `MV` value a mock was ever asked to set.
fn highest_level_written(state: &MockMarantzState) -> Option<f32> {
    state
        .writes()
        .iter()
        .filter_map(|line| line.strip_prefix("MV"))
        .filter(|digits| digits.bytes().all(|b| b.is_ascii_digit()))
        .map(|digits| {
            let whole: f32 = digits[..2].parse().unwrap();
            let half = if digits.len() == 3 { 0.5 } else { 0.0 };
            whole + half - 80.0
        })
        .reduce(f32::max)
}

// =============================================================================
// Projection: device volume reaches unmodified clients through `volume_control`
// =============================================================================

/// The tempting patch this fails: writing the device's volume into the stored provider zone.
/// The next provider snapshot replaces the whole zone and erases it.
#[tokio::test]
async fn linked_zone_publishes_device_volume_and_survives_a_provider_resnapshot() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    assert_eq!(
        h.state.aggregator.has_volume_control("roon:living").await,
        Some(false)
    );

    h.link("roon:living", FRONT).await;
    let zone = h.state.aggregator.get_zone("roon:living").await.unwrap();
    let volume = zone
        .volume_control
        .expect("linked zone publishes the device volume");
    assert_eq!(volume.value, -39.5);
    assert_eq!(volume.min, -80.0);
    assert_eq!(volume.max, -10.0, "the unit's own MVMAX");
    assert_eq!(volume.step, 0.5);
    assert_eq!(volume.scale, VolumeScale::Decibel);
    assert_eq!(
        volume.output_id.as_deref(),
        Some("marantz:0006781E0AB8:main")
    );
    assert_eq!(
        h.state.aggregator.has_volume_control("roon:living").await,
        Some(true)
    );

    // The provider republishes its zone, still with no volume of its own.
    h.publish_zone("roon:living");
    tokio::time::sleep(Duration::from_millis(100)).await;
    let zone = h.state.aggregator.get_zone("roon:living").await.unwrap();
    assert_eq!(zone.volume_control.map(|v| v.value), Some(-39.5));

    // Unlinking restores the provider's own (absent) volume.
    h.state.devices.unlink("roon:living").await.unwrap();
    let zone = h.state.aggregator.get_zone("roon:living").await.unwrap();
    assert!(zone.volume_control.is_none());
}

#[tokio::test]
async fn two_zones_linked_to_one_amplifier_agree_on_volume() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.zone_ready("lms:kitchen").await;
    h.link("roon:living", FRONT).await;
    h.link("lms:kitchen", FRONT).await;

    h.knob("roon:living", "vol_abs", Some(json!(-35.0)))
        .await
        .expect("confirmed");

    for zone_id in ["roon:living", "lms:kitchen"] {
        let zone = h.state.aggregator.get_zone(zone_id).await.unwrap();
        assert_eq!(
            zone.volume_control.map(|v| v.value),
            Some(-35.0),
            "{zone_id}"
        );
    }
}

/// The aim's feedback signal: a change made with the receiver's own remote reaches UHC within
/// about two seconds. Measured over repeated pushes against the mock; the hardware number is
/// recorded separately once measured on the units.
#[tokio::test]
async fn remote_change_reaches_linked_zones_within_two_seconds() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;

    let mut worst = Duration::ZERO;
    for step in 0..10 {
        let level = -50.0 + step as f32;
        let started = Instant::now();
        front.remote_set_level(level).await;
        h.wait_for(|state| async move {
            state
                .aggregator
                .get_zone("roon:living")
                .await
                .and_then(|zone| zone.volume_control)
                .is_some_and(|volume| volume.value == level)
        })
        .await;
        worst = worst.max(started.elapsed());
    }
    assert!(
        worst < Duration::from_secs(2),
        "worst remote-change latency {worst:?}"
    );
}

#[tokio::test]
async fn linked_device_changes_are_announced_for_every_linked_zone() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.zone_ready("lms:kitchen").await;
    h.link("roon:living", FRONT).await;
    h.link("lms:kitchen", FRONT).await;

    let mut events = h.bus.subscribe();
    front.remote_set_level(-45.0).await;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let event = tokio::time::timeout_at(deadline, events.recv())
            .await
            .expect("a LinkedDeviceChanged event")
            .unwrap();
        if let BusEvent::LinkedDeviceChanged {
            zone_ids,
            volume_control,
            ..
        } = event
        {
            assert_eq!(
                zone_ids,
                vec!["lms:kitchen".to_string(), "roon:living".to_string()]
            );
            assert_eq!(volume_control.map(|v| v.value), Some(-45.0));
            break;
        }
    }
}

// =============================================================================
// Safety
// =============================================================================

/// CRITICAL: identical models must never be confused. Fails any routing that picks "the
/// Marantz" by model, by discovery order, or by the only unit currently on.
#[tokio::test]
async fn critical_zone_a_volume_never_reaches_amp_b() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    let bedroom = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.attach(BEDROOM, "Bedroom", &bedroom).await;
    h.zone_ready("roon:living").await;
    h.zone_ready("roon:bedroom").await;
    h.link("roon:living", FRONT).await;
    h.link("roon:bedroom", BEDROOM).await;

    h.knob("roon:living", "vol_abs", Some(json!(-30.0)))
        .await
        .expect("confirmed");
    h.knob("roon:living", "mute", Some(json!(true)))
        .await
        .expect("confirmed");

    let front_state = front.snapshot().await;
    let bedroom_state = bedroom.snapshot().await;
    assert_eq!(
        front_state.writes(),
        vec!["MV50".to_string(), "MUON".to_string()]
    );
    assert!(
        bedroom_state.writes().is_empty(),
        "amp B received {:?}",
        bedroom_state.writes()
    );
    assert_eq!(bedroom_state.level_db(), -39.5);
    assert!(!bedroom_state.muted);
}

/// CRITICAL: connecting (and reconnecting) only *asks*. A level sent on connect would replay
/// whatever UHC last cached onto an amplifier someone has since turned down.
#[tokio::test]
async fn critical_no_level_sent_on_connect_or_reconnect() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.adapter.stop_family().await;
    h.attach(FRONT, "front", &front).await;

    let state = front.snapshot().await;
    assert!(
        state.writes().is_empty(),
        "connect sent state-changing lines: {:?}",
        state.writes()
    );
    assert!(state.telnet_received.iter().any(|line| line == "MV?"));
}

/// CRITICAL (#246, O1): the user ceiling bounds the level from *every* surface. Fails the patch
/// that clamps only in the knob handler and lets MCP through.
#[tokio::test]
async fn critical_ceiling_enforced_via_knob_and_mcp() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;

    let result = handle_devices(
        &h.state,
        HifiDevicesTool {
            action: "set_ceiling".to_string(),
            device_id: Some(id(FRONT).to_string()),
            zone_id: None,
            source_id: None,
            command: None,
            value: None,
            input: None,
            muted: None,
            ceiling: Some(-30.0),
        },
    )
    .await
    .unwrap();
    assert_ne!(result.is_error, Some(true));

    // Published maximum includes the ceiling: an unmodified client cannot even ask for more.
    let zone = h.state.aggregator.get_zone("roon:living").await.unwrap();
    assert_eq!(zone.volume_control.as_ref().map(|v| v.max), Some(-30.0));

    // Knob: absolute far above, and a relative climb.
    h.knob("roon:living", "vol_abs", Some(json!(10.0)))
        .await
        .expect("confirmed");
    h.knob("roon:living", "vol_up", Some(json!(6.0)))
        .await
        .expect("confirmed");
    // MCP: same zone, through hifi_control.
    let mcp = handle_control(
        &h.state,
        HifiControlTool {
            zone_id: "roon:living".to_string(),
            action: "volume_set".to_string(),
            value: Some(0.0),
        },
    )
    .await
    .unwrap();
    assert_ne!(mcp.is_error, Some(true), "{mcp:?}");

    let highest = highest_level_written(&front.snapshot().await).expect("levels were sent");
    assert!(
        highest <= -30.0,
        "a level above the ceiling reached the amplifier: {highest}"
    );
}

/// #621 on the device path: writing the level the amplifier already holds must confirm
/// promptly — against a fresh device readback, not the cache. Fails the short-circuit
/// "requested == cached, return ok" (which skips the device) and the naive "wait for a change"
/// (which hangs, because nothing changes).
#[tokio::test]
async fn critical_noop_write_confirms_without_timeout() {
    let h = harness().await;
    let front = MockMarantz::start_with(MockMarantzState {
        echo_noop_writes: false,
        ..MockMarantzState::default()
    })
    .await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;

    let started = Instant::now();
    let body = h
        .knob("roon:living", "vol_abs", Some(json!(-39.5)))
        .await
        .expect("a no-op write is confirmed by the readback query");
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(body["result"]["confirmation"], "confirmed");
    // The device was actually asked, not answered from cache.
    let received = front.snapshot().await.telnet_received;
    assert!(
        received.ends_with(&["MV405".to_string(), "MV?".to_string()]),
        "{received:?}"
    );
}

/// Confirmed ≠ sent: a write the device never answers is an error, never `ok`.
#[tokio::test]
async fn unacknowledged_write_is_reported_unconfirmed_not_success() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;
    front.state.lock().await.answers = false;

    let (status, body) = h
        .knob("roon:living", "vol_abs", Some(json!(-30.0)))
        .await
        .expect_err("an unanswered write is not a success");
    assert_eq!(status, StatusCode::GATEWAY_TIMEOUT);
    assert_eq!(body["error_code"], "DEVICE_UNCONFIRMED");
}

/// CRITICAL: a link to a device that is not observed refuses every level. Fails both the
/// fallback to the provider's own volume and a fallback to "the other Marantz".
#[tokio::test]
async fn critical_link_to_absent_device_refuses_writes() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:bedroom").await;
    h.link("roon:bedroom", BEDROOM).await; // linked, but that unit is not on the network

    let (status, body) = h
        .knob("roon:bedroom", "vol_abs", Some(json!(-30.0)))
        .await
        .expect_err("no device, no level");
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error_code"], "DEVICE_UNAVAILABLE");
    assert!(front.snapshot().await.writes().is_empty());
    let zone = h.state.aggregator.get_zone("roon:bedroom").await.unwrap();
    assert!(
        zone.volume_control.is_none(),
        "an absent device publishes no range"
    );
}

/// CRITICAL: a device whose maximum is unknown gets no level, even if its current level is
/// known (the SR6008 over HTTP alone).
#[tokio::test]
async fn critical_missing_level_value_is_refused_not_defaulted() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;

    let (status, body) = h
        .knob("roon:living", "vol_abs", None)
        .await
        .expect_err("vol_abs without a value has no safe meaning on a dB device");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], "INVALID_VALUE");
    assert!(front.snapshot().await.writes().is_empty());
}

// =============================================================================
// Power and input (manual only, decision D2)
// =============================================================================

#[tokio::test]
async fn power_and_input_round_trip_through_the_knob_path() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;

    let body = h
        .knob("roon:living", "select_input", Some(json!("TV AUDIO")))
        .await
        .expect("confirmed");
    assert_eq!(body["result"]["observed"]["input"], "TV");
    assert_eq!(front.snapshot().await.input, "TV");

    h.knob("roon:living", "power_off", None)
        .await
        .expect("confirmed");
    let summary = h
        .state
        .aggregator
        .linked_device("roon:living")
        .await
        .unwrap();
    assert_eq!(summary.power, DevicePower::Standby);
    // Power never carries a level.
    assert!(highest_level_written(&front.snapshot().await).is_none());

    h.knob("roon:living", "power_on", None)
        .await
        .expect("confirmed");
    assert!(front.snapshot().await.zone_on);
}

#[tokio::test]
async fn power_and_input_on_an_unlinked_zone_are_refused_not_sent_to_the_provider() {
    let h = harness().await;
    h.zone_ready("lms:study").await;
    for action in ["power_on", "power_off", "select_input"] {
        let (status, body) = h
            .knob("lms:study", action, Some(json!("CD")))
            .await
            .expect_err("no device link");
        assert_eq!(status, StatusCode::BAD_REQUEST, "{action}");
        assert_eq!(body["error_code"], "DEVICE_NOT_LINKED", "{action}");
    }
}

#[tokio::test]
async fn unknown_inputs_are_refused_with_the_catalog() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;
    let (status, body) = h
        .knob("roon:living", "select_input", Some(json!("PHONO")))
        .await
        .expect_err("PHONO is not on this unit");
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(body["error_code"], "DEVICE_INPUT_UNKNOWN");
    assert!(body["accepted"]
        .as_array()
        .unwrap()
        .contains(&json!("MPLAY")));
}

// =============================================================================
// MCP
// =============================================================================

#[tokio::test]
async fn hifi_devices_lists_devices_and_links_as_alpha() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;

    let link = handle_devices(
        &h.state,
        HifiDevicesTool {
            action: "link".to_string(),
            device_id: Some(id(FRONT).to_string()),
            zone_id: Some("roon:living".to_string()),
            source_id: Some("MPLAY".to_string()),
            command: None,
            value: None,
            input: None,
            muted: None,
            ceiling: None,
        },
    )
    .await
    .unwrap();
    assert_ne!(link.is_error, Some(true), "{link:?}");

    let list = handle_devices(
        &h.state,
        HifiDevicesTool {
            action: "list".to_string(),
            device_id: None,
            zone_id: None,
            source_id: None,
            command: None,
            value: None,
            input: None,
            muted: None,
            ceiling: None,
        },
    )
    .await
    .unwrap();
    let data = list.structured_content.expect("envelope")["data"].clone();
    assert_eq!(data["maturity"], "alpha");
    let device = &data["devices"][0];
    assert_eq!(device["device_id"], "marantz:0006781E0AB8");
    assert_eq!(device["maturity"], "alpha");
    assert_eq!(device["linked_zone_ids"], json!(["roon:living"]));
    assert_eq!(
        device["capabilities"]["observe"]["evidence"],
        "hardware-verified"
    );
    assert_eq!(
        device["capabilities"]["volume"]["evidence"],
        "implementation-derived"
    );
    assert_eq!(data["links"][0]["source_id"], "MPLAY");
}

/// `hifi_control volume_up` with no value on a linked zone steps by the device's 0.5 dB, not the
/// provider default of 5 — which on an amplifier is a loud surprise.
#[tokio::test]
async fn hifi_control_volume_on_a_linked_zone_steps_by_the_device_step() {
    let h = harness().await;
    let front = MockMarantz::start().await;
    h.attach(FRONT, "front", &front).await;
    h.zone_ready("roon:living").await;
    h.link("roon:living", FRONT).await;

    let result = handle_control(
        &h.state,
        HifiControlTool {
            zone_id: "roon:living".to_string(),
            action: "volume_up".to_string(),
            value: None,
        },
    )
    .await
    .unwrap();
    assert_ne!(result.is_error, Some(true), "{result:?}");
    assert_eq!(front.snapshot().await.level_db(), -39.0);
}
