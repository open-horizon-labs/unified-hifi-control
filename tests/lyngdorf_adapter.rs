//! Lyngdorf device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The units are `MockLyngdorf`, a **synthetic** server shaped from the `lyngdorf` library's
//! message tables and the vendor manuals it transcribes (`docs/mp-60.md`, `docs/tdai-1120.md`,
//! `docs/p-series.md`): `!COMMAND(param)` lines, CR terminated (TDAI adds an LF), MP/P and TDAI
//! reply shapes, and the standby behaviour the library measured on a P200 (volume and mute writes
//! discarded, `!SRC(n)` powering the unit on). Not hardware evidence. Every line received is
//! recorded with its arrival time.
//!
//! Tests named `critical_*` guard a physical failure mode.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::lyngdorf::protocol::Description;
use unified_hifi_control::adapters::lyngdorf::LyngdorfAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, SharedBus, VolumeScale};
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{
    Confirmation, DeviceCommandReport, DeviceDriver, DeviceError, DeviceOperation, DeviceRequest,
    DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceId, DevicePower,
};

// =============================================================================
// Mock unit
// =============================================================================

#[derive(Debug, Clone, Copy, PartialEq)]
enum Dialect {
    Mp,
    Tdai,
}

#[derive(Debug, Clone)]
struct MockState {
    dialect: Dialect,
    model: &'static str,
    power: bool,
    /// Tenths of a dB.
    volume: i32,
    /// `!MAXVOL` in tenths of a dB (MP/P only); writes clamp to it, as the P200 measurement shows.
    max_volume: Option<i32>,
    muted: bool,
    source: u16,
    sources: Vec<(u16, &'static str)>,
    received: Vec<(Instant, String)>,
}

impl MockState {
    fn mp() -> Self {
        Self {
            dialect: Dialect::Mp,
            model: "MP-60",
            power: true,
            volume: -300,
            max_volume: Some(-200),
            muted: false,
            source: 4,
            sources: vec![(0, "HDMI 1"), (4, "Spotify"), (6, "Roon")],
            received: Vec::new(),
        }
    }

    fn tdai() -> Self {
        Self {
            dialect: Dialect::Tdai,
            model: "TDAI-1120",
            max_volume: None,
            sources: vec![(0, "Digital 1 (Coax)"), (4, "Analog 1"), (7, "Streaming")],
            ..Self::mp()
        }
    }

    fn lines(&self) -> Vec<String> {
        self.received.iter().map(|(_, line)| line.clone()).collect()
    }

    /// Lines that change state (not queries).
    fn writes(&self) -> Vec<String> {
        self.lines()
            .into_iter()
            .filter(|line| !line.ends_with('?'))
            .collect()
    }

    fn eol(&self) -> &'static str {
        match self.dialect {
            Dialect::Mp => "\r",
            Dialect::Tdai => "\r\n",
        }
    }

    fn handle(&mut self, line: &str) -> Vec<String> {
        self.received.push((Instant::now(), line.to_string()));
        let Some(command) = line.strip_prefix('!') else {
            return Vec::new();
        };
        let mp = self.dialect == Dialect::Mp;
        let mut out = Vec::new();
        match command {
            "DEVICE?" => out.push(format!("!DEVICE({})", self.model)),
            "POWER?" if mp => out.push(format!("!POWER({})", u8::from(self.power))),
            "PWR?" if !mp => out.push(format!("!PWR({})", if self.power { "ON" } else { "OFF" })),
            "POWERONMAIN" if mp => self.power = true,
            "POWEROFFMAIN" if mp => self.power = false,
            "ON" if !mp => self.power = true,
            "OFF" if !mp => self.power = false,
            "VOL?" => out.push(format!("!VOL({})", self.volume)),
            "MAXVOL?" if mp => {
                if let Some(max) = self.max_volume {
                    out.push(format!("!MAXVOL({max})"));
                }
            }
            "MUTE?" => {
                // Measured on a P200: in standby the unit answers "muted" whatever was sent.
                let muted = self.muted || !self.power;
                out.push(if mp {
                    if muted {
                        "!MUTEON".to_string()
                    } else {
                        "!MUTEOFF".to_string()
                    }
                } else {
                    format!("!MUTE({})", if muted { "ON" } else { "OFF" })
                });
            }
            "MUTEON" if self.power => self.muted = true,
            "MUTEOFF" if self.power => self.muted = false,
            "SRCS?" if mp => {
                out.push(format!("!SRCCOUNT({})", self.sources.len()));
                for (index, name) in &self.sources {
                    out.push(format!("!SRC({index})\"{name}\""));
                }
            }
            "SRCLIST?" if !mp => {
                out.push(format!("!SRCCOUNT({})", self.sources.len()));
                for (index, name) in &self.sources {
                    out.push(format!("!SRCNAME({index},\"{name}\")"));
                }
            }
            "SRC?" => {
                let name = self
                    .sources
                    .iter()
                    .find(|(index, _)| *index == self.source)
                    .map(|(_, name)| *name)
                    .unwrap_or("");
                out.push(if mp {
                    format!("!SRC({})\"{name}\"", self.source)
                } else {
                    format!("!SRC({})", self.source)
                });
            }
            other => {
                if let Some(value) = other.strip_prefix("VOL(").and_then(|v| v.strip_suffix(')')) {
                    if self.power {
                        if let Ok(tenths) = value.parse::<i32>() {
                            let ceiling = self.max_volume.unwrap_or(i32::MAX);
                            self.volume = tenths.min(ceiling);
                        }
                    }
                } else if let Some(value) =
                    other.strip_prefix("SRC(").and_then(|v| v.strip_suffix(')'))
                {
                    if let Ok(index) = value.parse::<u16>() {
                        if self.sources.iter().any(|(known, _)| *known == index) {
                            self.source = index;
                            // Measured on a P200: a source change powers the main zone on.
                            self.power = true;
                        }
                    }
                }
            }
        }
        out
    }
}

struct MockLyngdorf {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    cancel: CancellationToken,
}

impl MockLyngdorf {
    async fn start(initial: MockState) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(initial));
        let cancel = CancellationToken::new();
        {
            let state = state.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        accepted = listener.accept() => {
                            let Ok((stream, _)) = accepted else { break };
                            tokio::spawn(serve(stream, state.clone(), cancel.clone()));
                        }
                    }
                }
            });
        }
        Self {
            addr,
            state,
            cancel,
        }
    }

    fn port(&self) -> u16 {
        self.addr.port()
    }

    async fn snapshot(&self) -> MockState {
        self.state.lock().await.clone()
    }
}

impl Drop for MockLyngdorf {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

async fn serve(mut stream: TcpStream, state: Arc<Mutex<MockState>>, cancel: CancellationToken) {
    let mut buffer = String::new();
    let mut chunk = [0u8; 256];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            read = stream.read(&mut chunk) => {
                let Ok(n) = read else { break };
                if n == 0 {
                    break;
                }
                buffer.push_str(&String::from_utf8_lossy(&chunk[..n]));
                while let Some(end) = buffer.find('\r') {
                    let line: String = buffer.drain(..=end).collect();
                    let line = line.trim().to_string();
                    let (replies, eol) = {
                        let mut state = state.lock().await;
                        (state.handle(&line), state.eol())
                    };
                    for reply in replies {
                        if stream.write_all(format!("{reply}{eol}").as_bytes()).await.is_err() {
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
    adapter: Arc<LyngdorfAdapter>,
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
        .with_confirm_timeout(Duration::from_millis(2500)),
    );
    let adapter = Arc::new(LyngdorfAdapter::new(bus.clone()));
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
        manufacturer: "Lyngdorf".to_string(),
        model: model.to_string(),
        friendly_name: format!("Lyngdorf {serial}"),
        serial: Some(serial.to_string()),
        udn: None,
    }
}

fn id(serial: &str) -> DeviceId {
    DeviceId::new("lyngdorf", serial).unwrap()
}

impl Harness {
    async fn attach(&self, serial: &str, mock: &MockLyngdorf) -> DeviceId {
        let model = mock.snapshot().await.model;
        let device_id = id(serial);
        self.adapter
            .attach_unit(
                &device_id,
                "127.0.0.1",
                mock.port(),
                description(model, serial),
            )
            .await
            .unwrap();
        // Ready: level, current source and the source list have all arrived.
        self.wait_for(
            |device| {
                device.volume.value.is_some() && device.input.is_some() && !device.inputs.is_empty()
            },
            &device_id,
        )
        .await;
        device_id
    }

    async fn device(&self, device_id: &DeviceId) -> Option<Device> {
        self.aggregator.get_device(device_id).await
    }

    async fn wait_for(&self, condition: impl Fn(&Device) -> bool, device_id: &DeviceId) -> Device {
        let deadline = Instant::now() + Duration::from_secs(8);
        while Instant::now() < deadline {
            if let Some(device) = self.device(device_id).await {
                if condition(&device) {
                    return device;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within 8s for {device_id}");
    }

    async fn control(
        &self,
        device_id: &DeviceId,
        request: DeviceRequest,
    ) -> Result<DeviceCommandReport, DeviceError> {
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

/// The highest `!VOL(n)` ever received, in dB.
fn highest_level_written(state: &MockState) -> Option<f32> {
    state
        .writes()
        .iter()
        .filter_map(|line| line.strip_prefix("!VOL("))
        .filter_map(|rest| rest.strip_suffix(')'))
        .filter_map(|tenths| tenths.parse::<f32>().ok())
        .map(|tenths| tenths / 10.0)
        .reduce(f32::max)
}

// =============================================================================
// Identity, scale and capabilities
// =============================================================================

#[tokio::test]
async fn mp_unit_is_observed_with_serial_identity_decibel_scale_and_its_own_maximum() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::mp()).await;
    let device_id = h.attach("ab12cd34", &mock).await;

    let device = h.device(&device_id).await.unwrap();
    assert_eq!(device.device_id.as_str(), "lyngdorf:AB12CD34");
    assert_eq!(device.vendor, "Lyngdorf");
    assert_eq!(device.model, "MP-60");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.connection.control_channel, ControlChannel::Polling);
    assert_eq!(device.input.as_deref(), Some("4"));
    assert_eq!(device.input_name(), Some("Spotify"));
    assert_eq!(
        device.inputs.len(),
        3,
        "the list is not polluted by the current-source answer"
    );

    let volume = &device.volume;
    assert_eq!(volume.scale, VolumeScale::Decibel);
    assert_eq!(volume.value, Some(-30.0));
    assert_eq!(volume.min, -99.9);
    assert_eq!(volume.step, 0.1);
    assert_eq!(volume.hardware_max, Some(-20.0), "the unit's own !MAXVOL");
    assert_eq!(volume.is_muted, Some(false));
    let control = device.volume_control().unwrap();
    assert_eq!(control.max, -20.0);

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
async fn tdai_unit_speaks_its_own_dialect_and_uses_the_documented_maximum() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::tdai()).await;
    let device_id = h.attach("tdai0001", &mock).await;

    let device = h.device(&device_id).await.unwrap();
    assert_eq!(device.model, "TDAI-1120");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.volume.is_muted, Some(false));
    assert_eq!(device.input.as_deref(), Some("4"));
    assert_eq!(device.input_name(), Some("Analog 1"));
    assert_eq!(
        device
            .inputs
            .iter()
            .map(|input| input.name.as_str())
            .collect::<Vec<_>>(),
        vec!["Digital 1 (Coax)", "Analog 1", "Streaming"],
        "a name with parentheses survives, and CR LF framing is handled"
    );
    // TDAI reports no !MAXVOL: the model's documented +12.0 dB stands.
    assert_eq!(device.volume.hardware_max, Some(12.0));

    // TDAI commands: !ON / !OFF and !MUTE(...) replies.
    h.control(&device_id, DeviceRequest::PowerOff)
        .await
        .unwrap();
    h.control(&device_id, DeviceRequest::PowerOn).await.unwrap();
    let writes = mock.snapshot().await.writes();
    assert_eq!(writes, vec!["!OFF", "!ON"]);
    let report = h
        .control(&device_id, DeviceRequest::Mute(true))
        .await
        .unwrap();
    assert_eq!(report.observed.volume.is_muted, Some(true));
}

#[tokio::test]
async fn unsupported_models_are_not_attached() {
    let h = harness().await;
    for model in ["TDAI-2170", "XYZ-1", ""] {
        let error = h
            .adapter
            .attach_unit(&id("zzz"), "127.0.0.1", 9, description(model, "zzz"))
            .await
            .expect_err("no command set is known");
        assert!(error.contains("not a supported Lyngdorf model"), "{error}");
    }
    assert_eq!(h.adapter.device_count().await, 0);
}

// =============================================================================
// Commands and safety
// =============================================================================

/// CRITICAL: the unit's own maximum and the user's ceiling both bound every level, and only the
/// clamped value reaches the unit, which the readback confirms. Tenths of a dB on the wire.
#[tokio::test]
async fn critical_volume_is_clamped_by_the_ceiling_and_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::mp()).await;
    let device_id = h.attach("ab12cd34", &mock).await;
    h.set_ceiling(&device_id, -35.0).await;

    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(-10.0))
        .await
        .unwrap();
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(-35.0));
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.volume.value, Some(-35.0));

    h.control(&device_id, DeviceRequest::VolumeRelative(10.0))
        .await
        .unwrap();
    let snapshot = mock.snapshot().await;
    assert_eq!(snapshot.volume, -350);
    let highest = highest_level_written(&snapshot).expect("levels were sent");
    assert!(
        highest <= -35.0,
        "a level above the ceiling reached the unit: {highest}"
    );
    assert!(snapshot.writes().contains(&"!VOL(-350)".to_string()));
}

/// CRITICAL: with no ceiling the model's documented maximum (TDAI +12 dB) still bounds the level.
#[tokio::test]
async fn critical_levels_never_exceed_the_models_documented_maximum() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::tdai()).await;
    let device_id = h.attach("tdai0001", &mock).await;

    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(24.0))
        .await
        .unwrap();
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(12.0));
    h.set_ceiling(&device_id, -20.0).await;
    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(0.0))
        .await
        .unwrap();
    assert_eq!(report.sent_volume, Some(-20.0));
    let highest = highest_level_written(&mock.snapshot().await).unwrap();
    assert!(highest <= 12.0);
}

/// The grid: a level the user asks for lands on the 0.1 dB grid and reads back unchanged.
#[tokio::test]
async fn volume_round_trips_on_the_tenth_of_a_db_grid() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::mp()).await;
    let device_id = h.attach("ab12cd34", &mock).await;
    for target in [-30.0_f32, -45.5, -60.0, -99.9] {
        let report = h
            .control(&device_id, DeviceRequest::VolumeAbsolute(target))
            .await
            .unwrap();
        let sent = report.sent_volume.unwrap();
        assert!(
            (sent - target).abs() <= 0.11,
            "asked {target}, sent {sent} (quantised down to the grid)"
        );
        assert!(sent <= target + 0.001, "never above the request");
        assert_eq!(report.observed.volume.value, Some(sent));
    }
}

#[tokio::test]
async fn mute_power_and_source_are_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::mp()).await;
    let device_id = h.attach("ab12cd34", &mock).await;

    let report = h
        .control(&device_id, DeviceRequest::Mute(true))
        .await
        .unwrap();
    assert_eq!(report.observed.volume.is_muted, Some(true));
    let report = h
        .control(&device_id, DeviceRequest::SelectInput("Roon".to_string()))
        .await
        .unwrap();
    assert_eq!(report.observed.input.as_deref(), Some("6"));
    let report = h
        .control(&device_id, DeviceRequest::PowerOff)
        .await
        .unwrap();
    assert_eq!(report.observed.power, DevicePower::Standby);
    let report = h.control(&device_id, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(report.observed.power, DevicePower::On);
    assert_eq!(
        mock.snapshot().await.writes(),
        vec!["!MUTEON", "!SRC(6)", "!POWEROFFMAIN", "!POWERONMAIN"]
    );
}

/// CRITICAL: connecting only *asks*. Not `!VERB(1)` (the reference client's push switch), not a
/// replayed level, not power. Checked on both dialects.
#[tokio::test]
async fn critical_no_state_changing_bytes_on_connect() {
    for initial in [MockState::mp(), MockState::tdai()] {
        let h = harness().await;
        let mock = MockLyngdorf::start(initial).await;
        let _ = h.attach("unit0001", &mock).await;
        // Let a poll cycle pass as well.
        tokio::time::sleep(Duration::from_millis(300)).await;
        let state = mock.snapshot().await;
        assert!(
            state.writes().is_empty(),
            "connect sent state-changing lines: {:?}",
            state.writes()
        );
        assert!(state.lines().iter().any(|line| line == "!VOL?"));
        assert!(state.lines().iter().any(|line| line.starts_with("!SRC")));
    }
}

/// CRITICAL: two units of one model never cross-route.
#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let lounge = MockLyngdorf::start(MockState::mp()).await;
    let study = MockLyngdorf::start(MockState {
        volume: -450,
        ..MockState::mp()
    })
    .await;
    let lounge_id = h.attach("unit0001", &lounge).await;
    let study_id = h.attach("unit0002", &study).await;
    assert_ne!(lounge_id, study_id);

    h.control(&lounge_id, DeviceRequest::VolumeAbsolute(-40.0))
        .await
        .unwrap();
    h.control(&lounge_id, DeviceRequest::Mute(true))
        .await
        .unwrap();

    assert_eq!(
        lounge.snapshot().await.writes(),
        vec!["!VOL(-400)", "!MUTEON"]
    );
    let study_state = study.snapshot().await;
    assert!(
        study_state.writes().is_empty(),
        "the other unit received {:?}",
        study_state.writes()
    );
    assert_eq!(study_state.volume, -450);
    assert!(!study_state.muted);
    assert_eq!(h.device(&study_id).await.unwrap().volume.value, Some(-45.0));
}

/// Standby discards volume and mute writes silently and `!SRC(n)` powers the unit on. So those
/// operations error instead of succeeding (or switching the unit on), and nothing is sent.
#[tokio::test]
async fn unsupported_in_standby_operations_error_and_send_nothing() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState {
        power: false,
        ..MockState::mp()
    })
    .await;
    let device_id = h.attach("ab12cd34", &mock).await;
    let device = h.device(&device_id).await.unwrap();
    assert_eq!(device.power, DevicePower::Standby);
    assert_eq!(
        device.volume.is_muted, None,
        "the unit answers muted in standby whatever was sent: not an observation"
    );

    for operation in [
        DeviceOperation::SetVolume(-40.0),
        DeviceOperation::SetMute(false),
        DeviceOperation::SelectInput("6".to_string()),
    ] {
        let error = h
            .adapter
            .execute(&device_id, operation.clone())
            .await
            .expect_err("refused in standby");
        assert!(
            error.contains("not supported while the unit is in standby"),
            "{error}"
        );
    }
    let state = mock.snapshot().await;
    assert!(state.writes().is_empty(), "{:?}", state.writes());
    assert!(
        !state.power,
        "a refused source change must not wake the unit"
    );

    // Powering on is allowed, and then writes work.
    let report = h.control(&device_id, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(report.observed.power, DevicePower::On);
    h.control(&device_id, DeviceRequest::VolumeAbsolute(-40.0))
        .await
        .unwrap();
}

#[tokio::test]
async fn a_level_the_model_cannot_express_errors() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::tdai()).await;
    let device_id = h.attach("tdai0001", &mock).await;
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SetVolume(20.0))
        .await
        .expect_err("TDAI tops out at +12 dB");
    assert!(error.contains("cannot be encoded"), "{error}");
    let error = h
        .adapter
        .execute(
            &device_id,
            DeviceOperation::SelectInput("Analog 1".to_string()),
        )
        .await
        .expect_err("not a source number");
    assert!(error.contains("cannot be encoded"), "{error}");
    assert!(mock.snapshot().await.writes().is_empty());
}

/// A real MP-60 drops everything past ~16 in-flight commands: writes and queries are paced.
#[tokio::test]
async fn writes_and_queries_are_paced() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::mp()).await;
    let device_id = h.attach("ab12cd34", &mock).await;
    h.control(&device_id, DeviceRequest::VolumeAbsolute(-40.0))
        .await
        .unwrap();
    let state = mock.snapshot().await;
    for pair in state.received.windows(2) {
        let gap = pair[1].0.duration_since(pair[0].0);
        assert!(
            gap >= Duration::from_millis(40),
            "{:?} then {:?} only {gap:?} apart",
            pair[0].1,
            pair[1].1
        );
    }
}

#[tokio::test]
async fn an_unreachable_unit_does_not_keep_publishing_stale_state() {
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::mp()).await;
    let device_id = h.attach("ab12cd34", &mock).await;
    assert!(h
        .device(&device_id)
        .await
        .unwrap()
        .volume_control()
        .is_some());

    mock.cancel.cancel();
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
    assert!(device.inputs.is_empty());
    assert!(device.volume_control().is_none());
    assert!(h.control(&device_id, DeviceRequest::PowerOn).await.is_err());
}

#[tokio::test]
async fn stopping_the_adapter_removes_its_devices() {
    use unified_hifi_control::adapters::devices::DeviceFamily;
    let h = harness().await;
    let mock = MockLyngdorf::start(MockState::mp()).await;
    let device_id = h.attach("ab12cd34", &mock).await;
    h.adapter.stop_family().await;
    let deadline = Instant::now() + Duration::from_secs(3);
    while h.device(&device_id).await.is_some() {
        assert!(Instant::now() < deadline, "device was not removed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
