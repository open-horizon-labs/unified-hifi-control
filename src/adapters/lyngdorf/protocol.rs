//! Lyngdorf / Steinway Lyngdorf wire formats: the `!COMMAND(param)` ASCII protocol on TCP 84 and
//! the UPnP device description used to identify a unit. Pure functions only.
//!
//! Sources:
//!
//! - `lyngdorf` 2.x (MIT), <https://github.com/fishloa/lyngdorf>: `lyngdorf/rio/protocol.py`
//!   (framing, quote-aware closing paren), `rio/client.py` (command shapes, `!{command}\r`),
//!   `models/mp_series.py`, `models/p_series.py`, `models/tdai_series.py` (message tables, volume
//!   ranges), `discovery.py` (`!DEVICE?` probe, SSDP `MediaRenderer:2`, serial identity) and
//!   `const.py` (command pacing).
//! - The vendor control manuals the library transcribes: `docs/mp-60.md`, `docs/p-series.md`
//!   (including its measurements on a real P200) and `docs/tdai-1120.md`.
//!
//! Two protocol dialects share the framing: the **MP / P** processors (`!POWERONMAIN`,
//! `!MUTEON`, `!SRCS?`, `!SRC(3)"HDMI"`) and the **TDAI** amplifiers (`!ON`, `!MUTE(ON)`,
//! `!SRCLIST?`, `!SRCNAME(3,"HDMI")`). Replies are parsed for both at once; only the *commands*
//! differ by dialect.
//!
//! **TDAI-2170 is excluded**: its manual documents a serial interface only (the library models
//! it with fixed source tables and `SUBSCRIBE`), and the catalog says to exclude a model that
//! requires serial.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::devices::service::DeviceOperation;

/// Control port (`const.py::DEFAULT_LYNGDORF_PORT`).
pub const DEFAULT_PORT: u16 = 84;

/// Minimum spacing between writes: `const.py::COMMAND_PACING_MS`. A real MP-60 silently drops
/// everything past about 16 in-flight commands.
pub const COMMAND_PACING: Duration = Duration::from_millis(50);

/// Lowest level the protocol can express, in dB (`!VOL(-999)`; all three manuals agree).
pub const MIN_DB: f32 = -99.9;

/// Which command set a model speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    /// MP-40/50/60 and P100/200/300.
    Mp,
    /// TDAI-1120/2210/3400.
    Tdai,
}

/// One supported model.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Model {
    /// The name `!DEVICE?` reports (lower case).
    pub name: &'static str,
    pub dialect: Dialect,
    /// The model's documented volume ceiling in dB: `+24.0` for MP/P, `+12.0` for TDAI
    /// (`MP_VOLUME_RANGE`, `P_VOLUME_RANGE`, `TDAI_VOLUME_RANGE`). A protocol bound, not a
    /// recommendation: it is never published as the unit's own maximum.
    pub max_db: f32,
}

pub const MODELS: &[Model] = &[
    Model {
        name: "mp-40",
        dialect: Dialect::Mp,
        max_db: 24.0,
    },
    Model {
        name: "mp-50",
        dialect: Dialect::Mp,
        max_db: 24.0,
    },
    Model {
        name: "mp-60",
        dialect: Dialect::Mp,
        max_db: 24.0,
    },
    Model {
        name: "p100",
        dialect: Dialect::Mp,
        max_db: 24.0,
    },
    Model {
        name: "p200",
        dialect: Dialect::Mp,
        max_db: 24.0,
    },
    Model {
        name: "p300",
        dialect: Dialect::Mp,
        max_db: 24.0,
    },
    Model {
        name: "tdai-1120",
        dialect: Dialect::Tdai,
        max_db: 12.0,
    },
    Model {
        name: "tdai-2210",
        dialect: Dialect::Tdai,
        max_db: 12.0,
    },
    Model {
        name: "tdai-3400",
        dialect: Dialect::Tdai,
        max_db: 12.0,
    },
];

/// Resolve a reported model name. Case and surrounding whitespace/quotes are tolerated; a brand
/// prefix is **not** (`discovery.py::lookup_model`, issue #58: a guessed substring match would
/// pick the wrong command set and ranges, and `None` is the better failure).
pub fn lookup_model(reported: &str) -> Option<&'static Model> {
    let reported = reported
        .trim()
        .trim_matches('"')
        .trim()
        .to_ascii_lowercase();
    MODELS.iter().find(|model| model.name == reported)
}

/// One state fact from a reply or push line.
#[derive(Debug, Clone, PartialEq)]
pub enum LyngdorfEvent {
    /// `!DEVICE(MP-60)`.
    Device(String),
    /// `!POWER(1)` (MP/P) / `!PWR(ON)` (TDAI).
    Power(bool),
    /// `!VOL(-300)`: tenths of a dB on the wire, dB here.
    Volume(f32),
    /// `!MAXVOL(-200)`: the unit's own user-set ceiling, in dB.
    MaxVolume(f32),
    /// `!MUTEON` / `!MUTEOFF` (MP/P) and `!MUTE(ON)` / `!MUTE(OFF)` (TDAI).
    Mute(bool),
    /// `!SRCCOUNT(4)`: a source list follows.
    SourceCount(usize),
    /// `!SRC(3)"HDMI"` (MP/P) or `!SRCNAME(3,"HDMI")` (TDAI): a source with its name. Either a
    /// list entry or the answer to a current-source query; see [`SourceTracker`].
    SourceNamed { index: u16, name: String },
    /// `!SRC(3)` with no name (TDAI's current-source answer).
    SourceIndex(u16),
}

/// Split a receive buffer into complete CR-terminated lines. The TDAI family follows the CR
/// with an LF, so both framing characters are stripped (`rio/protocol.py::data_received`).
pub fn drain_lines(buffer: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(end) = buffer.find('\r') {
        let line: String = buffer.drain(..=end).collect();
        let line = line.trim_matches(['\r', '\n']).trim();
        if !line.is_empty() {
            lines.push(line.to_string());
        }
    }
    lines
}

/// Index of the first `)` at or after `start` that is not inside a quoted section
/// (`rio/protocol.py::_find_closing_paren`): TDAI names such as `"Digital 1 (Coax)"` contain one.
fn closing_paren(message: &str, start: usize) -> Option<usize> {
    let mut in_quotes = false;
    for (offset, character) in message[start..].char_indices() {
        match character {
            '"' => in_quotes = !in_quotes,
            ')' if !in_quotes => return Some(start + offset),
            _ => {}
        }
    }
    None
}

/// `(command, first parameter, text after the closing paren with its quotes removed)`.
/// `!SRC(0)"HDMI"` is `("SRC", "0", "HDMI")`; `!MUTEON` is `("MUTEON", "", "")`.
fn split_reply(line: &str) -> Option<(&str, &str, String)> {
    let message = line.trim().strip_prefix('!')?;
    if let Some(open) = message.find('(') {
        if open >= 1 {
            if let Some(close) = closing_paren(message, open + 1) {
                let tail = message[close + 1..].trim();
                let tail = tail
                    .strip_prefix('"')
                    .and_then(|rest| rest.strip_suffix('"'))
                    .unwrap_or(tail);
                return Some((
                    &message[..open],
                    &message[open + 1..close],
                    tail.to_string(),
                ));
            }
        }
    }
    Some((message, "", String::new()))
}

/// A tenths-of-a-dB wire value as dB, refusing anything outside the protocol's range.
fn tenths_to_db(value: &str) -> Option<f32> {
    let tenths: i32 = value.trim().parse().ok()?;
    ((-999..=240).contains(&tenths)).then(|| tenths as f32 / 10.0)
}

/// Parse one reply or push line. Echo lines (`#…`), zone B (`!ZVOL…`) and everything else this
/// adapter does not model return `None`.
pub fn parse_line(line: &str) -> Option<LyngdorfEvent> {
    let (command, first, tail) = split_reply(line)?;
    match command {
        "DEVICE" => {
            let name = first.trim().trim_matches('"').trim();
            (!name.is_empty()).then(|| LyngdorfEvent::Device(name.to_string()))
        }
        "POWER" => match first.trim() {
            "1" => Some(LyngdorfEvent::Power(true)),
            "0" => Some(LyngdorfEvent::Power(false)),
            // The manual documents 0..3 (combined main + zone B); a real P200 answers main only.
            _ => None,
        },
        "PWR" => match first.trim() {
            "ON" => Some(LyngdorfEvent::Power(true)),
            "OFF" => Some(LyngdorfEvent::Power(false)),
            _ => None,
        },
        "VOL" => tenths_to_db(first).map(LyngdorfEvent::Volume),
        "MAXVOL" => tenths_to_db(first).map(LyngdorfEvent::MaxVolume),
        "MUTEON" => Some(LyngdorfEvent::Mute(true)),
        "MUTEOFF" => Some(LyngdorfEvent::Mute(false)),
        "MUTE" => match first.trim() {
            "ON" => Some(LyngdorfEvent::Mute(true)),
            "OFF" => Some(LyngdorfEvent::Mute(false)),
            _ => None,
        },
        "SRCCOUNT" => first.trim().parse().ok().map(LyngdorfEvent::SourceCount),
        "SRC" => {
            let index: u16 = first.trim().parse().ok()?;
            Some(if tail.is_empty() {
                LyngdorfEvent::SourceIndex(index)
            } else {
                LyngdorfEvent::SourceNamed { index, name: tail }
            })
        }
        "SRCNAME" => {
            let (index, name) = first.split_once(',')?;
            let index: u16 = index.trim().parse().ok()?;
            let name = name.trim().trim_matches('"').to_string();
            (!name.is_empty()).then_some(LyngdorfEvent::SourceNamed { index, name })
        }
        _ => None,
    }
}

/// Builds the source catalog and the current source from the two reply shapes that look alike.
///
/// On MP/P, `!SRCS?` answers `!SRCCOUNT(N)` then N lines `!SRC(X)"Name"`, and the current-source
/// query `!SRC?` answers the *same* shape (`!SRC(X)"Name"`). The reference library tells them
/// apart by whether the list is full yet; so does this. A list that ends short must not swallow
/// every later current-source answer, so the burst window also expires.
#[derive(Debug, Default, Clone)]
pub struct SourceTracker {
    pub catalog: BTreeMap<u16, String>,
    pub current: Option<u16>,
    remaining: usize,
    deadline: Option<Instant>,
}

/// How long after `!SRCCOUNT` a named line still counts as a list entry.
const LIST_WINDOW: Duration = Duration::from_secs(2);

impl SourceTracker {
    pub fn count(&mut self, count: usize, now: Instant) {
        self.catalog.clear();
        self.remaining = count;
        self.deadline = Some(now + LIST_WINDOW);
    }

    pub fn named(&mut self, index: u16, name: String, now: Instant) {
        let listing = self.remaining > 0 && self.deadline.is_some_and(|deadline| now <= deadline);
        self.catalog.insert(index, name);
        if listing {
            self.remaining -= 1;
        } else {
            self.remaining = 0;
            self.current = Some(index);
        }
    }

    pub fn index(&mut self, index: u16) {
        self.current = Some(index);
    }
}

/// Encode a level for `!VOL(n)`: tenths of a dB, which is how both families express it
/// (`rio/client.py::volume`: `volume*10`). Refuses anything off the 0.1 dB grid or outside the
/// model's range — the caller has already clamped, so a refusal means a bug upstream.
pub fn encode_volume(db: f32, max_db: f32) -> Option<String> {
    if !db.is_finite() || db < MIN_DB - 0.001 || db > max_db + 0.001 {
        return None;
    }
    let tenths = db * 10.0;
    if (tenths - tenths.round()).abs() > 0.01 {
        return None;
    }
    Some(format!("!VOL({})", tenths.round() as i32))
}

/// State queries sent on connect and by the poll, in order. Queries only.
pub fn state_queries(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        // `models/mp_series.py::MP_SETUP_MESSAGES`: POWER, SRC, VOL, MUTE, MAXVOL.
        Dialect::Mp => &["!POWER?", "!VOL?", "!MUTE?", "!SRC?", "!MAXVOL?"],
        // `models/tdai_series.py::TDAI_SETUP_MESSAGES`: PWR, VOL, MUTE. `!SRC?` answers a bare
        // index on TDAI, which is unambiguous against the list entries.
        Dialect::Tdai => &["!PWR?", "!VOL?", "!MUTE?", "!SRC?"],
    }
}

/// The source-list query: `!SRCS?` (MP/P) or `!SRCLIST?` (TDAI).
pub fn source_list_query(dialect: Dialect) -> &'static str {
    match dialect {
        Dialect::Mp => "!SRCS?",
        Dialect::Tdai => "!SRCLIST?",
    }
}

/// The wire command for an operation and the query whose answer confirms it.
pub fn command(model: &Model, operation: &DeviceOperation) -> Option<(String, &'static str)> {
    let dialect = model.dialect;
    let power_query = state_queries(dialect)[0];
    Some(match operation {
        DeviceOperation::SetVolume(db) => (encode_volume(*db, model.max_db)?, "!VOL?"),
        DeviceOperation::SetMute(true) => ("!MUTEON".to_string(), "!MUTE?"),
        DeviceOperation::SetMute(false) => ("!MUTEOFF".to_string(), "!MUTE?"),
        DeviceOperation::PowerOn => (
            match dialect {
                Dialect::Mp => "!POWERONMAIN",
                Dialect::Tdai => "!ON",
            }
            .to_string(),
            power_query,
        ),
        DeviceOperation::PowerStandby => (
            match dialect {
                Dialect::Mp => "!POWEROFFMAIN",
                Dialect::Tdai => "!OFF",
            }
            .to_string(),
            power_query,
        ),
        DeviceOperation::SelectInput(code) => {
            let index: u16 = code.trim().parse().ok()?;
            (format!("!SRC({index})"), "!SRC?")
        }
    })
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
    /// The device kind for a Lyngdorf or Steinway Lyngdorf unit, or `None` for any other device
    /// (the catalog's discovery record: MediaRenderer:2, manufacturer Lyngdorf / Steinway Lyngdorf).
    pub fn kind(&self) -> Option<&'static str> {
        self.manufacturer
            .to_ascii_lowercase()
            .contains("lyngdorf")
            .then_some("lyngdorf")
    }

    /// The hardware id: the description's serial number (the library's `fetch_device_serial`),
    /// or the UDN's uuid when it has none. Never the friendly name, model or IP.
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

    fn mp() -> &'static Model {
        lookup_model("MP-60").unwrap()
    }

    fn tdai() -> &'static Model {
        lookup_model("TDAI-1120").unwrap()
    }

    #[test]
    fn models_resolve_case_insensitively_without_guessing() {
        assert_eq!(lookup_model("MP-60").unwrap().dialect, Dialect::Mp);
        assert_eq!(lookup_model(" mp-60\n").unwrap().name, "mp-60");
        assert_eq!(lookup_model("\"P200\"").unwrap().name, "p200");
        assert_eq!(lookup_model("TDAI-3400").unwrap().dialect, Dialect::Tdai);
        // `discovery.py::lookup_model`, issue #58: a brand prefix is deliberately not tolerated.
        assert!(lookup_model("Steinway Lyngdorf P200").is_none());
        // The serial-only model is excluded even though the Python package lists it.
        assert!(lookup_model("TDAI-2170").is_none());
        assert!(lookup_model("").is_none());
    }

    /// `docs/tdai-1120.md`: `!DEVICE?` → `!DEVICE(TDAI-1120)`; `discovery.py`: quotes tolerated.
    #[test]
    fn device_replies_parse() {
        assert_eq!(
            parse_line("!DEVICE(TDAI-1120)"),
            Some(LyngdorfEvent::Device("TDAI-1120".to_string()))
        );
        assert_eq!(
            parse_line("!DEVICE(\"MP-60\")"),
            Some(LyngdorfEvent::Device("MP-60".to_string()))
        );
        assert_eq!(parse_line("!DEVICE()"), None);
    }

    /// `docs/mp-60.md` (`!POWER(X)`, 0 standby / 1 on) and `docs/tdai-1120.md` (`!PWR(ON|OFF)`).
    #[test]
    fn power_replies_parse_for_both_dialects() {
        assert_eq!(parse_line("!POWER(1)"), Some(LyngdorfEvent::Power(true)));
        assert_eq!(parse_line("!POWER(0)"), Some(LyngdorfEvent::Power(false)));
        assert_eq!(parse_line("!PWR(ON)"), Some(LyngdorfEvent::Power(true)));
        assert_eq!(parse_line("!PWR(OFF)"), Some(LyngdorfEvent::Power(false)));
        // The manual's combined 0..3 form is not main-zone power: not guessed at.
        assert_eq!(parse_line("!POWER(2)"), None);
        assert_eq!(parse_line("!PWR(MAYBE)"), None);
    }

    /// `docs/mp-60.md`: `!MUTE ?` → `!MUTEON` | `!MUTEOFF`; `docs/tdai-1120.md`: `!MUTE(ON|OFF)`.
    #[test]
    fn mute_replies_parse_for_both_dialects() {
        assert_eq!(parse_line("!MUTEON"), Some(LyngdorfEvent::Mute(true)));
        assert_eq!(parse_line("!MUTEOFF"), Some(LyngdorfEvent::Mute(false)));
        assert_eq!(parse_line("!MUTE(ON)"), Some(LyngdorfEvent::Mute(true)));
        assert_eq!(parse_line("!MUTE(OFF)"), Some(LyngdorfEvent::Mute(false)));
        // Zone B mute is another zone.
        assert_eq!(parse_line("!ZMUTEON"), None);
    }

    /// `docs/mp-60.md`: `!VOL(X)`, -999 to 240 (-99.9 to +24.0 dB); `!MAXVOL` shares the unit.
    #[test]
    fn volume_replies_decode_at_the_extremes() {
        assert_eq!(parse_line("!VOL(-999)"), Some(LyngdorfEvent::Volume(-99.9)));
        assert_eq!(parse_line("!VOL(-300)"), Some(LyngdorfEvent::Volume(-30.0)));
        assert_eq!(parse_line("!VOL(0)"), Some(LyngdorfEvent::Volume(0.0)));
        assert_eq!(parse_line("!VOL(240)"), Some(LyngdorfEvent::Volume(24.0)));
        assert_eq!(
            parse_line("!MAXVOL(0)"),
            Some(LyngdorfEvent::MaxVolume(0.0)),
            "a real MP-60 answers !MAXVOL(0): a genuine 0.0 dB ceiling (docs/p-series.md)"
        );
        // Out of range, malformed and other zones' levels are not the main volume.
        assert_eq!(parse_line("!VOL(241)"), None);
        assert_eq!(parse_line("!VOL(-1000)"), None);
        assert_eq!(parse_line("!VOL(abc)"), None);
        assert_eq!(parse_line("!VOL()"), None);
        assert_eq!(parse_line("!ZVOL(-300)"), None);
    }

    /// `docs/mp-60.md`: `!SRCS ?` → `!SRCCOUNT(N)` then `!SRC(X)"Name"` repeated.
    #[test]
    fn mp_source_lines_parse() {
        assert_eq!(
            parse_line("!SRCCOUNT(3)"),
            Some(LyngdorfEvent::SourceCount(3))
        );
        assert_eq!(
            parse_line("!SRC(0)\"HDMI\""),
            Some(LyngdorfEvent::SourceNamed {
                index: 0,
                name: "HDMI".to_string()
            })
        );
        assert_eq!(
            parse_line("!SRC(4)\"Spotify (Connect)\""),
            Some(LyngdorfEvent::SourceNamed {
                index: 4,
                name: "Spotify (Connect)".to_string()
            })
        );
    }

    /// `docs/tdai-1120.md`: `!SRCLIST ?` → `!SRCCOUNT(N)` then `!SRCNAME(a,"Name")`; `!SRC ?` → `!SRC(n)`.
    /// `rio/protocol.py`: the name may contain parentheses (`!SRCNAME(0,"Digital 1 (Coax)")`).
    #[test]
    fn tdai_source_lines_parse() {
        assert_eq!(
            parse_line("!SRCNAME(0,\"Digital 1 (Coax)\")"),
            Some(LyngdorfEvent::SourceNamed {
                index: 0,
                name: "Digital 1 (Coax)".to_string()
            })
        );
        assert_eq!(parse_line("!SRC(5)"), Some(LyngdorfEvent::SourceIndex(5)));
        assert_eq!(
            parse_line("!SRCNAME(5,\"\")"),
            None,
            "a source that does not exist"
        );
    }

    #[test]
    fn unmodelled_and_malformed_lines_are_ignored() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("VOL(-300)"), None, "no leading bang");
        assert_eq!(
            parse_line("#VOL(-300)"),
            None,
            "verbosity-2 echo of a command"
        );
        assert_eq!(parse_line("!PONG"), None);
        assert_eq!(parse_line("!STREAMTYPE(2)"), None);
        assert_eq!(parse_line("!SRC(x)"), None);
    }

    #[test]
    fn lines_split_on_cr_and_strip_the_tdai_lf() {
        let mut buffer = "!PWR(ON)\r\n!VOL(-300)\r\n!MUTE(".to_string();
        assert_eq!(drain_lines(&mut buffer), vec!["!PWR(ON)", "!VOL(-300)"]);
        // The LF of the last CR LF pair is still on its way; it is trimmed with the next line.
        assert_eq!(buffer, "\n!MUTE(");
        buffer.push_str("OFF)\r\n");
        assert_eq!(drain_lines(&mut buffer), vec!["!MUTE(OFF)"]);
    }

    #[test]
    fn tracker_separates_the_list_from_the_current_source() {
        let now = Instant::now();
        let mut tracker = SourceTracker::default();
        tracker.count(2, now);
        tracker.named(0, "HDMI".to_string(), now);
        tracker.named(3, "Spotify".to_string(), now);
        assert_eq!(
            tracker.current, None,
            "list entries are not the current source"
        );
        // The current-source answer has the same shape, arriving once the list is full.
        tracker.named(3, "Spotify".to_string(), now);
        assert_eq!(tracker.current, Some(3));
        assert_eq!(tracker.catalog.len(), 2);
    }

    #[test]
    fn a_short_list_does_not_swallow_later_current_source_answers() {
        let now = Instant::now();
        let mut tracker = SourceTracker::default();
        tracker.count(5, now);
        tracker.named(1, "HDMI".to_string(), now);
        let later = now + Duration::from_secs(10);
        tracker.named(1, "HDMI".to_string(), later);
        assert_eq!(tracker.current, Some(1));
    }

    /// `rio/client.py::volume`: `{volume*10:.0f}` inside the parens.
    #[test]
    fn volume_encodes_in_tenths_at_min_max_and_step() {
        assert_eq!(encode_volume(-99.9, 24.0).as_deref(), Some("!VOL(-999)"));
        assert_eq!(encode_volume(-30.0, 24.0).as_deref(), Some("!VOL(-300)"));
        assert_eq!(encode_volume(-30.1, 24.0).as_deref(), Some("!VOL(-301)"));
        assert_eq!(encode_volume(0.0, 24.0).as_deref(), Some("!VOL(0)"));
        assert_eq!(encode_volume(24.0, 24.0).as_deref(), Some("!VOL(240)"));
        assert_eq!(encode_volume(12.0, 12.0).as_deref(), Some("!VOL(120)"));
    }

    #[test]
    fn volume_outside_the_models_range_is_refused() {
        assert_eq!(encode_volume(24.1, 24.0), None);
        assert_eq!(
            encode_volume(12.1, 12.0),
            None,
            "TDAI tops out 12 dB lower than MP/P"
        );
        assert_eq!(encode_volume(-100.0, 24.0), None);
        assert_eq!(encode_volume(-30.05, 24.0), None, "off the 0.1 dB grid");
        assert_eq!(encode_volume(f32::NAN, 24.0), None);
        assert_eq!(encode_volume(f32::NEG_INFINITY, 24.0), None);
    }

    /// `models/mp_series.py` / `models/tdai_series.py` message tables, `rio/client.py` command shapes.
    #[test]
    fn commands_differ_by_dialect_only_where_the_protocols_do() {
        let cmd = |model: &Model, op: DeviceOperation| command(model, &op).unwrap().0;
        assert_eq!(cmd(mp(), DeviceOperation::PowerOn), "!POWERONMAIN");
        assert_eq!(cmd(mp(), DeviceOperation::PowerStandby), "!POWEROFFMAIN");
        assert_eq!(cmd(tdai(), DeviceOperation::PowerOn), "!ON");
        assert_eq!(cmd(tdai(), DeviceOperation::PowerStandby), "!OFF");
        for model in [mp(), tdai()] {
            assert_eq!(cmd(model, DeviceOperation::SetMute(true)), "!MUTEON");
            assert_eq!(cmd(model, DeviceOperation::SetMute(false)), "!MUTEOFF");
            assert_eq!(cmd(model, DeviceOperation::SetVolume(-30.0)), "!VOL(-300)");
            assert_eq!(
                cmd(model, DeviceOperation::SelectInput("4".to_string())),
                "!SRC(4)"
            );
        }
        assert_eq!(
            command(mp(), &DeviceOperation::PowerOn).unwrap().1,
            "!POWER?"
        );
        assert_eq!(
            command(tdai(), &DeviceOperation::PowerOn).unwrap().1,
            "!PWR?"
        );
        assert_eq!(command(mp(), &DeviceOperation::SetVolume(25.0)), None);
        assert_eq!(command(tdai(), &DeviceOperation::SetVolume(13.0)), None);
        assert_eq!(
            command(mp(), &DeviceOperation::SelectInput("HDMI".to_string())),
            None
        );
    }

    #[test]
    fn queries_never_carry_a_value() {
        for dialect in [Dialect::Mp, Dialect::Tdai] {
            for query in state_queries(dialect)
                .iter()
                .chain([source_list_query(dialect)].iter())
            {
                assert!(query.ends_with('?') && !query.contains('('), "{query}");
            }
        }
        assert_eq!(source_list_query(Dialect::Mp), "!SRCS?");
        assert_eq!(source_list_query(Dialect::Tdai), "!SRCLIST?");
    }

    const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<deviceType>urn:schemas-upnp-org:device:MediaRenderer:2</deviceType>
<friendlyName>Lounge</friendlyName>
<manufacturer>Steinway Lyngdorf</manufacturer>
<modelName>P200</modelName>
<serialNumber>ab12cd34</serialNumber>
<UDN>uuid:aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee</UDN>
</device></root>"#;

    /// Synthetic: shaped like a MediaRenderer:2 description (the catalog's discovery record);
    /// `discovery.py` reads `device/serialNumber` and a live P200 reports a bare `P200` model.
    #[test]
    fn description_identifies_a_lyngdorf_by_serial() {
        let description = parse_description(DESCRIPTION).unwrap();
        assert_eq!(description.kind(), Some("lyngdorf"));
        assert_eq!(description.model, "P200");
        assert_eq!(description.hardware_id().as_deref(), Some("ab12cd34"));
        let plain = DESCRIPTION.replace("Steinway Lyngdorf", "Lyngdorf");
        assert_eq!(parse_description(&plain).unwrap().kind(), Some("lyngdorf"));
    }

    #[test]
    fn identity_falls_back_to_the_udn_and_never_to_name_or_model() {
        let mut description = parse_description(DESCRIPTION).unwrap();
        description.serial = None;
        assert_eq!(
            description.hardware_id().as_deref(),
            Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
        );
        description.udn = None;
        assert_eq!(description.hardware_id(), None);
    }

    #[test]
    fn other_manufacturers_are_refused() {
        let other = DESCRIPTION.replace("Steinway Lyngdorf", "Sonos");
        assert_eq!(parse_description(&other).unwrap().kind(), None);
        assert_eq!(parse_description("<root/>"), None);
    }
}
