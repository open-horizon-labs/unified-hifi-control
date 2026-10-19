//! `hifi_devices`: playback devices (amplifiers, receivers) that zones feed (#783, #786). **Alpha.**
//!
//! A device is not a zone. A zone's music provider owns transport; the device a zone is *linked* to
//! owns power, volume, mute and the physical input. This tool lists devices, manages links, and
//! controls devices. `hifi_control` volume actions on a linked zone also reach the device (see
//! [`linked_zone_volume`]), so an agent that tries the obvious `hifi_control volume_up` on a room
//! moves the amplifier, not a provider gain stage the link has taken out of play.
//!
//! Power and input live here rather than in `hifi_control`: they are device operations, the
//! epic scoped its MCP surface to this tool, and `hifi_control`'s closed action set stays the
//! transport vocabulary every provider shares.

use crate::api::devices::device_inventory;
use crate::api::AppState;
use crate::devices::service::{DeviceError, DeviceTarget, DEVICE_ACTIONS};
use crate::devices::{DeviceId, DEVICE_CONTROL_MATURITY};
use crate::mcp::envelope::{Envelope, Refusal};
use crate::mcp::tools::transport::HifiControlTool;
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

const ACTIONS: &[&str] = &[
    "list",
    "links",
    "link",
    "unlink",
    "set_ceiling",
    "control",
    "report",
    "set_recording",
    "clear_report",
];

#[mcp_tool(
    name = "hifi_devices",
    description = "Alpha capability: expect refinement across releases. Amplifiers and receivers \
                    (playback devices) that zones play through, separate from the music provider. \
                    action=list shows every device with its inputs, power, volume range and linked \
                    zones. action=links lists zone links. action=link connects a zone_id to the \
                    device_id it plays through (optionally source_id, the device input the zone \
                    arrives on); after linking, that zone's volume and hifi_control volume actions \
                    drive the device. action=unlink removes a zone's link. action=set_ceiling sets \
                    a device's maximum volume in dB (omit ceiling to clear); a ceiling only ever \
                    lowers the device's own limit. action=control sends command (power_on, \
                    power_off, power_toggle, select_input, volume_set, volume_up, volume_down, \
                    mute, unmute) to a device_id, or to the device a zone_id is linked to. Volume \
                    values are dB. Inputs never switch automatically: use select_input. A command \
                    succeeds only when the device confirms it. Spec \
                    mismatches: action=set_recording enabled=true opts in to recording device \
                    protocol mismatches locally (off by default, memory only); action=report \
                    device_id builds a redacted preview with a prefilled GitHub issue link the \
                    user opens and submits themselves (sections: environment, anomalies, \
                    device_state, excerpts); action=clear_report forgets a device's recordings. \
                    Nothing is ever sent automatically."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiDevicesTool {
    /// list, links, link, unlink, set_ceiling, or control.
    pub action: String,
    /// The device, e.g. marantz:0006781E0AB8 (from action=list).
    #[serde(default)]
    pub device_id: Option<String>,
    /// A zone id (from hifi_zones). For link/unlink, the zone to (un)link; for control, the
    /// linked zone whose device to control.
    #[serde(default)]
    pub zone_id: Option<String>,
    /// For link: the device input this zone arrives on (an input id from action=list).
    #[serde(default)]
    pub source_id: Option<String>,
    /// For control: power_on, power_off, power_toggle, select_input, volume_set, volume_up,
    /// volume_down, mute, or unmute.
    #[serde(default)]
    pub command: Option<String>,
    /// For control: dB level for volume_set, or dB step for volume_up/volume_down (defaults to
    /// the device step).
    #[serde(default)]
    pub value: Option<f64>,
    /// For control select_input: the input id or its display name.
    #[serde(default)]
    pub input: Option<String>,
    /// For control mute: true or false. Omit to toggle.
    #[serde(default)]
    pub muted: Option<bool>,
    /// For set_ceiling: the maximum volume in dB. Omit to clear the ceiling.
    #[serde(default)]
    pub ceiling: Option<f64>,
    /// For set_recording: true to record device protocol mismatches locally, false to stop
    /// (and forget everything recorded).
    #[serde(default)]
    pub enabled: Option<bool>,
    /// For report: section ids to include (environment, anomalies, device_state, excerpts).
    /// Omit for all but raw protocol excerpts.
    #[serde(default)]
    pub sections: Option<Vec<String>>,
}

pub async fn handle_devices(
    state: &AppState,
    args: HifiDevicesTool,
) -> Result<CallToolResult, CallToolError> {
    let write = !matches!(args.action.as_str(), "list" | "links");
    let env = if write {
        Envelope::write("hifi_devices", args.action.clone())
    } else {
        Envelope::read("hifi_devices", args.action.clone())
    };
    match args.action.as_str() {
        "list" => Ok(env.json_result(&device_inventory(state).await)),
        "links" => {
            let inventory = device_inventory(state).await;
            Ok(env.json_result(&json!({
                "maturity": DEVICE_CONTROL_MATURITY,
                "links": inventory["links"],
                "link_store": inventory["link_store"],
            })))
        }
        "link" => {
            let (Some(zone_id), Some(device_id)) = (args.zone_id, args.device_id) else {
                return env.refused(
                    "link requires zone_id and device_id.",
                    Refusal::invalid_parameter(
                        "zone_id",
                        &[],
                        "Pass the zone (from hifi_zones) and the device it plays through (from \
                         hifi_devices action=list).",
                    ),
                );
            };
            let env = env
                .param("zone_id", zone_id.clone())
                .param("device_id", device_id.clone());
            match state
                .devices
                .link(&zone_id, &device_id, args.source_id)
                .await
            {
                Ok(link) => Ok(env.json_result(&json!({
                    "maturity": DEVICE_CONTROL_MATURITY,
                    "link": link,
                    "linked_device": state.aggregator.linked_device(&zone_id).await,
                }))),
                Err(error) => refuse(env, error),
            }
        }
        "unlink" => {
            let Some(zone_id) = args.zone_id else {
                return env.refused(
                    "unlink requires zone_id.",
                    Refusal::invalid_parameter("zone_id", &[], "Pass the linked zone."),
                );
            };
            let env = env.param("zone_id", zone_id.clone());
            match state.devices.unlink(&zone_id).await {
                Ok(removed) => Ok(env.json_result(&json!({
                    "maturity": DEVICE_CONTROL_MATURITY,
                    "removed": removed,
                }))),
                Err(error) => refuse(env, error),
            }
        }
        "set_ceiling" => {
            let Some(device_id) = args.device_id else {
                return env.refused(
                    "set_ceiling requires device_id.",
                    Refusal::invalid_parameter("device_id", &[], "Pass a device from action=list."),
                );
            };
            let ceiling = args.ceiling.map(|db| db as f32);
            let env = env
                .param("device_id", device_id.clone())
                .param_opt("ceiling", args.ceiling);
            match state.devices.set_ceiling(&device_id, ceiling).await {
                Ok(()) => Ok(env.json_result(&json!({
                    "maturity": DEVICE_CONTROL_MATURITY,
                    "device_id": device_id,
                    "ceiling": args.ceiling,
                }))),
                Err(error) => refuse(env, error),
            }
        }
        "control" => {
            let Some(command) = args.command.clone() else {
                return env.refused(
                    "control requires command.",
                    Refusal::invalid_parameter(
                        "command",
                        DEVICE_ACTIONS,
                        "Choose a device command.",
                    ),
                );
            };
            let target = match (&args.device_id, &args.zone_id) {
                (Some(device_id), None) => match DeviceId::parse(device_id) {
                    Some(id) => DeviceTarget::Device(id),
                    None => {
                        return refuse(
                            env,
                            DeviceError::UnknownDevice {
                                device_id: device_id.clone(),
                            },
                        )
                    }
                },
                (None, Some(zone_id)) => DeviceTarget::Zone(zone_id.clone()),
                _ => {
                    return env.refused(
                        "control requires exactly one of device_id or zone_id.",
                        Refusal::invalid_parameter(
                            "device_id",
                            &[],
                            "Address the device directly, or through a zone linked to it.",
                        ),
                    )
                }
            };
            let value = match command.as_str() {
                "select_input" => args.input.clone().map(serde_json::Value::String),
                "mute" => args.muted.map(serde_json::Value::Bool),
                _ => args.value.map(|v| json!(v)),
            };
            let env = env
                .param("command", command.clone())
                .param_opt("device_id", args.device_id.clone())
                .param_opt("zone_id", args.zone_id.clone())
                .param_opt("value", value.clone());
            control(state, env, target, &command, value.as_ref()).await
        }
        "report" => {
            let Some(device_id) = args.device_id else {
                return env.refused(
                    "report requires device_id.",
                    Refusal::invalid_parameter("device_id", &[], "Pass a device from action=list."),
                );
            };
            let env = env.param("device_id", device_id.clone());
            match crate::api::devices::mismatch_preview(state, &device_id, args.sections).await {
                Ok(preview) => Ok(env.json_result(&preview)),
                Err(error) => refuse(env, error),
            }
        }
        "set_recording" => {
            let Some(enabled) = args.enabled else {
                return env.refused(
                    "set_recording requires enabled.",
                    Refusal::invalid_parameter("enabled", &["true", "false"], "Opt in or out."),
                );
            };
            let env = env.param("enabled", enabled);
            match state.devices.set_recording(enabled).await {
                Ok(()) => Ok(env.json_result(&json!({
                    "maturity": DEVICE_CONTROL_MATURITY,
                    "recording": enabled,
                }))),
                Err(error) => refuse(env, error),
            }
        }
        "clear_report" => {
            let Some(device_id) = args.device_id.as_deref().and_then(DeviceId::parse) else {
                return env.refused(
                    "clear_report requires a valid device_id.",
                    Refusal::invalid_parameter("device_id", &[], "Pass a device from action=list."),
                );
            };
            state.devices.journal().clear(&device_id);
            Ok(env
                .param("device_id", device_id.to_string())
                .json_result(&json!({"maturity": DEVICE_CONTROL_MATURITY, "cleared": true})))
        }
        _ => env.refused(
            "Unknown hifi_devices action.",
            Refusal::invalid_parameter("action", ACTIONS, "Choose a documented action."),
        ),
    }
}

async fn control(
    state: &AppState,
    env: Envelope,
    target: DeviceTarget,
    command: &str,
    value: Option<&serde_json::Value>,
) -> Result<CallToolResult, CallToolError> {
    match state.devices.control_action(target, command, value).await {
        Ok(Some(report)) => Ok(env.json_result(&report)),
        Ok(None) => env.refused(
            format!("{command} is not a device command."),
            Refusal::invalid_parameter("command", DEVICE_ACTIONS,
                "Transport (play, pause, next...) belongs to the zone's provider: use hifi_control."),
        ),
        Err(error) => refuse(env, error),
    }
}

/// Map a device error to the envelope's refusal vocabulary.
fn refuse(env: Envelope, error: DeviceError) -> Result<CallToolResult, CallToolError> {
    let text = error.to_string();
    let refusal = match &error {
        DeviceError::NotLinked { .. } => Refusal::invalid_parameter(
            "zone_id",
            &[],
            format!("{text}. Link it with hifi_devices action=link, or use hifi_control for the provider."),
        ),
        DeviceError::UnknownDevice { .. } => Refusal::UnknownTarget {
            parameter: "device_id",
            discover_with: "hifi_devices",
            detail: text.clone(),
        },
        DeviceError::UnknownZone { .. } => Refusal::UnknownTarget {
            parameter: "zone_id",
            discover_with: "hifi_zones",
            detail: text.clone(),
        },
        DeviceError::UnknownInput { accepted, .. } => {
            let accepted: Vec<&str> = accepted.iter().map(String::as_str).collect();
            Refusal::invalid_parameter("input", &accepted, text.clone())
        }
        DeviceError::InvalidValue { parameter, .. } => {
            Refusal::invalid_parameter(parameter, &[], text.clone())
        }
        DeviceError::Expired { .. }
        | DeviceError::RangeUnknown { .. }
        | DeviceError::DeviceUnavailable { .. }
        | DeviceError::Store(_)
        | DeviceError::NoDriver { .. }
        | DeviceError::Dispatch { .. }
        | DeviceError::Unconfirmed { .. } => Refusal::backend_error(format!(
            "{text} [{}]",
            error.code()
        )),
    };
    env.refused(text, refusal)
}

/// `hifi_control` volume actions on a zone linked to a device go to the device.
///
/// Returns `None` when the zone has no link (the provider path applies) or the action is not a
/// volume action. With no `value`, `volume_up`/`volume_down` step by the device's own step (0.5 dB
/// on a Denon/Marantz) rather than `hifi_control`'s provider default of 5, which on an amplifier
/// is a loud surprise.
pub async fn linked_zone_volume(
    state: &AppState,
    args: &HifiControlTool,
) -> Option<Result<CallToolResult, CallToolError>> {
    if !matches!(
        args.action.as_str(),
        "volume_set" | "volume_up" | "volume_down"
    ) {
        return None;
    }
    let value = args.value.map(|v| json!(v));
    let result = crate::zone_control::dispatch_linked_device(
        &state.devices,
        &args.zone_id,
        &args.action,
        value.as_ref(),
    )
    .await?;
    let operation = match args.action.as_str() {
        "volume_set" => "volume_absolute",
        _ => "volume_relative",
    };
    let env = Envelope::write("hifi_control", operation)
        .param("zone_id", args.zone_id.clone())
        .param_opt("value", args.value);
    Some(match result {
        Ok(report) => Ok(env.json_result(&report)),
        Err(error) => refuse(env, error),
    })
}
