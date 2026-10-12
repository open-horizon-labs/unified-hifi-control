//! Denon/Marantz AVR wire formats: the Telnet ASCII protocol on TCP 23 and the `goform` HTTP/XML
//! documents. Pure functions only, so every encoding decision is unit-tested against captured
//! hardware bytes (`tests/fixtures/devices/marantz/sr6008/`).
//!
//! Command spellings and the source mapping are harvested from `denonavr` (MIT, (c) 2021 Oliver
//! Goetz, `denonavr/const.py` on `master` as of 2026-10-12). Every behaviour below that the
//! captures cover is cited to them; the rest is marked implementation-derived.

use crate::devices::service::DeviceOperation;
use crate::devices::{DeviceInput, DevicePower};

/// Queries sent on connect and after every command. Queries only: **no level is ever sent on
/// connect**, so a reconnect cannot change what the amplifier is doing.
pub const STATE_QUERIES: &[&str] = &["PW?", "ZM?", "MV?", "MU?", "SI?"];

/// Lowest level the protocol can express, in dB (`MV00`).
pub const MIN_DB: f32 = -80.0;

/// Offset between the Telnet `MV` value and dB: `MV405` is 40.5 absolute, -39.5 dB (captured).
const MV_OFFSET_DB: f32 = 80.0;

/// One state fact from a Telnet line.
#[derive(Debug, Clone, PartialEq)]
pub enum TelnetEvent {
    /// `PWON` / `PWSTANDBY`: whole-unit power.
    UnitPower(bool),
    /// `ZMON` / `ZMOFF`: main-zone power, which is what UHC calls the device's power.
    MainZonePower(bool),
    /// `MV405`: main-zone level, in dB.
    VolumeDb(f32),
    /// `MVMAX 70`: the unit's own maximum, in dB.
    VolumeMaxDb(f32),
    /// `MUON` / `MUOFF`.
    Mute(bool),
    /// `SIMPLAY`: current input code.
    Input(String),
}

/// Split a receive buffer into complete CR-terminated lines, leaving any partial line in place.
pub fn drain_lines(buffer: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(end) = buffer.find('\r') {
        let line: String = buffer.drain(..=end).collect();
        let line = line.trim_end_matches('\r').trim_start_matches('\n').trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Parse one Telnet line. Lines this adapter does not model — zone 2/3 (`Z2…`, `Z3…`), surround
/// (`MS…`, `PS…`), video (`SV…`) and the interleaved `@`-protocol (`@VOL:-395`) — return `None`.
pub fn parse_line(line: &str) -> Option<TelnetEvent> {
    let line = line.trim();
    if line.starts_with('@') {
        return None;
    }
    match line {
        "PWON" => return Some(TelnetEvent::UnitPower(true)),
        "PWSTANDBY" | "PWOFF" => return Some(TelnetEvent::UnitPower(false)),
        "ZMON" => return Some(TelnetEvent::MainZonePower(true)),
        "ZMOFF" => return Some(TelnetEvent::MainZonePower(false)),
        "MUON" => return Some(TelnetEvent::Mute(true)),
        "MUOFF" => return Some(TelnetEvent::Mute(false)),
        _ => {}
    }
    if let Some(rest) = line.strip_prefix("MVMAX") {
        return mv_to_db(rest.trim()).map(TelnetEvent::VolumeMaxDb);
    }
    if let Some(rest) = line.strip_prefix("MV") {
        return mv_to_db(rest).map(TelnetEvent::VolumeDb);
    }
    if let Some(code) = line.strip_prefix("SI") {
        let code = code.trim();
        return (!code.is_empty() && code != "?").then(|| TelnetEvent::Input(code.to_string()));
    }
    None
}

/// A line with a prefix this adapter models (`MV`, `PW`, `ZM`, `MU`, `SI`) that it could not
/// parse: a protocol variant the implementation does not know, worth reporting.
pub fn is_unparsed_modeled_line(line: &str) -> bool {
    let line = line.trim();
    ["MV", "PW", "ZM", "MU", "SI"]
        .iter()
        .any(|prefix| line.starts_with(prefix))
        && !line.ends_with('?')
        && parse_line(line).is_none()
}

/// Decode an `MV` value: two digits are whole steps, a third digit `5` is the half step.
fn mv_to_db(digits: &str) -> Option<f32> {
    if !(2..=3).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let whole: f32 = digits[..2].parse().ok()?;
    let half = match digits.as_bytes().get(2) {
        None => 0.0,
        Some(b'5') => 0.5,
        Some(b'0') => 0.0,
        Some(_) => return None,
    };
    Some(whole + half - MV_OFFSET_DB)
}

/// Encode a level for `MV`. Refuses anything that is not on the half-dB grid or is outside
/// what the protocol can express — the caller has already clamped, so a refusal here means a
/// bug upstream and must not be "fixed" by sending something nearby.
pub fn encode_volume(db: f32) -> Option<String> {
    let absolute = db + MV_OFFSET_DB;
    if !absolute.is_finite() || !(0.0..=98.0).contains(&absolute) {
        return None;
    }
    let doubled = absolute * 2.0;
    if (doubled - doubled.round()).abs() > 0.001 {
        return None;
    }
    let halves = doubled.round() as u32;
    let whole = halves / 2;
    Some(if halves % 2 == 1 {
        format!("MV{whole:02}5")
    } else {
        format!("MV{whole:02}")
    })
}

/// The Telnet command for an operation, and the query whose answer confirms it.
pub fn telnet_command(operation: &DeviceOperation) -> Option<(String, &'static str)> {
    Some(match operation {
        DeviceOperation::SetVolume(db) => (encode_volume(*db)?, "MV?"),
        DeviceOperation::SetMute(true) => ("MUON".to_string(), "MU?"),
        DeviceOperation::SetMute(false) => ("MUOFF".to_string(), "MU?"),
        // Main zone, not the whole unit: `PWSTANDBY` would also switch off zone 2.
        DeviceOperation::PowerOn => ("ZMON".to_string(), "ZM?"),
        DeviceOperation::PowerStandby => ("ZMOFF".to_string(), "ZM?"),
        DeviceOperation::SelectInput(code) => {
            if code.is_empty() || code.contains(['\r', '\n']) {
                return None;
            }
            (format!("SI{code}"), "SI?")
        }
    })
}

/// The `goform` HTTP command for an operation (main zone). Used when the Telnet session is
/// unavailable — notably power-on from standby, where the captured SR6008 refuses Telnet.
pub fn http_command_path(operation: &DeviceOperation) -> Option<String> {
    Some(match operation {
        DeviceOperation::SetVolume(db) => {
            // Re-validate through the Telnet encoder: same grid, same bounds.
            encode_volume(*db)?;
            format!("/goform/formiPhoneAppVolume.xml?1+{db:.1}")
        }
        DeviceOperation::SetMute(true) => "/goform/formiPhoneAppMute.xml?1+MuteOn".to_string(),
        DeviceOperation::SetMute(false) => "/goform/formiPhoneAppMute.xml?1+MuteOff".to_string(),
        DeviceOperation::PowerOn => "/goform/formiPhoneAppPower.xml?1+PowerOn".to_string(),
        DeviceOperation::PowerStandby => {
            "/goform/formiPhoneAppPower.xml?1+PowerStandby".to_string()
        }
        DeviceOperation::SelectInput(code) => {
            if code.is_empty() {
                return None;
            }
            format!(
                "/goform/formiPhoneAppDirect.xml?SI{}",
                urlencoding::encode(code)
            )
        }
    })
}

/// Path of the main-zone status document.
pub const MAIN_ZONE_STATUS_PATH: &str = "/goform/formMainZone_MainZoneXmlStatus.xml";

/// The `SI` code for an `InputFuncList` function name (denonavr `SOURCE_MAPPING`). Names with no
/// mapping are their own code, upper-cased.
pub fn input_code_for_function(function: &str) -> String {
    let function = function.trim();
    match function {
        "TV AUDIO" => "TV",
        "iPod/USB" => "USB/IPOD",
        "Bluetooth" => "BT",
        "Blu-ray" => "BD",
        "CBL/SAT" => "SAT/CBL",
        "NETWORK" => "NET",
        "Media Player" => "MPLAY",
        "AUX" => "AUX1",
        "Tuner" | "FM" => "TUNER",
        other => return other.to_ascii_uppercase(),
    }
    .to_string()
}

/// What `formMainZone_MainZoneXmlStatus.xml` says.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MainZoneStatus {
    pub power: Option<DevicePower>,
    pub volume_db: Option<f32>,
    pub muted: Option<bool>,
    pub input: Option<String>,
    pub display: Option<String>,
    pub inputs: Vec<DeviceInput>,
}

/// Parse the main-zone status document (captured on both units).
pub fn parse_main_zone_status(xml: &str) -> MainZoneStatus {
    let first = |tag: &str| values(xml, tag).into_iter().next();
    let power = first("Power").and_then(|value| match value.trim() {
        "ON" => Some(DevicePower::On),
        "STANDBY" | "OFF" => Some(DevicePower::Standby),
        _ => None,
    });
    // "--" is the display for minimum / muted-to-zero; it is not a level.
    let volume_db = first("MasterVolume")
        .and_then(|value| value.trim().parse::<f32>().ok())
        .filter(|db| db.is_finite());
    let muted = first("Mute").and_then(|value| match value.trim() {
        "on" | "ON" => Some(true),
        "off" | "OFF" => Some(false),
        _ => None,
    });
    let input = first("InputFuncSelect")
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    let display = first("VolumeDisplay")
        .map(|value| value.trim().to_ascii_lowercase())
        .filter(|value| !value.is_empty());

    let functions = values(xml, "InputFuncList");
    let renames = values(xml, "RenameSource");
    let deleted = values(xml, "SourceDelete");
    let inputs = functions
        .iter()
        .enumerate()
        .map(|(index, function)| {
            let function = function.trim();
            let rename = renames
                .get(index)
                .map(|name| name.trim())
                .filter(|name| !name.is_empty());
            DeviceInput {
                id: input_code_for_function(function),
                name: rename.unwrap_or(function).to_string(),
                available: deleted.get(index).map(String::as_str) != Some("DEL"),
            }
        })
        .collect();

    MainZoneStatus {
        power,
        volume_db,
        muted,
        input,
        display,
        inputs,
    }
}

/// The identity facts from a UPnP device description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub manufacturer: String,
    pub model: String,
    pub friendly_name: String,
    pub serial: Option<String>,
    pub udn: Option<String>,
}

impl Description {
    /// The device kind for a supported manufacturer, or `None` for any other device.
    pub fn kind(&self) -> Option<&'static str> {
        let manufacturer = self.manufacturer.trim().to_ascii_lowercase();
        if manufacturer == "marantz" {
            Some("marantz")
        } else if manufacturer == "denon" || manufacturer == "denon professional" {
            Some("denon")
        } else {
            None
        }
    }

    /// The hardware id: the serial number, or the MAC-bearing tail of the UDN when a model
    /// omits the serial. Never the friendly name or model.
    pub fn hardware_id(&self) -> Option<String> {
        if let Some(serial) = self.serial.as_deref().map(str::trim) {
            if !serial.is_empty() {
                return Some(serial.to_string());
            }
        }
        let udn = self.udn.as_deref()?;
        let tail = udn.rsplit('-').next()?;
        (tail.len() == 12 && tail.bytes().all(|b| b.is_ascii_hexdigit()))
            .then(|| tail.to_ascii_uppercase())
    }
}

/// Parse a UPnP device description. Returns `None` when it carries no manufacturer.
pub fn parse_description(xml: &str) -> Option<Description> {
    let text = |tag: &str| element_text(xml, tag).map(|value| value.trim().to_string());
    let manufacturer = text("manufacturer")?;
    Some(Description {
        manufacturer,
        // Denon/Marantz prefix the model with '*' ("*SR6008").
        model: text("modelName")
            .map(|model| model.trim_start_matches('*').to_string())
            .unwrap_or_default(),
        friendly_name: text("friendlyName").unwrap_or_default(),
        serial: text("serialNumber"),
        udn: text("UDN"),
    })
}

/// Text of the first `<tag>…</tag>` element.
fn element_text<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&close)? + start;
    Some(&xml[start..end])
}

/// The innermost `<value>` texts inside the first `<tag>…</tag>` block, in order. Handles the
/// nested `<value><value>CD</value></value>` shape `RenameSource` uses.
fn values(xml: &str, tag: &str) -> Vec<String> {
    let Some(block) = element_text(xml, tag) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut rest = block;
    while let Some(start) = rest.find("<value>") {
        let after = &rest[start + "<value>".len()..];
        let Some(end) = after.find("</value>") else {
            break;
        };
        let content = &after[..end];
        if content.contains('<') {
            // Nested: descend into the inner value.
            rest = after;
            continue;
        }
        out.push(content.to_string());
        rest = &after[end + "</value>".len()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRONT_STATUS: &str = include_str!(
        "../../../tests/fixtures/devices/marantz/sr6008/0006781E0AB8/main_zone_status.xml"
    );
    const BEDROOM_STATUS: &str = include_str!(
        "../../../tests/fixtures/devices/marantz/sr6008/0006781E2AB9/main_zone_status.xml"
    );
    const FRONT_DESCRIPTION: &str =
        include_str!("../../../tests/fixtures/devices/marantz/sr6008/0006781E0AB8/description.xml");
    const BEDROOM_DESCRIPTION: &str =
        include_str!("../../../tests/fixtures/devices/marantz/sr6008/0006781E2AB9/description.xml");
    const FRONT_TELNET_RX: &[u8] = include_bytes!(
        "../../../tests/fixtures/devices/marantz/sr6008/0006781E0AB8/telnet_state_queries.rx"
    );

    #[test]
    fn captured_telnet_replies_parse_into_state_and_ignore_the_at_protocol() {
        let mut buffer = String::from_utf8_lossy(FRONT_TELNET_RX).to_string();
        let events: Vec<TelnetEvent> = drain_lines(&mut buffer)
            .iter()
            .filter_map(|line| parse_line(line))
            .collect();
        assert!(buffer.is_empty());
        assert_eq!(
            events,
            vec![
                TelnetEvent::UnitPower(true),
                TelnetEvent::MainZonePower(true),
                TelnetEvent::VolumeDb(-39.5),
                TelnetEvent::VolumeMaxDb(-10.0),
                TelnetEvent::Mute(false),
                TelnetEvent::Input("MPLAY".to_string()),
            ]
        );
    }

    #[test]
    fn partial_lines_wait_for_their_terminator() {
        let mut buffer = String::from("MV40");
        assert!(drain_lines(&mut buffer).is_empty());
        buffer.push_str("5\rMU");
        assert_eq!(drain_lines(&mut buffer), vec!["MV405".to_string()]);
        assert_eq!(buffer, "MU");
    }

    #[test]
    fn only_malformed_modeled_lines_count_as_unparsed() {
        assert!(is_unparsed_modeled_line("MVX12"));
        assert!(is_unparsed_modeled_line("PWMAYBE"));
        assert!(!is_unparsed_modeled_line("MV405"));
        assert!(!is_unparsed_modeled_line("MVMAX 70"));
        assert!(!is_unparsed_modeled_line("Z2ON"));
        assert!(!is_unparsed_modeled_line("MSDIRECT"));
        assert!(!is_unparsed_modeled_line("@VOL:-395"));
        assert!(!is_unparsed_modeled_line("MV?"));
    }

    #[test]
    fn zone2_and_unmodelled_lines_are_ignored_not_misattributed() {
        for line in [
            "Z2ON",
            "Z250",
            "Z2MUON",
            "Z3SICD",
            "MSDIRECT",
            "PSMDAX OFF",
            "SVOFF",
            "@VOL:-395",
        ] {
            assert_eq!(parse_line(line), None, "{line}");
        }
    }

    /// CRITICAL: the encoder is the only place a dB level becomes wire bytes. `MV` is dB + 80;
    /// sending a dB value as if it were the absolute scale (or the reverse) is a 40-80 dB error.
    #[test]
    fn critical_volume_encoding_round_trips_the_captured_value() {
        assert_eq!(encode_volume(-39.5).as_deref(), Some("MV405"));
        assert_eq!(parse_line("MV405"), Some(TelnetEvent::VolumeDb(-39.5)));
        assert_eq!(encode_volume(-40.0).as_deref(), Some("MV40"));
        assert_eq!(encode_volume(-80.0).as_deref(), Some("MV00"));
        assert_eq!(encode_volume(-74.5).as_deref(), Some("MV055"));
        assert_eq!(parse_line("MV055"), Some(TelnetEvent::VolumeDb(-74.5)));
        assert_eq!(encode_volume(-10.0).as_deref(), Some("MV70"));
        assert_eq!(encode_volume(18.0).as_deref(), Some("MV98"));
    }

    #[test]
    fn critical_off_grid_and_out_of_range_levels_are_never_encoded() {
        assert_eq!(encode_volume(-39.7), None);
        assert_eq!(encode_volume(-80.5), None);
        assert_eq!(encode_volume(18.5), None);
        assert_eq!(encode_volume(f32::NAN), None);
        assert_eq!(http_command_path(&DeviceOperation::SetVolume(-39.7)), None);
    }

    #[test]
    fn commands_carry_their_confirming_query() {
        assert_eq!(
            telnet_command(&DeviceOperation::SetVolume(-30.0)),
            Some(("MV50".to_string(), "MV?"))
        );
        assert_eq!(
            telnet_command(&DeviceOperation::PowerOn),
            Some(("ZMON".to_string(), "ZM?"))
        );
        assert_eq!(
            telnet_command(&DeviceOperation::SelectInput("MPLAY".to_string())),
            Some(("SIMPLAY".to_string(), "SI?"))
        );
        assert_eq!(
            telnet_command(&DeviceOperation::SelectInput("CD\rPWSTANDBY".to_string())),
            None
        );
        assert_eq!(
            http_command_path(&DeviceOperation::SelectInput("SAT/CBL".to_string())).as_deref(),
            Some("/goform/formiPhoneAppDirect.xml?SISAT%2FCBL")
        );
        assert_eq!(
            http_command_path(&DeviceOperation::SetVolume(-39.5)).as_deref(),
            Some("/goform/formiPhoneAppVolume.xml?1+-39.5")
        );
    }

    #[test]
    fn captured_status_yields_level_input_and_renamed_catalog() {
        let status = parse_main_zone_status(FRONT_STATUS);
        assert_eq!(status.power, Some(DevicePower::On));
        assert_eq!(status.volume_db, Some(-39.5));
        assert_eq!(status.muted, Some(false));
        assert_eq!(status.input.as_deref(), Some("MPLAY"));
        assert_eq!(status.display.as_deref(), Some("absolute"));
        assert_eq!(status.inputs.len(), 13);
        let media_player = status.inputs.iter().find(|i| i.id == "MPLAY").unwrap();
        assert_eq!(media_player.name, "WiiM");
        assert!(media_player.available);
        let tv = status.inputs.iter().find(|i| i.id == "TV").unwrap();
        assert_eq!(tv.name, "TV AUDIO");
        assert!(tv.available);
        let cd = status.inputs.iter().find(|i| i.id == "CD").unwrap();
        assert!(!cd.available, "CD is source-deleted on this unit");
    }

    #[test]
    fn captured_standby_status_reports_standby_with_blank_names() {
        let status = parse_main_zone_status(BEDROOM_STATUS);
        assert_eq!(status.power, Some(DevicePower::Standby));
        assert_eq!(status.volume_db, Some(-59.0));
        // The status document keeps reporting the last-selected input in standby (`Power` reads
        // `OFF` there, where the main-zone document says `STANDBY`).
        assert_eq!(status.input.as_deref(), Some("MPLAY"));
        // Renames are blank in standby; names fall back to the function name.
        assert!(status.inputs.iter().all(|input| !input.name.is_empty()));
    }

    #[test]
    fn captured_descriptions_give_two_identities_for_two_identical_models() {
        let front = parse_description(FRONT_DESCRIPTION).unwrap();
        let bedroom = parse_description(BEDROOM_DESCRIPTION).unwrap();
        assert_eq!(front.kind(), Some("marantz"));
        assert_eq!(front.model, "SR6008");
        assert_eq!(bedroom.model, "SR6008");
        assert_eq!(front.hardware_id().as_deref(), Some("0006781E0AB8"));
        assert_eq!(bedroom.hardware_id().as_deref(), Some("0006781E2AB9"));
        assert_ne!(front.hardware_id(), bedroom.hardware_id());
        assert_eq!(front.friendly_name, "front");
    }

    #[test]
    fn udn_tail_is_the_fallback_identity() {
        let description = Description {
            manufacturer: "Denon".to_string(),
            model: "AVR".to_string(),
            friendly_name: "x".to_string(),
            serial: None,
            udn: Some("uuid:5f9ec1b3-ff59-19bb-8530-0006781e0ab8".to_string()),
        };
        assert_eq!(description.kind(), Some("denon"));
        assert_eq!(description.hardware_id().as_deref(), Some("0006781E0AB8"));
    }

    #[test]
    fn other_manufacturers_are_not_claimed() {
        let description = Description {
            manufacturer: "Signalyst".to_string(),
            model: "HQPlayer".to_string(),
            friendly_name: String::new(),
            serial: None,
            udn: None,
        };
        assert_eq!(description.kind(), None);
    }

    #[test]
    fn source_mapping_matches_the_captured_select_code() {
        assert_eq!(input_code_for_function("Media Player"), "MPLAY");
        assert_eq!(input_code_for_function("TV AUDIO"), "TV");
        assert_eq!(input_code_for_function("CBL/SAT"), "SAT/CBL");
        assert_eq!(input_code_for_function("M-XPORT"), "M-XPORT");
    }
}
