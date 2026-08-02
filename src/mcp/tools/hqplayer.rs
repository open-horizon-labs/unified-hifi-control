//! HQPlayer pipeline inspection and control.
//!
//! These four tools are advertised only when the HQPlayer adapter is enabled in
//! settings — see [`crate::mcp::tools::list_tools`].
//!
//! # The setting alias table is contract
//!
//! [`handle_set_pipeline`] accepts several spellings per setting
//! (`filter1x`/`filter_1x`, `filterNx`/`filter_nx`/`filternx`, `shaper`/`dither`,
//! `rate`/`samplerate`). None of that appears in `tools/list`, so dropping an
//! alias would turn a documented input into "Unknown setting" with a green
//! snapshot suite. `tests/mcp_contract.rs::hqplayer_set_pipeline_alias_table_is_pinned`
//! checks every alias individually.

use crate::adapters::hqplayer::HqpAdapter;
use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Provider, Refusal, Scope};
use crate::mcp::types::{McpHqpOptions, McpHqpSelection, McpHqpStatus, McpPipelineStatus};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

/// Get HQPlayer status
#[mcp_tool(
    name = "hifi_hqplayer_status",
    description = "Get HQPlayer Embedded status and current pipeline settings",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerStatusTool {
    /// Optional direct HQPlayer zone (`hqplayer:<instance>`). Omit for the default instance.
    pub zone_id: Option<String>,
}

/// List HQPlayer profiles
#[mcp_tool(
    name = "hifi_hqplayer_profiles",
    description = "List available HQPlayer Embedded configurations",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerProfilesTool {
    /// Optional direct HQPlayer zone (`hqplayer:<instance>`). Omit for the default instance.
    pub zone_id: Option<String>,
}

/// Load an HQPlayer profile
#[mcp_tool(
    name = "hifi_hqplayer_load_profile",
    description = "Load an HQPlayer Embedded configuration (will restart HQPlayer)",
    destructive_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerLoadProfileTool {
    /// Configuration name to load (get from hifi_hqplayer_profiles)
    pub profile: String,
    /// Optional direct HQPlayer zone (`hqplayer:<instance>`). Omit for the default instance.
    pub zone_id: Option<String>,
}

/// Change an HQPlayer pipeline setting
#[mcp_tool(
    name = "hifi_hqplayer_set_pipeline",
    description = "Change an immediate HQPlayer setting on an exact instance: mode, samplerate, filter1x, filterNx, shaper/dither, junk_filter, matrix_profile, convolution, adaptive_volume, repeat, or random. Call hifi_hqplayer_status first for current values and choices. This does not persist a profile."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerSetPipelineTool {
    /// Immediate setting to change; this does not persist an HQPlayer profile.
    pub setting: String,
    /// New value for the setting
    pub value: String,
    /// Optional direct HQPlayer zone (`hqplayer:<instance>`). Omit for the default instance.
    pub zone_id: Option<String>,
}

/// The setting names this tool advertises, used in the refusal message.
///
/// Kept next to the alias match below so the two stay in step.
const VALID_SETTINGS: &str = "mode, samplerate, filter1x, filterNx, shaper, dither, junk_filter, matrix_profile, convolution, adaptive_volume, repeat, random";

/// The same set, as the list an envelope refusal returns in `accepted`.
///
/// Derived from [`VALID_SETTINGS`] rather than restated, so the prose a human
/// reads and the array a model reads cannot drift apart.
fn valid_settings() -> Vec<String> {
    VALID_SETTINGS
        .split(", ")
        .map(str::to_string)
        .collect::<Vec<_>>()
}

/// The canonical spelling for a `setting` alias, or `None` if unrecognised.
///
/// This is the alias table from [`handle_set_pipeline`] rendered as data. Both
/// read the same match, so a dropped alias fails both at once.
fn canonical_setting(setting: &str) -> Option<&'static str> {
    match setting {
        "mode" => Some("mode"),
        "filter1x" | "filter_1x" => Some("filter1x"),
        "filterNx" | "filter_nx" | "filternx" => Some("filterNx"),
        "shaper" | "dither" => Some("shaper"),
        "rate" | "samplerate" => Some("samplerate"),
        "junk" | "junk_filter" => Some("junk_filter"),
        "matrix" | "matrix_profile" => Some("matrix_profile"),
        "convolution" => Some("convolution"),
        "adaptive" | "adaptive_volume" => Some("adaptive_volume"),
        "repeat" => Some("repeat"),
        "random" | "shuffle" => Some("random"),
        _ => None,
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "true" | "on" | "1" | "enabled" => Some(true),
        "false" | "off" | "0" | "disabled" => Some(false),
        _ => None,
    }
}

fn parse_repeat(value: &str) -> Option<u8> {
    match value.to_ascii_lowercase().as_str() {
        "0" | "off" | "none" => Some(0),
        "1" | "one" | "track" => Some(1),
        "2" | "all" => Some(2),
        _ => None,
    }
}

enum HqpTarget {
    Resolved {
        instance: String,
        adapter: Arc<HqpAdapter>,
        scope: Scope,
    },
    Invalid {
        scope: Scope,
        detail: String,
    },
    Missing {
        scope: Scope,
        instance: String,
    },
}

/// Resolve the same `hqplayer:<instance>` identity `hifi_control` uses. Keeping omission as the
/// compatibility default makes this additive for existing MCP clients while ensuring an explicit
/// target can never fall back to a different daemon.
async fn resolve_target(state: &AppState, zone_id: Option<&str>) -> HqpTarget {
    let Some(zone_id) = zone_id else {
        return HqpTarget::Resolved {
            instance: "default".to_string(),
            adapter: state.hqplayer.clone(),
            scope: Scope::provider_only(Provider::HqPlayer),
        };
    };

    let scope = Scope::for_zone(state, zone_id, Provider::HqPlayer).await;
    let Some(instance) = zone_id.strip_prefix("hqplayer:") else {
        return HqpTarget::Invalid {
            scope,
            detail: format!("zone_id {zone_id:?} must start with 'hqplayer:'"),
        };
    };
    if instance.is_empty() {
        return HqpTarget::Invalid {
            scope,
            detail: "zone_id 'hqplayer:' must name an instance".to_string(),
        };
    }
    match state.hqp_instances.get(instance).await {
        Some(adapter) => HqpTarget::Resolved {
            instance: instance.to_string(),
            adapter,
            scope,
        },
        None => HqpTarget::Missing {
            scope,
            instance: instance.to_string(),
        },
    }
}

fn target_failure(
    env: Envelope,
    target: HqpTarget,
) -> Result<(String, Arc<HqpAdapter>, Envelope), Result<CallToolResult, CallToolError>> {
    match target {
        HqpTarget::Resolved {
            instance,
            adapter,
            scope,
        } => Ok((instance, adapter, env.scope(scope))),
        HqpTarget::Invalid { scope, detail } => Err(env.scope(scope).refused(
            &detail,
            Refusal::invalid_parameter(
                "zone_id",
                &["hqplayer:<instance> from hifi_zones"],
                detail.clone(),
            ),
        )),
        HqpTarget::Missing { scope, instance } => Err(env
            .scope(scope)
            .failed(format!("HQPlayer instance '{instance}' is not configured"))),
    }
}

async fn hqp_status_payload_for_adapter(
    state: &AppState,
    instance: &str,
    adapter: &HqpAdapter,
) -> McpHqpStatus {
    let status = match state.aggregator.get_hqplayer_snapshot(instance).await {
        Some(snapshot) => {
            let mut connection = snapshot.observation.connection;
            connection.connected =
                snapshot.presence == crate::aggregator::HqpSnapshotPresence::Live;
            connection
        }
        None => adapter.get_status().await,
    };
    let (pipeline, options, options_unavailable_reason) =
        match crate::api::refresh_hqp_advanced_aggregate(state, instance).await {
            Ok(snapshot) => {
                let pipeline = snapshot.pipeline;
                let state = snapshot.state;
                let selection =
                    |setting: &crate::adapters::hqplayer::PipelineSetting| McpHqpSelection {
                        current: setting.selected.value.clone(),
                        choices: setting
                            .options
                            .iter()
                            .map(|option| option.value.clone())
                            .collect(),
                    };
                let options = McpHqpOptions {
                    mode: selection(&pipeline.settings.mode),
                    samplerate: selection(&pipeline.settings.samplerate),
                    filter1x: selection(&pipeline.settings.filter1x),
                    filter_nx: selection(&pipeline.settings.filter_nx),
                    shaper: selection(&pipeline.settings.shaper),
                    junk_filter: McpHqpSelection {
                        current: snapshot
                            .junk_filters
                            .iter()
                            .find(|item| item.index == state.filter_junk)
                            .map(|item| item.name.clone())
                            .unwrap_or_default(),
                        choices: snapshot
                            .junk_filters
                            .into_iter()
                            .map(|item| item.name)
                            .collect(),
                    },
                    matrix_profile: McpHqpSelection {
                        current: snapshot
                            .current_matrix_profile
                            .map(|profile| profile.name)
                            .unwrap_or_default(),
                        choices: snapshot
                            .matrix_profiles
                            .into_iter()
                            .map(|profile| profile.name)
                            .collect(),
                    },
                    convolution: state.convolution,
                    adaptive_volume: state.adaptive,
                    repeat: match state.repeat {
                        0 => "off",
                        1 => "one",
                        2 => "all",
                        _ => "unknown",
                    }
                    .to_string(),
                    random: state.random,
                };
                (Some(pipeline), Some(options), None)
            }
            Err(error) => (None, None, status.connected.then(|| error.to_string())),
        };

    McpHqpStatus {
        connected: status.connected,
        host: status.host,
        pipeline: pipeline.map(|p| McpPipelineStatus {
            state: p.status.state,
            filter: p.status.active_filter,
            shaper: p.status.active_shaper,
            rate: p.status.active_rate,
        }),
        options,
        options_unavailable_reason,
    }
}

/// Shared default-instance payload for the tool and `hifi://hqplayer/status` resource.
pub async fn hqp_status_payload(state: &AppState) -> McpHqpStatus {
    hqp_status_payload_for_adapter(state, "default", &state.hqplayer).await
}

pub async fn handle_status(
    state: &AppState,
    args: HifiHqplayerStatusTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::read("hifi_hqplayer_status", "get_status")
        .param_opt("zone_id", args.zone_id.as_deref());
    let target = resolve_target(state, args.zone_id.as_deref()).await;
    let (instance, adapter, env) = match target_failure(env, target) {
        Ok(resolved) => resolved,
        Err(result) => return result,
    };
    Ok(env.json_result(&hqp_status_payload_for_adapter(state, &instance, &adapter).await))
}

/// Fetch the default instance's profiles for the tool and MCP resource from one implementation.
pub async fn hqp_profiles_payload(state: &AppState) -> Result<Vec<String>, String> {
    crate::api::refresh_hqp_profiles_aggregate(state, "default")
        .await
        .map(|profiles| profiles.into_iter().map(|profile| profile.title).collect())
        .map_err(|error| error.to_string())
}

pub async fn handle_profiles(
    state: &AppState,
    args: HifiHqplayerProfilesTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::read("hifi_hqplayer_profiles", "list_profiles")
        .param_opt("zone_id", args.zone_id.as_deref());
    let target = resolve_target(state, args.zone_id.as_deref()).await;
    let (instance, _adapter, env) = match target_failure(env, target) {
        Ok(resolved) => resolved,
        Err(result) => return result,
    };
    let profile_names: Vec<String> =
        match crate::api::refresh_hqp_profiles_aggregate(state, &instance).await {
            Ok(profiles) => profiles.into_iter().map(|profile| profile.title).collect(),
            Err(error) => return env.failed(format!("Failed to list profiles: {error}")),
        };
    Ok(env.json_result(&profile_names))
}

pub async fn handle_load_profile(
    state: &AppState,
    args: HifiHqplayerLoadProfileTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::write("hifi_hqplayer_load_profile", "load_profile")
        .param("profile", &*args.profile)
        .param_opt("zone_id", args.zone_id.as_deref());
    let target = resolve_target(state, args.zone_id.as_deref()).await;
    let (instance, adapter, env) = match target_failure(env, target) {
        Ok(resolved) => resolved,
        Err(result) => return result,
    };

    match adapter.load_profile(&args.profile).await {
        // `load_profile` returns only after persistent XML readback and native/web recovery.
        // The envelope still carries no legacy-zone observation because pipeline configuration
        // is not represented by the ZoneAggregator's playback shape.
        Ok(()) => {
            if let Err(error) = state.hqp_instances.refresh_instance(&instance).await {
                return env.failed(format!(
                    "Loaded profile {}, but failed to publish recovered state: {error}",
                    args.profile
                ));
            }
            Ok(env.text_result(format!("Loaded profile: {}", args.profile)))
        }
        Err(e) => env.failed(format!("Failed to load profile: {}", e)),
    }
}

pub async fn handle_set_pipeline(
    state: &AppState,
    args: HifiHqplayerSetPipelineTool,
) -> Result<CallToolResult, CallToolError> {
    // `params.setting` reports the *canonical* spelling, so a client that sent
    // `filternx` learns the server understood `filterNx`. The alias table is
    // otherwise invisible: it appears in no input schema.
    let env = Envelope::write("hifi_hqplayer_set_pipeline", "set_pipeline")
        .param(
            "setting",
            canonical_setting(&args.setting).unwrap_or(&args.setting),
        )
        .param("value", &*args.value)
        .param_opt("zone_id", args.zone_id.as_deref());
    let target = resolve_target(state, args.zone_id.as_deref()).await;
    let (instance, adapter, env) = match target_failure(env, target) {
        Ok(resolved) => resolved,
        Err(result) => return result,
    };

    // All settings use name-based lookups; the adapter handles the conversion.
    // Only samplerate needs numeric parsing (an Hz value).
    let result = match args.setting.as_str() {
        // Accepts a name like "PCM", "DSD", "[source]".
        "mode" => adapter.set_mode(&args.value).await,
        "filter1x" | "filter_1x" => adapter.set_filter_1x(&args.value).await,
        "filterNx" | "filter_nx" | "filternx" => adapter.set_filter_nx(&args.value).await,
        "shaper" | "dither" => adapter.set_shaper(&args.value).await,
        "junk" | "junk_filter" => adapter.set_junk_filter(&args.value).await,
        "matrix" | "matrix_profile" => adapter.set_matrix_profile_named(&args.value).await,
        "convolution" => match parse_bool(&args.value) {
            Some(value) => adapter.set_convolution(value).await,
            None => {
                return env.refused(
                    "Invalid convolution value (expected true or false)",
                    Refusal::invalid_parameter(
                        "value",
                        &["true", "false"],
                        "convolution is an on/off immediate setting",
                    ),
                )
            }
        },
        "adaptive" | "adaptive_volume" => match parse_bool(&args.value) {
            Some(value) => adapter.set_adaptive_volume(value).await,
            None => {
                return env.refused(
                    "Invalid adaptive_volume value (expected true or false)",
                    Refusal::invalid_parameter(
                        "value",
                        &["true", "false"],
                        "adaptive_volume is an on/off immediate setting",
                    ),
                )
            }
        },
        "repeat" => match parse_repeat(&args.value) {
            Some(value) => adapter.set_repeat(value).await,
            None => {
                return env.refused(
                    "Invalid repeat value (expected off, one, or all)",
                    Refusal::invalid_parameter(
                        "value",
                        &["off", "one", "all"],
                        "repeat accepts off/0, one/1, or all/2",
                    ),
                )
            }
        },
        "random" | "shuffle" => match parse_bool(&args.value) {
            Some(value) => adapter.set_random(value).await,
            None => {
                return env.refused(
                    "Invalid random value (expected true or false)",
                    Refusal::invalid_parameter(
                        "value",
                        &["true", "false"],
                        "random is HQPlayer's immediate shuffle control",
                    ),
                )
            }
        },
        "rate" | "samplerate" => {
            // Samplerate uses an Hz value, e.g. "48000", "96000".
            if let Ok(v) = args.value.parse::<u32>() {
                adapter.set_rate(v).await
            } else {
                return env.refused(
                    "Invalid rate value (expected Hz like 48000, 96000)",
                    Refusal::InvalidParameter {
                        parameter: "value",
                        accepted: vec!["an integer sample rate in Hz, e.g. 48000".to_string()],
                        detail: format!(
                            "setting='samplerate' takes an Hz integer; {:?} does not parse as one.",
                            args.value
                        ),
                    },
                );
            }
        }
        _ => {
            return env.refused(
                format!(
                    "Unknown setting: {}. Valid: {}",
                    args.setting, VALID_SETTINGS
                ),
                Refusal::InvalidParameter {
                    parameter: "setting",
                    accepted: valid_settings(),
                    detail: format!(
                        "{:?} is not a pipeline setting. Aliases are also accepted: \
                         filter_1x, filter_nx, filternx, dither, rate.",
                        args.setting
                    ),
                },
            );
        }
    };

    // `SettingOutcome` covers `Ignored`/`Suppressed`/`Ambiguous` — a daemon that acknowledged the
    // write without moving the authoritative field, or refused it, or left it undeterminable.
    // `into_applied_result` collapses those to an `Err` naming the reason, so this cannot report
    // "Set X to Y" for a setting that did not actually change — the same collapse
    // `hqp_apply_named_setting`/`hqp_apply_legacy_setting` (`src/api/mod.rs`) already use.
    match result.and_then(crate::adapters::hqplayer::SettingOutcome::into_applied_result) {
        Ok(()) => {
            if let Err(error) = state.hqp_instances.refresh_instance(&instance).await {
                return env.failed(format!(
                    "Set {} to {}, but failed to publish verified state: {error}",
                    args.setting, args.value
                ));
            }
            if let Err(error) = crate::api::refresh_hqp_advanced_aggregate(state, &instance).await {
                return env.failed(format!(
                    "Set {} to {}, but failed to publish advanced state: {error}",
                    args.setting, args.value
                ));
            }
            Ok(env.text_result(format!("Set {} to {}", args.setting, args.value)))
        }
        Err(e) => env.failed(format!("Failed to set {}: {}", args.setting, e)),
    }
}
