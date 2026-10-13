//! Hegel device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The amplifier is `MockHegel`, a **synthetic** server shaped from `hegel-ip-client`'s README and
//! `client.py` (CR-terminated `-p.1`, `-v.50`, `-m.0`, `-i.3`; replies echo the new value; queries
//! end `.?`; state changes are pushed to every client). It is not hardware evidence: no Hegel unit
//! was available. Every byte it receives is recorded, so a test can assert what was (and was not)
//! sent.
//!
//! Tests named `critical_*` guard a physical failure mode: a level sent to the wrong amplifier, a
//! level above the user's ceiling, a state change sent on connect.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{broadcast, Mutex};
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::devices::{DeviceFamily, FamilyContext};
use unified_hifi_control::adapters::hegel::protocol::Description;
use unified_hifi_control::adapters::hegel::HegelAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, SharedBus, VolumeScale};
use unified_hifi_control::devices::links::{DeviceLinkStore, ManualDevice};
use unified_hifi_control::devices::service::{
    Confirmation, DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService,
    DeviceTarget,
};
use unified_hifi_control::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceId, DevicePower,
};

// =============================================================================
// Mock amplifier
// =============================================================================

#[derive(Debug, Clone)]
struct MockState {
    power: bool,
    volume: u8,
    muted: bool,
    input: u8,
    /// Every line received, in order, including queries.
    received: Vec<String>,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            power: true,
            volume: 30,
            muted: false,
            input: 3,
            received: Vec::new(),
        }
    }
}

impl MockState {
    /// Lines that change state (not queries), which is what a safety test inspects.
    fn writes(&self) -> Vec<String> {
        self.received
            .iter()
            .filter(|line| !line.ends_with('?'))
            .cloned()
            .collect()
    }

    /// Reply to one line, and whether it was a write.
    fn handle(&mut self, line: &str) -> Option<String> {
        self.received.push(line.to_string());
        let (key, value) = line.strip_prefix('-')?.split_once('.')?;
        match (key, value) {
            ("p", "?") => {}
            ("p", "1") => self.power = true,
            ("p", "0") => self.power = false,
            ("v", "?") => {}
            ("v", number) => self.volume = number.parse::<u8>().ok()?.min(100),
            ("m", "?") => {}
            ("m", "1") => self.muted = true,
            ("m", "0") => self.muted = false,
            ("i", "?") => {}
            ("i", number) => self.input = number.parse().ok()?,
            _ => return None,
        }
        Some(match key {
            "p" => format!("-p.{}\r", u8::from(self.power)),
            "v" => format!("-v.{}\r", self.volume),
            "m" => format!("-m.{}\r", u8::from(self.muted)),
            _ => format!("-i.{}\r", self.input),
        })
    }
}

struct MockHegel {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    push: broadcast::Sender<String>,
    cancel: CancellationToken,
    /// Cancelling this drops every open connection but keeps the listener (a unit rebooting its
    /// network stack).
    drop_connections: broadcast::Sender<()>,
}

impl MockHegel {
    async fn start() -> Self {
        Self::start_with(MockState::default()).await
    }

    async fn start_with(initial: MockState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(initial));
        let (push, _) = broadcast::channel(16);
        let (drop_connections, _) = broadcast::channel(4);
        let cancel = CancellationToken::new();
        {
            let state = state.clone();
            let push = push.clone();
            let drop_connections = drop_connections.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            tokio::spawn(serve(
                                stream,
                                state.clone(),
                                push.subscribe(),
                                drop_connections.subscribe(),
                                cancel.clone(),
                            ));
                        }
                    }
                }
            });
        }
        Self {
            addr,
            state,
            push,
            cancel,
            drop_connections,
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    async fn snapshot(&self) -> MockState {
        self.state.lock().await.clone()
    }

    /// The front-panel knob or the unit's remote: change the level and tell every client.
    async fn remote_set_volume(&self, level: u8) {
        self.state.lock().await.volume = level;
        if self.push.send(format!("-v.{level}\r")).is_err() {
            // No client is listening; nothing to push to.
        }
    }

    fn drop_connections(&self) {
        if self.drop_connections.send(()).is_err() {
            // No connection is open.
        }
    }

    /// The unit goes away entirely: connections drop and the port stops accepting.
    fn shutdown(&self) {
        self.cancel.cancel();
    }
}

async fn serve(
    mut stream: TcpStream,
    state: Arc<Mutex<MockState>>,
    mut push: broadcast::Receiver<String>,
    mut drop_connections: broadcast::Receiver<()>,
    cancel: CancellationToken,
) {
    let mut buffer = String::new();
    let mut chunk = [0u8; 256];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = drop_connections.recv() => break,
            pushed = push.recv() => {
                let Ok(line) = pushed else { continue };
                if stream.write_all(line.as_bytes()).await.is_err() {
                    break;
                }
            }
            read = stream.read(&mut chunk) => {
                let Ok(n) = read else { break };
                if n == 0 {
                    break;
                }
                buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
                while let Some(end) = buffer.find('\r') {
                    let line: String = buffer.drain(..=end).collect();
                    let line = line.trim().to_string();
                    let reply = state.lock().await.handle(&line);
                    if let Some(reply) = reply {
                        if stream.write_all(reply.as_bytes()).await.is_err() {
                            return;
                        }
                    }
                }
            }
        }
    }
}

// =============================================================================
// Harness
// =============================================================================

struct Harness {
    service: Arc<DeviceService>,
    aggregator: Arc<ZoneAggregator>,
    adapter: Arc<HegelAdapter>,
    _bus: SharedBus,
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
    let adapter = Arc::new(HegelAdapter::new(bus.clone()));
    service.register_driver(adapter.clone()).await;
    Harness {
        service,
        aggregator,
        adapter,
        _bus: bus,
        _dir: dir,
    }
}

fn description(model: &str, serial: &str) -> Description {
    Description {
        manufacturer: "Hegel".to_string(),
        model: model.to_string(),
        friendly_name: format!("Hegel {model} {serial}"),
        serial: Some(serial.to_string()),
        udn: None,
    }
}

fn id(serial: &str) -> DeviceId {
    DeviceId::new("hegel", serial).unwrap()
}

impl Harness {
    async fn attach(&self, model: &str, serial: &str, mock: &MockHegel) -> DeviceId {
        let device_id = id(serial);
        self.adapter
            .attach_unit(
                &device_id,
                "127.0.0.1",
                mock.port(),
                description(model, serial),
            )
            .await;
        // Ready means the unit has reported a level (the first full round of queries is in).
        self.wait_for(
            |device| device.volume.value.is_some() && device.input.is_some(),
            &device_id,
        )
        .await;
        device_id
    }

    async fn device(&self, device_id: &DeviceId) -> Option<Device> {
        self.aggregator.get_device(device_id).await
    }

    async fn wait_for(&self, condition: impl Fn(&Device) -> bool, device_id: &DeviceId) -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(device) = self.device(device_id).await {
                if condition(&device) {
                    return device;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within 5s for {device_id}");
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

    async fn set_ceiling(&self, device_id: &DeviceId, ceiling: f32) {
        self.service
            .set_ceiling(device_id.as_str(), Some(ceiling))
            .await
            .unwrap();
    }
}

fn highest_level_written(writes: &[String]) -> Option<f32> {
    writes
        .iter()
        .filter_map(|line| line.strip_prefix("-v."))
        .filter_map(|digits| digits.parse::<f32>().ok())
        .reduce(f32::max)
}

// =============================================================================
// Identity, scale and capabilities
// =============================================================================

#[tokio::test]
async fn device_is_observed_with_serial_identity_and_percentage_scale() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;

    let device = h.device(&device_id).await.unwrap();
    assert_eq!(device.device_id.as_str(), "hegel:H1900042");
    assert_eq!(device.device_id.kind(), "hegel");
    assert_eq!(device.vendor, "Hegel");
    assert_eq!(device.model, "H190");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.connection.control_channel, ControlChannel::Push);
    assert_eq!(device.input.as_deref(), Some("3"));
    assert_eq!(
        device.input_name(),
        Some("Analog 2"),
        "H190 input 3 per the model table"
    );
    assert_eq!(device.inputs.len(), 9);

    let volume = &device.volume;
    assert_eq!(volume.scale, VolumeScale::Percentage);
    assert_eq!(volume.value, Some(30.0));
    assert_eq!(volume.min, 0.0);
    assert_eq!(volume.step, 1.0);
    assert_eq!(
        volume.hardware_max,
        Some(100.0),
        "the protocol's full scale"
    );
    assert_eq!(volume.is_muted, Some(false));
    let control = device
        .volume_control()
        .expect("full scale bounds the range");
    assert_eq!((control.min, control.max), (0.0, 100.0));

    for capability in [
        &device.capabilities.observe,
        &device.capabilities.volume,
        &device.capabilities.mute,
        &device.capabilities.power,
        &device.capabilities.input_select,
    ] {
        assert!(capability.supported);
        assert_eq!(
            capability.evidence,
            CapabilityEvidence::ImplementationDerived
        );
        assert!(!capability.detail.is_empty());
    }
}

#[tokio::test]
async fn a_ceiling_lowers_the_published_maximum() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;
    h.set_ceiling(&device_id, 60.0).await;

    let device = h.device(&device_id).await.unwrap();
    let control = device.volume_control().expect("range stays bounded");
    assert_eq!((control.min, control.max, control.step), (0.0, 60.0, 1.0));
    assert_eq!(control.scale, VolumeScale::Percentage);
    assert_eq!(control.value, 30.0);
}

#[tokio::test]
async fn uncovered_model_is_published_with_a_caveat_and_no_input_list() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H600", "H600-1", &mock).await;

    let device = h.device(&device_id).await.unwrap();
    assert!(device.inputs.is_empty());
    assert!(!device.capabilities.input_select.supported);
    assert!(device.capabilities.volume.supported);
    assert!(
        device
            .capabilities
            .volume
            .detail
            .contains("not one of the covered"),
        "{}",
        device.capabilities.volume.detail
    );
}

// =============================================================================
// Commands: confirmed by readback
// =============================================================================

#[tokio::test]
async fn power_mute_and_input_are_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;

    let report = h
        .control(&device_id, DeviceRequest::Mute(true))
        .await
        .unwrap();
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.volume.is_muted, Some(true));

    // By display name: the service resolves it to the wire code.
    let report = h
        .control(
            &device_id,
            DeviceRequest::SelectInput("Optical 1".to_string()),
        )
        .await
        .unwrap();
    assert_eq!(report.observed.input.as_deref(), Some("5"));

    let report = h
        .control(&device_id, DeviceRequest::PowerOff)
        .await
        .unwrap();
    assert_eq!(report.observed.power, DevicePower::Standby);
    let report = h.control(&device_id, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(report.observed.power, DevicePower::On);

    assert_eq!(
        mock.snapshot().await.writes(),
        vec!["-m.1", "-i.5", "-p.0", "-p.1"]
    );
}

/// CRITICAL: the user's ceiling bounds the level from the service, and the amplifier is only
/// ever sent the clamped value, which the readback then confirms.
#[tokio::test]
async fn critical_volume_is_clamped_by_the_ceiling_and_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;
    h.set_ceiling(&device_id, 40.0).await;

    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(90.0))
        .await
        .unwrap();
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(40.0));
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.volume.value, Some(40.0));

    // A relative climb from the ceiling cannot pass it either.
    h.control(&device_id, DeviceRequest::VolumeRelative(15.0))
        .await
        .unwrap();

    let snapshot = mock.snapshot().await;
    assert_eq!(snapshot.volume, 40);
    let highest = highest_level_written(&snapshot.writes()).expect("levels were sent");
    assert!(
        highest <= 40.0,
        "a level above the ceiling reached the amplifier: {highest}"
    );
}

/// CRITICAL: with no ceiling the protocol's full scale is the limit, so a level past it is
/// clamped to 100 and never sent as anything larger; a ceiling lowers it from there.
#[tokio::test]
async fn critical_levels_never_exceed_the_protocol_maximum_or_the_ceiling() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;

    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(250.0))
        .await
        .unwrap();
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(100.0));
    h.set_ceiling(&device_id, 70.0).await;
    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(100.0))
        .await
        .unwrap();
    assert_eq!(report.sent_volume, Some(70.0));
    let highest = highest_level_written(&mock.snapshot().await.writes()).unwrap();
    assert!(highest <= 100.0);
    assert_eq!(mock.snapshot().await.volume, 70);
}

#[tokio::test]
async fn noop_write_is_confirmed_against_a_fresh_readback() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;
    h.set_ceiling(&device_id, 60.0).await;

    let started = Instant::now();
    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(30.0))
        .await
        .unwrap();
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    // The confirming query follows the command after a short gap; the echo already confirmed.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let received = mock.snapshot().await.received;
    assert!(
        received.ends_with(&["-v.30".to_string(), "-v.?".to_string()]),
        "the amplifier was asked, not answered from cache: {received:?}"
    );
}

// =============================================================================
// Safety
// =============================================================================

/// CRITICAL: connecting, and reconnecting after the session drops, only *asks*. A level or power
/// command replayed from cache onto an amplifier someone has since turned down is the failure.
#[tokio::test]
async fn critical_no_state_changing_bytes_on_connect_or_reconnect() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;
    h.set_ceiling(&device_id, 60.0).await;
    h.control(&device_id, DeviceRequest::VolumeAbsolute(35.0))
        .await
        .unwrap();
    let before = mock.snapshot().await.received.len();

    // The amplifier drops the session, then someone changes it at the unit.
    mock.drop_connections();
    h.wait_for(
        |device| device.connection.control_channel == ControlChannel::None,
        &device_id,
    )
    .await;
    mock.state.lock().await.volume = 12;
    let device = h
        .wait_for(|device| device.volume.value == Some(12.0), &device_id)
        .await;
    assert_eq!(device.connection.control_channel, ControlChannel::Push);

    let state = mock.snapshot().await;
    let after_connect: Vec<_> = state.received[before..].to_vec();
    assert!(
        after_connect.iter().all(|line| line.ends_with('?')),
        "reconnect sent state-changing lines: {after_connect:?}"
    );
    assert!(after_connect.iter().any(|line| line == "-v.?"));
    assert_eq!(state.volume, 12, "the remote's level was not overwritten");
    // And the very first connection sent only queries before UHC's own write.
    let first: Vec<_> = state.received[..before].iter().take(4).cloned().collect();
    assert_eq!(first, vec!["-p.?", "-v.?", "-m.?", "-i.?"]);
}

/// CRITICAL: two amplifiers of one model never cross-route. Fails any routing by model,
/// discovery order or "the only Hegel".
#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let living = MockHegel::start().await;
    let study = MockHegel::start_with(MockState {
        volume: 20,
        ..MockState::default()
    })
    .await;
    let living_id = h.attach("H190", "H190-0001", &living).await;
    let study_id = h.attach("H190", "H190-0002", &study).await;
    assert_ne!(living_id, study_id);
    h.set_ceiling(&living_id, 50.0).await;
    h.set_ceiling(&study_id, 50.0).await;

    h.control(&living_id, DeviceRequest::VolumeAbsolute(45.0))
        .await
        .unwrap();
    h.control(&living_id, DeviceRequest::Mute(true))
        .await
        .unwrap();

    assert_eq!(living.snapshot().await.writes(), vec!["-v.45", "-m.1"]);
    let study_state = study.snapshot().await;
    assert!(
        study_state.writes().is_empty(),
        "the other amplifier received {:?}",
        study_state.writes()
    );
    assert_eq!(study_state.volume, 20);
    assert!(!study_state.muted);
    let study_device = h.device(&study_id).await.unwrap();
    assert_eq!(study_device.volume.value, Some(20.0));
    assert_eq!(study_device.volume.is_muted, Some(false));
}

/// An operation the device cannot do errors rather than succeeding: input selection on a model
/// whose input numbering is unknown, and a level the protocol cannot express.
#[tokio::test]
async fn unsupported_operations_error_instead_of_succeeding() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H600", "H600-1", &mock).await;

    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SelectInput("1".to_string()))
        .await
        .expect_err("numbering is model specific");
    assert!(error.contains("not supported"), "{error}");
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SetVolume(101.0))
        .await
        .expect_err("outside 0-100");
    assert!(error.contains("cannot be encoded"), "{error}");
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SetVolume(33.5))
        .await
        .expect_err("not whole percent");
    assert!(error.contains("cannot be encoded"), "{error}");

    // Through the service the same refusal surfaces as an error, not a confirmation.
    let outcome = h
        .control(&device_id, DeviceRequest::SelectInput("1".to_string()))
        .await;
    assert!(outcome.is_err(), "{outcome:?}");
    assert!(mock.snapshot().await.writes().is_empty());
}

#[tokio::test]
async fn unknown_device_is_not_routed_anywhere() {
    let h = harness().await;
    let error = h
        .adapter
        .execute(&id("NOSUCHUNIT"), DeviceOperation::PowerOn)
        .await
        .expect_err("no such unit");
    assert!(error.contains("not connected"), "{error}");
}

// =============================================================================
// Readback and lifecycle
// =============================================================================

#[tokio::test]
async fn a_remote_change_is_pushed_and_observed() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;

    let started = Instant::now();
    mock.remote_set_volume(55).await;
    h.wait_for(|device| device.volume.value == Some(55.0), &device_id)
        .await;
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
}

/// An unreachable unit's last readings are not facts: nothing shows a stale level.
#[tokio::test]
async fn an_unreachable_unit_does_not_keep_publishing_stale_state() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;
    h.set_ceiling(&device_id, 60.0).await;
    assert!(h
        .device(&device_id)
        .await
        .unwrap()
        .volume_control()
        .is_some());

    mock.shutdown();
    let device = h
        .wait_for(
            |device| device.connection.control_channel == ControlChannel::None,
            &device_id,
        )
        .await;
    assert_eq!(
        device.power,
        DevicePower::Unknown,
        "never rounded to standby"
    );
    assert_eq!(device.volume.value, None);
    assert_eq!(device.volume.is_muted, None);
    assert!(device.volume_control().is_none());
    assert!(device.connection.detail.is_some());

    // A command to it fails fast and loudly.
    let outcome = h.control(&device_id, DeviceRequest::PowerOn).await;
    assert!(outcome.is_err(), "{outcome:?}");
}

#[tokio::test]
async fn stopping_the_adapter_removes_its_devices() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    let device_id = h.attach("H190", "H190-0042", &mock).await;
    h.adapter.stop_family().await;

    let deadline = Instant::now() + Duration::from_secs(3);
    while h.device(&device_id).await.is_some() {
        assert!(Instant::now() < deadline, "device was not removed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(h.adapter.device_count().await, 0);
}

/// A configured unit with a known `mac` is probed over the control protocol and published with an
/// id from that address. (Synthetic: multicast and unicast SSDP find nothing here, which is the
/// case this path exists for.)
#[tokio::test]
async fn a_configured_unit_with_a_mac_is_probed_then_published() {
    let h = harness().await;
    let mock = MockHegel::start().await;
    h.adapter
        .start_family(FamilyContext {
            manual: vec![ManualDevice {
                family: "hegel".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some(mock.port()),
                mac: Some("00:0A:0B:0C:0D:0E".to_string()),
            }],
        })
        .await
        .unwrap();

    let device_id = DeviceId::new("hegel", "000A0B0C0D0E").unwrap();
    let device = h.wait_for_slow(&device_id, Duration::from_secs(20)).await;
    assert_eq!(device.discovery, "manual");
    assert_eq!(device.vendor, "Hegel");
    assert!(
        device.inputs.is_empty(),
        "no description, so no model, so no input list"
    );
    assert_eq!(device.volume.value, Some(30.0));
    // Probing asked `-p.?`; nothing it sent changed state.
    assert!(mock.snapshot().await.writes().is_empty());
}

#[tokio::test]
async fn a_configured_host_that_is_not_a_hegel_is_never_published() {
    let h = harness().await;
    // A listener that accepts and says nothing: not the control protocol.
    let silent = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = silent.local_addr().unwrap().port();
    let cancel = CancellationToken::new();
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    accepted = silent.accept() => {
                        if let Ok((stream, _)) = accepted {
                            held.push(stream);
                        }
                    }
                }
            }
        });
    }
    h.adapter
        .start_family(FamilyContext {
            manual: vec![ManualDevice {
                family: "hegel".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some(port),
                mac: Some("00:0A:0B:0C:0D:0F".to_string()),
            }],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(10)).await;
    assert_eq!(h.adapter.device_count().await, 0);
    cancel.cancel();
    h.adapter.stop_family().await;
}

impl Harness {
    async fn wait_for_slow(&self, device_id: &DeviceId, limit: Duration) -> Device {
        let deadline = Instant::now() + limit;
        while Instant::now() < deadline {
            if let Some(device) = self.device(device_id).await {
                if device.volume.value.is_some() {
                    return device;
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        panic!("{device_id} was not published within {limit:?}");
    }
}
