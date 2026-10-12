//! BluOS (Bluesound, NAD/BluOS modules) wire formats: HTTP GET requests on port 11000 answered by
//! XML. Pure functions only, so every encoding decision is unit-tested.
//!
//! # Sources (no hardware was available; examples are shaped from them)
//!
//! - `pyblu` (MIT, (c) Louis Christ): `src/pyblu/player.py` (endpoints and parameters: `/Status`,
//!   `/SyncStatus`, `/Volume?level=&mute=`, `/Play?url=`, `/RadioBrowse?service=Capture`) and
//!   `src/pyblu/parse.py` (response attributes: `SyncStatus@mac`, `@id`, `@db`, `@volume`;
//!   `status/volume`, `/db`, `/mute`, `/inputId`; `volume@db`/`@mute`; `radiotime/item@id|@text|@URL`).
//!   <https://github.com/LouisChrist/pyblu>
//! - BluOS Custom Integration API reference (Bluesound), for the 0-100 volume range.
//!
//! Anything the sources do not pin down is marked implementation-derived.

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::devices::service::DeviceOperation;
use crate::devices::{CapabilityEvidence, DeviceCapabilities, DeviceCapability, DeviceInput};

pub const DEFAULT_PORT: u16 = 11000;
/// mDNS service type BluOS players advertise (the catalog's discovery record).
pub const MDNS_SERVICE: &str = "_musc._tcp.local.";

/// BluOS volume is a percentage: `Volume?level=0..100` (pyblu `Player.volume` docstring).
pub const MIN_LEVEL: i64 = 0;
pub const MAX_LEVEL: i64 = 100;

/// One XML element, flattened: its attributes and the text directly inside it.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Node {
    pub name: String,
    pub attrs: HashMap<String, String>,
    pub text: String,
}

/// Parse a document into its elements, in document order. Refuses malformed XML.
pub fn parse_nodes(xml: &str) -> Result<Vec<Node>, String> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut nodes: Vec<Node> = Vec::new();
    let mut open: Vec<usize> = Vec::new();
    let element = |start: &quick_xml::events::BytesStart<'_>| -> Result<Node, String> {
        let mut node = Node {
            name: String::from_utf8_lossy(start.local_name().as_ref()).to_string(),
            ..Node::default()
        };
        for attribute in start.attributes() {
            let attribute = attribute.map_err(|e| format!("bad attribute: {e}"))?;
            let key = String::from_utf8_lossy(attribute.key.local_name().as_ref()).to_string();
            let value = attribute
                .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                .map_err(|e| format!("bad attribute value: {e}"))?
                .to_string();
            node.attrs.insert(key, value);
        }
        Ok(node)
    };
    loop {
        match reader.read_event().map_err(|e| format!("bad XML: {e}"))? {
            Event::Start(start) => {
                nodes.push(element(&start)?);
                open.push(nodes.len() - 1);
            }
            Event::Empty(start) => nodes.push(element(&start)?),
            Event::End(_) => {
                open.pop();
            }
            Event::Text(text) => {
                if let Some(index) = open.last() {
                    let text = text.decode().map_err(|e| format!("bad text: {e}"))?;
                    nodes[*index].text.push_str(&text);
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !open.is_empty() {
        return Err("bad XML: unclosed element".to_string());
    }
    if nodes.is_empty() {
        return Err("empty document".to_string());
    }
    Ok(nodes)
}

fn first<'a>(nodes: &'a [Node], name: &str) -> Option<&'a Node> {
    nodes.iter().find(|node| node.name == name)
}

// =============================================================================
// /SyncStatus: identity
// =============================================================================

/// `/SyncStatus`: `<SyncStatus etag="..." id="192.168.1.30:11000" mac="90:56:82:AA:BB:CC"
/// name="Living" brand="Bluesound" model="P300" modelName="PULSE 2i" volume="20" db="-40.5" ...>`.
#[derive(Debug, Clone, PartialEq)]
pub struct SyncStatus {
    pub mac: String,
    pub name: String,
    pub brand: String,
    pub model: String,
    pub model_name: String,
}

pub fn parse_sync_status(xml: &str) -> Result<SyncStatus, String> {
    let nodes = parse_nodes(xml)?;
    let root = first(&nodes, "SyncStatus").ok_or_else(|| "no SyncStatus element".to_string())?;
    let attr = |key: &str| root.attrs.get(key).map(|v| v.trim().to_string());
    // The MAC is the identity. `id` is `ip:port`, which is an address, not an identity.
    let mac = attr("mac")
        .filter(|mac| !mac.is_empty())
        .ok_or_else(|| "SyncStatus carries no mac".to_string())?;
    let model_name = attr("modelName").unwrap_or_default();
    Ok(SyncStatus {
        mac,
        name: attr("name").unwrap_or_default(),
        brand: attr("brand").unwrap_or_default(),
        model: attr("model").unwrap_or_default(),
        model_name,
    })
}

impl SyncStatus {
    /// The model as a person knows it (`PULSE 2i`), else the model code (`P300`).
    pub fn display_model(&self) -> String {
        [&self.model_name, &self.model]
            .into_iter()
            .find(|m| !m.is_empty())
            .cloned()
            .unwrap_or_else(|| "BluOS player".to_string())
    }
}

// =============================================================================
// /Status
// =============================================================================

/// How a player's output volume behaves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeSupport {
    /// Not read yet.
    Unknown,
    /// A settable 0-100 level.
    Variable,
    /// Fixed-level output (the player reports a negative volume, `-1`): there is no volume to
    /// read or write, and UHC must never invent one.
    Fixed,
}

/// `/Status`: `<status etag="..."><db>-40.5</db><inputId>input0</inputId><mute>0</mute>
/// <service>Capture</service><state>stream</state><volume>20</volume>...</status>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Status {
    /// Percentage level; `None` when fixed or absent.
    pub volume: Option<i64>,
    pub volume_support: VolumeSupport,
    pub muted: Option<bool>,
    pub input_id: Option<String>,
}

pub fn parse_status(xml: &str) -> Result<Status, String> {
    let nodes = parse_nodes(xml)?;
    first(&nodes, "status").ok_or_else(|| "no status element".to_string())?;
    let text = |name: &str| {
        first(&nodes, name)
            .map(|n| n.text.trim().to_string())
            .filter(|t| !t.is_empty())
    };
    let (volume, volume_support) = match text("volume") {
        None => (None, VolumeSupport::Unknown),
        Some(raw) => match raw.parse::<i64>() {
            Ok(level) if level < 0 => (None, VolumeSupport::Fixed),
            Ok(level) if level <= MAX_LEVEL => (Some(level), VolumeSupport::Variable),
            // Out of the documented range: not a level UHC can trust.
            _ => (None, VolumeSupport::Unknown),
        },
    };
    Ok(Status {
        volume,
        volume_support,
        muted: if volume_support == VolumeSupport::Fixed {
            None
        } else {
            text("mute").map(|m| m == "1")
        },
        input_id: text("inputId"),
    })
}

// =============================================================================
// /RadioBrowse?service=Capture: inputs
// =============================================================================

/// One selectable input from `<radiotime service="Capture"><item id="input0" text="Optical
/// Input" URL="Capture%3Ahw%3Aimxspdif%2Cbluez%2Cinput0" .../></radiotime>`.
#[derive(Debug, Clone, PartialEq)]
pub struct CaptureInput {
    /// The item `id`, else the decoded URL.
    pub id: String,
    pub text: String,
    /// The decoded play URL (`Capture:hw:imxspdif,bluez,input0`), sent to `/Play?url=`.
    pub url: String,
}

pub fn parse_inputs(xml: &str) -> Result<Vec<CaptureInput>, String> {
    let nodes = parse_nodes(xml)?;
    first(&nodes, "radiotime").ok_or_else(|| "no radiotime element".to_string())?;
    Ok(nodes
        .iter()
        .filter(|node| node.name == "item")
        .filter_map(|item| {
            let raw = item.attrs.get("URL").or_else(|| item.attrs.get("url"))?;
            let url = urlencoding::decode(raw).ok()?.to_string();
            let text = item.attrs.get("text").cloned().unwrap_or_default();
            let id = item
                .attrs
                .get("id")
                .filter(|id| !id.is_empty())
                .cloned()
                .unwrap_or_else(|| url.clone());
            Some(CaptureInput {
                id,
                text: if text.is_empty() { url.clone() } else { text },
                url,
            })
        })
        .collect())
}

pub fn catalog(inputs: &[CaptureInput]) -> Vec<DeviceInput> {
    inputs
        .iter()
        .map(|input| DeviceInput {
            id: input.id.clone(),
            name: input.text.clone(),
            available: true,
        })
        .collect()
}

// =============================================================================
// Commands
// =============================================================================

pub fn status_path() -> &'static str {
    "/Status"
}

pub fn sync_status_path() -> &'static str {
    "/SyncStatus"
}

pub fn inputs_path() -> &'static str {
    "/RadioBrowse?service=Capture"
}

/// Encode a percentage level. Refuses non-finite and out-of-range levels instead of clamping:
/// the service already clamped and quantised, so an out-of-range level here is a bug.
pub fn encode_level(level: f32, limit: i64) -> Result<i64, String> {
    if !level.is_finite() {
        return Err("volume must be a finite number".to_string());
    }
    let value = level.round() as i64;
    let max = limit.min(MAX_LEVEL);
    if !(MIN_LEVEL..=max).contains(&value) {
        return Err(format!(
            "volume {level} is outside the player range {MIN_LEVEL}..{max}"
        ));
    }
    Ok(value)
}

/// The request path for one operation.
///
/// `PowerOn`/`PowerStandby` are always refused: BluOS has no universal power control (a streaming
/// module has no amplifier to switch, and the API documents no power command).
pub fn encode_operation(
    operation: &DeviceOperation,
    volume_support: VolumeSupport,
    inputs: &[CaptureInput],
) -> Result<String, String> {
    match operation {
        DeviceOperation::SetVolume(level) => {
            require_variable(volume_support)?;
            Ok(format!("/Volume?level={}", encode_level(*level, MAX_LEVEL)?))
        }
        DeviceOperation::SetMute(muted) => {
            require_variable(volume_support)?;
            Ok(format!("/Volume?mute={}", u8::from(*muted)))
        }
        DeviceOperation::PowerOn | DeviceOperation::PowerStandby => Err(
            "power control is not supported by BluOS players (the API has no universal power command)"
                .to_string(),
        ),
        DeviceOperation::SelectInput(id) => {
            let input = inputs
                .iter()
                .find(|input| &input.id == id)
                .ok_or_else(|| format!("{id} is not an input this player reported"))?;
            Ok(format!("/Play?url={}", urlencoding::encode(&input.url)))
        }
    }
}

fn require_variable(support: VolumeSupport) -> Result<(), String> {
    match support {
        VolumeSupport::Variable => Ok(()),
        VolumeSupport::Fixed => {
            Err("volume is not supported by this player: its output is fixed-level".to_string())
        }
        VolumeSupport::Unknown => {
            Err("this player has not reported a volume, so none can be sent".to_string())
        }
    }
}

/// Does `/Play` or `/Volume` answer carry a BluOS error element?
pub fn check_command_response(xml: &str) -> Result<(), String> {
    match parse_nodes(xml) {
        Ok(nodes) => match first(&nodes, "error") {
            Some(error) => Err(format!("player refused the command: {}", error.text.trim())),
            None => Ok(()),
        },
        // Some firmwares answer a bare 200; a body that is not XML is not an error by itself.
        Err(_) => Ok(()),
    }
}

// =============================================================================
// Capabilities
// =============================================================================

/// Capability claims for one model. Pure. All implementation-derived.
pub fn capabilities_for(
    model: &str,
    volume: VolumeSupport,
    inputs_known: bool,
) -> DeviceCapabilities {
    let claim = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    let pending = "implemented from pyblu and the BluOS API; no BluOS hardware exercised yet";
    let (volume_ok, volume_detail) = match volume {
        VolumeSupport::Variable => (true, format!("/Volume?level= (0-100 percent); {pending}")),
        VolumeSupport::Fixed => (
            false,
            format!(
                "{model} reports a negative volume: fixed-level output, nothing to set; {pending}"
            ),
        ),
        VolumeSupport::Unknown => (
            false,
            format!("{model} has not reported a volume yet; none is sent until it does; {pending}"),
        ),
    };
    DeviceCapabilities {
        observe: claim(
            true,
            format!("/Status polled over HTTP on port 11000; {pending}"),
        ),
        volume: claim(volume_ok, volume_detail),
        mute: claim(
            volume_ok,
            if volume_ok {
                format!("/Volume?mute=; {pending}")
            } else {
                format!("mute follows volume: unavailable while {model} has no settable volume; {pending}")
            },
        ),
        power: claim(
            false,
            format!(
                "the BluOS API has no universal power command (a streaming module has no \
                 amplifier to switch, and power is a host-device matter); refused with an \
                 error; {pending}"
            ),
        ),
        input_select: claim(
            inputs_known,
            if inputs_known {
                format!("/Play?url=Capture:... from /RadioBrowse?service=Capture; {pending}")
            } else {
                format!("{model} lists no Capture inputs; {pending}")
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped from pyblu's `parse_sync_status` expectations.
    const SYNC_STATUS: &str = r#"<SyncStatus icon="/images/players/P300_nt.png" volume="20"
        modelName="PULSE 2i" name="Living" model="P300" brand="Bluesound" etag="a1b2"
        mac="90:56:82:AA:BB:CC" id="192.168.1.30:11000" db="-40.5" schemaVersion="34"
        initialized="true"><master port="11000">192.168.1.31</master></SyncStatus>"#;

    /// Shaped from pyblu's `parse_status` expectations.
    const STATUS: &str = r#"<status etag="c3d4"><db>-40.5</db><inputId>input0</inputId>
        <mute>0</mute><name>Track</name><service>Capture</service><shuffle>0</shuffle>
        <state>stream</state><volume>20</volume><groupVolume>20</groupVolume></status>"#;

    const INPUTS: &str = r#"<radiotime service="Capture">
        <item id="input0" text="Optical Input" image="/i.png" inputType="spdif" type="audio"
              URL="Capture%3Ahw%3Aimxspdif%2Cbluez%2Cinput0"/>
        <item id="input1" text="Analog &amp; Line" image="/j.png"
              URL="Capture%3Ahw%3Aimxspdif%2Cbluez%2Cinput1"/>
        <item text="no url"/></radiotime>"#;

    #[test]
    fn sync_status_identity_is_the_mac_not_the_address() {
        let sync = parse_sync_status(SYNC_STATUS).unwrap();
        assert_eq!(sync.mac, "90:56:82:AA:BB:CC");
        assert_eq!(sync.name, "Living");
        assert_eq!(sync.brand, "Bluesound");
        assert_eq!(sync.display_model(), "PULSE 2i");
        // `id` is ip:port: with no mac there is no identity, and none is invented from it.
        let without_mac = SYNC_STATUS.replace(r#"mac="90:56:82:AA:BB:CC""#, "");
        assert!(parse_sync_status(&without_mac).is_err());
    }

    #[test]
    fn malformed_and_foreign_documents_are_refused() {
        assert!(parse_sync_status("not xml at all <<<").is_err());
        assert!(parse_sync_status("<html><body/></html>").is_err());
        assert!(parse_sync_status("").is_err());
        assert!(parse_status("<SyncStatus/>").is_err());
        assert!(parse_status("<status><volume>1").is_err());
        assert!(parse_inputs("<status/>").is_err());
    }

    #[test]
    fn status_carries_level_mute_and_input() {
        let status = parse_status(STATUS).unwrap();
        assert_eq!(status.volume, Some(20));
        assert_eq!(status.volume_support, VolumeSupport::Variable);
        assert_eq!(status.muted, Some(false));
        assert_eq!(status.input_id.as_deref(), Some("input0"));
        let muted = parse_status(&STATUS.replace("<mute>0</mute>", "<mute>1</mute>")).unwrap();
        assert_eq!(muted.muted, Some(true));
    }

    #[test]
    fn negative_volume_means_fixed_output_with_no_level_and_no_mute() {
        let fixed =
            parse_status(&STATUS.replace("<volume>20</volume>", "<volume>-1</volume>")).unwrap();
        assert_eq!(fixed.volume, None);
        assert_eq!(fixed.volume_support, VolumeSupport::Fixed);
        assert_eq!(fixed.muted, None);
    }

    #[test]
    fn missing_or_out_of_range_volume_is_unknown_never_a_level() {
        let none = parse_status("<status><state>stop</state></status>").unwrap();
        assert_eq!(none.volume_support, VolumeSupport::Unknown);
        let silly = parse_status("<status><volume>250</volume></status>").unwrap();
        assert_eq!(silly.volume, None);
        assert_eq!(silly.volume_support, VolumeSupport::Unknown);
        let garbage = parse_status("<status><volume>loud</volume></status>").unwrap();
        assert_eq!(garbage.volume_support, VolumeSupport::Unknown);
    }

    #[test]
    fn inputs_decode_urls_and_skip_items_without_one() {
        let inputs = parse_inputs(INPUTS).unwrap();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].id, "input0");
        assert_eq!(inputs[0].text, "Optical Input");
        assert_eq!(inputs[0].url, "Capture:hw:imxspdif,bluez,input0");
        assert_eq!(inputs[1].text, "Analog & Line", "entities are unescaped");
        let catalog = catalog(&inputs);
        assert_eq!(catalog[1].id, "input1");
        assert!(catalog.iter().all(|i| i.available));
    }

    #[test]
    fn level_encodes_at_min_max_and_rounds_to_whole_percent() {
        assert_eq!(encode_level(0.0, 100), Ok(0));
        assert_eq!(encode_level(100.0, 100), Ok(100));
        assert_eq!(encode_level(37.0, 100), Ok(37));
        assert_eq!(encode_level(36.6, 100), Ok(37));
    }

    #[test]
    fn out_of_range_and_non_finite_levels_are_refused_not_clamped() {
        assert!(encode_level(101.0, 100).is_err());
        assert!(encode_level(-1.0, 100).is_err());
        assert!(encode_level(f32::NAN, 100).is_err());
        assert!(encode_level(f32::NEG_INFINITY, 100).is_err());
        assert!(
            encode_level(60.0, 50).is_err(),
            "a limit below the range bounds it"
        );
    }

    #[test]
    fn operations_encode_to_the_documented_requests() {
        let inputs = parse_inputs(INPUTS).unwrap();
        let encode = |op| encode_operation(&op, VolumeSupport::Variable, &inputs);
        assert_eq!(
            encode(DeviceOperation::SetVolume(42.0)).unwrap(),
            "/Volume?level=42"
        );
        assert_eq!(
            encode(DeviceOperation::SetMute(true)).unwrap(),
            "/Volume?mute=1"
        );
        assert_eq!(
            encode(DeviceOperation::SetMute(false)).unwrap(),
            "/Volume?mute=0"
        );
        assert_eq!(
            encode(DeviceOperation::SelectInput("input0".into())).unwrap(),
            "/Play?url=Capture%3Ahw%3Aimxspdif%2Cbluez%2Cinput0"
        );
        assert!(encode(DeviceOperation::SelectInput("input9".into())).is_err());
        assert!(encode(DeviceOperation::SelectInput("Capture:x&level=100".into())).is_err());
    }

    #[test]
    fn power_is_always_refused() {
        for op in [DeviceOperation::PowerOn, DeviceOperation::PowerStandby] {
            let error = encode_operation(&op, VolumeSupport::Variable, &[]).unwrap_err();
            assert!(error.contains("not supported"), "{error}");
        }
    }

    #[test]
    fn fixed_or_unread_volume_refuses_level_and_mute() {
        for support in [VolumeSupport::Fixed, VolumeSupport::Unknown] {
            assert!(encode_operation(&DeviceOperation::SetVolume(10.0), support, &[]).is_err());
            assert!(encode_operation(&DeviceOperation::SetMute(true), support, &[]).is_err());
        }
    }

    #[test]
    fn command_error_elements_are_surfaced() {
        assert!(check_command_response("<state>stream</state>").is_ok());
        assert!(check_command_response("OK").is_ok());
        assert!(check_command_response("<error>bad</error>").is_err());
    }

    #[test]
    fn capabilities_follow_volume_support_and_never_claim_power() {
        let variable = capabilities_for("PULSE 2i", VolumeSupport::Variable, true);
        assert!(
            variable.volume.supported && variable.mute.supported && variable.input_select.supported
        );
        assert!(!variable.power.supported);
        let fixed = capabilities_for("NODE", VolumeSupport::Fixed, false);
        assert!(!fixed.volume.supported && !fixed.mute.supported && !fixed.input_select.supported);
        assert!(fixed.volume.detail.contains("fixed-level"));
        assert!(fixed.observe.supported);
        for claim in [
            &variable.observe,
            &variable.volume,
            &variable.power,
            &fixed.volume,
        ] {
            assert_eq!(claim.evidence, CapabilityEvidence::ImplementationDerived);
        }
    }
}
