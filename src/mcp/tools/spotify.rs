//! Spotify catalog and library operations that do not map cleanly to the
//! provider-neutral transport tools.

use crate::api::AppState;
use crate::mcp::envelope::Envelope;
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[mcp_tool(
    name = "hifi_spotify",
    description = "Spotify catalog and library access. Actions include browse_categories, browse_category_playlists, browse_featured_playlists, browse_new_releases, playlists, playlist_tracks, saved_tracks, saved_tracks_check, saved_tracks_add/remove, create_playlist, update_playlist, playlist_add/replace/remove. Use hifi_search and hifi_play for catalog search and playback. Playlist/library writes require the corresponding Spotify scopes."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiSpotifyTool {
    /// Operation to perform.
    pub action: String,
    /// Playlist id for playlist_tracks, update_playlist, or playlist_add.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<String>,
    /// Spotify browse category id.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
    /// Playlist name for create/update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Playlist description for create/update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Track URI for playlist_add.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Track id for saved-library membership or mutation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    /// Public visibility for create/update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    /// Maximum number of entries to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
}

pub async fn handle_spotify(
    state: &AppState,
    args: HifiSpotifyTool,
) -> Result<CallToolResult, CallToolError> {
    let accepted = [
        "browse_categories",
        "browse_category_playlists",
        "browse_featured_playlists",
        "browse_new_releases",
        "playlists",
        "playlist_tracks",
        "saved_tracks",
        "create_playlist",
        "update_playlist",
        "playlist_add",
        "playlist_replace",
        "playlist_remove",
        "saved_tracks_check",
        "saved_tracks_add",
        "saved_tracks_remove",
    ];
    if !accepted.contains(&args.action.as_str()) {
        return Envelope::read("hifi_spotify", "invalid_action").refused(
            format!("Unknown Spotify action '{}'.", args.action),
            crate::mcp::envelope::Refusal::invalid_parameter(
                "action",
                &accepted,
                "Choose one of the documented Spotify actions.",
            ),
        );
    }
    let params = json!({
        "playlist_id": args.playlist_id,
        "category_id": args.category_id,
        "name": args.name,
        "description": args.description,
        "uri": args.uri,
        "track_id": args.track_id,
        "public": args.public,
        "limit": args.limit,
    });
    let env = Envelope::write("hifi_spotify", "spotify_content").param("action", &*args.action);
    match state
        .adapter_registry
        .library_content("spotify", &args.action, &params)
        .await
    {
        Ok(value) => Ok(env.json_result(&value)),
        Err(e) => env.failed(format!("Spotify {} failed: {}", args.action, e)),
    }
}
