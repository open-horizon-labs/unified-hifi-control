//! Eversolo device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The mock below is **synthetic** but serves the JSON shapes of real captures: the `getState`,
//! `getModel` and `getInputAndOutputList` bodies come from `tests/fixtures/devices/eversolo/`
//! (real DMP-A8 Gen 2 payloads from https://github.com/tmeynell/ha-eversolo, MIT), patched with
//! the mock's mutable volume, mute, input and MAC. The set endpoints mirror the paths used by
//! hchris1/Eversolo and tomekceszke/eversolo-sdk. No Eversolo hardware has been used.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::Router;
use serde_json::Value;
use tempfile::TempDir;

use unified_hifi_control::adapters::eversolo::{EversoloAdapter, UnitOptions};
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    Confirmation, DeviceCommandReport, DeviceError, DeviceRequest, DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{Device, DeviceId, DevicePower};

const STATE: &str = include_str!("fixtures/devices/eversolo/getstate.json");
const MODEL: &str = include_str!("fixtures/devices/eversolo/getmodel.json");
const INPUTS: &str = include_str!("fixtures/devices/eversolo/getinputandoutputlist.json");

struct MockState {
    mac: String,
    volume: i64,
    max: i64,
    muted: bool,
    input_index: i64,
    locked: bool,
    able_remote_boot: bool,
    /// False: every endpoint answers 503, like a unit that has shut down.
    up: bool,
    /// Power-off is accepted (200) but the unit stays up.
    ignore_poweroff: bool,
    requests: Vec<String>,
}

type Shared = Arc<Mutex<MockState>>;

struct MockEversolo {
    state: Shared,
    port: u16,
}

async fn handle(
    State(state): State<Shared>,
    uri: Uri,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let mut s = state.lock().unwrap();
    s.requests.push(match uri.query() {
        Some(q) => format!("{}?{q}", uri.path()),
        None => uri.path().to_string(),
    });
    if !s.up {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let json = |body: &str, patch: &dyn Fn(&mut Value)| {
        let mut value: Value = serde_json::from_str(body).unwrap();
        patch(&mut value);
        axum::Json(value).into_response()
    };
    match uri.path() {
        "/ZidooMusicControl/v2/getState" => json(STATE, &|v| {
            v["volumeData"]["currenttVolume"] = s.volume.into();
            v["volumeData"]["maxVolume"] = s.max.into();
            v["volumeData"]["isMute"] = s.muted.into();
            v["volumeData"]["isLock"] = s.locked.into();
            v["deviceInfo"]["net_mac"] = s.mac.clone().into();
        }),
        "/ZidooControlCenter/getModel" => json(MODEL, &|v| {
            v["net_mac"] = s.mac.clone().into();
            v["ableRemoteBoot"] = s.able_remote_boot.into();
        }),
        "/ZidooMusicControl/v2/getInputAndOutputList" => json(INPUTS, &|v| {
            v["inputIndex"] = s.input_index.into();
        }),
        "/ZidooMusicControl/v2/setDevicesVolume" => {
            s.volume = query["volume"].parse().unwrap();
            axum::Json(serde_json::json!({"status": 200})).into_response()
        }
        "/ZidooMusicControl/v2/setMuteVolume" => {
            s.muted = query["isMute"] == "1";
            axum::Json(serde_json::json!({"status": 200})).into_response()
        }
        "/ZidooMusicControl/v2/setInputList" => {
            s.input_index = query["index"].parse().unwrap();
            axum::Json(serde_json::json!({"status": 200})).into_response()
        }
        "/ZidooMusicControl/v2/setPowerOption" => {
            if query["tag"] == "poweroff" && !s.ignore_poweroff {
                s.up = false;
            }
            axum::Json(serde_json::json!({"status": 200})).into_response()
        }
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

impl MockEversolo {
    async fn start(mac: &str) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            mac: mac.to_string(),
            volume: 109,
            max: 200,
            muted: false,
            input_index: 0,
            locked: false,
            able_remote_boot: true,
            up: true,
            ignore_poweroff: false,
            requests: Vec::new(),
        }));
        let app = Router::new().fallback(handle).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self { state, port }
    }

    fn requests(&self) -> Vec<String> {
        self.state.lock().unwrap().requests.clone()
    }

    fn writes(&self) -> Vec<String> {
        self.requests()
            .into_iter()
            .filter(|r| !r.contains("/get"))
            .collect()
    }

    fn set(&self, change: impl FnOnce(&mut MockState)) {
        change(&mut self.state.lock().unwrap());
    }
}

struct Harness {
    service: Arc<DeviceService>,
    aggregator: Arc<ZoneAggregator>,
    adapter: Arc<EversoloAdapter>,
    _dir: TempDir,
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
    let service = Arc::new(
        DeviceService::new(
            aggregator.clone(),
            bus.clone(),
            DeviceLinkStore::open(dir.path().join("device-links.json")),
        )
        .with_confirm_timeout(Duration::from_millis(2500)),
    );
    let adapter = Arc::new(EversoloAdapter::new(bus));
    service.register_driver(adapter.clone()).await;
    Harness {
        service,
        aggregator,
        adapter,
        _dir: dir,
    }
}

fn id(mac: &str) -> DeviceId {
    DeviceId::new("eversolo", mac).unwrap()
}

impl Harness {
    async fn attach(&self, mac: &str, mock: &MockEversolo) -> Device {
        self.adapter
            .attach_unit(&id(mac), "127.0.0.1", mock.port)
            .await;
        self.observed(mac).await
    }

    async fn observed(&self, mac: &str) -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(device) = self.aggregator.get_device(&id(mac)).await {
                return device;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("device not observed within 5s");
    }

    async fn current(&self, mac: &str) -> Device {
        self.aggregator.get_device(&id(mac)).await.unwrap()
    }

    async fn control(
        &self,
        mac: &str,
        request: DeviceRequest,
    ) -> Result<DeviceCommandReport, DeviceError> {
        self.service
            .control(DeviceTarget::Device(id(mac)), request)
            .await
    }
}

const FRONT: &str = "AABBCC000001";
const FRONT_MAC: &str = "aa:bb:cc:00:00:01";
const BEDROOM: &str = "AABBCC000002";
const BEDROOM_MAC: &str = "aa:bb:cc:00:00:02";

#[tokio::test]
async fn device_is_observed_with_reported_identity_scale_and_state() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    let device = h.attach(FRONT, &mock).await;

    assert_eq!(device.device_id.as_str(), "eversolo:AABBCC000001");
    assert_eq!(device.vendor, "Eversolo");
    assert_eq!(device.model, "DMP-A8 Gen 2");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.volume.scale, VolumeScale::Steps);
    assert_eq!(device.volume.value, Some(109.0));
    assert_eq!(device.volume.min, 0.0);
    assert_eq!(device.volume.hardware_max, Some(200.0));
    assert_eq!(device.volume.step, 1.0);
    assert_eq!(device.volume.is_muted, Some(false));
    assert_eq!(device.input.as_deref(), Some("XMOS"));
    let ids: Vec<_> = device.inputs.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["XMOS", "BT", "SPDIF", "EARC"]);
    assert!(device.volume_control().is_some());
    let caps = &device.capabilities;
    assert!(caps.observe.supported && caps.volume.supported && caps.mute.supported);
    assert!(caps.power.supported && caps.input_select.supported);
    assert!(caps.volume.detail.contains("setDevicesVolume"));
}

#[tokio::test]
async fn revision_increases_on_every_readback() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    let first = h.attach(FRONT, &mock).await;
    let report = h.control(FRONT, DeviceRequest::Mute(true)).await.unwrap();
    assert!(report.observed.revision > first.revision);
}

#[tokio::test]
async fn no_state_changing_requests_are_sent_on_connect() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    h.attach(FRONT, &mock).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!mock.requests().is_empty(), "state must have been read");
    assert!(
        mock.writes().is_empty(),
        "connect wrote: {:?}",
        mock.writes()
    );
}

#[tokio::test]
async fn volume_is_clamped_by_the_ceiling_and_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    h.attach(FRONT, &mock).await;
    h.service
        .set_ceiling(id(FRONT).as_str(), Some(150.0))
        .await
        .unwrap();

    let report = h
        .control(FRONT, DeviceRequest::VolumeAbsolute(190.0))
        .await
        .unwrap();
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(150.0));
    assert_eq!(report.observed.volume.value, Some(150.0));
    assert_eq!(
        mock.writes(),
        ["/ZidooMusicControl/v2/setDevicesVolume?volume=150"]
    );
}

#[tokio::test]
async fn mute_is_set_explicitly_and_confirmed() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    h.attach(FRONT, &mock).await;

    let on = h.control(FRONT, DeviceRequest::Mute(true)).await.unwrap();
    assert_eq!(on.confirmation, Confirmation::Confirmed);
    assert_eq!(on.observed.volume.is_muted, Some(true));
    // Muting an already muted unit still reads back and confirms; no toggle is ever sent.
    h.control(FRONT, DeviceRequest::Mute(true)).await.unwrap();
    let off = h.control(FRONT, DeviceRequest::Mute(false)).await.unwrap();
    assert_eq!(off.observed.volume.is_muted, Some(false));
    assert!(mock
        .writes()
        .iter()
        .all(|w| w.contains("setMuteVolume?isMute=")));
}

#[tokio::test]
async fn input_is_selected_by_tag_or_name_and_read_back() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    h.attach(FRONT, &mock).await;

    let report = h
        .control(FRONT, DeviceRequest::SelectInput("SPDIF".into()))
        .await
        .unwrap();
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.input.as_deref(), Some("SPDIF"));
    // The user's own name for the input resolves to the same tag.
    h.control(FRONT, DeviceRequest::SelectInput("TV".into()))
        .await
        .unwrap();
    assert_eq!(
        mock.writes(),
        [
            "/ZidooMusicControl/v2/setInputList?tag=SPDIF&index=2",
            "/ZidooMusicControl/v2/setInputList?tag=EARC&index=3"
        ]
    );
    let unknown = h
        .control(FRONT, DeviceRequest::SelectInput("HDMI".into()))
        .await;
    assert!(
        matches!(unknown, Err(DeviceError::UnknownInput { .. })),
        "{unknown:?}"
    );
}

#[tokio::test]
async fn power_off_is_confirmed_as_standby_once_the_unit_stops_answering() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    h.attach(FRONT, &mock).await;

    let report = h.control(FRONT, DeviceRequest::PowerOff).await.unwrap();
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.power, DevicePower::Standby);
    assert_eq!(
        mock.writes(),
        ["/ZidooMusicControl/v2/setPowerOption?tag=poweroff"]
    );
}

#[tokio::test]
async fn power_off_that_the_unit_ignores_is_not_called_standby() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    mock.set(|s| s.ignore_poweroff = true);
    h.attach(FRONT, &mock).await;
    let result = h.control(FRONT, DeviceRequest::PowerOff).await;
    assert!(
        matches!(result, Err(DeviceError::Unconfirmed { .. })),
        "{result:?}"
    );
    assert_eq!(h.current(FRONT).await.power, DevicePower::On);
}

#[tokio::test]
async fn power_on_sends_wake_on_lan_and_confirms_only_when_the_unit_answers() {
    let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let wol: SocketAddr = listener.local_addr().unwrap();
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    h.adapter
        .attach_unit_with(
            &id(FRONT),
            "127.0.0.1",
            mock.port,
            UnitOptions {
                wol_targets: Some(vec![wol]),
            },
        )
        .await;
    h.observed(FRONT).await;
    h.control(FRONT, DeviceRequest::PowerOff).await.unwrap();
    assert_eq!(h.current(FRONT).await.power, DevicePower::Standby);

    // The magic packet is sent, but the unit has not booted: unconfirmed, not success.
    let result = h.control(FRONT, DeviceRequest::PowerOn).await;
    assert!(
        matches!(result, Err(DeviceError::Unconfirmed { .. })),
        "{result:?}"
    );
    let mut buffer = [0u8; 256];
    let (len, _) = tokio::time::timeout(Duration::from_secs(1), listener.recv_from(&mut buffer))
        .await
        .expect("a magic packet was sent")
        .unwrap();
    assert_eq!(len, 102);
    assert_eq!(&buffer[..6], &[0xFF; 6]);
    assert_eq!(&buffer[6..12], &[0xAA, 0xBB, 0xCC, 0x00, 0x00, 0x01]);
    assert_ne!(h.current(FRONT).await.power, DevicePower::On);

    // The unit boots; the fast poll then reports it On with no further command.
    mock.set(|s| s.up = true);
    let deadline = Instant::now() + Duration::from_secs(6);
    while h.current(FRONT).await.power != DevicePower::On {
        assert!(
            Instant::now() < deadline,
            "unit never reported On after boot"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
async fn power_on_without_remote_boot_is_refused_and_sends_nothing() {
    let listener = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let wol = listener.local_addr().unwrap();
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    mock.set(|s| s.able_remote_boot = false);
    h.adapter
        .attach_unit_with(
            &id(FRONT),
            "127.0.0.1",
            mock.port,
            UnitOptions {
                wol_targets: Some(vec![wol]),
            },
        )
        .await;
    h.observed(FRONT).await;
    h.control(FRONT, DeviceRequest::PowerOff).await.unwrap();
    let result = h.control(FRONT, DeviceRequest::PowerOn).await;
    assert!(
        matches!(result, Err(DeviceError::Dispatch { .. })),
        "{result:?}"
    );
    let mut buffer = [0u8; 256];
    assert!(
        tokio::time::timeout(Duration::from_millis(300), listener.recv_from(&mut buffer))
            .await
            .is_err(),
        "no magic packet may be sent"
    );
}

#[tokio::test]
async fn locked_volume_is_unsupported_and_nothing_is_written() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    mock.set(|s| s.locked = true);
    let device = h.attach(FRONT, &mock).await;
    assert!(!device.capabilities.volume.supported);
    assert_eq!(device.volume.value, None);
    assert!(device.volume_control().is_none());

    for request in [
        DeviceRequest::VolumeAbsolute(100.0),
        DeviceRequest::Mute(true),
    ] {
        let result = h.control(FRONT, request.clone()).await;
        assert!(result.is_err(), "{request:?}: {result:?}");
    }
    assert!(mock.writes().is_empty(), "{:?}", mock.writes());
}

#[tokio::test]
async fn a_host_reporting_another_mac_is_not_controlled() {
    let h = harness().await;
    // Attached as BEDROOM, but the host answers as FRONT.
    let mock = MockEversolo::start(FRONT_MAC).await;
    let device = h.attach(BEDROOM, &mock).await;
    assert_eq!(device.power, DevicePower::Unknown);
    assert_eq!(device.input, None);
    assert_eq!(device.volume.value, None);
    assert!(device.connection.detail.unwrap().contains("different MAC"));
    let result = h.control(BEDROOM, DeviceRequest::Mute(true)).await;
    assert!(result.is_err(), "{result:?}");
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn an_unreachable_configured_unit_is_unknown_not_standby() {
    let h = harness().await;
    let mock = MockEversolo::start(FRONT_MAC).await;
    mock.set(|s| s.up = false);
    let device = h.attach(FRONT, &mock).await;
    assert_eq!(device.power, DevicePower::Unknown);
    assert_eq!(device.volume.value, None);
    assert!(device.volume_control().is_none());
    let result = h.control(FRONT, DeviceRequest::PowerOff).await;
    assert!(result.is_err(), "{result:?}");
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn two_units_never_cross_route() {
    let h = harness().await;
    let front = MockEversolo::start(FRONT_MAC).await;
    let bedroom = MockEversolo::start(BEDROOM_MAC).await;
    h.attach(FRONT, &front).await;
    h.attach(BEDROOM, &bedroom).await;

    h.control(FRONT, DeviceRequest::VolumeAbsolute(60.0))
        .await
        .unwrap();
    h.control(BEDROOM, DeviceRequest::SelectInput("BT".into()))
        .await
        .unwrap();
    assert_eq!(
        front.writes(),
        ["/ZidooMusicControl/v2/setDevicesVolume?volume=60"]
    );
    assert_eq!(
        bedroom.writes(),
        ["/ZidooMusicControl/v2/setInputList?tag=BT&index=1"]
    );
    assert_eq!(h.current(FRONT).await.volume.value, Some(60.0));
    assert_eq!(h.current(BEDROOM).await.volume.value, Some(109.0));
}
