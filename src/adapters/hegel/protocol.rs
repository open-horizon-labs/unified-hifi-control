//! Hegel amplifier wire formats: the ASCII command protocol on TCP 50001 and the UPnP device
//! description used to identify a unit. Pure functions only.
//!
//! Sources (all cited per item below):
//!
//! - `hegel-ip-client` 0.1.4 (MIT), <https://github.com/boazca/hegel-ip-client>: README
//!   "Protocol Details" (CR-terminated ASCII, the command spellings) and `const.py` / `client.py`
//!   (`COMMANDS`, `parse_reply_message`).
//! - Home Assistant's `hegel` integration `const.py` (`MODEL_INPUTS`),
//!   <https://github.com/home-assistant/core/blob/dev/homeassistant/components/hegel/const.py>:
//!   the covered models and their numbered input lists.
//!
//! No Hegel hardware has been available: every claim here is implementation-derived.

use crate::devices::service::DeviceOperation;

/// Control port (hegel-ip-client README: `HegelClient(host, port=50001)`).
pub const DEFAULT_PORT: u16 = 50001;

/// Queries sent on connect and by the poll. Queries only: **no level is ever sent on connect**.
/// (hegel-ip-client README: `-p.?`, `-v.?`, `-m.?`, `-i.?`.)
pub const STATE_QUERIES: &[&str] = &["-p.?", "-v.?", "-m.?", "-i.?"];

/// Lowest and highest level the protocol expresses: `-v.N`, N in 0..=100 percent
/// (`parse_reply_message` clamps `level / 100.0` to 0.0..=1.0).
pub const MIN_VOLUME: f32 = 0.0;
pub const MAX_VOLUME: f32 = 100.0;

/// One state fact from a reply or push line.
#[derive(Debug, Clone, PartialEq)]
pub enum HegelEvent {
    /// `-p.1` / `-p.0`.
    Power(bool),
    /// `-v.50`: percent.
    Volume(f32),
    /// `-m.1` / `-m.0` (`1` = muted).
    Mute(bool),
    /// `-i.3`: selected input number.
    Input(u8),
    /// `-e.N`: the amplifier refused a command.
    Error(String),
}

/// Split a receive buffer into complete lines, leaving any partial line in place. The protocol
/// terminates with CR (`client.py`: `readuntil(b"\r")`); a stray LF is tolerated.
pub fn drain_lines(buffer: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(end) = buffer.find(['\r', '\n']) {
        let line: String = buffer.drain(..=end).collect();
        let line = line.trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Parse one reply or push line. Heartbeat/reset lines (`-r.`) and anything unmodelled return
/// `None`. Replies the amplifier sends to a write carry the new value, so a write and a push are
/// handled identically.
pub fn parse_line(line: &str) -> Option<HegelEvent> {
    let line = line.trim();
    if let Some(value) = line.strip_prefix("-p.") {
        return match value.trim() {
            "1" => Some(HegelEvent::Power(true)),
            "0" => Some(HegelEvent::Power(false)),
            _ => None,
        };
    }
    if let Some(value) = line.strip_prefix("-v.") {
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let level: f32 = value.parse().ok()?;
        return (level <= MAX_VOLUME).then_some(HegelEvent::Volume(level));
    }
    if let Some(value) = line.strip_prefix("-m.") {
        return match value.trim() {
            "1" => Some(HegelEvent::Mute(true)),
            "0" => Some(HegelEvent::Mute(false)),
            _ => None,
        };
    }
    if let Some(value) = line.strip_prefix("-i.") {
        let value = value.trim();
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        return value.parse().ok().map(HegelEvent::Input);
    }
    line.strip_prefix("-e.")
        .map(|code| HegelEvent::Error(code.trim().to_string()))
}

/// Encode a level for `-v.N`. The caller has already clamped and quantised to whole percent, so
/// anything off the integer grid or outside 0..=100 is refused rather than "fixed".
pub fn encode_volume(level: f32) -> Option<String> {
    if !level.is_finite() || !(MIN_VOLUME..=MAX_VOLUME).contains(&level) {
        return None;
    }
    if (level - level.round()).abs() > 0.001 {
        return None;
    }
    Some(format!("-v.{}", level.round() as u32))
}

/// The wire command for an operation and the query whose answer confirms it
/// (README: `-p.1`/`-p.0`, `-m.1`/`-m.0`, `-i.N`, `-v.N`).
pub fn command(operation: &DeviceOperation) -> Option<(String, &'static str)> {
    Some(match operation {
        DeviceOperation::SetVolume(level) => (encode_volume(*level)?, "-v.?"),
        DeviceOperation::SetMute(true) => ("-m.1".to_string(), "-m.?"),
        DeviceOperation::SetMute(false) => ("-m.0".to_string(), "-m.?"),
        DeviceOperation::PowerOn => ("-p.1".to_string(), "-p.?"),
        DeviceOperation::PowerStandby => ("-p.0".to_string(), "-p.?"),
        DeviceOperation::SelectInput(code) => {
            let number: u8 = code.trim().parse().ok()?;
            if number == 0 {
                return None;
            }
            (format!("-i.{number}"), "-i.?")
        }
    })
}

/// Models Home Assistant lists as covered (its `MODEL_INPUTS` keys), with their numbered inputs.
/// Input `N` on the wire is entry `N` (1-based) of the list: HA builds
/// `dict(enumerate(MODEL_INPUTS[model], start=1))`.
pub const MODEL_INPUTS: &[(&str, &[&str])] = &[
    (
        "Röst",
        &[
            "Balanced",
            "Analog 1",
            "Analog 2",
            "Coaxial",
            "Optical 1",
            "Optical 2",
            "Optical 3",
            "USB",
            "Network",
        ],
    ),
    (
        "H95",
        &[
            "Analog 1",
            "Analog 2",
            "Coaxial",
            "Optical 1",
            "Optical 2",
            "Optical 3",
            "USB",
            "Network",
        ],
    ),
    (
        "H120",
        &[
            "Balanced",
            "Analog 1",
            "Analog 2",
            "Coaxial",
            "Optical 1",
            "Optical 2",
            "Optical 3",
            "USB",
            "Network",
        ],
    ),
    (
        "H190",
        &[
            "Balanced",
            "Analog 1",
            "Analog 2",
            "Coaxial",
            "Optical 1",
            "Optical 2",
            "Optical 3",
            "USB",
            "Network",
        ],
    ),
    (
        "H190V",
        &[
            "XLR",
            "Analog 1",
            "Analog 2",
            "Coaxial",
            "Optical 1",
            "Optical 2",
            "Optical 3",
            "USB",
            "Network",
            "Phono",
        ],
    ),
    (
        "H390",
        &[
            "XLR",
            "Analog 1",
            "Analog 2",
            "BNC",
            "Coaxial",
            "Optical 1",
            "Optical 2",
            "Optical 3",
            "USB",
            "Network",
        ],
    ),
    (
        "H590",
        &[
            "XLR 1",
            "XLR 2",
            "Analog 1",
            "Analog 2",
            "Analog 3",
            "BNC",
            "Coaxial",
            "Optical 1",
            "Optical 2",
            "Optical 3",
            "USB",
            "Network",
        ],
    ),
];

/// The covered model a reported model name denotes, or `None` for any other Hegel.
///
/// The longest covered name that prefixes the report wins, so `H190V` is not mistaken for
/// `H190` (Home Assistant's first-prefix match gets that wrong, because `H190` precedes `H190V`
/// in its table). A leading "Hegel " and the umlaut are tolerated.
pub fn covered_model(reported: &str) -> Option<&'static str> {
    let normalised = normalise_model(reported);
    MODEL_INPUTS
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| normalised.starts_with(&normalise_model(name)))
        .max_by_key(|name| name.len())
}

fn normalise_model(model: &str) -> String {
    let lowered = model.trim().to_lowercase().replace('ö', "o");
    lowered
        .strip_prefix("hegel")
        .map(str::trim)
        .unwrap_or(&lowered)
        .to_string()
}

/// The numbered input list for a model; empty for a model outside the covered list, whose input
/// numbering is not known (so input selection is refused rather than guessed).
pub fn inputs_for(model: &str) -> Vec<(String, String)> {
    let Some(covered) = covered_model(model) else {
        return Vec::new();
    };
    MODEL_INPUTS
        .iter()
        .find(|(name, _)| *name == covered)
        .map(|(_, inputs)| {
            inputs
                .iter()
                .enumerate()
                .map(|(index, name)| ((index + 1).to_string(), (*name).to_string()))
                .collect()
        })
        .unwrap_or_default()
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
    /// The device kind for a Hegel unit (manufacturer match), or `None` for any other device.
    /// The catalog's discovery record: SSDP MediaRenderer:1 with manufacturer "Hegel".
    pub fn kind(&self) -> Option<&'static str> {
        self.manufacturer
            .trim()
            .to_ascii_lowercase()
            .starts_with("hegel")
            .then_some("hegel")
    }

    /// The hardware id: the serial number, or the UDN's uuid when the description has no serial.
    /// Never the friendly name, model or IP.
    pub fn hardware_id(&self) -> Option<String> {
        if let Some(serial) = self.serial.as_deref().map(str::trim) {
            if !serial.is_empty() {
                return Some(serial.to_string());
            }
        }
        let udn = self.udn.as_deref()?.trim();
        let uuid = udn.strip_prefix("uuid:").unwrap_or(udn);
        (!uuid.is_empty()).then(|| uuid.to_string())
    }
}

/// Parse a UPnP device description. Returns `None` when it carries no manufacturer.
pub fn parse_description(xml: &str) -> Option<Description> {
    let text = |tag: &str| element_text(xml, tag).map(|value| value.trim().to_string());
    Some(Description {
        manufacturer: text("manufacturer")?,
        model: text("modelName").unwrap_or_default(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// README "Example Commands": `-p.?` `-p.1` `-p.0` `-v.?` `-v.50` `-m.1` `-m.0` `-i.?` `-i.1`.
    #[test]
    fn commands_match_the_readme_examples() {
        assert_eq!(
            command(&DeviceOperation::PowerOn),
            Some(("-p.1".to_string(), "-p.?"))
        );
        assert_eq!(
            command(&DeviceOperation::PowerStandby),
            Some(("-p.0".to_string(), "-p.?"))
        );
        assert_eq!(
            command(&DeviceOperation::SetVolume(50.0)),
            Some(("-v.50".to_string(), "-v.?"))
        );
        assert_eq!(
            command(&DeviceOperation::SetMute(true)),
            Some(("-m.1".to_string(), "-m.?"))
        );
        assert_eq!(
            command(&DeviceOperation::SetMute(false)),
            Some(("-m.0".to_string(), "-m.?"))
        );
        assert_eq!(
            command(&DeviceOperation::SelectInput("1".to_string())),
            Some(("-i.1".to_string(), "-i.?"))
        );
        assert_eq!(STATE_QUERIES, &["-p.?", "-v.?", "-m.?", "-i.?"]);
    }

    /// `client.py::parse_reply_message`: power is `.1`, volume digits, mute `1`/`0`, input digits.
    #[test]
    fn replies_parse_like_the_reference_client() {
        assert_eq!(parse_line("-p.1"), Some(HegelEvent::Power(true)));
        assert_eq!(parse_line("-p.0"), Some(HegelEvent::Power(false)));
        assert_eq!(parse_line("-v.50"), Some(HegelEvent::Volume(50.0)));
        assert_eq!(parse_line("-m.1"), Some(HegelEvent::Mute(true)));
        assert_eq!(parse_line("-m.0"), Some(HegelEvent::Mute(false)));
        assert_eq!(parse_line("-i.7"), Some(HegelEvent::Input(7)));
        assert_eq!(parse_line("-i.12"), Some(HegelEvent::Input(12)));
        assert_eq!(parse_line(" -v.0 "), Some(HegelEvent::Volume(0.0)));
    }

    #[test]
    fn heartbeats_and_malformed_lines_are_not_state() {
        // `client.py`: `-r.` / `-reset` lines are a reset/heartbeat message, not device state.
        assert_eq!(parse_line("-r.3"), None);
        assert_eq!(parse_line("-reset"), None);
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("hello"), None);
        assert_eq!(parse_line("-v."), None);
        assert_eq!(parse_line("-v.abc"), None);
        assert_eq!(parse_line("-v.-5"), None);
        assert_eq!(parse_line("-v.101"), None, "above the protocol maximum");
        assert_eq!(parse_line("-p.2"), None);
        assert_eq!(parse_line("-m.x"), None);
        assert_eq!(parse_line("-i."), None);
        assert_eq!(parse_line("-i.999"), None, "does not fit an input number");
        assert_eq!(parse_line("-e.1"), Some(HegelEvent::Error("1".to_string())));
    }

    #[test]
    fn volume_encodes_at_min_max_and_step() {
        assert_eq!(encode_volume(0.0).as_deref(), Some("-v.0"));
        assert_eq!(encode_volume(1.0).as_deref(), Some("-v.1"));
        assert_eq!(encode_volume(99.0).as_deref(), Some("-v.99"));
        assert_eq!(encode_volume(100.0).as_deref(), Some("-v.100"));
    }

    #[test]
    fn volume_outside_the_range_or_grid_is_refused_not_nudged() {
        assert_eq!(encode_volume(-1.0), None);
        assert_eq!(encode_volume(100.5), None);
        assert_eq!(encode_volume(50.5), None);
        assert_eq!(encode_volume(f32::NAN), None);
        assert_eq!(encode_volume(f32::INFINITY), None);
        assert_eq!(command(&DeviceOperation::SetVolume(150.0)), None);
    }

    #[test]
    fn input_selection_needs_a_positive_number() {
        assert_eq!(command(&DeviceOperation::SelectInput(String::new())), None);
        assert_eq!(
            command(&DeviceOperation::SelectInput("0".to_string())),
            None
        );
        assert_eq!(
            command(&DeviceOperation::SelectInput("Optical 1".to_string())),
            None
        );
        assert_eq!(
            command(&DeviceOperation::SelectInput("-i.1\r".to_string())),
            None
        );
    }

    #[test]
    fn lines_split_on_cr_and_keep_a_partial_tail() {
        let mut buffer = "-p.1\r-v.50\r-m.".to_string();
        assert_eq!(drain_lines(&mut buffer), vec!["-p.1", "-v.50"]);
        assert_eq!(buffer, "-m.");
        buffer.push_str("0\r");
        assert_eq!(drain_lines(&mut buffer), vec!["-m.0"]);
        assert!(buffer.is_empty());
    }

    /// HA `const.py::MODEL_INPUTS`: input N is the Nth entry, counting from 1.
    #[test]
    fn inputs_are_the_models_numbered_list() {
        let h190v = inputs_for("H190V");
        assert_eq!(h190v.first(), Some(&("1".to_string(), "XLR".to_string())));
        assert_eq!(h190v.last(), Some(&("10".to_string(), "Phono".to_string())));
        let h590 = inputs_for("H590");
        assert_eq!(h590.len(), 12);
        assert_eq!(h590[5], ("6".to_string(), "BNC".to_string()));
        let rost = inputs_for("Röst");
        assert_eq!(rost[0].1, "Balanced");
        assert_eq!(rost.len(), 9);
        assert_eq!(inputs_for("H95").len(), 8);
    }

    #[test]
    fn model_matching_is_longest_prefix_and_tolerant() {
        assert_eq!(covered_model("H190V"), Some("H190V"));
        assert_eq!(covered_model("H190"), Some("H190"));
        assert_eq!(covered_model("Hegel H390"), Some("H390"));
        assert_eq!(covered_model("h590"), Some("H590"));
        assert_eq!(covered_model("Rost"), Some("Röst"));
        assert_eq!(covered_model("RÖST"), Some("Röst"));
        // A newer platform is not extrapolated from a covered prefix.
        assert_eq!(covered_model("H600"), None);
        assert_eq!(covered_model("H400"), None);
        assert_eq!(covered_model(""), None);
        assert!(inputs_for("H600").is_empty());
    }

    const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
<friendlyName>Living room amp</friendlyName>
<manufacturer>Hegel</manufacturer>
<modelName>H190</modelName>
<serialNumber>H190-0042</serialNumber>
<UDN>uuid:11111111-2222-3333-4444-555555555555</UDN>
</device></root>"#;

    /// Synthetic: shaped like a UPnP MediaRenderer:1 description (the catalog's discovery record).
    #[test]
    fn description_identifies_a_hegel_by_serial() {
        let description = parse_description(DESCRIPTION).unwrap();
        assert_eq!(description.kind(), Some("hegel"));
        assert_eq!(description.model, "H190");
        assert_eq!(description.friendly_name, "Living room amp");
        assert_eq!(description.hardware_id().as_deref(), Some("H190-0042"));
    }

    #[test]
    fn identity_falls_back_to_the_udn_and_never_to_name_or_model() {
        let mut description = parse_description(DESCRIPTION).unwrap();
        description.serial = None;
        assert_eq!(
            description.hardware_id().as_deref(),
            Some("11111111-2222-3333-4444-555555555555")
        );
        description.udn = None;
        assert_eq!(description.hardware_id(), None);
    }

    #[test]
    fn other_manufacturers_and_empty_descriptions_are_refused() {
        let other = DESCRIPTION.replace(
            "<manufacturer>Hegel</manufacturer>",
            "<manufacturer>Sonos</manufacturer>",
        );
        assert_eq!(parse_description(&other).unwrap().kind(), None);
        assert_eq!(parse_description("<root></root>"), None);
        assert_eq!(parse_description("not xml at all"), None);
    }
}
