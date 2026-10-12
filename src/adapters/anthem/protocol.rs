//! Anthem MRX / AVM wire formats: the semicolon-framed ASCII protocol on TCP 14999. Pure
//! functions only.
//!
//! Sources:
//!
//! - `anthemav` 1.4.x (MIT), <https://github.com/nugget/python-anthemav>:
//!   `anthemav/protocol.py` (`LOOKUP` / `ZONELOOKUP` command tables, `set_model_command`,
//!   `_populate_inputs`, `_parse_message`, `Zone` volume/power/mute/input shapes, the error
//!   prefixes `!I` `!R` `!E` `!Z`), `anthemav/connection.py` (port 14999) and
//!   `tests/test_protocol.py` (`IDMMRX 740`, `Z1VOL-42`, `Z1PVOL53`, `Z1INP3`, `ISN01`, `IS1IN`).
//! - Anthem's own protocol sheets, linked from that README: MRX-x20 / AVM-60 and
//!   MRX-x40 / AVM-70 / AVM-90 (IP / RS-232 .xls). They were not available offline; every
//!   command below is the one the library sends, so the library is the evidence.
//!
//! # x20 versus x40
//!
//! The two generations differ in exactly the places that matter here, and the library branches
//! on the reported model (`set_model_command`: a model containing `40`, `70` or `90` is x40):
//!
//! | | x20 (MRX 520/720/1120, AVM 60) | x40 (MRX 540/740/1140, AVM 70/90) |
//! |---|---|---|
//! | volume | `Z1VOL-35` whole dB, -90..0 | `Z1PVOL53` percent, 0..100 |
//! | input names | `ISN01?` -> `ISN01Name` | `IS1IN?` -> `IS1INName` |
//! | MAC | `IDN?` | `EMAC?` (wired), `WMAC?` |
//! | feedback ("TX status") | `ECH` | `GCTXS` |
//!
//! The MDX / MDA multi-zone distribution amplifiers also use this port but a different command
//! set and zone model, and the catalog scopes this adapter to Zone 1 of the MRX / AVM family, so
//! they are refused.
//!
//! No Anthem hardware has been available: every claim is implementation-derived.

use std::collections::BTreeMap;

use crate::devices::service::DeviceOperation;

/// Control port (`connection.py`: `port: int = 14999`).
pub const DEFAULT_PORT: u16 = 14999;

/// Lowest and highest x20 level, in dB (`Zone.attenuation`: "Valid range from -90 to 0").
pub const MIN_DB: f32 = -90.0;
pub const MAX_DB: f32 = 0.0;
/// x40 level range, in percent (`Zone.volume`: "Valid range from 0 to 100").
pub const MIN_PERCENT: f32 = 0.0;
pub const MAX_PERCENT: f32 = 100.0;

/// The command generation of a unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Series {
    X20,
    X40,
}

/// Classify a reported model. `None` for an MDX / MDA distribution amplifier or an empty or
/// placeholder model: no command set is assumed for what cannot be classified.
///
/// The x40 test is the library's own (`set_model_command`): the model contains `40`, `70` or `90`.
pub fn series_for_model(model: &str) -> Option<Series> {
    let model = model.trim();
    let upper = model.to_ascii_uppercase();
    if model.is_empty()
        || upper == "UNKNOWN MODEL"
        || upper.contains("MDX")
        || upper.contains("MDA")
    {
        return None;
    }
    if model.contains("40") || model.contains("70") || model.contains("90") {
        Some(Series::X40)
    } else {
        Some(Series::X20)
    }
}

/// One state fact from a reply line (Zone 1 only).
#[derive(Debug, Clone, PartialEq)]
pub enum AnthemEvent {
    /// `IDMMRX 740`.
    Model(String),
    /// `IDN`, `EMAC`, `WMAC` or `MAC` reply: a hardware address, exactly as sent.
    Mac { kind: MacKind, value: String },
    /// `ICN8`: the active input count.
    InputCount(usize),
    /// `ISN01Turntable` (x20) or `IS1INTurntable` (x40).
    InputName { number: u16, name: String },
    /// `Z1POW1`.
    Power(bool),
    /// `Z1VOL-35`: dB.
    VolumeDb(i32),
    /// `Z1PVOL53`: percent.
    VolumePercent(u8),
    /// `Z1MUT1`.
    Mute(bool),
    /// `Z1INP3`.
    Input(u16),
    /// `ECH1` / `GCTXS1`: whether the unit transmits status.
    TxStatus(bool),
    /// `!I` / `!R` / `!E` / `!Z`: the unit refused a command.
    Rejected(Rejection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MacKind {
    /// x20 `IDN`.
    Idn,
    /// x40 wired `EMAC`.
    Wired,
    /// x40 Wi-Fi `WMAC`.
    Wifi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rejection {
    /// `!I`: invalid command.
    Invalid,
    /// `!R`: out-of-range value.
    OutOfRange,
    /// `!E`: recognised but cannot be executed now.
    CannotExecute,
    /// `!Z`: the zone is off.
    ZoneOff,
}

/// Split a receive buffer into complete `;`-terminated messages, leaving any partial message in
/// place. Empty messages (the unit sometimes answers a command with a bare `;`) are dropped: the
/// reference client re-parses its *last command* on them, which assumes the command took
/// effect — this adapter reads the state back instead.
pub fn drain_messages(buffer: &mut String) -> Vec<String> {
    let mut messages = Vec::new();
    while let Some(end) = buffer.find(';') {
        let message: String = buffer.drain(..=end).collect();
        let message = message.trim_end_matches(';').trim();
        if !message.is_empty() {
            messages.push(message.to_string());
        }
    }
    messages
}

/// Parse one message. Zone 2+ (`Z2…`), listening modes, video and every other attribute this
/// adapter does not model return `None`.
pub fn parse_message(message: &str) -> Option<AnthemEvent> {
    let message = message.trim();
    // `_parse_message`: error prefixes first.
    for (prefix, kind) in [
        ("!I", Rejection::Invalid),
        ("!R", Rejection::OutOfRange),
        ("!E", Rejection::CannotExecute),
        ("!Z", Rejection::ZoneOff),
    ] {
        if message.starts_with(prefix) {
            return Some(AnthemEvent::Rejected(kind));
        }
    }
    if let Some(rest) = message.strip_prefix("IDM") {
        let model = rest.trim();
        return (!model.is_empty()).then(|| AnthemEvent::Model(model.to_string()));
    }
    for (prefix, kind) in [
        ("EMAC", MacKind::Wired),
        ("WMAC", MacKind::Wifi),
        ("IDN", MacKind::Idn),
    ] {
        if let Some(rest) = message.strip_prefix(prefix) {
            let value = rest.trim();
            return (!value.is_empty()).then(|| AnthemEvent::Mac {
                kind,
                value: value.to_string(),
            });
        }
    }
    if let Some(rest) = message.strip_prefix("ICN") {
        return rest.trim().parse().ok().map(AnthemEvent::InputCount);
    }
    if let Some(rest) = message.strip_prefix("ECH") {
        return tx_status(rest);
    }
    if let Some(rest) = message.strip_prefix("GCTXS") {
        return tx_status(rest);
    }
    if let Some(rest) = message.strip_prefix("ISN") {
        // x20: `ISN01Turntable` — two-digit number, then the name (`len(data) > 5`).
        let (number, name) = (rest.get(..2)?, rest.get(2..)?);
        let number = number.parse().ok()?;
        let name = name.trim();
        return (!name.is_empty()).then(|| AnthemEvent::InputName {
            number,
            name: name.to_string(),
        });
    }
    if let Some(rest) = message.strip_prefix("IS") {
        // x40: `IS3INTurntable` — number, `IN`, name. (`IS3ARC1` is the ARC flag: not an input name.)
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return None;
        }
        let number = rest[..digits].parse().ok()?;
        let name = rest[digits..].strip_prefix("IN")?.trim();
        return (!name.is_empty()).then(|| AnthemEvent::InputName {
            number,
            name: name.to_string(),
        });
    }
    let zone = message.strip_prefix("Z1")?;
    if let Some(value) = zone.strip_prefix("POW") {
        return match value.trim() {
            "1" => Some(AnthemEvent::Power(true)),
            "0" => Some(AnthemEvent::Power(false)),
            _ => None,
        };
    }
    if let Some(value) = zone.strip_prefix("PVOL") {
        let percent: u8 = value.trim().parse().ok()?;
        return (f32::from(percent) <= MAX_PERCENT).then_some(AnthemEvent::VolumePercent(percent));
    }
    if let Some(value) = zone.strip_prefix("VOL") {
        let db: i32 = value.trim().parse().ok()?;
        return ((-90..=0).contains(&db)).then_some(AnthemEvent::VolumeDb(db));
    }
    if let Some(value) = zone.strip_prefix("MUT") {
        return match value.trim() {
            "1" => Some(AnthemEvent::Mute(true)),
            "0" => Some(AnthemEvent::Mute(false)),
            _ => None,
        };
    }
    if let Some(value) = zone.strip_prefix("INP") {
        return value.trim().parse().ok().map(AnthemEvent::Input);
    }
    None
}

fn tx_status(value: &str) -> Option<AnthemEvent> {
    match value.trim() {
        "0" => Some(AnthemEvent::TxStatus(false)),
        // `GCTXS` is 0 off, 1 IP on, 2 IP and RS-232 on; `ECH` is 0/1.
        "1" | "2" => Some(AnthemEvent::TxStatus(true)),
        _ => None,
    }
}

/// A usable hardware address: at least twelve hex digits once separators are removed, and not
/// all zero (`EMPTY_MAC = "00:00:00:00:00:00"` is what an unconfigured unit reports).
pub fn valid_mac(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(char::is_ascii_alphanumeric).collect();
    let usable = digits.len() >= 12
        && digits.bytes().all(|b| b.is_ascii_hexdigit())
        && digits.bytes().any(|b| b != b'0');
    usable.then_some(digits.to_ascii_uppercase())
}

/// Pick the hardware address for a unit: the wired MAC on x40, the `IDN` MAC on x20; the Wi-Fi
/// MAC only when nothing else is usable.
pub fn preferred_mac(macs: &[(MacKind, String)]) -> Option<String> {
    [MacKind::Wired, MacKind::Idn, MacKind::Wifi]
        .iter()
        .find_map(|wanted| {
            macs.iter()
                .filter(|(kind, _)| kind == wanted)
                .find_map(|(_, value)| valid_mac(value))
        })
}

/// Encode an x20 level for `Z1VOL`: whole dB only (the library only ever sends integers), -90..0.
pub fn encode_db(db: f32) -> Option<i32> {
    if !db.is_finite() || !(MIN_DB..=MAX_DB).contains(&db) || (db - db.round()).abs() > 0.001 {
        return None;
    }
    Some(db.round() as i32)
}

/// Encode an x40 level for `Z1PVOL`: whole percent, 0..100.
pub fn encode_percent(level: f32) -> Option<u8> {
    if !level.is_finite()
        || !(MIN_PERCENT..=MAX_PERCENT).contains(&level)
        || (level - level.round()).abs() > 0.001
    {
        return None;
    }
    Some(level.round() as u8)
}

/// The command (with its `;`) for an operation, and the query whose answer confirms it.
/// Zone 1 only.
pub fn command(series: Series, operation: &DeviceOperation) -> Option<(String, &'static str)> {
    Some(match operation {
        DeviceOperation::SetVolume(level) => match series {
            Series::X20 => (format!("Z1VOL{};", encode_db(*level)?), "Z1VOL?;"),
            Series::X40 => (format!("Z1PVOL{};", encode_percent(*level)?), "Z1PVOL?;"),
        },
        DeviceOperation::SetMute(true) => ("Z1MUT1;".to_string(), "Z1MUT?;"),
        DeviceOperation::SetMute(false) => ("Z1MUT0;".to_string(), "Z1MUT?;"),
        DeviceOperation::PowerOn => ("Z1POW1;".to_string(), "Z1POW?;"),
        DeviceOperation::PowerStandby => ("Z1POW0;".to_string(), "Z1POW?;"),
        DeviceOperation::SelectInput(code) => {
            let number: u16 = code.trim().parse().ok()?;
            // `Zone.input_number`: `1 <= number <= 99`.
            if !(1..=99).contains(&number) {
                return None;
            }
            (format!("Z1INP{number};"), "Z1INP?;")
        }
    })
}

/// State queries sent on connect and by the poll. Queries only: **no level is ever sent on
/// connect**. Zone 1 power, level, mute and input (`ZONELOOKUP`).
pub fn state_queries(series: Series) -> &'static [&'static str] {
    match series {
        Series::X20 => &["Z1POW?;", "Z1VOL?;", "Z1MUT?;", "Z1INP?;"],
        Series::X40 => &["Z1POW?;", "Z1PVOL?;", "Z1MUT?;", "Z1INP?;"],
    }
}

/// Identity queries once the model is known: `IDN` on x20, `EMAC`/`WMAC` on x40
/// (`set_model_command`).
pub fn identity_queries(series: Series) -> &'static [&'static str] {
    match series {
        Series::X20 => &["IDN?;"],
        Series::X40 => &["EMAC?;", "WMAC?;"],
    }
}

/// The query for the unit's transmit status: `ECH` (x20) or `GCTXS` (x40).
pub fn tx_status_query(series: Series) -> &'static str {
    match series {
        Series::X20 => "ECH?;",
        Series::X40 => "GCTXS?;",
    }
}

/// Queries for the names of inputs `1..=count` (`_populate_inputs`).
pub fn input_name_queries(series: Series, count: usize) -> Vec<String> {
    (1..=count.min(99))
        .map(|number| match series {
            Series::X20 => format!("ISN{number:02}?;"),
            Series::X40 => format!("IS{number}IN?;"),
        })
        .collect()
}

pub const MODEL_QUERY: &str = "IDM?;";
pub const INPUT_COUNT_QUERY: &str = "ICN?;";

/// An input catalog keyed by input number. Inputs the unit gave no name are absent.
pub type InputCatalog = BTreeMap<u16, String>;

#[cfg(test)]
mod tests {
    use super::*;

    /// `tests/test_protocol.py`: `IDMMRX 520`, `IDMMRX 740`, `IDMMRX 1120`, `IDMMDX16`.
    #[test]
    fn series_follow_the_reference_clients_model_rules() {
        assert_eq!(series_for_model("MRX 520"), Some(Series::X20));
        assert_eq!(series_for_model("MRX 720"), Some(Series::X20));
        assert_eq!(series_for_model("MRX 1120"), Some(Series::X20));
        assert_eq!(series_for_model("AVM 60"), Some(Series::X20));
        assert_eq!(series_for_model("MRX 540"), Some(Series::X40));
        assert_eq!(series_for_model("MRX 740"), Some(Series::X40));
        assert_eq!(series_for_model("MRX 1140"), Some(Series::X40));
        assert_eq!(series_for_model("AVM 70"), Some(Series::X40));
        assert_eq!(series_for_model("AVM 90"), Some(Series::X40));
        // Distribution amplifiers: a different zone model, out of scope.
        assert_eq!(series_for_model("MDX-16"), None);
        assert_eq!(series_for_model("MDX16"), None);
        assert_eq!(series_for_model("MDA-8"), None);
        assert_eq!(series_for_model(""), None);
        assert_eq!(series_for_model("Unknown Model"), None);
    }

    /// `tests/test_protocol.py` message examples, and the `ZONELOOKUP` / `LOOKUP` tables.
    #[test]
    fn zone_one_messages_parse() {
        assert_eq!(
            parse_message("IDMMRX 740"),
            Some(AnthemEvent::Model("MRX 740".to_string()))
        );
        assert_eq!(parse_message("Z1POW1"), Some(AnthemEvent::Power(true)));
        assert_eq!(parse_message("Z1POW0"), Some(AnthemEvent::Power(false)));
        assert_eq!(parse_message("Z1VOL-42"), Some(AnthemEvent::VolumeDb(-42)));
        assert_eq!(parse_message("Z1VOL-90"), Some(AnthemEvent::VolumeDb(-90)));
        assert_eq!(parse_message("Z1VOL0"), Some(AnthemEvent::VolumeDb(0)));
        assert_eq!(
            parse_message("Z1PVOL53"),
            Some(AnthemEvent::VolumePercent(53))
        );
        assert_eq!(
            parse_message("Z1PVOL100"),
            Some(AnthemEvent::VolumePercent(100))
        );
        assert_eq!(parse_message("Z1MUT1"), Some(AnthemEvent::Mute(true)));
        assert_eq!(parse_message("Z1MUT0"), Some(AnthemEvent::Mute(false)));
        assert_eq!(parse_message("Z1INP3"), Some(AnthemEvent::Input(3)));
        assert_eq!(parse_message("ICN8"), Some(AnthemEvent::InputCount(8)));
    }

    /// `tests/test_protocol.py` (`Z2PVOL51`, `Z2INP`) and `ZONELOOKUP`: other zones are not the device.
    #[test]
    fn other_zones_and_unmodelled_attributes_are_ignored() {
        assert_eq!(parse_message("Z2PVOL51"), None);
        assert_eq!(parse_message("Z2POW1"), None);
        assert_eq!(parse_message("Z2INP4"), None);
        assert_eq!(parse_message("Z1ALM02"), None);
        assert_eq!(parse_message("Z1VIR14"), None);
        assert_eq!(parse_message("IDS1.2"), None);
        assert_eq!(parse_message(""), None);
    }

    #[test]
    fn malformed_and_out_of_range_values_are_refused() {
        assert_eq!(parse_message("Z1VOL-91"), None);
        assert_eq!(parse_message("Z1VOL5"), None);
        assert_eq!(parse_message("Z1VOLabc"), None);
        assert_eq!(parse_message("Z1PVOL101"), None);
        assert_eq!(parse_message("Z1PVOL-1"), None);
        assert_eq!(parse_message("Z1POW2"), None);
        assert_eq!(parse_message("Z1MUT"), None);
        assert_eq!(parse_message("Z1INP"), None);
        assert_eq!(parse_message("IDM"), None);
        assert_eq!(parse_message("ICNx"), None);
    }

    /// `_parse_message`: `!I` invalid, `!R` out of range, `!E` cannot execute, `!Z` zone off.
    #[test]
    fn error_prefixes_are_rejections() {
        assert_eq!(
            parse_message("!IZ1VOL"),
            Some(AnthemEvent::Rejected(Rejection::Invalid))
        );
        assert_eq!(
            parse_message("!RZ1VOL-95"),
            Some(AnthemEvent::Rejected(Rejection::OutOfRange))
        );
        assert_eq!(
            parse_message("!EZ1POW1"),
            Some(AnthemEvent::Rejected(Rejection::CannotExecute))
        );
        assert_eq!(
            parse_message("!ZZ1VOL-30"),
            Some(AnthemEvent::Rejected(Rejection::ZoneOff))
        );
    }

    /// `_parse_message` comments: `ISN01Turntable` (x20) and `IS3INTurntable` (x40).
    #[test]
    fn input_names_parse_per_generation() {
        assert_eq!(
            parse_message("ISN01Turntable"),
            Some(AnthemEvent::InputName {
                number: 1,
                name: "Turntable".to_string()
            })
        );
        assert_eq!(
            parse_message("ISN12HDMI 4"),
            Some(AnthemEvent::InputName {
                number: 12,
                name: "HDMI 4".to_string()
            })
        );
        assert_eq!(
            parse_message("IS3INTurntable"),
            Some(AnthemEvent::InputName {
                number: 3,
                name: "Turntable".to_string()
            })
        );
        assert_eq!(
            parse_message("IS12INTV"),
            Some(AnthemEvent::InputName {
                number: 12,
                name: "TV".to_string()
            })
        );
        // An unnamed input and the x40 ARC flag are not names.
        assert_eq!(parse_message("ISN01"), None);
        assert_eq!(parse_message("IS3IN"), None);
        assert_eq!(parse_message("IS3ARC1"), None);
        assert_eq!(parse_message("ISNxxName"), None);
    }

    #[test]
    fn mac_and_tx_status_replies_parse() {
        assert_eq!(
            parse_message("IDN00:11:22:33:44:55"),
            Some(AnthemEvent::Mac {
                kind: MacKind::Idn,
                value: "00:11:22:33:44:55".to_string()
            })
        );
        assert_eq!(
            parse_message("EMAC001122334455"),
            Some(AnthemEvent::Mac {
                kind: MacKind::Wired,
                value: "001122334455".to_string()
            })
        );
        assert_eq!(
            parse_message("WMAC001122334466"),
            Some(AnthemEvent::Mac {
                kind: MacKind::Wifi,
                value: "001122334466".to_string()
            })
        );
        assert_eq!(parse_message("ECH1"), Some(AnthemEvent::TxStatus(true)));
        assert_eq!(parse_message("ECH0"), Some(AnthemEvent::TxStatus(false)));
        assert_eq!(parse_message("GCTXS0"), Some(AnthemEvent::TxStatus(false)));
        assert_eq!(parse_message("GCTXS2"), Some(AnthemEvent::TxStatus(true)));
    }

    /// `EMPTY_MAC`: an unconfigured unit reports all zeros, which is not an identity.
    #[test]
    fn hardware_addresses_must_be_usable() {
        assert_eq!(
            valid_mac("00:11:22:33:44:55").as_deref(),
            Some("001122334455")
        );
        assert_eq!(valid_mac("aabbccddeeff").as_deref(), Some("AABBCCDDEEFF"));
        assert_eq!(valid_mac("00:00:00:00:00:00"), None);
        assert_eq!(valid_mac("0011"), None);
        assert_eq!(valid_mac("zz:zz:zz:zz:zz:zz"), None);
        assert_eq!(valid_mac(""), None);
        let macs = vec![
            (MacKind::Wifi, "00:11:22:33:44:66".to_string()),
            (MacKind::Wired, "00:00:00:00:00:00".to_string()),
        ];
        assert_eq!(
            preferred_mac(&macs).as_deref(),
            Some("001122334466"),
            "wired is unusable"
        );
        let macs = vec![
            (MacKind::Wifi, "00:11:22:33:44:66".to_string()),
            (MacKind::Wired, "00:11:22:33:44:55".to_string()),
        ];
        assert_eq!(preferred_mac(&macs).as_deref(), Some("001122334455"));
        assert_eq!(preferred_mac(&[]), None);
    }

    #[test]
    fn messages_split_on_semicolons_and_drop_bare_acks() {
        let mut buffer = "Z1POW1;Z1VOL-35;;Z1MU".to_string();
        assert_eq!(drain_messages(&mut buffer), vec!["Z1POW1", "Z1VOL-35"]);
        assert_eq!(buffer, "Z1MU");
        buffer.push_str("T0;");
        assert_eq!(drain_messages(&mut buffer), vec!["Z1MUT0"]);
        assert!(buffer.is_empty());
    }

    /// `Zone.attenuation`: -90 to 0, whole dB.
    #[test]
    fn x20_volume_encodes_at_min_max_and_step() {
        let set = |db: f32| command(Series::X20, &DeviceOperation::SetVolume(db));
        assert_eq!(set(-35.0), Some(("Z1VOL-35;".to_string(), "Z1VOL?;")));
        assert_eq!(set(-90.0), Some(("Z1VOL-90;".to_string(), "Z1VOL?;")));
        assert_eq!(set(0.0), Some(("Z1VOL0;".to_string(), "Z1VOL?;")));
        assert_eq!(set(-1.0), Some(("Z1VOL-1;".to_string(), "Z1VOL?;")));
        // Off the whole-dB grid or out of range: refused, not nudged.
        assert_eq!(set(-35.5), None);
        assert_eq!(set(-90.5), None);
        assert_eq!(set(0.5), None);
        assert_eq!(set(f32::NAN), None);
    }

    /// `Zone.volume`: x40 sends `PVOL{value}` for 0..=100.
    #[test]
    fn x40_volume_encodes_percent_at_min_max_and_step() {
        let set = |level: f32| command(Series::X40, &DeviceOperation::SetVolume(level));
        assert_eq!(set(53.0), Some(("Z1PVOL53;".to_string(), "Z1PVOL?;")));
        assert_eq!(set(0.0), Some(("Z1PVOL0;".to_string(), "Z1PVOL?;")));
        assert_eq!(set(100.0), Some(("Z1PVOL100;".to_string(), "Z1PVOL?;")));
        assert_eq!(set(101.0), None);
        assert_eq!(set(-1.0), None);
        assert_eq!(set(50.5), None);
        // A dB level is never silently reinterpreted as a percentage on an x40.
        assert_eq!(set(-35.0), None);
    }

    /// `Zone.power` / `Zone.mute` / `Zone.input_number`.
    #[test]
    fn power_mute_and_input_commands() {
        for series in [Series::X20, Series::X40] {
            assert_eq!(
                command(series, &DeviceOperation::PowerOn),
                Some(("Z1POW1;".to_string(), "Z1POW?;"))
            );
            assert_eq!(
                command(series, &DeviceOperation::PowerStandby),
                Some(("Z1POW0;".to_string(), "Z1POW?;"))
            );
            assert_eq!(
                command(series, &DeviceOperation::SetMute(true)),
                Some(("Z1MUT1;".to_string(), "Z1MUT?;"))
            );
            assert_eq!(
                command(series, &DeviceOperation::SetMute(false)),
                Some(("Z1MUT0;".to_string(), "Z1MUT?;"))
            );
            assert_eq!(
                command(series, &DeviceOperation::SelectInput("3".to_string())),
                Some(("Z1INP3;".to_string(), "Z1INP?;"))
            );
            assert_eq!(
                command(series, &DeviceOperation::SelectInput("0".to_string())),
                None
            );
            assert_eq!(
                command(series, &DeviceOperation::SelectInput("100".to_string())),
                None
            );
            assert_eq!(
                command(series, &DeviceOperation::SelectInput("HDMI".to_string())),
                None
            );
            assert_eq!(
                command(
                    series,
                    &DeviceOperation::SelectInput("3;Z1VOL0".to_string())
                ),
                None,
                "an input code cannot smuggle a second command"
            );
        }
    }

    #[test]
    fn queries_carry_no_value_and_differ_by_generation() {
        for series in [Series::X20, Series::X40] {
            for query in state_queries(series)
                .iter()
                .chain(identity_queries(series))
                .chain([MODEL_QUERY, INPUT_COUNT_QUERY, tx_status_query(series)].iter())
            {
                assert!(query.ends_with("?;") && !query.contains('-'), "{query}");
            }
        }
        assert!(state_queries(Series::X40).contains(&"Z1PVOL?;"));
        assert!(!state_queries(Series::X20).contains(&"Z1PVOL?;"));
        assert_eq!(identity_queries(Series::X20), &["IDN?;"]);
        assert_eq!(identity_queries(Series::X40), &["EMAC?;", "WMAC?;"]);
        assert_eq!(
            input_name_queries(Series::X20, 2),
            vec!["ISN01?;", "ISN02?;"]
        );
        assert_eq!(
            input_name_queries(Series::X40, 2),
            vec!["IS1IN?;", "IS2IN?;"]
        );
        assert!(input_name_queries(Series::X40, 0).is_empty());
        assert_eq!(input_name_queries(Series::X20, 5000).len(), 99, "bounded");
    }
}
