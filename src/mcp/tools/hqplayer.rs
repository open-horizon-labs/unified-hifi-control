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

use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Provider, Refusal, Scope};
use crate::mcp::types::{McpHqpStatus, McpPipelineStatus};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

/// Get HQPlayer status
#[mcp_tool(
    name = "hifi_hqplayer_status",
    description = "Get HQPlayer Embedded status and current pipeline settings",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerStatusTool {}

/// List HQPlayer profiles
#[mcp_tool(
    name = "hifi_hqplayer_profiles",
    description = "List available HQPlayer Embedded configurations",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerProfilesTool {}

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
}

/// Change an HQPlayer pipeline setting
#[mcp_tool(
    name = "hifi_hqplayer_set_pipeline",
    description = "Change an HQPlayer pipeline setting (mode, samplerate, filter1x, filterNx, shaper, dither)"
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiHqplayerSetPipelineTool {
    /// Setting to change: mode, samplerate, filter1x, filterNx, shaper, dither
    pub setting: String,
    /// New value for the setting
    pub value: String,
}

/// The setting names this tool advertises, used in the refusal message.
///
/// Kept next to the alias match below so the two stay in step.
const VALID_SETTINGS: &str = "mode, samplerate, filter1x, filterNx, shaper, dither";

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
        _ => None,
    }
}

pub async fn handle_status(state: &AppState) -> Result<CallToolResult, CallToolError> {
    let status = state.hqplayer.get_status().await;
    let pipeline = state.hqplayer.get_pipeline_status().await.ok();

    let mcp_status = McpHqpStatus {
        connected: status.connected,
        host: status.host,
        pipeline: pipeline.map(|p| McpPipelineStatus {
            state: p.status.state,
            filter: p.status.active_filter,
            shaper: p.status.active_shaper,
            rate: p.status.active_rate,
        }),
    };
    Ok(Envelope::read("hifi_hqplayer_status", "get_status")
        .scope(Scope::provider_only(Provider::HqPlayer))
        .json_result(&mcp_status))
}

pub async fn handle_profiles(state: &AppState) -> Result<CallToolResult, CallToolError> {
    let profiles = state.hqplayer.get_cached_profiles().await;
    let profile_names: Vec<String> = profiles.into_iter().map(|p| p.title).collect();
    Ok(Envelope::read("hifi_hqplayer_profiles", "list_profiles")
        .scope(Scope::provider_only(Provider::HqPlayer))
        .json_result(&profile_names))
}

pub async fn handle_load_profile(
    state: &AppState,
    args: HifiHqplayerLoadProfileTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::write("hifi_hqplayer_load_profile", "load_profile")
        .param("profile", &*args.profile)
        .scope(Scope::provider_only(Provider::HqPlayer));

    match state.hqplayer.load_profile(&args.profile).await {
        // No `observed`: HQPlayer state is not in the aggregator, and #397's
        // constraints say it must not be moved there. Reading it back would mean
        // a second `get_pipeline_status()` round-trip against a server that is
        // restarting — a claim this code cannot support. `accepted` is the honest
        // ceiling; the client calls hifi_hqplayer_status when it wants the state.
        Ok(()) => Ok(env.text_result(format!("Loaded profile: {}", args.profile))),
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
        .scope(Scope::provider_only(Provider::HqPlayer));

    // All settings use name-based lookups; the adapter handles the conversion.
    // Only samplerate needs numeric parsing (an Hz value).
    let result = match args.setting.as_str() {
        // Accepts a name like "PCM", "DSD", "[source]".
        "mode" => state.hqplayer.set_mode(&args.value).await,
        "filter1x" | "filter_1x" => state.hqplayer.set_filter_1x(&args.value).await,
        "filterNx" | "filter_nx" | "filternx" => state.hqplayer.set_filter_nx(&args.value).await,
        "shaper" | "dither" => state.hqplayer.set_shaper(&args.value).await,
        "rate" | "samplerate" => {
            // Samplerate uses an Hz value, e.g. "48000", "96000".
            if let Ok(v) = args.value.parse::<u32>() {
                state.hqplayer.set_rate(v).await
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
        // No `observed` — same reason as load_profile.
        Ok(()) => Ok(env.text_result(format!("Set {} to {}", args.setting, args.value))),
        Err(e) => env.failed(format!("Failed to set {}: {}", args.setting, e)),
    }
}
