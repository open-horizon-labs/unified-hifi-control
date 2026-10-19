//! Spotify catalog and library operations that do not map cleanly to the
//! provider-neutral transport tools.

use crate::adapters::spotify::SpotifyApiError;
use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Refusal};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::json;

#[mcp_tool(
    name = "hifi_spotify",
    description = "Spotify playlists and saved tracks; writes need matching scopes. Browse requires UHC_SPOTIFY_QUOTA_MODE=extended. Search/play with hifi_search/hifi_play_ref."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiSpotifyTool {
    /// playlists, playlist_tracks, saved_tracks, saved_tracks_check, saved_tracks_add, saved_tracks_remove, create_playlist, update_playlist, playlist_add, playlist_replace, playlist_remove; extended mode: browse_categories, browse_category_playlists, browse_featured_playlists, browse_new_releases.
    pub action: String,
    /// Playlist ID for playlist reads/edits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist_id: Option<String>,
    /// Category ID; extended browse only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category_id: Option<String>,
    /// Playlist name for create/update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Playlist description for create/update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Track URI for playlist edits.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// Track ID for saved-track operations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub track_id: Option<String>,
    /// Playlist visibility for create/update.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub public: Option<bool>,
    /// Maximum results.
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
        Err(error) => env.refused(
            format!("Spotify {} failed: {}", args.action, error),
            refusal_for_spotify_error(&error),
        ),
    }
}

pub(super) fn refusal_for_spotify_error(error: &anyhow::Error) -> Refusal {
    match error.downcast_ref::<SpotifyApiError>() {
        Some(SpotifyApiError::RateLimited { retry_after }) => Refusal::RateLimited {
            retry_after_seconds: retry_after.map(|delay| delay.as_secs()),
            detail: error.to_string(),
        },
        Some(SpotifyApiError::QuotaExceeded) => Refusal::QuotaExceeded {
            code: "QUOTA_EXCEEDED",
            detail: error.to_string(),
        },
        None => Refusal::backend_error(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn spotify_rate_and_quota_failures_have_distinct_structured_reasons() {
        let rate = anyhow::Error::new(SpotifyApiError::RateLimited {
            retry_after: Some(Duration::from_secs(17)),
        });
        let rate =
            serde_json::to_value(refusal_for_spotify_error(&rate)).expect("rate refusal JSON");
        assert_eq!(rate["reason"], "rate_limited");
        assert_eq!(rate["retry_after_seconds"], 17);
        assert!(!rate.to_string().contains("provider response body"));

        let quota = anyhow::Error::new(SpotifyApiError::QuotaExceeded);
        let quota =
            serde_json::to_value(refusal_for_spotify_error(&quota)).expect("quota refusal JSON");
        assert_eq!(quota["reason"], "quota_exceeded");
        assert_eq!(quota["code"], "QUOTA_EXCEEDED");
        assert!(!quota.to_string().contains("provider response body"));
    }
}
