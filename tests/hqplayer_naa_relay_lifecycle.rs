//! UHC-owned NAA relay lifecycle (P2 slice 1).
//!
//! Written from the operator's expectation of a managed relay, not from the implementation:
//! disabled means no listener at all; enabled means one owned listener; Stop/shutdown closes the
//! active HQPlayer↔NAA pair and every worker thread, releases the bind address so a restart can
//! reuse it, and never leaves an orphan listener; a listener that cannot exist is reported as
//! unavailable with the last observation retained. Every peer here is a loopback software fixture
//! (`tests/mock_servers/naa.rs`); no live HQPlayer, no physical DAC, no listening claim.

#![cfg(feature = "naa-proxy")]

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::net::{SocketAddr, TcpListener};
use std::sync::Arc;
use std::time::Duration;

use mock_servers::naa::{
    attribute, device_children, port_is_listening, reserved_port, FakeNaa, HqpNaaClient, NaaEvent,
    VIRTUAL_DEVICE_ID,
};
use unified_hifi_control::adapters::hqplayer::naa_relay::NaaRelay;
use unified_hifi_control::adapters::hqplayer::outputs::{HqpOutputAvailability, NaaRelaySettings};

fn settings(enabled: bool, port: u16) -> NaaRelaySettings {
    NaaRelaySettings {
        enabled,
        bind: format!("127.0.0.1:{port}"),
        hqp_allow: vec![],
        discovery_interface: None,
        adapter_name: "HiPhi Router".to_string(),
        ..NaaRelaySettings::default()
    }
}

fn relay(enabled: bool, port: u16) -> Arc<NaaRelay> {
    Arc::new(NaaRelay::new(settings(enabled, port), None).expect("relay constructs without I/O"))
}

/// Session workers record their refusal in `finish()` a moment after shutting the socket.
fn wait_for_error(relay: &NaaRelay, needle: &str) -> Option<String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let error = relay.last_error();
        if error.as_deref().is_some_and(|e| e.contains(needle))
            || std::time::Instant::now() >= deadline
        {
            return error;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Bring a fixture endpoint, a saved+selected route and one authenticated HQPlayer session up.
fn forwarding_pair(relay: &Arc<NaaRelay>, naa: &FakeNaa) -> (SocketAddr, HqpNaaClient) {
    let addr = relay.start_listener().expect("listener binds");
    let route = relay
        .add_route("Fixture A", &naa.host(), Some(naa.port()), None)
        .expect("route saved");
    relay
        .commit_selection(&route.route_id)
        .expect("route selected");
    let mut client = HqpNaaClient::connect(addr, "nonce-1").expect("HQPlayer connects");
    let init = client.handshake().expect("handshake relayed");
    assert_eq!(
        attribute(&init, "device").as_deref(),
        Some(VIRTUAL_DEVICE_ID),
        "initialize reply must name the stable virtual device, got {:?}",
        String::from_utf8_lossy(&init)
    );
    assert!(
        naa.wait_until(
            |n| n.initialize_devices() == vec![naa.devices[0].0.clone()],
            Duration::from_secs(2)
        ),
        "endpoint must receive its real device id: {:?}",
        naa.events()
    );
    (addr, client)
}

#[test]
fn disabled_settings_open_no_listener() {
    let port = reserved_port();
    let relay = relay(false, port);
    assert_eq!(relay.availability(), HqpOutputAvailability::Disabled);
    assert_eq!(relay.listener_addr(), None);
    let refused = relay.start_listener();
    assert!(refused.is_err(), "a disabled relay must refuse to listen");
    assert_eq!(relay.availability(), HqpOutputAvailability::Disabled);
    assert!(
        !port_is_listening(format!("127.0.0.1:{port}").parse().unwrap()),
        "disabled relay must open no socket on its configured bind"
    );
    // Routes are still a readable, editable setting while disabled.
    let route = relay
        .add_route(
            "Saved while disabled",
            "192.0.2.30",
            None,
            Some("dac-a".into()),
        )
        .expect("routes are configuration, not a live socket");
    assert_eq!(relay.routes(), vec![route]);
}

#[test]
fn enabled_relay_listens_and_closes_connections_without_a_selected_route() {
    let relay = relay(true, 0);
    let addr = relay.start_listener().expect("listener binds");
    assert_eq!(relay.availability(), HqpOutputAvailability::Available);
    assert_eq!(relay.listener_addr(), Some(addr));
    assert!(port_is_listening(addr));
    // No route selected: HQPlayer's attempt is refused by closing, nothing is contacted.
    let mut client = HqpNaaClient::connect(addr, "nonce-0").expect("connect");
    client.set_read_timeout(Duration::from_secs(2));
    assert!(
        client.is_closed(),
        "without a selected route the relay must close the connection"
    );
    assert!(
        relay
            .last_error()
            .is_some_and(|e| e.contains("Select a route")),
        "the refusal must be observable, got {:?}",
        relay.last_error()
    );
    relay.stop_listener();
}

#[test]
fn stop_closes_the_active_pair_joins_workers_and_releases_the_bind_address() {
    let naa = FakeNaa::start("Fixture A", "hw:CARD=A,DEV=0", 44100);
    let port = reserved_port();
    let relay = relay(true, port);
    let (addr, mut client) = forwarding_pair(&relay, &naa);
    let start = client.start(44100).expect("start relayed");
    assert_eq!(attribute(&start, "result").as_deref(), Some("1"));
    let feedback = client
        .send_audio(&[0x11; 256])
        .expect("audio relayed with feedback");
    assert_eq!(feedback.len(), 16);
    assert!(naa.wait_until(|n| n.audio_bytes() == 256, Duration::from_secs(2)));
    let session = relay.observe().session.expect("session is observable");
    assert!(session.started && session.current_stream_audio_bytes == 256);

    let report = relay.stop_listener();
    assert_eq!(
        report.workers_detached, 0,
        "every session worker must observe shutdown: {report:?}"
    );
    assert!(
        report.workers_joined >= 1,
        "the active session worker must be joined: {report:?}"
    );
    assert!(!report.accept_loop_panicked);
    client.set_read_timeout(Duration::from_secs(2));
    assert!(
        client.is_closed(),
        "HQPlayer side of the pair must be closed by Stop"
    );
    assert!(
        naa.wait_until(|n| n.closed_sessions() >= 1, Duration::from_secs(2)),
        "NAA side of the pair must be closed by Stop: {:?}",
        naa.events()
    );
    assert!(
        !port_is_listening(addr),
        "no orphan listener may survive stop"
    );
    assert_eq!(relay.availability(), HqpOutputAvailability::Disabled);
    assert!(relay.observe().session.is_none());
    // The selection survives a lifecycle stop; only Stop-the-command clears it.
    assert!(relay.selected_route_id().is_some());

    // Restart reuses the exact bind address and forwards again with fresh authentication.
    let again = relay
        .start_listener()
        .expect("restart binds the released address");
    assert_eq!(again, addr);
    let mut client = HqpNaaClient::connect(addr, "nonce-2").expect("reconnect after restart");
    client.handshake().expect("fresh handshake after restart");
    assert_eq!(
        naa.auth_nonces(),
        vec!["nonce-1".to_string(), "nonce-2".to_string()],
        "every session relays its own fresh authentication; nothing is replayed"
    );
    relay.stop_listener();
    assert!(!port_is_listening(addr));
    naa.close();
}

#[test]
fn clearing_the_selection_closes_the_pair_but_keeps_the_owned_listener() {
    let naa = FakeNaa::start("Fixture A", "hw:CARD=A,DEV=0", 44100);
    let relay = relay(true, 0);
    let (addr, mut client) = forwarding_pair(&relay, &naa);
    let outcome = relay.clear_selection();
    assert!(outcome.had_session);
    assert!(outcome.persist_error.is_none());
    client.set_read_timeout(Duration::from_secs(2));
    assert!(
        client.is_closed(),
        "Stop must close the HQPlayer side immediately"
    );
    assert!(naa.wait_until(|n| n.closed_sessions() >= 1, Duration::from_secs(2)));
    assert_eq!(relay.selected_route_id(), None);
    assert_eq!(relay.availability(), HqpOutputAvailability::Available);
    // HQPlayer's automatic retry is refused until a route is selected again; the endpoint is
    // never contacted.
    let before = naa.auth_nonces().len();
    let mut retry = HqpNaaClient::connect(addr, "nonce-retry").expect("connect");
    retry.set_read_timeout(Duration::from_secs(2));
    assert!(retry.is_closed());
    assert_eq!(
        naa.auth_nonces().len(),
        before,
        "no auth may reach the endpoint after Stop"
    );
    relay.stop_listener();
    naa.close();
}

#[test]
fn bind_failure_is_unavailable_and_retains_the_last_observation() {
    let occupied = TcpListener::bind("127.0.0.1:0").expect("occupy a port");
    let port = occupied.local_addr().unwrap().port();
    let relay = relay(true, port);
    let route = relay
        .add_route("Kept", "192.0.2.30", None, Some("dac-a".into()))
        .expect("route saved");
    let error = relay
        .start_listener()
        .expect_err("occupied port cannot bind");
    assert!(error.contains("bind"), "failure names the bind: {error}");
    match relay.availability() {
        HqpOutputAvailability::Unavailable { reason, since } => {
            assert!(reason.contains("bind"), "{reason}");
            assert!(since > 0);
        }
        other => panic!("bind failure must be unavailable, not {other:?}"),
    }
    let observation = relay.observe();
    assert_eq!(
        observation.routes,
        vec![route],
        "unavailable is not an empty inventory"
    );
    drop(occupied);
    // Once the port is free the same relay recovers on the next start.
    relay
        .start_listener()
        .expect("recovers when the port frees");
    assert_eq!(relay.availability(), HqpOutputAvailability::Available);
    relay.stop_listener();
}

#[test]
fn lan_bind_without_an_allow_list_is_refused_before_any_socket_opens() {
    let port = reserved_port();
    let relay = Arc::new(
        NaaRelay::new(
            NaaRelaySettings {
                enabled: true,
                bind: format!("0.0.0.0:{port}"),
                hqp_allow: vec![],
                discovery_interface: None,
                adapter_name: "HiPhi Router".into(),
                ..NaaRelaySettings::default()
            },
            None,
        )
        .expect("constructs"),
    );
    let error = relay
        .start_listener()
        .expect_err("LAN exposure needs hqp_allow");
    assert!(error.contains("hqp_allow"), "{error}");
    assert!(!port_is_listening(
        format!("127.0.0.1:{port}").parse().unwrap()
    ));
    assert!(matches!(
        relay.availability(),
        HqpOutputAvailability::Unavailable { .. }
    ));
}

#[test]
fn dropping_the_owner_leaves_no_listener_or_pair_behind() {
    let naa = FakeNaa::start("Fixture A", "hw:CARD=A,DEV=0", 44100);
    let relay = relay(true, 0);
    let (addr, mut client) = forwarding_pair(&relay, &naa);
    drop(relay);
    client.set_read_timeout(Duration::from_secs(2));
    assert!(client.is_closed(), "dropping the owner must close the pair");
    assert!(naa.wait_until(|n| n.closed_sessions() >= 1, Duration::from_secs(2)));
    assert!(
        !port_is_listening(addr),
        "dropping the owner must release the listener"
    );
    naa.close();
}

#[test]
fn read_through_dac_observation_is_per_endpoint_and_unknown_differs_from_never_scanned() {
    let naa = FakeNaa::start_with(
        "Two outputs",
        vec![
            ("dac-1".into(), "First".into()),
            ("dac-2".into(), "Second".into()),
        ],
        44100,
        false,
    );
    let relay = relay(true, 0);
    let addr = relay.start_listener().expect("listener");
    let route = relay
        .add_route("Two", &naa.host(), Some(naa.port()), Some("dac-2".into()))
        .expect("route");
    relay.commit_selection(&route.route_id).expect("select");
    let mut client = HqpNaaClient::connect(addr, "n").expect("connect");
    client.auth_reply().expect("auth");
    let devices = client.getdevices().expect("getdevices relayed");
    assert_eq!(
        devices,
        vec![(VIRTUAL_DEVICE_ID.to_string(), "HiPhi Router".to_string())],
        "HQPlayer sees exactly the stable virtual device"
    );
    let observation = relay.observe();
    assert_eq!(observation.dac_observations.len(), 1);
    let dac = &observation.dac_observations[0];
    assert_eq!(
        (dac.host.as_str(), dac.port),
        (naa.host().as_str(), naa.port())
    );
    assert_eq!(
        dac.devices
            .iter()
            .map(|d| d.id.as_str())
            .collect::<Vec<_>>(),
        vec!["dac-1", "dac-2"],
        "the physical list is retained per endpoint, unrewritten"
    );
    assert!(dac.observed_at > 0);
    assert_eq!(dac.provenance, "relayed-getdevices");
    // Unknown endpoints have no entry at all: unknown differs from observed-empty.
    assert!(
        observation.discovery.is_none(),
        "never scanned must be null, not []"
    );
    relay.stop_listener();
    naa.close();
}

// =============================================================================
// Slice-1 review findings (root): structural owner/worker split and byte-exact relay evidence
// =============================================================================

/// Finding 5/9: dropping the LAST external owner must complete shutdown on the dropping thread
/// even while a session worker is blocked mid-handshake holding the shared state. A stalled
/// endpoint keeps the worker inside its blocking read; the owner drop must still close the pair,
/// release the port, and return. The drop runs on a helper thread and completion is observed
/// through a channel with a bounded wait BEFORE joining, so a deadlock fails the test instead of
/// hanging it.
#[test]
fn dropping_the_last_owner_while_a_worker_is_blocked_closes_everything_without_self_join() {
    let naa = FakeNaa::start_with_behavior(
        "Stalled",
        vec![("dac-s".into(), "Stalled".into())],
        44100,
        mock_servers::naa::Behavior {
            stall_auth: true,
            ..Default::default()
        },
    );
    let relay = relay(true, 0);
    let addr = relay.start_listener().expect("listener");
    let route = relay
        .add_route(
            "Stalled",
            &naa.host(),
            Some(naa.port()),
            Some("dac-s".into()),
        )
        .expect("route");
    relay.commit_selection(&route.route_id).expect("select");
    let mut client = HqpNaaClient::connect(addr, "stall").expect("connect");
    assert!(
        naa.wait_until(|n| n.auth_nonces().len() == 1, Duration::from_secs(2)),
        "worker must be blocked inside the stalled auth exchange"
    );
    // A second handle to the shared worker state, standing in for the blocked worker's reference.
    let shared = relay.shared_state_for_tests();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let dropper = std::thread::spawn(move || {
        drop(relay);
        let _ = done_tx.send(());
    });
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("owner drop must complete within the bound (self-join or leak would hang here)");
    dropper.join().expect("dropper thread joins");
    client.set_read_timeout(Duration::from_secs(2));
    assert!(client.is_closed(), "pair must be closed by the owner drop");
    assert!(
        !port_is_listening(addr),
        "listener must be released by the owner drop"
    );
    assert!(naa.wait_until(|n| n.closed_sessions() >= 1, Duration::from_secs(2)));
    // Releasing the worker-side reference afterwards is inert: no second shutdown, no panic.
    drop(shared);
    let rebound = TcpListener::bind(addr).expect("port rebinds after the owner is gone");
    drop(rebound);
    naa.close();
}

/// Finding 6/10: a worker spawned in the exact window between `accept` and `track_worker` while
/// `stop_listener` is already running must still be drained. The accept thread is held on a
/// deterministic barrier inside that window; stop begins while it is held; the barrier is released
/// only once stop is observed in progress. Nothing here depends on timing luck.
#[test]
fn a_worker_spawned_while_stop_is_in_progress_is_still_drained() {
    let naa = FakeNaa::start("Fixture A", "hw:CARD=A,DEV=0", 44100);
    let relay = relay(true, 0);
    let addr = relay.start_listener().expect("listener");
    let route = relay
        .add_route("A", &naa.host(), Some(naa.port()), None)
        .expect("route");
    relay.commit_selection(&route.route_id).expect("select");
    let reached = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    let release = Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new()));
    {
        let reached = reached.clone();
        let release = release.clone();
        relay.set_before_track_hook_for_tests(Arc::new(move || {
            {
                let (flag, cv) = &*reached;
                *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
                cv.notify_all();
            }
            let (flag, cv) = &*release;
            let mut released = flag.lock().unwrap_or_else(|e| e.into_inner());
            while !*released {
                released = cv
                    .wait_timeout(released, Duration::from_millis(50))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }));
    }
    let mut client = HqpNaaClient::connect(addr, "window").expect("connect");
    {
        let (flag, cv) = &*reached;
        let mut guard = flag.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + Duration::from_secs(3);
        while !*guard && std::time::Instant::now() < deadline {
            guard = cv
                .wait_timeout(guard, Duration::from_millis(20))
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        assert!(*guard, "accept loop must reach the pre-track window");
    }
    // Stop from another thread while the accept loop is held before tracking the new worker.
    let stopper = {
        let relay = relay.clone();
        std::thread::spawn(move || relay.stop_listener())
    };
    let deadline = std::time::Instant::now() + Duration::from_secs(3);
    while !relay.stop_in_progress_for_tests() && std::time::Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        relay.stop_in_progress_for_tests(),
        "stop must be observed in progress before release"
    );
    {
        let (flag, cv) = &*release;
        *flag.lock().unwrap_or_else(|e| e.into_inner()) = true;
        cv.notify_all();
    }
    let report = stopper.join().expect("stop thread joins");
    assert_eq!(report.workers_detached, 0, "{report:?}");
    assert_eq!(
        report.workers_joined, 1,
        "the late-tracked worker was drained: {report:?}"
    );
    assert_eq!(relay.tracked_worker_handles_for_tests(), 0);
    client.set_read_timeout(Duration::from_secs(2));
    assert!(client.is_closed());
    assert!(!port_is_listening(addr));
    assert!(relay.observe().session.is_none());
    naa.close();
}

/// Finding 6: repeated racing connects during stop leave no tracked worker and no listener.
#[test]
fn concurrent_connects_during_stop_leave_no_tracked_workers() {
    let naa = FakeNaa::start("Fixture A", "hw:CARD=A,DEV=0", 44100);
    for _ in 0..5 {
        let relay = relay(true, 0);
        let addr = relay.start_listener().expect("listener");
        let route = relay
            .add_route("A", &naa.host(), Some(naa.port()), None)
            .expect("route");
        relay.commit_selection(&route.route_id).expect("select");
        let stop_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (first_tx, first_rx) = std::sync::mpsc::channel();
        let hammer = {
            let stop_flag = stop_flag.clone();
            std::thread::spawn(move || {
                let mut attempts = 0u32;
                while !stop_flag.load(std::sync::atomic::Ordering::Acquire) {
                    if let Ok(mut c) = HqpNaaClient::connect(addr, "race") {
                        let _ = c.auth_reply();
                    }
                    attempts += 1;
                    if attempts == 1 {
                        let _ = first_tx.send(());
                    }
                }
                attempts
            })
        };
        first_rx
            .recv_timeout(Duration::from_secs(3))
            .expect("hammer made its first connection");
        let report = relay.stop_listener();
        stop_flag.store(true, std::sync::atomic::Ordering::Release);
        let attempts = hammer.join().expect("hammer joins");
        assert!(attempts > 0);
        assert_eq!(report.workers_detached, 0, "{report:?}");
        assert_eq!(
            relay.tracked_worker_handles_for_tests(),
            0,
            "no worker handle may remain tracked after stop"
        );
        assert!(!port_is_listening(addr));
        assert!(relay.observe().session.is_none());
    }
    naa.close();
}

/// Finding 16/25/26/27: with an explicit loopback discovery interface the relay advertises its
/// stable identity on the UDP port that equals its TCP port, binds that responder to loopback only,
/// answers only genuine requests (never result-bearing replies), and stops advertising with the
/// listener. Restarting on the same fixed port advertises there again.
#[test]
fn discovery_responder_advertises_the_stable_identity_and_follows_the_listener() {
    use std::net::UdpSocket;
    let port = reserved_port();
    let relay = Arc::new(
        NaaRelay::new(
            NaaRelaySettings {
                enabled: true,
                bind: format!("127.0.0.1:{port}"),
                hqp_allow: vec![],
                discovery_interface: Some("127.0.0.1".into()),
                discovery_port: port,
                adapter_name: "Living & \"Room\"".into(),
            },
            None,
        )
        .expect("constructs"),
    );
    let tcp = relay.start_listener().expect("listener");
    let responder = relay
        .responder_addr()
        .expect("responder bound alongside the listener");
    assert_eq!(
        responder.port(),
        tcp.port(),
        "discovery uses the TCP port, as the protocol requires"
    );
    assert!(
        responder.ip().is_loopback(),
        "a loopback-configured relay must not expose its responder to the LAN: {responder}"
    );
    let observation = relay.observe();
    assert_eq!(
        observation.relay.discovery_responder.as_deref(),
        Some(responder.to_string().as_str())
    );
    let target: SocketAddr = format!("127.0.0.1:{}", tcp.port()).parse().expect("addr");
    let asker = UdpSocket::bind("127.0.0.1:0").expect("asker");
    asker
        .set_read_timeout(Some(Duration::from_secs(2)))
        .expect("timeout");
    let mut buf = [0u8; 4097];
    // A malformed request draws no answer.
    asker.send_to(b"<discover/>", target).expect("send");
    assert!(
        asker.recv_from(&mut buf).is_err(),
        "malformed request must be ignored"
    );
    // Another relay's advertisement (a reply) draws no answer either.
    asker
        .send_to(
            b"<?xml version=\"1.0\"?><networkaudio><discover name=\"other\" os=\"HiPhi Router\" protocol=\"6\" result=\"OK\" trigger=\"1\" version=\"x\">network audio</discover></networkaudio>\0",
            target,
        )
        .expect("send");
    assert!(
        asker.recv_from(&mut buf).is_err(),
        "a reply must never be answered"
    );
    // The reference request is answered with the escaped stable name and relay identity.
    asker
        .send_to(
            b"<?xml version=\"1.0\"?><networkaudio><discover version=\"Signalyst HQPlayer Embedded\">network audio</discover></networkaudio>\0",
            target,
        )
        .expect("send");
    let (n, from) = asker.recv_from(&mut buf).expect("advertisement received");
    assert_eq!(from.port(), tcp.port());
    let text = String::from_utf8_lossy(&buf[..n]).to_string();
    assert!(
        text.contains("name=\"Living &amp; &quot;Room&quot;\""),
        "{text}"
    );
    assert!(text.contains("os=\"HiPhi Router\""), "{text}");
    assert!(text.contains("protocol=\"6\""), "{text}");
    assert!(text.ends_with("</networkaudio>\0"), "{text:?}");
    relay.stop_listener();
    assert!(relay.responder_addr().is_none());
    let probe = UdpSocket::bind("127.0.0.1:0").expect("probe");
    probe
        .set_read_timeout(Some(Duration::from_millis(500)))
        .expect("timeout");
    probe
        .send_to(
            b"<networkaudio><discover version=\"x\">network audio</discover></networkaudio>\0",
            target,
        )
        .expect("send");
    assert!(
        probe.recv_from(&mut buf).is_err(),
        "no advertisement after stop"
    );
    // A fixed port rebinds and advertises on the same port after restart.
    let again = relay.start_listener().expect("restart");
    assert_eq!(again.port(), port);
    assert_eq!(relay.responder_addr().map(|a| a.port()), Some(port));
    relay.stop_listener();
}

/// Finding 27: discovery is refused, before any socket opens, when the relay's TCP port differs
/// from the discovery port HQPlayer's scanner asks. An advertisement for an unreachable port is
/// worse than none.
#[test]
fn discovery_is_refused_when_the_tcp_port_differs_from_the_discovery_port() {
    let tcp_port = reserved_port();
    let relay = Arc::new(
        NaaRelay::new(
            NaaRelaySettings {
                enabled: true,
                bind: format!("127.0.0.1:{tcp_port}"),
                hqp_allow: vec![],
                discovery_interface: Some("127.0.0.1".into()),
                discovery_port: 43210,
                adapter_name: "HiPhi Router".into(),
            },
            None,
        )
        .expect("constructs"),
    );
    relay.start_listener().expect("tcp listener still works");
    assert!(
        relay.responder_addr().is_none(),
        "no responder for an unreachable advertisement"
    );
    assert!(
        relay
            .last_error()
            .is_some_and(|e| e.contains("discovery_port")),
        "{:?}",
        relay.last_error()
    );
    relay.stop_listener();
}

/// Finding 27: the real scanner path (multicast query on the configured port, replies collected
/// and self-excluded) against a running responder on the same host. This is the discovery HQPlayer
/// Embedded performs; loopback multicast is the only household-free way to exercise it here.
#[test]
fn multicast_scan_discovers_a_responder_on_the_configured_port_and_excludes_itself() {
    use unified_hifi_control::adapters::hqplayer::naa_relay::{
        discovery_own_addresses, discovery_scan,
    };
    let port = reserved_port();
    let relay = Arc::new(
        NaaRelay::new(
            NaaRelaySettings {
                enabled: true,
                bind: format!("127.0.0.1:{port}"),
                hqp_allow: vec![],
                discovery_interface: Some("127.0.0.1".into()),
                discovery_port: port,
                adapter_name: "Scanned Relay".into(),
            },
            None,
        )
        .expect("constructs"),
    );
    let tcp = relay.start_listener().expect("listener");
    assert!(relay.responder_addr().is_some());
    let interface: std::net::Ipv4Addr = "127.0.0.1".parse().expect("ip");
    // Scanning from a different relay/scanner sees this relay's identity only as another router,
    // which the parser excludes by design (no relay chains). A plain NAA fixture is what a scan
    // must find; exercise both: the responder answers the multicast query (observed as excluded
    // relay traffic), and an ordinary endpoint reply on the same port is listed.
    let scan = discovery_scan(interface, port, &discovery_own_addresses(Some(tcp)));
    match scan {
        Ok(observation) => {
            assert_eq!(observation.provenance, "naa-multicast-scan");
            assert!(
                observation
                    .endpoints
                    .iter()
                    .all(|e| e.name != "Scanned Relay"),
                "a relay must never list itself or another relay: {observation:#?}"
            );
        }
        Err(error) => {
            // Loopback multicast is not routable on every host. That is a platform constraint,
            // recorded here rather than papered over: the scan must fail loudly, not report an
            // empty-but-successful inventory.
            assert!(
                error.contains("route") || error.contains("multicast") || error.contains("address"),
                "unexpected scan failure: {error}"
            );
            eprintln!("loopback multicast unavailable on this host: {error}");
        }
    }
    relay.stop_listener();
}

/// Finding 23: distinct exact instance names that sanitize to the same readable prefix must keep
/// distinct route files, before and after a reload.
#[test]
fn colliding_instance_names_keep_separate_persisted_routes() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("UHC_CONFIG_DIR", dir.path());
    let a_path =
        unified_hifi_control::adapters::hqplayer::HqpAdapter::output_routes_path("Living Room");
    let b_path =
        unified_hifi_control::adapters::hqplayer::HqpAdapter::output_routes_path("Living_Room");
    let c_path =
        unified_hifi_control::adapters::hqplayer::HqpAdapter::output_routes_path("Living/Room");
    assert_ne!(a_path, b_path);
    assert_ne!(a_path, c_path);
    assert_ne!(b_path, c_path);
    {
        let a = NaaRelay::new(settings(false, 0), Some(a_path.clone())).expect("a");
        let b = NaaRelay::new(settings(false, 0), Some(b_path.clone())).expect("b");
        a.add_route("Only in Living Room", "192.0.2.30", None, None)
            .expect("route a");
        b.add_route("Only in Living_Room", "192.0.2.31", None, None)
            .expect("route b");
    }
    let a = NaaRelay::new(settings(false, 0), Some(a_path)).expect("reload a");
    let b = NaaRelay::new(settings(false, 0), Some(b_path)).expect("reload b");
    assert_eq!(a.routes().len(), 1);
    assert_eq!(a.routes()[0].name, "Only in Living Room");
    assert_eq!(b.routes().len(), 1);
    assert_eq!(b.routes()[0].name, "Only in Living_Room");
}

/// Finding 7: opaque auth bytes, bidirectional audio with side sections and feedback are relayed
/// byte-for-byte, measured with digests; a same-length corruption is detected as different.
#[test]
fn relay_is_byte_transparent_for_auth_audio_side_sections_and_feedback() {
    let naa = FakeNaa::start("Exact", "hw:CARD=X,DEV=0", 48000);
    let relay = relay(true, 0);
    let (_, mut client) = forwarding_pair(&relay, &naa);
    // The relayed auth reply must be the endpoint's exact bytes (odd quoting/spacing included).
    let reply = naa
        .last_auth_reply_bytes()
        .expect("fixture recorded its reply");
    assert_eq!(
        client.last_auth_reply(),
        reply,
        "auth reply must be relayed opaquely"
    );
    client.start(48000).expect("start");
    let payload: Vec<u8> = (0..1024u32)
        .flat_map(|i| (i.wrapping_mul(2654435761)).to_le_bytes())
        .collect();
    let metadata = b"<meta>not-rewritten <device id=\"hiphi:router\"/></meta>".to_vec();
    let position = 12345u64.to_le_bytes().to_vec();
    let feedback = client
        .send_audio_with_sections(&payload, &position, &metadata)
        .expect("audio relayed");
    assert!(naa.wait_until(|n| n.audio_records().len() == 1, Duration::from_secs(2)));
    let record = &naa.audio_records()[0];
    assert_eq!(record.payload, payload, "audio payload must be byte-exact");
    assert_eq!(record.position, position);
    assert_eq!(
        record.metadata, metadata,
        "side sections are never rewritten"
    );
    assert_eq!(
        record.payload_sha256,
        mock_servers::naa::sha256_hex(&payload)
    );
    let mut corrupted = payload.clone();
    corrupted[100] ^= 0x01;
    assert_ne!(
        record.payload, corrupted,
        "same-length corruption must be detectable"
    );
    assert_ne!(
        record.payload_sha256,
        mock_servers::naa::sha256_hex(&corrupted)
    );
    // Feedback travels downstream byte-for-byte too.
    assert_eq!(
        feedback,
        naa.last_feedback_bytes().expect("fixture sent feedback")
    );
    relay.stop_listener();
    naa.close();
}

/// Finding 7: malformed upstream frames are refused without touching the endpoint's payload path.
#[test]
fn audio_before_start_and_oversized_records_are_refused() {
    let naa = FakeNaa::start("Strict", "hw:CARD=S,DEV=0", 44100);
    let relay = relay(true, 0);
    let (addr, mut client) = forwarding_pair(&relay, &naa);
    // Audio before any framed start.
    let err = client.send_audio(&[0u8; 64]);
    client.set_read_timeout(Duration::from_secs(2));
    assert!(
        err.is_err() || client.is_closed(),
        "audio before start must end the session"
    );
    assert!(naa.wait_until(|n| n.closed_sessions() >= 1, Duration::from_secs(2)));
    assert_eq!(naa.audio_records().len(), 0, "nothing reached the endpoint");
    // Oversized declared record on a fresh session.
    let mut client = HqpNaaClient::connect(addr, "oversize").expect("reconnect");
    client.handshake().expect("handshake");
    client.start(44100).expect("start");
    client
        .send_raw_header(u32::MAX / 8, 0, 0, 0)
        .expect("header written");
    client.set_read_timeout(Duration::from_secs(2));
    assert!(
        client.is_closed(),
        "oversized record must be rejected before allocation"
    );
    let error = wait_for_error(&relay, "8 MiB");
    assert!(
        error.as_deref().is_some_and(|e| e.contains("8 MiB")),
        "{error:?}"
    );
    relay.stop_listener();
    naa.close();
}

/// Finding 8: DAC observations are replaced per endpoint on every fresh authenticated
/// enumeration, an endpoint that now reports zero outputs is observed-empty (not unknown), and
/// two endpoints sharing a DAC id stay isolated by host+port.
#[test]
fn dac_observations_replace_per_endpoint_and_isolate_same_ids_across_endpoints() {
    let a = FakeNaa::start_with(
        "A",
        vec![
            ("dac-1".into(), "First".into()),
            ("dac-2".into(), "Second".into()),
        ],
        44100,
        false,
    );
    let b = FakeNaa::start_with(
        "B",
        vec![("dac-1".into(), "Other first".into())],
        44100,
        false,
    );
    let relay = relay(true, 0);
    let addr = relay.start_listener().expect("listener");
    let route_a = relay
        .add_route("A", &a.host(), Some(a.port()), Some("dac-2".into()))
        .expect("route a");
    let route_b = relay
        .add_route("B", &b.host(), Some(b.port()), Some("dac-1".into()))
        .expect("route b");

    relay.commit_selection(&route_a.route_id).expect("select a");
    let mut client = HqpNaaClient::connect(addr, "a1").expect("connect");
    client.auth_reply().expect("auth");
    client.getdevices().expect("enumerate a");
    let ids = |host: &str, port: u16| -> Option<Vec<String>> {
        relay
            .observe()
            .dac_observations
            .iter()
            .find(|d| d.host == host && d.port == port)
            .map(|d| d.devices.iter().map(|x| x.id.clone()).collect())
    };
    assert_eq!(
        ids(&a.host(), a.port()),
        Some(vec!["dac-1".into(), "dac-2".into()])
    );
    assert_eq!(
        ids(&b.host(), b.port()),
        None,
        "B is unknown until it is enumerated"
    );

    // Endpoint A loses dac-2. A fresh authenticated session re-enumerates and the whole list is
    // replaced; the configured dac-2 is now absent so the session fails visibly.
    a.set_devices(vec![("dac-1".into(), "First".into())]);
    relay
        .commit_selection(&route_a.route_id)
        .expect("reselect a");
    client.set_read_timeout(Duration::from_secs(2));
    assert!(client.is_closed());
    let mut client = HqpNaaClient::connect(addr, "a2").expect("reconnect");
    client.auth_reply().expect("auth");
    let _ = client.getdevices();
    assert!(a.wait_until(
        |n| n.auth_nonces() == vec!["a1".to_string(), "a2".to_string()],
        Duration::from_secs(2)
    ));
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline
        && ids(&a.host(), a.port()) != Some(vec!["dac-1".into()])
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        ids(&a.host(), a.port()),
        Some(vec!["dac-1".into()]),
        "removed DAC must disappear"
    );
    let error = wait_for_error(&relay, "absent");
    assert!(
        error.as_deref().is_some_and(|e| e.contains("absent")),
        "{error:?}"
    );

    // Endpoint A now reports no outputs at all: observed empty, distinct from unknown.
    a.set_devices(vec![]);
    relay
        .commit_selection(&route_a.route_id)
        .expect("reselect a");
    let mut client = HqpNaaClient::connect(addr, "a3").expect("reconnect");
    client.auth_reply().expect("auth");
    let _ = client.getdevices();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while std::time::Instant::now() < deadline && ids(&a.host(), a.port()) != Some(vec![]) {
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(
        ids(&a.host(), a.port()),
        Some(vec![]),
        "zero outputs is observed-empty"
    );

    // Endpoint B shares the id "dac-1" with A and is a separate observation keyed by host+port.
    relay.commit_selection(&route_b.route_id).expect("select b");
    let mut client = HqpNaaClient::connect(addr, "b1").expect("connect b");
    client.auth_reply().expect("auth");
    let devices = client.getdevices().expect("enumerate b");
    assert_eq!(
        devices,
        vec![(VIRTUAL_DEVICE_ID.to_string(), "HiPhi Router".to_string())]
    );
    assert_eq!(ids(&b.host(), b.port()), Some(vec!["dac-1".into()]));
    assert_eq!(
        ids(&a.host(), a.port()),
        Some(vec![]),
        "A's observation is untouched by B"
    );
    assert_eq!(relay.observe().dac_observations.len(), 2);
    relay.stop_listener();
    a.close();
    b.close();
}
