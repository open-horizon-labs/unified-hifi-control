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
    description = "Read HiPhi Cloud source facts with provenance and attribution for current music; requires Cloud pairing. The first read can be slow. Check identity and stale/partial results; never present ambiguous candidate facts as certain.",
    read_only_hint = true,
    destructive_hint = false,
    open_world_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HifiMusicDetailsTool {
    /// Explicit zone ID; no default.
    pub zone_id: String,
    /// Wiki language code, e.g. en; required, no default.
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
