//! UDP fast-path listener for knob polling and commands.
//!
//! Provides a compact binary protocol for the knob's fast-path state updates,
//! avoiding HTTP + JSON overhead for the ~2s poll cycle.
//!
//! Wire format:
//! - Poll request (54 bytes): `[magic:u16 LE][sha:20 bytes][zone_id:32 bytes]`
//! - Poll response (48 bytes): `[magic:u16 LE][version:u8][flags:u8][sha:20 bytes]`
//!   `[volume:f32 LE][volume_min:f32 LE][volume_max:f32 LE][volume_step:f32 LE]`
//!   `[seek_position:i32 LE][length:u32 LE]`
//! - Command (40 bytes): `[magic:u16 LE][cmd:u8][_pad:u8][zone_id:32 bytes][value:f32 LE]`
//!   cmd=5: volume_set (value is absolute volume as f32)

use std::net::SocketAddr;
use tokio::net::UdpSocket;

use crate::api::AppState;
use crate::bus::PlaybackState;
use crate::knobs::manifest::compute_manifest_sha;

/// "RK" in little-endian
const MAGIC: u16 = 0x524B;
const POLL_REQUEST_SIZE: usize = 54;
const RESPONSE_SIZE: usize = 48;
const WIRE_VERSION: u8 = 1;
/// Minimum command packet size: magic(2) + cmd(1) + pad(1) + zone_id(32)
const CMD_SIZE: usize = 36;
/// Command packet with f32 value appended
const CMD_VOL_SIZE: usize = 40;
/// Command code: set absolute volume
const CMD_VOLUME_SET: u8 = 5;

/// Run the UDP fast-path listener. Never returns unless the socket fails to bind.
pub async fn run_udp_fast_path(state: AppState, port: u16) -> std::io::Result<()> {
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let socket = UdpSocket::bind(addr).await?;
    tracing::info!("UDP fast-path listening on {}", addr);

    let mut buf = [0u8; POLL_REQUEST_SIZE];

    loop {
        let (len, peer) = match socket.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("UDP recv error: {}", e);
                continue;
            }
        };

        if len < CMD_SIZE {
            tracing::debug!("UDP packet too short ({} bytes) from {}", len, peer);
            continue;
        }
        // Validate magic
        let magic = u16::from_le_bytes([buf[0], buf[1]]);
        if magic != MAGIC {
            tracing::debug!("UDP bad magic 0x{:04X} from {}", magic, peer);
            continue;
        }

        if len >= POLL_REQUEST_SIZE {
            // Poll request (54 bytes) — existing behavior
            let client_sha = extract_null_terminated(&buf[2..22]);
            let zone_id = extract_null_terminated(&buf[22..54]);
            let response = build_response(&state, &client_sha, &zone_id).await;
            if let Err(e) = socket.send_to(&response, peer).await {
                tracing::warn!("UDP send error to {}: {}", peer, e);
            }
        } else if len >= CMD_SIZE {
            // Command packet (36-40 bytes)
            let cmd = buf[2];
            let zone_id = extract_null_terminated(&buf[4..36]);
            let value = if len >= CMD_VOL_SIZE {
                Some(f32::from_le_bytes([buf[36], buf[37], buf[38], buf[39]]))
            } else {
                None
            };

            let cmd_state = state.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_command(&cmd_state, cmd, &zone_id, value).await {
                    tracing::debug!("UDP command error: {}", e);
                }
            });
        }
    }
}

/// Handle a UDP command packet. Fire-and-forget — errors are logged, not returned to sender.
async fn handle_command(
    state: &AppState,
    cmd: u8,
    zone_id: &str,
    value: Option<f32>,
) -> Result<(), String> {
    match cmd {
        CMD_VOLUME_SET => {
            let vol = value.ok_or_else(|| "volume_set command missing value".to_string())?;

            // Normalize zone_id (legacy without prefix = Roon)
            let prefixed = if zone_id.is_empty() {
                return Err("empty zone_id".to_string());
            } else if !zone_id.contains(':') {
                format!("roon:{}", zone_id)
            } else {
                zone_id.to_string()
            };

            // Route by prefix to the correct adapter
            if prefixed.starts_with("lms:") {
                let player_id = prefixed.trim_start_matches("lms:");
                state
                    .lms
                    .change_volume(player_id, vol, false)
                    .await
                    .map_err(|e| e.to_string())?;
            } else if prefixed.starts_with("openhome:") {
                let udn = prefixed.trim_start_matches("openhome:");
                state
                    .openhome
                    .control(udn, "vol_abs", Some(vol as i32))
                    .await
                    .map_err(|e| e.to_string())?;
            } else if prefixed.starts_with("upnp:") {
                let udn = prefixed.trim_start_matches("upnp:");
                state
                    .upnp
                    .control(udn, "vol_abs", Some(vol as i32))
                    .await
                    .map_err(|e| e.to_string())?;
            } else {
                // Roon (default)
                let roon_id = prefixed.trim_start_matches("roon:");
                state
                    .roon
                    .change_volume(roon_id, vol, false)
                    .await
                    .map_err(|e| e.to_string())?;
            }

            tracing::debug!("UDP volume_set zone={} vol={}", zone_id, vol);
            Ok(())
        }
        _ => {
            tracing::debug!("UDP unknown command code {} from zone={}", cmd, zone_id);
            Ok(())
        }
    }
}

/// Extract a null-terminated UTF-8 string from a fixed-size field.
fn extract_null_terminated(field: &[u8]) -> String {
    let end = field.iter().position(|&b| b == 0).unwrap_or(field.len());
    String::from_utf8_lossy(&field[..end]).into_owned()
}

/// Build the 48-byte UDP response.
async fn build_response(state: &AppState, _client_sha: &str, zone_id: &str) -> [u8; RESPONSE_SIZE] {
    let mut resp = [0u8; RESPONSE_SIZE];

    // Normalize zone_id (legacy without prefix = Roon)
    let prefixed_zone_id = if zone_id.is_empty() {
        // No zone specified — pick first available
        let zones = state.aggregator.get_zones().await;
        match zones.first() {
            Some(z) => z.zone_id.clone(),
            None => return make_empty_response(),
        }
    } else if !zone_id.contains(':') {
        format!("roon:{}", zone_id)
    } else {
        zone_id.to_string()
    };

    let zone = match state.aggregator.get_zone(&prefixed_zone_id).await {
        Some(z) => z,
        None => return make_empty_response(),
    };

    // Build fast state fields
    let is_playing = zone.state == PlaybackState::Playing;
    let vc = zone.volume_control.as_ref();
    let np = zone.now_playing.as_ref();

    let volume = vc.map(|v| v.value).unwrap_or(0.0);
    let volume_min = vc.map(|v| v.min).unwrap_or(0.0);
    let volume_max = vc.map(|v| v.max).unwrap_or(0.0);
    let volume_step = vc.map(|v| v.step).unwrap_or(1.0);
    let seek_position = np
        .and_then(|n| n.seek_position.map(|p| p as i32))
        .unwrap_or(-1);
    let length = np.and_then(|n| n.duration.map(|d| d as u32)).unwrap_or(0);

    // Flags
    let flags: u8 = (is_playing as u8)
        | ((zone.is_play_allowed as u8) << 1)
        | ((zone.is_pause_allowed as u8) << 2)
        | ((zone.is_next_allowed as u8) << 3)
        | ((zone.is_previous_allowed as u8) << 4);

    // Compute current SHA
    let pushed_sha = state.manifests.get_pushed_sha().await;
    let sha = if let Some(sha) = pushed_sha {
        sha
    } else {
        // Build default manifest to compute SHA (mirrors manifest_routes.rs logic)
        let (screens, nav) = build_default_screens(state, &zone, &prefixed_zone_id).await;
        compute_manifest_sha(&screens, &nav)
    };

    // Pack response
    resp[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    resp[2] = WIRE_VERSION;
    resp[3] = flags;

    // SHA as null-terminated string in 20-byte field
    let sha_bytes = sha.as_bytes();
    let sha_len = sha_bytes.len().min(19); // leave room for null terminator
    resp[4..4 + sha_len].copy_from_slice(&sha_bytes[..sha_len]);
    // rest of 4..24 already zeroed

    resp[24..28].copy_from_slice(&volume.to_le_bytes());
    resp[28..32].copy_from_slice(&volume_min.to_le_bytes());
    resp[32..36].copy_from_slice(&volume_max.to_le_bytes());
    resp[36..40].copy_from_slice(&volume_step.to_le_bytes());
    resp[40..44].copy_from_slice(&seek_position.to_le_bytes());
    resp[44..48].copy_from_slice(&length.to_le_bytes());

    resp
}

/// Build default screens for SHA computation (mirrors manifest_routes.rs build_default_manifest).
async fn build_default_screens(
    state: &AppState,
    zone: &crate::bus::Zone,
    zone_id: &str,
) -> (
    Vec<crate::knobs::manifest::Screen>,
    crate::knobs::manifest::Nav,
) {
    use crate::knobs::manifest::*;
    use crate::knobs::routes::get_all_zones_internal;

    let np = zone.now_playing.as_ref();

    let line1 = np
        .map(|n| {
            if n.title.is_empty() {
                "Idle".to_string()
            } else {
                n.title.clone()
            }
        })
        .unwrap_or_else(|| "Idle".to_string());
    let line2 = np.map(|n| n.artist.clone()).unwrap_or_default();
    let line3 = np.and_then(|n| {
        if n.album.is_empty() {
            None
        } else {
            Some(n.album.clone())
        }
    });

    let image_url = format!(
        "/knob/now_playing/image?zone_id={}",
        urlencoding::encode(zone_id)
    );
    let image_key = np.and_then(|n| n.image_key.clone());

    let mut lines = vec![
        TextLine {
            text: line1,
            style: "title".to_string(),
        },
        TextLine {
            text: line2,
            style: "subtitle".to_string(),
        },
    ];
    if let Some(album) = line3 {
        lines.push(TextLine {
            text: album,
            style: "detail".to_string(),
        });
    }

    let media = Screen::Media(MediaScreen {
        id: "now_playing".to_string(),
        image_url: Some(image_url),
        image_key,
        lines,
    });

    let zone_infos = get_all_zones_internal(state).await;
    let zones_screen = build_zones_screen(&zone_infos, zone_id);

    let screens = vec![media, zones_screen];
    let nav = Nav {
        order: vec!["now_playing".to_string(), "zones".to_string()],
        default: "now_playing".to_string(),
    };

    (screens, nav)
}

/// Build zones list screen (mirrors manifest_routes.rs).
fn build_zones_screen(
    zones: &[crate::knobs::routes::ZoneInfo],
    current_zone_id: &str,
) -> crate::knobs::manifest::Screen {
    use crate::knobs::manifest::*;

    let items = zones
        .iter()
        .map(|z| ListItem {
            id: z.zone_id.clone(),
            label: z.zone_name.clone(),
            sublabel: Some(z.state.clone()),
            selected: z.zone_id == current_zone_id,
            icon: None,
        })
        .collect();

    Screen::List(ListScreen {
        id: "zones".to_string(),
        title: Some("Zones".to_string()),
        items,
    })
}

/// Empty response when no zones are available.
fn make_empty_response() -> [u8; RESPONSE_SIZE] {
    let mut resp = [0u8; RESPONSE_SIZE];
    resp[0..2].copy_from_slice(&MAGIC.to_le_bytes());
    resp[2] = WIRE_VERSION;
    // flags = 0, sha = zeros, volume fields = 0.0
    // seek_position = -1
    resp[40..44].copy_from_slice(&(-1i32).to_le_bytes());
    resp
}
