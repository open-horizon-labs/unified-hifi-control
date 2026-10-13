//! Anthem device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The receivers are `MockAnthem`, a **synthetic** server shaped from `anthemav` (`protocol.py`,
//! `tests/test_protocol.py`): `;`-framed ASCII, `IDM`/`IDN`/`EMAC` identity, `ICN` + `ISNnn` (x20)
//! or `ISnIN` (x40) input names, `Z1POW`/`Z1VOL`/`Z1PVOL`/`Z1MUT`/`Z1INP`, and `!Z` for a write to a
//! powered-off zone. Not hardware evidence. Every message received is recorded.
//!
//! Tests named `critical_*` guard a physical failure mode.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::anthem::{AnthemAdapter, AnthemIdentity};
use unified_hifi_control::adapters::devices::{DeviceFamily, FamilyContext};
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, SharedBus, VolumeScale};
use unified_hifi_control::devices::links::{DeviceLinkStore, ManualDevice};
use unified_hifi_control::devices::service::{
    Confirmation, DeviceCommandReport, DeviceDriver, DeviceError, DeviceOperation, DeviceRequest,
    DeviceService, DeviceTarget,
};
use unified_hifi_control::devices::{
    CapabilityEvidence, ControlChannel, Device, DeviceId, DevicePower,
};

// =============================================================================
// Mock receiver
// =============================================================================

#[derive(Debug, Clone)]
struct MockState {
    model: &'static str,
    x40: bool,
    mac: &'static str,
    power: bool,
    /// x20: dB.
    volume_db: i32,
    /// x40: percent.
    volume_percent: u8,
    muted: bool,
    input: u16,
    inputs: Vec<(u16, &'static str)>,
    tx_on: bool,
    received: Vec<String>,
}

impl MockState {
    fn x20() -> Self {
        Self {
            model: "MRX 520",
            x40: false,
            mac: "00:11:22:33:44:55",
            power: true,
            volume_db: -40,
            volume_percent: 0,
            muted: false,
            input: 2,
            inputs: vec![(1, "TV"), (2, "Turntable"), (3, "Cast")],
            tx_on: true,
            received: Vec::new(),
        }
    }

    fn x40() -> Self {
        Self {
            model: "MRX 740",
            x40: true,
            mac: "00:AA:BB:CC:DD:01",
            volume_percent: 35,
            ..Self::x20()
        }
    }

    fn writes(&self) -> Vec<String> {
        self.received
            .iter()
            .filter(|message| !message.ends_with('?'))
            .cloned()
            .collect()
    }

    fn handle(&mut self, message: &str) -> Vec<String> {
        self.received.push(message.to_string());
        let mut out = Vec::new();
        match message {
            "IDM?" => out.push(format!("IDM{}", self.model)),
            "IDN?" if !self.x40 => out.push(format!("IDN{}", self.mac)),
            "EMAC?" if self.x40 => out.push(format!("EMAC{}", self.mac)),
            "WMAC?" if self.x40 => out.push("WMAC00:00:00:00:00:00".to_string()),
            "ICN?" => out.push(format!("ICN{}", self.inputs.len())),
            "ECH?" if !self.x40 => out.push(format!("ECH{}", u8::from(self.tx_on))),
            "GCTXS?" if self.x40 => out.push(format!("GCTXS{}", u8::from(self.tx_on))),
            "Z1POW?" => out.push(format!("Z1POW{}", u8::from(self.power))),
            "Z1VOL?" if !self.x40 => out.push(format!("Z1VOL{}", self.volume_db)),
            "Z1PVOL?" if self.x40 => out.push(format!("Z1PVOL{}", self.volume_percent)),
            "Z1MUT?" => out.push(format!("Z1MUT{}", u8::from(self.muted))),
            "Z1INP?" => out.push(format!("Z1INP{}", self.input)),
            other => {
                if let Some(number) = other
                    .strip_prefix("ISN")
                    .and_then(|rest| rest.strip_suffix('?'))
                    .and_then(|n| n.parse::<u16>().ok())
                    .filter(|_| !self.x40)
                {
                    if let Some((_, name)) = self.inputs.iter().find(|(n, _)| *n == number) {
                        out.push(format!("ISN{number:02}{name}"));
                    }
                } else if let Some(number) = other
                    .strip_prefix("IS")
                    .and_then(|rest| rest.strip_suffix("IN?"))
                    .and_then(|n| n.parse::<u16>().ok())
                    .filter(|_| self.x40)
                {
                    if let Some((_, name)) = self.inputs.iter().find(|(n, _)| *n == number) {
                        out.push(format!("IS{number}IN{name}"));
                    }
                } else if other.starts_with("Z1") && !other.ends_with('?') {
                    // A write. Powered-off zone: refused with `!Z`.
                    if !self.power && other != "Z1POW1" && other != "Z1POW0" {
                        out.push(format!("!Z{other}"));
                    } else if let Some(v) = other.strip_prefix("Z1POW") {
                        self.power = v == "1";
                        out.push(format!("Z1POW{}", u8::from(self.power)));
                    } else if let Some(v) = other.strip_prefix("Z1PVOL") {
                        if let Ok(level) = v.parse::<u8>() {
                            self.volume_percent = level;
                            out.push(format!("Z1PVOL{level}"));
                        }
                    } else if let Some(v) = other.strip_prefix("Z1VOL") {
                        if let Ok(level) = v.parse::<i32>() {
                            self.volume_db = level;
                            out.push(format!("Z1VOL{level}"));
                        }
                    } else if let Some(v) = other.strip_prefix("Z1MUT") {
                        self.muted = v == "1";
                        out.push(format!("Z1MUT{}", u8::from(self.muted)));
                    } else if let Some(v) = other.strip_prefix("Z1INP") {
                        if let Ok(n) = v.parse::<u16>() {
                            self.input = n;
                            out.push(format!("Z1INP{n}"));
                        }
                    }
                } else {
                    out.push(format!("!I{other}"));
                }
            }
        }
        out
    }
}

struct MockAnthem {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    cancel: CancellationToken,
}

impl MockAnthem {
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

impl Drop for MockAnthem {
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
                while let Some(end) = buffer.find(';') {
                    let message: String = buffer.drain(..=end).collect();
                    let message = message.trim_end_matches(';').trim().to_string();
                    let replies = state.lock().await.handle(&message);
                    for reply in replies {
                        if stream.write_all(format!("{reply};").as_bytes()).await.is_err() {
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
    adapter: Arc<AnthemAdapter>,
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
    let adapter = Arc::new(AnthemAdapter::new(bus.clone()));
    service.register_driver(adapter.clone()).await;
    Harness {
        service,
        aggregator,
        adapter,
        _bus: bus,
        _dir: dir,
    }
}

fn id(mac: &str) -> DeviceId {
    DeviceId::new("anthem", mac).unwrap()
}

impl Harness {
    async fn attach(&self, mock: &MockAnthem) -> DeviceId {
        let state = mock.snapshot().await;
        let device_id = id(state.mac);
        self.adapter
            .attach_unit(
                &device_id,
                "127.0.0.1",
                mock.port(),
                AnthemIdentity {
                    model: state.model.to_string(),
                    mac: state.mac.to_string(),
                },
            )
            .await
            .unwrap();
        self.wait_ready(&device_id).await;
        device_id
    }

    async fn wait_ready(&self, device_id: &DeviceId) -> Device {
        self.wait_for(
            |device| {
                device.volume.value.is_some() && device.input.is_some() && !device.inputs.is_empty()
            },
            device_id,
        )
        .await
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

// =============================================================================
// Identity, scale and capabilities
// =============================================================================

#[tokio::test]
async fn x20_receiver_reports_whole_db_with_the_protocol_maximum() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x20()).await;
    let device_id = h.attach(&mock).await;

    let device = h.device(&device_id).await.unwrap();
    assert_eq!(device.device_id.as_str(), "anthem:001122334455");
    assert_eq!(device.vendor, "Anthem");
    assert_eq!(device.model, "MRX 520");
    assert_eq!(device.discovery, "manual");
    assert_eq!(device.maturity, "alpha");
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.connection.control_channel, ControlChannel::Polling);
    assert_eq!(device.input.as_deref(), Some("2"));
    assert_eq!(device.input_name(), Some("Turntable"));
    assert_eq!(device.inputs.len(), 3);

    let volume = &device.volume;
    assert_eq!(volume.scale, VolumeScale::Decibel);
    assert_eq!(volume.value, Some(-40.0));
    assert_eq!(
        (volume.min, volume.hardware_max, volume.step),
        (-90.0, Some(0.0), 1.0)
    );
    assert_eq!(volume.is_muted, Some(false));
    assert!(device.volume_control().is_some());

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
        assert!(
            capability.detail.contains("Zone 1"),
            "{}",
            capability.detail
        );
    }
}

#[tokio::test]
async fn x40_receiver_uses_percent_and_its_own_input_queries() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x40()).await;
    let device_id = h.attach(&mock).await;

    let device = h.device(&device_id).await.unwrap();
    assert_eq!(device.model, "MRX 740");
    assert_eq!(device.volume.scale, VolumeScale::Percentage);
    assert_eq!(device.volume.value, Some(35.0));
    assert_eq!(
        (device.volume.min, device.volume.hardware_max),
        (0.0, Some(100.0))
    );
    assert_eq!(
        device.input_name(),
        Some("Turntable"),
        "IS<n>IN names parsed"
    );

    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(60.0))
        .await
        .unwrap();
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.volume.value, Some(60.0));

    let state = mock.snapshot().await;
    assert_eq!(state.writes(), vec!["Z1PVOL60"]);
    assert!(
        !state
            .received
            .iter()
            .any(|m| m.starts_with("ISN") || m == "IDN?" || m == "ECH?"),
        "x20-only queries were sent to an x40: {:?}",
        state.received
    );
}

#[tokio::test]
async fn a_distribution_amplifier_is_not_attached() {
    let h = harness().await;
    for model in ["MDX-16", "MDA-8", ""] {
        let error = h
            .adapter
            .attach_unit(
                &id("0011223344FF"),
                "127.0.0.1",
                9,
                AnthemIdentity {
                    model: model.to_string(),
                    mac: "00:11:22:33:44:FF".to_string(),
                },
            )
            .await
            .expect_err("no command set is assumed");
        assert!(error.contains("not a supported Anthem"), "{error}");
    }
    assert_eq!(h.adapter.device_count().await, 0);
}

// =============================================================================
// Commands and safety
// =============================================================================

/// CRITICAL: the user's ceiling lowers the protocol maximum, and only the clamped whole-dB value
/// reaches the receiver, confirmed by readback.
#[tokio::test]
async fn critical_volume_is_clamped_by_the_ceiling_and_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x20()).await;
    let device_id = h.attach(&mock).await;
    h.set_ceiling(&device_id, -30.0).await;

    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(-5.0))
        .await
        .unwrap();
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(-30.0));
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.volume.value, Some(-30.0));

    h.control(&device_id, DeviceRequest::VolumeRelative(12.0))
        .await
        .unwrap();
    let state = mock.snapshot().await;
    assert_eq!(state.volume_db, -30);
    for write in state.writes() {
        let level: i32 = write.strip_prefix("Z1VOL").unwrap().parse().unwrap();
        assert!(
            level <= -30,
            "a level above the ceiling reached the receiver: {write}"
        );
    }
}

/// CRITICAL: with no ceiling the protocol's full scale (0 dB, 100 %) bounds the level.
#[tokio::test]
async fn critical_levels_never_exceed_the_protocol_maximum() {
    let h = harness().await;
    let x20 = MockAnthem::start(MockState::x20()).await;
    let x20_id = h.attach(&x20).await;
    let report = h
        .control(&x20_id, DeviceRequest::VolumeAbsolute(15.0))
        .await
        .unwrap();
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(0.0));
    assert_eq!(x20.snapshot().await.writes(), vec!["Z1VOL0"]);

    let x40 = MockAnthem::start(MockState::x40()).await;
    let x40_id = h.attach(&x40).await;
    let report = h
        .control(&x40_id, DeviceRequest::VolumeAbsolute(500.0))
        .await
        .unwrap();
    assert_eq!(report.sent_volume, Some(100.0));
    h.set_ceiling(&x40_id, 70.0).await;
    let report = h
        .control(&x40_id, DeviceRequest::VolumeAbsolute(100.0))
        .await
        .unwrap();
    assert_eq!(report.sent_volume, Some(70.0));
}

#[tokio::test]
async fn power_mute_and_input_are_confirmed_by_readback() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x20()).await;
    let device_id = h.attach(&mock).await;

    let report = h
        .control(&device_id, DeviceRequest::Mute(true))
        .await
        .unwrap();
    assert_eq!(report.observed.volume.is_muted, Some(true));
    let report = h
        .control(&device_id, DeviceRequest::SelectInput("Cast".to_string()))
        .await
        .unwrap();
    assert_eq!(report.observed.input.as_deref(), Some("3"));
    let report = h
        .control(&device_id, DeviceRequest::PowerOff)
        .await
        .unwrap();
    assert_eq!(report.observed.power, DevicePower::Standby);
    let report = h.control(&device_id, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(report.observed.power, DevicePower::On);
    assert_eq!(
        mock.snapshot().await.writes(),
        vec!["Z1MUT1", "Z1INP3", "Z1POW0", "Z1POW1"]
    );
}

/// CRITICAL: connecting only *asks*. Not `ECH1` / `GCTXS1` (the reference client's transmit
/// switch), not a replayed level, not power. Checked on both generations.
#[tokio::test]
async fn critical_no_state_changing_bytes_on_connect() {
    for initial in [MockState::x20(), MockState::x40()] {
        let h = harness().await;
        let mock = MockAnthem::start(initial).await;
        let _ = h.attach(&mock).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let state = mock.snapshot().await;
        assert!(
            state.writes().is_empty(),
            "connect sent state-changing messages: {:?}",
            state.writes()
        );
        assert!(state.received.iter().any(|m| m == "Z1POW?"));
    }
}

/// CRITICAL: two receivers of one model never cross-route.
#[tokio::test]
async fn critical_two_units_of_one_model_never_cross_route() {
    let h = harness().await;
    let lounge = MockAnthem::start(MockState::x20()).await;
    let study = MockAnthem::start(MockState {
        mac: "00:11:22:33:44:66",
        volume_db: -55,
        ..MockState::x20()
    })
    .await;
    let lounge_id = h.attach(&lounge).await;
    let study_id = h.attach(&study).await;
    assert_ne!(lounge_id, study_id);

    h.control(&lounge_id, DeviceRequest::VolumeAbsolute(-35.0))
        .await
        .unwrap();
    h.control(&lounge_id, DeviceRequest::Mute(true))
        .await
        .unwrap();

    assert_eq!(lounge.snapshot().await.writes(), vec!["Z1VOL-35", "Z1MUT1"]);
    let study_state = study.snapshot().await;
    assert!(
        study_state.writes().is_empty(),
        "{:?}",
        study_state.writes()
    );
    assert_eq!(study_state.volume_db, -55);
    assert_eq!(h.device(&study_id).await.unwrap().volume.value, Some(-55.0));
}

/// Operations the receiver cannot do error instead of succeeding: a level off the whole-dB grid,
/// and anything but power while Zone 1 is off (it would be ignored).
#[tokio::test]
async fn unsupported_operations_error_instead_of_succeeding() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x20()).await;
    let device_id = h.attach(&mock).await;
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SetVolume(-35.5))
        .await
        .expect_err("x20 volume is whole dB");
    assert!(
        error.contains("cannot be encoded") && error.contains("whole dB"),
        "{error}"
    );
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SelectInput("200".to_string()))
        .await
        .expect_err("not an input number");
    assert!(error.contains("cannot be encoded"), "{error}");
    assert!(mock.snapshot().await.writes().is_empty());

    let off = MockAnthem::start(MockState {
        power: false,
        mac: "00:11:22:33:44:77",
        ..MockState::x20()
    })
    .await;
    let off_id = h.attach(&off).await;
    assert_eq!(h.device(&off_id).await.unwrap().power, DevicePower::Standby);
    for operation in [
        DeviceOperation::SetVolume(-40.0),
        DeviceOperation::SetMute(true),
        DeviceOperation::SelectInput("1".to_string()),
    ] {
        let error = h
            .adapter
            .execute(&off_id, operation)
            .await
            .expect_err("refused while Zone 1 is off");
        assert!(
            error.contains("not supported while Zone 1 is off"),
            "{error}"
        );
    }
    assert!(off.snapshot().await.writes().is_empty());
    let report = h.control(&off_id, DeviceRequest::PowerOn).await.unwrap();
    assert_eq!(report.observed.power, DevicePower::On);
}

#[tokio::test]
async fn transmit_status_off_is_reported_not_silently_changed() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState {
        tx_on: false,
        ..MockState::x20()
    })
    .await;
    let device_id = h.attach(&mock).await;
    let device = h
        .wait_for(|d| d.connection.detail.is_some(), &device_id)
        .await;
    assert!(
        device
            .connection
            .detail
            .as_deref()
            .unwrap()
            .contains("TX status"),
        "{:?}",
        device.connection.detail
    );
    assert!(
        mock.snapshot().await.writes().is_empty(),
        "ECH1 must not be sent"
    );
}

#[tokio::test]
async fn an_unreachable_unit_does_not_keep_publishing_stale_state() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x20()).await;
    let device_id = h.attach(&mock).await;
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
    assert!(h.control(&device_id, DeviceRequest::PowerOn).await.is_err());
}

// =============================================================================
// Manual configuration (the only way Anthem units are found)
// =============================================================================

#[tokio::test]
async fn a_configured_host_is_identified_by_the_units_own_mac_then_published() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x40()).await;
    h.adapter
        .start_family(FamilyContext {
            manual: vec![ManualDevice {
                family: "anthem".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some(mock.port()),
                mac: None,
            }],
        })
        .await
        .unwrap();
    // The wired MAC is the identity (the Wi-Fi MAC was all zeros).
    let device_id = id("00AABBCCDD01");
    let device = h.wait_ready(&device_id).await;
    assert_eq!(device.discovery, "manual");
    assert_eq!(device.model, "MRX 740");
    // Identification only asked.
    assert!(mock.snapshot().await.writes().is_empty());
    h.adapter.stop_family().await;
}

#[tokio::test]
async fn a_unit_with_no_usable_mac_falls_back_to_the_configured_one() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState {
        mac: "00:00:00:00:00:00",
        ..MockState::x20()
    })
    .await;
    h.adapter
        .start_family(FamilyContext {
            manual: vec![ManualDevice {
                family: "anthem".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some(mock.port()),
                mac: Some("00:11:22:33:44:99".to_string()),
            }],
        })
        .await
        .unwrap();
    let device_id = id("001122334499");
    h.wait_ready(&device_id).await;
    h.adapter.stop_family().await;
}

#[tokio::test]
async fn a_unit_with_no_identity_at_all_is_never_published() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState {
        mac: "00:00:00:00:00:00",
        ..MockState::x20()
    })
    .await;
    h.adapter
        .start_family(FamilyContext {
            manual: vec![ManualDevice {
                family: "anthem".to_string(),
                host: "127.0.0.1".to_string(),
                port: Some(mock.port()),
                mac: None,
            }],
        })
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(h.adapter.device_count().await, 0);
    h.adapter.stop_family().await;
}

#[tokio::test]
async fn stopping_the_adapter_removes_its_devices() {
    let h = harness().await;
    let mock = MockAnthem::start(MockState::x20()).await;
    let device_id = h.attach(&mock).await;
    h.adapter.stop_family().await;
    let deadline = Instant::now() + Duration::from_secs(3);
    while h.device(&device_id).await.is_some() {
        assert!(Instant::now() < deadline, "device was not removed");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}
