//! Yamaha MusicCast (Yamaha Extended Control, "YXC") wire formats: HTTP GET requests answered by
//! JSON documents. Pure functions only, so every encoding decision is unit-tested.
//!
//! # Sources (no hardware was available; every example below is quoted or shaped from them)
//!
//! - YXC request/response shapes and endpoint paths: `aiomusiccast` (MIT, (c) vigonotion),
//!   `aiomusiccast/pyamaha.py` and `musiccast_device.py` on `main` as of 2026-10-12.
//!   <https://github.com/vigonotion/aiomusiccast>
//! - Second reference for `getFeatures` / `getStatus` field names:
//!   <https://github.com/foxthefox/yamaha-yxc-nodejs>
//! - The Yamaha "Extended Control API Specification (Basic)" documents the same fields; it is
//!   distributed by Yamaha to developers and was not fetched here.
//!
//! Anything the sources do not pin down is marked implementation-derived.

use std::collections::HashMap;

use serde_json::Value;

use crate::bus::VolumeScale;
use crate::devices::service::DeviceOperation;
use crate::devices::CapabilityEvidence;
use crate::devices::{DeviceCapabilities, DeviceCapability, DeviceInput, DevicePower};

/// Base path of the YXC API on the device (aiomusiccast `pyamaha.py`).
pub const CONTROL_PATH: &str = "/YamahaExtendedControl/v1";
/// The only zone UHC models: the amplifier's main zone. Zones 2-4 are not devices of their own.
pub const ZONE: &str = "main";
/// SSDP `manufacturer` the candidate filter matches (a candidate filter only: the device must
/// also answer the YXC probe before it is accepted).
pub const MANUFACTURER: &str = "Yamaha Corporation";

/// `response_code` meanings from the YXC specification, as listed by `aiomusiccast`/`yxc-nodejs`.
pub fn response_code_name(code: i64) -> &'static str {
    match code {
        0 => "successful request",
        1 => "initializing",
        2 => "internal error",
        3 => "invalid request (unsupported on this model or source)",
        4 => "invalid parameter (outside the device's range)",
        5 => "guarded (the device refuses this in its current state)",
        6 => "time out",
        99 => "firmware updating",
        100..=103 => "access error (the device refused the request)",
        104..=106 => "other errors (the device refused the request)",
        _ => "unknown error",
    }
}

/// Check a command response. YXC answers HTTP 200 with a JSON body whose `response_code` is the
/// real status.
pub fn check_response(body: &str) -> Result<(), String> {
    let json: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    match json.get("response_code").and_then(as_i64) {
        Some(0) => Ok(()),
        Some(code) => Err(format!(
            "device answered response_code {code}: {}",
            response_code_name(code)
        )),
        None => Err("response has no response_code".to_string()),
    }
}

fn as_i64(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) => n.as_i64().or_else(|| {
            n.as_f64()
                .filter(|f| f.is_finite() && f.fract() == 0.0)
                .map(|f| f as i64)
        }),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn as_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(n) => n.as_f64().filter(|f| f.is_finite()),
        Value::String(s) => s.trim().parse().ok().filter(|f: &f64| f.is_finite()),
        _ => None,
    }
}

fn parse_ok(body: &str) -> Result<Value, String> {
    let json: Value = serde_json::from_str(body).map_err(|e| format!("not JSON: {e}"))?;
    check_response(body)?;
    Ok(json)
}

// =============================================================================
// system/getDeviceInfo
// =============================================================================

/// `system/getDeviceInfo`.
///
/// Shape (aiomusiccast / YXC spec): `{"response_code":0,"model_name":"RX-V481",
/// "destination":"BG","device_id":"00A0DEAD07DA","system_id":"0A3F5B2C","system_version":2.81,
/// "api_version":1.17,...}`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct DeviceInfo {
    pub model_name: String,
    pub device_id: Option<String>,
    pub system_id: Option<String>,
    pub serial_number: Option<String>,
    pub api_version: Option<f64>,
}

impl DeviceInfo {
    /// The hardware identity: `device_id` (the unit's MAC-derived id), else a serial number, else
    /// the `system_id`. Never the model or name: two identical units differ only here.
    pub fn hardware_id(&self) -> Option<String> {
        [&self.device_id, &self.serial_number, &self.system_id]
            .into_iter()
            .flatten()
            .map(|id| id.trim())
            .find(|id| !id.is_empty())
            .map(str::to_string)
    }
}

pub fn parse_device_info(body: &str) -> Result<DeviceInfo, String> {
    let json = parse_ok(body)?;
    let text = |key: &str| {
        json.get(key)
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
    };
    let model_name =
        text("model_name").ok_or_else(|| "getDeviceInfo has no model_name".to_string())?;
    let info = DeviceInfo {
        model_name,
        device_id: text("device_id"),
        system_id: text("system_id"),
        serial_number: text("serial_number"),
        api_version: json.get("api_version").and_then(as_f64),
    };
    if info.hardware_id().is_none() {
        return Err("getDeviceInfo carries no device_id, serial_number or system_id".to_string());
    }
    Ok(info)
}

// =============================================================================
// system/getFeatures
// =============================================================================

/// The `volume` entry of a zone's `range_step`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VolumeRange {
    pub min: i64,
    pub max: i64,
    pub step: i64,
}

impl VolumeRange {
    /// `Percentage` only when the device's native range is exactly 0-100. Anything else (YXC
    /// commonly reports 0-161 or 0-60) is carried in its own units, because relabelling it as a
    /// percentage would make "50" mean a different loudness on every model.
    pub fn scale(&self) -> VolumeScale {
        if self.min == 0 && self.max == 100 {
            VolumeScale::Percentage
        } else {
            VolumeScale::Steps
        }
    }
}

/// What `system/getFeatures` says the main zone can do.
///
/// Shape: `{"system":{"func_list":[...],"zone_num":1,"input_list":[{"id":"hdmi1",...}]},
/// "zone":[{"id":"main","func_list":["power","sleep","volume","mute",...],
/// "input_list":["hdmi1","net_radio",...],
/// "range_step":[{"id":"volume","min":0,"max":161,"step":1},...],
/// "actual_volume_mode_list":["db","numeric"]}]}`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Features {
    /// The main zone's `func_list`.
    pub functions: Vec<String>,
    pub volume: Option<VolumeRange>,
    /// Input ids the main zone can select, in the device's order.
    pub inputs: Vec<String>,
    /// Whether the zone reports an `actual_volume` in dB (`actual_volume_mode_list` has `db`).
    pub actual_volume_db: bool,
}

impl Features {
    pub fn has(&self, function: &str) -> bool {
        self.functions.iter().any(|f| f == function)
    }
}

pub fn parse_features(body: &str) -> Result<Features, String> {
    let json = parse_ok(body)?;
    let zones = json
        .get("zone")
        .and_then(Value::as_array)
        .ok_or_else(|| "getFeatures has no zone list".to_string())?;
    let main = zones
        .iter()
        .find(|zone| zone.get("id").and_then(Value::as_str) == Some(ZONE))
        .ok_or_else(|| "getFeatures lists no main zone".to_string())?;
    let strings = |key: &str| -> Vec<String> {
        main.get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect()
            })
            .unwrap_or_default()
    };
    let volume = main
        .get("range_step")
        .and_then(Value::as_array)
        .and_then(|ranges| {
            ranges
                .iter()
                .find(|r| r.get("id").and_then(Value::as_str) == Some("volume"))
        })
        .and_then(|range| {
            let min = range.get("min").and_then(as_i64)?;
            let max = range.get("max").and_then(as_i64)?;
            let step = range.get("step").and_then(as_i64).unwrap_or(1);
            (max > min && step > 0).then_some(VolumeRange { min, max, step })
        });
    Ok(Features {
        functions: strings("func_list"),
        volume,
        inputs: strings("input_list"),
        actual_volume_db: strings("actual_volume_mode_list").iter().any(|m| m == "db"),
    })
}

// =============================================================================
// system/getNameText, system/getNetworkStatus
// =============================================================================

/// `system/getNameText` (no `id`): `{"zone_list":[{"id":"main","text":"Living Room"}],
/// "input_list":[{"id":"hdmi1","text":"TV"}],...}`. Returns input id -> the name the user gave it.
pub fn parse_input_names(body: &str) -> Result<HashMap<String, String>, String> {
    let json = parse_ok(body)?;
    Ok(json
        .get("input_list")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    Some((
                        item.get("id")?.as_str()?.to_string(),
                        item.get("text")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// `system/getNetworkStatus`: `{"network_name":"Living Room",...}`. The unit's friendly name.
pub fn parse_network_name(body: &str) -> Option<String> {
    parse_ok(body)
        .ok()?
        .get("network_name")?
        .as_str()
        .map(str::to_string)
        .filter(|name| !name.trim().is_empty())
}

/// Build the input catalog: the zone's selectable ids, named by the user's renames where known.
pub fn build_inputs(features: &Features, names: &HashMap<String, String>) -> Vec<DeviceInput> {
    features
        .inputs
        .iter()
        .map(|id| DeviceInput {
            id: id.clone(),
            name: names
                .get(id)
                .filter(|name| !name.trim().is_empty())
                .cloned()
                .unwrap_or_else(|| id.clone()),
            available: true,
        })
        .collect()
}

// =============================================================================
// main/getStatus
// =============================================================================

/// `actual_volume`: `{"mode":"db","value":-40.5,"unit":"dB"}`. Present on models that report the
/// level in decibels as well as in native units.
#[derive(Debug, Clone, PartialEq)]
pub struct ActualVolume {
    pub value: f64,
    pub unit: String,
}

impl ActualVolume {
    pub fn display(&self) -> String {
        let unit = if self.unit.is_empty() {
            "dB"
        } else {
            &self.unit
        };
        format!("{} {unit}", self.value)
    }
}

/// `main/getStatus`: `{"response_code":0,"power":"on","sleep":0,"volume":80,"mute":false,
/// "max_volume":161,"input":"hdmi1","distribution_enable":true,"sound_program":"straight",
/// "actual_volume":{"mode":"db","value":-40.5,"unit":"dB"},...}`.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct MainStatus {
    pub power: Option<DevicePower>,
    pub volume: Option<i64>,
    /// The unit's own volume limit (`max_volume`), which can be below the range maximum.
    pub max_volume: Option<i64>,
    pub mute: Option<bool>,
    pub input: Option<String>,
    pub actual_volume: Option<ActualVolume>,
}

pub fn parse_status(body: &str) -> Result<MainStatus, String> {
    let json = parse_ok(body)?;
    let power = match json.get("power").and_then(Value::as_str) {
        Some("on") => Some(DevicePower::On),
        Some("standby") => Some(DevicePower::Standby),
        _ => None,
    };
    Ok(MainStatus {
        power,
        volume: json.get("volume").and_then(as_i64),
        max_volume: json.get("max_volume").and_then(as_i64),
        mute: json.get("mute").and_then(Value::as_bool),
        input: json
            .get("input")
            .and_then(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.is_empty()),
        actual_volume: json.get("actual_volume").and_then(|actual| {
            Some(ActualVolume {
                value: as_f64(actual.get("value")?)?,
                unit: actual
                    .get("unit")
                    .and_then(Value::as_str)
                    .unwrap_or("dB")
                    .to_string(),
            })
        }),
    })
}

// =============================================================================
// SSDP device description
// =============================================================================

/// The fields of the UPnP description (`LOCATION`) that matter for discovery.
#[derive(Debug, Clone, PartialEq)]
pub struct Description {
    pub manufacturer: String,
    pub model: String,
    pub friendly_name: String,
    /// `<yamaha:X_yxcControlURL>`: present only on YXC (MusicCast) devices.
    pub yxc_control_url: Option<String>,
    /// `<yamaha:X_URLBase>`, e.g. `http://192.168.1.20:80/`: where the YXC API listens. The
    /// description itself is served on a different port.
    pub url_base: Option<String>,
}

impl Description {
    /// The YXC HTTP port: the `X_URLBase` port, else 80.
    pub fn control_port(&self) -> u16 {
        self.url_base
            .as_deref()
            .and_then(|base| url::Url::parse(base).ok())
            .and_then(|url| url.port_or_known_default())
            .unwrap_or(80)
    }

    /// Candidate filter: Yamaha *and* advertising a YXC control URL. A legacy (RX-V "YNCA")
    /// receiver is also a Yamaha MediaRenderer; it is not MusicCast, and has no YXC API.
    pub fn is_candidate(&self) -> bool {
        self.manufacturer.contains("Yamaha") && self.yxc_control_url.is_some()
    }
}

fn tag_text(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let start = xml.find(&open)? + open.len();
    let end = xml[start..].find(&format!("</{tag}>"))? + start;
    let text = xml[start..end].trim();
    (!text.is_empty()).then(|| text.to_string())
}

pub fn parse_description(xml: &str) -> Option<Description> {
    Some(Description {
        manufacturer: tag_text(xml, "manufacturer")?,
        model: tag_text(xml, "modelName").unwrap_or_default(),
        friendly_name: tag_text(xml, "friendlyName").unwrap_or_default(),
        yxc_control_url: tag_text(xml, "yamaha:X_yxcControlURL"),
        url_base: tag_text(xml, "yamaha:X_URLBase"),
    })
}

// =============================================================================
// Commands
// =============================================================================

pub fn status_path() -> String {
    format!("{CONTROL_PATH}/{ZONE}/getStatus")
}

pub fn device_info_path() -> String {
    format!("{CONTROL_PATH}/system/getDeviceInfo")
}

pub fn features_path() -> String {
    format!("{CONTROL_PATH}/system/getFeatures")
}

pub fn name_text_path() -> String {
    format!("{CONTROL_PATH}/system/getNameText")
}

pub fn network_status_path() -> String {
    format!("{CONTROL_PATH}/system/getNetworkStatus")
}

/// Encode a native volume level. Refuses non-finite and out-of-range levels instead of clamping:
/// the service has already clamped and quantised, so an out-of-range level here is a bug, and a
/// guess must never reach an amplifier.
pub fn encode_volume(level: f32, range: &VolumeRange, limit: Option<i64>) -> Result<i64, String> {
    if !level.is_finite() {
        return Err("volume must be a finite number".to_string());
    }
    let max = limit.map_or(range.max, |limit| limit.min(range.max));
    let steps = ((f64::from(level) - range.min as f64) / range.step as f64).round();
    let value = range.min + steps as i64 * range.step;
    if value < range.min || value > max {
        return Err(format!(
            "volume {level} is outside the device range {}..{max}",
            range.min
        ));
    }
    Ok(value)
}

/// The request path for one operation. `SetVolume` needs the range, and every input must be in
/// the catalog the device reported.
pub fn encode_operation(
    operation: &DeviceOperation,
    range: Option<&VolumeRange>,
    limit: Option<i64>,
    inputs: &[DeviceInput],
) -> Result<String, String> {
    match operation {
        DeviceOperation::SetVolume(level) => {
            let range = range.ok_or_else(|| {
                "volume control is not supported by this device (no volume range in getFeatures)"
                    .to_string()
            })?;
            let value = encode_volume(*level, range, limit)?;
            Ok(format!("{CONTROL_PATH}/{ZONE}/setVolume?volume={value}"))
        }
        DeviceOperation::SetMute(muted) => {
            Ok(format!("{CONTROL_PATH}/{ZONE}/setMute?enable={muted}"))
        }
        DeviceOperation::PowerOn => Ok(format!("{CONTROL_PATH}/{ZONE}/setPower?power=on")),
        DeviceOperation::PowerStandby => {
            Ok(format!("{CONTROL_PATH}/{ZONE}/setPower?power=standby"))
        }
        DeviceOperation::SelectInput(input) => {
            if !inputs.iter().any(|candidate| &candidate.id == input) {
                return Err(format!("{input} is not an input this device reported"));
            }
            Ok(format!(
                "{CONTROL_PATH}/{ZONE}/setInput?input={}",
                urlencoding::encode(input)
            ))
        }
    }
}

// =============================================================================
// Capabilities
// =============================================================================

/// Capability claims for one model, from what `getFeatures` reported. Pure.
///
/// All claims are implementation-derived: no MusicCast hardware has been exercised.
pub fn capabilities_for(model: &str, features: &Features) -> DeviceCapabilities {
    let claim = |supported: bool, detail: String| DeviceCapability {
        supported,
        evidence: CapabilityEvidence::ImplementationDerived,
        detail,
    };
    let pending = "implemented from the YXC specification and aiomusiccast; no MusicCast hardware \
                   exercised yet";
    let scale = match features.volume {
        Some(range) if range.scale() == VolumeScale::Percentage => {
            "native range 0-100, reported as a percentage".to_string()
        }
        Some(range) => format!(
            "native units {}..{} step {} (not a percentage and not dB; the model's own scale)",
            range.min, range.max, range.step
        ),
        None => String::new(),
    };
    DeviceCapabilities {
        observe: claim(
            true,
            format!("main/getStatus polled over HTTP for {model}; {pending}"),
        ),
        volume: claim(
            features.has("volume") && features.volume.is_some(),
            if features.has("volume") && features.volume.is_some() {
                format!("main/setVolume, {scale}, limited by the unit's max_volume; {pending}")
            } else {
                format!(
                    "{model} does not list volume with a range in getFeatures (fixed output or \
                     source-controlled); {pending}"
                )
            },
        ),
        mute: claim(
            features.has("mute"),
            if features.has("mute") {
                format!("main/setMute; {pending}")
            } else {
                format!("{model} does not list mute in getFeatures; {pending}")
            },
        ),
        power: claim(
            features.has("power"),
            if features.has("power") {
                format!(
                    "main/setPower on|standby over HTTP. Waking a unit in standby needs network \
                     standby enabled on the device; otherwise it does not answer; {pending}"
                )
            } else {
                format!("{model} does not list power in getFeatures; {pending}")
            },
        ),
        input_select: claim(
            !features.inputs.is_empty(),
            if features.inputs.is_empty() {
                format!("{model} lists no selectable inputs for the main zone; {pending}")
            } else {
                format!("main/setInput with the ids from getFeatures; {pending}")
            },
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shaped from the `getDeviceInfo` example in the YXC spec / aiomusiccast docs.
    const DEVICE_INFO: &str = r#"{"response_code":0,"model_name":"RX-V481","destination":"BG",
        "device_id":"00A0DEAD07DA","system_id":"0A3F5B2C","system_version":2.81,
        "api_version":1.17,"netmodule_generation":1,"operation_mode":"normal"}"#;

    /// Shaped from the `getFeatures` response in the YXC spec.
    const FEATURES: &str = r#"{"response_code":0,
        "system":{"func_list":["wired_lan","wireless_lan"],"zone_num":1,
                  "input_list":[{"id":"hdmi1","distribution_enable":true}]},
        "zone":[{"id":"main","func_list":["power","sleep","volume","mute","sound_program"],
                 "input_list":["hdmi1","hdmi2","net_radio","airplay"],
                 "range_step":[{"id":"volume","min":0,"max":161,"step":1},
                               {"id":"subwoofer_volume","min":-12,"max":12,"step":1}],
                 "actual_volume_mode_list":["db","numeric"]}]}"#;

    const STATUS: &str = r#"{"response_code":0,"power":"on","sleep":0,"volume":80,"mute":false,
        "max_volume":161,"input":"hdmi1","distribution_enable":true,"sound_program":"straight",
        "actual_volume":{"mode":"db","value":-40.5,"unit":"dB"}}"#;

    #[test]
    fn device_info_gives_identity_from_device_id() {
        let info = parse_device_info(DEVICE_INFO).unwrap();
        assert_eq!(info.model_name, "RX-V481");
        assert_eq!(info.hardware_id().as_deref(), Some("00A0DEAD07DA"));
        assert_eq!(info.api_version, Some(1.17));
    }

    #[test]
    fn identity_falls_back_through_serial_then_system_id_never_the_model() {
        let info = parse_device_info(
            r#"{"response_code":0,"model_name":"WXA-50","system_id":"0A3F5B2C"}"#,
        )
        .unwrap();
        assert_eq!(info.hardware_id().as_deref(), Some("0A3F5B2C"));
        let info = parse_device_info(
            r#"{"response_code":0,"model_name":"WXA-50","device_id":"","serial_number":"XYZ1","system_id":"0A"}"#,
        )
        .unwrap();
        assert_eq!(info.hardware_id().as_deref(), Some("XYZ1"));
        // No identity at all: refused, not invented from the model name.
        assert!(parse_device_info(r#"{"response_code":0,"model_name":"WXA-50"}"#).is_err());
    }

    #[test]
    fn error_and_malformed_responses_are_refused() {
        assert!(parse_device_info("<html>not yxc</html>").is_err());
        assert!(parse_device_info(r#"{"response_code":3}"#).is_err());
        assert!(parse_features(r#"{"response_code":0}"#).is_err());
        assert!(parse_features(r#"{"response_code":0,"zone":[{"id":"zone2"}]}"#).is_err());
        assert!(parse_status(r#"{"response_code":4}"#).is_err());
        assert!(check_response(r#"{"response_code":0}"#).is_ok());
        assert!(check_response(r#"{"response_code":4}"#)
            .unwrap_err()
            .contains("invalid parameter"));
        assert!(check_response("{}").is_err());
    }

    #[test]
    fn features_report_range_inputs_and_functions() {
        let features = parse_features(FEATURES).unwrap();
        assert_eq!(
            features.volume,
            Some(VolumeRange {
                min: 0,
                max: 161,
                step: 1
            })
        );
        assert_eq!(features.inputs, ["hdmi1", "hdmi2", "net_radio", "airplay"]);
        assert!(features.has("volume") && features.has("mute") && features.has("power"));
        assert!(features.actual_volume_db);
    }

    #[test]
    fn scale_is_percentage_only_for_a_zero_to_hundred_range() {
        let range = |min, max| VolumeRange { min, max, step: 1 };
        assert_eq!(range(0, 100).scale(), VolumeScale::Percentage);
        // The common YXC range is not a percentage and is reported in native units.
        assert_eq!(range(0, 161).scale(), VolumeScale::Steps);
        assert_eq!(range(0, 60).scale(), VolumeScale::Steps);
        assert_eq!(range(-80, 100).scale(), VolumeScale::Steps);
    }

    #[test]
    fn status_carries_native_level_limit_mute_input_and_actual_db() {
        let status = parse_status(STATUS).unwrap();
        assert_eq!(status.power, Some(DevicePower::On));
        assert_eq!(status.volume, Some(80));
        assert_eq!(status.max_volume, Some(161));
        assert_eq!(status.mute, Some(false));
        assert_eq!(status.input.as_deref(), Some("hdmi1"));
        assert_eq!(status.actual_volume.unwrap().display(), "-40.5 dB");
        let standby = parse_status(r#"{"response_code":0,"power":"standby"}"#).unwrap();
        assert_eq!(standby.power, Some(DevicePower::Standby));
        assert_eq!(standby.volume, None);
    }

    #[test]
    fn unknown_power_word_is_not_rounded_to_standby() {
        let status = parse_status(r#"{"response_code":0,"power":"toggle"}"#).unwrap();
        assert_eq!(status.power, None);
    }

    #[test]
    fn input_names_come_from_name_text_with_id_fallback() {
        let names = parse_input_names(
            r#"{"response_code":0,"zone_list":[{"id":"main","text":"Living Room"}],
                "input_list":[{"id":"hdmi1","text":"TV"},{"id":"hdmi2","text":""}]}"#,
        )
        .unwrap();
        let features = parse_features(FEATURES).unwrap();
        let inputs = build_inputs(&features, &names);
        assert_eq!(inputs[0].id, "hdmi1");
        assert_eq!(inputs[0].name, "TV");
        assert_eq!(
            inputs[1].name, "hdmi2",
            "an empty rename falls back to the id"
        );
        assert_eq!(inputs[3].name, "airplay");
        assert!(inputs.iter().all(|i| i.available));
    }

    #[test]
    fn description_filter_needs_yamaha_and_a_yxc_control_url() {
        // Shaped from a MusicCast UPnP description (aiomusiccast `check_yamaha_ssdp`).
        let musiccast = r#"<root xmlns:yamaha="urn:schemas-yamaha-com:device-1-0"><device>
            <manufacturer>Yamaha Corporation</manufacturer><modelName>RX-V481</modelName>
            <friendlyName>Living Room</friendlyName>
            <yamaha:X_device><yamaha:X_URLBase>http://192.168.1.20:80/</yamaha:X_URLBase>
            <yamaha:X_yxcControlURL>/YamahaExtendedControl/v1/</yamaha:X_yxcControlURL>
            </yamaha:X_device></device></root>"#;
        let description = parse_description(musiccast).unwrap();
        assert!(description.is_candidate());
        assert_eq!(description.model, "RX-V481");
        assert_eq!(description.friendly_name, "Living Room");
        assert_eq!(description.control_port(), 80);
        let moved = musiccast.replace("192.168.1.20:80", "192.168.1.20:8080");
        assert_eq!(parse_description(&moved).unwrap().control_port(), 8080);

        let legacy = musiccast.replace(
            "<yamaha:X_yxcControlURL>/YamahaExtendedControl/v1/</yamaha:X_yxcControlURL>",
            "",
        );
        assert!(!parse_description(&legacy).unwrap().is_candidate());
        let other = musiccast.replace("Yamaha Corporation", "Denon");
        assert!(!parse_description(&other).unwrap().is_candidate());
        assert!(parse_description("<root/>").is_none());
    }

    #[test]
    fn volume_encodes_at_min_max_and_step() {
        let range = VolumeRange {
            min: 0,
            max: 161,
            step: 1,
        };
        assert_eq!(encode_volume(0.0, &range, None), Ok(0));
        assert_eq!(encode_volume(161.0, &range, None), Ok(161));
        assert_eq!(encode_volume(80.0, &range, None), Ok(80));

        let stepped = VolumeRange {
            min: 0,
            max: 60,
            step: 2,
        };
        assert_eq!(encode_volume(30.0, &stepped, None), Ok(30));
        assert_eq!(encode_volume(31.0, &stepped, None), Ok(32));
    }

    #[test]
    fn volume_outside_range_or_above_the_unit_limit_is_refused_not_clamped() {
        let range = VolumeRange {
            min: 0,
            max: 161,
            step: 1,
        };
        assert!(encode_volume(162.0, &range, None).is_err());
        assert!(encode_volume(-1.0, &range, None).is_err());
        assert!(encode_volume(f32::NAN, &range, None).is_err());
        assert!(encode_volume(f32::INFINITY, &range, None).is_err());
        // The unit's own max_volume (status) is below the range maximum.
        assert_eq!(encode_volume(100.0, &range, Some(100)), Ok(100));
        assert!(encode_volume(101.0, &range, Some(100)).is_err());
    }

    #[test]
    fn operations_encode_to_the_documented_requests() {
        let range = VolumeRange {
            min: 0,
            max: 161,
            step: 1,
        };
        let inputs = build_inputs(&parse_features(FEATURES).unwrap(), &HashMap::new());
        let encode = |op| encode_operation(&op, Some(&range), None, &inputs);
        assert_eq!(
            encode(DeviceOperation::SetVolume(80.0)).unwrap(),
            "/YamahaExtendedControl/v1/main/setVolume?volume=80"
        );
        assert_eq!(
            encode(DeviceOperation::SetMute(true)).unwrap(),
            "/YamahaExtendedControl/v1/main/setMute?enable=true"
        );
        assert_eq!(
            encode(DeviceOperation::SetMute(false)).unwrap(),
            "/YamahaExtendedControl/v1/main/setMute?enable=false"
        );
        assert_eq!(
            encode(DeviceOperation::PowerOn).unwrap(),
            "/YamahaExtendedControl/v1/main/setPower?power=on"
        );
        assert_eq!(
            encode(DeviceOperation::PowerStandby).unwrap(),
            "/YamahaExtendedControl/v1/main/setPower?power=standby"
        );
        assert_eq!(
            encode(DeviceOperation::SelectInput("net_radio".into())).unwrap(),
            "/YamahaExtendedControl/v1/main/setInput?input=net_radio"
        );
        // An input the device never reported is refused, and injection is not possible.
        assert!(encode(DeviceOperation::SelectInput("hdmi9".into())).is_err());
        assert!(encode(DeviceOperation::SelectInput("hdmi1&volume=161".into())).is_err());
        // Without a volume range there is no volume write.
        assert!(encode_operation(&DeviceOperation::SetVolume(1.0), None, None, &inputs).is_err());
    }

    #[test]
    fn capabilities_follow_getfeatures_and_are_implementation_derived() {
        let full = capabilities_for("RX-V481", &parse_features(FEATURES).unwrap());
        assert!(full.volume.supported && full.mute.supported && full.power.supported);
        assert!(full.input_select.supported);
        assert!(full.volume.detail.contains("native units 0..161"));
        for claim in [
            &full.observe,
            &full.volume,
            &full.mute,
            &full.power,
            &full.input_select,
        ] {
            assert_eq!(claim.evidence, CapabilityEvidence::ImplementationDerived);
        }

        let bare = capabilities_for("WXA-50", &Features::default());
        assert!(bare.observe.supported);
        assert!(!bare.volume.supported && !bare.mute.supported);
        assert!(!bare.power.supported && !bare.input_select.supported);
    }
}
