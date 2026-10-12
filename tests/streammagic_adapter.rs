//! StreamMagic (Cambridge Audio) device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The unit is `MockStreamMagic`, defined in this file: a **synthetic** WebSocket server shaped
//! from the aiostreammagic models and client (<https://github.com/noahhusby/aiostreammagic>). It
//! is not a capture from a unit and proves wiring, not hardware behaviour.
//!
//! Every frame the mock receives is recorded, so a test can assert what was (and was not) sent.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio::sync::{broadcast, Mutex};
use tokio_tungstenite::tungstenite::Message;
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::streammagic::StreamMagicAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    Confirmation, DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService,
    DeviceTarget,
};
use unified_hifi_control::devices::{Device, DeviceId, DevicePower};

// =============================================================================
// Synthetic StreamMagic unit
// =============================================================================

#[derive(Debug, Clone)]
struct MockState {
    unit_id: String,
    model: String,
    name: String,
    power: bool,
    source: String,
    pre_amp: bool,
    volume: u32,
    muted: bool,
    limit: Option<u32>,
    /// Every frame received, in order, across all connections.
    frames: Vec<Value>,
    connections: usize,
}

impl MockState {
    fn evo(unit_id: &str, name: &str) -> Self {
        Self {
            unit_id: unit_id.to_string(),
            model: "Evo 150".to_string(),
            name: name.to_string(),
            power: true,
            source: "MEDIA_PLAYER".to_string(),
            pre_amp: true,
            volume: 35,
            muted: false,
            limit: Some(80),
            frames: Vec::new(),
            connections: 0,
        }
    }

    fn zone_state(&self) -> Value {
        json!({
            "source": self.source, "power": self.power, "pre_amp_mode": self.pre_amp,
            "pre_amp_state": self.pre_amp, "volume_percent": self.volume, "volume_db": -40,
            "mute": self.muted, "cbus": "off", "standby_mode": "NETWORK"
        })
    }

    fn data(&self, path: &str) -> Value {
        match path {
            "/system/info" => json!({
                "name": self.name, "model": self.model, "timezone": "UTC", "locale": "en_US",
                "udn": format!("uuid:00000000-0000-1000-8000-{}", self.unit_id),
                "unit_id": self.unit_id, "api": "1.7"
            }),
            "/system/sources" => json!({"sources": [
                {"id": "MEDIA_PLAYER", "name": "Media Player", "default_name": "Media Player",
                 "nameable": false, "ui_selectable": true, "description": "x",
                 "description_locale": "x"},
                {"id": "IR", "name": "Optical", "default_name": "Optical", "nameable": true,
                 "ui_selectable": true, "description": "x", "description_locale": "x"}
            ]}),
            "/zone/state" => self.zone_state(),
            "/system/power" => json!({"power": if self.power { "ON" } else { "NETWORK" }}),
            "/zone/audio" => match self.limit {
                Some(limit) => json!({"volume_limit_percent": limit}),
                None => json!({}),
            },
            _ => json!({}),
        }
    }
}

fn response(path: &str, result: i64, message: &str, data: Value) -> String {
    json!({"path": path, "type": "response", "result": result, "message": message,
           "params": {"data": data}})
    .to_string()
}

fn is_write(frame: &Value) -> bool {
    let params = &frame["params"];
    [
        "volume_percent",
        "mute",
        "source",
        "power",
        "volume_step_change",
    ]
    .iter()
    .any(|key| params.get(*key).is_some())
}

struct MockStreamMagic {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    push: broadcast::Sender<String>,
    kick: broadcast::Sender<()>,
    cancel: CancellationToken,
}

impl Drop for MockStreamMagic {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl MockStreamMagic {
    async fn start(initial: MockState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(initial));
        let (push, _) = broadcast::channel(16);
        let (kick, _) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        {
            let (state, push, kick, cancel) =
                (state.clone(), push.clone(), kick.clone(), cancel.clone());
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { continue };
                            let (state, push, kick, cancel) =
                                (state.clone(), push.subscribe(), kick.subscribe(), cancel.clone());
                            tokio::spawn(serve(stream, state, push, kick, cancel));
                        }
                    }
                }
            });
        }
        Self {
            addr,
            state,
            push,
            kick,
            cancel,
        }
    }

    async fn snapshot(&self) -> MockState {
        self.state.lock().await.clone()
    }

    /// Frames that set state (not queries or subscriptions).
    async fn writes(&self) -> Vec<Value> {
        self.snapshot()
            .await
            .frames
            .into_iter()
            .filter(is_write)
            .collect()
    }

    /// Someone turns the knob on the unit: the unit pushes, UHC did nothing.
    async fn remote_set_volume(&self, volume: u32) {
        let data = {
            let mut state = self.state.lock().await;
            state.volume = volume;
            state.zone_state()
        };
        let frame = json!({"path": "/zone/state", "type": "update", "params": {"data": data}});
        self.push.send(frame.to_string()).ok();
    }

    /// Drop every open connection (the unit rebooted its network stack).
    fn drop_connections(&self) {
        self.kick.send(()).ok();
    }
}

async fn serve(
    stream: tokio::net::TcpStream,
    state: Arc<Mutex<MockState>>,
    mut push: broadcast::Receiver<String>,
    mut kick: broadcast::Receiver<()>,
    cancel: CancellationToken,
) {
    let Ok(mut ws) = tokio_tungstenite::accept_async(stream).await else {
        return;
    };
    state.lock().await.connections += 1;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = kick.recv() => break,
            pushed = push.recv() => {
                let Ok(frame) = pushed else { continue };
                if ws.send(Message::Text(frame)).await.is_err() { break }
            }
            message = ws.next() => {
                let Some(Ok(message)) = message else { break };
                let Message::Text(text) = message else { continue };
                let Ok(frame) = serde_json::from_str::<Value>(&text) else { continue };
                let replies = handle(&state, frame).await;
                for reply in replies {
                    if ws.send(Message::Text(reply)).await.is_err() { return }
                }
            }
        }
    }
}

async fn handle(state: &Arc<Mutex<MockState>>, frame: Value) -> Vec<String> {
    let mut state = state.lock().await;
    state.frames.push(frame.clone());
    let path = frame["path"].as_str().unwrap_or_default().to_string();
    let params = frame["params"].clone();
    if !is_write(&frame) {
        return vec![response(&path, 200, "OK", state.data(&path))];
    }
    let mut refused = None;
    if let Some(volume) = params.get("volume_percent").and_then(Value::as_u64) {
        state.volume = volume as u32;
    }
    if let Some(muted) = params.get("mute").and_then(Value::as_bool) {
        state.muted = muted;
    }
    if let Some(source) = params.get("source").and_then(Value::as_str) {
        if source == "MEDIA_PLAYER" || source == "IR" {
            state.source = source.to_string();
        } else {
            refused = Some("unknown source");
        }
    }
    if let Some(power) = params.get("power").and_then(Value::as_str) {
        state.power = power == "ON";
    }
    match refused {
        Some(message) => vec![response(&path, 400, message, json!({}))],
        None => {
            let update = json!({"path": "/zone/state", "type": "update",
                                "params": {"data": state.zone_state()}});
            vec![response(&path, 200, "OK", json!({})), update.to_string()]
        }
    }
}

// =============================================================================
// Harness (copied in shape from tests/device_safety.rs)
// =============================================================================

struct Harness {
    adapter: Arc<StreamMagicAdapter>,
    service: Arc<DeviceService>,
    aggregator: Arc<ZoneAggregator>,
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
        .with_confirm_timeout(Duration::from_millis(1500)),
    );
    let adapter = Arc::new(StreamMagicAdapter::new(bus));
    service.register_driver(adapter.clone()).await;
    Harness {
        adapter,
        service,
        aggregator,
        _dir: dir,
    }
}

fn id(unit: &str) -> DeviceId {
    DeviceId::new("cambridge", unit).unwrap()
}

impl Harness {
    async fn attach(&self, unit: &str, mock: &MockStreamMagic) -> DeviceId {
        let device_id = self
            .adapter
            .attach_endpoint("127.0.0.1", mock.addr.port())
            .await
            .expect("probe");
        assert_eq!(device_id, id(unit));
        self.wait_for(|device| device.inputs.len() == 2 && device.power != DevicePower::Unknown)
            .await_device(self, &device_id)
            .await;
        device_id
    }

    async fn device(&self, device_id: &DeviceId) -> Device {
        self.aggregator.get_device(device_id).await.expect("device")
    }

    fn wait_for<F: Fn(&Device) -> bool>(&self, predicate: F) -> Wait<F> {
        Wait { predicate }
    }

    async fn volume(
        &self,
        device_id: &DeviceId,
        request: DeviceRequest,
    ) -> Result<(), DeviceError> {
        self.service
            .control(DeviceTarget::Device(device_id.clone()), request)
            .await
            .map(|_| ())
    }
}

struct Wait<F> {
    predicate: F,
}

impl<F: Fn(&Device) -> bool> Wait<F> {
    async fn await_device(self, harness: &Harness, device_id: &DeviceId) {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(device) = harness.aggregator.get_device(device_id).await {
                if (self.predicate)(&device) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within 8s");
    }
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn unit_is_observed_with_identity_and_percentage_scale() {
    let h = harness().await;
    let mock = MockStreamMagic::start(MockState::evo("0AABBCCDDEE0", "Living Room")).await;
    let device_id = h.attach("0AABBCCDDEE0", &mock).await;
    // The limit arrives in its own /zone/audio message, possibly after inputs and power.
    h.wait_for(|device| device.volume.hardware_max == Some(80.0))
        .await_device(&h, &device_id)
        .await;

    let device = h.device(&device_id).await;
    assert_eq!(device_id.as_str(), "cambridge:0AABBCCDDEE0");
    assert_eq!(device.vendor, "Cambridge Audio");
    assert_eq!(device.model, "Evo 150");
    assert_eq!(device.name, "Living Room");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.input.as_deref(), Some("MEDIA_PLAYER"));
    assert_eq!(device.inputs.len(), 2);
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(device.volume.value, Some(35.0));
    assert_eq!(device.volume.min, 0.0);
    assert_eq!(device.volume.step, 1.0);
    assert_eq!(
        device.volume.hardware_max,
        Some(80.0),
        "the unit's volume_limit_percent"
    );
    assert_eq!(device.volume.is_muted, Some(false));
    assert!(device.capabilities.volume.supported);
    assert!(device.volume_control().is_some());
}

#[tokio::test]
async fn volume_write_is_clamped_by_the_ceiling_and_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockStreamMagic::start(MockState::evo("0AABBCCDDEE0", "Living Room")).await;
    let device_id = h.attach("0AABBCCDDEE0", &mock).await;
    h.service
        .set_ceiling(device_id.as_str(), Some(50.0))
        .await
        .unwrap();

    let report = h
        .service
        .control(
            DeviceTarget::Device(device_id.clone()),
            DeviceRequest::VolumeAbsolute(90.0),
        )
        .await
        .expect("confirmed");
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(50.0));
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.volume.value, Some(50.0));

    let writes = mock.writes().await;
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert_eq!(writes[0]["path"], "/zone/state");
    assert_eq!(writes[0]["params"]["volume_percent"], 50);
}

#[tokio::test]
async fn mute_input_and_power_are_written_and_read_back() {
    let h = harness().await;
    let mock = MockStreamMagic::start(MockState::evo("0AABBCCDDEE0", "Living Room")).await;
    let device_id = h.attach("0AABBCCDDEE0", &mock).await;
    let target = || DeviceTarget::Device(device_id.clone());

    let report = h
        .service
        .control(target(), DeviceRequest::Mute(true))
        .await
        .unwrap();
    assert_eq!(report.observed.volume.is_muted, Some(true));

    let report = h
        .service
        .control(target(), DeviceRequest::SelectInput("IR".into()))
        .await
        .unwrap();
    assert_eq!(report.observed.input.as_deref(), Some("IR"));

    let report = h
        .service
        .control(target(), DeviceRequest::PowerOff)
        .await
        .unwrap();
    assert_eq!(report.observed.power, DevicePower::Standby);
    let report = h
        .service
        .control(target(), DeviceRequest::PowerOn)
        .await
        .unwrap();
    assert_eq!(report.observed.power, DevicePower::On);

    let writes = mock.writes().await;
    let params: Vec<&Value> = writes.iter().map(|w| &w["params"]).collect();
    assert_eq!(params[0]["mute"], true);
    assert_eq!(params[1]["source"], "IR");
    assert_eq!(writes[2]["path"], "/system/power");
    assert_eq!(
        params[2]["power"], "NETWORK",
        "standby keeps the network up"
    );
    assert_eq!(params[3]["power"], "ON");
}

#[tokio::test]
async fn remote_changes_reach_the_service_without_a_command() {
    let h = harness().await;
    let mock = MockStreamMagic::start(MockState::evo("0AABBCCDDEE0", "Living Room")).await;
    let device_id = h.attach("0AABBCCDDEE0", &mock).await;
    mock.remote_set_volume(61).await;
    h.wait_for(|device| device.volume.value == Some(61.0))
        .await_device(&h, &device_id)
        .await;
    assert!(mock.writes().await.is_empty());
}

/// Opening (and reopening) a session only asks and subscribes. A level sent on reconnect would
/// replay whatever UHC last cached onto a unit someone has since turned down.
#[tokio::test]
async fn critical_no_state_changing_frame_on_connect_or_reconnect() {
    let h = harness().await;
    let mock = MockStreamMagic::start(MockState::evo("0AABBCCDDEE0", "Living Room")).await;
    let device_id = h.attach("0AABBCCDDEE0", &mock).await;
    assert!(mock.writes().await.is_empty());

    mock.drop_connections();
    let deadline = Instant::now() + Duration::from_secs(8);
    while mock.snapshot().await.connections < 2 {
        assert!(Instant::now() < deadline, "the adapter never reconnected");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    mock.remote_set_volume(20).await;
    h.wait_for(|device| device.volume.value == Some(20.0))
        .await_device(&h, &device_id)
        .await;
    assert!(
        mock.writes().await.is_empty(),
        "writes after reconnect: {:?}",
        mock.writes().await
    );
    // Subscriptions were issued on both sessions.
    let frames = mock.snapshot().await.frames;
    let subscriptions = frames
        .iter()
        .filter(|f| f["params"].get("update") == Some(&json!(100)))
        .count();
    assert!(subscriptions >= 10, "{subscriptions}");
}

/// CRITICAL: two units of one model must never share a route.
#[tokio::test]
async fn critical_two_units_of_the_same_model_never_cross_route() {
    let h = harness().await;
    let a = MockStreamMagic::start(MockState::evo("0AAAAAAAAAA1", "Kitchen")).await;
    let b = MockStreamMagic::start(MockState::evo("0BBBBBBBBBB2", "Bedroom")).await;
    let a_id = h.attach("0AAAAAAAAAA1", &a).await;
    let b_id = h.attach("0BBBBBBBBBB2", &b).await;
    assert_ne!(a_id, b_id);
    assert_eq!(h.adapter.device_count().await, 2);
    h.service
        .set_ceiling(a_id.as_str(), Some(60.0))
        .await
        .unwrap();

    h.volume(&a_id, DeviceRequest::VolumeAbsolute(45.0))
        .await
        .unwrap();
    h.volume(&a_id, DeviceRequest::Mute(true)).await.unwrap();

    assert_eq!(a.writes().await.len(), 2);
    assert!(
        b.writes().await.is_empty(),
        "unit B received {:?}",
        b.writes().await
    );
    let b_device = h.device(&b_id).await;
    assert_eq!(b_device.volume.value, Some(35.0));
    assert_eq!(b_device.volume.is_muted, Some(false));
}

#[tokio::test]
async fn fixed_output_units_report_volume_unsupported_and_refuse_writes() {
    let h = harness().await;
    let mut fixed = MockState::evo("0CCCCCCCCCC3", "CXN");
    fixed.model = "CXN (V2)".to_string();
    fixed.pre_amp = false;
    let mock = MockStreamMagic::start(fixed).await;
    let device_id = h.attach("0CCCCCCCCCC3", &mock).await;

    let device = h.device(&device_id).await;
    assert_eq!(
        device.volume.value, None,
        "a fixed output has no volume to show"
    );
    assert!(!device.capabilities.volume.supported);
    assert!(device.volume_control().is_none());
    assert_eq!(device.volume.hardware_max, Some(80.0));
    // Mute and power are still honest.
    assert!(device.capabilities.mute.supported);

    // Through the service the request reaches the driver (an absolute level needs no current
    // value), and the driver refuses before any frame is built.
    h.service
        .set_ceiling(device_id.as_str(), Some(50.0))
        .await
        .unwrap();
    let error = h
        .volume(&device_id, DeviceRequest::VolumeAbsolute(10.0))
        .await
        .unwrap_err();
    assert!(
        matches!(&error, DeviceError::Dispatch { detail, .. } if detail.contains("not supported")),
        "{error:?}"
    );

    // And the driver refuses on its own if called directly.
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SetVolume(10.0))
        .await
        .unwrap_err();
    assert!(error.contains("not supported"), "{error}");
    assert!(mock.writes().await.is_empty());
}

#[tokio::test]
async fn a_rejected_command_errors_rather_than_succeeding() {
    let h = harness().await;
    let mock = MockStreamMagic::start(MockState::evo("0AABBCCDDEE0", "Living Room")).await;
    let device_id = h.attach("0AABBCCDDEE0", &mock).await;
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SelectInput("NOPE".into()))
        .await
        .unwrap_err();
    assert!(error.contains("refused"), "{error}");
    assert_eq!(
        h.device(&device_id).await.input.as_deref(),
        Some("MEDIA_PLAYER")
    );
}

#[tokio::test]
async fn a_host_that_is_not_a_streammagic_unit_is_not_published() {
    let h = harness().await;
    // Accepts TCP, never speaks WebSocket.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = listener.accept() => { drop(accepted); }
                }
            }
        });
    }
    let result = h.adapter.attach_endpoint("127.0.0.1", port).await;
    cancel.cancel();
    assert!(result.is_err());
    assert_eq!(h.adapter.device_count().await, 0);
}
