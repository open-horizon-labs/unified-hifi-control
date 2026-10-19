//! Playback devices: the amplifier or receiver a zone feeds (#774).
//!
//! # A device is not a zone
//!
//! A zone is a playback destination owned by a music provider (Roon, LMS, ...): it has transport,
//! a queue and now-playing. A device is the hardware that zone's audio arrives at: it has power,
//! volume, mute and a physical input selector, and no transport at all. Modelling an amplifier as
//! one more zone was considered and rejected (`docs/device-control-weave.md`, option B): the zone the
//! user actually holds would still report no volume, and transport-less zones would appear in
//! every list and in the Home Assistant export.
//!
//! So devices live beside zones in the [`crate::aggregator::ZoneAggregator`], and a user-confirmed
//! [`DeviceLink`] says which device a zone feeds and on which input. The aggregator composes the
//! effective zone on read: transport and now-playing from the provider, volume from the linked
//! device. Clients that already read `volume_control` get the amplifier without a release.
//!
//! # Maturity
//!
//! Everything in this module is **alpha** ([`DEVICE_CONTROL_MATURITY`]). Every API and MCP
//! payload that carries a device says so, and the Settings toggle wears the badge.

pub mod anomalies;
pub mod links;
pub mod report;
pub mod service;

use serde::{Deserialize, Serialize};
use std::fmt;

/// Maturity label carried by every device payload and the Settings toggle.
pub const DEVICE_CONTROL_MATURITY: &str = "alpha";

/// Device kinds this build can name. A kind is the vendor brand that prefixes a [`DeviceId`];
/// one driver may serve several kinds (Denon and Marantz share one protocol family).
pub const DEVICE_KINDS: &[&str] = &[
    "denon",
    "marantz",
    "wiim",
    "yamaha",
    "bluos",
    "cambridge",
    "hegel",
    "lyngdorf",
    "onkyo",
    "sonos",
    "anthem",
    "arcam",
    "trinnov",
    "eversolo",
    // Home Assistant media_player entities (#790).
    "ha",
];

/// A device's hardware identity: `<kind>:<hardware id>`, e.g. `marantz:0006781E0AB8`.
///
/// Deliberately **not** a [`crate::bus::PrefixedZoneId`]: devices are not zones, and the zone
/// prefix list is the gate that keeps transport-less ids out of every zone surface.
///
/// The hardware id is the unit's serial number as the device reports it (the SR6008 reports its
/// MAC there, and embeds the same value in its UDN). It is never a model name, friendly name or
/// IP address: two identical receivers on one network differ only by this value, and links
/// persist against it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct DeviceId(String);

impl DeviceId {
    /// Build an id from a kind and a hardware id. The hardware id is normalised to upper-case
    /// alphanumerics so `00:06:78:1e:0a:b8` and `0006781E0AB8` name the same unit.
    pub fn new(kind: &str, hardware_id: &str) -> Option<Self> {
        if !DEVICE_KINDS.contains(&kind) {
            return None;
        }
        let normalized: String = hardware_id
            .chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_uppercase())
            .collect();
        if normalized.is_empty() {
            return None;
        }
        Some(Self(format!("{kind}:{normalized}")))
    }

    /// Parse `<kind>:<hardware id>`. Refuses unknown kinds, empty ids and nested prefixes.
    pub fn parse(value: &str) -> Option<Self> {
        let (kind, hardware_id) = value.split_once(':')?;
        if hardware_id.is_empty() || hardware_id.contains(':') {
            return None;
        }
        let parsed = Self::new(kind, hardware_id)?;
        // Only canonical spellings round-trip; a lower-case id is a different string and would
        // otherwise create two links for one unit.
        (parsed.0 == value).then_some(parsed)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn kind(&self) -> &str {
        self.0.split(':').next().unwrap_or_default()
    }

    pub fn hardware_id(&self) -> &str {
        self.0.split(':').nth(1).unwrap_or_default()
    }

    /// The `output_id` a linked zone's `volume_control` carries for this device's main zone.
    ///
    /// It rides the existing `VolumeChanged { output_id }` event, so every consumer that already
    /// matches volume updates by output id handles device volume without a new event type.
    pub fn main_output_id(&self) -> String {
        format!("{}:main", self.0)
    }
}

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl TryFrom<String> for DeviceId {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value).ok_or_else(|| format!("not a device id: {value}"))
    }
}

impl From<DeviceId> for String {
    fn from(id: DeviceId) -> Self {
        id.0
    }
}

/// Main-zone power as the device last reported it.
///
/// Power is a device fact, never a [`crate::bus::PlaybackState`]: a provider can be `playing` into
/// an amplifier in standby, and that combination is exactly what a surface needs to see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DevicePower {
    On,
    Standby,
    /// Not yet observed, or the device is unreachable. Never rounded to `standby`.
    #[default]
    Unknown,
}

/// How UHC currently talks to the device. Orthogonal to [`DevicePower`]: an amplifier in standby
/// may still answer HTTP, and an amplifier that is on may refuse a second control session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlChannel {
    /// A persistent push session (Denon/Marantz Telnet on TCP 23).
    Push,
    /// Polled HTTP only: slower readback, and some facts (the hardware volume limit) are unknown.
    Polling,
    /// Not reachable.
    #[default]
    None,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceConnection {
    pub control_channel: ControlChannel,
    /// Why the preferred channel is not in use, when it is not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// One physical input the device can select.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceInput {
    /// The protocol code used to select it (Denon/Marantz `SI` codes such as `MPLAY`, `CD`).
    /// Stable across renames; links store this, never the display name.
    pub id: String,
    /// The name the user gave the input on the device, or the function name when unrenamed.
    pub name: String,
    /// False when the user hid the input on the device ("source delete").
    pub available: bool,
}

/// The device's volume, in decibels, as it last reported it.
///
/// Decibels are the canonical unit because they are what the protocol carries (`MasterVolume`
/// and the Telnet `MV` value are both dB-derived) and what the knob already renders for HQPlayer.
/// A front panel that shows "absolute" 0-98 is a display convention, recorded in
/// [`Self::display`] so a UI can mirror it; it never changes what is sent.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceVolume {
    /// The device's native scale. Every level, bound, ceiling and step below is in it: the clamp
    /// and ceiling work in the device's own units, so a 0–100 streamer and a dB receiver are both
    /// bounded without converting between scales (the v2.5 incident class).
    pub scale: crate::bus::VolumeScale,
    /// Current level. `None` until read.
    pub value: Option<f32>,
    /// Lowest settable level in dB.
    pub min: f32,
    /// The device's own maximum (Denon/Marantz `MVMAX`), in dB. `None` until read; absolute and
    /// relative volume writes are refused while it is unknown.
    pub hardware_max: Option<f32>,
    /// The user's ceiling for this device, in dB. Only ever lowers the effective maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ceiling: Option<f32>,
    /// Smallest settable increment in dB.
    pub step: f32,
    pub is_muted: Option<bool>,
    /// Front-panel convention: `absolute` (0-98) or `relative` (dB). Display only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
}

impl DeviceVolume {
    /// The highest level UHC will publish or send: the hardware maximum lowered by the ceiling.
    ///
    /// When the device never reports its maximum (a Denon/Marantz reached only over HTTP: `MVMAX`
    /// is Telnet-only), the user's ceiling alone bounds the range. With neither, there is no range
    /// and every level is refused.
    pub fn effective_max(&self) -> Option<f32> {
        match (self.hardware_max, self.ceiling) {
            (Some(hardware), Some(ceiling)) if ceiling < hardware => Some(ceiling.max(self.min)),
            (Some(hardware), _) => Some(hardware),
            (None, Some(ceiling)) => Some(ceiling.max(self.min)),
            (None, None) => None,
        }
    }
}

/// Evidence behind a capability claim, so a client can tell a tested path from a derived one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CapabilityEvidence {
    /// Exercised against real hardware of this model, with a captured fixture.
    HardwareVerified,
    /// Implemented from the vendor protocol or an upstream client, not yet exercised here.
    ImplementationDerived,
}

/// One device capability: whether UHC has a path, and what that claim rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapability {
    pub supported: bool,
    pub evidence: CapabilityEvidence,
    pub detail: String,
}

/// Per-model capability claims. Read claims and write claims are separate because they are
/// verified separately: observing a level and setting one are different evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceCapabilities {
    pub observe: DeviceCapability,
    pub volume: DeviceCapability,
    pub mute: DeviceCapability,
    pub power: DeviceCapability,
    pub input_select: DeviceCapability,
}

/// A device as its adapter last observed it. Published whole on every observation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Device {
    pub device_id: DeviceId,
    /// Vendor brand as the device reports it ("Marantz").
    pub vendor: String,
    /// Model as the device reports it ("SR6008").
    pub model: String,
    /// The device's own friendly name ("front").
    pub name: String,
    pub host: String,
    /// `ssdp` or `manual`.
    pub discovery: String,
    pub maturity: String,
    pub connection: DeviceConnection,
    pub power: DevicePower,
    /// Current input code (see [`DeviceInput::id`]).
    pub input: Option<String>,
    pub inputs: Vec<DeviceInput>,
    pub volume: DeviceVolume,
    pub capabilities: DeviceCapabilities,
    /// Increments on every readback the adapter publishes, changed or not. Command confirmation
    /// waits for a revision newer than the one current at dispatch, so a write whose target
    /// equals the cached value is still confirmed against the device rather than the cache (#621).
    pub revision: u64,
    /// Milliseconds since the epoch of the last readback.
    pub last_updated: u64,
}

impl Device {
    /// The `volume_control` a zone linked to this device publishes, or `None` while the device
    /// cannot bound a level (current level or hardware maximum not yet read).
    ///
    /// `max` is the effective maximum — the hardware limit lowered by the user's ceiling — so no
    /// client that respects the published range can even request a level above the ceiling.
    pub fn volume_control(&self) -> Option<crate::bus::VolumeControl> {
        if !self.capabilities.volume.supported {
            return None;
        }
        let value = self.volume.value?;
        let max = self.volume.effective_max()?;
        if !(self.volume.min.is_finite() && max.is_finite() && self.volume.min < max) {
            return None;
        }
        Some(crate::bus::VolumeControl {
            value,
            min: self.volume.min,
            max,
            step: self.volume.step,
            is_muted: self.volume.is_muted.unwrap_or(false),
            scale: self.volume.scale,
            output_id: Some(self.device_id.main_output_id()),
        })
    }

    /// The current input's display name, when it is in the catalog.
    pub fn input_name(&self) -> Option<&str> {
        let current = self.input.as_deref()?;
        self.inputs
            .iter()
            .find(|input| input.id == current)
            .map(|input| input.name.as_str())
    }
}

/// A user-confirmed statement that a zone's audio arrives at a device, optionally on one input.
///
/// Which amplifier a zone feeds cannot be discovered, so links are only ever created by a person
/// (or an agent acting for one). Many zones may link to one device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceLink {
    pub zone_id: String,
    pub device_id: DeviceId,
    /// The input this zone arrives on (a [`DeviceInput::id`]). Data only: nothing switches the
    /// input automatically (decision D2); it tells a surface which linked zone is audible.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
}

/// An ephemeral composition snapshot from one aggregator read. An absent
/// device keeps the configured owner intact; it never restores provider gain.
/// This is not a new device identity, persisted document, or state cache.
#[derive(Debug, Clone)]
pub struct ZoneDeviceBinding {
    pub link: DeviceLink,
    pub device: Option<Device>,
}

impl ZoneDeviceBinding {
    pub fn volume_control(&self) -> Option<crate::bus::VolumeControl> {
        self.device.as_ref().and_then(Device::volume_control)
    }

    pub fn summary(&self) -> LinkedDeviceSummary {
        LinkedDeviceSummary::new(&self.link, self.device.as_ref())
    }
}

/// The device facts a linked zone surfaces, flattened for zone payloads and SSE.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LinkedDeviceSummary {
    pub device_id: DeviceId,
    pub name: String,
    pub maturity: String,
    /// False when the device is not currently observed (offline, adapter disabled).
    pub present: bool,
    pub power: DevicePower,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_name: Option<String>,
    /// The link's input, when the user named one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    /// Whether the device is currently on this link's input. `None` when either side is unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_linked_input: Option<bool>,
    pub inputs: Vec<DeviceInput>,
}

impl LinkedDeviceSummary {
    pub fn new(link: &DeviceLink, device: Option<&Device>) -> Self {
        match device {
            Some(device) => Self {
                device_id: link.device_id.clone(),
                name: device.name.clone(),
                maturity: device.maturity.clone(),
                present: true,
                power: device.power,
                input: device.input.clone(),
                input_name: device.input_name().map(str::to_string),
                source_id: link.source_id.clone(),
                on_linked_input: match (&link.source_id, &device.input) {
                    (Some(source), Some(current)) => Some(source == current),
                    _ => None,
                },
                inputs: device.inputs.clone(),
            },
            None => Self {
                device_id: link.device_id.clone(),
                name: link.device_id.to_string(),
                maturity: DEVICE_CONTROL_MATURITY.to_string(),
                present: false,
                power: DevicePower::Unknown,
                input: None,
                input_name: None,
                source_id: link.source_id.clone(),
                on_linked_input: None,
                inputs: Vec::new(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_models_with_different_serials_are_different_devices() {
        let front = DeviceId::new("marantz", "0006781E0AB8").unwrap();
        let bedroom = DeviceId::new("marantz", "0006781E2AB9").unwrap();
        assert_ne!(front, bedroom);
        assert_eq!(front.as_str(), "marantz:0006781E0AB8");
        assert_eq!(front.kind(), "marantz");
        assert_eq!(front.hardware_id(), "0006781E0AB8");
        assert_eq!(front.main_output_id(), "marantz:0006781E0AB8:main");
    }

    #[test]
    fn hardware_ids_normalise_so_one_unit_has_one_id() {
        assert_eq!(
            DeviceId::new("marantz", "00:06:78:1e:0a:b8").unwrap(),
            DeviceId::new("marantz", "0006781E0AB8").unwrap()
        );
    }

    #[test]
    fn parsing_refuses_non_canonical_unknown_and_zone_shaped_ids() {
        assert!(DeviceId::parse("marantz:0006781E0AB8").is_some());
        assert!(DeviceId::parse("marantz:0006781e0ab8").is_none());
        assert!(DeviceId::parse("bose:ABC").is_none());
        assert!(DeviceId::parse("roon:zone").is_none());
        assert!(DeviceId::parse("marantz:").is_none());
        assert!(DeviceId::parse("marantz:A:B").is_none());
        assert!(DeviceId::parse("marantz").is_none());
    }

    #[test]
    fn ceiling_only_lowers_the_effective_maximum() {
        let mut volume = DeviceVolume {
            scale: crate::bus::VolumeScale::Decibel,
            value: Some(-40.0),
            min: -80.0,
            hardware_max: Some(-10.0),
            ceiling: None,
            step: 0.5,
            is_muted: Some(false),
            display: None,
        };
        assert_eq!(volume.effective_max(), Some(-10.0));
        volume.ceiling = Some(-25.0);
        assert_eq!(volume.effective_max(), Some(-25.0));
        // A ceiling above the hardware limit can never raise it.
        volume.ceiling = Some(0.0);
        assert_eq!(volume.effective_max(), Some(-10.0));
        // Unknown hardware maximum: the user's ceiling alone bounds the range...
        volume.hardware_max = None;
        volume.ceiling = Some(-25.0);
        assert_eq!(volume.effective_max(), Some(-25.0));
        // ...and with neither, there is no range at all.
        volume.ceiling = None;
        assert_eq!(volume.effective_max(), None);
    }
}
