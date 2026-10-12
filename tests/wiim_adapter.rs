//! WiiM (LinkPlay) device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! `MockWiim` is a **synthetic** LinkPlay `httpapi.asp` server shaped from the WiiM / LinkPlay
//! HTTP API documentation and the Linkplay2020/wiim client; no WiiM hardware exists for these
//! tests. Two mocks of the same model run side by side so a test that routes by model, discovery
//! order or "the only WiiM" fails.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::Router;
use tokio::net::TcpListener;

use unified_hifi_control::adapters::devices::DeviceFamily;
use unified_hifi_control::adapters::wiim::protocol::StatusEx;
use unified_hifi_control::adapters::wiim::WiimAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{Device, DeviceId, DevicePower};

const KITCHEN: &str = "FF970016A6FE22C1660AB4D8";
const OFFICE: &str = "FF31F09ED6C4E8F6C2C1BD5A";

#[derive(Debug, Default)]
struct MockState {
    vol: u32,
    mute: bool,
    mode: u32,
    uuid: String,
    /// Every `setPlayerCmd` received: the only state-changing requests.
    writes: Vec<String>,
    /// Every command received.
    requests: Vec<String>,
}

struct MockWiim {
    state: Arc<Mutex<MockState>>,
    base: String,
}

fn mode_for(input: &str) -> Option<u32> {
    Some(match input {
        "wifi" => 31,
        "line-in" => 40,
        "bluetooth" => 41,
        "optical" => 42,
        "RCA" => 43,
        "co-axial" => 44,
        _ => return None,
    })
}

async fn handler(
    State(state): State<Arc<Mutex<MockState>>>,
    Query(query): Query<HashMap<String, String>>,
) -> String {
    let command = query.get("command").cloned().unwrap_or_default();
    let mut s = state.lock().unwrap();
    s.requests.push(command.clone());
    if command == "getStatusEx" {
        return format!(
            r#"{{"uuid":"{}","DeviceName":"WiiM Mini-{}","project":"Muzo_Mini","firmware":"Linkplay.4.6.415145","MAC":"08:E9:F6:8F:8F:A2"}}"#,
            s.uuid,
            &s.uuid[s.uuid.len() - 4..]
        );
    }
    if command == "getPlayerStatus" {
        return format!(
            r#"{{"type":"0","mode":"{}","status":"play","vol":"{}","mute":"{}"}}"#,
            s.mode,
            s.vol,
            u8::from(s.mute)
        );
    }
    if let Some(rest) = command.strip_prefix("setPlayerCmd:") {
        s.writes.push(rest.to_string());
        let parts: Vec<&str> = rest.splitn(3, ':').collect();
        match parts.as_slice() {
            ["vol", n] => {
                if let Ok(n) = n.parse::<u32>() {
                    s.vol = n.min(100);
                    return "OK".into();
                }
            }
            ["mute", n] => {
                s.mute = *n == "1";
                return "OK".into();
            }
            ["switchmode", input] => {
                if let Some(mode) = mode_for(input) {
                    s.mode = mode;
                    return "OK".into();
                }
            }
            _ => {}
        }
        return "Failed".into();
    }
    "unknown command".into()
}

impl MockWiim {
    async fn start(uuid: &str, vol: u32) -> Self {
        let state = Arc::new(Mutex::new(MockState {
            vol,
            mode: 31,
            uuid: uuid.to_string(),
            ..Default::default()
        }));
        let app = Router::new()
            .route("/httpapi.asp", get(handler))
            .with_state(state.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self { state, base }
    }

    fn writes(&self) -> Vec<String> {
        self.state.lock().unwrap().writes.clone()
    }

    fn vol(&self) -> u32 {
        self.state.lock().unwrap().vol
    }

    fn status(&self) -> StatusEx {
        let s = self.state.lock().unwrap();
        StatusEx {
            uuid: Some(s.uuid.clone()),
            mac: None,
            name: format!("WiiM Mini-{}", &s.uuid[s.uuid.len() - 4..]),
            project: "Muzo_Mini".to_string(),
        }
    }
}

struct Harness {
    service: Arc<DeviceService>,
    aggregator: Arc<ZoneAggregator>,
    adapter: Arc<WiimAdapter>,
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
    let service = Arc::new(
        DeviceService::new(
            aggregator.clone(),
            bus.clone(),
            DeviceLinkStore::open(dir.path().join("device-links.json")),
        )
        .with_confirm_timeout(Duration::from_millis(2500)),
    );
    let adapter = Arc::new(WiimAdapter::new(bus));
    service.register_driver(adapter.clone()).await;
    Harness {
        service,
        aggregator,
        adapter,
        _dir: dir,
    }
}

fn id(uuid: &str) -> DeviceId {
    DeviceId::new("wiim", uuid).unwrap()
}

impl Harness {
    async fn attach(&self, uuid: &str, mock: &MockWiim) -> DeviceId {
        let device_id = self
            .adapter
            .attach_unit(&mock.base, mock.status())
            .await
            .expect("attached");
        assert_eq!(device_id, id(uuid));
        self.wait_device(&device_id, |d| d.volume.value.is_some())
            .await;
        device_id
    }

    async fn wait_device(
        &self,
        device_id: &DeviceId,
        condition: impl Fn(&Device) -> bool,
    ) -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(device) = self.aggregator.get_device(device_id).await {
                if condition(&device) {
                    return device;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within 5s");
    }

    async fn control(
        &self,
        device_id: &DeviceId,
        request: DeviceRequest,
    ) -> Result<unified_hifi_control::devices::service::DeviceCommandReport, DeviceError> {
        self.service
            .control(DeviceTarget::Device(device_id.clone()), request)
            .await
    }
}

#[tokio::test]
async fn observed_with_uuid_identity_percentage_scale_and_no_power() {
    let h = harness().await;
    let mock = MockWiim::start(KITCHEN, 42).await;
    let device_id = h.attach(KITCHEN, &mock).await;

    let device = h.aggregator.get_device(&device_id).await.unwrap();
    assert_eq!(device.device_id.as_str(), format!("wiim:{KITCHEN}"));
    assert_eq!(device.vendor, "WiiM");
    assert_eq!(device.model, "Muzo Mini");
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(device.volume.value, Some(42.0));
    assert_eq!(device.volume.min, 0.0);
    assert_eq!(device.volume.step, 1.0);
    assert_eq!(device.volume.hardware_max, Some(100.0));
    assert_eq!(device.volume.is_muted, Some(false));
    assert_eq!(device.power, DevicePower::Unknown);
    assert_eq!(device.input.as_deref(), Some("wifi"));
    assert!(!device.capabilities.power.supported);
    assert!(device.capabilities.volume.supported);
    assert_eq!(device.maturity, "alpha");
}

/// CRITICAL: connecting only asks. A level sent on connect would replay a cached level onto a
/// speaker someone has since turned down.
#[tokio::test]
async fn critical_no_state_changing_request_on_connect() {
    let h = harness().await;
    let mock = MockWiim::start(KITCHEN, 42).await;
    h.attach(KITCHEN, &mock).await;
    // Let several polls elapse.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        mock.writes().is_empty(),
        "connect wrote {:?}",
        mock.writes()
    );
    assert!(mock
        .state
        .lock()
        .unwrap()
        .requests
        .iter()
        .any(|r| r == "getPlayerStatus"));
}

/// CRITICAL: with a ceiling, no level above it ever reaches the speaker, and the write is confirmed by readback.
#[tokio::test]
async fn critical_ceiling_bounds_volume_and_write_is_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockWiim::start(KITCHEN, 20).await;
    let device_id = h.attach(KITCHEN, &mock).await;

    // The protocol's 0–100 is the hardware maximum; the ceiling below lowers it.

    h.service
        .set_ceiling(device_id.as_str(), Some(35.0))
        .await
        .unwrap();
    h.wait_device(&device_id, |d| d.volume.effective_max() == Some(35.0))
        .await;

    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(90.0))
        .await
        .expect("confirmed");
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(35.0));
    assert_eq!(report.observed.volume.value, Some(35.0));
    assert_eq!(mock.writes(), vec!["vol:35".to_string()]);
    assert_eq!(mock.vol(), 35);

    // A relative climb is resolved against the observed level and still capped.
    h.control(&device_id, DeviceRequest::VolumeRelative(20.0))
        .await
        .expect("confirmed");
    assert!(
        mock.vol() <= 35,
        "level above the ceiling reached the speaker: {}",
        mock.vol()
    );
}

#[tokio::test]
async fn mute_and_input_are_written_and_read_back() {
    let h = harness().await;
    let mock = MockWiim::start(KITCHEN, 20).await;
    let device_id = h.attach(KITCHEN, &mock).await;

    let muted = h
        .control(&device_id, DeviceRequest::Mute(true))
        .await
        .expect("confirmed");
    assert_eq!(muted.observed.volume.is_muted, Some(true));
    let selected = h
        .control(&device_id, DeviceRequest::SelectInput("optical".into()))
        .await
        .expect("confirmed");
    assert_eq!(selected.observed.input.as_deref(), Some("optical"));
    assert_eq!(
        mock.writes(),
        vec!["mute:1".to_string(), "switchmode:optical".to_string()]
    );
}

/// CRITICAL: two units of one model never cross-route.
#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let kitchen = MockWiim::start(KITCHEN, 20).await;
    let office = MockWiim::start(OFFICE, 25).await;
    let kitchen_id = h.attach(KITCHEN, &kitchen).await;
    let office_id = h.attach(OFFICE, &office).await;
    assert_ne!(kitchen_id, office_id);

    h.service
        .set_ceiling(kitchen_id.as_str(), Some(60.0))
        .await
        .unwrap();
    h.wait_device(&kitchen_id, |d| d.volume.effective_max() == Some(60.0))
        .await;
    h.control(&kitchen_id, DeviceRequest::VolumeAbsolute(55.0))
        .await
        .expect("confirmed");

    assert_eq!(kitchen.writes(), vec!["vol:55".to_string()]);
    assert!(
        office.writes().is_empty(),
        "office received {:?}",
        office.writes()
    );
    assert_eq!(office.vol(), 25);
    let office_device = h.aggregator.get_device(&office_id).await.unwrap();
    assert_eq!(office_device.volume.value, Some(25.0));
}

/// Power is not modelled as pause: it errors, at the service and at the driver, and nothing is
/// sent.
#[tokio::test]
async fn unsupported_power_errors_instead_of_succeeding() {
    let h = harness().await;
    let mock = MockWiim::start(KITCHEN, 20).await;
    let device_id = h.attach(KITCHEN, &mock).await;

    assert!(h.control(&device_id, DeviceRequest::PowerOn).await.is_err());
    assert!(h
        .control(&device_id, DeviceRequest::PowerOff)
        .await
        .is_err());
    for operation in [DeviceOperation::PowerOn, DeviceOperation::PowerStandby] {
        let error = h.adapter.execute(&device_id, operation).await.unwrap_err();
        assert!(error.contains("not supported"), "{error}");
    }
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn unknown_input_is_refused_without_sending() {
    let h = harness().await;
    let mock = MockWiim::start(KITCHEN, 20).await;
    let device_id = h.attach(KITCHEN, &mock).await;
    assert!(h
        .control(&device_id, DeviceRequest::SelectInput("phono".into()))
        .await
        .is_err());
    let error = h
        .adapter
        .execute(
            &device_id,
            DeviceOperation::SelectInput("x&command=reboot".into()),
        )
        .await
        .unwrap_err();
    assert!(error.contains("unknown input"), "{error}");
    assert!(mock.writes().is_empty());
}

#[tokio::test]
async fn stopping_the_family_removes_the_device() {
    let h = harness().await;
    let mock = MockWiim::start(KITCHEN, 20).await;
    let device_id = h.attach(KITCHEN, &mock).await;
    h.adapter.stop_family().await;
    let deadline = Instant::now() + Duration::from_secs(5);
    while h.aggregator.get_device(&device_id).await.is_some() {
        assert!(
            Instant::now() < deadline,
            "device still observed after stop"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
