//! What each controller lets its owner configure from UHC.
//!
//! One table, shared by the server (which rejects settings a controller does not have) and the
//! settings page (which renders only those). A setting appears here only when the firmware
//! applies it; stored-but-ignored fields are deliberately absent so the page never offers a
//! control that does nothing.

use crate::firmware_catalog::FirmwareTarget;

/// How a setting is edited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingKind {
    Toggle,
    /// Whole numbers in `min..=max`.
    Number {
        min: i64,
        max: i64,
        unit: &'static str,
    },
    /// One of a closed list of `(value, label)` pairs.
    Choice(&'static [(i64, &'static str)]),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettingSpec {
    /// Key in the controller's `config` object (and in `PUT /config/{id}`).
    pub key: &'static str,
    pub label: &'static str,
    pub help: &'static str,
    pub kind: SettingKind,
}

/// Which of the shared display and power settings (rotation, art, dim, sleep, ...) the firmware
/// acts on. The bridge stores them for every controller; only some drivers use them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerSupport {
    Full,
    /// Only the album-art timeout (Slate has no battery or charge detection).
    ArtOnly,
    None,
}

/// Every device-specific key the bridge knows. Anything else is not a setting.
pub const ALL_KEYS: &[&str] = &[
    "show_ip",
    "brightness",
    "body_enabled",
    "sound_enabled",
    "sound_volume",
];

const BRIGHTNESS: SettingSpec = SettingSpec {
    key: "brightness",
    label: "Brightness",
    help: "Backlight level. The lowest allowed value stays readable so the setting can be undone.",
    kind: SettingKind::Number {
        min: 5,
        max: 100,
        unit: "%",
    },
};

const SHOW_IP: SettingSpec = SettingSpec {
    key: "show_ip",
    label: "Show IP address",
    help: "Print the controller's address on the display so its own web page is easy to find.",
    kind: SettingKind::Toggle,
};

const STACKCHAN: &[SettingSpec] = &[
    SettingSpec {
        key: "body_enabled",
        label: "Body language",
        help: "Let Kizz move while it plays and listens.",
        kind: SettingKind::Toggle,
    },
    SettingSpec {
        key: "sound_enabled",
        label: "Sounds",
        help: "Play Kizz's confirmation and expression sounds.",
        kind: SettingKind::Toggle,
    },
    SettingSpec {
        key: "sound_volume",
        label: "Sound volume",
        help: "Loudness of Kizz's own sounds, not of the music.",
        kind: SettingKind::Choice(&[(0, "Low"), (1, "Medium"), (2, "High")]),
    },
];

/// Settings the controller has beyond the shared display and power group.
pub fn device_settings(target: FirmwareTarget) -> &'static [SettingSpec] {
    use FirmwareTarget::*;
    match target {
        Frame => &[SHOW_IP],
        Dial | LegacyKnob | Halo => &[BRIGHTNESS],
        Stackchan => STACKCHAN,
        Rlcd | Joy | Tough | M5Dial | StickS3 | Stopwatch => &[],
    }
}

pub fn power_support(target: FirmwareTarget) -> PowerSupport {
    use FirmwareTarget::*;
    match target {
        Frame | Halo => PowerSupport::None,
        Rlcd => PowerSupport::ArtOnly,
        Dial | LegacyKnob | Joy | Tough | M5Dial | StickS3 | Stopwatch | Stackchan => {
            PowerSupport::Full
        }
    }
}

fn spec(target: FirmwareTarget, key: &str) -> Result<&'static SettingSpec, String> {
    device_settings(target)
        .iter()
        .find(|setting| setting.key == key)
        .ok_or_else(|| format!("{} has no \"{key}\" setting", target.name()))
}

pub fn check_toggle(target: FirmwareTarget, key: &str) -> Result<(), String> {
    match spec(target, key)?.kind {
        SettingKind::Toggle => Ok(()),
        _ => Err(format!("\"{key}\" is not a toggle")),
    }
}

pub fn check_number(target: FirmwareTarget, key: &str, value: i64) -> Result<(), String> {
    match spec(target, key)?.kind {
        SettingKind::Number { min, max, .. } if (min..=max).contains(&value) => Ok(()),
        SettingKind::Number { min, max, .. } => {
            Err(format!("\"{key}\" must be between {min} and {max}"))
        }
        SettingKind::Choice(options) if options.iter().any(|(option, _)| *option == value) => {
            Ok(())
        }
        SettingKind::Choice(_) => Err(format!("\"{key}\" is not one of the offered choices")),
        SettingKind::Toggle => Err(format!("\"{key}\" is not a number")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::firmware_catalog::FirmwareTarget;

    #[test]
    fn each_controller_offers_only_settings_its_firmware_applies() {
        let keys = |target| {
            device_settings(target)
                .iter()
                .map(|setting| setting.key)
                .collect::<Vec<_>>()
        };
        assert_eq!(keys(FirmwareTarget::Frame), ["show_ip"]);
        assert_eq!(keys(FirmwareTarget::Dial), ["brightness"]);
        assert_eq!(keys(FirmwareTarget::LegacyKnob), ["brightness"]);
        assert_eq!(keys(FirmwareTarget::Halo), ["brightness"]);
        assert_eq!(
            keys(FirmwareTarget::Stackchan),
            ["body_enabled", "sound_enabled", "sound_volume"]
        );
        for target in [
            FirmwareTarget::Rlcd,
            FirmwareTarget::Joy,
            FirmwareTarget::Tough,
            FirmwareTarget::M5Dial,
            FirmwareTarget::StickS3,
            FirmwareTarget::Stopwatch,
        ] {
            assert!(keys(target).is_empty(), "{target:?}");
        }
    }

    #[test]
    fn every_setting_key_is_unique_per_controller_and_belongs_to_the_known_set() {
        for target in FirmwareTarget::ALL {
            let specs = device_settings(*target);
            for (index, spec) in specs.iter().enumerate() {
                assert!(ALL_KEYS.contains(&spec.key), "{}", spec.key);
                assert!(specs[index + 1..].iter().all(|other| other.key != spec.key));
                assert!(!spec.label.is_empty() && !spec.help.is_empty());
            }
        }
    }

    #[test]
    fn display_and_power_controls_follow_what_the_firmware_honours() {
        // Frame and HALO store these fields but their display drivers ignore them.
        assert_eq!(power_support(FirmwareTarget::Frame), PowerSupport::None);
        assert_eq!(power_support(FirmwareTarget::Halo), PowerSupport::None);
        // Slate acts only on the album-art timeout.
        assert_eq!(power_support(FirmwareTarget::Rlcd), PowerSupport::ArtOnly);
        for target in [
            FirmwareTarget::Dial,
            FirmwareTarget::LegacyKnob,
            FirmwareTarget::Tough,
            FirmwareTarget::Joy,
            FirmwareTarget::M5Dial,
            FirmwareTarget::StickS3,
            FirmwareTarget::Stopwatch,
            FirmwareTarget::Stackchan,
        ] {
            assert_eq!(power_support(target), PowerSupport::Full, "{target:?}");
        }
    }

    #[test]
    fn values_outside_a_settings_range_are_rejected() {
        let t = FirmwareTarget::Dial;
        assert_eq!(check_number(t, "brightness", 5), Ok(()));
        assert_eq!(check_number(t, "brightness", 100), Ok(()));
        // 0 would black the screen with no way to see the control that undoes it.
        assert!(check_number(t, "brightness", 4).is_err());
        assert!(check_number(t, "brightness", 101).is_err());
        let stack = FirmwareTarget::Stackchan;
        assert_eq!(check_number(stack, "sound_volume", 2), Ok(()));
        assert!(check_number(stack, "sound_volume", 3).is_err());
        assert!(check_number(stack, "sound_volume", -1).is_err());
    }

    #[test]
    fn a_setting_a_controller_does_not_have_is_rejected_by_name() {
        assert!(check_number(FirmwareTarget::Frame, "brightness", 50).is_err());
        assert!(check_toggle(FirmwareTarget::Dial, "show_ip").is_err());
        assert!(check_toggle(FirmwareTarget::Frame, "show_ip").is_ok());
        assert!(check_toggle(FirmwareTarget::Frame, "no_such_key").is_err());
    }
}
