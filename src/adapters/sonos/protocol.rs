//! Sonos wire formats: the UPnP device description and the SOAP calls on port 1400. Pure
//! functions only, so every encoding decision is unit-tested.
//!
//! Sources (no Sonos hardware was available; every behaviour here is implementation-derived):
//! - SoCo (MIT) <https://github.com/SoCo/SoCo>: `core.py` `volume`/`mute` (`RenderingControl`
//!   `GetVolume`/`SetVolume`/`GetMute`/`SetMute`, `InstanceID` 0, `Channel` `Master`),
//!   `fixed_volume` (`GetOutputFixed`), `switch_to_line_in` (`x-rincon-stream:<uid>`),
//!   `switch_to_tv` (`x-sonos-htastream:<uid>:spdif`), `is_playing_line_in`/`is_playing_tv`.
//!   Audited against SoCo `soco/core.py` and `soco/services.py` on `master`
//!   (<https://raw.githubusercontent.com/SoCo/SoCo/master/soco/core.py>): `volume` is "an integer
//!   between 0 and 100" (SoCo coerces; this adapter refuses out-of-range instead), `GetVolume` /
//!   `SetVolume` / `GetMute` / `SetMute` use `InstanceID` 0 + `Channel` `Master` and
//!   `DesiredVolume` / `DesiredMute` "1"/"0", `GetOutputFixed` -> `CurrentFixed` "1" (SoCo: only
//!   Connect and Port can be fixed; `GetSupportsOutputFixed` is the capability probe), line-in is
//!   `x-rincon-stream:<uid>` and TV is `x-sonos-htastream:<uid>:spdif` via `SetAVTransportURI` with
//!   empty metadata, input detection is `TrackURI` matched with `^x-rincon-stream:` /
//!   `^x-sonos-htastream:`, control URLs are `/MediaRenderer/<Service>/Control`, discovery is
//!   `urn:schemas-upnp-org:device:ZonePlayer:1` and port 1400. All match. SoCo does not follow
//!   `SetAVTransportURI` with `Play`; this adapter does, because the transport otherwise stays
//!   stopped (kept; it is not in SoCo). Which models have line-in / TV is not in SoCo either.
//! - node-sonos (MIT) <https://github.com/bencevans/node-sonos>: the same SOAP envelopes and the
//!   `/MediaRenderer/RenderingControl/Control` and `/MediaRenderer/AVTransport/Control` paths.
//! - UPnP AV `RenderingControl:1` and `AVTransport:1` service templates (upnp.org).
//!
//! # Boundaries
//!
//! - **No power.** Sonos has no generic power on/off; power operations are refused.
//! - **Input is model dependent.** Line-in and TV (HDMI ARC / optical) exist only on some models.
//!   The catalog is derived from the model name and is empty for models not listed, so an input
//!   a unit may not have is never offered.

use crate::devices::service::DeviceOperation;
use crate::devices::DeviceInput;

pub const PORT: u16 = 1400;
pub const DESCRIPTION_PATH: &str = "/xml/device_description.xml";
/// SSDP search target for Sonos players.
pub const SSDP_TARGET: &str = "urn:schemas-upnp-org:device:ZonePlayer:1";

pub const NO_POWER: &str =
    "power is not supported by this device: Sonos has no generic power on/off";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    RenderingControl,
    AvTransport,
}

impl Service {
    pub fn control_path(self) -> &'static str {
        match self {
            Self::RenderingControl => "/MediaRenderer/RenderingControl/Control",
            Self::AvTransport => "/MediaRenderer/AVTransport/Control",
        }
    }

    pub fn urn(self) -> &'static str {
        match self {
            Self::RenderingControl => "urn:schemas-upnp-org:service:RenderingControl:1",
            Self::AvTransport => "urn:schemas-upnp-org:service:AVTransport:1",
        }
    }
}

/// One SOAP call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoapCall {
    pub service: Service,
    pub action: &'static str,
    pub args: Vec<(&'static str, String)>,
}

impl SoapCall {
    fn new(service: Service, action: &'static str, args: Vec<(&'static str, String)>) -> Self {
        Self {
            service,
            action,
            args,
        }
    }

    /// The `SOAPACTION` header value (quoted, as UPnP requires).
    pub fn soap_action(&self) -> String {
        format!("\"{}#{}\"", self.service.urn(), self.action)
    }

    /// The request envelope.
    pub fn envelope(&self) -> String {
        let args: String = self
            .args
            .iter()
            .map(|(name, value)| format!("<{name}>{}</{name}>", escape_xml(value)))
            .collect();
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
             <s:Body><u:{action} xmlns:u=\"{urn}\">{args}</u:{action}></s:Body></s:Envelope>",
            action = self.action,
            urn = self.service.urn(),
        )
    }
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn instance() -> (&'static str, String) {
    ("InstanceID", "0".to_string())
}

pub fn get_volume() -> SoapCall {
    SoapCall::new(
        Service::RenderingControl,
        "GetVolume",
        vec![instance(), ("Channel", "Master".to_string())],
    )
}

pub fn get_mute() -> SoapCall {
    SoapCall::new(
        Service::RenderingControl,
        "GetMute",
        vec![instance(), ("Channel", "Master".to_string())],
    )
}

pub fn get_output_fixed() -> SoapCall {
    SoapCall::new(
        Service::RenderingControl,
        "GetOutputFixed",
        vec![instance()],
    )
}

pub fn get_position_info() -> SoapCall {
    SoapCall::new(Service::AvTransport, "GetPositionInfo", vec![instance()])
}

/// Highest and lowest level the protocol expresses.
pub const MIN_VOLUME: f32 = 0.0;
pub const MAX_VOLUME: f32 = 100.0;

/// Encode an absolute level. Refuses anything not finite or outside 0..=100.
pub fn encode_volume(level: f32) -> Option<u8> {
    (level.is_finite() && (MIN_VOLUME..=MAX_VOLUME).contains(&level)).then(|| level.round() as u8)
}

/// Parse a `GetVolume` response into a level, refusing values outside 0..=100.
pub fn parse_volume(xml: &str) -> Option<f32> {
    let level = element_text(xml, "CurrentVolume")?
        .trim()
        .parse::<f32>()
        .ok()?;
    (MIN_VOLUME..=MAX_VOLUME).contains(&level).then_some(level)
}

/// Parse a `GetMute` response.
pub fn parse_mute(xml: &str) -> Option<bool> {
    match element_text(xml, "CurrentMute")?.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// Parse a `GetOutputFixed` response: whether the unit's output is fixed-level.
pub fn parse_fixed(xml: &str) -> Option<bool> {
    match element_text(xml, "CurrentFixed")?.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// The `TrackURI` of a `GetPositionInfo` response.
pub fn parse_track_uri(xml: &str) -> Option<String> {
    element_text(xml, "TrackURI").map(|uri| decode_xml(uri.trim()))
}

/// The UPnP error code of a SOAP fault, e.g. `701` (transition not available).
pub fn parse_fault(xml: &str) -> Option<String> {
    element_text(xml, "errorCode").map(|code| code.trim().to_string())
}

/// The calls that carry out an operation, in order, or the refusal for one the device cannot do.
///
/// `uid` is the player's `RINCON_…` id; `inputs` is the model's catalog; `fixed` is whether the
/// unit reported a fixed output.
pub fn calls_for(
    operation: &DeviceOperation,
    uid: &str,
    inputs: &[DeviceInput],
    fixed: bool,
) -> Result<Vec<SoapCall>, String> {
    match operation {
        DeviceOperation::SetVolume(level) => {
            if fixed {
                return Err("volume is not supported by this device: its output is fixed".into());
            }
            let level = encode_volume(*level)
                .ok_or_else(|| format!("volume {level} is outside the device's 0-100 range"))?;
            Ok(vec![SoapCall::new(
                Service::RenderingControl,
                "SetVolume",
                vec![
                    instance(),
                    ("Channel", "Master".to_string()),
                    ("DesiredVolume", level.to_string()),
                ],
            )])
        }
        DeviceOperation::SetMute(muted) => Ok(vec![SoapCall::new(
            Service::RenderingControl,
            "SetMute",
            vec![
                instance(),
                ("Channel", "Master".to_string()),
                ("DesiredMute", u8::from(*muted).to_string()),
            ],
        )]),
        DeviceOperation::SelectInput(requested) => {
            let input = inputs
                .iter()
                .find(|input| {
                    input.available
                        && (input.id.eq_ignore_ascii_case(requested.trim())
                            || input.name.eq_ignore_ascii_case(requested.trim()))
                })
                .ok_or_else(|| format!("input {requested:?} is not available on this device"))?;
            let uri = input_uri(&input.id, uid)
                .ok_or_else(|| format!("input {requested:?} has no stream address"))?;
            Ok(vec![
                SoapCall::new(
                    Service::AvTransport,
                    "SetAVTransportURI",
                    vec![
                        instance(),
                        ("CurrentURI", uri),
                        ("CurrentURIMetaData", String::new()),
                    ],
                ),
                // Selecting a stream URI leaves the transport stopped; Play makes it audible.
                SoapCall::new(
                    Service::AvTransport,
                    "Play",
                    vec![instance(), ("Speed", "1".to_string())],
                ),
            ])
        }
        DeviceOperation::PowerOn | DeviceOperation::PowerStandby => Err(NO_POWER.to_string()),
    }
}

/// Input ids.
pub const LINE_IN: &str = "line-in";
pub const TV: &str = "tv";

/// The stream URI that selects an input (SoCo `switch_to_line_in` / `switch_to_tv`).
pub fn input_uri(id: &str, uid: &str) -> Option<String> {
    match id {
        LINE_IN => Some(format!("x-rincon-stream:{uid}")),
        TV => Some(format!("x-sonos-htastream:{uid}:spdif")),
        _ => None,
    }
}

/// The input a `TrackURI` names, if it names one (SoCo `is_playing_line_in` / `is_playing_tv`).
pub fn input_for_uri(uri: &str) -> Option<&'static str> {
    if uri.starts_with("x-rincon-stream:") {
        Some(LINE_IN)
    } else if uri.starts_with("x-sonos-htastream:") {
        Some(TV)
    } else {
        None
    }
}

fn model_key(model: &str) -> String {
    let lower = model.trim().to_ascii_lowercase();
    lower
        .strip_prefix("sonos ")
        .unwrap_or(&lower)
        .trim()
        .to_string()
}

/// The inputs a model physically has, from its name. Empty for models not listed: an input the
/// unit may not have is never offered.
pub fn inputs_for_model(model: &str) -> Vec<DeviceInput> {
    let key = model_key(model);
    let has_line_in = matches!(
        key.as_str(),
        "five" | "port" | "amp" | "era 100" | "era 300"
    ) || key.starts_with("play:5")
        || key.starts_with("connect");
    let has_tv = matches!(
        key.as_str(),
        "beam" | "arc" | "arc ultra" | "ray" | "playbar" | "playbase" | "amp"
    );
    let mut inputs = Vec::new();
    if has_line_in {
        inputs.push(DeviceInput {
            id: LINE_IN.to_string(),
            name: "Line In".to_string(),
            available: true,
        });
    }
    if has_tv {
        inputs.push(DeviceInput {
            id: TV.to_string(),
            name: "TV".to_string(),
            available: true,
        });
    }
    inputs
}

/// A parsed `device_description.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Description {
    pub manufacturer: String,
    /// `modelName`, e.g. `Sonos Beam`.
    pub model: String,
    /// `roomName`, the name the user gave the player.
    pub room_name: String,
    pub friendly_name: String,
    /// `serialNum`, e.g. `5C-AA-FD-12-34-56:5` (MAC, then a check digit).
    pub serial: Option<String>,
    /// The root `UDN`, e.g. `uuid:RINCON_5CAAFD12345601400`.
    pub udn: Option<String>,
}

impl Description {
    pub fn is_sonos(&self) -> bool {
        self.manufacturer.to_ascii_lowercase().contains("sonos")
    }

    /// The player's `RINCON_<MAC>01400` id, from the UDN.
    pub fn rincon(&self) -> Option<String> {
        let udn = self.udn.as_deref()?.trim();
        let id = udn.strip_prefix("uuid:").unwrap_or(udn);
        // Sub-devices append `_MR`/`_MS`; the root UDN does not.
        let id = id.split('_').take(2).collect::<Vec<_>>().join("_");
        let mac = id.strip_prefix("RINCON_")?;
        (mac.len() > 12 && mac.bytes().all(|b| b.is_ascii_hexdigit())).then_some(id)
    }

    /// The hardware id: the 12 hex digits of the MAC (UDN `RINCON_<MAC>01400`, else the
    /// `serialNum` MAC). Never the room name, model or IP.
    pub fn hardware_id(&self) -> Option<String> {
        if let Some(rincon) = self.rincon() {
            let mac = rincon.strip_prefix("RINCON_")?;
            return Some(mac[..12].to_ascii_uppercase());
        }
        let serial = self.serial.as_deref()?.split(':').next()?.trim();
        let mac: String = serial.chars().filter(char::is_ascii_hexdigit).collect();
        (mac.len() == 12).then(|| mac.to_ascii_uppercase())
    }

    /// The model without the `Sonos ` brand prefix.
    pub fn model_label(&self) -> String {
        self.model
            .trim()
            .strip_prefix("Sonos ")
            .unwrap_or(self.model.trim())
            .to_string()
    }

    /// The display name: the room, else the friendly name.
    pub fn name(&self) -> String {
        if self.room_name.is_empty() {
            self.friendly_name.clone()
        } else {
            self.room_name.clone()
        }
    }
}

/// Parse a UPnP device description. `None` when it carries no manufacturer.
pub fn parse_description(xml: &str) -> Option<Description> {
    let text = |tag: &str| element_text(xml, tag).map(|v| decode_xml(v.trim()));
    Some(Description {
        manufacturer: text("manufacturer")?,
        model: text("modelName").unwrap_or_default(),
        room_name: text("roomName").unwrap_or_default(),
        friendly_name: text("friendlyName").unwrap_or_default(),
        serial: text("serialNum"),
        udn: text("UDN"),
    })
}

fn decode_xml(value: &str) -> String {
    value
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Text of the first `<tag>…</tag>` element (SOAP responses and descriptions use unprefixed
/// element names for these fields).
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

    // Shape per a real Sonos device_description.xml as quoted in SoCo's test fixtures and the
    // node-sonos documentation (abridged to the fields this adapter reads).
    const DESCRIPTION: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0"><device>
<deviceType>urn:schemas-upnp-org:device:ZonePlayer:1</deviceType>
<friendlyName>192.168.1.50 - Sonos Beam</friendlyName>
<manufacturer>Sonos, Inc.</manufacturer>
<modelName>Sonos Beam</modelName>
<serialNum>5C-AA-FD-12-34-56:5</serialNum>
<UDN>uuid:RINCON_5CAAFD12345601400</UDN>
<roomName>Living &amp; Dining</roomName>
<deviceList><device><UDN>uuid:RINCON_5CAAFD12345601400_MR</UDN></device></deviceList>
</device></root>"#;

    #[test]
    fn description_identity_comes_from_the_udn_mac() {
        let d = parse_description(DESCRIPTION).unwrap();
        assert!(d.is_sonos());
        assert_eq!(d.rincon().as_deref(), Some("RINCON_5CAAFD12345601400"));
        assert_eq!(d.hardware_id().as_deref(), Some("5CAAFD123456"));
        assert_eq!(d.model_label(), "Beam");
        assert_eq!(d.name(), "Living & Dining");
    }

    #[test]
    fn identity_falls_back_to_the_serial_mac_and_never_to_names() {
        let mut d = parse_description(DESCRIPTION).unwrap();
        d.udn = None;
        assert_eq!(d.hardware_id().as_deref(), Some("5CAAFD123456"));
        d.serial = None;
        assert_eq!(d.hardware_id(), None);
        d.udn = Some("uuid:not-a-rincon".into());
        assert_eq!(d.hardware_id(), None);
    }

    #[test]
    fn sub_device_udn_does_not_change_the_id() {
        let mut d = parse_description(DESCRIPTION).unwrap();
        d.udn = Some("uuid:RINCON_5CAAFD12345601400_MR".into());
        assert_eq!(d.rincon().as_deref(), Some("RINCON_5CAAFD12345601400"));
    }

    #[test]
    fn non_sonos_or_empty_descriptions_are_refused() {
        assert!(parse_description("<root></root>").is_none());
        let other = parse_description("<manufacturer>Denon</manufacturer>").unwrap();
        assert!(!other.is_sonos());
        assert_eq!(other.hardware_id(), None);
    }

    #[test]
    fn volume_and_mute_responses_parse() {
        // RenderingControl:1 response shape (SoCo/node-sonos).
        let volume = r#"<s:Envelope><s:Body><u:GetVolumeResponse xmlns:u="urn:schemas-upnp-org:service:RenderingControl:1"><CurrentVolume>27</CurrentVolume></u:GetVolumeResponse></s:Body></s:Envelope>"#;
        assert_eq!(parse_volume(volume), Some(27.0));
        assert_eq!(parse_volume("<CurrentVolume>0</CurrentVolume>"), Some(0.0));
        assert_eq!(
            parse_volume("<CurrentVolume>100</CurrentVolume>"),
            Some(100.0)
        );
        assert_eq!(parse_volume("<CurrentVolume>101</CurrentVolume>"), None);
        assert_eq!(parse_volume("<CurrentVolume>loud</CurrentVolume>"), None);
        assert_eq!(parse_volume("<nothing/>"), None);
        assert_eq!(parse_mute("<CurrentMute>1</CurrentMute>"), Some(true));
        assert_eq!(parse_mute("<CurrentMute>0</CurrentMute>"), Some(false));
        assert_eq!(parse_mute("<CurrentMute>2</CurrentMute>"), None);
        assert_eq!(parse_fixed("<CurrentFixed>1</CurrentFixed>"), Some(true));
        assert_eq!(parse_fixed("<CurrentFixed>0</CurrentFixed>"), Some(false));
    }

    #[test]
    fn fault_error_code_is_extracted() {
        let fault = r#"<s:Fault><faultstring>UPnPError</faultstring><detail><UPnPError xmlns="urn:schemas-upnp-org:control-1-0"><errorCode>701</errorCode></UPnPError></detail></s:Fault>"#;
        assert_eq!(parse_fault(fault).as_deref(), Some("701"));
        assert_eq!(parse_fault("<ok/>"), None);
    }

    #[test]
    fn volume_encodes_at_min_max_and_refuses_outside() {
        assert_eq!(encode_volume(0.0), Some(0));
        assert_eq!(encode_volume(100.0), Some(100));
        assert_eq!(encode_volume(12.4), Some(12));
        assert_eq!(encode_volume(100.1), None);
        assert_eq!(encode_volume(-1.0), None);
        assert_eq!(encode_volume(f32::NAN), None);
    }

    #[test]
    fn set_volume_envelope_matches_the_upnp_shape() {
        let calls = calls_for(&DeviceOperation::SetVolume(35.0), "RINCON_X", &[], false).unwrap();
        assert_eq!(calls.len(), 1);
        let call = &calls[0];
        assert_eq!(
            call.soap_action(),
            "\"urn:schemas-upnp-org:service:RenderingControl:1#SetVolume\""
        );
        assert_eq!(
            call.service.control_path(),
            "/MediaRenderer/RenderingControl/Control"
        );
        let body = call.envelope();
        assert!(body.contains(
            "<u:SetVolume xmlns:u=\"urn:schemas-upnp-org:service:RenderingControl:1\">\
             <InstanceID>0</InstanceID><Channel>Master</Channel><DesiredVolume>35</DesiredVolume>\
             </u:SetVolume>"
        ));
    }

    #[test]
    fn mute_encodes_and_fixed_output_refuses_volume() {
        let calls = calls_for(&DeviceOperation::SetMute(true), "R", &[], false).unwrap();
        assert!(calls[0].envelope().contains("<DesiredMute>1</DesiredMute>"));
        assert!(calls_for(&DeviceOperation::SetVolume(10.0), "R", &[], true).is_err());
        assert!(calls_for(&DeviceOperation::SetVolume(101.0), "R", &[], false).is_err());
    }

    #[test]
    fn input_selection_uses_soco_stream_uris() {
        let inputs = inputs_for_model("Sonos Beam");
        let calls = calls_for(
            &DeviceOperation::SelectInput("tv".into()),
            "RINCON_5CAAFD12345601400",
            &inputs,
            false,
        )
        .unwrap();
        assert_eq!(calls[0].action, "SetAVTransportURI");
        assert!(calls[0]
            .envelope()
            .contains("<CurrentURI>x-sonos-htastream:RINCON_5CAAFD12345601400:spdif</CurrentURI>"));
        assert_eq!(calls[1].action, "Play");
        // Beam has no line-in: refused, never sent.
        assert!(calls_for(
            &DeviceOperation::SelectInput("line-in".into()),
            "R",
            &inputs,
            false
        )
        .is_err());
        assert_eq!(
            input_uri(LINE_IN, "RINCON_A01400").as_deref(),
            Some("x-rincon-stream:RINCON_A01400")
        );
    }

    #[test]
    fn current_input_comes_from_the_track_uri() {
        assert_eq!(
            input_for_uri("x-rincon-stream:RINCON_A01400"),
            Some(LINE_IN)
        );
        assert_eq!(
            input_for_uri("x-sonos-htastream:RINCON_A01400:spdif"),
            Some(TV)
        );
        assert_eq!(input_for_uri("x-sonos-spotify:abc"), None);
        let info = "<TrackURI>x-sonos-htastream:RINCON_A01400:spdif</TrackURI>";
        assert_eq!(
            parse_track_uri(info).as_deref(),
            Some("x-sonos-htastream:RINCON_A01400:spdif")
        );
    }

    #[test]
    fn model_catalogs_are_conservative() {
        let ids =
            |m: &str| -> Vec<String> { inputs_for_model(m).into_iter().map(|i| i.id).collect() };
        assert_eq!(ids("Sonos Beam"), vec!["tv"]);
        assert_eq!(ids("Sonos Five"), vec!["line-in"]);
        assert_eq!(ids("Sonos Amp"), vec!["line-in", "tv"]);
        assert_eq!(ids("Sonos Connect:Amp"), vec!["line-in"]);
        assert!(ids("Sonos One").is_empty());
        assert!(ids("Sonos Mystery Model").is_empty());
    }

    #[test]
    fn power_is_refused() {
        for op in [DeviceOperation::PowerOn, DeviceOperation::PowerStandby] {
            assert!(calls_for(&op, "R", &[], false)
                .unwrap_err()
                .contains("not supported"));
        }
    }

    #[test]
    fn envelope_escapes_arguments() {
        let call = SoapCall::new(
            Service::AvTransport,
            "SetAVTransportURI",
            vec![("CurrentURI", "a&b<c>".to_string())],
        );
        assert!(call
            .envelope()
            .contains("<CurrentURI>a&amp;b&lt;c&gt;</CurrentURI>"));
    }
}
