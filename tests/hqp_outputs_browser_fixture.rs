//! Opt-in software peers for a separately running, real Dioxus UHC server.
//!
//! This is a fixture launcher, not a passing UI test. It supplies native HQPlayer and NAA wire
//! peers only. Configure the actual server using the emitted manifest, then exercise its public
//! APIs and hydrated page. No API response or UHC projection is manufactured here.
#![cfg(feature = "naa-proxy")]

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::net::{SocketAddr, TcpListener};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use mock_servers::hqplayer::corpus::VERIFIED_PROFILE;
use mock_servers::hqplayer::model::{DaemonModel, Metadata};
use mock_servers::hqplayer::wire::{WirePolicy, WireServer};
use mock_servers::naa::{auto_client_payload, AutoHqpClient, FakeNaa};
use serde_json::json;

fn unused_loopback_address() -> SocketAddr {
    // This reserves no long-lived listener: the real UHC relay must bind it. A collision remains
    // observable as a production relay bind failure, not hidden by a substitute fixture.
    TcpListener::bind("127.0.0.1:0")
        .expect("find loopback port")
        .local_addr()
        .expect("loopback address")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "manual bounded peer launcher; does not itself qualify UI behavior"]
async fn browser_software_peers() {
    let directory = PathBuf::from(
        std::env::var("UHC_BROWSER_FIXTURE_DIR")
            .expect("set UHC_BROWSER_FIXTURE_DIR to a fresh private directory"),
    );
    std::fs::create_dir_all(&directory).expect("fixture directory");
    let manifest = directory.join("peers.json");
    assert!(!manifest.exists(), "use a fresh directory for each run");
    let stop_file = directory.join("stop");
    assert!(!stop_file.exists(), "stop marker already exists");
    let lifetime = std::env::var("UHC_BROWSER_FIXTURE_SECONDS")
        .ok()
        .map(|s| s.parse::<u64>().expect("positive integer fixture seconds"))
        .unwrap_or(1800);
    assert!(
        (1..=7200).contains(&lifetime),
        "fixture lifetime must be 1–7200 seconds"
    );

    let mut daemons = Vec::new();
    let mut models = Vec::new();
    let mut clients = Vec::new();
    let mut instances = Vec::new();
    // Deliberately exercise names that used to collide in persistence paths.
    for name in ["Browser Room", "Browser_Room"] {
        let model = DaemonModel::with_profile(VERIFIED_PROFILE);
        model.external_change(|state| {
            state.playback = 0;
            state.track = 3;
            state.track_id = "t-3".to_string();
            state.position = 41;
            state.length = 215;
            state.metadata = Some(Metadata::sample());
        });
        let daemon = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
        let relay_address = unused_loopback_address();
        let client = AutoHqpClient::start(relay_address, 44100);
        instances.push(json!({
            "instance": name,
            "zone_id": format!("hqplayer:{name}"),
            "native_address": daemon.addr().to_string(),
            "relay_bind": relay_address.to_string(),
            "discovery_port": relay_address.port(),
            "note": "custom port for isolated peers; not Embedded standard-port discovery proof"
        }));
        models.push(model);
        clients.push(client);
        daemons.push(daemon);
    }
    let destinations = [
        FakeNaa::start_with(
            "Software DAC A",
            vec![
                ("dac-a".into(), "Software DAC A".into()),
                ("dac-a-alt".into(), "Software DAC A alternate".into()),
            ],
            44100,
            false,
        ),
        FakeNaa::start("Software DAC B", "dac-b", 44100),
    ];
    let info = json!({
        "pid": std::process::id(),
        "lifetime_seconds": lifetime,
        "stop_file": stop_file,
        "instances": instances,
        "destinations": [
            {"name":"Software DAC A", "address":destinations[0].addr().to_string(), "device_id":"dac-a"},
            {"name":"Software DAC B", "address":destinations[1].addr().to_string(), "device_id":"dac-b"}
        ],
        "evidence": "software peers only; actual UHC server and browser run separately"
    });
    std::fs::write(&manifest, serde_json::to_vec_pretty(&info).unwrap()).expect("write manifest");
    println!("BROWSER_PEERS_READY {}", manifest.display());

    let started = Instant::now();
    while started.elapsed() < Duration::from_secs(lifetime) && !stop_file.exists() {
        for (client, model) in clients.iter().zip(&models) {
            client.set_playing(model.state().playback == 2);
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    for client in clients {
        client.close();
    }
    let expected = auto_client_payload();
    let evidence: Vec<_> = destinations.iter().map(|naa| {
        let records = naa.audio_records();
        json!({
            "address": naa.addr().to_string(),
            "records": records.len(),
            "audio_bytes": naa.audio_bytes(),
            "all_received_payloads_match": !records.is_empty() && records.iter().all(|r| r.payload == expected)
        })
    }).collect();
    std::fs::write(
        directory.join("final-evidence.json"),
        serde_json::to_vec_pretty(&evidence).unwrap(),
    )
    .expect("write software evidence");
    for daemon in daemons {
        daemon.shutdown().await;
    }
    println!("BROWSER_PEERS_STOPPED");
}
