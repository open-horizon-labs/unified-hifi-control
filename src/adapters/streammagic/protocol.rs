//! StreamMagic (Cambridge Audio) wire formats: pure encode/parse, no I/O.
//!
//! Spec: the `aiostreammagic` client (MIT) —
//! <https://github.com/noahhusby/aiostreammagic> (`stream_magic.py`, `endpoints.py`,
//! `models.py`). The protocol is JSON text frames over a WebSocket at `ws://<host>/smoip`:
//!
//! - request: `{"path": "/zone/state", "params": {"zone": "ZONE1", "mute": true}}`
//! - subscribe: the same frame with `{"update": 100, "zone": "ZONE1"}` as params
//! - response: `{"path", "type": "response", "result": 200, "message": "OK", "params": {"data": {..}}}`
//! - push: a frame on a subscribed path carrying `params.data`
//!
//! Every example in the tests below is shaped from those models (field names are the models'
//! `alias`es). **Synthetic**: no frame here was captured from a unit.

use serde_json::{json, Map, Value};

use crate::devices::{DeviceInput, DevicePower};

pub const SMOIP_PATH: &str = "/smoip";
pub const ZONE: &str = "ZONE1";

pub const PATH_INFO: &str = "/system/info";
pub const PATH_SOURCES: &str = "/system/sources";
pub const PATH_POWER: &str = "/system/power";
pub const PATH_ZONE_STATE: &str = "/zone/state";
/// Read only: the unit's own volume limit (`volume_limit_percent`) lives here.
pub const PATH_AUDIO: &str = "/zone/audio";

/// Paths subscribed on connect. All read-only: subscribing never changes the unit.
pub const SUBSCRIPTIONS: &[&str] = &[
    PATH_INFO,
    PATH_SOURCES,
    PATH_ZONE_STATE,
    PATH_POWER,
    PATH_AUDIO,
];

/// mDNS service types the family browses (catalog: `docs/uhc-native-lan-client-catalog.md` §5).
pub const MDNS_SERVICE_TYPES: &[&str] = &["_stream-magic._tcp.local.", "_smoip._tcp.local."];

/// aiostreammagic hard-codes port 80 for the WebSocket.
pub const DEFAULT_PORT: u16 = 80;

/// The lowest and highest percentage the protocol accepts
/// (`set_volume` raises outside `0..=100` in aiostreammagic).
pub const VOLUME_MIN_PERCENT: f32 = 0.0;
pub const VOLUME_MAX_PERCENT: f32 = 100.0;

fn frame(path: &str, params: Value) -> String {
    json!({ "path": path, "params": params }).to_string()
}

/// A plain query: the request with no params.
pub fn query(path: &str) -> String {
    frame(path, Value::Object(Map::new()))
}

pub fn subscribe(path: &str) -> String {
    frame(path, json!({ "update": 100, "zone": ZONE }))
}

/// `volume_percent`. Refuses anything outside `0..=100` or non-finite; the value is rounded to
/// the integer the unit takes.
pub fn set_volume_percent(percent: f32) -> Option<String> {
    if !percent.is_finite() || !(VOLUME_MIN_PERCENT..=VOLUME_MAX_PERCENT).contains(&percent) {
        return None;
    }
    Some(frame(
        PATH_ZONE_STATE,
        json!({ "zone": ZONE, "volume_percent": percent.round() as u32 }),
    ))
}

pub fn set_mute(muted: bool) -> String {
    frame(PATH_ZONE_STATE, json!({ "zone": ZONE, "mute": muted }))
}

pub fn set_source(source_id: &str) -> String {
    frame(
        PATH_ZONE_STATE,
        json!({ "zone": ZONE, "source": source_id }),
    )
}

/// `power_on`: `/system/power?power=ON`.
pub fn power_on() -> String {
    frame(PATH_POWER, json!({ "power": "ON" }))
}

/// Standby that keeps the network up (`NETWORK`), so UHC can wake the unit again. ECO mode
/// would drop networking, which is a unit setting UHC does not touch.
pub fn power_network_standby() -> String {
    frame(PATH_POWER, json!({ "power": "NETWORK" }))
}

/// One frame from the unit.
#[derive(Debug, Clone, PartialEq)]
pub struct Incoming {
    pub path: String,
    /// `response` for a reply to a request; other values are pushes.
    pub kind: Option<String>,
    pub result: Option<i64>,
    pub message: Option<String>,
    pub data: Option<Value>,
}

impl Incoming {
    pub fn is_response(&self) -> bool {
        self.kind.as_deref() == Some("response")
    }

    /// A response that is not `result: 200`.
    pub fn error(&self) -> Option<String> {
        match self.result {
            Some(200) | None => None,
            Some(code) => Some(format!(
                "{} (result {code})",
                self.message.as_deref().unwrap_or("rejected")
            )),
        }
    }
}

/// Parse a text frame. `None` for anything that is not a JSON object with a `path`.
pub fn parse_message(text: &str) -> Option<Incoming> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let path = object.get("path")?.as_str()?.to_string();
    Some(Incoming {
        path,
        kind: object
            .get("type")
            .and_then(Value::as_str)
            .map(str::to_string),
        result: object.get("result").and_then(Value::as_i64),
        message: object
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_string),
        data: object
            .get("params")
            .and_then(|params| params.get("data"))
            .cloned(),
    })
}

/// `/system/info` (`Info` model): the unit's identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Info {
    pub name: String,
    pub model: String,
    pub udn: Option<String>,
    pub unit_id: Option<String>,
}

impl Info {
    pub fn from_data(data: &Value) -> Option<Self> {
        let text = |key: &str| {
            data.get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_string)
        };
        let info = Self {
            name: text("name").unwrap_or_default(),
            model: text("model")?,
            udn: text("udn"),
            unit_id: text("unit_id"),
        };
        info.hardware_id()?;
        Some(info)
    }

    /// `unit_id` when present, otherwise the UDN's uuid. Never the model, name or address.
    pub fn hardware_id(&self) -> Option<String> {
        if let Some(unit_id) = &self.unit_id {
            return Some(unit_id.clone());
        }
        let udn = self.udn.as_deref()?;
        let uuid = udn.strip_prefix("uuid:").unwrap_or(udn).trim();
        (!uuid.is_empty()).then(|| uuid.to_string())
    }
}

/// `/zone/state` (`State` model), keeping only what a device needs. Absent fields stay `None`
/// so a partial push never erases what an earlier frame reported.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ZoneState {
    pub source: Option<String>,
    pub power: Option<bool>,
    pub pre_amp_mode: Option<bool>,
    pub volume_percent: Option<f32>,
    pub mute: Option<bool>,
}

impl ZoneState {
    pub fn from_data(data: &Value) -> Self {
        Self {
            source: data
                .get("source")
                .and_then(Value::as_str)
                .map(str::to_string),
            power: data.get("power").and_then(Value::as_bool),
            pre_amp_mode: data.get("pre_amp_mode").and_then(Value::as_bool),
            volume_percent: data
                .get("volume_percent")
                .and_then(Value::as_f64)
                .map(|v| v as f32)
                .filter(|v| (VOLUME_MIN_PERCENT..=VOLUME_MAX_PERCENT).contains(v)),
            mute: data.get("mute").and_then(Value::as_bool),
        }
    }
}

/// `/system/sources` (`Source` model). A source that is not `ui_selectable` is listed as
/// unavailable rather than dropped.
pub fn parse_sources(data: &Value) -> Vec<DeviceInput> {
    let Some(sources) = data.get("sources").and_then(Value::as_array) else {
        return Vec::new();
    };
    sources
        .iter()
        .filter_map(|source| {
            let id = source.get("id").and_then(Value::as_str)?.to_string();
            let name = source
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .or_else(|| source.get("default_name").and_then(Value::as_str))
                .unwrap_or(&id)
                .to_string();
            let available = source
                .get("ui_selectable")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Some(DeviceInput {
                id,
                name,
                available,
            })
        })
        .collect()
}

/// `/system/power`: the `power` string (`ON`, `NETWORK`, or an idle/standby spelling). Anything
/// unrecognised is `None`, never rounded to standby.
pub fn parse_power(data: &Value) -> Option<DevicePower> {
    match data.get("power")?.as_str()?.to_ascii_uppercase().as_str() {
        "ON" => Some(DevicePower::On),
        "NETWORK" | "IDLE" | "STANDBY" | "ECO_MODE" => Some(DevicePower::Standby),
        _ => None,
    }
}

/// `/zone/audio` `volume_limit_percent` (`Audio` model, 1..=100): the unit's own maximum.
pub fn parse_volume_limit(data: &Value) -> Option<f32> {
    let limit = data.get("volume_limit_percent")?.as_f64()? as f32;
    (1.0..=VOLUME_MAX_PERCENT).contains(&limit).then_some(limit)
}

/// Where a browsed service answers the control protocol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub host: String,
    pub port: u16,
}

/// Turn a resolved mDNS service into the endpoint to try. `_smoip` advertises the control
/// port; `_stream-magic` is the app-facing record and the WebSocket stays on port 80
/// (aiostreammagic's constant). IPv4 only: link-local IPv6 needs a scope id the WebSocket URL
/// cannot carry.
pub fn endpoint_from_service(
    service_type: &str,
    addresses: &[std::net::IpAddr],
    advertised_port: u16,
) -> Option<Endpoint> {
    let address = addresses.iter().find(|a| a.is_ipv4())?;
    let port = if service_type.starts_with("_smoip.") && advertised_port != 0 {
        advertised_port
    } else {
        DEFAULT_PORT
    };
    Some(Endpoint {
        host: address.to_string(),
        port,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parsed(text: &str) -> Value {
        serde_json::from_str(text).unwrap()
    }

    #[test]
    fn requests_match_the_documented_frames() {
        assert_eq!(
            parsed(&query(PATH_INFO)),
            json!({"path": "/system/info", "params": {}})
        );
        assert_eq!(
            parsed(&subscribe(PATH_ZONE_STATE)),
            json!({"path": "/zone/state", "params": {"update": 100, "zone": "ZONE1"}})
        );
        assert_eq!(
            parsed(&set_volume_percent(42.0).unwrap()),
            json!({"path": "/zone/state", "params": {"zone": "ZONE1", "volume_percent": 42}})
        );
        assert_eq!(
            parsed(&set_mute(true)),
            json!({"path": "/zone/state", "params": {"zone": "ZONE1", "mute": true}})
        );
        assert_eq!(
            parsed(&set_source("MEDIA_PLAYER")),
            json!({"path": "/zone/state", "params": {"zone": "ZONE1", "source": "MEDIA_PLAYER"}})
        );
        assert_eq!(
            parsed(&power_on()),
            json!({"path": "/system/power", "params": {"power": "ON"}})
        );
        assert_eq!(
            parsed(&power_network_standby()),
            json!({"path": "/system/power", "params": {"power": "NETWORK"}})
        );
    }

    #[test]
    fn volume_encodes_at_the_bounds_and_refuses_outside_them() {
        assert!(set_volume_percent(0.0).is_some());
        assert!(set_volume_percent(100.0).is_some());
        assert!(set_volume_percent(-1.0).is_none());
        assert!(set_volume_percent(100.5).is_none());
        assert!(set_volume_percent(f32::NAN).is_none());
        assert!(set_volume_percent(f32::INFINITY).is_none());
        // Step is one percent: a fractional level rounds to the integer the unit takes.
        assert_eq!(
            parsed(&set_volume_percent(41.6).unwrap())["params"]["volume_percent"],
            json!(42)
        );
    }

    #[test]
    fn info_response_yields_identity_from_unit_id() {
        let text = r#"{"path":"/system/info","type":"response","result":200,"message":"OK",
            "params":{"data":{"name":"Living Room","model":"Evo 150","timezone":"UTC",
            "locale":"en_US","udn":"uuid:8e1d1f3c-0000-1000-8000-00aabbccddee",
            "unit_id":"0AABBCCDDEE0","api":"1.7"}}}"#;
        let message = parse_message(text).unwrap();
        assert!(message.is_response());
        assert_eq!(message.error(), None);
        let info = Info::from_data(message.data.as_ref().unwrap()).unwrap();
        assert_eq!(info.model, "Evo 150");
        assert_eq!(info.hardware_id().as_deref(), Some("0AABBCCDDEE0"));
    }

    #[test]
    fn identity_falls_back_to_the_udn_and_never_to_name_or_model() {
        let info = Info::from_data(&json!({
            "name": "x", "model": "CXN", "udn": "uuid:1234-ABCD"
        }))
        .unwrap();
        assert_eq!(info.hardware_id().as_deref(), Some("1234-ABCD"));
        assert!(Info::from_data(&json!({"name": "x", "model": "CXN"})).is_none());
        assert!(Info::from_data(&json!({"unit_id": "A"})).is_none());
    }

    #[test]
    fn zone_state_reads_the_state_model() {
        let data = json!({"source":"MEDIA_PLAYER","power":true,"pre_amp_mode":true,
            "pre_amp_state":true,"volume_percent":35,"volume_db":-40,"mute":false,
            "cbus":"off","standby_mode":"NETWORK"});
        let state = ZoneState::from_data(&data);
        assert_eq!(state.source.as_deref(), Some("MEDIA_PLAYER"));
        assert_eq!(state.power, Some(true));
        assert_eq!(state.pre_amp_mode, Some(true));
        assert_eq!(state.volume_percent, Some(35.0));
        assert_eq!(state.mute, Some(false));
    }

    #[test]
    fn partial_and_out_of_range_state_does_not_invent_values() {
        let state = ZoneState::from_data(&json!({"mute": true}));
        assert_eq!(state.volume_percent, None);
        assert_eq!(state.power, None);
        let state = ZoneState::from_data(&json!({"volume_percent": 250}));
        assert_eq!(state.volume_percent, None);
    }

    #[test]
    fn sources_list_marks_unselectable_inputs_unavailable() {
        let data = json!({"sources":[
            {"id":"MEDIA_PLAYER","name":"Media Player","default_name":"Media Player",
             "nameable":false,"ui_selectable":true,"description":"x","description_locale":"x"},
            {"id":"IR","name":"","default_name":"Optical","nameable":true,"ui_selectable":false,
             "description":"x","description_locale":"x"}
        ]});
        let inputs = parse_sources(&data);
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[0].id, "MEDIA_PLAYER");
        assert!(inputs[0].available);
        assert_eq!(inputs[1].name, "Optical");
        assert!(!inputs[1].available);
        assert!(parse_sources(&json!({})).is_empty());
    }

    #[test]
    fn power_strings_map_and_unknown_stays_unknown() {
        assert_eq!(parse_power(&json!({"power":"ON"})), Some(DevicePower::On));
        assert_eq!(
            parse_power(&json!({"power":"NETWORK"})),
            Some(DevicePower::Standby)
        );
        assert_eq!(parse_power(&json!({"power":"???"})), None);
        assert_eq!(parse_power(&json!({})), None);
    }

    #[test]
    fn volume_limit_is_the_units_own_maximum() {
        assert_eq!(
            parse_volume_limit(&json!({"volume_limit_percent": 80})),
            Some(80.0)
        );
        assert_eq!(
            parse_volume_limit(&json!({"volume_limit_percent": 0})),
            None
        );
        assert_eq!(
            parse_volume_limit(&json!({"volume_limit_percent": 101})),
            None
        );
        assert_eq!(parse_volume_limit(&json!({})), None);
    }

    #[test]
    fn rejected_responses_carry_the_units_message() {
        let message = parse_message(
            r#"{"path":"/zone/state","type":"response","result":400,"message":"bad param"}"#,
        )
        .unwrap();
        assert_eq!(message.error().as_deref(), Some("bad param (result 400)"));
    }

    #[test]
    fn malformed_frames_are_refused() {
        assert!(parse_message("not json").is_none());
        assert!(parse_message("[1,2]").is_none());
        assert!(parse_message(r#"{"type":"response"}"#).is_none());
    }

    #[test]
    fn endpoints_prefer_ipv4_and_the_smoip_port() {
        let v4: std::net::IpAddr = "192.168.1.20".parse().unwrap();
        let v6: std::net::IpAddr = "fe80::1".parse().unwrap();
        assert_eq!(
            endpoint_from_service("_smoip._tcp.local.", &[v6, v4], 8080),
            Some(Endpoint {
                host: "192.168.1.20".into(),
                port: 8080
            })
        );
        assert_eq!(
            endpoint_from_service("_stream-magic._tcp.local.", &[v4], 12345),
            Some(Endpoint {
                host: "192.168.1.20".into(),
                port: 80
            })
        );
        assert_eq!(endpoint_from_service("_smoip._tcp.local.", &[v6], 80), None);
    }
}
