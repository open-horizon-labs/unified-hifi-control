//! Apple Music catalog, library, playlist, and feedback operations.
//!
//! Apple authorization remains on the native companion. This tool forwards a
//! provider-neutral operation through the registered Apple Music library
//! adapter; the companion returns only the requested JSON result.

use crate::api::AppState;
use crate::mcp::envelope::Envelope;
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[mcp_tool(
    name = "hifi_apple_music",
    description = "Apple Music catalog, library, playlist, and feedback access through a paired native companion. Actions include catalog_search, library, playlists, playlist_tracks, recent, recommendations, favorites, queue_plan, playlist_create, playlist_update, playlist_add, playlist_remove, favorite_add/remove, and feedback. Apple authorization stays on the companion; operations are limited to documented MusicKit capabilities and may be refused when the companion or account cannot perform them. Use hifi_search/hifi_play for exact content selection and playback."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiAppleMusicTool {
    /// Operation to perform.
    pub action: String,
    /// Provider content or playlist identifier, held server-side where possible.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Search/query text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Opaque Apple Music ref or provider URI for a content operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Optional target execution-owner zone.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// Optional user-facing name/description for playlist operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Optional playlist description for playlist operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Explicit confirmation for destructive account mutations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<bool>,
    /// Maximum number of entries to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

pub async fn handle_apple_music(
    state: &AppState,
    args: HifiAppleMusicTool,
) -> Result<CallToolResult, CallToolError> {
    const ACTIONS: &[&str] = &[
        "catalog_search",
        "library",
        "playlists",
        "playlist_tracks",
        "recent",
        "recommendations",
        "favorites",
        "queue_plan",
        "playlist_create",
        "playlist_update",
        "playlist_add",
        "playlist_remove",
        "favorite_add",
        "favorite_remove",
        "feedback",
    ];
    if !ACTIONS.contains(&args.action.as_str()) {
        return Envelope::read("hifi_apple_music", "invalid_action").refused(
            format!("Unknown Apple Music action '{}'.", args.action),
            crate::mcp::envelope::Refusal::invalid_parameter(
                "action",
                ACTIONS,
                "Choose one of the documented Apple Music actions.",
            ),
        );
    }

    let mutation = matches!(
        args.action.as_str(),
        "playlist_create"
            | "playlist_update"
            | "playlist_add"
            | "playlist_remove"
            | "favorite_add"
            | "favorite_remove"
            | "feedback"
    );
    let confirmed = args.confirm.unwrap_or(false);
    if mutation && !confirmed {
        return Envelope::write("hifi_apple_music", "confirmation_required")
            .param("action", &*args.action)
            .refused(
                "This Apple Music account mutation requires explicit confirm=true.",
                crate::mcp::envelope::Refusal::InvalidParameter {
                    parameter: "confirm",
                    accepted: vec!["true".to_string()],
                    detail: "UHC will not make an account or playlist mutation without explicit confirmation.".to_string(),
                },
            );
    }

    let params = json!({
        "id": args.id,
        "query": args.query,
        "uri": args.uri,
        "zone_id": args.zone_id,
        "name": args.name,
        "description": args.description,
        "confirm": confirmed,
        "limit": args.limit,
    });
    let env =
        Envelope::write("hifi_apple_music", "apple_music_content").param("action", &*args.action);
    match state
        .adapter_registry
        .library_content("applemusic", &args.action, &params)
        .await
    {
        Ok(value) => Ok(env.json_result(&value)),
        Err(e) => env.failed(format!("Apple Music {} failed: {}", args.action, e)),
    }
}
