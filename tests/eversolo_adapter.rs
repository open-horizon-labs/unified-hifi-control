//! Eversolo device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The mock below is **synthetic**, shaped from the vendor document
//! `Eversolo_DMP-A6_TCP_en_v1.0.pdf`: an HTTP server answering
//! `GET .../RemoteControl/sendkey?key=KEY` on an ephemeral port. The document defines no response
//! body, so the mock returns an empty 200. No Eversolo hardware has been used.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, Uri};
use axum::Router;
use tempfile::TempDir;

use unified_hifi_control::adapters::eversolo::EversoloAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    DeviceError, DeviceRequest, DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{Device, DeviceId, DevicePower};

#[derive(Default)]
struct MockState {
    /// `path?query` of every request received.
    requests: Mutex<Vec<String>>,
    /// Paths that answer 404 (to exercise the document's two URL forms).
    not_found: Mutex<Vec<String>>,
}

struct MockEversolo {
    state: Arc<MockState>,
    port: u16,
}

async fn sendkey(
    State(state): State<Arc<MockState>>,
    uri: Uri,
    RawQuery(query): RawQuery,
) -> StatusCode {
    let path = uri.path().to_string();
    if state
        .not_found
        .lock()
        .map(|paths| paths.contains(&path))
        .unwrap_or(false)
    {
        return StatusCode::NOT_FOUND;
    }
    if let Ok(mut requests) = state.requests.lock() {
        requests.push(format!("{path}?{}", query.unwrap_or_default()));
    }
    StatusCode::OK
}

impl MockEversolo {
    async fn start() -> Self {
        let state = Arc::new(MockState::default());
        let app = Router::new().fallback(sendkey).with_state(state.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });
        Self { state, port }
    }

    fn requests(&self) -> Vec<String> {
        self.state.requests.lock().unwrap().clone()
    }

    fn keys(&self) -> Vec<String> {
        self.requests()
            .iter()
            .filter_map(|r| r.split_once("?key=").map(|(_, k)| k.to_string()))
            .collect()
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
        .with_confirm_timeout(Duration::from_millis(600)),
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

    async fn control(&self, mac: &str, request: DeviceRequest) -> Result<(), DeviceError> {
        self.service
            .control(DeviceTarget::Device(id(mac)), request)
            .await
            .map(|_| ())
    }
}

const FRONT: &str = "AA:BB:CC:00:11:22";
const BEDROOM: &str = "AA:BB:CC:00:11:33";

#[tokio::test]
async fn device_is_observed_with_mac_identity_and_honest_capabilities() {
    let h = harness().await;
    let mock = MockEversolo::start().await;
    let device = h.attach(FRONT, &mock).await;

    assert_eq!(device.device_id.as_str(), "eversolo:AABBCC001122");
    assert_eq!(device.vendor, "Eversolo");
    assert_eq!(device.maturity, "alpha");
    // Nothing is readable: power is unknown (never rounded to standby), there is no level.
    assert_eq!(device.power, DevicePower::Unknown);
    assert_eq!(device.volume.scale, VolumeScale::Unknown);
    assert_eq!(device.volume.value, None);
    assert_eq!(device.volume.hardware_max, None);
    assert!(device.volume_control().is_none());
    assert!(!device.capabilities.volume.supported);
    assert!(!device.capabilities.mute.supported);
    assert!(device.capabilities.power.supported);
    assert!(device.capabilities.input_select.supported);
    let ids: Vec<_> = device.inputs.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids, ["XMOS", "BT", "USB", "SPDIF", "COA"]);
}

#[tokio::test]
async fn no_state_changing_bytes_are_sent_on_connect() {
    let h = harness().await;
    let mock = MockEversolo::start().await;
    h.attach(FRONT, &mock).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        mock.requests().is_empty(),
        "connect must send no key: {:?}",
        mock.requests()
    );
}

#[tokio::test]
async fn input_selection_sends_the_documented_key_and_publishes_a_fresh_revision() {
    let h = harness().await;
    let mock = MockEversolo::start().await;
    let before = h.attach(FRONT, &mock).await;

    // Nothing reads the input back, so the service must say unconfirmed rather than succeed.
    let result = h
        .control(FRONT, DeviceRequest::SelectInput("USB".into()))
        .await;
    assert!(
        matches!(result, Err(DeviceError::Unconfirmed { .. })),
        "{result:?}"
    );
    assert_eq!(mock.keys(), ["Key.DAC.USB"]);
    let after = h.aggregator.get_device(&id(FRONT)).await.unwrap();
    assert!(
        after.revision > before.revision,
        "readback is a new revision"
    );
    assert_eq!(after.input, None, "an input never read is never claimed");
}

#[tokio::test]
async fn power_off_sends_key_poweroff_and_power_on_is_refused() {
    let h = harness().await;
    let mock = MockEversolo::start().await;
    h.attach(FRONT, &mock).await;

    let off = h.control(FRONT, DeviceRequest::PowerOff).await;
    assert!(
        matches!(off, Err(DeviceError::Unconfirmed { .. })),
        "{off:?}"
    );
    assert_eq!(mock.keys(), ["Key.Poweroff"]);

    let on = h.control(FRONT, DeviceRequest::PowerOn).await;
    assert!(matches!(on, Err(DeviceError::Dispatch { .. })), "{on:?}");
    assert_eq!(mock.keys(), ["Key.Poweroff"], "power on must send nothing");
}

#[tokio::test]
async fn unsupported_operations_error_and_send_nothing() {
    let h = harness().await;
    let mock = MockEversolo::start().await;
    h.attach(FRONT, &mock).await;
    // A user ceiling gives volume a bound the document never does; it must still be refused.
    h.service
        .set_ceiling(id(FRONT).as_str(), Some(50.0))
        .await
        .unwrap();

    for request in [
        DeviceRequest::VolumeAbsolute(20.0),
        DeviceRequest::Mute(true),
        DeviceRequest::PowerOn,
    ] {
        let result = h.control(FRONT, request.clone()).await;
        assert!(
            matches!(result, Err(DeviceError::Dispatch { .. })),
            "{request:?}: {result:?}"
        );
    }
    assert!(mock.requests().is_empty(), "{:?}", mock.requests());
}

#[tokio::test]
async fn unlisted_inputs_are_refused() {
    let h = harness().await;
    let mock = MockEversolo::start().await;
    h.attach(FRONT, &mock).await;
    let result = h
        .control(FRONT, DeviceRequest::SelectInput("HDMI".into()))
        .await;
    assert!(
        matches!(result, Err(DeviceError::UnknownInput { .. })),
        "{result:?}"
    );
    assert!(mock.requests().is_empty());
}

#[tokio::test]
async fn two_units_never_cross_route() {
    let h = harness().await;
    let front = MockEversolo::start().await;
    let bedroom = MockEversolo::start().await;
    h.attach(FRONT, &front).await;
    h.attach(BEDROOM, &bedroom).await;

    h.control(FRONT, DeviceRequest::SelectInput("BT".into()))
        .await
        .ok();
    h.control(BEDROOM, DeviceRequest::SelectInput("COA".into()))
        .await
        .ok();
    assert_eq!(front.keys(), ["Key.DAC.BT"]);
    assert_eq!(bedroom.keys(), ["Key.DAC.COA"]);
}

#[tokio::test]
async fn a_404_on_the_example_path_falls_back_to_the_base_url_path() {
    let h = harness().await;
    let mock = MockEversolo::start().await;
    mock.state
        .not_found
        .lock()
        .unwrap()
        .push("/ZidooControlCenter/RemoteControl/sendkey".to_string());
    h.attach(FRONT, &mock).await;

    h.control(FRONT, DeviceRequest::SelectInput("SPDIF".into()))
        .await
        .ok();
    assert_eq!(
        mock.requests(),
        ["/ControlCenter/RemoteControl/sendkey?key=Key.DAC.SPDIF"]
    );
}
