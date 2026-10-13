//! Yamaha MusicCast device family, end to end through `DeviceService` (#774, alpha).
//!
//! The device is `MockMusicCast`, an in-file **synthetic** YXC server shaped from the
//! aiomusiccast / YXC-spec response examples (see `src/adapters/musiccast/protocol.rs`). It is
//! not hardware evidence: no MusicCast device has been exercised.
//!
//! Tests named `critical_*` guard a physical failure mode: a level sent to the wrong amplifier,
//! a level above the user's ceiling, a state-changing request sent on connect.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::{StatusCode, Uri};
use axum::Router;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use unified_hifi_control::adapters::devices::DeviceFamily;
use unified_hifi_control::adapters::musiccast::MusicCastAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{
    create_bus, BusEvent, PlaybackState, SharedBus, VolumeScale, Zone,
};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{CapabilityEvidence, ControlChannel, DeviceId, DevicePower};

// =============================================================================
// Synthetic MusicCast device
// =============================================================================

struct MockState {
    device_id: String,
    model: String,
    functions: Vec<&'static str>,
    range_max: i64,
    inputs: Vec<(&'static str, &'static str)>,
    power: &'static str,
    volume: i64,
    max_volume: i64,
    mute: bool,
    input: String,
    offline: bool,
    requests: Vec<String>,
}

impl MockState {
    fn new(device_id: &str) -> Self {
        Self {
            device_id: device_id.to_string(),
            model: "RX-V481".to_string(),
            functions: vec!["power", "sleep", "volume", "mute"],
            range_max: 161,
            inputs: vec![("hdmi1", "TV"), ("hdmi2", ""), ("net_radio", "NET RADIO")],
            power: "on",
            volume: 80,
            max_volume: 161,
            mute: false,
            input: "hdmi1".to_string(),
            offline: false,
            requests: Vec::new(),
        }
    }
}

#[derive(Clone)]
struct MockMusicCast {
    state: Arc<Mutex<MockState>>,
    port: u16,
}

impl MockMusicCast {
    async fn start(configure: impl FnOnce(&mut MockState)) -> Self {
        let mut initial = MockState::new("00A0DEAD07DA");
        configure(&mut initial);
        let state = Arc::new(Mutex::new(initial));
        let app = Router::new().fallback(handle).with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self { state, port }
    }

    fn with<R>(&self, f: impl FnOnce(&mut MockState) -> R) -> R {
        f(&mut self.state.lock().unwrap())
    }

    /// Every request that is not a `get*` query: what a safety test inspects.
    fn writes(&self) -> Vec<String> {
        self.with(|s| {
            s.requests
                .iter()
                .filter(|path| {
                    !path
                        .split('?')
                        .next()
                        .unwrap_or("")
                        .rsplit('/')
                        .next()
                        .unwrap_or("")
                        .starts_with("get")
                })
                .cloned()
                .collect()
        })
    }
}

fn query(uri: &Uri, key: &str) -> Option<String> {
    url::form_urlencoded::parse(uri.query().unwrap_or("").as_bytes())
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.to_string())
}

async fn handle(State(state): State<Arc<Mutex<MockState>>>, uri: Uri) -> (StatusCode, String) {
    let mut s = state.lock().unwrap();
    s.requests.push(
        uri.path_and_query()
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
    );
    if s.offline {
        return (StatusCode::SERVICE_UNAVAILABLE, String::new());
    }
    let Some(route) = uri.path().strip_prefix("/YamahaExtendedControl/v1/") else {
        return (StatusCode::NOT_FOUND, "not found".to_string());
    };
    let ok = json!({"response_code": 0});
    let body: Value = match route {
        "system/getDeviceInfo" => json!({
            "response_code": 0, "model_name": s.model, "destination": "BG",
            "device_id": s.device_id, "system_id": "0A3F5B2C", "system_version": 2.81,
            "api_version": 1.17, "netmodule_generation": 1, "operation_mode": "normal"
        }),
        "system/getFeatures" => json!({
            "response_code": 0,
            "system": {"func_list": ["wired_lan"], "zone_num": 1, "input_list": []},
            "zone": [{
                "id": "main", "func_list": s.functions,
                "input_list": s.inputs.iter().map(|(id, _)| id).collect::<Vec<_>>(),
                "range_step": [
                    {"id": "volume", "min": 0, "max": s.range_max, "step": 1},
                    {"id": "subwoofer_volume", "min": -12, "max": 12, "step": 1}],
                "actual_volume_mode_list": ["db", "numeric"]
            }]
        }),
        "system/getNameText" => json!({
            "response_code": 0,
            "zone_list": [{"id": "main", "text": "Living Room"}],
            "input_list": s.inputs.iter().map(|(id, text)| json!({"id": id, "text": text})).collect::<Vec<_>>()
        }),
        "system/getNetworkStatus" => json!({"response_code": 0, "network_name": "Living Room"}),
        "main/getStatus" => json!({
            "response_code": 0, "power": s.power, "sleep": 0, "volume": s.volume,
            "mute": s.mute, "max_volume": s.max_volume, "input": s.input,
            "actual_volume": {"mode": "db", "value": (s.volume as f64 - 161.0) / 2.0, "unit": "dB"}
        }),
        "main/setVolume" => match query(&uri, "volume").and_then(|v| v.parse::<i64>().ok()) {
            Some(v) if (0..=s.max_volume).contains(&v) => {
                s.volume = v;
                ok
            }
            _ => json!({"response_code": 4}),
        },
        "main/setMute" => {
            s.mute = query(&uri, "enable").as_deref() == Some("true");
            ok
        }
        "main/setPower" => {
            s.power = if query(&uri, "power").as_deref() == Some("on") {
                "on"
            } else {
                "standby"
            };
            ok
        }
        "main/setInput" => match query(&uri, "input") {
            Some(input) if s.inputs.iter().any(|(id, _)| *id == input) => {
                s.input = input;
                ok
            }
            _ => json!({"response_code": 4}),
        },
        _ => return (StatusCode::NOT_FOUND, "not found".to_string()),
    };
    (StatusCode::OK, body.to_string())
}

// =============================================================================
// Harness
// =============================================================================

struct Harness {
    bus: SharedBus,
    aggregator: Arc<ZoneAggregator>,
    devices: Arc<DeviceService>,
    adapter: Arc<MusicCastAdapter>,
    _dir: tempfile::TempDir,
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
    let dir = tempfile::tempdir().unwrap();
    let devices = Arc::new(
        DeviceService::new(
            aggregator.clone(),
            bus.clone(),
            DeviceLinkStore::open(dir.path().join("device-links.json")),
        )
        .with_confirm_timeout(Duration::from_millis(3000)),
    );
    let adapter = Arc::new(MusicCastAdapter::new(bus.clone()));
    devices.register_driver(adapter.clone()).await;
    Harness {
        bus,
        aggregator,
        devices,
        adapter,
        _dir: dir,
    }
}

impl Harness {
    async fn attach(&self, mock: &MockMusicCast) -> DeviceId {
        let id = self
            .adapter
            .attach_unit("127.0.0.1", mock.port)
            .await
            .expect("the mock answers the YXC probe");
        self.wait_device(&id).await;
        id
    }

    async fn wait_device(&self, id: &DeviceId) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if self.aggregator.get_device(id).await.is_some() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("{id} never observed");
    }

    async fn control(
        &self,
        id: &DeviceId,
        request: DeviceRequest,
    ) -> Result<unified_hifi_control::devices::service::DeviceCommandReport, DeviceError> {
        self.devices
            .control(DeviceTarget::Device(id.clone()), request)
            .await
    }
}

fn yamaha(hardware_id: &str) -> DeviceId {
    DeviceId::new("yamaha", hardware_id).unwrap()
}

// =============================================================================
// Observation
// =============================================================================

#[tokio::test]
async fn observed_with_hardware_identity_and_native_volume_scale() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    let id = h.attach(&mock).await;
    assert_eq!(id, yamaha("00A0DEAD07DA"));
    assert_eq!(id.as_str(), "yamaha:00A0DEAD07DA");

    let device = h.aggregator.get_device(&id).await.unwrap();
    assert_eq!(device.vendor, "Yamaha");
    assert_eq!(device.model, "RX-V481");
    assert_eq!(device.name, "Living Room");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.connection.control_channel, ControlChannel::Polling);
    assert_eq!(device.input.as_deref(), Some("hdmi1"));
    assert_eq!(device.input_name(), Some("TV"));
    let names: Vec<_> = device.inputs.iter().map(|i| i.name.as_str()).collect();
    assert_eq!(names, ["TV", "hdmi2", "NET RADIO"]);

    // 0-161 is the model's own scale: not a percentage, never relabelled as dB.
    assert_eq!(device.volume.scale, VolumeScale::Steps);
    assert_eq!(device.volume.value, Some(80.0));
    assert_eq!(device.volume.min, 0.0);
    assert_eq!(device.volume.hardware_max, Some(161.0));
    assert_eq!(device.volume.step, 1.0);
    assert_eq!(device.volume.is_muted, Some(false));
    // The model's dB readout is carried for display only.
    assert_eq!(device.volume.display.as_deref(), Some("-40.5 dB"));
    assert!(device.volume_control().is_some());

    let c = &device.capabilities;
    for claim in [&c.observe, &c.volume, &c.mute, &c.power, &c.input_select] {
        assert!(claim.supported);
        assert_eq!(claim.evidence, CapabilityEvidence::ImplementationDerived);
    }
}

#[tokio::test]
async fn a_zero_to_hundred_range_is_a_percentage_and_the_unit_limit_is_the_hardware_max() {
    let h = harness().await;
    let mock = MockMusicCast::start(|s| {
        s.range_max = 100;
        s.max_volume = 70;
        s.volume = 30;
    })
    .await;
    let id = h.attach(&mock).await;
    let device = h.aggregator.get_device(&id).await.unwrap();
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(
        device.volume.hardware_max,
        Some(70.0),
        "max_volume from getStatus"
    );
}

#[tokio::test]
async fn a_device_that_does_not_speak_yxc_is_not_accepted() {
    let h = harness().await;
    // Answers HTTP, but has no YXC API (a legacy receiver, or any web server).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(
            listener,
            Router::new().fallback(|| async { "<html>hi</html>" }),
        )
        .await
        .ok();
    });
    assert!(h.adapter.attach_unit("127.0.0.1", port).await.is_err());
    assert_eq!(h.adapter.device_count().await, 0);
}

#[tokio::test]
async fn an_unreachable_unit_reports_unknown_power_never_standby() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    let id = h.attach(&mock).await;
    mock.with(|s| s.offline = true);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let device = h.aggregator.get_device(&id).await.unwrap();
        if device.connection.control_channel == ControlChannel::None {
            assert_eq!(device.power, DevicePower::Unknown);
            break;
        }
        assert!(Instant::now() < deadline, "the outage was never observed");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// =============================================================================
// Safety
// =============================================================================

/// CRITICAL: connecting only *asks*. Every request before the first command is a `get*` query.
#[tokio::test]
async fn critical_no_state_changing_request_on_connect() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    h.attach(&mock).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(mock.writes().is_empty(), "sent {:?}", mock.writes());
    let requests = mock.with(|s| s.requests.clone());
    for expected in ["getDeviceInfo", "getFeatures", "getStatus"] {
        assert!(
            requests.iter().any(|r| r.contains(expected)),
            "{expected} was never queried: {requests:?}"
        );
    }
}

/// CRITICAL: the user's ceiling bounds the level in the model's native units, and the write is
/// confirmed by a fresh readback, not the cache.
#[tokio::test]
async fn critical_ceiling_clamps_a_write_and_readback_confirms_it() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    let id = h.attach(&mock).await;
    h.devices
        .set_ceiling(id.as_str(), Some(100.0))
        .await
        .unwrap();

    let report = h
        .control(&id, DeviceRequest::VolumeAbsolute(150.0))
        .await
        .expect("confirmed");
    assert_eq!(report.sent_volume, Some(100.0));
    assert!(report.clamped);
    assert_eq!(report.observed.volume.value, Some(100.0));
    assert_eq!(mock.with(|s| s.volume), 100);
    assert_eq!(
        mock.writes(),
        ["/YamahaExtendedControl/v1/main/setVolume?volume=100"]
    );

    // A relative climb is resolved from the observed level and stops at the ceiling too.
    h.control(&id, DeviceRequest::VolumeRelative(30.0))
        .await
        .expect("confirmed");
    assert_eq!(mock.with(|s| s.volume), 100);
}

/// CRITICAL: two identical models never cross-route; each is addressed by its own device_id.
#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let a = MockMusicCast::start(|s| s.device_id = "00A0DEAD0001".to_string()).await;
    let b = MockMusicCast::start(|s| s.device_id = "00A0DEAD0002".to_string()).await;
    let id_a = h.attach(&a).await;
    let id_b = h.attach(&b).await;
    assert_ne!(id_a, id_b);

    h.control(&id_a, DeviceRequest::VolumeAbsolute(50.0))
        .await
        .expect("confirmed");
    h.control(&id_a, DeviceRequest::Mute(true))
        .await
        .expect("confirmed");

    assert_eq!(a.with(|s| (s.volume, s.mute)), (50, true));
    assert!(b.writes().is_empty(), "unit B received {:?}", b.writes());
    assert_eq!(b.with(|s| (s.volume, s.mute)), (80, false));
}

/// CRITICAL: an address that now answers as another device (DHCP reuse) never receives a write.
#[tokio::test]
async fn critical_a_reassigned_address_is_refused_before_any_write() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    let id = h.attach(&mock).await;
    mock.with(|s| s.device_id = "00A0DEAD9999".to_string());

    let error = h
        .control(&id, DeviceRequest::VolumeAbsolute(10.0))
        .await
        .expect_err("must refuse");
    assert!(matches!(error, DeviceError::Dispatch { .. }), "{error:?}");
    assert!(
        mock.writes().is_empty(),
        "wrote to the new owner: {:?}",
        mock.writes()
    );
    assert_eq!(mock.with(|s| s.volume), 80);
}

// =============================================================================
// Operations
// =============================================================================

#[tokio::test]
async fn power_mute_and_input_are_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockMusicCast::start(|s| s.power = "standby").await;
    let id = h.attach(&mock).await;
    assert_eq!(
        h.aggregator.get_device(&id).await.unwrap().power,
        DevicePower::Standby
    );

    let report = h
        .control(&id, DeviceRequest::PowerOn)
        .await
        .expect("confirmed");
    assert_eq!(report.observed.power, DevicePower::On);

    let report = h
        .control(&id, DeviceRequest::Mute(true))
        .await
        .expect("confirmed");
    assert_eq!(report.observed.volume.is_muted, Some(true));
    let report = h
        .control(&id, DeviceRequest::MuteToggle)
        .await
        .expect("confirmed");
    assert_eq!(report.observed.volume.is_muted, Some(false));

    // The service resolves a display name to the protocol id.
    let report = h
        .control(&id, DeviceRequest::SelectInput("net radio".into()))
        .await
        .expect("confirmed");
    assert_eq!(report.observed.input.as_deref(), Some("net_radio"));

    let report = h
        .control(&id, DeviceRequest::PowerOff)
        .await
        .expect("confirmed");
    assert_eq!(report.observed.power, DevicePower::Standby);
}

/// Writing the level the unit already holds still confirms (#621): against the device, not the cache.
#[tokio::test]
async fn a_noop_volume_write_confirms_without_timeout() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    let id = h.attach(&mock).await;
    let started = Instant::now();
    h.control(&id, DeviceRequest::VolumeAbsolute(80.0))
        .await
        .expect("confirmed");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[tokio::test]
async fn unsupported_operations_error_and_send_nothing() {
    let h = harness().await;
    // A model whose getFeatures lists neither mute nor volume (a fixed-output zone).
    let mock = MockMusicCast::start(|s| s.functions = vec!["power"]).await;
    let id = h.attach(&mock).await;
    let device = h.aggregator.get_device(&id).await.unwrap();
    assert!(!device.capabilities.volume.supported);
    assert!(!device.capabilities.mute.supported);
    assert_eq!(
        device.volume.value, None,
        "no level is published for a unit with no volume"
    );
    assert!(device.volume_control().is_none());

    // Straight to the driver, as a buggy caller would: it must refuse, not guess.
    for operation in [
        DeviceOperation::SetVolume(40.0),
        DeviceOperation::SetMute(true),
        DeviceOperation::SelectInput("hdmi9".to_string()),
    ] {
        let result = h.adapter.execute(&id, operation.clone()).await;
        assert!(result.is_err(), "{operation:?} must be refused");
    }
    // Through the service: a volume request has no range to bound it.
    assert!(matches!(
        h.control(&id, DeviceRequest::VolumeAbsolute(40.0)).await,
        Err(DeviceError::RangeUnknown { .. })
    ));
    assert!(mock.writes().is_empty(), "sent {:?}", mock.writes());
}

#[tokio::test]
async fn an_input_the_unit_never_reported_is_refused_before_it_is_sent() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    let id = h.attach(&mock).await;
    assert!(h
        .adapter
        .execute(
            &id,
            DeviceOperation::SelectInput("hdmi1&volume=161".to_string())
        )
        .await
        .is_err());
    assert!(matches!(
        h.control(&id, DeviceRequest::SelectInput("phono".into()))
            .await,
        Err(DeviceError::UnknownInput { .. })
    ));
    assert!(mock.writes().is_empty());
}

// =============================================================================
// Projection onto a linked zone
// =============================================================================

#[tokio::test]
async fn a_linked_zone_publishes_the_native_volume_range() {
    let h = harness().await;
    let mock = MockMusicCast::start(|_| {}).await;
    let id = h.attach(&mock).await;
    h.bus.publish(BusEvent::ZoneDiscovered {
        zone: Zone {
            zone_id: "roon:living".to_string(),
            zone_name: "Living".to_string(),
            state: PlaybackState::Playing,
            volume_control: None,
            now_playing: None,
            source: "roon".to_string(),
            is_controllable: true,
            is_seekable: false,
            last_updated: 0,
            is_play_allowed: false,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        },
    });
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.aggregator.get_zone("roon:living").await.is_none() {
        assert!(Instant::now() < deadline);
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    h.devices
        .link("roon:living", id.as_str(), None)
        .await
        .expect("link");
    let volume = h
        .aggregator
        .get_zone("roon:living")
        .await
        .unwrap()
        .volume_control
        .expect("linked zone publishes the device volume");
    assert_eq!(
        (volume.value, volume.min, volume.max, volume.step),
        (80.0, 0.0, 161.0, 1.0)
    );
    assert_eq!(volume.scale, VolumeScale::Steps);
    assert_eq!(
        volume.output_id.as_deref(),
        Some("yamaha:00A0DEAD07DA:main")
    );
}

#[tokio::test]
async fn the_family_is_named_musiccast_and_serves_the_yamaha_kind() {
    let adapter = MusicCastAdapter::new(create_bus());
    assert_eq!(adapter.family(), "musiccast");
    assert_eq!(adapter.kinds(), &["yamaha"]);
}
