//! Protocol Checker CLI
//!
//! Validates JSON payloads against the API protocol schema.
//! Can be used to validate recorded responses or test fixtures.
//!
//! Usage:
//!   protocol_checker validate <type> <json-file>
//!   protocol_checker validate <type> --stdin
//!   protocol_checker list-types
//!   protocol_checker generate-example <type>
//!
//! Types: status, roon-status, zone, zones, control-request, volume-request,
//!        success-response, error-response, bus-event

// Dev tool - allow unwrap for CLI simplicity
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
// Schema structs are used for JSON validation via Deserialize, fields read by serde
#![allow(dead_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::env;
use std::fs;
use std::io::{self, Read};
use std::process;

// Schema types (mirrored from the main codebase for standalone validation)

#[derive(Debug, Deserialize)]
struct StatusResponse {
    service: String,
    version: String,
    uptime_secs: u64,
    roon_connected: bool,
    bus_subscribers: usize,
}

#[derive(Debug, Deserialize)]
struct RoonStatus {
    connected: bool,
    core_name: Option<String>,
    core_version: Option<String>,
    zone_count: usize,
}

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

#[derive(Debug, Serialize, Deserialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct VolumeRequest {
    output_id: String,
    value: i32,
    #[serde(default)]
    relative: bool,
}

#[derive(Debug, Deserialize)]
struct SuccessResponse {
    ok: bool,
}

#[derive(Debug, Deserialize)]
struct ErrorResponse {
    error: String,
}

// HQPlayer types
#[derive(Debug, Deserialize)]
struct HqpConnectionStatus {
    connected: bool,
    host: Option<String>,
    state: Option<String>,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HqpPipelineStatus {
    mode: Option<u32>,
    mode_str: Option<String>,
    filter: Option<u32>,
    filter_str: Option<String>,
    shaper: Option<u32>,
    shaper_str: Option<String>,
    rate: Option<u32>,
    rate_str: Option<String>,
    volume: Option<i32>,
    #[serde(default)]
    playing: bool,
    track_title: Option<String>,
    track_artist: Option<String>,
    track_album: Option<String>,
    track_duration: Option<u64>,
    track_position: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct HqpProfile {
    name: String,
    active: bool,
}

// LMS types
#[derive(Debug, Deserialize)]
struct LmsStatus {
    connected: bool,
    host: Option<String>,
    port: u16,
    player_count: usize,
    #[serde(default)]
    players: Vec<LmsPlayerInfo>,
}

#[derive(Debug, Deserialize)]
struct LmsPlayerInfo {
    playerid: String,
    name: String,
}

#[derive(Debug, Deserialize)]
struct LmsPlayer {
    player_id: String,
    name: String,
    connected: bool,
    power: bool,
    mode: String,
    volume: i32,
    muted: bool,
    current_title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    duration: Option<f64>,
    time: Option<f64>,
    artwork_url: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload")]
enum BusEvent {
    RoonConnected {
        core_name: String,
        version: String,
    },
    RoonDisconnected,
    ZoneUpdated {
        zone_id: String,
        display_name: String,
        state: String,
    },
    ZoneRemoved {
        zone_id: String,
    },
    NowPlayingChanged {
        zone_id: String,
        title: Option<String>,
        artist: Option<String>,
        album: Option<String>,
        image_key: Option<String>,
    },
    SeekPositionChanged {
        zone_id: String,
        position: i64,
    },
    VolumeChanged {
        output_id: String,
        value: f32,
        is_muted: bool,
    },
    HqpConnected {
        host: String,
    },
    HqpDisconnected {
        host: String,
    },
    HqpStateChanged {
        host: String,
        state: String,
    },
    HqpPipelineChanged {
        host: String,
        filter: Option<String>,
        shaper: Option<String>,
        rate: Option<String>,
    },
    LmsConnected {
        host: String,
    },
    LmsDisconnected {
        host: String,
    },
    LmsPlayerStateChanged {
        player_id: String,
        state: String,
    },
    ControlCommand {
        zone_id: String,
        action: String,
        value: Option<Value>,
    },
}

const SUPPORTED_TYPES: &[&str] = &[
    "status",
    "roon-status",
    "zone",
    "zones",
    "control-request",
    "volume-request",
    "success-response",
    "error-response",
    "bus-event",
    // HQPlayer types
    "hqp-status",
    "hqp-pipeline",
    "hqp-profiles",
    // LMS types
    "lms-status",
    "lms-player",
    "lms-players",
];

fn main() {
    let args: Vec<String> = env::args().collect();

    if args.len() < 2 {
        print_usage();
        process::exit(1);
    }

    match args[1].as_str() {
        "validate" => {
            if args.len() < 3 {
                eprintln!("Error: Missing type argument");
                print_usage();
                process::exit(1);
            }
            let schema_type = &args[2];
            let json = if args.len() >= 4 {
                if args[3] == "--stdin" {
                    read_stdin()
                } else {
                    read_file(&args[3])
                }
            } else {
                read_stdin()
            };
            validate(schema_type, &json);
        }
        "list-types" => {
            println!("Supported schema types:");
            for t in SUPPORTED_TYPES {
                println!("  {}", t);
            }
        }
        "generate-example" => {
            if args.len() < 3 {
                eprintln!("Error: Missing type argument");
                print_usage();
                process::exit(1);
            }
            generate_example(&args[2]);
        }
        "help" | "--help" | "-h" => {
            print_usage();
        }
        _ => {
            eprintln!("Unknown command: {}", args[1]);
            print_usage();
            process::exit(1);
        }
    }
}

fn print_usage() {
    eprintln!("Protocol Checker - Validate API payloads against schema");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  protocol_checker validate <type> <json-file>");
    eprintln!("  protocol_checker validate <type> --stdin");
    eprintln!("  protocol_checker list-types");
    eprintln!("  protocol_checker generate-example <type>");
    eprintln!();
    eprintln!("Examples:");
    eprintln!("  protocol_checker validate zone response.json");
    eprintln!("  echo '{{\"ok\":true}}' | protocol_checker validate success-response --stdin");
    eprintln!("  protocol_checker generate-example bus-event");
}

fn read_stdin() -> String {
    let mut input = String::new();
    io::stdin()
        .read_to_string(&mut input)
        .expect("Failed to read stdin");
    input
}

fn read_file(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| {
        eprintln!("Error reading file '{}': {}", path, e);
        process::exit(1);
    })
}

fn validate(schema_type: &str, json: &str) {
    // First, parse as generic JSON to catch syntax errors
    let value: Value = match serde_json::from_str(json) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("INVALID: JSON parse error: {}", e);
            process::exit(1);
        }
    };

    let result = match schema_type {
        "status" => serde_json::from_value::<StatusResponse>(value).map(|_| ()),
        "roon-status" => serde_json::from_value::<RoonStatus>(value).map(|_| ()),
        "zone" => serde_json::from_value::<Zone>(value).map(|_| ()),
        "zones" => serde_json::from_value::<Vec<Zone>>(value).map(|_| ()),
        "control-request" => serde_json::from_value::<ControlRequest>(value).map(|_| ()),
        "volume-request" => serde_json::from_value::<VolumeRequest>(value).map(|_| ()),
        "success-response" => serde_json::from_value::<SuccessResponse>(value).map(|_| ()),
        "error-response" => serde_json::from_value::<ErrorResponse>(value).map(|_| ()),
        "bus-event" => serde_json::from_value::<BusEvent>(value).map(|_| ()),
        // HQPlayer types
        "hqp-status" => serde_json::from_value::<HqpConnectionStatus>(value).map(|_| ()),
        "hqp-pipeline" => serde_json::from_value::<HqpPipelineStatus>(value).map(|_| ()),
        "hqp-profiles" => serde_json::from_value::<Vec<HqpProfile>>(value).map(|_| ()),
        // LMS types
        "lms-status" => serde_json::from_value::<LmsStatus>(value).map(|_| ()),
        "lms-player" => serde_json::from_value::<LmsPlayer>(value).map(|_| ()),
        "lms-players" => serde_json::from_value::<Vec<LmsPlayer>>(value).map(|_| ()),
        _ => {
            eprintln!("Unknown schema type: {}", schema_type);
            eprintln!("Run 'protocol_checker list-types' to see supported types");
            process::exit(1);
        }
    };

    match result {
        Ok(()) => {
            println!("VALID: JSON conforms to '{}' schema", schema_type);
        }
        Err(e) => {
            eprintln!(
                "INVALID: Schema validation failed for '{}': {}",
                schema_type, e
            );
            process::exit(1);
        }
    }
}

fn generate_example(schema_type: &str) {
    let example: Value = match schema_type {
        "status" => serde_json::json!({
            "service": "unified-hifi-control",
            "version": "0.1.0",
            "uptime_secs": 3600,
            "roon_connected": true,
            "bus_subscribers": 2
        }),
        "roon-status" => serde_json::json!({
            "connected": true,
            "core_name": "Roon Core",
            "core_version": "2.0.0",
            "zone_count": 3
        }),
        "zone" => serde_json::json!({
            "zone_id": "zone-12345",
            "display_name": "Living Room",
            "state": "playing",
            "is_next_allowed": true,
            "is_previous_allowed": true,
            "is_pause_allowed": true,
            "is_play_allowed": false,
            "now_playing": {
                "title": "Beethoven Symphony No. 9",
                "artist": "Vienna Philharmonic",
                "album": "Complete Symphonies",
                "image_key": "img-abc123",
                "seek_position": 145,
                "length": 4200
            },
            "outputs": [{
                "output_id": "output-1",
                "display_name": "DAC",
                "volume": {
                    "value": 50.0,
                    "min": 0.0,
                    "max": 100.0,
                    "is_muted": false
                }
            }]
        }),
        "zones" => serde_json::json!([
            {
                "zone_id": "zone-1",
                "display_name": "Living Room",
                "state": "playing",
                "is_next_allowed": true,
                "is_previous_allowed": false,
                "is_pause_allowed": true,
                "is_play_allowed": false,
                "now_playing": null,
                "outputs": []
            },
            {
                "zone_id": "zone-2",
                "display_name": "Office",
                "state": "stopped",
                "is_next_allowed": false,
                "is_previous_allowed": false,
                "is_pause_allowed": false,
                "is_play_allowed": true,
                "now_playing": null,
                "outputs": []
            }
        ]),
        "control-request" => serde_json::json!({
            "zone_id": "zone-12345",
            "action": "play"
        }),
        "volume-request" => serde_json::json!({
            "output_id": "output-1",
            "value": 50,
            "relative": false
        }),
        "success-response" => serde_json::json!({
            "ok": true
        }),
        "error-response" => serde_json::json!({
            "error": "Zone not found: zone-invalid"
        }),
        "bus-event" => {
            // Show multiple examples for bus events
            println!("// RoonConnected event:");
            println!(
                "{}",
                serde_json::to_string_pretty(&BusEvent::RoonConnected {
                    core_name: "Roon Core".to_string(),
                    version: "2.0.0".to_string(),
                })
                .unwrap()
            );
            println!();
            println!("// ZoneUpdated event:");
            println!(
                "{}",
                serde_json::to_string_pretty(&BusEvent::ZoneUpdated {
                    zone_id: "zone-1".to_string(),
                    display_name: "Living Room".to_string(),
                    state: "playing".to_string(),
                })
                .unwrap()
            );
            println!();
            println!("// NowPlayingChanged event:");
            println!(
                "{}",
                serde_json::to_string_pretty(&BusEvent::NowPlayingChanged {
                    zone_id: "zone-1".to_string(),
                    title: Some("Test Song".to_string()),
                    artist: Some("Test Artist".to_string()),
                    album: Some("Test Album".to_string()),
                    image_key: Some("img-key".to_string()),
                })
                .unwrap()
            );
            println!();
            println!("// VolumeChanged event:");
            println!(
                "{}",
                serde_json::to_string_pretty(&BusEvent::VolumeChanged {
                    output_id: "output-1".to_string(),
                    value: 75.0,
                    is_muted: false,
                })
                .unwrap()
            );
            return;
        }
        "hqp-status" => serde_json::json!({
            "connected": true,
            "host": "192.168.1.100",
            "port": 4321,
            "info": {
                "name": "HQPlayer",
                "product": "HQPlayer Embedded",
                "version": "5.0.0",
                "platform": "Linux",
                "engine": "CUDA"
            }
        }),
        "hqp-pipeline" => serde_json::json!({
            "status": {
                "state": "Playing",
                "mode": "PCM",
                "active_mode": "PCM",
                "active_filter": "poly-sinc-gauss-hires-lp",
                "active_shaper": "NS5",
                "active_rate": 705600,
                "convolution": false,
                "invert": false
            },
            "volume": {
                "value": -12,
                "min": -60,
                "max": 0,
                "is_fixed": false
            },
            "settings": {
                "mode": {
                    "selected": { "value": "0", "label": "PCM" },
                    "options": [
                        { "value": "0", "label": "PCM" },
                        { "value": "1", "label": "SDM" }
                    ]
                },
                "filter1x": {
                    "selected": { "value": "5", "label": "poly-sinc-gauss-hires-lp" },
                    "options": []
                },
                "filter_nx": {
                    "selected": { "value": "5", "label": "poly-sinc-gauss-hires-lp" },
                    "options": []
                },
                "shaper": {
                    "selected": { "value": "3", "label": "NS5" },
                    "options": []
                },
                "samplerate": {
                    "selected": { "value": "0", "label": "Auto" },
                    "options": []
                }
            }
        }),
        "hqp-profiles" => serde_json::json!([
            { "value": "DSD_512", "title": "DSD 512" },
            { "value": "PCM_768", "title": "PCM 768kHz" },
            { "value": "Default", "title": "Default Settings" }
        ]),
        "lms-status" => serde_json::json!({
            "connected": true,
            "host": "192.168.1.50",
            "port": 9000,
            "server_version": "8.3.0",
            "player_count": 2
        }),
        "lms-player" => serde_json::json!({
            "player_id": "aa:bb:cc:dd:ee:ff",
            "name": "Kitchen Squeezebox",
            "state": "playing",
            "mode": "play",
            "power": true,
            "volume": 65,
            "muted": false,
            "current_title": "Autumn Leaves",
            "artist": "Bill Evans",
            "album": "Portrait in Jazz",
            "duration": 291.0,
            "time": 45.5,
            "artwork_url": "http://192.168.1.50:9000/music/12345/cover.jpg"
        }),
        "lms-players" => serde_json::json!([
            {
                "player_id": "aa:bb:cc:dd:ee:ff",
                "name": "Kitchen Squeezebox",
                "state": "playing",
                "mode": "play",
                "power": true,
                "volume": 65,
                "muted": false,
                "current_title": "Autumn Leaves",
                "artist": "Bill Evans",
                "album": null,
                "duration": null,
                "time": null,
                "artwork_url": null
            },
            {
                "player_id": "11:22:33:44:55:66",
                "name": "Bedroom Radio",
                "state": "stopped",
                "mode": "stop",
                "power": false,
                "volume": 40,
                "muted": false,
                "current_title": null,
                "artist": null,
                "album": null,
                "duration": null,
                "time": null,
                "artwork_url": null
            }
        ]),
        _ => {
            eprintln!("Unknown schema type: {}", schema_type);
            eprintln!("Run 'protocol_checker list-types' to see supported types");
            process::exit(1);
        }
    };

    println!("{}", serde_json::to_string_pretty(&example).unwrap());
}
