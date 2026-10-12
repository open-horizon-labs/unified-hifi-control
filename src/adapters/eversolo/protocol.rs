//! Eversolo HTTP key protocol: pure request construction and tables (#774). **Alpha.**
//!
//! # Source
//!
//! The vendor document `Eversolo_DMP-A6_TCP_en_v1.0.pdf`
//! (<https://music.eversolo.com/dmp/instruction/Eversolo_DMP-A6_TCP_en_v1.0.pdf>, 3 pages, v1.0).
//! Section names below are the document's own headings: "Terminology", "Base url", "For example",
//! "Commands code table".
//!
//! # What the document defines
//!
//! - The "TCP" protocol is **HTTP** ("Terminology": TCP = HTTP) on port **9529**, a GET of
//!   `http://IP:9529/ControlCenter/RemoteControl/sendkey?key=KEY` ("Base url").
//! - A fixed table of key codes ("Commands code table").
//!
//! # What the document does NOT define
//!
//! No state query, no response body, no discovery mechanism, no device identity, no absolute
//! volume, no volume scale/range/step, no mute state, no power-on key. See `mod.rs` for how the
//! adapter degrades honestly around each gap.
//!
//! # Ambiguity: the URL path
//!
//! "Base url" gives `/ControlCenter/RemoteControl/sendkey`; the worked example in "For example"
//! gives `/ZidooControlCenter/RemoteControl/sendkey`. The document does not reconcile them.
//! [`SENDKEY_PATHS`] lists both, example form first, and the adapter falls back on HTTP 404.

use crate::devices::DeviceInput;

/// Control port ("Base url": `http://IP:9529/...`).
pub const DEFAULT_PORT: u16 = 9529;

/// "For example" form, then "Base url" form. See the module docs.
pub const SENDKEY_PATHS: [&str; 2] = [
    "/ZidooControlCenter/RemoteControl/sendkey",
    "/ControlCenter/RemoteControl/sendkey",
];

// ---- Commands code table ------------------------------------------------------------------

/// "Key.Poweroff": Power off.
pub const KEY_POWEROFF: &str = "Key.Poweroff";
/// "Key.Reboot": Reboot. Documented, never sent by this adapter.
pub const KEY_REBOOT: &str = "Key.Reboot";
/// "Key.DAC.Switch": Loop switching input source. A cycle, not a selection; never sent.
pub const KEY_DAC_SWITCH: &str = "Key.DAC.Switch";
/// "Key.VolumeUp": Volume up. Documented; unusable without a readable level, never sent.
pub const KEY_VOLUME_UP: &str = "Key.VolumeUp";
/// "Key.VolumeDown": Volume down. Documented; unusable without a readable level, never sent.
pub const KEY_VOLUME_DOWN: &str = "Key.VolumeDown";
/// "Key.Mute": Mute. A toggle with no readable state, so it cannot implement `SetMute(bool)`.
pub const KEY_MUTE: &str = "Key.Mute";

/// "Commands code table", input rows: (input code, key, display name as the table quotes it).
/// The input code is the key's suffix after `Key.DAC.`; it is the stable [`DeviceInput::id`].
const INPUT_TABLE: [(&str, &str, &str); 5] = [
    // Key.DAC.XMOS: Switch input source to "Internal player"
    ("XMOS", "Key.DAC.XMOS", "Internal player"),
    // Key.DAC.BT: Switch input source to "Bluetooth In"
    ("BT", "Key.DAC.BT", "Bluetooth In"),
    // Key.DAC.USB: Switch input source to "USB-C In"
    ("USB", "Key.DAC.USB", "USB-C In"),
    // Key.DAC.SPDIF: Switch input source to "Optical In"
    ("SPDIF", "Key.DAC.SPDIF", "Optical In"),
    // Key.DAC.COA: Switch input source to "Coaxial In"
    ("COA", "Key.DAC.COA", "Coaxial In"),
];

/// The selectable inputs the table documents.
pub fn inputs() -> Vec<DeviceInput> {
    INPUT_TABLE
        .iter()
        .map(|(id, _, name)| DeviceInput {
            id: (*id).to_string(),
            name: (*name).to_string(),
            available: true,
        })
        .collect()
}

/// The key that selects an input code, or `None` for a code the table does not list.
pub fn input_key(code: &str) -> Option<&'static str> {
    INPUT_TABLE
        .iter()
        .find(|(id, _, _)| *id == code)
        .map(|(_, key, _)| *key)
}

// ---- Request construction -----------------------------------------------------------------

/// A key is `Key.` followed by dot-separated alphanumerics, as every table entry is. Anything
/// else is refused so a caller-supplied string can never smuggle a query parameter or path.
pub fn is_valid_key(key: &str) -> bool {
    key.strip_prefix("Key.").is_some_and(|rest| {
        !rest.is_empty()
            && rest
                .split('.')
                .all(|part| !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric()))
    })
}

/// `http://host:port<path>?key=<key>` ("Base url"). `None` for an invalid key or host.
pub fn sendkey_url(host: &str, port: u16, path: &str, key: &str) -> Option<String> {
    if !is_valid_key(key) || host.is_empty() || host.contains(['/', '?', '#', '@', ' ']) {
        return None;
    }
    // An IPv6 literal needs brackets in a URL authority.
    let authority = if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    };
    Some(format!("http://{authority}{path}?key={key}"))
}

/// Whether an HTTP status means the key was accepted. The document defines no response body, so
/// only the status is read; a 2xx is acceptance, not confirmation of the resulting state.
pub fn status_accepted(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Hardware id from a configured MAC. The document defines no identity query, so a configured
/// MAC is the only stable identity there is. Needs 12 hex digits (any common separator).
pub fn hardware_id_from_mac(mac: &str) -> Option<String> {
    let digits: String = mac.chars().filter(char::is_ascii_alphanumeric).collect();
    (digits.len() == 12 && digits.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| digits.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn example_url_matches_the_documents_for_example_section() {
        // "For example": http://192.168.11.80:9529/ZidooControlCenter/RemoteControl/sendkey?key=Key.Poweroff
        assert_eq!(
            sendkey_url("192.168.11.80", 9529, SENDKEY_PATHS[0], KEY_POWEROFF).as_deref(),
            Some("http://192.168.11.80:9529/ZidooControlCenter/RemoteControl/sendkey?key=Key.Poweroff")
        );
    }

    #[test]
    fn base_url_form_matches_the_documents_base_url_section() {
        // "Base url": http://IP:9529/ControlCenter/RemoteControl/sendkey?key=KEY
        assert_eq!(
            sendkey_url("10.0.0.5", DEFAULT_PORT, SENDKEY_PATHS[1], "Key.DAC.USB").as_deref(),
            Some("http://10.0.0.5:9529/ControlCenter/RemoteControl/sendkey?key=Key.DAC.USB")
        );
    }

    #[test]
    fn ipv6_hosts_are_bracketed() {
        let url = sendkey_url("fe80::1", 9529, SENDKEY_PATHS[1], KEY_MUTE).unwrap();
        assert!(url.starts_with("http://[fe80::1]:9529/"));
    }

    #[test]
    fn every_input_row_in_the_table_maps_to_its_key() {
        // "Commands code table", rows Key.DAC.XMOS / BT / USB / SPDIF / COA.
        let expected = [
            ("XMOS", "Key.DAC.XMOS", "Internal player"),
            ("BT", "Key.DAC.BT", "Bluetooth In"),
            ("USB", "Key.DAC.USB", "USB-C In"),
            ("SPDIF", "Key.DAC.SPDIF", "Optical In"),
            ("COA", "Key.DAC.COA", "Coaxial In"),
        ];
        let inputs = inputs();
        assert_eq!(inputs.len(), expected.len());
        for ((code, key, name), input) in expected.iter().zip(&inputs) {
            assert_eq!(&input.id, code);
            assert_eq!(&input.name, name);
            assert!(input.available);
            assert_eq!(input_key(code), Some(*key));
        }
    }

    #[test]
    fn unlisted_input_codes_are_refused() {
        assert_eq!(input_key("HDMI"), None);
        assert_eq!(input_key("Key.DAC.USB"), None);
        assert_eq!(input_key(""), None);
        // Output-port keys (Key.OUT.*) are not inputs.
        assert_eq!(input_key("XLR"), None);
    }

    #[test]
    fn every_documented_key_is_valid() {
        for key in [
            "Key.Poweroff",
            "Key.Reboot",
            "Key.DAC.Switch",
            "Key.DAC.XMOS",
            "Key.OUT.XLRRCA",
            "Key.VolumeUp",
            "Key.VolumeDown",
            "Key.Mute",
            "Key.MediaPlay.Pause",
            "Key.Screen.ON",
        ] {
            assert!(is_valid_key(key), "{key}");
        }
    }

    #[test]
    fn malformed_keys_and_hosts_are_refused() {
        for key in [
            "",
            "Poweroff",
            "Key.",
            "Key..USB",
            "Key.Power&key=Key.Reboot",
            "Key.A b",
            "Key.A/../B",
        ] {
            assert!(!is_valid_key(key), "{key:?}");
            assert_eq!(sendkey_url("10.0.0.5", 9529, SENDKEY_PATHS[0], key), None);
        }
        assert_eq!(sendkey_url("", 9529, SENDKEY_PATHS[0], KEY_MUTE), None);
        assert_eq!(
            sendkey_url("10.0.0.5/x", 9529, SENDKEY_PATHS[0], KEY_MUTE),
            None
        );
    }

    #[test]
    fn only_2xx_is_acceptance() {
        assert!(status_accepted(200));
        assert!(status_accepted(204));
        assert!(!status_accepted(199));
        assert!(!status_accepted(301));
        assert!(!status_accepted(404));
        assert!(!status_accepted(500));
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
        assert_eq!(hardware_id_from_mac(""), None);
    }
}
