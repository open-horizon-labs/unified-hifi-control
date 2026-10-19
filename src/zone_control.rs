//! Shared zone command routing. The aggregator composes state; this module
//! resolves the corresponding capability owner before provider dispatch.
//! A link delegates volume/mute/power/input to DeviceService. Transport stays
//! with the music provider. There is no second device identity or state cache.

use std::sync::Arc;

use crate::adapters::{AdapterCommand, AdapterCommandResponse};
use crate::aggregator::ZoneAggregator;
use crate::api::{
    dispatch_lms_runtime_command_via, dispatch_openhome_runtime_command_via,
    dispatch_roon_runtime_command_via, dispatch_upnp_runtime_command_via, AdapterRegistry,
};
use crate::bus::runtime::CommandGateway;
use crate::bus::Command;
use crate::knobs::routes::dispatch_hqplayer_action_via;

use crate::devices::service::{
    is_device_only_action, DeviceCommandReport, DeviceError, DeviceService, DeviceTarget,
};

/// A provider-neutral zone action; volume units remain explicit.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    Play,
    Pause,
    Next,
    Previous,
    VolumeUp,
    VolumeDown,
    /// Percent volume, already clamped to 0-100.
    Volume(f64),
    /// Zone-native absolute volume, including a linked amplifier's signed dB.
    VolumeNative(f64),
    Mute(bool),
}

impl Action {
    fn into_adapter_command(self) -> AdapterCommand {
        match self {
            Action::Play => AdapterCommand::Play,
            Action::Pause => AdapterCommand::Pause,
            Action::Next => AdapterCommand::Next,
            Action::Previous => AdapterCommand::Previous,
            Action::VolumeUp => AdapterCommand::VolumeRelative(1),
            Action::VolumeDown => AdapterCommand::VolumeRelative(-1),
            // The adapter registry's volume scale is each provider's native
            // 0-100 percentage convention (see `AdapterCommand::VolumeAbsolute`
            // call sites in `src/knobs/routes.rs`).
            Action::Volume(value) => AdapterCommand::VolumeAbsolute(value.round() as i32),
            Action::VolumeNative(value) => AdapterCommand::VolumeAbsolute(value.round() as i32),
            Action::Mute(muted) => AdapterCommand::Mute(muted),
        }
    }

    /// Native transport/volume `Command` for the reliable command gateway's
    /// percentage-scale legacy providers (Roon, LMS, OpenHome, UPnP). Not
    /// used for HQPlayer, whose decibel scale needs its own zone-aware
    /// translation - see [`Action::into_hqplayer_action`].
    fn into_gateway_command(self) -> Command {
        match self {
            Action::Play => Command::Play,
            Action::Pause => Command::Pause,
            Action::Next => Command::Next,
            Action::Previous => Command::Previous,
            Action::VolumeUp => Command::VolumeRelative {
                delta: 1.0,
                output_id: None,
            },
            Action::VolumeDown => Command::VolumeRelative {
                delta: -1.0,
                output_id: None,
            },
            Action::Volume(value) => Command::VolumeAbsolute {
                value: value as f32,
                output_id: None,
            },
            Action::VolumeNative(value) => Command::VolumeAbsolute {
                value: value as f32,
                output_id: None,
            },
            Action::Mute(muted) => Command::Mute {
                muted,
                output_id: None,
            },
        }
    }

    /// The action name and optional numeric value `dispatch_hqplayer_action_via` expects, or
    /// `None` for a verb HQPlayer's decibel-scale volume cannot honor from a 0-100 MQTT payload.
    fn into_hqplayer_action(self) -> Option<(&'static str, Option<f64>)> {
        match self {
            Action::Play => Some(("play", None)),
            Action::Pause => Some(("pause", None)),
            Action::Next => Some(("next", None)),
            Action::Previous => Some(("previous", None)),
            Action::VolumeUp | Action::VolumeDown => None,
            Action::Volume(_) | Action::Mute(_) => None,
            Action::VolumeNative(value) => Some(("volume", Some(value))),
        }
    }
}

/// Outcome of routing one command, for logging/testing.
#[derive(Debug, PartialEq)]
pub enum DispatchOutcome {
    Sent,
    /// Native dispatch was attempted but its outcome could not be confirmed.
    Unknown(String),
    /// The owning provider (adapter or gateway endpoint) rejected the command.
    Refused(String),
    /// The command was not attempted: the zone's prefix has no bridged provider, or the verb is
    /// not supported for that provider (e.g. HQPlayer volume/mute over MQTT's percentage scale).
    Unsupported(String),
}

/// Route one parsed action to the adapter or gateway endpoint that owns `zone_id`.
pub async fn dispatch(
    adapter_registry: &Arc<AdapterRegistry>,
    aggregator: &ZoneAggregator,
    devices: &DeviceService,
    reliable_commands: Option<&CommandGateway>,
    zone_id: &str,
    action: Action,
) -> DispatchOutcome {
    if matches!(&action, Action::Volume(value) | Action::VolumeNative(value) if !value.is_finite())
    {
        return DispatchOutcome::Refused("volume must be finite".into());
    }
    if let Some(result) = dispatch_linked_action(devices, zone_id, &action).await {
        return match result {
            Ok(_) => DispatchOutcome::Sent,
            Err(error @ (DeviceError::Unconfirmed { .. } | DeviceError::Dispatch { .. })) => {
                DispatchOutcome::Unknown(error.to_string())
            }
            Err(error) => DispatchOutcome::Refused(error.to_string()),
        };
    }
    let Some(prefix) = zone_id.split(':').next().filter(|p| !p.is_empty()) else {
        return DispatchOutcome::Unsupported("zone id has no provider prefix".to_string());
    };

    if adapter_registry.has_adapter(prefix).await {
        let command = action.into_adapter_command();
        return match adapter_registry.command(prefix, zone_id, command).await {
            Ok(AdapterCommandResponse { success: true, .. }) => DispatchOutcome::Sent,
            Ok(AdapterCommandResponse { error, .. }) => {
                DispatchOutcome::Refused(error.unwrap_or_else(|| "command refused".to_string()))
            }
            Err(error) => DispatchOutcome::Refused(error.to_string()),
        };
    }

    match prefix {
        "roon" => gateway_outcome(
            dispatch_roon_runtime_command_via(
                reliable_commands,
                zone_id,
                action.into_gateway_command(),
            )
            .await,
        ),
        "lms" => gateway_outcome(
            dispatch_lms_runtime_command_via(
                reliable_commands,
                zone_id,
                action.into_gateway_command(),
            )
            .await,
        ),
        "openhome" => gateway_outcome(
            dispatch_openhome_runtime_command_via(
                reliable_commands,
                zone_id,
                action.into_gateway_command(),
            )
            .await,
        ),
        "upnp" => gateway_outcome(
            dispatch_upnp_runtime_command_via(
                reliable_commands,
                zone_id,
                action.into_gateway_command(),
            )
            .await,
        ),
        "hqplayer" => {
            let Some((hqp_action, value)) = action.into_hqplayer_action() else {
                return DispatchOutcome::Unsupported(
                    "HQPlayer volume/mute is not available over MQTT's 0-100 percentage scale"
                        .to_string(),
                );
            };
            let instance = zone_id.strip_prefix("hqplayer:").unwrap_or(zone_id);
            match dispatch_hqplayer_action_via(
                aggregator,
                reliable_commands,
                zone_id,
                instance,
                hqp_action,
                value,
            )
            .await
            {
                Ok(()) => DispatchOutcome::Sent,
                Err(error) => DispatchOutcome::Refused(error.message().to_string()),
            }
        }
        _ => DispatchOutcome::Unsupported(format!("no bridged provider for '{prefix}' zones")),
    }
}

/// Fold one of the reliable-gateway dispatch functions' `anyhow::Result<()>` into the shared
/// outcome type this module's callers log against.
fn gateway_outcome(result: anyhow::Result<()>) -> DispatchOutcome {
    match result {
        Ok(()) => DispatchOutcome::Sent,
        Err(error) => DispatchOutcome::Refused(error.to_string()),
    }
}

/// Resolve the device-owned part of a zone action. `None` means provider
/// routing applies; an error on a linked zone must never fall back to it.
/// Surface adapters translate their vocabulary but do not resolve links.
pub async fn dispatch_linked_device(
    devices: &DeviceService,
    zone_id: &str,
    action: &str,
    value: Option<&serde_json::Value>,
) -> Option<Result<DeviceCommandReport, DeviceError>> {
    let binding = match devices.zone_binding(zone_id).await {
        Some(binding) => binding,
        None if is_device_only_action(action) => {
            return Some(Err(DeviceError::NotLinked {
                zone_id: zone_id.to_owned(),
            }))
        }
        None => return None,
    };
    // A native action is pinned to the owner chosen for this dispatch. Device
    // semantics and defaults are interpreted by DeviceService, not this router.
    match devices
        .control_action(DeviceTarget::Device(binding.link.device_id), action, value)
        .await
    {
        Ok(Some(report)) => Some(Ok(report)),
        Ok(None) => None,
        Err(error) => Some(Err(error)),
    }
}

async fn dispatch_linked_action(
    devices: &DeviceService,
    zone_id: &str,
    action: &Action,
) -> Option<Result<DeviceCommandReport, DeviceError>> {
    use serde_json::json;
    let (name, value) = match action {
        Action::VolumeNative(value) => ("volume", Some(json!(value))),
        Action::VolumeUp => ("volume_up", None),
        Action::VolumeDown => ("volume_down", None),
        Action::Mute(muted) => ("mute", Some(json!(muted))),
        Action::Volume(value) => {
            let binding = devices.zone_binding(zone_id).await?;
            return Some(
                devices
                    .control_percentage(DeviceTarget::Device(binding.link.device_id), *value)
                    .await,
            );
        }
        _ => return None,
    };
    dispatch_linked_device(devices, zone_id, name, value.as_ref()).await
}
