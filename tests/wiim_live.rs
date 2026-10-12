//! Live checks against real WiiM devices on the LAN, through the production adapter and
//! `DeviceService` (#788). Ignored by default.
//!
//! ```sh
//! UHC_LIVE_WIIM=1 cargo test --test wiim_live -- --ignored --nocapture --test-threads=1
//! ```
//!
//! `live_wiim_read_path` is read-only. `live_wiim_write_path` changes one unit's volume by one
//! step *down* and back, and toggles mute on and off, restoring the original state.

use std::sync::Arc;
use std::time::{Duration, Instant};

use unified_hifi_control::adapters::devices::{DeviceFamily, FamilyContext};
use unified_hifi_control::adapters::wiim::WiimAdapter;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::create_bus;
use unified_hifi_control::devices::links::DeviceLinkStore;
use unified_hifi_control::devices::service::{DeviceRequest, DeviceService, DeviceTarget};
use unified_hifi_control::devices::Device;

async fn discover() -> (
    Arc<DeviceService>,
    Arc<ZoneAggregator>,
    Vec<Device>,
    tempfile::TempDir,
) {
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
        .with_confirm_timeout(Duration::from_secs(6)),
    );
    let adapter = Arc::new(WiimAdapter::new(bus));
    service.register_driver(adapter.clone()).await;
    adapter
        .start_family(FamilyContext::default())
        .await
        .unwrap();

    // mDNS answers trickle in; wait until the set stops growing.
    let started = Instant::now();
    let mut last = 0;
    let mut stable_since = Instant::now();
    while started.elapsed() < Duration::from_secs(40) {
        let count = aggregator.get_devices().await.len();
        if count != last {
            last = count;
            stable_since = Instant::now();
        } else if count > 0 && stable_since.elapsed() > Duration::from_secs(8) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    let devices = aggregator.get_devices().await;
    println!(
        "discovered {} WiiM device(s) in {:?}",
        devices.len(),
        started.elapsed()
    );
    for d in &devices {
        println!(
            "  {} name={:?} model={:?} host={} power={:?} channel={:?} vol={:?} {:?} max={:?} muted={:?} input={:?} inputs={}",
            d.device_id, d.name, d.model, d.host, d.power, d.connection.control_channel,
            d.volume.value, d.volume.scale, d.volume.hardware_max, d.volume.is_muted,
            d.input, d.inputs.len()
        );
    }
    (service, aggregator, devices, dir)
}

#[tokio::test]
#[ignore = "needs real WiiM devices; set UHC_LIVE_WIIM=1"]
async fn live_wiim_read_path() {
    if std::env::var("UHC_LIVE_WIIM").is_err() {
        return;
    }
    let (_service, _aggregator, devices, _dir) = discover().await;
    assert!(!devices.is_empty(), "no WiiM device discovered over mDNS");
    for d in &devices {
        assert_eq!(d.device_id.kind(), "wiim");
        assert!(d.volume.value.is_some(), "{}: no volume read", d.device_id);
    }
    let mut ids: Vec<_> = devices.iter().map(|d| d.device_id.clone()).collect();
    ids.dedup();
    assert_eq!(ids.len(), devices.len(), "two units shared an id");
}

#[tokio::test]
#[ignore = "changes a real WiiM's volume by one step and back; set UHC_LIVE_WIIM=1"]
async fn live_wiim_write_path() {
    if std::env::var("UHC_LIVE_WIIM").is_err() {
        return;
    }
    let (service, aggregator, devices, _dir) = discover().await;
    // A unit with a readable, non-zero, unmuted volume, so one step down is audible-safe.
    let target = devices
        .iter()
        .filter(|d| {
            d.volume.value.is_some_and(|v| (1.0..100.0).contains(&v))
                && d.volume.is_muted == Some(false)
        })
        .min_by(|a, b| a.device_id.cmp(&b.device_id))
        .expect("a WiiM with a non-zero, unmuted volume")
        .clone();
    let id = target.device_id.clone();
    let original = target.volume.value.unwrap();
    println!(
        "write test on {} ({:?}) starting at {original}",
        id, target.name
    );

    let timed = |label: &'static str, started: Instant| {
        println!("  {label}: confirmed in {:?}", started.elapsed());
    };

    let t = Instant::now();
    let down = service
        .control(
            DeviceTarget::Device(id.clone()),
            DeviceRequest::VolumeAbsolute(original - 1.0),
        )
        .await
        .expect("volume down confirmed");
    timed("volume down", t);
    assert_eq!(down.observed.volume.value, Some(original - 1.0));

    let t = Instant::now();
    let noop = service
        .control(
            DeviceTarget::Device(id.clone()),
            DeviceRequest::VolumeAbsolute(original - 1.0),
        )
        .await
        .expect("no-op write confirmed");
    timed("no-op volume", t);
    assert_eq!(noop.observed.volume.value, Some(original - 1.0));

    let t = Instant::now();
    service
        .control(
            DeviceTarget::Device(id.clone()),
            DeviceRequest::VolumeAbsolute(original),
        )
        .await
        .expect("volume restored");
    timed("volume restore", t);

    let t = Instant::now();
    service
        .control(DeviceTarget::Device(id.clone()), DeviceRequest::Mute(true))
        .await
        .expect("mute confirmed");
    timed("mute on", t);
    let t = Instant::now();
    service
        .control(DeviceTarget::Device(id.clone()), DeviceRequest::Mute(false))
        .await
        .expect("unmute confirmed");
    timed("mute off", t);

    let power = service
        .control(DeviceTarget::Device(id.clone()), DeviceRequest::PowerOff)
        .await;
    println!("  power off (expected refused): {:?}", power.as_ref().err());
    assert!(
        power.is_err(),
        "WiiM has no power control; a power command must not succeed"
    );

    let end = aggregator.get_device(&id).await.unwrap();
    assert_eq!(end.volume.value, Some(original));
    assert_eq!(end.volume.is_muted, Some(false));
}
