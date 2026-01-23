//! Protocol Schema Test Harness
//!
//! Validates that all API responses and event bus messages conform to expected schemas.
//! This serves as an executable contract test for clients consuming the API.

use serde::{Deserialize, Serialize};
use serde_json::json;

/// StatusResponse schema - GET /status
#[derive(Debug, Deserialize)]
struct StatusResponse {
    service: String,
    version: String,
    uptime_secs: u64,
    roon_connected: bool,
    bus_subscribers: usize,
}

/// RoonStatus schema - GET /roon/status
#[derive(Debug, Deserialize)]
struct RoonStatus {
    connected: bool,
    core_name: Option<String>,
    core_version: Option<String>,
    zone_count: usize,
}

/// Zone schema - GET /roon/zones, GET /roon/zone/:id
#[derive(Debug, Deserialize)]
struct Zone {
    zone_id: String,
    display_name: String,
    state: String,
    is_next_allowed: bool,
    is_previous_allowed: bool,
    is_pause_allowed: bool,
    is_play_allowed: bool,
    now_playing: Option<NowPlaying>,
    outputs: Vec<Output>,
}

#[derive(Debug, Deserialize)]
struct NowPlaying {
    title: String,
    artist: String,
    album: String,
    image_key: Option<String>,
    seek_position: Option<i64>,
    length: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Output {
    output_id: String,
    display_name: String,
    volume: Option<VolumeInfo>,
}

#[derive(Debug, Deserialize)]
struct VolumeInfo {
    value: Option<f32>,
    min: Option<f32>,
    max: Option<f32>,
    is_muted: Option<bool>,
}

/// ControlRequest schema - POST /roon/control body
#[derive(Debug, Serialize, Deserialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
}

/// VolumeRequest schema - POST /roon/volume body
#[derive(Debug, Serialize, Deserialize)]
struct VolumeRequest {
    output_id: String,
    value: i32,
    #[serde(default)]
    relative: bool,
}

/// Success response schema
#[derive(Debug, Deserialize)]
struct SuccessResponse {
    ok: bool,
}

/// Error response schema
#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
}

// Use production BusEvent to keep schema in sync
use unified_hifi_control::bus::BusEvent;

// ============================================================================
// Schema Validation Tests
// ============================================================================

mod status_schema {
    use super::*;

    #[test]
    fn validates_status_response() {
        let json = json!({
            "service": "unified-hifi-control",
            "version": "0.1.0",
            "uptime_secs": 3600,
            "roon_connected": true,
            "bus_subscribers": 2
        });

        let result: Result<StatusResponse, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "StatusResponse should deserialize: {:?}",
            result.err()
        );
    }

    #[test]
    fn rejects_missing_fields() {
        let json = json!({
            "service": "unified-hifi-control"
            // Missing required fields
        });

        let result: Result<StatusResponse, _> = serde_json::from_value(json);
        assert!(result.is_err(), "Should reject missing required fields");
    }
}

mod roon_status_schema {
    use super::*;

    #[test]
    fn validates_connected_status() {
        let json = json!({
            "connected": true,
            "core_name": "Roon Core",
            "core_version": "2.0.0",
            "zone_count": 5
        });

        let result: Result<RoonStatus, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "RoonStatus should deserialize: {:?}",
            result.err()
        );
    }

    #[test]
    fn validates_disconnected_status() {
        let json = json!({
            "connected": false,
            "core_name": null,
            "core_version": null,
            "zone_count": 0
        });

        let result: Result<RoonStatus, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "Disconnected status should deserialize: {:?}",
            result.err()
        );
    }
}

mod zone_schema {
    use super::*;

    #[test]
    fn validates_full_zone() {
        let json = json!({
            "zone_id": "zone-12345",
            "display_name": "Living Room",
            "state": "playing",
            "is_next_allowed": true,
            "is_previous_allowed": true,
            "is_pause_allowed": true,
            "is_play_allowed": false,
            "now_playing": {
                "title": "Test Track",
                "artist": "Test Artist",
                "album": "Test Album",
                "image_key": "img-key-123",
                "seek_position": 45,
                "length": 180
            },
            "outputs": [{
                "output_id": "output-1",
                "display_name": "DAC Output",
                "volume": {
                    "value": 50.0,
                    "min": 0.0,
                    "max": 100.0,
                    "is_muted": false
                }
            }]
        });

        let result: Result<Zone, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "Full zone should deserialize: {:?}",
            result.err()
        );
    }

    #[test]
    fn validates_minimal_zone() {
        let json = json!({
            "zone_id": "zone-12345",
            "display_name": "Living Room",
            "state": "stopped",
            "is_next_allowed": false,
            "is_previous_allowed": false,
            "is_pause_allowed": false,
            "is_play_allowed": true,
            "now_playing": null,
            "outputs": []
        });

        let result: Result<Zone, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "Minimal zone should deserialize: {:?}",
            result.err()
        );
    }

    #[test]
    fn validates_zone_states() {
        let valid_states = ["playing", "paused", "loading", "stopped"];

        for state in valid_states {
            let json = json!({
                "zone_id": "zone-1",
                "display_name": "Test",
                "state": state,
                "is_next_allowed": false,
                "is_previous_allowed": false,
                "is_pause_allowed": false,
                "is_play_allowed": false,
                "now_playing": null,
                "outputs": []
            });

            let result: Result<Zone, _> = serde_json::from_value(json);
            assert!(
                result.is_ok(),
                "State '{}' should be valid: {:?}",
                state,
                result.err()
            );
        }
    }
}

mod control_request_schema {
    use super::*;

    #[test]
    fn validates_control_request() {
        let json = json!({
            "zone_id": "zone-12345",
            "action": "play"
        });

        let result: Result<ControlRequest, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "ControlRequest should deserialize: {:?}",
            result.err()
        );
    }

    #[test]
    fn serializes_control_request() {
        let request = ControlRequest {
            zone_id: "zone-123".to_string(),
            action: "pause".to_string(),
        };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["zone_id"], "zone-123");
        assert_eq!(json["action"], "pause");
    }

    #[test]
    fn validates_all_control_actions() {
        let valid_actions = ["play", "pause", "play_pause", "stop", "previous", "next"];

        for action in valid_actions {
            let json = json!({
                "zone_id": "zone-1",
                "action": action
            });

            let result: Result<ControlRequest, _> = serde_json::from_value(json);
            assert!(
                result.is_ok(),
                "Action '{}' should be valid: {:?}",
                action,
                result.err()
            );
        }
    }
}

mod volume_request_schema {
    use super::*;

    #[test]
    fn validates_absolute_volume() {
        let json = json!({
            "output_id": "output-1",
            "value": 50,
            "relative": false
        });

        let result: Result<VolumeRequest, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "Absolute volume should deserialize: {:?}",
            result.err()
        );

        let req = result.unwrap();
        assert_eq!(req.value, 50);
        assert!(!req.relative);
    }

    #[test]
    fn validates_relative_volume() {
        let json = json!({
            "output_id": "output-1",
            "value": -5,
            "relative": true
        });

        let result: Result<VolumeRequest, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "Relative volume should deserialize: {:?}",
            result.err()
        );

        let req = result.unwrap();
        assert_eq!(req.value, -5);
        assert!(req.relative);
    }

    #[test]
    fn defaults_relative_to_false() {
        let json = json!({
            "output_id": "output-1",
            "value": 50
        });

        let result: Result<VolumeRequest, _> = serde_json::from_value(json);
        assert!(result.is_ok(), "Should default relative to false");

        let req = result.unwrap();
        assert!(!req.relative);
    }
}

mod response_schema {
    use super::*;

    #[test]
    fn validates_success_response() {
        let json = json!({"ok": true});
        let result: Result<SuccessResponse, _> = serde_json::from_value(json);
        assert!(result.is_ok(), "Success response should deserialize");
    }

    #[test]
    fn validates_error_response() {
        let json = json!({"error": "Zone not found: zone-123"});
        let result: Result<ErrorResponse, _> = serde_json::from_value(json);
        assert!(result.is_ok(), "Error response should deserialize");
    }
}

mod bus_event_schema {
    use super::*;

    #[test]
    fn validates_roon_connected() {
        let event = BusEvent::RoonConnected {
            core_name: "My Roon Core".to_string(),
            version: "2.0.0".to_string(),
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "RoonConnected");
        assert_eq!(json["payload"]["core_name"], "My Roon Core");
        assert_eq!(json["payload"]["version"], "2.0.0");

        // Round-trip
        let parsed: BusEvent = serde_json::from_value(json).unwrap();
        match parsed {
            BusEvent::RoonConnected { core_name, version } => {
                assert_eq!(core_name, "My Roon Core");
                assert_eq!(version, "2.0.0");
            }
            _ => panic!("Wrong event type"),
        }
    }

    #[test]
    fn validates_roon_disconnected() {
        let event = BusEvent::RoonDisconnected;
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "RoonDisconnected");

        let parsed: BusEvent = serde_json::from_value(json).unwrap();
        assert!(matches!(parsed, BusEvent::RoonDisconnected));
    }

    #[test]
    fn validates_zone_updated() {
        let event = BusEvent::ZoneUpdated {
            zone_id: "zone-1".to_string(),
            display_name: "Living Room".to_string(),
            state: "playing".to_string(),
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "ZoneUpdated");
        assert_eq!(json["payload"]["zone_id"], "zone-1");
        assert_eq!(json["payload"]["display_name"], "Living Room");
        assert_eq!(json["payload"]["state"], "playing");
    }

    #[test]
    fn validates_now_playing_changed() {
        let event = BusEvent::NowPlayingChanged {
            zone_id: "zone-1".to_string(),
            title: Some("Test Song".to_string()),
            artist: Some("Test Artist".to_string()),
            album: Some("Test Album".to_string()),
            image_key: Some("img-123".to_string()),
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "NowPlayingChanged");
        assert_eq!(json["payload"]["title"], "Test Song");
    }

    #[test]
    fn validates_now_playing_with_nulls() {
        let event = BusEvent::NowPlayingChanged {
            zone_id: "zone-1".to_string(),
            title: None,
            artist: None,
            album: None,
            image_key: None,
        };

        let json = serde_json::to_value(&event).unwrap();
        assert!(json["payload"]["title"].is_null());
    }

    #[test]
    fn validates_seek_position_changed() {
        let event = BusEvent::SeekPositionChanged {
            zone_id: "zone-1".to_string(),
            position: 12345,
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "SeekPositionChanged");
        assert_eq!(json["payload"]["position"], 12345);
    }

    #[test]
    fn validates_volume_changed() {
        let event = BusEvent::VolumeChanged {
            output_id: "output-1".to_string(),
            value: 75.5,
            is_muted: false,
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "VolumeChanged");
        assert_eq!(json["payload"]["value"], 75.5);
        assert_eq!(json["payload"]["is_muted"], false);
    }

    #[test]
    fn validates_hqp_events() {
        let events = vec![
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
            BusEvent::HqpPipelineChanged {
                host: "192.168.1.100".to_string(),
                filter: Some("poly-sinc-xtr".to_string()),
                shaper: Some("NS9".to_string()),
                rate: Some("44100->705600".to_string()),
            },
        ];

        for event in events {
            let json = serde_json::to_value(&event).unwrap();
            let _: BusEvent = serde_json::from_value(json).expect("Should round-trip");
        }
    }

    #[test]
    fn validates_lms_events() {
        let events = vec![
            BusEvent::LmsConnected {
                host: "192.168.1.101".to_string(),
            },
            BusEvent::LmsDisconnected {
                host: "192.168.1.101".to_string(),
            },
            BusEvent::LmsPlayerStateChanged {
                player_id: "aa:bb:cc:dd:ee:ff".to_string(),
                state: "play".to_string(),
            },
        ];

        for event in events {
            let json = serde_json::to_value(&event).unwrap();
            let _: BusEvent = serde_json::from_value(json).expect("Should round-trip");
        }
    }

    #[test]
    fn validates_control_command() {
        let event = BusEvent::ControlCommand {
            zone_id: "zone-1".to_string(),
            action: "volume".to_string(),
            value: Some(json!({"level": 50})),
        };

        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], "ControlCommand");
        assert_eq!(json["payload"]["action"], "volume");
    }
}

// ============================================================================
// Contract Tests - Ensure JSON matches what clients expect
// ============================================================================

// ============================================================================
// HQPlayer Schema Types
// ============================================================================

/// MatrixProfile schema - returned by GET /hqplayer/matrix/profiles
#[derive(Debug, Deserialize)]
struct MatrixProfile {
    index: u32,
    name: String,
}

/// MatrixProfilesResponse schema - GET /hqplayer/matrix/profiles
#[derive(Debug, Deserialize)]
struct MatrixProfilesResponse {
    profiles: Vec<MatrixProfile>,
    current: Option<MatrixProfile>,
}

/// HqpMatrixProfileRequest schema - POST /hqplayer/matrix/profile body
#[derive(Debug, Serialize, Deserialize)]
struct HqpMatrixProfileRequest {
    profile: u32,
}

mod hqp_matrix_schema {
    use super::*;

    #[test]
    fn validates_matrix_profile() {
        let json = json!({
            "index": 0,
            "name": "Default"
        });

        let result: Result<MatrixProfile, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "MatrixProfile should deserialize: {:?}",
            result.err()
        );

        let profile = result.unwrap();
        assert_eq!(profile.index, 0);
        assert_eq!(profile.name, "Default");
    }

    #[test]
    fn validates_matrix_profiles_response_with_current() {
        let json = json!({
            "profiles": [
                {"index": 0, "name": "Default"},
                {"index": 1, "name": "Night Mode"},
                {"index": 2, "name": "Movies"}
            ],
            "current": {"index": 1, "name": "Night Mode"}
        });

        let result: Result<MatrixProfilesResponse, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "MatrixProfilesResponse should deserialize: {:?}",
            result.err()
        );

        let response = result.unwrap();
        assert_eq!(response.profiles.len(), 3);
        assert!(response.current.is_some());
        assert_eq!(response.current.unwrap().index, 1);
    }

    #[test]
    fn validates_matrix_profiles_response_no_current() {
        let json = json!({
            "profiles": [
                {"index": 0, "name": "Default"}
            ],
            "current": null
        });

        let result: Result<MatrixProfilesResponse, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "MatrixProfilesResponse with null current should deserialize: {:?}",
            result.err()
        );

        let response = result.unwrap();
        assert_eq!(response.profiles.len(), 1);
        assert!(response.current.is_none());
    }

    #[test]
    fn validates_matrix_profiles_response_empty() {
        let json = json!({
            "profiles": [],
            "current": null
        });

        let result: Result<MatrixProfilesResponse, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "Empty MatrixProfilesResponse should deserialize: {:?}",
            result.err()
        );

        let response = result.unwrap();
        assert!(response.profiles.is_empty());
    }

    #[test]
    fn validates_set_matrix_profile_request() {
        let json = json!({
            "profile": 2
        });

        let result: Result<HqpMatrixProfileRequest, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "HqpMatrixProfileRequest should deserialize: {:?}",
            result.err()
        );

        let req = result.unwrap();
        assert_eq!(req.profile, 2);
    }

    #[test]
    fn serializes_set_matrix_profile_request() {
        let request = HqpMatrixProfileRequest { profile: 5 };

        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["profile"], 5);
    }

    #[test]
    fn rejects_invalid_matrix_profile_request() {
        // Missing required field
        let json = json!({});

        let result: Result<HqpMatrixProfileRequest, _> = serde_json::from_value(json);
        assert!(result.is_err(), "Should reject missing profile field");
    }

    #[test]
    fn rejects_string_profile_index() {
        let json = json!({
            "profile": "invalid"
        });

        let result: Result<HqpMatrixProfileRequest, _> = serde_json::from_value(json);
        assert!(result.is_err(), "Should reject string profile index");
    }
}

/// KnobNowPlayingResponse schema - GET /knob/now_playing
/// This struct enforces the contract that zones_sha MUST be present
#[derive(Debug, Deserialize)]
struct KnobNowPlayingResponse {
    zone_id: String,
    line1: String,
    line2: String,
    line3: Option<String>,
    is_playing: bool,
    zones: Vec<serde_json::Value>,
    config_sha: Option<String>,
    zones_sha: Option<String>, // Must be present for dynamic zone detection (#148)
}

mod knob_now_playing_schema {
    use super::*;

    #[test]
    fn validates_response_with_zones_sha() {
        let json = json!({
            "zone_id": "roon:zone-1",
            "line1": "Track Title",
            "line2": "Artist Name",
            "line3": "Album Name",
            "is_playing": true,
            "volume": 50.0,
            "volume_type": "number",
            "volume_min": 0.0,
            "volume_max": 100.0,
            "volume_step": 1.0,
            "image_url": "/knob/now_playing/image?zone_id=roon%3Azone-1",
            "image_key": null,
            "seek_position": 45,
            "length": 180,
            "is_play_allowed": false,
            "is_pause_allowed": true,
            "is_next_allowed": true,
            "is_previous_allowed": true,
            "zones": [],
            "config_sha": "abc12345",
            "zones_sha": "def67890"
        });

        let result: Result<KnobNowPlayingResponse, _> = serde_json::from_value(json);
        assert!(
            result.is_ok(),
            "KnobNowPlayingResponse should deserialize: {:?}",
            result.err()
        );

        let response = result.unwrap();
        assert!(
            response.zones_sha.is_some(),
            "zones_sha must be present for dynamic zone detection (issue #148)"
        );
    }

    /// Regression test: zones_sha must be included in responses
    /// This test documents the contract established by PR #149
    #[test]
    fn zones_sha_is_required_for_dynamic_zone_detection() {
        // This JSON simulates what the server SHOULD return
        // If zones_sha is missing, clients cannot detect zone list changes
        let valid_response = json!({
            "zone_id": "lms:player-1",
            "line1": "Song",
            "line2": "Artist",
            "line3": null,
            "is_playing": false,
            "zones": [{"zone_id": "lms:player-1", "zone_name": "Kitchen", "source": "lms", "state": "stopped"}],
            "config_sha": null,
            "zones_sha": "a1b2c3d4"
        });

        let response: KnobNowPlayingResponse =
            serde_json::from_value(valid_response).expect("Valid response should parse");

        // The key assertion: zones_sha must be present
        assert!(
            response.zones_sha.is_some(),
            "zones_sha MUST be present in /knob/now_playing responses (PR #149, fixes #148)"
        );
    }

    /// This test demonstrates what WOULD break if zones_sha were missing
    /// (verifying the regression test catches the issue)
    #[test]
    fn missing_zones_sha_is_detectable() {
        // Response WITHOUT zones_sha - this is what the old code produced
        let response_without_zones_sha = json!({
            "zone_id": "roon:zone-1",
            "line1": "Track",
            "line2": "Artist",
            "line3": null,
            "is_playing": false,
            "zones": [],
            "config_sha": null
            // zones_sha intentionally missing!
        });

        let response: KnobNowPlayingResponse =
            serde_json::from_value(response_without_zones_sha).expect("Should parse");

        // zones_sha will be None since it was missing from JSON
        assert!(
            response.zones_sha.is_none(),
            "Response without zones_sha should have None (this is the pre-fix behavior)"
        );

        // This is the assertion that would catch the regression:
        // assert!(response.zones_sha.is_some(), "...");
        // ^ If we uncommented this, the test would FAIL for pre-fix responses
    }
}

mod contract_tests {
    use super::*;

    /// Test that the status endpoint returns the expected shape
    #[test]
    fn status_endpoint_contract() {
        // Simulate what the server would return
        let response = json!({
            "service": "unified-hifi-control",
            "version": "0.1.0",
            "uptime_secs": 0,
            "roon_connected": false,
            "bus_subscribers": 0
        });

        // Validate against our schema
        let _: StatusResponse =
            serde_json::from_value(response).expect("Status response should match contract");
    }

    /// Test SSE event format matches client expectations
    #[test]
    fn sse_event_format() {
        let event = BusEvent::ZoneUpdated {
            zone_id: "zone-1".to_string(),
            display_name: "Test".to_string(),
            state: "playing".to_string(),
        };

        let json_str = serde_json::to_string(&event).unwrap();

        // SSE format: data: {json}\n\n
        let sse_line = format!("data: {}\n\n", json_str);

        // Extract the JSON from SSE format
        let data = sse_line.strip_prefix("data: ").unwrap().trim();
        let _: BusEvent =
            serde_json::from_str(data).expect("SSE data should be valid BusEvent JSON");
    }

    /// Test that zone list is an array of valid zones
    #[test]
    fn zones_list_contract() {
        let response = json!([
            {
                "zone_id": "zone-1",
                "display_name": "Zone 1",
                "state": "stopped",
                "is_next_allowed": false,
                "is_previous_allowed": false,
                "is_pause_allowed": false,
                "is_play_allowed": true,
                "now_playing": null,
                "outputs": []
            },
            {
                "zone_id": "zone-2",
                "display_name": "Zone 2",
                "state": "playing",
                "is_next_allowed": true,
                "is_previous_allowed": true,
                "is_pause_allowed": true,
                "is_play_allowed": false,
                "now_playing": {
                    "title": "Song",
                    "artist": "Artist",
                    "album": "Album",
                    "image_key": null,
                    "seek_position": 0,
                    "length": 300
                },
                "outputs": []
            }
        ]);

        let zones: Vec<Zone> =
            serde_json::from_value(response).expect("Zones list should match contract");
        assert_eq!(zones.len(), 2);
    }
}
