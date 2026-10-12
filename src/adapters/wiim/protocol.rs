//! WiiM / LinkPlay wire formats: the `httpapi.asp` HTTP API and the mDNS record. Pure functions
//! only, so every encoding decision is unit-tested.
//!
//! Sources (no WiiM hardware was available; every behaviour here is implementation-derived):
//! - WiiM "HTTP API for WiiM Products" and the LinkPlay "HTTP API" document
//!   (<https://developer.arylic.com/httpapi/>, <https://www.wiimhome.com/pdf/HTTP%20API%20for%20WiiM%20Products.pdf>):
//!   `getStatusEx`, `getPlayerStatus`, `setPlayerCmd:vol:N`, `setPlayerCmd:mute:N`,
//!   `setPlayerCmd:switchmode:<input>`, and the `mode` numbers of `getPlayerStatus`.
//! - Reference client <https://github.com/Linkplay2020/wiim> (`wiim/endpoint.py`, MIT), which
//!   drives the same endpoints.
//!
//! # Boundaries
//!
//! - **No power.** LinkPlay has no power command; `pause`/`stop` are transport, not power. Power
//!   operations are refused rather than modelled as pause (catalog: "do not model pause as power").
//! - Volume is `vol` 0..=100 (a percentage). 100 is the protocol full scale and is used as the maximum.

use serde_json::Value;

use crate::devices::service::DeviceOperation;
use crate::devices::DeviceInput;

/// mDNS service type LinkPlay devices advertise.
pub const MDNS_SERVICE: &str = "_linkplay._tcp.local.";

pub const STATUS_EX_PATH: &str = "/httpapi.asp?command=getStatusEx";
pub const PLAYER_STATUS_PATH: &str = "/httpapi.asp?command=getPlayerStatus";

pub const MIN_VOLUME: f32 = 0.0;
pub const MAX_VOLUME: f32 = 100.0;

/// Why power is refused; surfaced verbatim to the caller.
pub const NO_POWER: &str =
    "power is not supported by this device: WiiM/LinkPlay exposes no power command (pause is \
     transport, not power)";

/// `switchmode` arguments the HTTP API documents, with display names. Which of them a given model
/// physically has is model dependent and not reported by the device; the device refuses the rest.
pub const INPUTS: &[(&str, &str)] = &[
    ("wifi", "Network"),
    ("line-in", "Line In"),
    ("bluetooth", "Bluetooth"),
    ("optical", "Optical"),
    ("co-axial", "Coaxial"),
    ("RCA", "RCA"),
    ("HDMI", "HDMI"),
];

/// The input catalog as [`DeviceInput`]s.
pub fn input_catalog() -> Vec<DeviceInput> {
    INPUTS
        .iter()
        .map(|(id, name)| DeviceInput {
            id: (*id).to_string(),
            name: (*name).to_string(),
            available: true,
        })
        .collect()
}

/// Identity and labels from `getStatusEx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEx {
    pub uuid: Option<String>,
    pub mac: Option<String>,
    /// `DeviceName`, e.g. `WiiM Mini-8FA2`.
    pub name: String,
    /// `project`, e.g. `Muzo_Mini`, `WiiM_Pro_with_gc4a`.
    pub project: String,
}

impl StatusEx {
    /// The hardware id: the `uuid` (stable across Wi-Fi and Ethernet), else the `MAC`. Never the
    /// name, project or IP.
    pub fn hardware_id(&self) -> Option<String> {
        [self.uuid.as_deref(), self.mac.as_deref()]
            .into_iter()
            .flatten()
            .map(str::trim)
            .find(|value| !value.is_empty())
            .map(str::to_string)
    }

    /// Model label: the `project` with underscores as spaces.
    pub fn model(&self) -> String {
        self.project.replace('_', " ")
    }
}

fn text(value: &Value, key: &str) -> Option<String> {
    match value.get(key)? {
        Value::String(s) => Some(s.trim().to_string()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Parse a `getStatusEx` body. `None` unless it is a JSON object carrying a `uuid` or `MAC`, which
/// is what separates a LinkPlay device from any other host answering on the same port.
pub fn parse_status_ex(body: &str) -> Option<StatusEx> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    let status = StatusEx {
        uuid: text(&value, "uuid").filter(|v| !v.is_empty()),
        mac: text(&value, "MAC").filter(|v| !v.is_empty()),
        name: text(&value, "DeviceName").unwrap_or_default(),
        project: text(&value, "project").unwrap_or_default(),
    };
    status.hardware_id()?;
    Some(status)
}

/// What `getPlayerStatus` tells the device layer.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlayerStatus {
    pub volume: Option<f32>,
    pub muted: Option<bool>,
    /// The current input as a `switchmode` id, when the `mode` maps to one.
    pub input: Option<String>,
}

/// Parse a `getPlayerStatus` body. Values arrive as strings (`"vol":"40"`). A `vol` outside
/// 0..=100 is refused, never clamped into a plausible level.
pub fn parse_player_status(body: &str) -> Option<PlayerStatus> {
    let value: Value = serde_json::from_str(body.trim()).ok()?;
    if !value.is_object() {
        return None;
    }
    let volume = text(&value, "vol")
        .and_then(|v| v.parse::<f32>().ok())
        .filter(|v| (MIN_VOLUME..=MAX_VOLUME).contains(v));
    let muted = text(&value, "mute").and_then(|v| match v.as_str() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    });
    let input = text(&value, "mode")
        .and_then(|v| v.parse::<u32>().ok())
        .and_then(input_for_mode);
    if volume.is_none() && muted.is_none() && input.is_none() && value.get("status").is_none() {
        return None;
    }
    Some(PlayerStatus {
        volume,
        muted,
        input,
    })
}

/// Map a `getPlayerStatus` `mode` to a `switchmode` id. Network sources (AirPlay 1, DLNA 2,
/// playlist 10, HTTP API 20, Spotify Connect 31 and the other streaming services 30..39) are all
/// the `wifi` input; 0 (idle) and 99 (multiroom guest) name no input.
pub fn input_for_mode(mode: u32) -> Option<String> {
    let id = match mode {
        1 | 2 | 10 | 20 | 30..=39 => "wifi",
        40 | 46 => "line-in",
        41 => "bluetooth",
        42 => "optical",
        43 => "RCA",
        44 => "co-axial",
        _ => return None,
    };
    Some(id.to_string())
}

/// Encode an absolute level. Refuses anything not finite or outside 0..=100: the service has
/// already clamped, so a refusal here means a caller bypassed it.
pub fn encode_volume(level: f32) -> Option<u8> {
    (level.is_finite() && (MIN_VOLUME..=MAX_VOLUME).contains(&level)).then(|| level.round() as u8)
}

/// Resolve an input id or display name (case-insensitive) to a catalog `switchmode` id.
pub fn resolve_input(requested: &str) -> Option<&'static str> {
    let requested = requested.trim();
    INPUTS
        .iter()
        .find(|(id, name)| {
            id.eq_ignore_ascii_case(requested) || name.eq_ignore_ascii_case(requested)
        })
        .map(|(id, _)| *id)
}

/// The request path for an operation, or the refusal for one the device cannot do.
pub fn command_path(operation: &DeviceOperation) -> Result<String, String> {
    match operation {
        DeviceOperation::SetVolume(level) => encode_volume(*level)
            .map(|v| format!("/httpapi.asp?command=setPlayerCmd:vol:{v}"))
            .ok_or_else(|| format!("volume {level} is outside the device's 0-100 range")),
        DeviceOperation::SetMute(muted) => Ok(format!(
            "/httpapi.asp?command=setPlayerCmd:mute:{}",
            u8::from(*muted)
        )),
        DeviceOperation::SelectInput(input) => resolve_input(input)
            .map(|id| format!("/httpapi.asp?command=setPlayerCmd:switchmode:{id}"))
            .ok_or_else(|| format!("unknown input {input:?}")),
        DeviceOperation::PowerOn | DeviceOperation::PowerStandby => Err(NO_POWER.to_string()),
    }
}

/// Whether a `setPlayerCmd` reply means success. The device answers the literal `OK`.
pub fn command_accepted(body: &str) -> bool {
    body.trim().eq_ignore_ascii_case("ok")
}

#[cfg(test)]
mod tests {
    use super::*;

    // Shape per the WiiM/LinkPlay HTTP API document (getStatusEx example).
    const STATUS_EX: &str = r#"{"uuid":"FF970016A6FE22C1660AB4D8","DeviceName":"WiiM Mini-8FA2","GroupName":"WiiM Mini-8FA2","ssid":"WiiM Mini-8FA2","firmware":"Linkplay.4.6.415145","hardware":"ALLWINNER-R328","project":"Muzo_Mini","MAC":"08:E9:F6:8F:8F:A2"}"#;
    // Shape per the LinkPlay HTTP API document (getPlayerStatus example); strings throughout.
    const PLAYER: &str = r#"{"type":"0","ch":"0","mode":"31","loop":"0","eq":"0","status":"play","curpos":"1000","totlen":"200000","vol":"40","mute":"0"}"#;

    #[test]
    fn status_ex_identity_prefers_uuid_then_mac() {
        let s = parse_status_ex(STATUS_EX).unwrap();
        assert_eq!(s.hardware_id().as_deref(), Some("FF970016A6FE22C1660AB4D8"));
        assert_eq!(s.model(), "Muzo Mini");
        assert_eq!(s.name, "WiiM Mini-8FA2");
        let only_mac = parse_status_ex(r#"{"MAC":"08:E9:F6:8F:8F:A2","DeviceName":"x"}"#).unwrap();
        assert_eq!(only_mac.hardware_id().as_deref(), Some("08:E9:F6:8F:8F:A2"));
    }

    #[test]
    fn status_ex_without_identity_or_json_is_refused() {
        assert!(parse_status_ex(r#"{"DeviceName":"x","project":"y"}"#).is_none());
        assert!(parse_status_ex(r#"{"uuid":"  "}"#).is_none());
        assert!(parse_status_ex("<html>not linkplay</html>").is_none());
        assert!(parse_status_ex("[1,2]").is_none());
        assert!(parse_status_ex("").is_none());
    }

    #[test]
    fn player_status_parses_string_values() {
        let p = parse_player_status(PLAYER).unwrap();
        assert_eq!(p.volume, Some(40.0));
        assert_eq!(p.muted, Some(false));
        assert_eq!(p.input.as_deref(), Some("wifi"));
        let muted = parse_player_status(r#"{"mode":"40","vol":"0","mute":"1"}"#).unwrap();
        assert_eq!(muted.volume, Some(0.0));
        assert_eq!(muted.muted, Some(true));
        assert_eq!(muted.input.as_deref(), Some("line-in"));
    }

    #[test]
    fn player_status_refuses_malformed_and_out_of_range() {
        assert!(parse_player_status("garbage").is_none());
        assert!(parse_player_status("{}").is_none());
        let bad = parse_player_status(r#"{"status":"stop","vol":"250","mute":"7"}"#).unwrap();
        assert_eq!(bad.volume, None);
        assert_eq!(bad.muted, None);
        let negative = parse_player_status(r#"{"status":"stop","vol":"-1"}"#).unwrap();
        assert_eq!(negative.volume, None);
    }

    #[test]
    fn modes_map_to_switchmode_ids() {
        assert_eq!(input_for_mode(1).as_deref(), Some("wifi"));
        assert_eq!(input_for_mode(32).as_deref(), Some("wifi"));
        assert_eq!(input_for_mode(41).as_deref(), Some("bluetooth"));
        assert_eq!(input_for_mode(42).as_deref(), Some("optical"));
        assert_eq!(input_for_mode(43).as_deref(), Some("RCA"));
        assert_eq!(input_for_mode(44).as_deref(), Some("co-axial"));
        assert_eq!(input_for_mode(0), None);
        assert_eq!(input_for_mode(99), None);
    }

    #[test]
    fn volume_encodes_at_min_max_and_refuses_outside() {
        assert_eq!(encode_volume(0.0), Some(0));
        assert_eq!(encode_volume(100.0), Some(100));
        assert_eq!(encode_volume(37.6), Some(38));
        assert_eq!(encode_volume(100.5), None);
        assert_eq!(encode_volume(-0.5), None);
        assert_eq!(encode_volume(f32::NAN), None);
        assert_eq!(encode_volume(f32::INFINITY), None);
    }

    #[test]
    fn commands_encode_to_documented_paths() {
        assert_eq!(
            command_path(&DeviceOperation::SetVolume(30.0)).unwrap(),
            "/httpapi.asp?command=setPlayerCmd:vol:30"
        );
        assert_eq!(
            command_path(&DeviceOperation::SetMute(true)).unwrap(),
            "/httpapi.asp?command=setPlayerCmd:mute:1"
        );
        assert_eq!(
            command_path(&DeviceOperation::SetMute(false)).unwrap(),
            "/httpapi.asp?command=setPlayerCmd:mute:0"
        );
        assert_eq!(
            command_path(&DeviceOperation::SelectInput("line-in".into())).unwrap(),
            "/httpapi.asp?command=setPlayerCmd:switchmode:line-in"
        );
        // Display names resolve; arbitrary text never reaches the URL.
        assert_eq!(
            command_path(&DeviceOperation::SelectInput("optical".into())).unwrap(),
            "/httpapi.asp?command=setPlayerCmd:switchmode:optical"
        );
        assert!(command_path(&DeviceOperation::SelectInput("x&command=reboot".into())).is_err());
    }

    #[test]
    fn power_is_refused_not_modelled_as_pause() {
        for op in [DeviceOperation::PowerOn, DeviceOperation::PowerStandby] {
            let err = command_path(&op).unwrap_err();
            assert!(err.contains("not supported"), "{err}");
        }
        assert!(command_path(&DeviceOperation::SetVolume(101.0)).is_err());
    }

    #[test]
    fn reply_ok_is_the_only_success() {
        assert!(command_accepted("OK\n"));
        assert!(!command_accepted("Failed"));
        assert!(!command_accepted("unknown command"));
    }
}
