//! Arcam device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The unit is an in-file **synthetic** mock shaped from the frame layout and command tables of
//! the MIT-licensed `arcam_fmj` client (`STR zone cc len data ETR` commands, `STR zone cc ac len
//! data ETR` answers, direct writes on SA/PA/ST series, simulated RC5 on AVR series, `0x85`
//! "invalid at this time" for state queries while in standby, the AMX discovery answer). It is
//! not hardware evidence; every capability claim stays `implementation-derived`.
//!
//! Two units share one model throughout, so a test that routes by model or "the only Arcam"
//! fails here.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::arcam::ArcamAdapter;
use unified_hifi_control::adapters::devices::{DeviceFamily, FamilyContext};
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::{DeviceLinkStore, ManualDevice};
use unified_hifi_control::devices::service::{
    DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceId, DevicePower,
};

const STUDY: &str = "00226B1C4D5E";
const DEN: &str = "00226B1C4D5F";

// ---------------------------------------------------------------------------------------------
// Synthetic unit
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct MockState {
    power: bool,
    volume: u8,
    muted: bool,
    /// `CURRENT_SOURCE` byte.
    source: u8,
    answers: bool,
    /// Older models drop the session on standby.
    close_on_standby: bool,
    amx_model: &'static str,
    /// Every non-query frame received, whole.
    writes: Vec<Vec<u8>>,
    /// Every frame received, whole (queries included).
    frames: Vec<Vec<u8>>,
    amx_requests: usize,
    connections: usize,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            power: true,
            volume: 40,
            muted: false,
            source: 0x06, // CD on the SA series
            answers: true,
            close_on_standby: false,
            amx_model: "SA30",
            writes: Vec::new(),
            frames: Vec::new(),
            amx_requests: 0,
            connections: 0,
        }
    }
}

struct MockArcam {
    port: u16,
    state: Arc<Mutex<MockState>>,
    cancel: CancellationToken,
}

impl Drop for MockArcam {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn answer(cc: u8, data: &[u8]) -> Vec<u8> {
    let mut frame = vec![0x21, 0x01, cc, 0x00, data.len() as u8];
    frame.extend_from_slice(data);
    frame.push(0x0D);
    frame
}

fn refusal(cc: u8, code: u8) -> Vec<u8> {
    vec![0x21, 0x01, cc, code, 0x00, 0x0D]
}

/// Status frame for one command code from the current state; `0x85` for everything but power
/// while the unit is in standby.
fn status(state: &MockState, cc: u8) -> Option<Vec<u8>> {
    if cc != 0x00 && !state.power {
        return Some(refusal(cc, 0x85));
    }
    match cc {
        0x00 => Some(answer(cc, &[u8::from(state.power)])),
        0x0D => Some(answer(cc, &[state.volume])),
        0x0E => Some(answer(cc, &[if state.muted { 0 } else { 1 }])),
        0x1D => Some(answer(cc, &[state.source])),
        _ => None,
    }
}

impl MockArcam {
    async fn start() -> Self {
        Self::start_with(MockState::default()).await
    }

    async fn start_with(state: MockState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(Mutex::new(state));
        let cancel = CancellationToken::new();
        let accept_state = state.clone();
        let accept_cancel = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = accept_cancel.cancelled() => break,
                    accepted = listener.accept() => {
                        let Ok((mut stream, _)) = accepted else { break };
                        accept_state.lock().await.connections += 1;
                        let state = accept_state.clone();
                        let cancel = accept_cancel.clone();
                        tokio::spawn(async move {
                            let mut buffer: Vec<u8> = Vec::new();
                            let mut chunk = [0u8; 256];
                            loop {
                                let read = tokio::select! {
                                    _ = cancel.cancelled() => return,
                                    read = stream.read(&mut chunk) => read,
                                };
                                let Ok(n) = read else { return };
                                if n == 0 {
                                    return;
                                }
                                buffer.extend_from_slice(&chunk[..n]);
                                while let Some(frame) = take_frame(&mut buffer) {
                                    let (replies, close) = handle(&state, &frame).await;
                                    for reply in replies {
                                        if stream.write_all(&reply).await.is_err() {
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
        Self {
            port,
            state,
            cancel,
        }
    }

    async fn snapshot(&self) -> MockState {
        self.state.lock().await.clone()
    }
}

/// One controller frame (`STR zone cc len data ETR`) or the AMX request, whole.
fn take_frame(buffer: &mut Vec<u8>) -> Option<Vec<u8>> {
    if buffer.starts_with(b"AMX\r") {
        return Some(buffer.drain(..4).collect());
    }
    while !buffer.is_empty() && buffer[0] != 0x21 && !buffer.starts_with(b"AMX\r") {
        if b"AMX\r".starts_with(buffer.as_slice()) {
            return None;
        }
        buffer.remove(0);
    }
    if buffer.starts_with(b"AMX\r") {
        return Some(buffer.drain(..4).collect());
    }
    if buffer.len() < 4 {
        return None;
    }
    let total = 5 + buffer[3] as usize;
    if buffer.len() < total {
        return None;
    }
    Some(buffer.drain(..total).collect())
}

/// `(replies, close the session afterwards)`.
async fn handle(state: &Arc<Mutex<MockState>>, frame: &[u8]) -> (Vec<Vec<u8>>, bool) {
    let mut state = state.lock().await;
    state.frames.push(frame.to_vec());
    if !state.answers {
        return (Vec::new(), false);
    }
    if frame == b"AMX\r" {
        state.amx_requests += 1;
        let reply = format!(
            "AMXB<Device-SDKClass=Receiver><Device-Make=ARCAM><Device-Model={}><Device-Revision=1.0>\r",
            state.amx_model
        );
        return (vec![reply.into_bytes()], false);
    }
    let (cc, data) = (frame[2], &frame[4..frame.len() - 1]);
    let is_query = data == [0xF0];
    if !is_query {
        state.writes.push(frame.to_vec());
    }
    let mut close = false;
    let mut affected = vec![cc];
    if !is_query {
        match (cc, data) {
            (0x00, [value]) => {
                state.power = *value == 1;
                close = !state.power && state.close_on_standby;
            }
            (0x0D, [value]) if state.power => state.volume = *value,
            (0x0E, [value]) if state.power => state.muted = *value == 0,
            (0x1D, [value]) if state.power => state.source = *value,
            // Simulated RC5 (AVR series): power, mute, then CD on the HDA series.
            (0x08, [16, 123]) => {
                state.power = true;
                affected = vec![0x00];
            }
            (0x08, [16, 124]) => {
                state.power = false;
                close = state.close_on_standby;
                affected = vec![0x00];
            }
            (0x08, [16, 26]) => {
                state.muted = true;
                affected = vec![0x0E];
            }
            (0x08, [16, 120]) => {
                state.muted = false;
                affected = vec![0x0E];
            }
            (0x08, [16, 118]) => {
                state.source = 0x01; // CD on the HDA series
                affected = vec![0x1D];
            }
            _ => return (vec![refusal(cc, 0x84)], false),
        }
    }
    let replies = affected
        .into_iter()
        .filter_map(|cc| status(&state, cc))
        .collect();
    (replies, close)
}

// ---------------------------------------------------------------------------------------------
// Harness (copied in shape from tests/device_safety.rs)
// ---------------------------------------------------------------------------------------------

struct Harness {
    aggregator: Arc<ZoneAggregator>,
    service: Arc<DeviceService>,
    adapter: Arc<ArcamAdapter>,
    _dir: tempfile::TempDir,
}

fn id(mac: &str) -> DeviceId {
    DeviceId::new("arcam", mac).unwrap()
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
    let adapter = Arc::new(ArcamAdapter::new(bus));
    service.register_driver(adapter.clone()).await;
    Harness {
        aggregator,
        service,
        adapter,
        _dir: dir,
    }
}

impl Harness {
    async fn attach_model(&self, mac: &str, model: &str, mock: &MockArcam) -> Device {
        self.adapter
            .attach_unit(&id(mac), "127.0.0.1", mock.port, model, "Study")
            .await;
        self.wait(mac, |d| d.volume.value.is_some() && d.input.is_some())
            .await
    }

    async fn attach(&self, mac: &str, mock: &MockArcam) -> Device {
        self.attach_model(mac, "SA30", mock).await
    }

    async fn wait(&self, mac: &str, condition: impl Fn(&Device) -> bool) -> Device {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(device) = self.aggregator.get_device(&id(mac)).await {
                if condition(&device) {
                    return device;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("device {mac} did not reach the expected state within 5s");
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
    let mock = MockArcam::start().await;
    let device = h.attach(STUDY, &mock).await;

    assert_eq!(device.device_id.as_str(), "arcam:00226B1C4D5E");
    assert_eq!(device.vendor, "Arcam");
    assert_eq!(device.model, "SA30");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.connection.control_channel, ControlChannel::Push);
    assert_eq!(device.input.as_deref(), Some("CD"));
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(device.volume.value, Some(40.0));
    assert_eq!(device.volume.min, 0.0);
    assert_eq!(device.volume.hardware_max, Some(99.0));
    assert_eq!(device.volume.step, 1.0);
    assert_eq!(device.volume.is_muted, Some(false));
    assert!(device.inputs.iter().any(|i| i.id == "NET"));
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
    assert!(device
        .capabilities
        .power
        .detail
        .contains("IP wake is unsupported"));
}

#[tokio::test]
async fn volume_is_clamped_by_a_ceiling_and_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockArcam::start().await;
    h.attach(STUDY, &mock).await;
    h.service
        .set_ceiling(id(STUDY).as_str(), Some(30.0))
        .await
        .unwrap();

    let observed = h
        .request(STUDY, DeviceRequest::VolumeAbsolute(80.0))
        .await
        .expect("confirmed by readback");
    assert_eq!(observed.volume.value, Some(30.0));
    // Exactly one level was sent, and it is the ceiling.
    assert_eq!(
        mock.snapshot().await.writes,
        vec![vec![0x21, 0x01, 0x0D, 0x01, 30, 0x0D]]
    );
    // Within the ceiling it passes through exactly, and 99 is never reachable.
    let observed = h
        .request(STUDY, DeviceRequest::VolumeAbsolute(12.0))
        .await
        .unwrap();
    assert_eq!(observed.volume.value, Some(12.0));
    assert!(mock
        .snapshot()
        .await
        .writes
        .iter()
        .all(|frame| frame[4] <= 30));
}

#[tokio::test]
async fn critical_no_state_changing_bytes_on_connect_or_reconnect() {
    let h = harness().await;
    let mock = MockArcam::start().await;
    h.attach(STUDY, &mock).await;
    let state = mock.snapshot().await;
    assert!(state.writes.is_empty(), "connect sent {:?}", state.writes);
    for cc in [0x00u8, 0x0D, 0x0E, 0x1D] {
        assert!(
            state
                .frames
                .contains(&vec![0x21, 0x01, cc, 0x01, 0xF0, 0x0D]),
            "missing query for {cc:#04x}"
        );
    }

    h.adapter.stop_family().await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.attach(STUDY, &mock).await;
    let state = mock.snapshot().await;
    assert!(state.connections >= 2, "expected a second session");
    assert!(state.writes.is_empty(), "reconnect sent {:?}", state.writes);
}

#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let study = MockArcam::start().await;
    let den = MockArcam::start().await;
    let study_device = h.attach(STUDY, &study).await;
    let den_device = h.attach(DEN, &den).await;
    assert_ne!(study_device.device_id, den_device.device_id);
    assert_eq!(study_device.model, den_device.model);

    h.service
        .set_ceiling(id(STUDY).as_str(), Some(60.0))
        .await
        .unwrap();
    h.request(STUDY, DeviceRequest::VolumeAbsolute(25.0))
        .await
        .unwrap();
    h.request(STUDY, DeviceRequest::Mute(true)).await.unwrap();

    assert_eq!(
        study.snapshot().await.writes,
        vec![
            vec![0x21, 0x01, 0x0D, 0x01, 25, 0x0D],
            vec![0x21, 0x01, 0x0E, 0x01, 0x00, 0x0D]
        ]
    );
    let den_state = den.snapshot().await;
    assert!(
        den_state.writes.is_empty(),
        "den received {:?}",
        den_state.writes
    );
    assert_eq!(den_state.volume, 40);
    assert!(!den_state.muted);
    assert_eq!(
        h.aggregator
            .get_device(&id(DEN))
            .await
            .unwrap()
            .volume
            .value,
        Some(40.0)
    );
}

/// SA series: direct writes for power, mute and source.
#[tokio::test]
async fn sa_series_round_trips_with_direct_writes() {
    let h = harness().await;
    let mock = MockArcam::start().await;
    h.attach(STUDY, &mock).await;

    let observed = h
        .request(STUDY, DeviceRequest::SelectInput("NET".to_string()))
        .await
        .unwrap();
    assert_eq!(observed.input.as_deref(), Some("NET"));
    assert_eq!(mock.snapshot().await.source, 0x0B);
    let observed = h.request(STUDY, DeviceRequest::Mute(true)).await.unwrap();
    assert_eq!(observed.volume.is_muted, Some(true));
    let observed = h.request(STUDY, DeviceRequest::Mute(false)).await.unwrap();
    assert_eq!(observed.volume.is_muted, Some(false));
    let observed = h.request(STUDY, DeviceRequest::PowerOff).await.unwrap();
    assert_eq!(observed.power, DevicePower::Standby);
    let observed = h.request(STUDY, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(observed.power, DevicePower::On);
    assert_eq!(
        mock.snapshot().await.writes,
        vec![
            vec![0x21, 1, 0x1D, 1, 0x0B, 0x0D],
            vec![0x21, 1, 0x0E, 1, 0x00, 0x0D],
            vec![0x21, 1, 0x0E, 1, 0x01, 0x0D],
            vec![0x21, 1, 0x00, 1, 0x00, 0x0D],
            vec![0x21, 1, 0x00, 1, 0x01, 0x0D],
        ]
    );
}

/// AVR (HDA) series: power, mute and source go out as simulated RC5 key presses.
#[tokio::test]
async fn avr_series_uses_simulated_rc5() {
    let h = harness().await;
    let mock = MockArcam::start_with(MockState {
        source: 0x03, // AV on the HDA series
        amx_model: "AVR30",
        ..MockState::default()
    })
    .await;
    let device = h.attach_model(STUDY, "AVR30", &mock).await;
    assert_eq!(device.input.as_deref(), Some("AV"));
    assert!(device.capabilities.power.detail.contains("RC5"));

    h.request(STUDY, DeviceRequest::Mute(true)).await.unwrap();
    let observed = h
        .request(STUDY, DeviceRequest::SelectInput("CD".to_string()))
        .await
        .unwrap();
    assert_eq!(observed.input.as_deref(), Some("CD"));
    h.request(STUDY, DeviceRequest::PowerOff).await.unwrap();
    h.request(STUDY, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(
        mock.snapshot().await.writes,
        vec![
            vec![0x21, 1, 0x08, 2, 16, 26, 0x0D],
            vec![0x21, 1, 0x08, 2, 16, 118, 0x0D],
            vec![0x21, 1, 0x08, 2, 16, 124, 0x0D],
            vec![0x21, 1, 0x08, 2, 16, 123, 0x0D],
        ]
    );
}

/// A unit that keeps LAN control in standby answers power queries (and refuses the rest with
/// 0x85); power-on then works over the session and the rest of the state is re-read.
#[tokio::test]
async fn power_on_from_a_standby_unit_that_keeps_its_network() {
    let h = harness().await;
    let mock = MockArcam::start_with(MockState {
        power: false,
        ..MockState::default()
    })
    .await;
    h.adapter
        .attach_unit(&id(STUDY), "127.0.0.1", mock.port, "SA30", "Study")
        .await;
    let device = h.wait(STUDY, |d| d.power == DevicePower::Standby).await;
    assert_eq!(device.volume.value, None, "no level is read in standby");

    let observed = h.request(STUDY, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(observed.power, DevicePower::On);
    let device = h
        .wait(STUDY, |d| d.volume.value == Some(40.0) && d.input.is_some())
        .await;
    assert_eq!(device.input.as_deref(), Some("CD"));
}

/// Unsupported or malformed operations error at the driver rather than succeeding or reaching
/// the unit.
#[tokio::test]
async fn unsupported_operations_error_instead_of_succeeding() {
    let h = harness().await;
    let pa = MockArcam::start().await;
    h.adapter
        .attach_unit(&id(STUDY), "127.0.0.1", pa.port, "PA720", "Power")
        .await;
    h.wait(STUDY, |d| d.power == DevicePower::On).await;
    let driver: &dyn DeviceDriver = h.adapter.as_ref();
    for operation in [
        DeviceOperation::SetVolume(10.0),
        DeviceOperation::SelectInput("CD".to_string()),
    ] {
        let error = driver
            .execute(&id(STUDY), operation.clone())
            .await
            .expect_err(&format!("{operation:?} on a power amplifier"));
        assert!(
            error.contains("power amplifier") || error.contains("not selectable"),
            "{error}"
        );
    }
    let device = h.aggregator.get_device(&id(STUDY)).await.unwrap();
    assert!(!device.capabilities.volume.supported);
    assert!(!device.capabilities.input_select.supported);
    assert_eq!(device.volume.value, None);
    assert!(pa.snapshot().await.writes.is_empty());

    // SA30: an input the series lacks, and levels outside 0-99.
    let sa = MockArcam::start().await;
    h.attach(DEN, &sa).await;
    for operation in [
        DeviceOperation::SelectInput("BT".to_string()),
        DeviceOperation::SetVolume(100.0),
        DeviceOperation::SetVolume(-1.0),
    ] {
        driver
            .execute(&id(DEN), operation.clone())
            .await
            .expect_err(&format!("{operation:?} must be refused"));
    }
    assert!(sa.snapshot().await.writes.is_empty());
    let error = h
        .request(DEN, DeviceRequest::SelectInput("BT".to_string()))
        .await
        .unwrap_err();
    assert!(
        matches!(error, DeviceError::UnknownInput { .. }),
        "{error:?}"
    );
}

/// An unrecognised model gets no series-dependent write (the client library would guess a
/// series; a wrong guess selects the wrong input).
#[tokio::test]
async fn an_unrecognised_model_refuses_series_dependent_writes() {
    let h = harness().await;
    let mock = MockArcam::start().await;
    h.adapter
        .attach_unit(&id(STUDY), "127.0.0.1", mock.port, "Radia X1", "Study")
        .await;
    h.wait(STUDY, |d| d.power == DevicePower::On).await;
    let driver: &dyn DeviceDriver = h.adapter.as_ref();
    for operation in [
        DeviceOperation::PowerStandby,
        DeviceOperation::SetMute(true),
        DeviceOperation::SelectInput("CD".to_string()),
    ] {
        let error = driver
            .execute(&id(STUDY), operation.clone())
            .await
            .expect_err("unrecognised model");
        assert!(error.contains("not recognised"), "{error}");
    }
    assert!(mock.snapshot().await.writes.is_empty());
}

/// Older models switch the network off in standby: power_on must say so, not pretend.
#[tokio::test]
async fn power_on_of_an_unreachable_unit_errors_clearly() {
    let h = harness().await;
    let closed = {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        listener.local_addr().unwrap().port()
    };
    h.adapter
        .attach_unit(&id(STUDY), "127.0.0.1", closed, "SA30", "Study")
        .await;
    let driver: &dyn DeviceDriver = h.adapter.as_ref();
    let error = driver
        .execute(&id(STUDY), DeviceOperation::PowerOn)
        .await
        .unwrap_err();
    assert!(error.contains("not reachable"), "{error}");
    assert!(error.contains("IP wake is unsupported"), "{error}");
    assert!(h.aggregator.get_device(&id(STUDY)).await.is_none());
}

#[tokio::test]
async fn a_silent_listener_is_never_published() {
    let h = harness().await;
    let mock = MockArcam::start_with(MockState {
        answers: false,
        ..MockState::default()
    })
    .await;
    h.adapter
        .attach_unit(&id(STUDY), "127.0.0.1", mock.port, "SA30", "Study")
        .await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(mock.snapshot().await.connections >= 1);
    assert!(h.aggregator.get_device(&id(STUDY)).await.is_none());
}

/// A unit that drops the network on standby: the closed session right after an accepted
/// power-off is standby, and the confirmation says so.
#[tokio::test]
async fn power_off_that_closes_the_session_is_confirmed_as_standby() {
    let h = harness().await;
    let mock = MockArcam::start_with(MockState {
        close_on_standby: true,
        ..MockState::default()
    })
    .await;
    h.attach(STUDY, &mock).await;
    let observed = h.request(STUDY, DeviceRequest::PowerOff).await.unwrap();
    assert_eq!(observed.power, DevicePower::Standby);
    let device = h
        .wait(STUDY, |d| {
            d.connection
                .detail
                .as_deref()
                .is_some_and(|detail| detail.contains("inferred"))
        })
        .await;
    assert_eq!(device.power, DevicePower::Standby);
    assert_eq!(device.connection.control_channel, ControlChannel::None);
}

/// Manual device: the configured MAC is the identity, and the AMX answer supplies the model.
#[tokio::test]
async fn manual_device_takes_its_model_from_the_amx_answer() {
    let h = harness().await;
    let mock = MockArcam::start().await;
    h.adapter
        .start_family(FamilyContext {
            manual: vec![
                ManualDevice {
                    family: "arcam".to_string(),
                    host: "127.0.0.1".to_string(),
                    port: Some(mock.port),
                    mac: Some("00:22:6b:1c:4d:5e".to_string()),
                },
                // No MAC: the protocol reports no serial, so this one is refused.
                ManualDevice {
                    family: "arcam".to_string(),
                    host: "127.0.0.2".to_string(),
                    port: Some(mock.port),
                    mac: None,
                },
            ],
        })
        .await
        .unwrap();
    let device = h
        .wait(STUDY, |d| d.volume.value.is_some() && d.input.is_some())
        .await;
    assert_eq!(device.model, "SA30");
    assert_eq!(device.discovery, "manual");
    assert_eq!(device.input.as_deref(), Some("CD"));
    assert!(mock.snapshot().await.amx_requests >= 1);
    assert_eq!(h.adapter.device_count().await, 1);
    h.adapter.stop_family().await;
}
