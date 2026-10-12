//! Trinnov Altitude device adapter, end to end through `DeviceService` (#774, alpha).
//!
//! The processor is `MockTrinnov`, defined in this file: a **synthetic** TCP server shaped from
//! the py-trinnov-altitude parser and client
//! (<https://github.com/binarylogic/py-trinnov-altitude>). It is not a capture from an Altitude
//! and proves wiring, not hardware behaviour.
//!
//! Every line the mock receives is recorded, so a test can assert what was (and was not) sent.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpSocket, TcpStream, UdpSocket};
use tokio::sync::{broadcast, watch, Mutex};
use tokio_util::sync::CancellationToken;

use unified_hifi_control::adapters::trinnov::TrinnovAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, VolumeScale};
use unified_hifi_control::devices::links::{DeviceLinkStore, ManualDevice};
use unified_hifi_control::devices::service::{
    Confirmation, DeviceDriver, DeviceError, DeviceOperation, DeviceRequest, DeviceService,
    DeviceTarget,
};
use unified_hifi_control::devices::{ControlChannel, Device, DeviceId, DevicePower};

// =============================================================================
// Synthetic Altitude
// =============================================================================

#[derive(Debug, Clone)]
struct MockState {
    welcome_id: String,
    volume: f32,
    muted: bool,
    profile: i32,
    profiles: Vec<(i32, String)>,
    /// Every line received, across all sessions.
    received: Vec<String>,
    sessions: usize,
}

impl MockState {
    fn new(welcome_id: &str) -> Self {
        Self {
            welcome_id: welcome_id.to_string(),
            volume: -40.0,
            muted: false,
            profile: 2,
            profiles: vec![
                (1, "Apple TV".into()),
                (2, "Kaleidescape".into()),
                (3, "PS5".into()),
            ],
            received: Vec::new(),
            sessions: 0,
        }
    }

    /// Lines that change the processor (not the handshake or queries).
    fn writes(&self) -> Vec<String> {
        self.received
            .iter()
            .filter(|l| {
                !(l.starts_with("id ")
                    || l.starts_with("get_")
                    || l.starts_with("send ")
                    || l.as_str() == "upmixer")
            })
            .cloned()
            .collect()
    }
}

struct MockTrinnov {
    addr: SocketAddr,
    state: Arc<Mutex<MockState>>,
    kick: broadcast::Sender<()>,
    powered: watch::Sender<bool>,
    cancel: CancellationToken,
}

impl Drop for MockTrinnov {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

fn bind(addr: SocketAddr) -> std::io::Result<TcpListener> {
    let socket = TcpSocket::new_v4()?;
    socket.set_reuseaddr(true)?;
    socket.bind(addr)?;
    socket.listen(16)
}

impl MockTrinnov {
    async fn start(initial: MockState) -> Self {
        let listener = bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let addr = listener.local_addr().unwrap();
        let state = Arc::new(Mutex::new(initial));
        let (kick, _) = broadcast::channel(4);
        let (powered, mut power_rx) = watch::channel(true);
        let cancel = CancellationToken::new();
        {
            let (state, kick, cancel) = (state.clone(), kick.clone(), cancel.clone());
            tokio::spawn(async move {
                let mut listener = Some(listener);
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        changed = power_rx.changed() => {
                            if changed.is_err() { break }
                            // Off: stop listening, like a processor in standby (connection refused).
                            listener = if *power_rx.borrow() { bind(addr).ok() } else { None };
                        }
                        accepted = async {
                            match &listener {
                                Some(l) => l.accept().await,
                                None => std::future::pending().await,
                            }
                        } => {
                            let Ok((stream, _)) = accepted else { continue };
                            tokio::spawn(serve(stream, state.clone(), kick.subscribe(), cancel.clone()));
                        }
                    }
                }
            });
        }
        Self {
            addr,
            state,
            kick,
            powered,
            cancel,
        }
    }

    async fn snapshot(&self) -> MockState {
        self.state.lock().await.clone()
    }

    fn drop_sessions(&self) {
        self.kick.send(()).ok();
    }

    /// The processor finishes booting (or powers down): it starts or stops accepting sessions.
    fn set_listening(&self, on: bool) {
        self.powered.send(on).ok();
        if !on {
            self.drop_sessions();
        }
    }
}

async fn serve(
    stream: TcpStream,
    state: Arc<Mutex<MockState>>,
    mut kick: broadcast::Receiver<()>,
    cancel: CancellationToken,
) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    let welcome = {
        let mut state = state.lock().await;
        state.sessions += 1;
        format!(
            "Welcome on Trinnov Optimizer (Version 4.2.1, ID {})\n",
            state.welcome_id
        )
    };
    if write.write_all(welcome.as_bytes()).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            _ = kick.recv() => break,
            line = lines.next_line() => {
                let Ok(Some(line)) = line else { break };
                let (reply, close) = handle(&state, line.trim()).await;
                if write.write_all(reply.as_bytes()).await.is_err() { break }
                if close { break }
            }
        }
    }
}

async fn handle(state: &Arc<Mutex<MockState>>, line: &str) -> (String, bool) {
    let mut state = state.lock().await;
    state.received.push(line.to_string());
    let mut reply = String::new();
    let mut close = false;
    if line == "get_current_state" {
        reply.push_str(&format!(
            "VOLUME {:.1}\nMUTE {}\n",
            state.volume,
            u8::from(state.muted)
        ));
        reply.push_str("PROFILES_CLEAR\n");
        for (index, name) in &state.profiles {
            reply.push_str(&format!("PROFILE {index}: {name}\n"));
        }
        reply.push_str(&format!("CURRENT_PROFILE {}\n", state.profile));
    } else if line == "send volume" {
        reply.push_str(&format!("VOLUME {:.1}\n", state.volume));
    } else if line == "get_current_profile" {
        reply.push_str(&format!("CURRENT_PROFILE {}\n", state.profile));
    } else if let Some(db) = line.strip_prefix("volume ") {
        if let Ok(db) = db.parse::<f32>() {
            state.volume = db;
            reply.push_str(&format!("VOLUME {db:.1}\n"));
        }
    } else if let Some(flag) = line.strip_prefix("mute ") {
        state.muted = flag == "1";
        reply.push_str(&format!("MUTE {flag}\n"));
    } else if let Some(index) = line.strip_prefix("profile ") {
        if let Ok(index) = index.parse::<i32>() {
            state.profile = index;
            reply.push_str(&format!("CURRENT_PROFILE {index}\n"));
        }
    } else if line == "power_off_SECURED_FHZMCH48FE" {
        reply.push_str("OK\n");
        close = true;
    }
    (reply, close)
}

/// A UDP listener standing in for the broadcast address, recording Wake-on-LAN datagrams.
struct WolListener {
    addr: SocketAddr,
    packets: Arc<Mutex<Vec<Vec<u8>>>>,
    cancel: CancellationToken,
}

impl Drop for WolListener {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl WolListener {
    async fn start() -> Self {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let addr = socket.local_addr().unwrap();
        let packets = Arc::new(Mutex::new(Vec::new()));
        let cancel = CancellationToken::new();
        {
            let (packets, cancel) = (packets.clone(), cancel.clone());
            tokio::spawn(async move {
                let mut buffer = [0u8; 256];
                loop {
                    tokio::select! {
                        _ = cancel.cancelled() => break,
                        received = socket.recv_from(&mut buffer) => {
                            let Ok((n, _)) = received else { break };
                            packets.lock().await.push(buffer[..n].to_vec());
                        }
                    }
                }
            });
        }
        Self {
            addr,
            packets,
            cancel,
        }
    }

    async fn received(&self) -> Vec<Vec<u8>> {
        self.packets.lock().await.clone()
    }
}

// =============================================================================
// Harness
// =============================================================================

const MAC_A: &str = "00:1B:21:AA:BB:01";
const MAC_B: &str = "00:1B:21:AA:BB:02";

struct Harness {
    adapter: Arc<TrinnovAdapter>,
    service: Arc<DeviceService>,
    aggregator: Arc<ZoneAggregator>,
    wol: WolListener,
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
    let adapter = Arc::new(TrinnovAdapter::new(bus).with_shutdown_hold(Duration::from_millis(300)));
    service.register_driver(adapter.clone()).await;
    Harness {
        adapter,
        service,
        aggregator,
        wol: WolListener::start().await,
        _dir: dir,
    }
}

fn manual(mock: &MockTrinnov, mac: Option<&str>) -> ManualDevice {
    ManualDevice {
        family: "trinnov".to_string(),
        host: "127.0.0.1".to_string(),
        port: Some(mock.addr.port()),
        mac: mac.map(str::to_string),
    }
}

impl Harness {
    async fn attach(&self, mock: &MockTrinnov, mac: Option<&str>) -> DeviceId {
        let device_id = self
            .adapter
            .attach_unit(&manual(mock, mac), self.wol.addr)
            .await
            .expect("attach");
        self.wait(&device_id, |d| {
            d.power == DevicePower::On && !d.inputs.is_empty()
        })
        .await;
        device_id
    }

    async fn wait<F: Fn(&Device) -> bool>(&self, device_id: &DeviceId, predicate: F) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if let Some(device) = self.aggregator.get_device(device_id).await {
                if predicate(&device) {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not met within 10s");
    }

    async fn device(&self, device_id: &DeviceId) -> Device {
        self.aggregator.get_device(device_id).await.expect("device")
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

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn unit_is_observed_with_mac_identity_decibels_and_profiles() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, Some(MAC_A)).await;

    let device = h.device(&device_id).await;
    assert_eq!(
        device_id.as_str(),
        "trinnov:001B21AABB01",
        "identity is the MAC, never name or IP"
    );
    assert_eq!(device.vendor, "Trinnov");
    assert_eq!(device.discovery, "manual");
    assert_eq!(device.connection.control_channel, ControlChannel::Push);
    assert_eq!(device.power, DevicePower::On);
    assert_eq!(device.volume.scale, VolumeScale::Decibel);
    assert_eq!(device.volume.value, Some(-40.0));
    assert_eq!(device.volume.min, -120.0);
    assert_eq!(
        device.volume.hardware_max,
        Some(20.0),
        "the protocol's full scale"
    );
    assert_eq!(device.volume.is_muted, Some(false));
    assert_eq!(device.input.as_deref(), Some("2"));
    let names: Vec<_> = device
        .inputs
        .iter()
        .map(|i| (i.id.as_str(), i.name.as_str()))
        .collect();
    assert_eq!(
        names,
        vec![("1", "Apple TV"), ("2", "Kaleidescape"), ("3", "PS5")]
    );
}

#[tokio::test]
async fn a_unit_without_a_mac_is_identified_by_its_welcome_id() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, None).await;
    assert_eq!(device_id.as_str(), "trinnov:4711");
    assert!(h
        .device(&device_id)
        .await
        .capabilities
        .power
        .detail
        .contains("no MAC"));
}

/// A ceiling only lowers the protocol maximum: a request above it is clamped, and the level
/// the processor reports back confirms it.
#[tokio::test]
async fn volume_is_clamped_by_the_ceiling_and_confirmed() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, Some(MAC_A)).await;

    // Without a ceiling the range is the protocol's: -120..+20.
    assert_eq!(
        h.device(&device_id).await.volume_control().map(|v| v.max),
        Some(20.0)
    );

    h.service
        .set_ceiling(device_id.as_str(), Some(-20.0))
        .await
        .unwrap();
    assert_eq!(
        h.device(&device_id).await.volume.effective_max(),
        Some(-20.0)
    );
    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(-5.0))
        .await
        .expect("confirmed");
    assert!(report.clamped);
    assert_eq!(report.sent_volume, Some(-20.0));
    assert_eq!(report.confirmation, Confirmation::Confirmed);
    assert_eq!(report.observed.volume.value, Some(-20.0));
    assert_eq!(
        mock.snapshot().await.writes(),
        vec!["volume -20.0".to_string()]
    );

    // Above the protocol maximum with a ceiling above it too: bounded by the protocol.
    h.service
        .set_ceiling(device_id.as_str(), Some(50.0))
        .await
        .unwrap();
    let report = h
        .control(&device_id, DeviceRequest::VolumeAbsolute(30.0))
        .await
        .expect("confirmed");
    assert_eq!(report.sent_volume, Some(20.0));
}

#[tokio::test]
async fn relative_volume_is_sent_as_an_absolute_level_never_dvolume() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, Some(MAC_A)).await;
    h.service
        .set_ceiling(device_id.as_str(), Some(0.0))
        .await
        .unwrap();

    h.control(&device_id, DeviceRequest::VolumeRelative(2.0))
        .await
        .expect("confirmed");
    let writes = mock.snapshot().await.writes();
    assert_eq!(writes, vec!["volume -38.0".to_string()]);
    assert!(writes.iter().all(|w| !w.starts_with("dvolume")));
}

#[tokio::test]
async fn mute_and_input_are_written_and_read_back() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, Some(MAC_A)).await;

    let report = h
        .control(&device_id, DeviceRequest::Mute(true))
        .await
        .expect("mute");
    assert_eq!(report.observed.volume.is_muted, Some(true));
    let report = h
        .control(&device_id, DeviceRequest::SelectInput("3".into()))
        .await
        .expect("input");
    assert_eq!(report.observed.input.as_deref(), Some("3"));
    // The display name resolves to the same profile index.
    let report = h
        .control(&device_id, DeviceRequest::SelectInput("apple tv".into()))
        .await
        .expect("input by name");
    assert_eq!(report.observed.input.as_deref(), Some("1"));
    assert_eq!(
        mock.snapshot().await.writes(),
        vec![
            "mute 1".to_string(),
            "profile 3".to_string(),
            "profile 1".to_string()
        ]
    );
}

/// CRITICAL: connecting and reconnecting only handshakes and asks.
#[tokio::test]
async fn critical_no_state_changing_line_on_connect_or_reconnect() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, Some(MAC_A)).await;
    assert!(mock.snapshot().await.writes().is_empty());

    mock.drop_sessions();
    let deadline = Instant::now() + Duration::from_secs(10);
    while mock.snapshot().await.sessions < 2 {
        assert!(Instant::now() < deadline, "the adapter never reconnected");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    h.wait(&device_id, |d| {
        d.power == DevicePower::On && d.volume.value.is_some()
    })
    .await;

    let received = mock.snapshot().await.received;
    assert!(mock.snapshot().await.writes().is_empty(), "{received:?}");
    assert!(
        received.iter().any(|l| l.starts_with("id ")),
        "{received:?}"
    );
    assert_eq!(received.iter().filter(|l| l.starts_with("id ")).count(), 2);
}

/// A dropped session is `unknown`, never rounded to `standby`.
#[tokio::test]
async fn a_lost_session_is_unknown_not_standby() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, Some(MAC_A)).await;
    mock.set_listening(false);
    h.wait(&device_id, |d| {
        d.connection.control_channel == ControlChannel::None
    })
    .await;
    let device = h.device(&device_id).await;
    assert_eq!(device.power, DevicePower::Unknown);
    assert_eq!(device.volume.value, None, "a stale level must not be shown");
}

/// CRITICAL: two processors must never share a route.
#[tokio::test]
async fn critical_two_units_never_cross_route() {
    let h = harness().await;
    let a = MockTrinnov::start(MockState::new("1001")).await;
    let b = MockTrinnov::start(MockState::new("1002")).await;
    let a_id = h.attach(&a, Some(MAC_A)).await;
    let b_id = h.attach(&b, Some(MAC_B)).await;
    assert_ne!(a_id, b_id);
    assert_eq!(h.adapter.device_count().await, 2);
    h.service
        .set_ceiling(a_id.as_str(), Some(-10.0))
        .await
        .unwrap();

    h.control(&a_id, DeviceRequest::VolumeAbsolute(-30.0))
        .await
        .expect("a");
    h.control(&a_id, DeviceRequest::Mute(true))
        .await
        .expect("a mute");

    assert_eq!(a.snapshot().await.writes().len(), 2);
    assert!(
        b.snapshot().await.writes().is_empty(),
        "{:?}",
        b.snapshot().await.writes()
    );
    assert_eq!(h.device(&b_id).await.volume.value, Some(-40.0));
}

#[tokio::test]
async fn unsupported_operations_error_rather_than_succeeding() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, None).await;

    // No MAC: Wake-on-LAN has no target. (The unit is up, so ask the driver directly; the
    // service would see "already on".)
    mock.set_listening(false);
    h.wait(&device_id, |d| d.power == DevicePower::Unknown)
        .await;
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::PowerOn)
        .await
        .unwrap_err();
    assert!(error.contains("not supported without a MAC"), "{error}");
    assert!(h.wol.received().await.is_empty());

    // Not connected: nothing can be sent.
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SetMute(true))
        .await
        .unwrap_err();
    assert!(error.contains("not connected"), "{error}");

    // A non-numeric profile is not a Trinnov profile index.
    mock.set_listening(true);
    h.wait(&device_id, |d| d.power == DevicePower::On).await;
    let error = h
        .adapter
        .execute(&device_id, DeviceOperation::SelectInput("hdmi".into()))
        .await
        .unwrap_err();
    assert!(error.contains("not a Trinnov profile index"), "{error}");
}

/// Power off completes on `OK`. Power on is a magic packet, which is *not* confirmation: the
/// command is reported unconfirmed, and the unit reads `on` only once its session is back.
#[tokio::test]
async fn power_off_is_acknowledged_and_power_on_is_not_confirmed_by_the_wake_packet() {
    let h = harness().await;
    let mock = MockTrinnov::start(MockState::new("4711")).await;
    let device_id = h.attach(&mock, Some(MAC_A)).await;

    let report = h
        .control(&device_id, DeviceRequest::PowerOff)
        .await
        .expect("acknowledged");
    assert_eq!(report.observed.power, DevicePower::Standby);
    assert!(mock
        .snapshot()
        .await
        .received
        .contains(&"power_off_SECURED_FHZMCH48FE".to_string()));
    mock.set_listening(false);
    h.wait(&device_id, |d| {
        d.connection.control_channel == ControlChannel::None
    })
    .await;
    assert_eq!(h.device(&device_id).await.power, DevicePower::Standby);

    // Wake: the packet leaves, but the processor has not answered, so this is not a success.
    tokio::time::sleep(Duration::from_millis(400)).await; // past the shutdown hold
    let error = h
        .control(&device_id, DeviceRequest::PowerOn)
        .await
        .unwrap_err();
    assert!(
        matches!(error, DeviceError::Unconfirmed { .. }),
        "{error:?}"
    );
    let packets = h.wol.received().await;
    assert_eq!(packets.len(), 1);
    let mac = [0x00, 0x1B, 0x21, 0xAA, 0xBB, 0x01];
    assert_eq!(packets[0].len(), 102);
    assert_eq!(&packets[0][..6], &[0xFF; 6]);
    assert_eq!(&packets[0][6..12], &mac);
    let device = h.device(&device_id).await;
    assert_ne!(
        device.power,
        DevicePower::On,
        "a sent packet is not a readback"
    );
    assert!(device
        .connection
        .detail
        .as_deref()
        .unwrap_or_default()
        .contains("wake requested"));

    // The processor finishes booting: the session reconnects and only then does it read `on`.
    mock.set_listening(true);
    h.wait(&device_id, |d| {
        d.power == DevicePower::On && d.volume.value.is_some()
    })
    .await;
    assert!(mock
        .snapshot()
        .await
        .writes()
        .iter()
        .all(|w| w == "power_off_SECURED_FHZMCH48FE"));
}
