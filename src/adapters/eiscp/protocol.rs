//! Pure eISCP wire formats: framing, message parsing, command encoding, discovery.
//!
//! Implemented from Onkyo's published protocol documentation only (never from a third-party
//! client's source):
//!
//! - Onkyo "ISCP Spec - Receivers: Integra Serial Communication Protocol for AV Receiver",
//!   version 1.15, 31 August 2009 (§1.2 "ISCP over Ethernet (eISCP)", the `PWR`, `AMT`, `MVL`
//!   and `SLI` command tables):
//!   <https://community.symcon.de/uploads/short-url/7mxbIQ7qRIghfbEQrvcrEkU57ad.pdf>
//!   (a later, longer-lived revision list is summarised at
//!   <https://en.everybodywiki.com/Integra_Serial_Control_Protocol>)
//! - Public description of eISCP framing and the UDP `ECNQSTN` discovery query/response
//!   (`model/port/region/identifier`):
//!   <https://sudonull.com/post/9179-Onkyos-ISCP-eISCP-protocol-control-of-Onkyo-devices-over-the-network>
//!
//! # Framing (spec §1.2)
//!
//! ```text
//! "ISCP" | header size (u32 BE, 0x10) | data size (u32 BE) | version 0x01 | reserved 00 00 00 | data
//! ```
//!
//! `data` is the ISCP message: `!` + unit type (`1` receiver) + three command characters +
//! parameter + end character (`CR`; receivers answer `EOF CR LF`).

use crate::devices::service::DeviceOperation;
use crate::devices::{DeviceInput, DevicePower};

pub const DEFAULT_PORT: u16 = 60128;
pub const HEADER_SIZE: u32 = 16;
/// Spec: "The Interval Time Receiving the message needs to take more than 50msec."
pub const MAX_DATA_SIZE: usize = 64 * 1024;
/// Highest `MVL` value the spec defines (`"00"-"64"`, 0-100 in hexadecimal). Some models stop at
/// `0x50` (0-80) and clamp a higher write themselves; the readback then reports the truth.
pub const MAX_PROTOCOL_VOLUME: u8 = 0x64;

/// Queries sent on connect and on each poll. Queries only: never a state-changing command.
pub const STATE_QUERIES: &[&str] = &["PWRQSTN", "MVLQSTN", "AMTQSTN", "SLIQSTN"];

/// Build one eISCP frame. `unit_type` is `'1'` for a receiver and `'x'` for the discovery query.
pub fn encode_frame(unit_type: char, message: &str) -> Vec<u8> {
    let mut data = Vec::with_capacity(message.len() + 3);
    data.push(b'!');
    data.push(unit_type as u8);
    data.extend_from_slice(message.as_bytes());
    data.push(b'\r');
    let mut frame = Vec::with_capacity(HEADER_SIZE as usize + data.len());
    frame.extend_from_slice(b"ISCP");
    frame.extend_from_slice(&HEADER_SIZE.to_be_bytes());
    frame.extend_from_slice(&(data.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]);
    frame.extend_from_slice(&data);
    frame
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameError {
    BadMagic,
    BadHeaderSize(u32),
    TooLarge(u32),
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic => f.write_str("not an eISCP frame (missing ISCP magic)"),
            Self::BadHeaderSize(size) => write!(f, "invalid eISCP header size {size}"),
            Self::TooLarge(size) => write!(f, "eISCP data size {size} exceeds the safety limit"),
        }
    }
}

/// Take the next complete frame's data out of `buffer`, if one has fully arrived.
///
/// An error means the stream is out of step (TCP has no resynchronisation point); the caller
/// must drop the session rather than guess.
pub fn next_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>, FrameError> {
    if buffer.len() >= 4 && &buffer[..4] != b"ISCP" {
        return Err(FrameError::BadMagic);
    }
    if buffer.len() < 16 {
        return Ok(None);
    }
    let header_size = u32::from_be_bytes([buffer[4], buffer[5], buffer[6], buffer[7]]);
    let data_size = u32::from_be_bytes([buffer[8], buffer[9], buffer[10], buffer[11]]);
    if !(HEADER_SIZE..=1024).contains(&header_size) {
        return Err(FrameError::BadHeaderSize(header_size));
    }
    if data_size as usize > MAX_DATA_SIZE {
        return Err(FrameError::TooLarge(data_size));
    }
    let total = header_size as usize + data_size as usize;
    if buffer.len() < total {
        return Ok(None);
    }
    let data = buffer[header_size as usize..total].to_vec();
    buffer.drain(..total);
    Ok(Some(data))
}

/// A frame's data as `(unit type, command + parameter)`, end characters removed.
pub fn split_message(data: &[u8]) -> Option<(char, String)> {
    let text = std::str::from_utf8(data).ok()?;
    let text = text.trim_end_matches(['\r', '\n', '\u{1a}']);
    let rest = text.strip_prefix('!')?;
    let mut chars = rest.chars();
    let unit = chars.next()?;
    Some((unit, chars.as_str().to_string()))
}

/// What a receiver can tell us about its main zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Power(bool),
    Mute(bool),
    /// Raw `MVL` units (hexadecimal on the wire).
    Volume(u8),
    /// `SLI` input code, two upper-case hex digits.
    Input(String),
}

/// Parse a status message (`PWR01`, `MVL28`, ...). Unknown commands and `N/A` parameters are
/// `None`: a receiver sends plenty we do not model.
pub fn parse_message(message: &str) -> Option<Event> {
    if message.len() < 5 || !message.is_ascii() {
        return None;
    }
    let (command, parameter) = message.split_at(3);
    match command {
        "PWR" => match parameter {
            "01" => Some(Event::Power(true)),
            "00" => Some(Event::Power(false)),
            _ => None,
        },
        "AMT" => match parameter {
            "01" => Some(Event::Mute(true)),
            "00" => Some(Event::Mute(false)),
            _ => None,
        },
        "MVL" => {
            if parameter.len() != 2 || !parameter.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            u8::from_str_radix(parameter, 16).ok().map(Event::Volume)
        }
        "SLI" => {
            if parameter.len() != 2 || !parameter.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            Some(Event::Input(parameter.to_ascii_uppercase()))
        }
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

/// Encode an operation as `(command, query that reads it back)`. Levels arrive already clamped
/// and quantised; this only refuses what cannot be written.
pub fn encode_operation(operation: &DeviceOperation) -> Result<(String, &'static str), String> {
    match operation {
        DeviceOperation::SetVolume(level) => {
            if !level.is_finite() || *level < 0.0 || *level > f32::from(MAX_PROTOCOL_VOLUME) {
                return Err(format!(
                    "volume {level} is outside the eISCP range 0-{MAX_PROTOCOL_VOLUME}"
                ));
            }
            Ok((format!("MVL{:02X}", level.round() as u8), "MVLQSTN"))
        }
        DeviceOperation::SetMute(muted) => Ok((
            format!("AMT{}", if *muted { "01" } else { "00" }),
            "AMTQSTN",
        )),
        DeviceOperation::PowerOn => Ok(("PWR01".to_string(), "PWRQSTN")),
        DeviceOperation::PowerStandby => Ok(("PWR00".to_string(), "PWRQSTN")),
        DeviceOperation::SelectInput(code) => {
            if code.len() != 2 || !code.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("{code:?} is not an eISCP input code"));
            }
            Ok((format!("SLI{}", code.to_ascii_uppercase()), "SLIQSTN"))
        }
    }
}

/// Input selector codes from the spec's `SLI` table (v1.15). The receiver offers no catalog
/// query, so which of these a given model actually has is unknown: they are all listed as
/// available and the receiver refuses the ones it lacks.
pub const INPUT_CODES: &[(&str, &str)] = &[
    ("00", "VIDEO1 VCR/DVR"),
    ("01", "VIDEO2 CBL/SAT"),
    ("02", "VIDEO3 GAME"),
    ("03", "VIDEO4 AUX1"),
    ("04", "VIDEO5 AUX2"),
    ("10", "DVD"),
    ("20", "TV/TAPE"),
    ("22", "PHONO"),
    ("23", "CD"),
    ("24", "FM"),
    ("25", "AM"),
    ("26", "TUNER"),
    ("27", "MUSIC SERVER"),
    ("28", "INTERNET RADIO"),
    ("29", "USB (front)"),
    ("2A", "USB (rear)"),
    ("30", "MULTI CH"),
    ("40", "UNIVERSAL PORT"),
];

pub fn input_catalog(current: Option<&str>) -> Vec<DeviceInput> {
    let mut inputs: Vec<DeviceInput> = INPUT_CODES
        .iter()
        .map(|(id, name)| DeviceInput {
            id: (*id).to_string(),
            name: (*name).to_string(),
            available: true,
        })
        .collect();
    // A code the table does not know (newer firmware) is still what the unit is on.
    if let Some(current) = current {
        if !inputs.iter().any(|input| input.id == current) {
            inputs.push(DeviceInput {
                id: current.to_string(),
                name: format!("Input {current}"),
                available: true,
            });
        }
    }
    inputs
}

/// The discovery query: a UDP broadcast frame whose unit type is `x` (any), message `ECNQSTN`.
pub fn discovery_request() -> Vec<u8> {
    encode_frame('x', "ECNQSTN")
}

/// A receiver's answer to `ECNQSTN`: `!1ECN<model>/<port>/<region>/<identifier>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Discovery {
    pub model: String,
    pub port: u16,
    pub region: String,
    /// The receiver's MAC address, twelve hex digits. The hardware id.
    pub identifier: String,
}

impl Discovery {
    /// Vendor as far as the model name says: Integra and Pioneer ship the same protocol under
    /// their own prefixes. Display only; identity never depends on it.
    pub fn vendor(&self) -> &'static str {
        let model = self.model.to_ascii_uppercase();
        if ["DRX", "DTR", "DTM", "DHC", "DBS", "DRC"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            "Integra"
        } else if ["VSX", "SC-", "SX-", "LX"]
            .iter()
            .any(|prefix| model.starts_with(prefix))
        {
            "Pioneer"
        } else {
            "Onkyo"
        }
    }
}

/// Parse a whole UDP datagram (header included) answering `ECNQSTN`.
pub fn parse_discovery_response(datagram: &[u8]) -> Option<Discovery> {
    let mut buffer = datagram.to_vec();
    let data = next_frame(&mut buffer).ok()??;
    let (_unit, message) = split_message(&data)?;
    let body = message.strip_prefix("ECN")?;
    let mut parts = body.split('/');
    let model = parts.next()?.trim();
    let port: u16 = parts.next()?.trim().parse().ok()?;
    let region = parts.next()?.trim();
    let identifier = parts.next()?.trim();
    if parts.next().is_some()
        || model.is_empty()
        || port == 0
        || region.is_empty()
        || identifier.len() != 12
        || !identifier.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return None;
    }
    Some(Discovery {
        model: model.to_string(),
        port,
        region: region.to_string(),
        identifier: identifier.to_ascii_uppercase(),
    })
}

/// The identity facts from a UPnP device description (SSDP discovery path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub manufacturer: String,
    pub model: String,
    pub friendly_name: String,
}

impl Description {
    /// True for the manufacturers whose receivers speak eISCP (catalog: ONKYO,
    /// "Onkyo & Pioneer Corporation", Pioneer, and Integra's own spelling).
    pub fn is_eiscp_vendor(&self) -> bool {
        let manufacturer = self.manufacturer.trim().to_ascii_lowercase();
        manufacturer.contains("onkyo")
            || manufacturer.contains("pioneer")
            || manufacturer.contains("integra")
    }
}

pub fn parse_description(xml: &str) -> Option<Description> {
    let text = |tag: &str| element_text(xml, tag).map(|value| value.trim().to_string());
    Some(Description {
        manufacturer: text("manufacturer")?,
        model: text("modelName").unwrap_or_default(),
        friendly_name: text("friendlyName").unwrap_or_default(),
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

    /// Spec v1.15 §1.2: header `ISCP`, size 0x10, data size, version 1, reserved 0; data is
    /// `!1PWR01\r` for "Power On".
    #[test]
    fn power_on_frame_matches_the_spec_layout() {
        let frame = encode_frame('1', "PWR01");
        let mut expected = b"ISCP".to_vec();
        expected.extend_from_slice(&[0, 0, 0, 0x10, 0, 0, 0, 8, 1, 0, 0, 0]);
        expected.extend_from_slice(b"!1PWR01\r");
        assert_eq!(frame, expected);
    }

    #[test]
    fn frames_round_trip_and_split_across_reads() {
        let frame = encode_frame('1', "MVL28");
        let mut buffer = frame[..10].to_vec();
        assert_eq!(next_frame(&mut buffer), Ok(None));
        buffer.extend_from_slice(&frame[10..frame.len() - 2]);
        assert_eq!(next_frame(&mut buffer), Ok(None));
        buffer.extend_from_slice(&frame[frame.len() - 2..]);
        // A second frame glued on must stay in the buffer.
        buffer.extend_from_slice(&encode_frame('1', "AMT01"));
        let data = next_frame(&mut buffer).unwrap().unwrap();
        assert_eq!(split_message(&data), Some(('1', "MVL28".to_string())));
        let data = next_frame(&mut buffer).unwrap().unwrap();
        assert_eq!(split_message(&data), Some(('1', "AMT01".to_string())));
        assert!(buffer.is_empty());
    }

    #[test]
    fn receiver_end_characters_are_stripped() {
        // Spec §1.2: receivers end with [EOF][CR][LF].
        assert_eq!(
            split_message(b"!1SSTT00\x1a\r\n"),
            Some(('1', "SSTT00".to_string()))
        );
        assert_eq!(split_message(b"1PWR01\r"), None);
    }

    #[test]
    fn malformed_frames_are_refused() {
        let mut buffer = b"HTTP/1.1 200 OK".to_vec();
        assert_eq!(next_frame(&mut buffer), Err(FrameError::BadMagic));
        let mut bad_header = encode_frame('1', "PWR01");
        bad_header[7] = 4;
        assert_eq!(
            next_frame(&mut bad_header),
            Err(FrameError::BadHeaderSize(4))
        );
        let mut huge = encode_frame('1', "PWR01");
        huge[8] = 0x7f;
        assert!(matches!(
            next_frame(&mut huge),
            Err(FrameError::TooLarge(_))
        ));
    }

    #[test]
    fn status_messages_parse() {
        assert_eq!(parse_message("PWR01"), Some(Event::Power(true)));
        assert_eq!(parse_message("PWR00"), Some(Event::Power(false)));
        assert_eq!(parse_message("AMT01"), Some(Event::Mute(true)));
        assert_eq!(parse_message("AMT00"), Some(Event::Mute(false)));
        // Spec: MVL is hexadecimal; "28" is 40, "64" is the 0-100 maximum.
        assert_eq!(parse_message("MVL28"), Some(Event::Volume(40)));
        assert_eq!(parse_message("MVL64"), Some(Event::Volume(100)));
        assert_eq!(parse_message("MVL00"), Some(Event::Volume(0)));
        assert_eq!(parse_message("SLI2a"), Some(Event::Input("2A".to_string())));
    }

    #[test]
    fn unknown_or_malformed_status_is_ignored() {
        assert_eq!(parse_message("MVLN/A"), None);
        assert_eq!(parse_message("MVL2"), None);
        assert_eq!(parse_message("MVLZZ"), None);
        assert_eq!(parse_message("PWR02"), None);
        assert_eq!(parse_message("NTI Some title"), None);
        assert_eq!(parse_message(""), None);
    }

    #[test]
    fn operations_encode_per_the_command_tables() {
        let encode = |op| encode_operation(&op).unwrap();
        assert_eq!(
            encode(DeviceOperation::SetVolume(40.0)),
            ("MVL28".to_string(), "MVLQSTN")
        );
        assert_eq!(
            encode(DeviceOperation::SetVolume(0.0)),
            ("MVL00".to_string(), "MVLQSTN")
        );
        assert_eq!(
            encode(DeviceOperation::SetVolume(100.0)),
            ("MVL64".to_string(), "MVLQSTN")
        );
        assert_eq!(encode(DeviceOperation::SetMute(true)).0, "AMT01");
        assert_eq!(encode(DeviceOperation::SetMute(false)).0, "AMT00");
        assert_eq!(encode(DeviceOperation::PowerOn).0, "PWR01");
        assert_eq!(encode(DeviceOperation::PowerStandby).0, "PWR00");
        assert_eq!(
            encode(DeviceOperation::SelectInput("2a".to_string())).0,
            "SLI2A"
        );
    }

    #[test]
    fn out_of_range_volume_and_bad_input_codes_are_refused() {
        assert!(encode_operation(&DeviceOperation::SetVolume(101.0)).is_err());
        assert!(encode_operation(&DeviceOperation::SetVolume(-1.0)).is_err());
        assert!(encode_operation(&DeviceOperation::SetVolume(f32::NAN)).is_err());
        assert!(encode_operation(&DeviceOperation::SelectInput("ZZ".to_string())).is_err());
        assert!(encode_operation(&DeviceOperation::SelectInput("123".to_string())).is_err());
    }

    #[test]
    fn discovery_request_is_an_x_unit_ecn_query() {
        let request = discovery_request();
        assert_eq!(&request[..4], b"ISCP");
        assert_eq!(&request[16..], b"!xECNQSTN\r");
    }

    /// Shape per the public description of the `ECN` answer (model/port/region/identifier).
    #[test]
    fn discovery_response_parses() {
        let mut datagram = encode_frame('1', "ECNTX-NR709/60128/DX/0009B04448E0");
        let found = parse_discovery_response(&datagram).unwrap();
        assert_eq!(found.model, "TX-NR709");
        assert_eq!(found.port, 60128);
        assert_eq!(found.region, "DX");
        assert_eq!(found.identifier, "0009B04448E0");
        assert_eq!(found.vendor(), "Onkyo");
        datagram = encode_frame('1', "ECNVSX-LX303/60128/XX/0009b04448e1");
        let found = parse_discovery_response(&datagram).unwrap();
        assert_eq!(found.vendor(), "Pioneer");
        assert_eq!(found.identifier, "0009B04448E1");
    }

    #[test]
    fn malformed_discovery_responses_are_refused() {
        for message in [
            "ECNTX-NR709/60128/DX",
            "ECNTX-NR709/notaport/DX/0009B04448E0",
            "ECNTX-NR709/60128/DX/0009B04448",
            "ECN/60128/DX/0009B04448E0",
            "ECNTX-NR709/60128/DX/0009B04448E0/extra",
            "PWR01",
        ] {
            assert_eq!(
                parse_discovery_response(&encode_frame('1', message)),
                None,
                "{message}"
            );
        }
        assert_eq!(parse_discovery_response(b"garbage"), None);
    }

    #[test]
    fn description_matches_onkyo_pioneer_and_integra_only() {
        let xml = "<root><manufacturer>Onkyo &amp; Pioneer Corporation</manufacturer>\
                   <modelName>TX-NR686</modelName><friendlyName>Den</friendlyName></root>";
        let description = parse_description(xml).unwrap();
        assert!(description.is_eiscp_vendor());
        assert_eq!(description.model, "TX-NR686");
        let other = parse_description("<manufacturer>Sonos, Inc.</manufacturer>").unwrap();
        assert!(!other.is_eiscp_vendor());
        assert!(parse_description("<root/>").is_none());
    }

    #[test]
    fn input_catalog_keeps_an_unknown_current_code() {
        let catalog = input_catalog(Some("2E"));
        assert!(catalog
            .iter()
            .any(|input| input.id == "23" && input.name == "CD"));
        assert!(catalog.iter().any(|input| input.id == "2E"));
        assert_eq!(input_catalog(Some("23")).len(), INPUT_CODES.len());
    }
}
