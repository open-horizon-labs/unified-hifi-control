//! Pure Arcam wire formats: frames, command codes, per-series source tables, discovery.
//!
//! Command codes, answer codes, source tables and RC5 tables come from the MIT-licensed
//! `arcam_fmj` client (<https://github.com/elupus/arcam_fmj>, files `packets.py`, `commands.py`,
//! `codecs.py`, `rc5.py`, `models.py`), which in turn encodes Arcam's published RS232/IP control
//! documents (SH256E, SH274E, SH289E, SH306E, SH309). Cited per item below.
//!
//! # Framing (`packets.py`)
//!
//! ```text
//! command:  0x21 | zone | cc | data length | data... | 0x0D
//! response: 0x21 | zone | cc | answer code | data length | data... | 0x0D
//! ```
//!
//! A query is a command whose data is the single byte `0xF0`.

use crate::devices::service::DeviceOperation;
use crate::devices::{DeviceInput, DevicePower};

pub const DEFAULT_PORT: u16 = 50000;
const STR: u8 = 0x21;
const ETR: u8 = 0x0D;
const QUERY: u8 = 0xF0;
/// Main zone. Zone 2 is not exposed.
pub const ZONE: u8 = 1;

/// Command codes (`commands.py`, `CommandCodes`).
pub mod cc {
    pub const POWER: u8 = 0x00;
    pub const SIMULATE_RC5: u8 = 0x08;
    pub const VOLUME: u8 = 0x0D;
    pub const MUTE: u8 = 0x0E;
    pub const CURRENT_SOURCE: u8 = 0x1D;
}

/// Answer codes (`codecs.py`, `AnswerCodes`).
pub mod ac {
    pub const STATUS_UPDATE: u8 = 0x00;
    pub const ZONE_INVALID: u8 = 0x82;
    pub const COMMAND_NOT_RECOGNISED: u8 = 0x83;
    pub const PARAMETER_NOT_RECOGNISED: u8 = 0x84;
    pub const COMMAND_INVALID_AT_THIS_TIME: u8 = 0x85;
    pub const INVALID_DATA_LENGTH: u8 = 0x86;
}

/// Highest level the protocol defines for volume (0-99). A protocol constant, not a reading from
/// the unit.
pub const MAX_VOLUME: u8 = 99;

/// Product family, which decides how each command is written (`models.py`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Series {
    /// AVR380/450/750 (SH256E).
    Avr450,
    /// AV860/AVR850/550/390, SR250, RV-6/9, MC-10 (SH274E).
    Avr860,
    /// AVR5/10/11/20/21/30/31, AV40/41, SDP-55/58 (SH289E).
    Hda,
    /// SA10/20/30/750 integrated amplifiers (SH306E).
    Sa,
    /// PA720/240/410 power amplifiers: no volume, no source.
    Pa,
    /// ST60 streamer (SH309).
    St,
}

const MODELS: &[(&str, Series)] = &[
    ("AVR380", Series::Avr450),
    ("AVR450", Series::Avr450),
    ("AVR750", Series::Avr450),
    ("AV860", Series::Avr860),
    ("AVR850", Series::Avr860),
    ("AVR550", Series::Avr860),
    ("AVR390", Series::Avr860),
    ("SR250", Series::Avr860),
    ("RV-6", Series::Avr860),
    ("RV-9", Series::Avr860),
    ("MC-10", Series::Avr860),
    ("AVR5", Series::Hda),
    ("AVR10", Series::Hda),
    ("AVR20", Series::Hda),
    ("AVR30", Series::Hda),
    ("AV40", Series::Hda),
    ("AVR11", Series::Hda),
    ("AVR21", Series::Hda),
    ("AVR31", Series::Hda),
    ("AV41", Series::Hda),
    ("SDP-55", Series::Hda),
    ("SDP-58", Series::Hda),
    ("SA10", Series::Sa),
    ("SA20", Series::Sa),
    ("SA30", Series::Sa),
    ("SA750", Series::Sa),
    ("PA720", Series::Pa),
    ("PA240", Series::Pa),
    ("PA410", Series::Pa),
    ("ST60", Series::St),
];

impl Series {
    /// The series for a model name as a device description or AMX answer gives it ("SA30",
    /// "Arcam SA30"). `None` for a model not in the tables: the adapter then refuses every write
    /// whose encoding depends on the series, rather than guess one (the client library guesses
    /// the 450 series; a wrong guess would select the wrong input).
    pub fn for_model(model: &str) -> Option<Self> {
        let normalised = model.trim().to_ascii_uppercase();
        if let Some((_, series)) = MODELS.iter().find(|(name, _)| *name == normalised) {
            return Some(*series);
        }
        MODELS
            .iter()
            .filter(|(name, _)| {
                // Whole-token match so "AVR5" does not claim "AVR50".
                normalised
                    .split(|c: char| c.is_whitespace() || c == '_' || c == '(' || c == ')')
                    .any(|token| token == *name)
            })
            .max_by_key(|(name, _)| name.len())
            .map(|(_, series)| *series)
    }

    pub fn has_volume(self) -> bool {
        !matches!(self, Series::Pa)
    }

    /// `POWER_WRITE_SUPPORTED` / `MUTE_WRITE_SUPPORTED` in `commands.py`: a direct write of the
    /// command. Other series take simulated RC5 key presses.
    fn direct_power_and_mute(self) -> bool {
        matches!(self, Series::Sa | Series::Pa | Series::St)
    }

    /// `SOURCE_WRITE_SUPPORTED`: only the SA series takes a direct source write.
    fn direct_source(self) -> bool {
        matches!(self, Series::Sa)
    }

    pub fn can_select_source(self) -> bool {
        !matches!(self, Series::Pa)
    }

    /// Wire byte for each source name when *reading* `CURRENT_SOURCE` (`codecs.py`
    /// `*_SOURCE_MAPPING`). Zone-1 entries only.
    fn source_read_table(self) -> &'static [(&'static str, u8)] {
        match self {
            Series::Avr450 | Series::Avr860 => &[
                ("CD", 0x01),
                ("BD", 0x02),
                ("AV", 0x03),
                ("SAT", 0x04),
                ("PVR", 0x05),
                ("VCR", 0x06),
                ("AUX", 0x08),
                ("DISPLAY", 0x09),
                ("FM", 0x0B),
                ("DAB", 0x0C),
                ("NET", 0x0E),
                ("USB", 0x0F),
                ("STB", 0x10),
                ("GAME", 0x11),
                ("PHONO", 0x12),
                ("ARC_ERC", 0x13),
            ],
            Series::Hda => &[
                ("CD", 0x01),
                ("BD", 0x02),
                ("AV", 0x03),
                ("SAT", 0x04),
                ("PVR", 0x05),
                ("UHD", 0x06),
                ("AUX", 0x08),
                ("DISPLAY", 0x09),
                ("FM", 0x0B),
                ("DAB", 0x0C),
                ("NET", 0x0E),
                ("USB", 0x0F),
                ("STB", 0x10),
                ("GAME", 0x11),
                ("BT", 0x12),
            ],
            // NET and USB share 0x0B on the SA series; reading yields NET, so USB is not listed.
            Series::Sa => &[
                ("PHONO", 0x01),
                ("AUX", 0x02),
                ("PVR", 0x03),
                ("AV", 0x04),
                ("STB", 0x05),
                ("CD", 0x06),
                ("BD", 0x07),
                ("SAT", 0x08),
                ("GAME", 0x09),
                ("NET", 0x0B),
                ("ARC_ERC", 0x0D),
            ],
            Series::St => &[
                ("DIG1", 0x01),
                ("DIG2", 0x02),
                ("DIG3", 0x03),
                ("DIG4", 0x04),
                ("NET_USB", 0x05),
            ],
            Series::Pa => &[],
        }
    }

    /// Simulated RC5 key `(system, command)` that selects a source on series without a direct
    /// write (`rc5.py` `RC5CODE_SOURCE`, zone 1).
    fn source_rc5(self, source: &str) -> Option<[u8; 2]> {
        let code = match self {
            Series::Avr450 => match source {
                "STB" => 1,
                "AV" => 2,
                "DAB" => 72,
                "FM" => 54,
                "BD" => 4,
                "GAME" => 5,
                "VCR" => 6,
                "CD" => 7,
                "AUX" => 8,
                "DISPLAY" => 9,
                "SAT" => 0,
                "PVR" => 34,
                "USB" => 18,
                "NET" => 11,
                _ => return None,
            },
            Series::Avr860 => match source {
                "STB" => 100,
                "AV" => 94,
                "DAB" => 72,
                "FM" => 28,
                "BD" => 98,
                "GAME" => 97,
                "VCR" => 119,
                "CD" => 118,
                "AUX" => 99,
                "DISPLAY" => 58,
                "SAT" => 27,
                "PVR" => 96,
                "USB" => 93,
                "NET" => 92,
                _ => return None,
            },
            Series::Hda => match source {
                "STB" => 100,
                "AV" => 94,
                "DAB" => 72,
                "FM" => 28,
                "BD" => 98,
                "GAME" => 97,
                "UHD" => 125,
                "CD" => 118,
                "AUX" => 99,
                "DISPLAY" => 58,
                "SAT" => 27,
                "PVR" => 96,
                "NET" => 92,
                "BT" => 122,
                _ => return None,
            },
            // The ST60 reads one combined NET_USB source; its RC5 key for it is the NET key.
            Series::St => {
                return match source {
                    "DIG1" => Some([21, 94]),
                    "DIG2" => Some([21, 98]),
                    "DIG3" => Some([21, 27]),
                    "DIG4" => Some([21, 97]),
                    "NET_USB" => Some([21, 92]),
                    _ => None,
                }
            }
            Series::Sa | Series::Pa => return None,
        };
        Some([16, code])
    }

    /// Selectable inputs: what the series can both read back and write.
    pub fn inputs(self) -> Vec<DeviceInput> {
        if !self.can_select_source() {
            return Vec::new();
        }
        self.source_read_table()
            .iter()
            .filter(|(name, _)| self.direct_source() || self.source_rc5(name).is_some())
            .map(|(name, _)| DeviceInput {
                id: (*name).to_string(),
                name: input_display_name(name),
                available: true,
            })
            .collect()
    }

    /// Source name for a `CURRENT_SOURCE` byte, or `None` for an unmapped byte.
    pub fn source_name(self, byte: u8) -> Option<&'static str> {
        self.source_read_table()
            .iter()
            .find(|(_, wire)| *wire == byte)
            .map(|(name, _)| *name)
    }

    fn source_byte(self, name: &str) -> Option<u8> {
        self.source_read_table()
            .iter()
            .find(|(source, _)| *source == name)
            .map(|(_, wire)| *wire)
    }

    /// RC5 mute key (`RC5CODE_MUTE`, zone 1). `mute = false` is the discrete "unmute" key.
    fn mute_rc5(self, mute: bool) -> Option<[u8; 2]> {
        match (self, mute) {
            (Series::Avr450, true) => Some([16, 119]),
            (Series::Avr450, false) => Some([16, 120]),
            (Series::Avr860 | Series::Hda, true) => Some([16, 26]),
            (Series::Avr860 | Series::Hda, false) => Some([16, 120]),
            _ => None,
        }
    }

    /// RC5 power keys (`RC5CODE_POWER`, zone 1): discrete on `[16,123]` and standby `[16,124]`.
    fn power_rc5(self, on: bool) -> Option<[u8; 2]> {
        match self {
            Series::Avr450 | Series::Avr860 | Series::Hda => {
                Some(if on { [16, 123] } else { [16, 124] })
            }
            _ => None,
        }
    }
}

fn input_display_name(source: &str) -> String {
    match source {
        "ARC_ERC" => "HDMI ARC".to_string(),
        "NET" => "Network".to_string(),
        "NET_USB" => "Network / USB".to_string(),
        "BT" => "Bluetooth".to_string(),
        "UHD" => "UHD".to_string(),
        other => other.to_string(),
    }
}

/// Encode one command frame. Fails when the data cannot be length-prefixed.
pub fn encode_command(zone: u8, command: u8, data: &[u8]) -> Result<Vec<u8>, String> {
    let length = u8::try_from(data.len()).map_err(|_| "Arcam command data too long".to_string())?;
    let mut frame = Vec::with_capacity(data.len() + 5);
    frame.extend_from_slice(&[STR, zone, command, length]);
    frame.extend_from_slice(data);
    frame.push(ETR);
    Ok(frame)
}

/// A query for `command`: data is the single byte `0xF0`.
pub fn query(command: u8) -> Vec<u8> {
    vec![STR, ZONE, command, 1, QUERY, ETR]
}

/// Queries sent on connect and on each full refresh. Queries only.
pub fn state_queries(series: Option<Series>) -> Vec<Vec<u8>> {
    let mut queries = vec![query(cc::POWER)];
    if series.is_none_or(Series::has_volume) {
        queries.push(query(cc::VOLUME));
    }
    queries.push(query(cc::MUTE));
    if series.is_none_or(Series::can_select_source) {
        queries.push(query(cc::CURRENT_SOURCE));
    }
    queries
}

/// A parsed response frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub zone: u8,
    pub command: u8,
    pub answer: u8,
    pub data: Vec<u8>,
}

/// Take every complete response out of `buffer`.
///
/// Resynchronises on garbage instead of failing: leading bytes that are not a frame start are
/// dropped, a frame whose terminator is not `0x0D` is dropped one byte at a time, and the AMX
/// discovery answer (`AMXB...\r`, or the `\x01^AMX` variant) is consumed and ignored here (see
/// [`parse_amx_response`]).
pub fn drain_responses(buffer: &mut Vec<u8>) -> Vec<Response> {
    let mut out = Vec::new();
    loop {
        if buffer.is_empty() {
            break;
        }
        if buffer[0] != STR {
            if buffer.starts_with(b"AMX") || buffer.starts_with(b"\x01^AMX") {
                match buffer.iter().position(|b| *b == ETR) {
                    Some(end) => {
                        buffer.drain(..=end);
                        continue;
                    }
                    None => break,
                }
            }
            if b"AMX".starts_with(buffer.as_slice()) || b"\x01^AMX".starts_with(buffer.as_slice()) {
                // A possible AMX prefix split across reads.
                break;
            }
            buffer.remove(0);
            continue;
        }
        if buffer.len() < 5 {
            break;
        }
        let total = 6 + usize::from(buffer[4]);
        if buffer.len() < total {
            break;
        }
        if buffer[total - 1] != ETR {
            buffer.remove(0);
            continue;
        }
        out.push(Response {
            zone: buffer[1],
            command: buffer[2],
            answer: buffer[3],
            data: buffer[5..5 + usize::from(buffer[4])].to_vec(),
        });
        buffer.drain(..total);
    }
    out
}

/// What a response means for the main zone's state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reply {
    Power(bool),
    Mute(bool),
    Volume(u8),
    /// A source name, or `CODE_xx` for a byte the series table does not map.
    Source(String),
    /// The unit refused a command or query. Not an error for queries in standby.
    Rejected {
        command: u8,
        answer: u8,
    },
}

pub fn answer_text(answer: u8) -> &'static str {
    match answer {
        ac::ZONE_INVALID => "zone invalid",
        ac::COMMAND_NOT_RECOGNISED => "command not recognised",
        ac::PARAMETER_NOT_RECOGNISED => "parameter not recognised",
        ac::COMMAND_INVALID_AT_THIS_TIME => "command invalid at this time",
        ac::INVALID_DATA_LENGTH => "invalid data length",
        _ => "unknown error",
    }
}

/// Interpret a response. `None` for zones and commands this adapter does not model.
pub fn parse_reply(response: &Response, series: Option<Series>) -> Option<Reply> {
    if response.zone != ZONE {
        return None;
    }
    if !matches!(
        response.command,
        cc::POWER | cc::VOLUME | cc::MUTE | cc::CURRENT_SOURCE | cc::SIMULATE_RC5
    ) {
        return None;
    }
    if response.answer != ac::STATUS_UPDATE {
        return Some(Reply::Rejected {
            command: response.command,
            answer: response.answer,
        });
    }
    let byte = *response.data.first()?;
    match response.command {
        // 0x01 on, 0x00 standby (`state.py` get_power).
        cc::POWER => match byte {
            0x01 => Some(Reply::Power(true)),
            0x00 => Some(Reply::Power(false)),
            _ => None,
        },
        // 0x00 is muted, 0x01 unmuted (`state.py` get_mute).
        cc::MUTE => match byte {
            0x00 => Some(Reply::Mute(true)),
            0x01 => Some(Reply::Mute(false)),
            _ => None,
        },
        cc::VOLUME => (byte <= MAX_VOLUME).then_some(Reply::Volume(byte)),
        cc::CURRENT_SOURCE => Some(Reply::Source(
            series
                .and_then(|series| series.source_name(byte))
                .map(str::to_string)
                .unwrap_or_else(|| format!("CODE_{byte:02X}")),
        )),
        _ => None,
    }
}

pub fn power_of(on: bool) -> DevicePower {
    if on {
        DevicePower::On
    } else {
        DevicePower::Standby
    }
}

/// One operation as frames to send, and the commands to read back afterwards.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub frames: Vec<Vec<u8>>,
    pub readback: Vec<u8>,
}

/// Plan an operation for a series. Levels arrive clamped and quantised; this only refuses what
/// the series cannot be written with.
pub fn plan_operation(series: Option<Series>, operation: &DeviceOperation) -> Result<Plan, String> {
    let unrecognised = || {
        "the Arcam model is not recognised, so this command cannot be encoded safely (its \
         encoding depends on the product series)"
            .to_string()
    };
    match operation {
        DeviceOperation::SetVolume(level) => {
            if series.is_some_and(|series| !series.has_volume()) {
                return Err(
                    "this Arcam model is a power amplifier with no volume control".to_string(),
                );
            }
            if !level.is_finite() || *level < 0.0 || *level > f32::from(MAX_VOLUME) {
                return Err(format!(
                    "volume {level} is outside the Arcam range 0-{MAX_VOLUME}"
                ));
            }
            Ok(Plan {
                frames: vec![encode_command(ZONE, cc::VOLUME, &[level.round() as u8])?],
                readback: vec![cc::VOLUME],
            })
        }
        DeviceOperation::SetMute(muted) => {
            let series = series.ok_or_else(unrecognised)?;
            let frame = if series.direct_power_and_mute() {
                // 0x00 mutes, 0x01 unmutes (`state.py` set_mute).
                encode_command(ZONE, cc::MUTE, &[if *muted { 0x00 } else { 0x01 }])?
            } else {
                let key = series
                    .mute_rc5(*muted)
                    .ok_or_else(|| "mute is not supported by this Arcam model".to_string())?;
                encode_command(ZONE, cc::SIMULATE_RC5, &key)?
            };
            Ok(Plan {
                frames: vec![frame],
                readback: vec![cc::MUTE],
            })
        }
        DeviceOperation::PowerOn | DeviceOperation::PowerStandby => {
            let on = matches!(operation, DeviceOperation::PowerOn);
            let series = series.ok_or_else(unrecognised)?;
            let frame = if series.direct_power_and_mute() {
                encode_command(ZONE, cc::POWER, &[u8::from(on)])?
            } else {
                let key = series
                    .power_rc5(on)
                    .ok_or_else(|| "power is not supported by this Arcam model".to_string())?;
                encode_command(ZONE, cc::SIMULATE_RC5, &key)?
            };
            // Powering on: also re-read everything the unit could not answer in standby.
            let readback = if on {
                vec![cc::POWER, cc::VOLUME, cc::MUTE, cc::CURRENT_SOURCE]
            } else {
                vec![cc::POWER]
            };
            Ok(Plan {
                frames: vec![frame],
                readback,
            })
        }
        DeviceOperation::SelectInput(source) => {
            let series = series.ok_or_else(unrecognised)?;
            if !series.can_select_source()
                || !series.inputs().iter().any(|input| input.id == *source)
            {
                return Err(format!(
                    "input {source:?} is not selectable on this Arcam model"
                ));
            }
            let frame = if series.direct_source() {
                let byte = series
                    .source_byte(source)
                    .ok_or_else(|| format!("no source code for {source:?}"))?;
                encode_command(ZONE, cc::CURRENT_SOURCE, &[byte])?
            } else {
                let key = series
                    .source_rc5(source)
                    .ok_or_else(|| format!("no RC5 key for {source:?}"))?;
                encode_command(ZONE, cc::SIMULATE_RC5, &key)?
            };
            Ok(Plan {
                frames: vec![frame],
                readback: vec![cc::CURRENT_SOURCE],
            })
        }
    }
}

/// The AMX discovery request sent over the control connection (`packets.py` `AmxDuetRequest`).
pub const AMX_REQUEST: &[u8] = b"AMX\r";

/// An AMX Duet answer: `AMXB<Device-Make=ARCAM><Device-Model=SA30>...\r`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AmxInfo {
    pub make: Option<String>,
    pub model: Option<String>,
    pub revision: Option<String>,
}

pub fn parse_amx_response(data: &[u8]) -> Option<AmxInfo> {
    let text = std::str::from_utf8(data).ok()?;
    let body = text.trim_end_matches('\r').strip_prefix("AMXB")?;
    let mut info = AmxInfo {
        make: None,
        model: None,
        revision: None,
    };
    let mut rest = body;
    while let Some(start) = rest.find('<') {
        let after = &rest[start + 1..];
        let end = after.find('>')?;
        if let Some((key, value)) = after[..end].split_once('=') {
            match key {
                "Device-Make" => info.make = Some(value.to_string()),
                "Device-Model" => info.model = Some(value.to_string()),
                "Device-Revision" => info.revision = Some(value.to_string()),
                _ => {}
            }
        }
        rest = &after[end + 1..];
    }
    Some(info)
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
    /// Catalog: SSDP manufacturer `ARCAM`. A rebadged JBL/AudioControl unit is not claimed.
    pub fn is_arcam(&self) -> bool {
        self.manufacturer.trim().eq_ignore_ascii_case("arcam")
    }

    /// Hardware id: the serial number, else the MAC-bearing tail of the UDN, else the UDN's
    /// UUID. Never the model, friendly name or address.
    pub fn hardware_id(&self) -> Option<String> {
        if let Some(serial) = self.serial.as_deref().map(str::trim) {
            if !serial.is_empty() {
                return Some(serial.to_string());
            }
        }
        let uuid = self.udn.as_deref()?.trim().strip_prefix("uuid:")?;
        let tail = uuid.rsplit('-').next()?;
        if tail.len() == 12 && tail.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(tail.to_ascii_uppercase());
        }
        (!uuid.is_empty()).then(|| uuid.to_string())
    }
}

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

    fn plan(series: Series, op: DeviceOperation) -> Plan {
        plan_operation(Some(series), &op).unwrap()
    }

    /// Frame layout per `packets.py` `CommandPacket.to_bytes`: STR zone cc len data ETR.
    #[test]
    fn query_and_command_frames_match_the_packet_layout() {
        assert_eq!(query(cc::POWER), vec![0x21, 0x01, 0x00, 0x01, 0xF0, 0x0D]);
        assert_eq!(
            encode_command(1, cc::VOLUME, &[20]).unwrap(),
            vec![0x21, 0x01, 0x0D, 0x01, 0x14, 0x0D]
        );
        assert_eq!(
            encode_command(1, cc::SIMULATE_RC5, &[16, 123]).unwrap(),
            vec![0x21, 0x01, 0x08, 0x02, 0x10, 0x7B, 0x0D]
        );
        assert!(encode_command(1, 0, &[0u8; 300]).is_err());
    }

    /// Response layout per `packets.py` `ResponsePacket.from_bytes`: STR zone cc ac len data ETR.
    #[test]
    fn responses_parse_and_resync() {
        let mut buffer = vec![0x00, 0x21, 0x01, 0x00, 0x00, 0x01, 0x01, 0x0D];
        buffer.extend_from_slice(&[0x21, 0x01, 0x0D, 0x00, 0x01, 0x14, 0x0D]);
        // A half frame stays buffered.
        buffer.extend_from_slice(&[0x21, 0x01, 0x0E, 0x00]);
        let responses = drain_responses(&mut buffer);
        assert_eq!(responses.len(), 2);
        assert_eq!(parse_reply(&responses[0], None), Some(Reply::Power(true)));
        assert_eq!(parse_reply(&responses[1], None), Some(Reply::Volume(20)));
        assert_eq!(buffer, vec![0x21, 0x01, 0x0E, 0x00]);
        buffer.extend_from_slice(&[0x01, 0x00, 0x0D]);
        let responses = drain_responses(&mut buffer);
        assert_eq!(parse_reply(&responses[0], None), Some(Reply::Mute(true)));
        assert!(buffer.is_empty());
    }

    #[test]
    fn bad_terminator_and_amx_frames_are_skipped() {
        // Wrong ETR: the false start is dropped, the real frame behind it survives.
        let mut buffer = vec![0x21, 0x01, 0x00, 0x00, 0x01, 0x01, 0x55];
        buffer.extend_from_slice(&[0x21, 0x01, 0x00, 0x00, 0x01, 0x00, 0x0D]);
        let responses = drain_responses(&mut buffer);
        assert_eq!(responses.len(), 1);
        assert_eq!(parse_reply(&responses[0], None), Some(Reply::Power(false)));
        let mut amx = b"AMXB<Device-Make=ARCAM>\r".to_vec();
        amx.extend_from_slice(&[0x21, 0x01, 0x0D, 0x00, 0x01, 0x05, 0x0D]);
        assert_eq!(drain_responses(&mut amx).len(), 1);
    }

    #[test]
    fn rejections_and_malformed_values() {
        let rejected = Response {
            zone: 1,
            command: cc::VOLUME,
            answer: ac::COMMAND_INVALID_AT_THIS_TIME,
            data: vec![],
        };
        assert_eq!(
            parse_reply(&rejected, None),
            Some(Reply::Rejected {
                command: cc::VOLUME,
                answer: 0x85
            })
        );
        let too_loud = Response {
            zone: 1,
            command: cc::VOLUME,
            answer: 0,
            data: vec![100],
        };
        assert_eq!(parse_reply(&too_loud, None), None);
        let zone2 = Response {
            zone: 2,
            command: cc::POWER,
            answer: 0,
            data: vec![1],
        };
        assert_eq!(parse_reply(&zone2, None), None);
        assert_eq!(answer_text(0x85), "command invalid at this time");
    }

    #[test]
    fn model_names_map_to_series() {
        assert_eq!(Series::for_model("SA30"), Some(Series::Sa));
        assert_eq!(Series::for_model("Arcam AVR30"), Some(Series::Hda));
        assert_eq!(Series::for_model("AVR550"), Some(Series::Avr860));
        assert_eq!(Series::for_model("ST60"), Some(Series::St));
        assert_eq!(Series::for_model("PA720"), Some(Series::Pa));
        // Whole-token matching: AVR5 is not AVR50.
        assert_eq!(Series::for_model("AVR50"), None);
        assert_eq!(Series::for_model("Radia"), None);
    }

    #[test]
    fn volume_encodes_at_min_and_max_and_refuses_beyond() {
        let sa = |level| plan_operation(Some(Series::Sa), &DeviceOperation::SetVolume(level));
        assert_eq!(sa(0.0).unwrap().frames[0][4], 0);
        assert_eq!(sa(99.0).unwrap().frames[0][4], 99);
        assert!(sa(100.0).is_err());
        assert!(sa(-1.0).is_err());
        assert!(sa(f32::NAN).is_err());
        // Power amplifiers have no volume.
        assert!(plan_operation(Some(Series::Pa), &DeviceOperation::SetVolume(10.0)).is_err());
    }

    /// Direct writes on SA/PA/ST (`POWER_WRITE_SUPPORTED`), RC5 on AVRs (`rc5.py`).
    #[test]
    fn power_and_mute_use_direct_writes_or_rc5_per_series() {
        assert_eq!(
            plan(Series::Sa, DeviceOperation::PowerOn).frames[0],
            vec![0x21, 1, 0x00, 1, 0x01, 0x0D]
        );
        assert_eq!(
            plan(Series::St, DeviceOperation::PowerStandby).frames[0],
            vec![0x21, 1, 0x00, 1, 0x00, 0x0D]
        );
        assert_eq!(
            plan(Series::Hda, DeviceOperation::PowerOn).frames[0],
            vec![0x21, 1, 0x08, 2, 16, 123, 0x0D]
        );
        assert_eq!(
            plan(Series::Hda, DeviceOperation::PowerStandby).frames[0],
            vec![0x21, 1, 0x08, 2, 16, 124, 0x0D]
        );
        assert_eq!(
            plan(Series::Sa, DeviceOperation::SetMute(true)).frames[0],
            vec![0x21, 1, 0x0E, 1, 0x00, 0x0D]
        );
        assert_eq!(
            plan(Series::Sa, DeviceOperation::SetMute(false)).frames[0],
            vec![0x21, 1, 0x0E, 1, 0x01, 0x0D]
        );
        assert_eq!(
            plan(Series::Avr860, DeviceOperation::SetMute(true)).frames[0],
            vec![0x21, 1, 0x08, 2, 16, 26, 0x0D]
        );
        assert_eq!(
            plan(Series::Avr860, DeviceOperation::SetMute(false)).frames[0],
            vec![0x21, 1, 0x08, 2, 16, 120, 0x0D]
        );
        assert_eq!(
            plan(Series::Hda, DeviceOperation::PowerOn).readback[0],
            cc::POWER
        );
        assert_eq!(
            plan(Series::Hda, DeviceOperation::PowerStandby).readback,
            vec![cc::POWER]
        );
    }

    #[test]
    fn source_selection_is_direct_on_sa_and_rc5_elsewhere() {
        // SA: CD is 0x06.
        assert_eq!(
            plan(Series::Sa, DeviceOperation::SelectInput("CD".to_string())).frames[0],
            vec![0x21, 1, 0x1D, 1, 0x06, 0x0D]
        );
        // HDA: CD is RC5 [16,118].
        assert_eq!(
            plan(Series::Hda, DeviceOperation::SelectInput("CD".to_string())).frames[0],
            vec![0x21, 1, 0x08, 2, 16, 118, 0x0D]
        );
        // ST60: DIG1 is RC5 [21,94].
        assert_eq!(
            plan(Series::St, DeviceOperation::SelectInput("DIG1".to_string())).frames[0],
            vec![0x21, 1, 0x08, 2, 21, 94, 0x0D]
        );
        // A source the series lacks is refused, not guessed.
        assert!(plan_operation(
            Some(Series::Sa),
            &DeviceOperation::SelectInput("BT".to_string())
        )
        .is_err());
        assert!(plan_operation(
            Some(Series::Pa),
            &DeviceOperation::SelectInput("CD".to_string())
        )
        .is_err());
    }

    #[test]
    fn unrecognised_models_refuse_series_dependent_writes() {
        for op in [
            DeviceOperation::PowerOn,
            DeviceOperation::SetMute(true),
            DeviceOperation::SelectInput("CD".to_string()),
        ] {
            assert!(plan_operation(None, &op).is_err());
        }
        assert!(plan_operation(None, &DeviceOperation::SetVolume(10.0)).is_ok());
    }

    #[test]
    fn source_bytes_read_back_per_series() {
        let reply = |series, byte| {
            parse_reply(
                &Response {
                    zone: 1,
                    command: cc::CURRENT_SOURCE,
                    answer: 0,
                    data: vec![byte],
                },
                Some(series),
            )
        };
        assert_eq!(
            reply(Series::Sa, 0x06),
            Some(Reply::Source("CD".to_string()))
        );
        assert_eq!(
            reply(Series::Hda, 0x12),
            Some(Reply::Source("BT".to_string()))
        );
        assert_eq!(
            reply(Series::Avr450, 0x12),
            Some(Reply::Source("PHONO".to_string()))
        );
        assert_eq!(
            reply(Series::St, 0x05),
            Some(Reply::Source("NET_USB".to_string()))
        );
        assert_eq!(
            reply(Series::Sa, 0x7F),
            Some(Reply::Source("CODE_7F".to_string()))
        );
    }

    #[test]
    fn input_catalogs_only_list_selectable_sources() {
        let sa: Vec<String> = Series::Sa.inputs().into_iter().map(|i| i.id).collect();
        assert!(sa.contains(&"CD".to_string()) && sa.contains(&"NET".to_string()));
        assert!(!sa.contains(&"USB".to_string()));
        let st: Vec<String> = Series::St.inputs().into_iter().map(|i| i.id).collect();
        assert_eq!(st, vec!["DIG1", "DIG2", "DIG3", "DIG4", "NET_USB"]);
        assert!(Series::Pa.inputs().is_empty());
        // PHONO has no RC5 key on the 450 series, so it is readable but not selectable.
        let avr450: Vec<String> = Series::Avr450.inputs().into_iter().map(|i| i.id).collect();
        assert!(!avr450.contains(&"PHONO".to_string()));
    }

    #[test]
    fn amx_answers_parse() {
        let info = parse_amx_response(
            b"AMXB<Device-SDKClass=Receiver><Device-Make=ARCAM><Device-Model=SA30><Device-Revision=1.2.3>\r",
        )
        .unwrap();
        assert_eq!(info.make.as_deref(), Some("ARCAM"));
        assert_eq!(info.model.as_deref(), Some("SA30"));
        assert_eq!(info.revision.as_deref(), Some("1.2.3"));
        assert_eq!(parse_amx_response(b"nope"), None);
    }

    #[test]
    fn description_identity() {
        let xml = "<root><manufacturer>ARCAM</manufacturer><modelName>SA30</modelName>\
                   <friendlyName>Study</friendlyName>\
                   <UDN>uuid:5f9ec1b3-ff59-19bb-8530-00226b1c4d5e</UDN></root>";
        let description = parse_description(xml).unwrap();
        assert!(description.is_arcam());
        assert_eq!(description.hardware_id().as_deref(), Some("00226B1C4D5E"));
        let serial = Description {
            serial: Some(" SN123 ".to_string()),
            ..description.clone()
        };
        assert_eq!(serial.hardware_id().as_deref(), Some("SN123"));
        let other = parse_description("<manufacturer>Denon</manufacturer>").unwrap();
        assert!(!other.is_arcam());
        assert_eq!(other.hardware_id(), None);
    }

    #[test]
    fn state_queries_skip_what_the_series_lacks() {
        assert_eq!(state_queries(Some(Series::Pa)).len(), 2);
        assert_eq!(state_queries(Some(Series::Sa)).len(), 4);
        assert_eq!(state_queries(None).len(), 4);
    }
}
