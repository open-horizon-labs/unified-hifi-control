//! Trinnov Altitude wire formats: pure encode/parse, no I/O.
//!
//! Spec: the `trinnov-altitude` Python client (Apache-2.0) —
//! <https://github.com/binarylogic/py-trinnov-altitude> (`trinnov_altitude/protocol.py`,
//! `client.py`, `docs/PROTOCOL_ARCHITECTURE.md`). The protocol is plain text lines on TCP
//! 44100: commands end in `\n`, pushes end in `\n` (a trailing `\r` is stripped).
//!
//! Command strings used here, as the client sends them:
//!
//! | purpose            | line                                         |
//! |--------------------|----------------------------------------------|
//! | handshake          | `id <client>`                                |
//! | state query        | `get_current_state`                          |
//! | volume query       | `send volume`                                |
//! | profile query      | `get_current_profile`                        |
//! | absolute volume    | `volume <dB, one decimal>`                   |
//! | mute               | `mute 1` / `mute 0`                          |
//! | source (profile)   | `profile <n>`                                |
//! | acknowledged power-off | `power_off_SECURED_FHZMCH48FE` (answers `OK`) |
//!
//! `dvolume <delta>` exists but is **never** encoded here: the device service always sends an
//! absolute level, and a relative command replayed after a retry would stack.
//!
//! Every example in the tests is shaped from the client's parser rules; **synthetic**, not a
//! capture from an Altitude.

/// Default control port (`DEFAULT_PORT` in the client).
pub const DEFAULT_PORT: u16 = 44100;

/// Lowest and highest volume the client documents (`VOLUME_MIN`/`VOLUME_MAX`), in dB. The
/// maximum is the protocol's full scale; the user's ceiling only lowers it.
pub const VOLUME_MIN_DB: f32 = -120.0;
pub const VOLUME_SPEC_MAX_DB: f32 = 20.0;

/// The client identifier sent in the `id` handshake.
pub const CLIENT_ID: &str = "unified-hifi-control";

pub const POWER_OFF_COMMAND: &str = "power_off_SECURED_FHZMCH48FE";
pub const STATE_QUERY: &str = "get_current_state";
pub const VOLUME_QUERY: &str = "send volume";
pub const PROFILE_QUERY: &str = "get_current_profile";

/// Wake-on-LAN goes to UDP port 9 on the broadcast address.
pub const WOL_PORT: u16 = 9;

pub fn id_command(client: &str) -> String {
    format!("id {client}")
}

/// `volume <dB>`, rounded to one decimal as the client does. Refuses non-finite levels and
/// anything outside the documented range.
pub fn volume_command(db: f32) -> Option<String> {
    if !db.is_finite() || !(VOLUME_MIN_DB..=VOLUME_SPEC_MAX_DB).contains(&db) {
        return None;
    }
    Some(format!("volume {:.1}", (db * 10.0).round() / 10.0))
}

pub fn mute_command(muted: bool) -> String {
    format!("mute {}", u8::from(muted))
}

pub fn profile_command(index: u32) -> String {
    format!("profile {index}")
}

/// One line the processor sends.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// `Welcome on Trinnov Optimizer (Version <v>, ID <n>)`.
    Welcome {
        version: String,
        id: String,
    },
    Volume(f32),
    Mute(bool),
    /// `CURRENT_PROFILE <n>` or the bare `PROFILE <n>`.
    CurrentProfile(i32),
    /// `PROFILE <n>: <name>`: one catalog entry.
    Profile {
        index: i32,
        name: String,
    },
    ProfilesClear,
    /// `OK`: the acknowledgement of a command that is acknowledged.
    Ok,
    Error(String),
    Bye,
}

/// Parse one line (without its terminator). `None` for everything UHC does not read, which is
/// most of the protocol.
pub fn parse_line(line: &str) -> Option<Event> {
    let line = line.trim_end_matches(['\r', '\n']);
    if let Some(rest) = strip_prefix_ci(line, "Welcome on Trinnov Optimizer (Version ") {
        let (version, rest) = rest.split_once(", ID ")?;
        let id = rest.strip_suffix(')')?;
        if version.is_empty() || id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        return Some(Event::Welcome {
            version: version.to_string(),
            id: id.to_string(),
        });
    }
    if let Some(error) = strip_prefix_ci(line, "ERROR: ") {
        return Some(Event::Error(error.to_string()));
    }
    if line.eq_ignore_ascii_case("OK") {
        return Some(Event::Ok);
    }
    if line.eq_ignore_ascii_case("BYE") {
        return Some(Event::Bye);
    }
    if line.eq_ignore_ascii_case("PROFILES_CLEAR") {
        return Some(Event::ProfilesClear);
    }
    let (keyword, rest) = line.split_once(char::is_whitespace)?;
    match keyword.to_ascii_uppercase().as_str() {
        "VOLUME" => rest
            .trim()
            .parse::<f32>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Event::Volume),
        "MUTE" => match rest.trim() {
            "0" => Some(Event::Mute(false)),
            "1" => Some(Event::Mute(true)),
            _ => None,
        },
        "CURRENT_PROFILE" => rest.trim().parse().ok().map(Event::CurrentProfile),
        "PROFILE" => {
            let rest = rest.trim_start();
            match rest.split_once(": ") {
                Some((index, name)) => Some(Event::Profile {
                    index: index.trim().parse().ok()?,
                    name: name.to_string(),
                }),
                None => rest.trim().parse().ok().map(Event::CurrentProfile),
            }
        }
        _ => None,
    }
}

fn strip_prefix_ci<'a>(line: &'a str, prefix: &str) -> Option<&'a str> {
    let head = line.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &line[prefix.len()..])
}

/// Split complete lines off the front of `buffer`, leaving any partial line for the next read.
pub fn drain_lines(buffer: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(end) = buffer.find('\n') {
        let line: String = buffer.drain(..=end).collect();
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if !line.is_empty() {
            lines.push(line);
        }
    }
    lines
}

/// Parse a MAC (`aa:bb:cc:dd:ee:ff`, `aa-bb-...`, or bare hex).
pub fn parse_mac(mac: &str) -> Option<[u8; 6]> {
    let hex: String = mac
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | '.'))
        .collect();
    if hex.len() != 12 || !hex.is_ascii() {
        return None;
    }
    let mut bytes = [0u8; 6];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[index * 2..index * 2 + 2], 16).ok()?;
    }
    Some(bytes)
}

/// The Wake-on-LAN magic packet: six `0xFF` then the MAC sixteen times (102 bytes).
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut packet = [0xFFu8; 102];
    for repeat in 0..16 {
        packet[6 + repeat * 6..12 + repeat * 6].copy_from_slice(&mac);
    }
    packet
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn welcome_carries_version_and_id() {
        // protocol.py: r"^Welcome on Trinnov Optimizer \(Version (\S+), ID (\d+)\)$"
        assert_eq!(
            parse_line("Welcome on Trinnov Optimizer (Version 4.2.1, ID 1234567)"),
            Some(Event::Welcome {
                version: "4.2.1".into(),
                id: "1234567".into()
            })
        );
        assert_eq!(
            parse_line("Welcome on Trinnov Optimizer (Version 4.2.1, ID x)"),
            None
        );
    }

    #[test]
    fn push_events_parse() {
        assert_eq!(parse_line("VOLUME -35.5"), Some(Event::Volume(-35.5)));
        assert_eq!(parse_line("VOLUME 0"), Some(Event::Volume(0.0)));
        assert_eq!(parse_line("MUTE 1"), Some(Event::Mute(true)));
        assert_eq!(parse_line("MUTE 0"), Some(Event::Mute(false)));
        assert_eq!(
            parse_line("CURRENT_PROFILE 3"),
            Some(Event::CurrentProfile(3))
        );
        assert_eq!(parse_line("PROFILE 2"), Some(Event::CurrentProfile(2)));
        assert_eq!(
            parse_line("PROFILE 1: Apple TV"),
            Some(Event::Profile {
                index: 1,
                name: "Apple TV".into()
            })
        );
        assert_eq!(parse_line("PROFILES_CLEAR"), Some(Event::ProfilesClear));
        assert_eq!(parse_line("OK"), Some(Event::Ok));
        assert_eq!(parse_line("BYE"), Some(Event::Bye));
        assert_eq!(
            parse_line("ERROR: bad command"),
            Some(Event::Error("bad command".into()))
        );
    }

    #[test]
    fn keywords_match_case_insensitively_but_names_keep_their_case() {
        assert_eq!(parse_line("volume -20.0"), Some(Event::Volume(-20.0)));
        assert_eq!(
            parse_line("profile 4: Kaleidescape"),
            Some(Event::Profile {
                index: 4,
                name: "Kaleidescape".into()
            })
        );
    }

    #[test]
    fn unread_and_malformed_lines_are_ignored() {
        for line in [
            "",
            "DIM 0",
            "SRATE 48000",
            "MUTE 2",
            "MUTE",
            "VOLUME",
            "VOLUME loud",
            "VOLUME NaN",
            "CURRENT_PROFILE x",
            "PROFILE x: Name",
            "garbage line",
        ] {
            assert_eq!(parse_line(line), None, "{line:?}");
        }
    }

    #[test]
    fn crlf_is_stripped_and_partial_lines_wait() {
        let mut buffer = "VOLUME -30.0\r\nMUTE 1\nPROFIL".to_string();
        assert_eq!(drain_lines(&mut buffer), vec!["VOLUME -30.0", "MUTE 1"]);
        assert_eq!(buffer, "PROFIL");
        buffer.push_str("E 2\n");
        assert_eq!(drain_lines(&mut buffer), vec!["PROFILE 2"]);
        assert!(buffer.is_empty());
    }

    #[test]
    fn commands_encode_as_the_client_sends_them() {
        assert_eq!(id_command("uhc"), "id uhc");
        assert_eq!(volume_command(-30.0).unwrap(), "volume -30.0");
        assert_eq!(volume_command(-30.55).unwrap(), "volume -30.6");
        assert_eq!(mute_command(true), "mute 1");
        assert_eq!(mute_command(false), "mute 0");
        assert_eq!(profile_command(3), "profile 3");
        assert_eq!(POWER_OFF_COMMAND, "power_off_SECURED_FHZMCH48FE");
    }

    #[test]
    fn volume_is_refused_outside_the_documented_range() {
        assert_eq!(volume_command(VOLUME_MIN_DB).unwrap(), "volume -120.0");
        assert_eq!(volume_command(VOLUME_SPEC_MAX_DB).unwrap(), "volume 20.0");
        assert!(volume_command(-120.5).is_none());
        assert!(volume_command(20.5).is_none());
        assert!(volume_command(f32::NAN).is_none());
        assert!(volume_command(f32::NEG_INFINITY).is_none());
    }

    #[test]
    fn no_encoder_emits_a_relative_volume_command() {
        for command in [
            volume_command(-10.0).unwrap(),
            mute_command(true),
            profile_command(1),
        ] {
            assert!(!command.starts_with("dvolume"));
        }
    }

    #[test]
    fn mac_parsing_and_magic_packet() {
        let mac = parse_mac("00:1B:21:AA:BB:CC").unwrap();
        assert_eq!(mac, [0x00, 0x1B, 0x21, 0xAA, 0xBB, 0xCC]);
        assert_eq!(parse_mac("001b21aabbcc"), Some(mac));
        assert_eq!(parse_mac("00-1b-21-aa-bb-cc"), Some(mac));
        assert!(parse_mac("00:1B:21:AA:BB").is_none());
        assert!(parse_mac("zz:1B:21:AA:BB:CC").is_none());
        assert!(parse_mac("").is_none());

        let packet = magic_packet(mac);
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xFF; 6]);
        for repeat in 0..16 {
            assert_eq!(&packet[6 + repeat * 6..12 + repeat * 6], &mac);
        }
    }
}
