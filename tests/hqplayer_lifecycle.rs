//! Managed HQPlayer producer lifecycle conformance (issue #162).
//!
//! These tests use the same stateful wire daemon as the native-protocol suite. The lifecycle is the
//! client under test: no live HQPlayer is contacted, and no route handler is allowed to stand in for
//! continuous observation or recovery.

#[allow(dead_code, unused_imports, unused_variables)]
mod mock_servers;

use std::sync::{Arc, Once};
use std::time::Duration;

use mock_servers::hqplayer::corpus::VERIFIED_PROFILE;
use mock_servers::hqplayer::model::{DaemonModel, Metadata};
use mock_servers::hqplayer::wire::{Chunking, WirePolicy, WireServer};
use unified_hifi_control::adapters::hqplayer::{HqpAdapter, HqpInstanceManager, HqpTimeouts};
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::aggregator::ZoneAggregator;
use unified_hifi_control::bus::{create_bus, BusEvent, PlaybackState, Zone};

fn isolate_config_dir() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let dir = std::env::temp_dir().join(format!("uhc-hqp-lifecycle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create isolated lifecycle config dir");
        std::env::set_var("UHC_CONFIG_DIR", dir);
    });
}

fn fast_timeouts() -> HqpTimeouts {
    HqpTimeouts {
        connect: Duration::from_millis(100),
        response: Duration::from_millis(500),
        reconnect_delay: Duration::from_millis(10),
        max_attempts: 1,
    }
}

async fn advance_until_zone(
    aggregator: &ZoneAggregator,
    mut condition: impl FnMut(&Zone) -> bool,
) -> Zone {
    for _ in 0..100 {
        if let Some(zone) = aggregator.get_zone("hqplayer:rig").await {
            if condition(&zone) {
                return zone;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!(
        "zone did not settle inside the bounded polling budget; zones={:?}",
        aggregator.get_zones().await
    );
}

/// The coordinator owns one logical HQPlayer lifecycle. Individual configured instances are child
/// workers of that supervisor, so shutdown has one adapter-wide acknowledgment and one place that
/// reconciles runtime additions/removals.
///
/// **Label: client-red.** Issue #162 rejects a default-instance-only lifecycle.
#[test]
fn the_instance_manager_is_the_managed_hqplayer_adapter() {
    fn assert_startable<T: Startable>() {}
    assert_startable::<HqpInstanceManager>();
}

/// Reachability starts after a coherent handshake, not after TCP accept. A malformed required
/// `Status` response must never publish a placeholder zone or leave the adapter connected.
///
/// **Label: client-red.** The pre-lifecycle `connect()` swallowed Status failure with `default()`.
#[tokio::test]
async fn a_failed_initial_status_never_becomes_a_connected_partial_zone() {
    isolate_config_dir();
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    let server = WireServer::start(
        Arc::new(model.clone()),
        WirePolicy {
            // Syntactically valid but semantically empty. Framing alone must not turn absent
            // authoritative fields into a stopped/fixed-volume zone through parser defaults.
            malformed_for_element: Some((
                "Status".to_string(),
                "<?xml version=\"1.0\"?><Status/>".to_string(),
            )),
            ..WirePolicy::default()
        },
    )
    .await;
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let aggregator_task = {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run().await })
    };
    tokio::task::yield_now().await;

    let manager = HqpInstanceManager::new(bus.clone());
    let adapter = manager
        .add_instance(
            "rig".to_string(),
            "127.0.0.1".to_string(),
            Some(server.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;

    manager.start().await.expect("start managed lifecycle");
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    for _ in 0..100 {
        if server.stats().element_count("Status") > 0 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        server.stats().element_count("Status") > 0,
        "the RED must exercise the failed handshake"
    );
    assert!(!adapter.get_status().await.connected);
    assert!(aggregator.get_zone("hqplayer:rig").await.is_none());

    manager.stop().await;
    bus.publish(BusEvent::ShuttingDown { reason: None });
    aggregator_task.await.expect("aggregator stops");
    server.stop();
}

/// A full observed snapshot is the convergence primitive. An external controller changing playback,
/// decimal volume and metadata must replace the aggregator-owned zone without any page/API read.
///
/// **Label: client-red.** Before #162 no HQPlayer task polls after startup.
#[tokio::test]
async fn external_changes_converge_into_the_aggregator_without_a_surface_read() {
    isolate_config_dir();
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    model.external_change(|s| {
        s.playback = 2;
        s.track = 1;
        s.track_id = "first".to_string();
        s.position = 10;
        s.length = 300;
        s.metadata = Some(Metadata::sample());
    });
    let server = WireServer::start(Arc::new(model.clone()), WirePolicy::default()).await;
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let aggregator_task = {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run().await })
    };
    tokio::task::yield_now().await;

    let manager = HqpInstanceManager::new(bus.clone());
    let adapter = manager
        .add_instance(
            "rig".to_string(),
            "127.0.0.1".to_string(),
            Some(server.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    manager.start().await.expect("start managed lifecycle");

    let first = advance_until_zone(&aggregator, |_| true).await;
    assert_eq!(first.state, PlaybackState::Playing);
    assert_eq!(
        first.now_playing.as_ref().map(|n| n.title.as_str()),
        Some("Alice in Wonderland")
    );
    assert_eq!(first.volume_control.as_ref().map(|v| v.value), Some(-23.5));

    model.external_change(|s| {
        s.playback = 1;
        s.track = 2;
        s.track_id = "second".to_string();
        s.position = 77;
        s.volume_db = -31.5;
        s.metadata = Some(Metadata {
            artist: "Nina Simone".to_string(),
            album: "Pastel Blues".to_string(),
            song: "Sinnerman".to_string(),
            samplerate: 96_000,
            bits: 24,
            channels: 2,
            bitrate: 4_608,
        });
    });

    let changed = advance_until_zone(&aggregator, |z| {
        z.now_playing
            .as_ref()
            .map(|n| n.title == "Sinnerman")
            .unwrap_or(false)
    })
    .await;
    assert_eq!(changed.state, PlaybackState::Paused);
    assert_eq!(
        changed.now_playing.as_ref().map(|n| n.artist.as_str()),
        Some("Nina Simone")
    );
    assert_eq!(
        changed.now_playing.as_ref().and_then(|n| n.seek_position),
        Some(77.0)
    );
    assert_eq!(
        changed.volume_control.as_ref().map(|v| v.value),
        Some(-31.5)
    );

    // A definitive server-side clear replaces the whole snapshot. Delta-only publication cannot
    // express either of these transitions: NowPlayingChanged(None, ..) currently constructs an
    // empty Some(NowPlaying), and VolumeChanged cannot remove a volume control.
    model.external_change(|s| {
        s.playback = 0;
        s.track = 0;
        s.track_id.clear();
        s.position = 0;
        s.length = 0;
        s.metadata = None;
        s.volume_range.enabled = false;
    });

    let cleared = advance_until_zone(&aggregator, |z| {
        z.now_playing.is_none() && z.volume_control.is_none()
    })
    .await;
    assert_eq!(cleared.state, PlaybackState::Stopped);

    manager.stop().await;
    for _ in 0..100 {
        if aggregator.get_zone("hqplayer:rig").await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(aggregator.get_zone("hqplayer:rig").await.is_none());
    bus.publish(BusEvent::ShuttingDown { reason: None });
    aggregator_task.await.expect("aggregator stops");
    server.stop();
}

/// A reconnect has durable provenance even when a consumer never samples the short disconnected
/// edge. Session identity, rather than a transient boolean, is what makes later snapshot ordering
/// unambiguous.
///
/// **Label: client-red.** Issue #162 requires a monotonic producer epoch after each fresh handshake.
#[tokio::test]
async fn a_reconnected_session_advances_the_producer_epoch() {
    isolate_config_dir();
    let model = DaemonModel::with_profile(VERIFIED_PROFILE);
    let server = WireServer::start(Arc::new(model), WirePolicy::default()).await;
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let aggregator_task = {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run().await })
    };
    tokio::task::yield_now().await;

    let manager = HqpInstanceManager::new(bus.clone());
    let adapter = manager
        .add_instance(
            "rig".to_string(),
            "127.0.0.1".to_string(),
            Some(server.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    manager.start().await.expect("start managed lifecycle");
    advance_until_zone(&aggregator, |_| true).await;

    let first_epoch = adapter.producer_epoch().await;
    assert!(first_epoch > 0, "the initial coherent session has an epoch");

    // Deliberately do not observe `connected=false` or the temporary ZoneRemoved event here. The
    // next coherent snapshot must still carry a distinguishable session identity.
    adapter.disconnect().await;
    for _ in 0..100 {
        if adapter.producer_epoch().await > first_epoch
            && aggregator.get_zone("hqplayer:rig").await.is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    assert_eq!(
        adapter.producer_epoch().await,
        first_epoch.wrapping_add(1),
        "one fresh coherent handshake advances the epoch exactly once"
    );
    assert!(aggregator.get_zone("hqplayer:rig").await.is_some());

    manager.stop().await;
    bus.publish(BusEvent::ShuttingDown { reason: None });
    aggregator_task.await.expect("aggregator stops");
    server.stop();
}

/// Runtime instance changes reconcile with the managed worker set. Adding a configured instance
/// starts observation without restarting UHC; removing one stops and joins its worker while leaving
/// unrelated instances healthy.
///
/// **Label: client-red.** The pre-reconciliation manager snapshots instances only in `start()`.
#[tokio::test]
async fn runtime_instance_add_and_remove_reconcile_managed_workers() {
    isolate_config_dir();
    let first_server = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let second_server = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let aggregator_task = {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run().await })
    };
    tokio::task::yield_now().await;

    let manager = HqpInstanceManager::new(bus.clone());
    let first = manager
        .add_instance(
            "first".to_string(),
            "127.0.0.1".to_string(),
            Some(first_server.port()),
            None,
            None,
            None,
        )
        .await;
    first.set_timeouts(fast_timeouts()).await;
    manager.start().await.expect("start managed lifecycle");
    for _ in 0..100 {
        if aggregator.get_zone("hqplayer:first").await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(aggregator.get_zone("hqplayer:first").await.is_some());

    let second = manager
        .add_instance(
            "second".to_string(),
            "127.0.0.1".to_string(),
            Some(second_server.port()),
            None,
            None,
            None,
        )
        .await;
    second.set_timeouts(fast_timeouts()).await;
    for _ in 0..50 {
        if aggregator.get_zone("hqplayer:second").await.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        aggregator.get_zone("hqplayer:second").await.is_some(),
        "a runtime addition must get a managed observer without process restart"
    );

    assert!(manager.remove_instance("first").await);
    for _ in 0..50 {
        if aggregator.get_zone("hqplayer:first").await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(aggregator.get_zone("hqplayer:first").await.is_none());
    assert!(
        aggregator.get_zone("hqplayer:second").await.is_some(),
        "removing one instance must not tear down another instance's lane"
    );

    let unavailable = std::net::TcpListener::bind(("127.0.0.1", 0))
        .expect("reserve unavailable replacement address");
    let unavailable_port = unavailable
        .local_addr()
        .expect("unavailable replacement address")
        .port();
    drop(unavailable);
    second
        .configure(
            "127.0.0.1".to_string(),
            Some(unavailable_port),
            None,
            None,
            None,
        )
        .await;
    for _ in 0..50 {
        if aggregator.get_zone("hqplayer:second").await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        aggregator.get_zone("hqplayer:second").await.is_none(),
        "reconfiguration removes the old reachable projection before replacement recovery"
    );
    assert!(second.get_status().await.info.is_none());

    manager.stop().await;
    bus.publish(BusEvent::ShuttingDown { reason: None });
    aggregator_task.await.expect("aggregator stops");
    first_server.stop();
    second_server.stop();
}

/// An unavailable daemon does not become a partial zone, then converges when it appears. A later
/// daemon restart on the same endpoint removes the old zone, advances session provenance and
/// republishes the replacement daemon's authoritative metadata.
///
/// **Label: client-pin.** This covers unavailable startup, mid-poll EOF, delayed recovery and restart.
#[tokio::test]
async fn unavailable_startup_and_daemon_restart_recover_without_stale_state() {
    isolate_config_dir();
    let reservation =
        std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve restart test address");
    let restart_addr = reservation.local_addr().expect("reserved local address");
    drop(reservation);

    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let aggregator_task = {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run().await })
    };
    tokio::task::yield_now().await;

    let manager = HqpInstanceManager::new(bus.clone());
    let adapter = manager
        .add_instance(
            "rig".to_string(),
            restart_addr.ip().to_string(),
            Some(restart_addr.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    manager
        .start()
        .await
        .expect("an unavailable child does not prevent lifecycle startup");
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!adapter.get_status().await.connected);
    assert!(aggregator.get_zone("hqplayer:rig").await.is_none());

    let first_model = DaemonModel::with_profile(VERIFIED_PROFILE);
    first_model.external_change(|s| {
        s.playback = 2;
        s.track = 1;
        s.track_id = "before-restart".to_string();
        s.metadata = Some(Metadata::sample());
    });
    let first_server =
        WireServer::start_on(restart_addr, Arc::new(first_model), WirePolicy::default()).await;
    let first_zone = advance_until_zone(&aggregator, |z| {
        z.now_playing
            .as_ref()
            .map(|n| n.title == "Alice in Wonderland")
            .unwrap_or(false)
    })
    .await;
    assert_eq!(first_zone.state, PlaybackState::Playing);
    let first_epoch = adapter.producer_epoch().await;

    first_server.shutdown().await;
    for _ in 0..100 {
        if aggregator.get_zone("hqplayer:rig").await.is_none() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        aggregator.get_zone("hqplayer:rig").await.is_none(),
        "the pre-restart snapshot must not remain presented as reachable"
    );

    let second_model = DaemonModel::with_profile(VERIFIED_PROFILE);
    second_model.external_change(|s| {
        s.playback = 1;
        s.track = 2;
        s.track_id = "after-restart".to_string();
        s.metadata = Some(Metadata {
            artist: "Restarted Artist".to_string(),
            album: "Restarted Album".to_string(),
            song: "After Restart".to_string(),
            samplerate: 192_000,
            bits: 24,
            channels: 2,
            bitrate: 9_216,
        });
    });
    let second_server =
        WireServer::start_on(restart_addr, Arc::new(second_model), WirePolicy::default()).await;
    let recovered = advance_until_zone(&aggregator, |z| {
        z.now_playing
            .as_ref()
            .map(|n| n.title == "After Restart")
            .unwrap_or(false)
    })
    .await;
    assert_eq!(recovered.state, PlaybackState::Paused);
    assert_eq!(
        recovered.now_playing.as_ref().map(|n| n.artist.as_str()),
        Some("Restarted Artist")
    );
    assert_eq!(
        adapter.producer_epoch().await,
        first_epoch.wrapping_add(1),
        "the replacement daemon is a new coherent producer session"
    );

    manager.stop().await;
    bus.publish(BusEvent::ShuttingDown { reason: None });
    aggregator_task.await.expect("aggregator stops");
    second_server.stop();
}

/// Lifecycle start is idempotent, stop returns only after the child has disconnected, and the same
/// manager can start one fresh worker afterwards. This pins both clean shutdown and the absence of
/// duplicate producers.
///
/// **Label: client-pin.** Cancellation safety is observable through connection/session counts.
#[tokio::test]
async fn clean_shutdown_joins_children_and_restart_creates_one_fresh_producer() {
    isolate_config_dir();
    let server = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let bus = create_bus();
    let aggregator = Arc::new(ZoneAggregator::new(bus.clone()));
    let aggregator_task = {
        let aggregator = aggregator.clone();
        tokio::spawn(async move { aggregator.run().await })
    };
    tokio::task::yield_now().await;

    let manager = HqpInstanceManager::new(bus.clone());
    let adapter = manager
        .add_instance(
            "rig".to_string(),
            "127.0.0.1".to_string(),
            Some(server.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;

    manager.start().await.expect("initial lifecycle start");
    advance_until_zone(&aggregator, |_| true).await;
    let first_epoch = adapter.producer_epoch().await;
    let first_connections = server.stats().connections();

    manager
        .start()
        .await
        .expect("duplicate start is idempotent");
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(adapter.producer_epoch().await, first_epoch);
    assert_eq!(server.stats().connections(), first_connections);

    manager.stop().await;
    let stopped = adapter.get_status().await;
    assert!(
        !stopped.connected,
        "stop returns after the child has disconnected"
    );
    assert!(
        stopped.info.is_none(),
        "disconnected status cannot present old daemon identity without stale provenance"
    );
    assert!(aggregator.get_zone("hqplayer:rig").await.is_none());

    manager.start().await.expect("restart managed lifecycle");
    for _ in 0..100 {
        if adapter.producer_epoch().await > first_epoch
            && aggregator.get_zone("hqplayer:rig").await.is_some()
        {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(adapter.producer_epoch().await, first_epoch.wrapping_add(1));
    assert_eq!(
        server.stats().connections(),
        first_connections + 1,
        "restart creates exactly one replacement producer"
    );

    manager.stop().await;
    bus.publish(BusEvent::ShuttingDown { reason: None });
    aggregator_task.await.expect("aggregator stops");
    server.stop();
}

/// Reconfiguration is ordered after an in-flight native conversation. The adapter must not present
/// the new host identity while a response from the old host can still complete and be interpreted.
///
/// **Label: client-red.** Before the conversation lease, `configure` changed state before it waited
/// for the old connection mutex.
#[tokio::test]
async fn reconfiguration_cannot_overtake_an_old_host_response() {
    isolate_config_dir();
    let server = WireServer::start(
        Arc::new(DaemonModel::with_profile(VERIFIED_PROFILE)),
        WirePolicy::default(),
    )
    .await;
    let adapter = Arc::new(HqpAdapter::new(create_bus()));
    adapter
        .configure(
            "127.0.0.1".to_string(),
            Some(server.port()),
            None,
            None,
            None,
        )
        .await;
    adapter.set_timeouts(fast_timeouts()).await;
    adapter.connect().await.expect("coherent initial session");

    server.set_policy(WirePolicy {
        chunking: Chunking::AfterMarker("state=\"".to_string()),
        chunk_delay: Duration::from_millis(200),
        ..WirePolicy::default()
    });
    let state_requests = server.stats().element_count("State");
    let state_task = {
        let adapter = adapter.clone();
        tokio::spawn(async move { adapter.get_state().await })
    };
    for _ in 0..100 {
        if server.stats().element_count("State") > state_requests {
            break;
        }
        tokio::time::sleep(Duration::from_millis(2)).await;
    }

    let configure_task = {
        let adapter = adapter.clone();
        tokio::spawn(async move {
            adapter
                .configure("192.0.2.1".to_string(), Some(4321), None, None, None)
                .await;
        })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!(
        adapter.get_status().await.host.as_deref(),
        Some("127.0.0.1"),
        "host identity changes only after the old-host conversation completes"
    );

    state_task
        .await
        .expect("state task joins")
        .expect("old-host state response completes");
    configure_task.await.expect("configure task joins");
    assert_eq!(
        adapter.get_status().await.host.as_deref(),
        Some("192.0.2.1")
    );
    server.stop();
}
