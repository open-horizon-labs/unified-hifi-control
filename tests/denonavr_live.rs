//! Live read-path check against real Denon/Marantz receivers on the local network (#781).
//!
//! Ignored by default and gated on `UHC_LIVE_DENONAVR_SERIALS`, a comma-separated list of the
//! serials expected on the network:
//!
//! ```sh
//! UHC_LIVE_DENONAVR_SERIALS=0006781E0AB8,0006781E2AB9 \
//!   cargo test --test denonavr_live -- --ignored --nocapture
//! ```
//!
//! **Read-only.** The adapter is started exactly as in production: SSDP discovery, description
//! fetch, the Telnet push session (state queries only — no level is sent on connect), and HTTP
//! polling. No command is issued, so no device state changes.

use std::time::{Duration, Instant};

use unified_hifi_control::adapters::denonavr::DenonAvrAdapter;
use unified_hifi_control::adapters::devices::{DeviceFamily, FamilyContext};
use unified_hifi_control::bus::{create_bus, BusEvent};
use unified_hifi_control::devices::{ControlChannel, Device, DeviceId, DevicePower};

#[tokio::test]
#[ignore = "needs real receivers on the LAN; set UHC_LIVE_DENONAVR_SERIALS"]
async fn live_receivers_are_discovered_identified_and_observed() {
    let Ok(serials) = std::env::var("UHC_LIVE_DENONAVR_SERIALS") else {
        eprintln!("UHC_LIVE_DENONAVR_SERIALS not set; skipping");
        return;
    };
    let expected: Vec<DeviceId> = serials
        .split(',')
        .filter_map(|serial| DeviceId::new("marantz", serial.trim()))
        .collect();
    assert!(!expected.is_empty());

    let bus = create_bus();
    let mut events = bus.subscribe();
    let adapter = DenonAvrAdapter::new(bus.clone());
    adapter
        .start_family(FamilyContext::default())
        .await
        .unwrap();

    let mut latest: std::collections::HashMap<DeviceId, Device> = Default::default();
    let started = Instant::now();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(45);
    loop {
        let settled = expected.iter().all(|id| {
            latest.get(id).is_some_and(|device| {
                device.power != DevicePower::Unknown
                    && device.volume.value.is_some()
                    && (device.power != DevicePower::On || device.volume.hardware_max.is_some())
            })
        });
        if settled {
            break;
        }
        match tokio::time::timeout_at(deadline, events.recv()).await {
            Ok(Ok(BusEvent::DeviceObserved { device })) => {
                latest.insert(device.device_id.clone(), *device);
            }
            Ok(_) => {}
            Err(_) => break,
        }
    }
    adapter.stop_family().await;

    for id in &expected {
        let device = latest
            .get(id)
            .unwrap_or_else(|| panic!("{id} was not discovered within 45s"));
        println!(
            "{id} name={:?} model={} host={} power={:?} channel={:?} level={:?} dB max={:?} dB \
             muted={:?} input={:?} ({:?}) inputs={} detail={:?}",
            device.name,
            device.model,
            device.host,
            device.power,
            device.connection.control_channel,
            device.volume.value,
            device.volume.hardware_max,
            device.volume.is_muted,
            device.input,
            device.input_name(),
            device.inputs.len(),
            device.connection.detail,
        );
        assert_eq!(device.model, "SR6008");
        assert!(device.volume.value.is_some(), "{id}: no level observed");
        assert_ne!(
            device.power,
            DevicePower::Unknown,
            "{id}: power not observed"
        );
        if device.power == DevicePower::On {
            assert_eq!(
                device.connection.control_channel,
                ControlChannel::Push,
                "{id}: a powered unit should accept the push session"
            );
            assert!(
                device.volume.hardware_max.is_some(),
                "{id}: MVMAX not observed"
            );
        }
    }
    println!("settled in {:?}", started.elapsed());
}
