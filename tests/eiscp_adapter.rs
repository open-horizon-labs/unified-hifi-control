//! eISCP (Onkyo/Integra/Pioneer) device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The receiver is an in-file **synthetic** mock shaped from Onkyo's published ISCP spec v1.15
//! (eISCP framing, `PWR`/`MVL`/`AMT`/`SLI`, receiver `[EOF][CR][LF]` terminators) and the public
//! description of the UDP `ECNQSTN` discovery answer. It is not hardware evidence; every
//! capability claim stays `implementation-derived`.
//!
//! Two receivers share one model throughout, so a test that routes by model or "the only
//! Onkyo" fails here.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::devices::{DeviceFamily, FamilyContext};
use unified_hifi_control::adapters::eiscp::EiscpAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::{DeviceLinkStore, ManualDevice};
use unified_hifi_control::devices::service::{
    DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceId, DevicePower,
};

const FRONT: &str = "0009B04448E0";
const DEN: &str = "0009B04448E1";

// ---------------------------------------------------------------------------------------------
// Synthetic receiver
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MockState {
    power: bool,
    volume: u8,
    max_volume: u8,
    muted: bool,
    input: String,
    /// False: accept the TCP session but never answer anything.
    answers: bool,
    /// A receiver without network standby closes the session when it goes to standby.
    close_on_standby: bool,
    received: Vec<String>,
    connections: usize,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            power: true,
            volume: 40,
            max_volume: 100,
            muted: false,
            input: "23".to_string(),
            answers: true,
            close_on_standby: false,
            received: Vec::new(),
            connections: 0,
        }
    }
}

impl MockState {
    /// Every message that was not a query: what actually changed the receiver.
    fn writes(&self) -> Vec<String> {
        self.received
            .iter()
            .filter(|message| !message.ends_with("QSTN"))
            .cloned()
            .collect()
    }
}

struct MockReceiver {
    port: u16,
    state: Arc<Mutex<MockState>>,
    cancel: CancellationToken,
    /// Unsolicited messages pushed to every open session (the receiver's own remote).
    push: tokio::sync::broadcast::Sender<String>,
}

impl Drop for MockReceiver {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

/// A receiver-side frame: header, then `!1<message>` and the spec's `[EOF][CR][LF]`.
fn receiver_frame(message: &str) -> Vec<u8> {
    let data = format!("!1{message}\x1a\r\n");
    let mut frame = b"ISCP".to_vec();
    frame.extend_from_slice(&16u32.to_be_bytes());
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[1, 0, 0, 0]);
    frame.extend_from_slice(data.as_bytes());
    frame
}

/// Take one controller frame's message (`PWR01`) out of the buffer.
fn take_message(buffer: &mut Vec<u8>) -> Option<String> {
    if buffer.len() < 16 {
        return None;
    }
    let header = u32::from_be_bytes(buffer[4..8].try_into().ok()?) as usize;
    let size = u32::from_be_bytes(buffer[8..12].try_into().ok()?) as usize;
    if buffer.len() < header + size {
        return None;
    }
    let data = String::from_utf8_lossy(&buffer[header..header + size]).to_string();
    buffer.drain(..header + size);
    Some(data.trim_start_matches("!1").trim_end().to_string())
}

impl MockReceiver {
    async fn start() -> Self {
        Self::start_with(MockState::default(), "TX-NR709", FRONT).await
    }

    async fn start_with(state: MockState, model: &'static str, mac: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(state));
        let cancel = CancellationToken::new();
        let (push, _) = tokio::sync::broadcast::channel::<String>(16);
        {
            let push = push.clone();
            let state = state.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((mut stream, _)) = accepted else { break };
                            state.lock().await.connections += 1;
                            let state = state.clone();
                            let cancel = cancel.clone();
                            let mut pushed = push.subscribe();
                            tokio::spawn(async move {
                                let mut buffer = Vec::new();
                                let mut chunk = [0u8; 512];
                                loop {
                                    let read = tokio::select! {
                                        _ = cancel.cancelled() => return,
                                        message = pushed.recv() => {
                                            if let Ok(message) = message {
                                                if stream.write_all(&receiver_frame(&message)).await.is_err() {
                                                    return;
                                                }
                                            }
                                            continue;
                                        }
                                        read = stream.read(&mut chunk) => read,
                                    };
                                    let Ok(n) = read else { return };
                                    if n == 0 {
                                        return;
                                    }
                                    buffer.extend_from_slice(&chunk[..n]);
                                    while let Some(message) = take_message(&mut buffer) {
                                        let (replies, close) = handle(&state, &message).await;
                                        for reply in replies {
                                            if stream.write_all(&receiver_frame(&reply)).await.is_err() {
                                                return;
                                            }
                                        }
                                        if close {
                                            return;
                                        }
                                    }
                                }
                            });
                        }
                    }
                }
            });
        }
        // UDP discovery responder on the same port number.
        if let Ok(socket) = UdpSocket::bind(("127.0.0.1", port)).await {
            let cancel = cancel.clone();
            tokio::spawn(async move {
                let mut buffer = [0u8; 256];
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        received = socket.recv_from(&mut buffer) => {
                            let Ok((n, from)) = received else { break };
                            if buffer[16..n].starts_with(b"!xECNQSTN") {
                                let answer = receiver_frame(&format!("ECN{model}/{port}/XX/{mac}"));
                                if socket.send_to(&answer, from).await.is_err() {
                                    break;
                                }
                            }
                        }
                    }
                }
            });
        }
        Self {
            port,
            state,
            cancel,
            push,
        }
    }

    async fn snapshot(&self) -> MockState {
        self.state.lock().await.clone()
    }
}

/// `(replies, close the session afterwards)`.
async fn handle(state: &Arc<Mutex<MockState>>, message: &str) -> (Vec<String>, bool) {
    let mut state = state.lock().await;
    state.received.push(message.to_string());
    if !state.answers || message.len() < 5 {
        return (Vec::new(), false);
    }
    let (command, parameter) = message.split_at(3);
    let mut close = false;
    match (command, parameter) {
        ("PWR", "00") => {
            state.power = false;
            close = state.close_on_standby;
        }
        ("PWR", "01") => state.power = true,
        ("AMT", "00") => state.muted = false,
        ("AMT", "01") => state.muted = true,
        ("MVL", value) if value != "QSTN" => {
            if let Ok(level) = u8::from_str_radix(value, 16) {
                state.volume = level.min(state.max_volume);
            }
        }
        ("SLI", value) if value != "QSTN" => state.input = value.to_string(),
        _ => {}
    }
    let reply = match command {
        "PWR" => format!("PWR{:02X}", u8::from(state.power)),
        "AMT" => format!("AMT{:02X}", u8::from(state.muted)),
        "MVL" => format!("MVL{:02X}", state.volume),
        "SLI" => format!("SLI{}", state.input),
        _ => return (Vec::new(), false),
    };
    (vec![reply], close)
}

// ---------------------------------------------------------------------------------------------
// Harness (copied in shape from tests/device_safety.rs)
// ---------------------------------------------------------------------------------------------

struct Harness {
    aggregator: Arc<ZoneAggregator>,
    service: Arc<DeviceService>,
    adapter: Arc<EiscpAdapter>,
    _dir: tempfile::TempDir,
}

fn id(mac: &str) -> DeviceId {
    DeviceId::new("onkyo", mac).unwrap()
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
    let adapter = Arc::new(EiscpAdapter::new(bus));
    service.register_driver(adapter.clone()).await;
    Harness {
        aggregator,
        service,
        adapter,
        _dir: dir,
    }
}

impl Harness {
    async fn attach(&self, mac: &str, name: &str, mock: &MockReceiver) -> Device {
        self.adapter
            .attach_unit(&id(mac), "127.0.0.1", mock.port, "Onkyo", "TX-NR709", name)
            .await;
        self.observed(mac).await
    }

    /// Wait until the unit has been observed with a level.
    async fn observed(&self, mac: &str) -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(device) = self.aggregator.get_device(&id(mac)).await {
                if device.volume.value.is_some() && device.input.is_some() {
                    return device;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("device {mac} not observed within 5s");
    }

    async fn request(&self, mac: &str, request: DeviceRequest) -> Result<Device, DeviceError> {
        self.service
            .control(DeviceTarget::Device(id(mac)), request)
            .await
            .map(|report| report.observed)
    }
}

// ---------------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------------

#[tokio::test]
async fn device_is_observed_with_identity_scale_and_derived_claims() {
    let h = harness().await;
    let mock = MockReceiver::start().await;
    let device = h.attach(FRONT, "Den", &mock).await;

    assert_eq!(device.device_id.as_str(), "onkyo:0009B04448E0");
    assert_eq!(device.vendor, "Onkyo");
    assert_eq!(device.model, "TX-NR709");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.connection.control_channel, ControlChannel::Push);
    assert_eq!(device.input.as_deref(), Some("23"));
    assert_eq!(device.input_name(), Some("CD"));
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(device.volume.value, Some(40.0));
    assert_eq!(device.volume.min, 0.0);
    assert_eq!(device.volume.step, 1.0);
    assert_eq!(device.volume.is_muted, Some(false));
    assert_eq!(
        device.volume.hardware_max,
        Some(100.0),
        "the spec's full scale (MVL 0x64)"
    );
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
    }
    assert!(device.capabilities.power.detail.contains("Network Standby"));
}

/// The spec's full scale bounds a level with no ceiling; a ceiling only lowers it. Nothing above
/// the bound is ever sent.
#[tokio::test]
async fn volume_is_bounded_by_full_scale_and_lowered_by_a_ceiling() {
    let h = harness().await;
    let mock = MockReceiver::start().await;
    h.attach(FRONT, "Den", &mock).await;

    let observed = h
        .request(FRONT, DeviceRequest::VolumeAbsolute(250.0))
        .await
        .unwrap();
    assert_eq!(observed.volume.value, Some(100.0));
    assert_eq!(mock.snapshot().await.writes(), vec!["MVL64".to_string()]);

    h.service
        .set_ceiling(id(FRONT).as_str(), Some(50.0))
        .await
        .unwrap();
    let observed = h
        .request(FRONT, DeviceRequest::VolumeAbsolute(90.0))
        .await
        .expect("confirmed by readback");
    assert_eq!(observed.volume.value, Some(50.0));
    // 50 is 0x32 on the wire; nothing above the ceiling was ever sent.
    assert_eq!(
        mock.snapshot().await.writes(),
        vec!["MVL64".to_string(), "MVL32".to_string()]
    );
    // The write is followed by a readback query (after the receiver's 50 ms settling gap).
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let received = mock.snapshot().await.received;
        if received.ends_with(&["MVL32".to_string(), "MVLQSTN".to_string()]) {
            break;
        }
        assert!(Instant::now() < deadline, "no readback query: {received:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // A level inside the ceiling goes through exactly.
    let observed = h
        .request(FRONT, DeviceRequest::VolumeAbsolute(20.0))
        .await
        .unwrap();
    assert_eq!(observed.volume.value, Some(20.0));
    assert_eq!(mock.snapshot().await.writes().last().unwrap(), "MVL14");
}

/// Models with a 0-80 range cap the level themselves; the readback reports the truth.
#[tokio::test]
async fn a_receiver_that_caps_the_level_is_reported_truthfully() {
    let h = harness().await;
    let mock = MockReceiver::start_with(
        MockState {
            max_volume: 0x50,
            ..MockState::default()
        },
        "TX-NR709",
        FRONT,
    )
    .await;
    h.attach(FRONT, "Den", &mock).await;
    h.service
        .set_ceiling(id(FRONT).as_str(), Some(95.0))
        .await
        .unwrap();
    let report = h
        .service
        .control(
            DeviceTarget::Device(id(FRONT)),
            DeviceRequest::VolumeAbsolute(95.0),
        )
        .await
        .unwrap();
    assert_eq!(report.observed.volume.value, Some(80.0));
}

#[tokio::test]
async fn critical_no_state_changing_bytes_on_connect_or_reconnect() {
    let h = harness().await;
    let mock = MockReceiver::start().await;
    h.attach(FRONT, "Den", &mock).await;
    let state = mock.snapshot().await;
    assert!(
        state.writes().is_empty(),
        "connect sent {:?}",
        state.writes()
    );
    for query in ["PWRQSTN", "MVLQSTN", "AMTQSTN", "SLIQSTN"] {
        assert!(state.received.iter().any(|m| m == query), "missing {query}");
    }

    // Reconnect: stop the family (drops the session), attach again.
    h.adapter.stop_family().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.attach(FRONT, "Den", &mock).await;
    let state = mock.snapshot().await;
    assert!(state.connections >= 2, "expected a second session");
    assert!(
        state.writes().is_empty(),
        "reconnect sent {:?}",
        state.writes()
    );
}

#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let front = MockReceiver::start().await;
    let den = MockReceiver::start_with(MockState::default(), "TX-NR709", DEN).await;
    let front_device = h.attach(FRONT, "Front", &front).await;
    let den_device = h.attach(DEN, "Den", &den).await;
    assert_ne!(front_device.device_id, den_device.device_id);
    assert_eq!(front_device.model, den_device.model);

    h.service
        .set_ceiling(id(FRONT).as_str(), Some(60.0))
        .await
        .unwrap();
    h.request(FRONT, DeviceRequest::VolumeAbsolute(30.0))
        .await
        .unwrap();
    h.request(FRONT, DeviceRequest::Mute(true)).await.unwrap();

    assert_eq!(
        front.snapshot().await.writes(),
        vec!["MVL1E".to_string(), "AMT01".to_string()]
    );
    let den_state = den.snapshot().await;
    assert!(
        den_state.writes().is_empty(),
        "den received {:?}",
        den_state.writes()
    );
    assert_eq!(den_state.volume, 40);
    assert!(!den_state.muted);
    // ...and the aggregator still holds the den at its own level.
    let den_now = h.aggregator.get_device(&id(DEN)).await.unwrap();
    assert_eq!(den_now.volume.value, Some(40.0));
}

#[tokio::test]
async fn power_mute_and_input_round_trip() {
    let h = harness().await;
    let mock = MockReceiver::start().await;
    h.attach(FRONT, "Den", &mock).await;

    let observed = h
        .request(FRONT, DeviceRequest::SelectInput("23".to_string()))
        .await
        .unwrap();
    assert_eq!(observed.input.as_deref(), Some("23"));
    let observed = h
        .request(FRONT, DeviceRequest::SelectInput("2a".to_ascii_uppercase()))
        .await
        .unwrap();
    assert_eq!(observed.input.as_deref(), Some("2A"));
    assert_eq!(mock.snapshot().await.input, "2A");

    let observed = h.request(FRONT, DeviceRequest::Mute(true)).await.unwrap();
    assert_eq!(observed.volume.is_muted, Some(true));
    let observed = h.request(FRONT, DeviceRequest::Mute(false)).await.unwrap();
    assert_eq!(observed.volume.is_muted, Some(false));

    let observed = h.request(FRONT, DeviceRequest::PowerOff).await.unwrap();
    assert_eq!(observed.power, DevicePower::Standby);
    let observed = h.request(FRONT, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(observed.power, DevicePower::On);
    assert_eq!(
        mock.snapshot().await.writes(),
        ["SLI23", "SLI2A", "AMT01", "AMT00", "PWR00", "PWR01"]
    );
}

/// A change made with the receiver's own remote is pushed and observed without polling.
#[tokio::test]
async fn unsolicited_status_is_observed() {
    let h = harness().await;
    let mock = MockReceiver::start().await;
    h.attach(FRONT, "Den", &mock).await;
    for message in ["MVL37", "AMT01", "SLI10"] {
        mock.push.send(message.to_string()).unwrap();
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        let device = h.aggregator.get_device(&id(FRONT)).await.unwrap();
        if device.volume.value == Some(55.0)
            && device.volume.is_muted == Some(true)
            && device.input.as_deref() == Some("10")
        {
            break;
        }
        assert!(Instant::now() < deadline, "push not observed: {device:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(mock.snapshot().await.writes().is_empty());
}

/// Unsupported or malformed operations error at the driver rather than succeeding or reaching
/// the receiver.
#[tokio::test]
async fn unsupported_operations_error_instead_of_succeeding() {
    let h = harness().await;
    let mock = MockReceiver::start().await;
    h.attach(FRONT, "Den", &mock).await;

    let driver: &dyn DeviceDriver = h.adapter.as_ref();
    for operation in [
        DeviceOperation::SelectInput("ZZ".to_string()),
        DeviceOperation::SelectInput("CD2".to_string()),
        DeviceOperation::SetVolume(101.0),
        DeviceOperation::SetVolume(-1.0),
    ] {
        let error = driver
            .execute(&id(FRONT), operation.clone())
            .await
            .expect_err(&format!("{operation:?} must be refused"));
        assert!(!error.is_empty());
    }
    assert!(mock.snapshot().await.writes().is_empty());

    // The service refuses an input outside the catalog before the driver is involved.
    let error = h
        .request(FRONT, DeviceRequest::SelectInput("TV AUDIO".to_string()))
        .await
        .unwrap_err();
    assert!(
        matches!(error, DeviceError::UnknownInput { .. }),
        "{error:?}"
    );
    assert!(mock.snapshot().await.writes().is_empty());

    // Not this adapter's unit.
    let stranger = DeviceId::new("onkyo", "0009B0FFFFFF").unwrap();
    assert!(driver
        .execute(&stranger, DeviceOperation::SetMute(true))
        .await
        .is_err());
}

/// A receiver in standby with network control off refuses the TCP session: power_on must say
/// so rather than pretend.
#[tokio::test]
async fn power_on_of_an_unreachable_receiver_errors_clearly() {
    let h = harness().await;
    // A port nothing listens on.
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    h.adapter
        .attach_unit(&id(FRONT), "127.0.0.1", closed, "Onkyo", "TX-NR709", "Den")
        .await;
    let driver: &dyn DeviceDriver = h.adapter.as_ref();
    let error = driver
        .execute(&id(FRONT), DeviceOperation::PowerOn)
        .await
        .unwrap_err();
    assert!(error.contains("not reachable"), "{error}");
    assert!(error.contains("Network Standby"), "{error}");
    assert!(error.contains("Wake-on-LAN"), "{error}");
    // Never observed, so never published.
    assert!(h.aggregator.get_device(&id(FRONT)).await.is_none());
}

/// A unit is published only after its session has answered; one that accepts TCP and says
/// nothing is not a device.
#[tokio::test]
async fn a_silent_listener_is_never_published() {
    let h = harness().await;
    let mock = MockReceiver::start_with(
        MockState {
            answers: false,
            ..MockState::default()
        },
        "TX-NR709",
        FRONT,
    )
    .await;
    h.adapter
        .attach_unit(
            &id(FRONT),
            "127.0.0.1",
            mock.port,
            "Onkyo",
            "TX-NR709",
            "Den",
        )
        .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(mock.snapshot().await.connections >= 1);
    assert!(h.aggregator.get_device(&id(FRONT)).await.is_none());
}

/// A receiver without network standby drops the session after power-off. That is standby, not a
/// failure, and the confirmation must say so.
#[tokio::test]
async fn power_off_that_closes_the_session_is_confirmed_as_standby() {
    let h = harness().await;
    let mock = MockReceiver::start_with(
        MockState {
            close_on_standby: true,
            ..MockState::default()
        },
        "TX-NR709",
        FRONT,
    )
    .await;
    h.attach(FRONT, "Den", &mock).await;
    let observed = h.request(FRONT, DeviceRequest::PowerOff).await.unwrap();
    assert_eq!(observed.power, DevicePower::Standby);
    // The session then closes; the inference is recorded for anyone reading the detail.
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let device = h.aggregator.get_device(&id(FRONT)).await.unwrap();
        if device
            .connection
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("inferred"))
        {
            assert_eq!(device.power, DevicePower::Standby);
            assert_eq!(device.connection.control_channel, ControlChannel::None);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "standby inference never recorded"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Discovery through a configured host: the UDP `ECNQSTN` answer supplies model and MAC, and the
/// unit is then controlled over TCP. (Broadcast and SSDP also run and find nothing here.)
#[tokio::test]
async fn manual_host_is_discovered_through_ecn_and_controlled() {
    let h = harness().await;
    let mock = MockReceiver::start_with(MockState::default(), "TX-NR709", DEN).await;
    h.adapter
        .start_family(FamilyContext {
            manual: vec![ManualDevice {
                family: "eiscp".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some(mock.port),
                mac: None,
            }],
        })
        .await
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(25);
    let device = loop {
        if let Some(device) = h.aggregator.get_device(&id(DEN)).await {
            break device;
        }
        assert!(Instant::now() < deadline, "manual device never discovered");
        tokio::time::sleep(Duration::from_millis(100)).await;
    };
    assert_eq!(device.model, "TX-NR709");
    assert_eq!(device.discovery, "manual");
    assert_eq!(device.volume.value, Some(40.0));
    h.adapter.stop_family().await;
}
