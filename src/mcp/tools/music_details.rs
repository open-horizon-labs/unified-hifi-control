//! Explicit, optional Cloud context reads; playback and resources never call this tool.
use crate::{
    api::AppState,
    mcp::envelope::{Envelope, Refusal, Scope},
    music_context::{shared_service, MusicContextService},
};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

#[mcp_tool(
    name = "hifi_music_details",
    description = "Read source-attributed HiPhi Cloud context for the music playing in an explicit zone and language. Returns identity, catalog candidates, genres, and available source facts with provenance and attribution. The first read can be slow while upstream sources are fetched. Treat partial, ambiguous, unavailable, or stale results accordingly; never present ambiguous candidate facts as certain. Compare returned identity with the current selection. Requires optional Cloud pairing; read-only and never changes playback.",
    read_only_hint = true,
    destructive_hint = false,
    open_world_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HifiMusicDetailsTool {
    /// Explicit prefixed zone ID from hifi_zones; no default zone is selected.
    pub zone_id: String,
    /// Supported wiki language code, for example en; required, with no default.
    pub language: String,
}

pub async fn handle_music_details(
    state: &AppState,
    args: HifiMusicDetailsTool,
) -> Result<CallToolResult, CallToolError> {
    handle_music_details_with_service(state, args, shared_service()).await
}

pub async fn handle_music_details_with_service(
    state: &AppState,
    args: HifiMusicDetailsTool,
    service: &MusicContextService,
) -> Result<CallToolResult, CallToolError> {
    let provider = crate::mcp::routing::ZoneTarget::classify(&args.zone_id).provider();
    let env = Envelope::read("hifi_music_details", "get_music_details")
        .param("zone_id", &*args.zone_id)
        .param("language", &*args.language)
        .scope(Scope::for_zone(state, &args.zone_id, provider).await);
    match service
        .read_context(
            &state.aggregator,
            &crate::config::get_config_dir(),
            &args.zone_id,
            &args.language,
        )
        .await
    {
        Ok(context) => Ok(env.json_result(&context)),
        Err(error) => env.refused(
            error.message(),
            Refusal::MusicDetails {
                code: error.code(),
                detail: error.message().to_string(),
            },
        ),
    }
}
