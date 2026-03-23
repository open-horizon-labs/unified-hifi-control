//! UDP fast-path listener for knob polling and commands.
//!
//! Provides a compact binary protocol for the knob's fast-path state updates,
//! avoiding HTTP + JSON overhead for the ~2s poll cycle.
//!
//! Wire format (v2 — 64-byte zone_id):
//! - Poll request (86 bytes): `[magic:u16 LE][sha:20 bytes][zone_id:64 bytes]`
//! - Poll response (48 bytes): `[magic:u16 LE][version:u8][flags:u8][sha:20 bytes]`
//!   `[volume:f32 LE][volume_min:f32 LE][volume_max:f32 LE][volume_step:f32 LE]`
//!   `[seek_position:i32 LE][length:u32 LE]`
//! - Command (72 bytes): `[magic:u16 LE][cmd:u8][_pad:u8][zone_id:64 bytes][value:f32 LE]`
//!   cmd=5: volume_set (value is absolute volume as f32)

use std::net::SocketAddr;
use tokio::net::UdpSocket;

use crate::api::AppState;
use crate::bus::PlaybackState;
use crate::knobs::manifest::{compute_manifest_sha, compute_manifest_sha_full};

/// "RK" in little-endian
const MAGIC: u16 = 0x524B;
/// Poll request: magic(2) + sha(20) + zone_id(64) = 86
const POLL_REQUEST_SIZE: usize = 86;
const RESPONSE_SIZE: usize = 48;
const WIRE_VERSION: u8 = 1;
/// Minimum command packet size: magic(2) + cmd(1) + pad(1) + zone_id(64)
const CMD_SIZE: usize = 68;
/// Command packet with f32 value appended: CMD_SIZE + f32(4) = 72
const CMD_VOL_SIZE: usize = 72;
/// Size of the zone_id field in wire structs (accommodates 41-char Roon IDs)
#[cfg(test)]
const ZONE_ID_FIELD_SIZE: usize = 64;
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
            // Poll request (86 bytes): [magic:2][sha:20][zone_id:64]
            let client_sha = extract_null_terminated(&buf[2..22]);
            let zone_id = extract_null_terminated(&buf[22..86]);
            let response = build_response(&state, &client_sha, &zone_id, peer).await;
            if let Err(e) = socket.send_to(&response, peer).await {
                tracing::warn!("UDP send error to {}: {}", peer, e);
            }
        } else if len >= CMD_SIZE {
            // Command packet (68-72 bytes): [magic:2][cmd:1][pad:1][zone_id:64][value:4]
            let cmd = buf[2];
            let zone_id = extract_null_terminated(&buf[4..68]);
            let value = if len >= CMD_VOL_SIZE {
                Some(f32::from_le_bytes([buf[68], buf[69], buf[70], buf[71]]))
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
            if zone_id.is_empty() {
                return Err("empty zone_id".to_string());
            }
            let prefixed = super::normalize_zone_id(zone_id);

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
            } else if prefixed.starts_with("roon:") || !prefixed.contains(':') {
                // Roon (explicit prefix or unprefixed legacy ID)
                let roon_id = prefixed.trim_start_matches("roon:");
                state
                    .roon
                    .change_volume(roon_id, vol, false)
                    .await
                    .map_err(|e| e.to_string())?;
            } else {
                return Err(format!("Unknown zone prefix in '{}'", prefixed));
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
async fn build_response(
    state: &AppState,
    _client_sha: &str,
    zone_id: &str,
    addr: SocketAddr,
) -> [u8; RESPONSE_SIZE] {
    let mut resp = [0u8; RESPONSE_SIZE];

    // Normalize zone_id (legacy without prefix = Roon)
    let prefixed_zone_id = if zone_id.is_empty() {
        // No zone specified — pick first available
        let zones = state.aggregator.get_zones().await;
        match zones.first() {
            Some(z) => z.zone_id.clone(),
            None => return make_empty_response(),
        }
    } else {
        super::normalize_zone_id(zone_id)
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
    let pushed_sha = state.manifests.get_pushed_sha(&prefixed_zone_id).await;
    let sha = if let Some(sha) = pushed_sha {
        // Memex push takes priority
        sha
    } else {
        // Look up device_id by source IP for layout condition-stack check
        let peer_ip = addr.ip().to_string();
        let device_id = state.knobs.find_by_ip(&peer_ip).await;
        tracing::debug!(peer_ip = %peer_ip, device_id = ?device_id, "UDP device resolution");

        if let Some(ref dev_id) = device_id {
            // Check layout condition stack for this device
            if let Some(layout) = state.layouts.resolve_layout(dev_id, &zone).await {
                let screens =
                    crate::knobs::manifest_routes::merge_live_state(layout.screens, &zone);
                compute_manifest_sha_full(&screens, &layout.nav, &layout.interactions)
            } else {
                // No matching layout — fall through to default
                let (screens, nav) = crate::knobs::manifest_routes::build_default_manifest(
                    state,
                    &zone,
                    &prefixed_zone_id,
                )
                .await;
                compute_manifest_sha(&screens, &nav)
            }
        } else {
            // No device_id known — build default manifest to compute SHA
            let (screens, nav) = crate::knobs::manifest_routes::build_default_manifest(
                state,
                &zone,
                &prefixed_zone_id,
            )
            .await;
            compute_manifest_sha(&screens, &nav)
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── Wire format size assertions ──────────────────────────────────

    #[test]
    fn wire_sizes_are_self_consistent() {
        // Poll: magic(2) + sha(20) + zone_id(64)
        assert_eq!(POLL_REQUEST_SIZE, 2 + 20 + ZONE_ID_FIELD_SIZE);
        // Command minimum: magic(2) + cmd(1) + pad(1) + zone_id(64)
        assert_eq!(CMD_SIZE, 2 + 1 + 1 + ZONE_ID_FIELD_SIZE);
        // Command with value: CMD_SIZE + f32(4)
        assert_eq!(CMD_VOL_SIZE, CMD_SIZE + 4);
    }

    #[test]
    fn zone_id_field_fits_roon_ids() {
        // Roon zone IDs are 41 chars: "roon:" (5) + 32-char hex = 37, or with longer hashes
        // Real example: "roon:160170c84d9088ec6e42b34500d67e60c231" = 41 chars
        let roon_id = "roon:160170c84d9088ec6e42b34500d67e60c231";
        assert_eq!(roon_id.len(), 41);
        assert!(
            roon_id.len() < ZONE_ID_FIELD_SIZE,
            "Roon zone ID ({} bytes) must fit in zone_id field ({} bytes) with null terminator",
            roon_id.len(),
            ZONE_ID_FIELD_SIZE
        );
    }

    // ── extract_null_terminated ──────────────────────────────────────

    #[test]
    fn extract_null_terminated_basic() {
        let mut field = [0u8; 64];
        let id = b"roon:abc123";
        field[..id.len()].copy_from_slice(id);
        assert_eq!(extract_null_terminated(&field), "roon:abc123");
    }

    #[test]
    fn extract_null_terminated_full_field() {
        // Field completely filled, no null terminator
        let field = [b'x'; 64];
        assert_eq!(extract_null_terminated(&field).len(), 64);
    }

    #[test]
    fn extract_null_terminated_empty() {
        let field = [0u8; 64];
        assert_eq!(extract_null_terminated(&field), "");
    }

    #[test]
    fn extract_null_terminated_roon_zone_id() {
        let mut field = [0u8; 64];
        let id = b"roon:160170c84d9088ec6e42b34500d67e60c231";
        field[..id.len()].copy_from_slice(id);
        assert_eq!(
            extract_null_terminated(&field),
            "roon:160170c84d9088ec6e42b34500d67e60c231"
        );
    }

    // ── Poll request packet layout ──────────────────────────────────

    #[test]
    fn poll_request_layout() {
        let mut pkt = [0u8; POLL_REQUEST_SIZE];
        let zone_id = b"roon:160170c84d9088ec6e42b34500d67e60c231";

        // Pack: [magic:2][sha:20][zone_id:64]
        pkt[0..2].copy_from_slice(&MAGIC.to_le_bytes());
        pkt[2..22].copy_from_slice(&[0xAA; 20]); // dummy sha
        pkt[22..22 + zone_id.len()].copy_from_slice(zone_id);

        // Verify magic
        assert_eq!(u16::from_le_bytes([pkt[0], pkt[1]]), MAGIC);
        // Verify zone_id extraction at correct offset
        assert_eq!(
            extract_null_terminated(&pkt[22..86]),
            "roon:160170c84d9088ec6e42b34500d67e60c231"
        );
    }

    // ── Command packet layout ───────────────────────────────────────

    #[test]
    fn command_packet_layout_with_value() {
        let mut pkt = [0u8; CMD_VOL_SIZE];
        let zone_id = b"roon:160170c84d9088ec6e42b34500d67e60c231";
        let volume: f32 = -12.5;

        // Pack: [magic:2][cmd:1][pad:1][zone_id:64][value:4]
        pkt[0..2].copy_from_slice(&MAGIC.to_le_bytes());
        pkt[2] = CMD_VOLUME_SET;
        pkt[3] = 0; // pad
        pkt[4..4 + zone_id.len()].copy_from_slice(zone_id);
        pkt[68..72].copy_from_slice(&volume.to_le_bytes());

        // Verify fields at correct offsets
        assert_eq!(u16::from_le_bytes([pkt[0], pkt[1]]), MAGIC);
        assert_eq!(pkt[2], CMD_VOLUME_SET);
        assert_eq!(
            extract_null_terminated(&pkt[4..68]),
            "roon:160170c84d9088ec6e42b34500d67e60c231"
        );
        assert_eq!(
            f32::from_le_bytes([pkt[68], pkt[69], pkt[70], pkt[71]]),
            -12.5
        );
    }

    #[test]
    fn command_packet_layout_without_value() {
        let mut pkt = [0u8; CMD_SIZE];
        let zone_id = b"lms:ab:cd:ef:12:34:56";

        pkt[0..2].copy_from_slice(&MAGIC.to_le_bytes());
        pkt[2] = 0xFF; // hypothetical future command
        pkt[4..4 + zone_id.len()].copy_from_slice(zone_id);

        assert_eq!(
            extract_null_terminated(&pkt[4..68]),
            "lms:ab:cd:ef:12:34:56"
        );
        // No value field in CMD_SIZE packet
        assert_eq!(pkt.len(), CMD_SIZE);
    }

    // ── Response packet (unchanged) ─────────────────────────────────

    #[test]
    fn empty_response_has_correct_magic_and_seek() {
        let resp = make_empty_response();
        assert_eq!(u16::from_le_bytes([resp[0], resp[1]]), MAGIC);
        assert_eq!(resp[2], WIRE_VERSION);
        assert_eq!(resp[3], 0); // flags = 0
        assert_eq!(
            i32::from_le_bytes([resp[40], resp[41], resp[42], resp[43]]),
            -1
        );
        assert_eq!(resp.len(), RESPONSE_SIZE);
    }

    #[test]
    fn response_size_unchanged() {
        // Response has no zone_id field — must remain 48 bytes
        assert_eq!(RESPONSE_SIZE, 48);
    }
}
