//! Hardware identity and release artifact catalog. Never infer a target from directory order.
/// A closed catalog prevents unrecognized hardware from receiving another device's image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FirmwareTarget {
    LegacyKnob,
    Dial,
    Frame,
    Tough,
    Joy,
    M5Dial,
    Rlcd,
    Stackchan,
    StickS3,
    Stopwatch,
}

impl FirmwareTarget {
    pub const ALL: &'static [Self] = &[
        Self::LegacyKnob,
        Self::Dial,
        Self::Frame,
        Self::Tough,
        Self::Joy,
        Self::M5Dial,
        Self::Rlcd,
        Self::Stackchan,
        Self::StickS3,
        Self::Stopwatch,
    ];

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "knob" | "roon-knob" => Some(Self::LegacyKnob),
            "dial" | "hiphi-dial" => Some(Self::Dial),
            "frame" | "hiphi-frame" => Some(Self::Frame),
            "tough" | "hiphi-tough" => Some(Self::Tough),
            "joy" | "hiphi-joy" => Some(Self::Joy),
            "m5dial" | "hiphi-m5dial" | "hiphi-dial-beta" => Some(Self::M5Dial),
            "rlcd" | "hiphi-rlcd" => Some(Self::Rlcd),
            "stackchan" | "hiphi-stackchan" | "hiphi-kizz-beta" => Some(Self::Stackchan),
            "sticks3" | "hiphi-sticks3" | "hiphi-sticks3-beta" => Some(Self::StickS3),
            "stopwatch" | "hiphi-stopwatch" | "hiphi-stopwatch-beta" => Some(Self::Stopwatch),
            _ => None,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::LegacyKnob => "knob",
            Self::Dial => "dial",
            Self::Frame => "frame",
            Self::Tough => "tough",
            Self::Joy => "joy",
            Self::M5Dial => "m5dial",
            Self::Rlcd => "rlcd",
            Self::Stackchan => "stackchan",
            Self::StickS3 => "sticks3",
            Self::Stopwatch => "stopwatch",
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::LegacyKnob | Self::Dial => "HiPhi Dial",
            Self::Frame => "HiPhi Frame",
            Self::Tough => "HiPhi Tough",
            Self::Joy => "HiPhi Joy",
            Self::M5Dial => "HiPhi Dial Lab",
            Self::Rlcd => "HiPhi Slate",
            Self::Stackchan => "Kizz Playback Companion",
            Self::StickS3 => "HiPhi Twist",
            Self::Stopwatch => "HiPhi Remote",
        }
    }

    pub fn application_file(self) -> String {
        if self == Self::LegacyKnob {
            "roon_knob.bin".into()
        } else {
            format!("hiphi_{}.bin", self.slug())
        }
    }

    pub fn merged_file(self) -> String {
        if self == Self::LegacyKnob {
            "roon_knob_merged.bin".into()
        } else {
            format!("hiphi_{}_merged.bin", self.slug())
        }
    }

    pub fn storage_directories(self, root: &std::path::Path) -> Vec<std::path::PathBuf> {
        let mut directories = vec![root.join(self.slug())];
        if self == Self::LegacyKnob {
            directories.push(root.to_path_buf());
        }
        directories
    }

    pub fn valid_application_filename(self, filename: &str) -> bool {
        if filename == self.application_file() {
            return true;
        }
        if self != Self::LegacyKnob {
            return false;
        }
        let Some(version) = filename
            .strip_prefix("roon_knob")
            .and_then(|s| s.strip_suffix(".bin"))
        else {
            return false;
        };
        let version = version
            .trim_start_matches(['_', '-'])
            .trim_start_matches('v');
        let parts: Vec<_> = version.split('.').collect();
        parts.len() == 3
            && parts
                .iter()
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    }

    pub fn chip_family(self) -> &'static str {
        if self == Self::Tough {
            "ESP32"
        } else {
            "ESP32-S3"
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actual_firmware_identities_resolve_to_their_own_artifacts() {
        for (identity, stem) in [
            ("hiphi-dial", "hiphi_dial"),
            ("hiphi-frame", "hiphi_frame"),
            ("hiphi-tough", "hiphi_tough"),
            ("hiphi-joy", "hiphi_joy"),
            ("hiphi-m5dial", "hiphi_m5dial"),
            ("hiphi-rlcd", "hiphi_rlcd"),
            ("hiphi-stackchan", "hiphi_stackchan"),
            ("hiphi-sticks3", "hiphi_sticks3"),
            ("hiphi-stopwatch", "hiphi_stopwatch"),
        ] {
            let target = FirmwareTarget::parse(identity).unwrap();
            assert_eq!(target.application_file(), format!("{stem}.bin"));
            assert_eq!(target.merged_file(), format!("{stem}_merged.bin"));
        }
    }

    #[test]
    fn legacy_knob_is_explicit_and_unknown_target_cannot_flash_dial() {
        assert_eq!(
            FirmwareTarget::parse("knob"),
            Some(FirmwareTarget::LegacyKnob)
        );
        assert_eq!(
            FirmwareTarget::LegacyKnob.application_file(),
            "roon_knob.bin"
        );
        for unknown in [
            "",
            "frmae",
            "../frame",
            "../../roon_knob.bin",
            "hiphi-unknown",
        ] {
            assert_eq!(FirmwareTarget::parse(unknown), None);
        }
    }

    #[test]
    fn tough_is_esp32_and_every_other_current_target_is_s3() {
        for target in FirmwareTarget::ALL {
            assert_eq!(
                target.chip_family(),
                if *target == FirmwareTarget::Tough {
                    "ESP32"
                } else {
                    "ESP32-S3"
                }
            );
            assert_ne!(target.application_file(), target.merged_file());
        }
    }
}

#[cfg(test)]
mod artifact_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn explicit_frame_never_searches_legacy_directory() {
        let root = Path::new("firmware");
        assert_eq!(
            FirmwareTarget::Frame.storage_directories(root),
            vec![root.join("frame")]
        );
        assert_eq!(
            FirmwareTarget::LegacyKnob.storage_directories(root),
            vec![root.join("knob"), root.to_path_buf()]
        );
    }

    #[test]
    fn artifact_metadata_cannot_select_another_target_or_escape_storage() {
        assert!(FirmwareTarget::Frame.valid_application_filename("hiphi_frame.bin"));
        assert!(!FirmwareTarget::Frame.valid_application_filename("hiphi_dial.bin"));
        assert!(!FirmwareTarget::Frame.valid_application_filename("hiphi_frame_merged.bin"));
        for unsafe_name in [
            "../hiphi_frame.bin",
            "/tmp/hiphi_frame.bin",
            "hiphi_frame.bin/../roon_knob.bin",
        ] {
            assert!(!FirmwareTarget::Frame.valid_application_filename(unsafe_name));
        }
        assert!(FirmwareTarget::LegacyKnob.valid_application_filename("roon_knob_v2.5.2.bin"));
        assert!(!FirmwareTarget::LegacyKnob.valid_application_filename("roon_knob_merged.bin"));
        assert!(!FirmwareTarget::LegacyKnob.valid_application_filename("roon_knob_evil.bin"));
    }
}

#[cfg(test)]
mod deployed_alias_tests {
    use super::*;
    #[test]
    fn m5_beta_firmware_identity_is_not_confused_with_primary_dial() {
        for (identity, target) in [
            ("hiphi-dial-beta", FirmwareTarget::M5Dial),
            ("hiphi-sticks3-beta", FirmwareTarget::StickS3),
            ("hiphi-stopwatch-beta", FirmwareTarget::Stopwatch),
            ("hiphi-kizz-beta", FirmwareTarget::Stackchan),
        ] {
            assert_eq!(FirmwareTarget::parse(identity), Some(target));
        }
    }
}
