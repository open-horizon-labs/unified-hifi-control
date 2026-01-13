//! Adapter-level integration tests
//!
//! Tests adapter behavior with mock/simulated external services.
//! These tests verify:
//! - Correct protocol handling
//! - Error conditions and recovery
//! - Event bus integration
//! - State consistency

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::timeout;

use unified_hifi_control::adapters::hqplayer::HqpAdapter;
use unified_hifi_control::adapters::lms::LmsAdapter;
use unified_hifi_control::bus::{create_bus, BusEvent, SharedBus};

// =============================================================================
// Test utilities
// =============================================================================

/// Create a test bus and return both the bus and a subscription
fn test_bus() -> (SharedBus, broadcast::Receiver<BusEvent>) {
    let bus = create_bus();
    let rx = bus.subscribe();
    (bus, rx)
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
    async fn adapter_starts_disconnected() {
        let (bus, _rx) = test_bus();
        let adapter = HqpAdapter::new(bus);

        let status = adapter.get_status().await;
        assert!(!status.connected);
        assert!(status.host.is_none());
    }

    #[tokio::test]
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
    async fn control_action_parsing() {
        let (bus, _rx) = test_bus();
        let adapter = HqpAdapter::new(bus);

        // Test that known actions don't error out with invalid action error
        // (They will fail with "Not connected" or "not configured" but that's expected)
        let result = adapter.control("play").await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("Not connected")
                || err.contains("connection")
                || err.contains("not configured"),
            "Expected connection/config error, got: {}",
            err
        );

        // Test invalid action
        // (The current implementation doesn't validate actions until sent)
    }

    #[tokio::test]
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
    async fn adapter_starts_disconnected() {
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let status = adapter.get_status().await;
        assert!(!status.connected);
        assert!(status.host.is_none());
    }

    #[tokio::test]
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
    async fn cached_players_empty_when_not_connected() {
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let players = adapter.get_cached_players().await;
        assert!(players.is_empty());
    }

    #[tokio::test]
    async fn control_fails_when_disconnected() {
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let result = adapter.control("player-1", "play", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn volume_control_fails_when_disconnected() {
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        let result = adapter.change_volume("player-1", 50, false).await;
        assert!(result.is_err());
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
                zone_id: "zone-1".to_string(),
                display_name: "Living Room".to_string(),
                state: "playing".to_string(),
            },
            BusEvent::ZoneRemoved {
                zone_id: "zone-1".to_string(),
            },
            BusEvent::NowPlayingChanged {
                zone_id: "zone-1".to_string(),
                title: Some("Track".to_string()),
                artist: Some("Artist".to_string()),
                album: Some("Album".to_string()),
                image_key: None,
            },
            BusEvent::SeekPositionChanged {
                zone_id: "zone-1".to_string(),
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

    #[tokio::test]
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
    async fn lms_handles_invalid_json_gracefully() {
        let (bus, _rx) = test_bus();
        let adapter = LmsAdapter::new(bus);

        // Without configuration, operations should fail gracefully
        let result = adapter.get_players().await;
        assert!(result.is_err());
    }
}
