//! Adapter-level integration tests
//!
//! Tests adapter behavior with mock/simulated external services.
//! These tests verify:
//! - Correct protocol handling
//! - Error conditions and recovery
//! - Event bus integration
//! - State consistency

mod mock_servers;

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::timeout;

use unified_hifi_control::adapters::hqplayer::HqpAdapter;
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::adapters::Startable;
use unified_hifi_control::bus::{create_bus, BusEvent, PrefixedZoneId, SharedBus};

// =============================================================================
// Test utilities
// =============================================================================

/// Create a test bus and return both the bus and a subscription
fn test_bus() -> (SharedBus, broadcast::Receiver<BusEvent>) {
    let bus = create_bus();
    let rx = bus.subscribe();
    (bus, rx)
}

/// Clear LMS config file to ensure tests start with unconfigured state.
/// Needed because `configure()` saves config to disk, which persists across test runs.
fn clear_lms_config() {
    use unified_hifi_control::config::get_config_file_path;
    let path = get_config_file_path("lms-config.json");
    let _ = std::fs::remove_file(path);
}

/// Wait for a specific event type with timeout
async fn expect_event<F>(
    rx: &mut broadcast::Receiver<BusEvent>,
    predicate: F,
    timeout_ms: u64,
) -> Option<BusEvent>
where
    F: Fn(&BusEvent) -> bool,
{
    let deadline = Duration::from_millis(timeout_ms);
    match timeout(deadline, async {
        loop {
            match rx.recv().await {
                Ok(event) if predicate(&event) => return Some(event),
                Ok(_) => continue, // Keep waiting for matching event
                Err(_) => return None,
            }
        }
    })
    .await
    {
        Ok(event) => event,
        Err(_) => None, // Timeout
    }
}

// =============================================================================
// HQPlayer adapter integration tests
// =============================================================================

mod hqplayer_integration {
    use super::*;

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn adapter_starts_disconnected() {
        let (bus, _rx) = test_bus();
        let adapter = HqpAdapter::new(bus);

        let status = adapter.get_status().await;
        // Note: host may be Some if config was previously saved - that's OK
        // The key assertion is that we're not connected
        assert!(!status.connected);
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn configure_sets_host() {
        let (bus, _rx) = test_bus();
        let adapter = HqpAdapter::new(bus);

        adapter
            .configure("192.168.1.100".to_string(), Some(4321), None, None, None)
            .await;

        let status = adapter.get_status().await;
        assert_eq!(status.host, Some("192.168.1.100".to_string()));
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn control_action_parsing() {
        let (bus, _rx) = test_bus();
        let adapter = HqpAdapter::new(bus);

        // Test that known actions don't error out with invalid action error
        // (They will fail with connection-related errors - that's expected)
        let result = adapter.control("play").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string().to_lowercase();
        assert!(
            err.contains("not connected")
                || err.contains("connection")
                || err.contains("not configured")
                || err.contains("timeout"),
            "Expected connection/config/timeout error, got: {}",
            err
        );

        // Test invalid action
        // (The current implementation doesn't validate actions until sent)
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn volume_range_is_correct() {
        // HQPlayer uses 0-100 for volume
        let (bus, _rx) = test_bus();
        let adapter = HqpAdapter::new(bus);

        // Volume should be clamped to 0-100
        // Since we can't connect, we just verify the adapter exists
        // and would clamp values (tested in volume_safety.rs more thoroughly)
        let result = adapter.set_volume(150).await;
        assert!(result.is_err()); // Can't set volume when disconnected
    }
}

// =============================================================================
// LMS adapter integration tests
// =============================================================================

mod lms_integration {
    use super::*;

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn adapter_starts_disconnected() {
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let status = adapter.get_status().await;
        // Note: host may be Some if config was previously saved - that's OK
        // The key assertion is that we're not connected
        assert!(!status.connected);
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn configure_sets_host_and_port() {
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        adapter
            .configure("192.168.1.50".to_string(), Some(9000), None, None)
            .await;

        let status = adapter.get_status().await;
        assert_eq!(status.host, Some("192.168.1.50".to_string()));
        assert_eq!(status.port, 9000);
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn cached_players_empty_when_not_connected() {
        clear_lms_config(); // Ensure no config from parallel tests
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let players = adapter.get_cached_players().await;
        assert!(players.is_empty());
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn control_fails_when_disconnected() {
        clear_lms_config(); // Ensure no config from parallel tests
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let result = adapter.control("player-1", "play", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn volume_control_fails_when_disconnected() {
        clear_lms_config(); // Ensure no config from parallel tests
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let result = adapter.change_volume("player-1", 50.0, false).await;
        assert!(result.is_err());
    }

    /// Regression test for PR #164: Polling emits NowPlayingChanged events
    /// including metadata clearing (title/artist/album become None)
    ///
    /// This test verifies the update_players() method emits proper bus events
    /// when metadata changes, including when metadata is cleared.
    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn polling_emits_now_playing_changed_including_cleared() {
        use mock_servers::lms::MockLmsServer;

        // Start mock LMS server
        let server = MockLmsServer::start().await;
        let player_id = "aa:bb:cc:dd:ee:ff";
        server.add_player(player_id, "Test Player").await;

        // Create adapter and subscribe to bus events
        let (bus, mut rx) = test_bus();
        let adapter = LmsAdapter::new(bus.clone());

        // Configure adapter to connect to mock server
        let addr = server.addr();
        adapter
            .configure(addr.ip().to_string(), Some(addr.port()), None, None)
            .await;

        // First update - establishes baseline (emits ZoneDiscovered)
        adapter.update_players().await.expect("initial update");

        // Drain any initial events (ZoneDiscovered, etc.)
        tokio::time::sleep(Duration::from_millis(100)).await;
        while rx.try_recv().is_ok() {}

        // Set now playing info on mock server
        server
            .set_now_playing(player_id, "Test Song", "Test Artist", "Test Album")
            .await;
        server.set_mode(player_id, "play").await;

        // Second update - should emit NowPlayingChanged with track info
        adapter.update_players().await.expect("update with track");

        // Check for NowPlayingChanged event with track info
        let event = expect_event(
            &mut rx,
            |e| matches!(e, BusEvent::NowPlayingChanged { title: Some(t), .. } if t == "Test Song"),
            1000,
        )
        .await;
        assert!(
            event.is_some(),
            "Expected NowPlayingChanged with track info"
        );

        // Clear metadata on mock server (simulating track stop)
        server.set_now_playing(player_id, "", "", "").await;
        server.set_mode(player_id, "stop").await;

        // Third update - should emit NowPlayingChanged with cleared metadata (None values)
        adapter.update_players().await.expect("update with cleared");

        // Check for NowPlayingChanged event with cleared metadata
        let cleared_event = expect_event(
            &mut rx,
            |e| matches!(e, BusEvent::NowPlayingChanged { title: None, .. }),
            1000,
        )
        .await;
        assert!(
            cleared_event.is_some(),
            "Expected NowPlayingChanged with cleared metadata (title: None)"
        );

        // Verify the cleared event has all None values
        if let Some(BusEvent::NowPlayingChanged {
            title,
            artist,
            album,
            ..
        }) = cleared_event
        {
            assert!(title.is_none(), "title should be None when cleared");
            assert!(artist.is_none(), "artist should be None when cleared");
            assert!(album.is_none(), "album should be None when cleared");
        }

        server.stop().await;
    }
}

// =============================================================================
// Event bus integration tests
// =============================================================================

mod bus_integration {
    use super::*;

    #[tokio::test]
    async fn bus_broadcasts_to_multiple_subscribers() {
        let bus = create_bus();
        let mut rx1 = bus.subscribe();
        let mut rx2 = bus.subscribe();

        bus.publish(BusEvent::RoonConnected {
            core_name: "Test Core".to_string(),
            version: "1.0".to_string(),
        });

        // Both subscribers should receive the event
        let event1 = rx1.recv().await.unwrap();
        let event2 = rx2.recv().await.unwrap();

        match (&event1, &event2) {
            (
                BusEvent::RoonConnected { core_name: n1, .. },
                BusEvent::RoonConnected { core_name: n2, .. },
            ) => {
                assert_eq!(n1, "Test Core");
                assert_eq!(n2, "Test Core");
            }
            _ => panic!("Expected RoonConnected events"),
        }
    }

    #[tokio::test]
    async fn bus_tracks_subscriber_count() {
        let bus = create_bus();
        assert_eq!(bus.subscriber_count(), 0);

        let _rx1 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 1);

        let _rx2 = bus.subscribe();
        assert_eq!(bus.subscriber_count(), 2);

        drop(_rx1);
        // Note: subscriber_count may not immediately decrease due to broadcast channel behavior
    }

    #[tokio::test]
    async fn bus_events_serialize_correctly() {
        // Test all event types serialize to valid JSON
        let events = vec![
            BusEvent::RoonConnected {
                core_name: "Core".to_string(),
                version: "2.0".to_string(),
            },
            BusEvent::RoonDisconnected,
            BusEvent::ZoneUpdated {
                zone_id: PrefixedZoneId::roon("zone-1"),
                display_name: "Living Room".to_string(),
                state: "playing".to_string(),
            },
            BusEvent::ZoneRemoved {
                zone_id: PrefixedZoneId::roon("zone-1"),
            },
            BusEvent::NowPlayingChanged {
                zone_id: PrefixedZoneId::roon("zone-1"),
                title: Some("Track".to_string()),
                artist: Some("Artist".to_string()),
                album: Some("Album".to_string()),
                image_key: None,
            },
            BusEvent::SeekPositionChanged {
                zone_id: PrefixedZoneId::roon("zone-1"),
                position: 120,
            },
            BusEvent::VolumeChanged {
                output_id: "output-1".to_string(),
                value: 50.0,
                is_muted: false,
            },
            BusEvent::HqpConnected {
                host: "192.168.1.100".to_string(),
            },
            BusEvent::HqpDisconnected {
                host: "192.168.1.100".to_string(),
            },
            BusEvent::HqpStateChanged {
                host: "192.168.1.100".to_string(),
                state: "playing".to_string(),
            },
            BusEvent::LmsConnected {
                host: "192.168.1.50".to_string(),
            },
            BusEvent::LmsDisconnected {
                host: "192.168.1.50".to_string(),
            },
            BusEvent::LmsPlayerStateChanged {
                player_id: "player-1".to_string(),
                state: "playing".to_string(),
            },
        ];

        for event in events {
            let json = serde_json::to_string(&event);
            assert!(json.is_ok(), "Failed to serialize {:?}", event);

            // Verify it deserializes back
            let json_str = json.unwrap();
            let parsed: Result<BusEvent, _> = serde_json::from_str(&json_str);
            assert!(parsed.is_ok(), "Failed to deserialize: {}", json_str);
        }
    }
}

// =============================================================================
// Cross-adapter integration tests
// =============================================================================

mod cross_adapter {
    use super::*;

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn multiple_adapters_share_bus() {
        let (bus, mut rx) = test_bus();

        // Create multiple adapters sharing the same bus
        let hqp = Arc::new(HqpAdapter::new(bus.clone()));
        let lms = Arc::new(LmsAdapter::new(bus.clone()));

        // The bus should have subscribers from the test
        assert!(bus.subscriber_count() >= 1);

        // Events published to bus should be received
        bus.publish(BusEvent::HqpConnected {
            host: "test".to_string(),
        });

        let event = rx.recv().await.unwrap();
        match event {
            BusEvent::HqpConnected { host } => assert_eq!(host, "test"),
            _ => panic!("Wrong event type"),
        }
    }
}

// =============================================================================
// Error handling tests
// =============================================================================

mod error_handling {
    use super::*;

    /// RAII guard for env vars - restores original value (or removes) on drop
    struct EnvGuard {
        key: &'static str,
        original: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let original = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn hqp_handles_connection_timeout() {
        let (bus, _rx) = test_bus();
        let adapter = HqpAdapter::new(bus);

        // Configure with unreachable host
        adapter
            .configure(
                "10.255.255.1".to_string(), // Likely unreachable
                Some(4321),
                None,
                None,
                None,
            )
            .await;

        // Control should fail gracefully
        let result = adapter.control("play").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_fails_gracefully_when_unconfigured() {
        // Use temp config dir to ensure no saved config interferes
        let _guard = EnvGuard::set(
            "UHC_CONFIG_DIR",
            "/tmp/uhc-test-nonexistent-lms-unconfigured",
        );

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        // Without configuration, operations should fail gracefully
        let result = adapter.get_players().await;

        assert!(
            result.is_err(),
            "get_players should fail when not configured"
        );
    }
}

// =============================================================================
// Mock server integration tests
// =============================================================================

mod mock_server_tests {
    use super::*;
    use crate::mock_servers::{MockHqpServer, MockLmsServer, MockOpenHomeDevice, MockUpnpRenderer};

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_connects_to_mock_server() {
        // Start mock LMS server
        let mock = MockLmsServer::start().await;
        mock.add_player("aa:bb:cc:dd:ee:ff", "Living Room").await;
        mock.add_player("11:22:33:44:55:66", "Kitchen").await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        // Configure adapter to connect to mock
        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;

        // Start the adapter (spawns async connection via AdapterHandle)
        let result = adapter.start().await;
        assert!(result.is_ok(), "Failed to start adapter: {:?}", result);

        // Wait for connection to establish (AdapterHandle connects asynchronously)
        let mut connected = false;
        for _ in 0..50 {
            let status = adapter.get_status().await;
            if status.connected {
                connected = true;
                break;
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        }
        assert!(connected, "Adapter failed to connect within timeout");

        // Fetch players from mock
        let players = adapter.get_players().await;
        assert!(players.is_ok());
        let players = players.unwrap();
        assert_eq!(players.len(), 2);

        adapter.stop().await;
        mock.stop().await;
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_mock_responds_to_status_query() {
        let mock = MockLmsServer::start().await;
        mock.add_player("aa:bb:cc:dd:ee:ff", "Test Player").await;
        mock.set_mode("aa:bb:cc:dd:ee:ff", "play").await;
        mock.set_volume("aa:bb:cc:dd:ee:ff", 75).await;
        mock.set_now_playing(
            "aa:bb:cc:dd:ee:ff",
            "Test Song",
            "Test Artist",
            "Test Album",
        )
        .await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;

        // Start the adapter
        adapter.start().await.unwrap();

        let player = adapter.get_player_status("aa:bb:cc:dd:ee:ff").await;
        assert!(player.is_ok());
        let player = player.unwrap();
        assert_eq!(player.mode, "play");
        assert_eq!(player.volume, 75);
        assert_eq!(player.title, "Test Song");

        adapter.stop().await;
        mock.stop().await;
    }

    #[tokio::test]
    async fn hqp_mock_responds_to_getinfo() {
        let mock = MockHqpServer::start().await;

        // Use reqwest to test the mock directly (not through adapter)
        // because HQP adapter uses a complex TCP protocol
        let mut stream = tokio::net::TcpStream::connect(mock.addr()).await.unwrap();

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        stream
            .write_all(b"<?xml version=\"1.0\"?>\n")
            .await
            .unwrap();
        stream.write_all(b"<GetInfo/>\n").await.unwrap();

        let mut response = vec![0u8; 1024];
        let n = stream.read(&mut response).await.unwrap();
        let response = String::from_utf8_lossy(&response[..n]);

        assert!(response.contains("MockHQPlayer"));
        assert!(response.contains("version=\"5.0.0\""));

        mock.stop().await;
    }

    #[tokio::test]
    async fn upnp_mock_serves_description() {
        let mock = MockUpnpRenderer::start().await;

        let client = reqwest::Client::new();
        let response = client
            .get(mock.description_url())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(response.contains("Mock UPnP Renderer"));
        assert!(response.contains("MediaRenderer"));
        assert!(response.contains("AVTransport"));

        mock.stop().await;
    }

    #[tokio::test]
    async fn openhome_mock_serves_metadata() {
        let mock = MockOpenHomeDevice::start().await;
        mock.set_state("Playing").await;
        mock.set_volume(65).await;
        mock.set_track(
            "Test Track",
            "Test Artist",
            "Test Album",
            "http://example.com/art.jpg",
        )
        .await;

        let client = reqwest::Client::new();
        let response = client
            .get(mock.description_url())
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert!(response.contains("Mock OpenHome Device"));
        assert!(response.contains("av-openhome-org"));

        mock.stop().await;
    }

    /// Tests that the LMS adapter's "play" command correctly resumes from pause.
    ///
    /// Per real-world testing (issue #68), the LMS "play" command handles both
    /// starting from stopped AND resuming from pause. The adapter can simply
    /// send "play" without checking cached state.
    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_adapter_play_resumes_from_pause() {
        // Start mock server with player in paused state
        let mock = MockLmsServer::start().await;
        let player_id = "aa:bb:cc:dd:ee:ff";
        mock.add_player(player_id, "Test Player").await;
        mock.set_mode(player_id, "pause").await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;

        adapter.start().await.unwrap();

        // Verify initial state is paused
        let player = adapter.get_player_status(player_id).await.unwrap();
        assert_eq!(player.mode, "pause", "Player should start paused");

        // Send "play" command through the adapter
        adapter.control(player_id, "play", None).await.unwrap();

        // Give the adapter a moment to update its cache
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Verify player is now playing
        let player = adapter.get_player_status(player_id).await.unwrap();
        assert_eq!(
            player.mode, "play",
            "Player should be playing after 'play' command resumes from pause"
        );

        adapter.stop().await;
        mock.stop().await;
    }

    /// Tests that the LMS adapter's "play" command correctly starts from stopped.
    ///
    /// This ensures the fix for resume-from-pause doesn't break play-from-stopped.
    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_adapter_play_starts_from_stopped() {
        // Start mock server with player in stopped state
        let mock = MockLmsServer::start().await;
        let player_id = "aa:bb:cc:dd:ee:ff";
        mock.add_player(player_id, "Test Player").await;
        mock.set_mode(player_id, "stop").await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;

        adapter.start().await.unwrap();

        // Verify initial state is stopped
        let player = adapter.get_player_status(player_id).await.unwrap();
        assert_eq!(player.mode, "stop", "Player should start stopped");

        // Send "play" command through the adapter
        adapter.control(player_id, "play", None).await.unwrap();

        // Give the adapter a moment to update its cache
        tokio::time::sleep(Duration::from_millis(150)).await;

        // Verify player is now playing
        let player = adapter.get_player_status(player_id).await.unwrap();
        assert_eq!(
            player.mode, "play",
            "Player should be playing after 'play' command from stopped"
        );

        adapter.stop().await;
        mock.stop().await;
    }

    // -------------------------------------------------------------------------
    // LMS sync groups (#510): multiroom_status / set_group_members /
    // ungroup_members over the mock server's `sync` / `syncgroups ?` support.
    // -------------------------------------------------------------------------

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_multiroom_status_reports_no_groups_before_syncing() {
        let mock = MockLmsServer::start().await;
        mock.add_player("aa:bb:cc:dd:ee:ff", "Living Room").await;
        mock.add_player("11:22:33:44:55:66", "Kitchen").await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);
        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;
        adapter.start().await.unwrap();

        let status = adapter.multiroom_status().await.unwrap();
        assert_eq!(
            status["groups"].as_array().unwrap().len(),
            0,
            "no players are synced yet"
        );

        adapter.stop().await;
        mock.stop().await;
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_multiroom_set_members_syncs_member_into_leaders_group() {
        let mock = MockLmsServer::start().await;
        let leader = "aa:bb:cc:dd:ee:ff";
        let member = "11:22:33:44:55:66";
        mock.add_player(leader, "Living Room").await;
        mock.add_player(member, "Kitchen").await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);
        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;
        adapter.start().await.unwrap();
        mock.clear_commands().await;

        let leader_zone = PrefixedZoneId::lms(leader).to_string();
        let member_zone = PrefixedZoneId::lms(member).to_string();
        let result = adapter
            .set_group_members(&leader_zone, &[member_zone.clone()], &[])
            .await
            .expect("set_group_members should succeed");

        let groups = result["groups"].as_array().expect("groups array");
        assert_eq!(groups.len(), 1, "one group after syncing one member");
        assert_eq!(groups[0]["leader_zone_id"], serde_json::json!(leader_zone));
        assert_eq!(groups[0]["member_zone_ids"], serde_json::json!([member_zone]));
        assert_eq!(groups[0]["can_set_members"], serde_json::json!(true));

        // The leader is addressed, the member is the argument -- verified live
        // (#403) to make the *addressed* player the sync master and keep its
        // queue, which is why the leader (not the member) issues the command.
        let leader_commands = mock.write_commands(leader).await;
        assert!(
            leader_commands.contains(&vec!["sync".to_string(), member.to_string()]),
            "expected `sync {member}` addressed to the leader, got {leader_commands:?}"
        );

        assert_eq!(
            mock.sync_groups().await,
            vec![vec![leader.to_string(), member.to_string()]]
        );

        adapter.stop().await;
        mock.stop().await;
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_multiroom_ungroup_unsyncs_member() {
        let mock = MockLmsServer::start().await;
        let leader = "aa:bb:cc:dd:ee:ff";
        let member = "11:22:33:44:55:66";
        mock.add_player(leader, "Living Room").await;
        mock.add_player(member, "Kitchen").await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);
        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;
        adapter.start().await.unwrap();

        let leader_zone = PrefixedZoneId::lms(leader).to_string();
        let member_zone = PrefixedZoneId::lms(member).to_string();
        adapter
            .set_group_members(&leader_zone, &[member_zone.clone()], &[])
            .await
            .expect("initial sync should succeed");
        mock.clear_commands().await;

        let result = adapter
            .ungroup_members(&[member_zone])
            .await
            .expect("ungroup_members should succeed");
        assert_eq!(
            result["groups"].as_array().unwrap().len(),
            0,
            "the group dissolves once its only member leaves"
        );

        let member_commands = mock.write_commands(member).await;
        assert_eq!(
            member_commands,
            vec![vec!["sync".to_string(), "-".to_string()]],
            "the member is addressed directly to unsync itself"
        );

        assert!(
            mock.sync_groups().await.is_empty(),
            "server-side group state must also be gone"
        );

        adapter.stop().await;
        mock.stop().await;
    }

    #[tokio::test]
    #[serial_test::serial(lms_config)]
    async fn lms_multiroom_set_members_rejects_unknown_player_before_any_sync_call() {
        let mock = MockLmsServer::start().await;
        let leader = "aa:bb:cc:dd:ee:ff";
        mock.add_player(leader, "Living Room").await;

        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);
        adapter
            .configure(
                mock.addr().ip().to_string(),
                Some(mock.addr().port()),
                None,
                None,
            )
            .await;
        adapter.start().await.unwrap();
        mock.clear_commands().await;

        let leader_zone = PrefixedZoneId::lms(leader).to_string();
        let unknown_zone = PrefixedZoneId::lms("00:00:00:00:00:00").to_string();
        let result = adapter
            .set_group_members(&leader_zone, &[unknown_zone], &[])
            .await;
        assert!(
            result.is_err(),
            "an unknown member must be refused, not synced"
        );
        assert!(
            mock.write_commands(leader).await.is_empty(),
            "no sync command should reach LMS once validation fails"
        );
        assert!(mock.sync_groups().await.is_empty());

        adapter.stop().await;
        mock.stop().await;
    }
}
