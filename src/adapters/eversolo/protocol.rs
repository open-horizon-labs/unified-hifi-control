//! Eversolo (Zidoo control-center) HTTP/JSON protocol: pure request construction and parsing
//! (#774). **Alpha.**
//!
//! # Sources actually read
//!
//! There is no vendor document for the state API. The vendor PDF
//! (<https://music.eversolo.com/dmp/instruction/Eversolo_DMP-A6_TCP_en_v1.0.pdf>) defines only the
//! remote-key GET. Everything else here is established from community clients, cross-checked
//! against each other (read with `gh api`, 2026-10-12):
//!
//! - <https://github.com/dmpa6-hacks/rest-api-reverse-engineering> `README.md` (iOS-app packet
//!   capture of a DMP-A6): `connect`, `getInputAndOutputList`, `setInputList`, `getPowerOption`,
//!   `setPowerOption`, port 9529, plain HTTP GET.
//! - <https://github.com/tomekceszke/eversolo-sdk> `src/eversolo/{_constants,_device,_models}.py`
//!   and `tests/conftest.py`: `getState` with `volumeData`, `setDevicesVolume`, `setMuteVolume`,
//!   `getModel`, JSON `status` codes (200, 405, 803, 804, 805), volume 0-200.
//! - <https://github.com/hchris1/Eversolo> `custom_components/eversolo/{api,coordinator,
//!   media_player}.py`: the same endpoints in a deployed HA integration; Wake-on-LAN for power-on
//!   using `net_mac` from `getModel`; volume as `currenttVolume / maxVolume`.
//! - <https://github.com/tmeynell/ha-eversolo> `custom_components/eversolo/{api,wake_on_lan,
//!   manifest.json}`, `docs/adr/0004-*.md`, `tests/fixtures/*` (real DMP-A8 Gen 2 captures): the
//!   fixtures in `tests/fixtures/devices/eversolo/`; WoL on UDP ports 9 and 9517 (the vendor app's
//!   `WakeThread.wakeOnLan` is the app's only power-on path); SSDP manifest matcher
//!   (`manufacturer: EVERSOLO`, `MediaRenderer:1`).
//! - <https://github.com/mase1981/uc-intg-eversolo> `uc_intg_eversolo/device.py`, `DISCOVERY_ANALYSIS.md`
//!   (A6 vs A10 input/output differences).
//! - <https://github.com/andrewdunndev/narjo-eversolo-upnp> `eversolo_descriptor.xml`,
//!   `ssdp_dump.txt` (SSDP advertisement of a DMP-A6).
//!
//! # Endpoint map (all `GET`, port 9529, JSON bodies)
//!
//! | Purpose | Path |
//! |---|---|
//! | State (volume, mute, device info) | `/ZidooMusicControl/v2/getState` |
//! | Inputs, current input index | `/ZidooMusicControl/v2/getInputAndOutputList` |
//! | Model, `net_mac`, `ableRemoteBoot` | `/ZidooControlCenter/getModel` (HA: `/ControlCenter/getModel`) |
//! | Absolute volume | `/ZidooMusicControl/v2/setDevicesVolume?volume=N` |
//! | Mute (explicit) | `/ZidooMusicControl/v2/setMuteVolume?isMute=1\|0` |
//! | Input select | `/ZidooMusicControl/v2/setInputList?tag=TAG&index=N` |
//! | Power off | `/ZidooMusicControl/v2/setPowerOption?tag=poweroff` (fallback: `Key.Poweroff`) |
//! | Power on | Wake-on-LAN magic packet (no HTTP path exists) |
//!
//! # The JSON `status` field is not trusted
//!
//! tmeynell/ha-eversolo ADR-0004 records endpoints that return HTTP 200 and do nothing, and
//! `status: 803` while succeeding. Writes are therefore judged by HTTP status only and proven by
//! readback. Reads require the field they need to be present.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use serde_json::Value;

/// Control port (vendor PDF "Base url"; every community client).
pub const DEFAULT_PORT: u16 = 9529;

pub const STATE_PATH: &str = "/ZidooMusicControl/v2/getState";
pub const INPUT_OUTPUT_PATH: &str = "/ZidooMusicControl/v2/getInputAndOutputList";
/// `getModel` exists under both prefixes in community clients; the Zidoo form first.
pub const MODEL_PATHS: [&str; 2] = ["/ZidooControlCenter/getModel", "/ControlCenter/getModel"];
/// Vendor PDF: "For example" uses the Zidoo form, "Base url" the short form.
pub const SENDKEY_PATHS: [&str; 2] = [
    "/ZidooControlCenter/RemoteControl/sendkey",
    "/ControlCenter/RemoteControl/sendkey",
];
/// Vendor PDF "Commands code table": `Key.Poweroff`.
pub const KEY_POWEROFF: &str = "Key.Poweroff";
/// `getPowerOption` tag list in the real capture: poweroff, reboot, standby, screen, timeshutdown.
pub const POWER_TAG_OFF: &str = "poweroff";

/// Wake-on-LAN UDP ports: the vendor app broadcasts to 9517; port 9 also wakes the unit
/// (tmeynell/ha-eversolo `wake_on_lan.py`, live-tested there).
pub const WOL_PORTS: [u16; 2] = [9, 9517];

// ---- Parsed shapes ------------------------------------------------------------------------

/// `getState.volumeData`, in the device's own native units.
#[derive(Debug, Clone, PartialEq)]
pub struct VolumeState {
    /// `currenttVolume` (sic).
    pub current: Option<i64>,
    pub min: i64,
    /// `maxVolume`; `None` when absent or not above `min`.
    pub max: Option<i64>,
    pub muted: Option<bool>,
    /// `isVolumeEnable` (defaults true when absent).
    pub enabled: bool,
    /// `isLock`.
    pub locked: bool,
    /// The device's own display string ("-45.5 dB" on a DMP-A8 Gen 2, "25" on a PLAY). Never
    /// interpreted.
    pub display: Option<String>,
}

impl VolumeState {
    /// Whether UHC may write a level: the device says volume is enabled and not locked.
    pub fn adjustable(&self) -> bool {
        self.enabled && !self.locked && self.current.is_some() && self.max.is_some()
    }
}

/// What one `getState` read yields.
#[derive(Debug, Clone, PartialEq)]
pub struct StateReading {
    pub volume: VolumeState,
    /// `deviceInfo.net_mac`.
    pub mac: Option<String>,
    /// `deviceInfo.model`.
    pub model: Option<String>,
}

/// `getModel`.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelInfo {
    pub model: String,
    pub name: Option<String>,
    pub net_mac: Option<String>,
    pub firmware: Option<String>,
    /// `ableRemoteBoot`: the unit can be woken remotely. Absent is treated as false.
    pub able_remote_boot: bool,
    /// `ableRemoteShutdown`: absent is treated as true (older firmware omits it).
    pub able_remote_shutdown: bool,
}

/// One entry of `inputData`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputEntry {
    /// `tag` ("XMOS", "BT", "SPDIF", "EARC", "RCA_A", ...): the stable code.
    pub tag: String,
    /// The user's name for it ("Record Player").
    pub name: String,
    /// `index`, sent back with the tag when selecting.
    pub index: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputList {
    pub inputs: Vec<InputEntry>,
    /// `inputIndex`.
    pub current_index: Option<i64>,
}

impl InputList {
    /// The tag of the selected input: the entry whose `index` equals `inputIndex`.
    pub fn current_tag(&self) -> Option<&str> {
        let wanted = self.current_index?;
        self.inputs
            .iter()
            .find(|input| input.index == wanted)
            .map(|input| input.tag.as_str())
    }

    pub fn find(&self, tag: &str) -> Option<&InputEntry> {
        self.inputs.iter().find(|input| input.tag == tag)
    }
}

/// UPnP descriptor facts used by discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub manufacturer: String,
    pub friendly_name: Option<String>,
    pub device_type: Option<String>,
}

impl Description {
    /// An Eversolo MediaRenderer. tmeynell/ha-eversolo `manifest.json` matches exactly this.
    pub fn is_eversolo(&self) -> bool {
        self.manufacturer.eq_ignore_ascii_case("EVERSOLO")
            && self
                .device_type
                .as_deref()
                .is_some_and(|t| t.contains("MediaRenderer"))
    }
}

// ---- Parsing ------------------------------------------------------------------------------

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_bool(value: &Value) -> Option<bool> {
    match value {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_i64().map(|i| i != 0),
        _ => None,
    }
}

fn text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn parse_object(body: &str) -> Result<Value, String> {
    let value: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    if value.is_object() {
        Ok(value)
    } else {
        Err("JSON body is not an object".to_string())
    }
}

/// Parse `getState`. `volumeData` must be present: without it nothing about the unit is known.
pub fn parse_state(body: &str) -> Result<StateReading, String> {
    let root = parse_object(body)?;
    let data = root
        .get("volumeData")
        .filter(|v| v.is_object())
        .ok_or("getState has no volumeData")?;
    let min = data.get("minVolume").and_then(as_i64).unwrap_or(0);
    let max = data
        .get("maxVolume")
        .and_then(as_i64)
        .filter(|max| *max > min);
    let info = root.get("deviceInfo");
    Ok(StateReading {
        volume: VolumeState {
            // "currenttVolume": the double t is the device's spelling.
            current: data.get("currenttVolume").and_then(as_i64),
            min,
            max,
            muted: data.get("isMute").and_then(as_bool),
            enabled: data.get("isVolumeEnable").and_then(as_bool).unwrap_or(true),
            locked: data.get("isLock").and_then(as_bool).unwrap_or(false),
            display: text(data.get("display")),
        },
        mac: text(info.and_then(|i| i.get("net_mac"))),
        model: text(info.and_then(|i| i.get("model"))),
    })
}

/// Parse `getModel`.
pub fn parse_model(body: &str) -> Result<ModelInfo, String> {
    let root = parse_object(body)?;
    let model = text(root.get("model")).ok_or("getModel has no model")?;
    Ok(ModelInfo {
        name: text(root.get("deviceName")).or_else(|| text(root.get("disModel"))),
        net_mac: text(root.get("net_mac")),
        firmware: text(root.get("firmware")),
        able_remote_boot: root
            .get("ableRemoteBoot")
            .and_then(as_bool)
            .unwrap_or(false),
        able_remote_shutdown: root
            .get("ableRemoteShutdown")
            .and_then(as_bool)
            .unwrap_or(true),
        model,
    })
}

/// Whether `tag` is safe to put in a query string and plausible as an input tag.
pub fn is_valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 32
        && tag
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Parse `getInputAndOutputList`. Entries with an unusable tag are dropped.
pub fn parse_input_list(body: &str) -> Result<InputList, String> {
    let root = parse_object(body)?;
    let data = root
        .get("inputData")
        .and_then(Value::as_array)
        .ok_or("getInputAndOutputList has no inputData")?;
    let inputs = data
        .iter()
        .enumerate()
        .filter_map(|(position, entry)| {
            let tag = text(entry.get("tag"))?;
            if !is_valid_tag(&tag) {
                return None;
            }
            Some(InputEntry {
                name: text(entry.get("name")).unwrap_or_else(|| tag.clone()),
                index: entry
                    .get("index")
                    .and_then(as_i64)
                    .unwrap_or(position as i64),
                tag,
            })
        })
        .collect();
    Ok(InputList {
        inputs,
        current_index: root.get("inputIndex").and_then(as_i64),
    })
}

/// Text of the first `<tag>` element, without namespace handling (the descriptor's root device
/// is the only one with these elements).
fn element_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    let value = xml[start..end].trim();
    (!value.is_empty()).then(|| value.to_string())
}

/// Parse a UPnP `description.xml`. `None` when there is no manufacturer.
pub fn parse_description(xml: &str) -> Option<Description> {
    Some(Description {
        manufacturer: element_text(xml, "manufacturer")?,
        friendly_name: element_text(xml, "friendlyName"),
        device_type: element_text(xml, "deviceType"),
    })
}

// ---- Request construction -----------------------------------------------------------------

/// `http://host:port<path_and_query>`. `None` for an unusable host.
pub fn endpoint_url(host: &str, port: u16, path_and_query: &str) -> Option<String> {
    if host.is_empty() || host.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    let authority = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Some(format!("http://{authority}{path_and_query}"))
}

pub fn set_volume_path(level: i64) -> String {
    format!("/ZidooMusicControl/v2/setDevicesVolume?volume={level}")
}

pub fn set_mute_path(muted: bool) -> String {
    format!(
        "/ZidooMusicControl/v2/setMuteVolume?isMute={}",
        u8::from(muted)
    )
}

/// `None` for a tag that is not safe to send.
pub fn set_input_path(tag: &str, index: i64) -> Option<String> {
    is_valid_tag(tag).then(|| format!("/ZidooMusicControl/v2/setInputList?tag={tag}&index={index}"))
}

pub fn set_power_path(tag: &str) -> Option<String> {
    is_valid_tag(tag).then(|| format!("/ZidooMusicControl/v2/setPowerOption?tag={tag}"))
}

/// A key is `Key.` followed by dot-separated alphanumerics, as every PDF table entry is.
pub fn is_valid_key(key: &str) -> bool {
    key.strip_prefix("Key.").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()))
    })
}

/// `<sendkey path>?key=KEY`. `None` for an invalid key.
pub fn sendkey_path(path: &str, key: &str) -> Option<String> {
    is_valid_key(key).then(|| format!("{path}?key={key}"))
}

/// Whether an HTTP status means a write was accepted (2xx). Acceptance is not confirmation.
pub fn status_accepted(status: u16) -> bool {
    (200..300).contains(&status)
}

// ---- Identity and Wake-on-LAN -------------------------------------------------------------

/// Hardware id from a MAC (12 hex digits, any common separator), upper-case.
pub fn hardware_id_from_mac(mac: &str) -> Option<String> {
    let digits: String = mac.chars().filter(char::is_ascii_alphanumeric).collect();
    (digits.len() == 12 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| digits.to_ascii_uppercase())
}

/// The standard 102-byte magic packet: 6 x 0xFF then the MAC 16 times.
pub fn magic_packet(mac: &str) -> Option<Vec<u8>> {
    let id = hardware_id_from_mac(mac)?;
    let bytes: Vec<u8> = (0..6)
        .map(|i| u8::from_str_radix(&id[i * 2..i * 2 + 2], 16))
        .collect::<Result<_, _>>()
        .ok()?;
    let mut packet = vec![0xFF; 6];
    for _ in 0..16 {
        packet.extend_from_slice(&bytes);
    }
    Some(packet)
}

/// Where to send the magic packet: the host's own /24 broadcast (what woke the unit in
/// tmeynell/ha-eversolo's live test) and the global broadcast (what the vendor app uses), on each
/// of [`WOL_PORTS`]. A host that is not an IPv4 literal has no subnet, so only the global one.
pub fn wol_targets(host: &str) -> Vec<SocketAddr> {
    let mut addresses = Vec::new();
    if let Ok(IpAddr::V4(v4)) = host.parse::<IpAddr>() {
        let o = v4.octets();
        addresses.push(Ipv4Addr::new(o[0], o[1], o[2], 255));
    }
    addresses.push(Ipv4Addr::BROADCAST);
    addresses.dedup();
    addresses
        .into_iter()
        .flat_map(|ip| WOL_PORTS.map(|port| SocketAddr::new(IpAddr::V4(ip), port)))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const STATE_A8: &str = include_str!("../../../tests/fixtures/devices/eversolo/getstate.json");
    const STATE_EARC: &str =
        include_str!("../../../tests/fixtures/devices/eversolo/getstate_earc.json");
    const MODEL_A8: &str = include_str!("../../../tests/fixtures/devices/eversolo/getmodel.json");
    const IO_A8: &str =
        include_str!("../../../tests/fixtures/devices/eversolo/getinputandoutputlist.json");
    const DESCRIPTION_A6: &str =
        include_str!("../../../tests/fixtures/devices/eversolo/description.xml");

    // Source: https://github.com/tomekceszke/eversolo-sdk tests/conftest.py STATE_DATA
    // (SDK test data for a PLAY on firmware v1.5.75, not a raw capture).
    const STATE_PLAY: &str = r#"{"state":3,"volumeData":{"currenttVolume":50,"minVolume":0,
        "maxVolume":200,"isMute":false,"display":"25","isLock":false,"isVolumeEnable":true}}"#;

    #[test]
    fn a8_state_volume_in_native_steps() {
        // tmeynell/ha-eversolo tests/fixtures/getstate_local_file.json (real DMP-A8 Gen 2).
        let reading = parse_state(STATE_A8).unwrap();
        let v = &reading.volume;
        assert_eq!(v.current, Some(109));
        assert_eq!((v.min, v.max), (0, Some(200)));
        assert_eq!(v.muted, Some(false));
        assert!(v.enabled && !v.locked && v.adjustable());
        assert_eq!(v.display.as_deref(), Some("-45.5 dB"));
        assert_eq!(reading.mac.as_deref(), Some("aa:bb:cc:00:00:01"));
        assert_eq!(reading.model.as_deref(), Some("DMP-A8 Gen 2"));
    }

    #[test]
    fn a8_display_matches_half_db_steps_on_both_captures() {
        // Observation only (two samples): dB = step / 2 - 100. The adapter does not rely on it.
        for (body, step, db) in [(STATE_A8, 109.0, -45.5), (STATE_EARC, 127.0, -36.5)] {
            let v = parse_state(body).unwrap().volume;
            assert_eq!(v.current, Some(step as i64));
            assert_eq!(step / 2.0 - 100.0, db);
            assert_eq!(v.display, Some(format!("{db:.1} dB")));
        }
    }

    #[test]
    fn play_state_uses_the_same_field_names() {
        let v = parse_state(STATE_PLAY).unwrap().volume;
        assert_eq!((v.current, v.min, v.max), (Some(50), 0, Some(200)));
        assert_eq!(v.display.as_deref(), Some("25"));
    }

    #[test]
    fn locked_or_disabled_volume_is_not_adjustable() {
        let locked = r#"{"volumeData":{"currenttVolume":10,"maxVolume":100,"isLock":true}}"#;
        assert!(!parse_state(locked).unwrap().volume.adjustable());
        let off = r#"{"volumeData":{"currenttVolume":10,"maxVolume":100,"isVolumeEnable":false}}"#;
        assert!(!parse_state(off).unwrap().volume.adjustable());
        let no_max = r#"{"volumeData":{"currenttVolume":10}}"#;
        assert!(!parse_state(no_max).unwrap().volume.adjustable());
    }

    #[test]
    fn malformed_state_is_refused() {
        for body in [
            "",
            "[]",
            "{}",
            r#"{"status":805}"#,
            r#"{"volumeData":3}"#,
            "nope",
        ] {
            assert!(parse_state(body).is_err(), "{body:?}");
        }
    }

    #[test]
    fn a8_model() {
        // tmeynell/ha-eversolo tests/fixtures/getmodel.json (real DMP-A8 Gen 2).
        let model = parse_model(MODEL_A8).unwrap();
        assert_eq!(model.model, "DMP-A8 Gen 2");
        assert_eq!(model.net_mac.as_deref(), Some("aa:bb:cc:00:00:01"));
        assert_eq!(model.firmware.as_deref(), Some("v1.1.50"));
        assert!(model.able_remote_boot && model.able_remote_shutdown);
        assert!(parse_model(r#"{"status":200}"#).is_err());
    }

    #[test]
    fn a8_inputs_keep_tags_and_user_names() {
        // tmeynell/ha-eversolo tests/fixtures/getinputandoutputlist.json: "Record Player" is the
        // user's rename of the SPDIF input; the tag is the stable id.
        let list = parse_input_list(IO_A8).unwrap();
        let tags: Vec<_> = list.inputs.iter().map(|i| i.tag.as_str()).collect();
        assert_eq!(tags, ["XMOS", "BT", "SPDIF", "EARC"]);
        assert_eq!(list.find("SPDIF").unwrap().name, "Record Player");
        assert_eq!(list.find("EARC").unwrap().index, 3);
        assert_eq!(list.current_index, Some(0));
        assert_eq!(list.current_tag(), Some("XMOS"));
        assert_eq!(list.find("USB"), None);
    }

    #[test]
    fn input_entries_with_unsendable_tags_are_dropped() {
        let body = r#"{"inputIndex":1,"inputData":[
            {"tag":"A/B","name":"x","index":0},{"tag":"RCA_A","name":"Line","index":1}]}"#;
        let list = parse_input_list(body).unwrap();
        assert_eq!(list.inputs.len(), 1);
        assert_eq!(list.current_tag(), Some("RCA_A"));
        assert!(parse_input_list("{}").is_err());
    }

    #[test]
    fn a6_upnp_description_is_recognised_as_eversolo() {
        // andrewdunndev/narjo-eversolo-upnp eversolo_descriptor.xml (real DMP-A6).
        let d = parse_description(DESCRIPTION_A6).unwrap();
        assert_eq!(d.manufacturer, "EVERSOLO");
        assert_eq!(d.friendly_name.as_deref(), Some("DMP-A6"));
        assert!(d.is_eversolo());
        let other = "<root><device><deviceType>urn:schemas-upnp-org:device:MediaRenderer:1\
            </deviceType><manufacturer>Sonos, Inc.</manufacturer></device></root>";
        assert!(!parse_description(other).unwrap().is_eversolo());
        assert_eq!(parse_description("<root/>"), None);
    }

    #[test]
    fn write_paths_match_the_community_clients() {
        // hchris1/Eversolo api.py async_set_volume / async_mute / async_unmute / async_set_input /
        // async_trigger_power_off.
        assert_eq!(
            set_volume_path(120),
            "/ZidooMusicControl/v2/setDevicesVolume?volume=120"
        );
        assert_eq!(
            set_mute_path(true),
            "/ZidooMusicControl/v2/setMuteVolume?isMute=1"
        );
        assert_eq!(
            set_mute_path(false),
            "/ZidooMusicControl/v2/setMuteVolume?isMute=0"
        );
        assert_eq!(
            set_input_path("SPDIF", 2).as_deref(),
            Some("/ZidooMusicControl/v2/setInputList?tag=SPDIF&index=2")
        );
        assert_eq!(
            set_power_path(POWER_TAG_OFF).as_deref(),
            Some("/ZidooMusicControl/v2/setPowerOption?tag=poweroff")
        );
    }

    #[test]
    fn injection_in_tags_and_keys_is_refused() {
        assert_eq!(set_input_path("X&tag=Y", 0), None);
        assert_eq!(set_input_path("", 0), None);
        assert_eq!(set_power_path("a b"), None);
        assert_eq!(
            sendkey_path(SENDKEY_PATHS[0], "Key.Power&key=Key.Reboot"),
            None
        );
        assert_eq!(
            sendkey_path(SENDKEY_PATHS[0], KEY_POWEROFF).as_deref(),
            Some("/ZidooControlCenter/RemoteControl/sendkey?key=Key.Poweroff")
        );
    }

    #[test]
    fn urls_bracket_ipv6_and_refuse_bad_hosts() {
        assert_eq!(
            endpoint_url("192.168.11.80", 9529, STATE_PATH).as_deref(),
            Some("http://192.168.11.80:9529/ZidooMusicControl/v2/getState")
        );
        assert!(endpoint_url("fe80::1", 9529, "/x")
            .unwrap()
            .starts_with("http://[fe80::1]:9529/"));
        assert_eq!(endpoint_url("", 9529, "/x"), None);
        assert_eq!(endpoint_url("a/b", 9529, "/x"), None);
    }

    #[test]
    fn only_2xx_is_acceptance() {
        assert!(status_accepted(200) && status_accepted(204));
        assert!(!status_accepted(404) && !status_accepted(500) && !status_accepted(301));
    }

    #[test]
    fn mac_identity_needs_twelve_hex_digits() {
        assert_eq!(
            hardware_id_from_mac("aa:bb:cc:00:11:22").as_deref(),
            Some("AABBCC001122")
        );
        assert_eq!(
            hardware_id_from_mac("AA-BB-CC-00-11-22").as_deref(),
            Some("AABBCC001122")
        );
        assert_eq!(hardware_id_from_mac("aa:bb:cc"), None);
        assert_eq!(hardware_id_from_mac("zz:bb:cc:00:11:22"), None);
    }

    #[test]
    fn magic_packet_layout() {
        let packet = magic_packet("aa:bb:cc:00:11:22").unwrap();
        assert_eq!(packet.len(), 102);
        assert_eq!(&packet[..6], &[0xFF; 6]);
        assert_eq!(&packet[6..12], &[0xAA, 0xBB, 0xCC, 0x00, 0x11, 0x22]);
        assert_eq!(&packet[96..], &[0xAA, 0xBB, 0xCC, 0x00, 0x11, 0x22]);
        assert_eq!(magic_packet("nope"), None);
    }

    #[test]
    fn wol_targets_cover_subnet_and_global_broadcast_on_both_ports() {
        let targets: Vec<String> = wol_targets("192.168.0.60")
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(
            targets,
            [
                "192.168.0.255:9",
                "192.168.0.255:9517",
                "255.255.255.255:9",
                "255.255.255.255:9517"
            ]
        );
        assert_eq!(wol_targets("eversolo.local").len(), 2);
    }
}
