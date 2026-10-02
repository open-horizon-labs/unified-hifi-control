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

    pub fn cache_directory(
        self,
        root: &std::path::Path,
        channel: FirmwareChannel,
    ) -> std::path::PathBuf {
        if self == Self::LegacyKnob && channel == FirmwareChannel::Stable {
            root.to_path_buf()
        } else {
            root.join(channel.slug()).join(self.slug())
        }
    }

    pub fn filename_version(self, filename: &str) -> Option<&str> {
        let application = self.application_file();
        let stem = application.strip_suffix(".bin")?;
        let suffix = filename.strip_prefix(stem)?.strip_suffix(".bin")?;
        let version = if self == Self::LegacyKnob {
            suffix
                .trim_start_matches(['_', '-'])
                .trim_start_matches('v')
        } else {
            suffix.strip_prefix("_v")?
        };
        parse_firmware_version(version).map(|_| version)
    }

    pub fn valid_application_filename(self, filename: &str) -> bool {
        filename == self.application_file() || self.filename_version(filename).is_some()
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
            FirmwareTarget::Frame.cache_directory(root, FirmwareChannel::Stable),
            root.join("stable/frame")
        );
        assert_eq!(
            FirmwareTarget::LegacyKnob.cache_directory(root, FirmwareChannel::Stable),
            root.to_path_buf()
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

#[cfg(test)]
mod multi_device_catalog_tests {
    use super::*;
    #[test]
    fn each_target_accepts_only_its_own_valid_versioned_application() {
        for target in FirmwareTarget::ALL {
            let stem = target
                .application_file()
                .trim_end_matches(".bin")
                .to_string();
            for version in ["3.0.0", "3.0.0-beta.2", "3.0.0-alpha.10"] {
                assert!(target.valid_application_filename(&format!("{stem}_v{version}.bin")));
            }
            assert!(!target.valid_application_filename(&format!("{stem}_v../escape.bin")));
        }
    }
}

/// Serving a prerelease requires an explicit query selection; caches are isolated by channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FirmwareChannel {
    #[default]
    Stable,
    Beta,
    Alpha,
}
impl FirmwareChannel {
    pub const ALL: &'static [Self] = &[Self::Stable, Self::Beta, Self::Alpha];
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "stable" => Some(Self::Stable),
            "beta" => Some(Self::Beta),
            "alpha" => Some(Self::Alpha),
            _ => None,
        }
    }
    pub fn slug(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Beta => "beta",
            Self::Alpha => "alpha",
        }
    }
    pub fn matches_version(self, version: &str) -> bool {
        parse_firmware_version(version).is_some_and(|parts| match self {
            Self::Stable => parts.3 == 2,
            Self::Beta => parts.3 == 1,
            Self::Alpha => parts.3 == 0,
        })
    }
}

/// Closed published version grammar: three numeric components, optionally alpha.N/beta.N.
/// Parsed tuples order alpha before beta before stable, and compare numeric counters correctly.
fn parse_firmware_version(version: &str) -> Option<(u64, u64, u64, u8, u64)> {
    let (core, suffix) = version
        .split_once('-')
        .map_or((version, None), |(core, suffix)| (core, Some(suffix)));
    let number = |part: &str| {
        if part.is_empty()
            || !part.bytes().all(|b| b.is_ascii_digit())
            || (part.len() > 1 && part.starts_with('0'))
        {
            None
        } else {
            part.parse::<u64>().ok()
        }
    };
    let components: Vec<_> = core.split('.').collect();
    if components.len() != 3 {
        return None;
    }
    let (stage, counter) = match suffix {
        None => (2, 0),
        Some(suffix) => {
            let (stage, counter) = suffix.split_once('.')?;
            (
                match stage {
                    "alpha" => 0,
                    "beta" => 1,
                    _ => return None,
                },
                number(counter)?,
            )
        }
    };
    Some((
        number(components[0])?,
        number(components[1])?,
        number(components[2])?,
        stage,
        counter,
    ))
}

pub fn compare_firmware_versions(left: &str, right: &str) -> Option<std::cmp::Ordering> {
    Some(
        parse_firmware_version(left.trim_start_matches('v'))?
            .cmp(&parse_firmware_version(right.trim_start_matches('v'))?),
    )
}
