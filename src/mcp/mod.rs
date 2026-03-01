//! MCP (Model Context Protocol) server for AI assistant integration
//!
//! Provides HTTP endpoints for MCP clients with both Streamable HTTP and SSE transports.
//! Routes are integrated into the main Axum app on port 8088 at /mcp endpoint.

use crate::api::{load_app_settings, AppState};
use async_trait::async_trait;
use axum::http::{HeaderMap, Method, Uri};
use axum::{body::Body, extract::Extension, response::IntoResponse};
use rust_mcp_sdk::{
    id_generator::{FastIdGenerator, UuidGenerator},
    macros::{mcp_tool, JsonSchema},
    mcp_server::{McpAppState, McpHttpHandler, ServerHandler, ToMcpServerHandler},
    schema::{
        schema_utils::CallToolError, CallToolRequestParams, CallToolResult, Implementation,
        InitializeResult, ListToolsResult, PaginatedRequestParams, ProtocolVersion, RpcError,
        ServerCapabilities, ServerCapabilitiesTools, TextContent,
    },
    session_store::InMemorySessionStore,
    tool_box, McpServer, TransportOptions,
};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Duration};

/// MCP session header name
const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";

// ============================================================================
// Tool Definitions
// ============================================================================

/// List all available playback zones
#[mcp_tool(
    name = "hifi_zones",
    description = "List all available playback zones (Roon, LMS, OpenHome, UPnP)",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiZonesTool {}

/// Get current playback state for a zone
#[mcp_tool(
    name = "hifi_now_playing",
    description = "Get current playback state for a zone (track, artist, album, play state, volume)",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiNowPlayingTool {
    /// The zone ID to query (get from hifi_zones)
    pub zone_id: String,
}

/// Control playback
#[mcp_tool(
    name = "hifi_control",
    description = "Control playback: play, pause, playpause (toggle), next, previous, or adjust volume"
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiControlTool {
    /// The zone ID to control
    pub zone_id: String,
    /// Action: play, pause, playpause, next, previous, volume_set, volume_up, volume_down
    pub action: String,
    /// For volume actions: the level (0-100 for volume_set) or amount to change
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// Search for music across library and streaming services.
#[mcp_tool(
    name = "hifi_search",
    description = "Search for music across library and streaming services. Returns items with rich metadata (title, artist, album, year, genre, duration, image_url, source) and a _play token for playback via hifi_play_item. Library results include local files with full metadata. Streaming results include TIDAL/Qobuz items. Results are ready to evaluate and play.",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiSearchTool {
    /// Search query (e.g., "Hotel California", "Eagles", "jazz piano")
    pub query: String,
    /// Zone ID (determines which adapter to search). Required.
    pub zone_id: String,
    /// Search source: "library" (default), "tidal", "qobuz". Roon only.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Play a specific item by passing back keys from hifi_search results.
/// The caller (LLM) decides what to play; the bridge just executes.
#[mcp_tool(
    name = "hifi_play_item",
    description = "Play a specific item using keys from a prior hifi_search call. Pass the _play token from search results as the item parameter — do not modify it. The bridge routes to the correct adapter based on zone_id prefix."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiPlayItemTool {
    /// Zone ID to play on
    pub zone_id: String,
    /// All item fields from search results — pass back as-is
    pub item: serde_json::Value,
    /// Action: "play" (default), "queue", "radio" (Roon only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Get overall bridge status
#[mcp_tool(
    name = "hifi_status",
    description = "Get overall bridge status (Roon connection, HQPlayer config)",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiStatusTool {}

/// Get the current ESP device manifest (layout) for a zone
#[mcp_tool(
    name = "esp_get_manifest",
    description = "Get the current manifest (layout) for a zone's ESP device. Returns screens, elements, and encoder bindings. Returns a message if no custom manifest exists (zone is using default layout). Call before esp_configure to inspect existing layout.",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct EspGetManifestTool {
    /// Zone ID to query (get from hifi_zones)
    pub zone_id: String,
}

/// Configure an ESP device's layout via natural language
#[mcp_tool(
    name = "esp_configure",
    description = "Configure a zone's ESP device layout using natural language. Sends your prompt to an LLM which generates a new manifest and applies it immediately. Requires a Memex license. Example prompts: 'Add a mute button', 'Show 30-second skip for podcasts', 'Remove the previous button'."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct EspConfigureTool {
    /// Natural language description of the desired layout change
    pub prompt: String,
    /// Zone ID to configure (get from hifi_zones)
    pub zone_id: String,
}

/// List all actions that can be assigned to ESP device elements
#[mcp_tool(
    name = "esp_get_actions",
    description = "List all actions available for ESP device elements (tap, long-press, encoder turn/press). Use when building an esp_configure prompt that refers to specific actions.",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct EspGetActionsTool {}

/// List all icons available on the ESP device firmware
#[mcp_tool(
    name = "esp_get_icons",
    description = "List all icons the ESP device firmware can render. Use when building an esp_configure prompt that refers to specific icons.",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct EspGetIconsTool {}

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

// Generate toolbox enum with all tools
tool_box!(
    HifiTools,
    [
        HifiZonesTool,
        HifiNowPlayingTool,
        HifiControlTool,
        HifiSearchTool,
        HifiPlayItemTool,
        HifiStatusTool,
        HifiHqplayerStatusTool,
        HifiHqplayerProfilesTool,
        HifiHqplayerLoadProfileTool,
        HifiHqplayerSetPipelineTool,
        EspGetManifestTool,
        EspConfigureTool,
        EspGetActionsTool,
        EspGetIconsTool
    ]
);

// ============================================================================
// Response Types (for JSON serialization)
// ============================================================================

#[derive(Debug, Serialize)]
struct McpZone {
    zone_id: String,
    zone_name: String,
    state: String,
    volume: Option<f64>,
    is_muted: Option<bool>,
}

#[derive(Debug, Serialize)]
struct McpNowPlaying {
    zone_id: String,
    zone_name: String,
    state: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    volume: Option<f64>,
    is_muted: Option<bool>,
}

#[derive(Debug, Serialize)]
struct McpHqpStatus {
    connected: bool,
    host: Option<String>,
    pipeline: Option<McpPipelineStatus>,
}

#[derive(Debug, Serialize)]
struct McpPipelineStatus {
    state: String,
    filter: String,
    shaper: String,
    rate: u32,
}

// ============================================================================
// Server Handler
// ============================================================================

/// MCP server handler with access to app state
pub struct HifiMcpHandler {
    state: AppState,
}

impl HifiMcpHandler {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }

    fn text_result(text: String) -> CallToolResult {
        CallToolResult::text_content(vec![TextContent::from(text)])
    }

    fn error_result(msg: String) -> Result<CallToolResult, CallToolError> {
        Ok(CallToolResult::text_content(vec![TextContent::from(
            format!("Error: {}", msg),
        )]))
    }

    fn json_result<T: Serialize>(data: &T) -> CallToolResult {
        let json = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());
        Self::text_result(json)
    }

    // Helper method for volume control
    async fn set_volume(
        &self,
        zone_id: &str,
        value: f64,
        relative: bool,
    ) -> Result<CallToolResult, CallToolError> {
        let result = if zone_id.starts_with("lms:") {
            self.state
                .lms
                .change_volume(zone_id, value as f32, relative)
                .await
        } else if zone_id.starts_with("roon:") || !zone_id.contains(':') {
            self.state
                .roon
                .change_volume(zone_id, value as f32, relative)
                .await
        } else {
            return Self::error_result("Volume control not supported for this zone type".into());
        };

        match result {
            Ok(()) => Ok(Self::text_result(format!(
                "Volume {}",
                if relative { "adjusted" } else { "set" }
            ))),
            Err(e) => Self::error_result(format!("Volume error: {}", e)),
        }
    }
}

#[async_trait]
impl ServerHandler for HifiMcpHandler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        let mut tools = HifiTools::tools();

        // Filter out HQPlayer tools if adapter is disabled in settings
        let settings = load_app_settings();
        if !settings.adapters.hqplayer {
            tools.retain(|t| !t.name.starts_with("hifi_hqplayer"));
        }

        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools,
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: HifiTools = HifiTools::try_from(params).map_err(CallToolError::new)?;

        match tool {
            HifiTools::HifiZonesTool(_) => {
                let zones = self.state.aggregator.get_zones().await;
                let mcp_zones: Vec<McpZone> = zones
                    .into_iter()
                    .map(|z| McpZone {
                        zone_id: z.zone_id,
                        zone_name: z.zone_name,
                        state: z.state.to_string(),
                        volume: z.volume_control.as_ref().map(|v| v.value as f64),
                        is_muted: z.volume_control.as_ref().map(|v| v.is_muted),
                    })
                    .collect();
                Ok(Self::json_result(&mcp_zones))
            }

            HifiTools::HifiNowPlayingTool(args) => {
                match self.state.aggregator.get_zone(&args.zone_id).await {
                    Some(z) => {
                        let np = McpNowPlaying {
                            zone_id: z.zone_id,
                            zone_name: z.zone_name,
                            state: z.state.to_string(),
                            title: z.now_playing.as_ref().map(|n| n.title.clone()),
                            artist: z.now_playing.as_ref().map(|n| n.artist.clone()),
                            album: z.now_playing.as_ref().map(|n| n.album.clone()),
                            volume: z.volume_control.as_ref().map(|v| v.value as f64),
                            is_muted: z.volume_control.as_ref().map(|v| v.is_muted),
                        };
                        Ok(Self::json_result(&np))
                    }
                    None => Self::error_result(format!("Zone not found: {}", args.zone_id)),
                }
            }

            HifiTools::HifiControlTool(args) => {
                // Map MCP actions to backend actions
                let backend_action = match args.action.as_str() {
                    "play" => "play",
                    "pause" => "pause",
                    "playpause" => "play_pause",
                    "next" => "next",
                    "previous" | "prev" => "previous",
                    "volume_set" => {
                        if let Some(v) = args.value {
                            return self.set_volume(&args.zone_id, v, false).await;
                        }
                        return Self::error_result("volume_set requires a value (0-100)".into());
                    }
                    "volume_up" => {
                        let delta = args.value.unwrap_or(5.0);
                        return self.set_volume(&args.zone_id, delta, true).await;
                    }
                    "volume_down" => {
                        let delta = args.value.unwrap_or(5.0);
                        return self.set_volume(&args.zone_id, -delta, true).await;
                    }
                    other => other,
                };

                // Determine which adapter to use based on zone_id prefix
                let result = if args.zone_id.starts_with("lms:") {
                    self.state
                        .lms
                        .control(&args.zone_id, backend_action, None)
                        .await
                } else if args.zone_id.starts_with("openhome:") {
                    self.state
                        .openhome
                        .control(&args.zone_id, backend_action, None)
                        .await
                } else if args.zone_id.starts_with("upnp:") {
                    self.state
                        .upnp
                        .control(&args.zone_id, backend_action, None)
                        .await
                } else {
                    // Default to Roon
                    self.state.roon.control(&args.zone_id, backend_action).await
                };

                match result {
                    Ok(()) => {
                        // Return updated state
                        if let Some(zone) = self.state.aggregator.get_zone(&args.zone_id).await {
                            let np = McpNowPlaying {
                                zone_id: zone.zone_id,
                                zone_name: zone.zone_name,
                                state: zone.state.to_string(),
                                title: zone.now_playing.as_ref().map(|n| n.title.clone()),
                                artist: zone.now_playing.as_ref().map(|n| n.artist.clone()),
                                album: zone.now_playing.as_ref().map(|n| n.album.clone()),
                                volume: zone.volume_control.as_ref().map(|v| v.value as f64),
                                is_muted: zone.volume_control.as_ref().map(|v| v.is_muted),
                            };
                            let json = serde_json::to_string_pretty(&np)
                                .unwrap_or_else(|_| "{}".to_string());
                            Ok(Self::text_result(format!(
                                "Action '{}' executed.\n\nCurrent state:\n{}",
                                args.action, json
                            )))
                        } else {
                            Ok(Self::text_result(format!(
                                "Action '{}' executed.",
                                args.action
                            )))
                        }
                    }
                    Err(e) => Self::error_result(format!("Control error: {}", e)),
                }
            }

            HifiTools::HifiSearchTool(args) => {
                let use_lms = args.zone_id.starts_with("lms:");

                if use_lms {
                    match self
                        .state
                        .lms
                        .search_unified(&args.query, Some(&args.zone_id), Some(20))
                        .await
                    {
                        Ok(items) => {
                            let results: Vec<serde_json::Value> = items
                                .into_iter()
                                .map(|item| {
                                    let play = serde_json::json!({
                                        "adapter": "lms",
                                        "item_id": item.item_id,
                                        "url": item.url,
                                        "id": item.id,
                                        "result_type": item.result_type.to_string(),
                                    });
                                    serde_json::json!({
                                        "title": item.title,
                                        "artist": item.artist,
                                        "album": item.album,
                                        "category": item.result_type.to_string(),
                                        "year": item.year,
                                        "genre": item.genre,
                                        "duration": item.duration,
                                        "image_url": item.image_url,
                                        "source": if item.item_id.is_some() { "streaming" } else { "library" },
                                        "_play": play,
                                    })
                                })
                                .collect();
                            Ok(Self::json_result(&serde_json::json!({
                                "results": results
                            })))
                        }
                        Err(e) => Self::error_result(format!("Search error: {}", e)),
                    }
                } else {
                    use crate::adapters::roon::SearchSource;
                    let source = match args.source.as_deref() {
                        Some("tidal") => SearchSource::Tidal,
                        Some("qobuz") => SearchSource::Qobuz,
                        _ => SearchSource::Library,
                    };
                    let zone_id = Some(args.zone_id.as_str());
                    match self
                        .state
                        .roon
                        .search_playable(&args.query, zone_id, Some(20), source)
                        .await
                    {
                        Ok((session_key, items)) => {
                            let results: Vec<serde_json::Value> = items
                                .into_iter()
                                .map(|(category, item)| {
                                    let play = serde_json::json!({
                                        "adapter": "roon",
                                        "session_key": session_key,
                                        "item_key": item.item_key,
                                    });
                                    serde_json::json!({
                                        "title": item.title,
                                        "subtitle": item.subtitle,
                                        "category": category,
                                        "_play": play,
                                    })
                                })
                                .collect();
                            Ok(Self::json_result(&serde_json::json!({
                                "results": results
                            })))
                        }
                        Err(e) => Self::error_result(format!("Search error: {}", e)),
                    }
                }
            }
            HifiTools::HifiPlayItemTool(args) => {
                // Extract _play blob — the opaque token from search results
                let play = match args.item.get("_play").or(Some(&args.item)) {
                    Some(p) => p,
                    None => return Self::error_result("missing _play in item".into()),
                };
                let adapter = play.get("adapter").and_then(|v| v.as_str()).unwrap_or("");

                match adapter {
                    "lms" => {
                        use crate::adapters::lms::LmsPlayAction;
                        let action = LmsPlayAction::parse(args.action.as_deref());
                        // Pass the _play blob directly — play_item reads item_id/url/id from it
                        match self.state.lms.play_item(&args.zone_id, play, action).await {
                            Ok(message) => Ok(Self::text_result(message)),
                            Err(e) => Self::error_result(format!("Play error: {}", e)),
                        }
                    }
                    "roon" => {
                        let session_key = match play.get("session_key").and_then(|v| v.as_str()) {
                            Some(k) => k,
                            None => {
                                return Self::error_result("missing session_key in _play".into())
                            }
                        };
                        let item_key = match play.get("item_key").and_then(|v| v.as_str()) {
                            Some(k) => k,
                            None => return Self::error_result("missing item_key in _play".into()),
                        };
                        let action = {
                            use crate::adapters::roon::PlayAction;
                            PlayAction::parse(args.action.as_deref().unwrap_or("play"))
                        };
                        let item_title = args
                            .item
                            .get("title")
                            .and_then(|v| v.as_str())
                            .unwrap_or("unknown");
                        let bare_zone_id =
                            args.zone_id.strip_prefix("roon:").unwrap_or(&args.zone_id);
                        match self
                            .state
                            .roon
                            .execute_play_action(
                                session_key,
                                bare_zone_id,
                                item_title,
                                item_key,
                                action,
                            )
                            .await
                        {
                            Ok(message) => Ok(Self::text_result(message)),
                            Err(e) => Self::error_result(format!("Play error: {}", e)),
                        }
                    }
                    _ => {
                        Self::error_result(format!("Unknown adapter '{}' in _play token", adapter))
                    }
                }
            }

            HifiTools::HifiStatusTool(_) => {
                let roon_status = self.state.roon.get_status().await;
                let hqp_status = self.state.hqplayer.get_status().await;

                let status = serde_json::json!({
                    "roon": {
                        "connected": roon_status.connected,
                        "core_name": roon_status.core_name,
                    },
                    "hqplayer": {
                        "connected": hqp_status.connected,
                        "host": hqp_status.host,
                    }
                });
                Ok(Self::json_result(&status))
            }

            HifiTools::HifiHqplayerStatusTool(_) => {
                let status = self.state.hqplayer.get_status().await;
                let pipeline = self.state.hqplayer.get_pipeline_status().await.ok();

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
                Ok(Self::json_result(&mcp_status))
            }

            HifiTools::HifiHqplayerProfilesTool(_) => {
                let profiles = self.state.hqplayer.get_cached_profiles().await;
                let profile_names: Vec<String> = profiles.into_iter().map(|p| p.title).collect();
                Ok(Self::json_result(&profile_names))
            }

            HifiTools::HifiHqplayerLoadProfileTool(args) => {
                match self.state.hqplayer.load_profile(&args.profile).await {
                    Ok(()) => Ok(Self::text_result(format!(
                        "Loaded profile: {}",
                        args.profile
                    ))),
                    Err(e) => Self::error_result(format!("Failed to load profile: {}", e)),
                }
            }

            HifiTools::EspGetManifestTool(args) => {
                match self
                    .state
                    .manifests
                    .get_current_manifest_json(&args.zone_id)
                    .await
                {
                    Some(json) => {
                        let text = serde_json::to_string_pretty(&json)
                            .unwrap_or_else(|_| "{}".to_string());
                        Ok(Self::text_result(text))
                    }
                    None => Ok(Self::text_result(format!(
                        "No custom manifest for zone '{}' — using default layout.",
                        args.zone_id
                    ))),
                }
            }

            HifiTools::EspConfigureTool(args) => {
                use crate::knobs::llm_manifest::{
                    build_system_prompt, parse_manifest_from_llm_text, LlmProxyClient,
                    RealLlmProxyClient,
                };

                let license = match self.state.event_reporter.get_license().await {
                    Some(l) => l,
                    None => {
                        return Self::error_result(
                            "Memex license required for knob_configure".into(),
                        )
                    }
                };

                let current_manifest_json = match self
                    .state
                    .manifests
                    .get_current_manifest_json(&args.zone_id)
                    .await
                {
                    Some(json) => {
                        serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string())
                    }
                    None => serde_json::to_string_pretty(&serde_json::json!({
                        "screens": [{"type": "media", "id": "now_playing", "lines": [
                            {"text": "Now Playing", "style": "title"},
                            {"text": "Artist", "style": "subtitle"}
                        ]}],
                        "nav": {"order": ["now_playing"], "default": "now_playing"}
                    }))
                    .unwrap_or_else(|_| "{}".to_string()),
                };

                let system_prompt = build_system_prompt(&current_manifest_json, "dial");
                let llm_client = RealLlmProxyClient::new();
                let llm_response = match llm_client
                    .call(&system_prompt, &args.prompt, &license)
                    .await
                {
                    Ok(r) => r,
                    Err(e) => return Self::error_result(format!("LLM proxy error: {}", e)),
                };

                let parsed = match parse_manifest_from_llm_text(&llm_response) {
                    Ok(p) => p,
                    Err(e) => {
                        return Self::error_result(format!("Failed to parse LLM output: {}", e))
                    }
                };

                self.state
                    .manifests
                    .set_full(
                        &args.zone_id,
                        parsed.screens,
                        parsed.nav,
                        parsed.interactions,
                    )
                    .await;

                Ok(Self::text_result(format!(
                    "Manifest updated for zone '{}'. The knob will show the new layout on next poll.",
                    args.zone_id
                )))
            }

            HifiTools::EspGetActionsTool(_) => {
                let actions = crate::knobs::manifest_routes::actions_handler().await.0;
                Ok(Self::json_result(&actions))
            }

            HifiTools::EspGetIconsTool(_) => {
                let icons = crate::knobs::manifest_routes::icons_handler().await.0;
                Ok(Self::json_result(&icons))
            }

            HifiTools::HifiHqplayerSetPipelineTool(args) => {
                // All settings now use name-based lookups - adapter handles conversion
                // Only samplerate needs numeric parsing (Hz value)
                let result = match args.setting.as_str() {
                    "mode" => {
                        // Accepts name like "PCM", "DSD", "[source]"
                        self.state.hqplayer.set_mode(&args.value).await
                    }
                    "filter1x" | "filter_1x" => {
                        self.state.hqplayer.set_filter_1x(&args.value).await
                    }
                    "filterNx" | "filter_nx" | "filternx" => {
                        self.state.hqplayer.set_filter_nx(&args.value).await
                    }
                    "shaper" | "dither" => self.state.hqplayer.set_shaper(&args.value).await,
                    "rate" | "samplerate" => {
                        // Samplerate uses Hz value (e.g., "48000", "96000")
                        if let Ok(v) = args.value.parse::<u32>() {
                            self.state.hqplayer.set_rate(v).await
                        } else {
                            return Self::error_result(
                                "Invalid rate value (expected Hz like 48000, 96000)".into(),
                            );
                        }
                    }
                    _ => {
                        return Self::error_result(format!(
                            "Unknown setting: {}. Valid: mode, samplerate, filter1x, filterNx, shaper, dither",
                            args.setting
                        ));
                    }
                };

                match result {
                    Ok(()) => Ok(Self::text_result(format!(
                        "Set {} to {}",
                        args.setting, args.value
                    ))),
                    Err(e) => Self::error_result(format!("Failed to set {}: {}", args.setting, e)),
                }
            }
        }
    }
}

// ============================================================================
// MCP State Container (for Extension layer)
// ============================================================================

/// Container for MCP-specific state, passed via Extension
#[derive(Clone)]
pub struct McpExtState {
    pub mcp_state: Arc<McpAppState>,
    pub http_handler: Arc<McpHttpHandler>,
}

// ============================================================================
// Axum Route Handlers (mirrors rust-mcp-sdk's internal handlers)
// ============================================================================

pub async fn handle_mcp_get(
    headers: HeaderMap,
    uri: Uri,
    Extension(ext): Extension<McpExtState>,
) -> impl IntoResponse {
    let request = McpHttpHandler::create_request(Method::GET, uri, headers, None);
    match ext
        .http_handler
        .handle_streamable_http(request, ext.mcp_state)
        .await
    {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            axum::response::Response::from_parts(parts, Body::new(body))
        }
        // Response builder with valid status/body cannot fail
        #[allow(clippy::unwrap_used)]
        Err(e) => axum::response::Response::builder()
            .status(500)
            .body(Body::from(format!("MCP error: {}", e)))
            .unwrap(),
    }
}

pub async fn handle_mcp_post(
    headers: HeaderMap,
    uri: Uri,
    Extension(ext): Extension<McpExtState>,
    payload: String,
) -> impl IntoResponse {
    // Check for stale session and auto-recover
    let headers = match auto_recover_session(&headers, &uri, &ext, &payload).await {
        Some(new_headers) => new_headers,
        None => headers,
    };

    let request = McpHttpHandler::create_request(Method::POST, uri, headers, Some(&payload));
    match ext
        .http_handler
        .handle_streamable_http(request, ext.mcp_state)
        .await
    {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            axum::response::Response::from_parts(parts, Body::new(body))
        }
        // Response builder with valid status/body cannot fail
        #[allow(clippy::unwrap_used)]
        Err(e) => axum::response::Response::builder()
            .status(500)
            .body(Body::from(format!("MCP error: {}", e)))
            .unwrap(),
    }
}

/// Check if client has a stale session and auto-initialize a new one.
/// Returns new headers with fresh session ID, or None if no recovery needed.
async fn auto_recover_session(
    headers: &HeaderMap,
    uri: &Uri,
    ext: &McpExtState,
    _payload: &str,
) -> Option<HeaderMap> {
    // Get session ID from header
    let session_id = headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())?;

    // Check if session exists
    if ext
        .mcp_state
        .session_store
        .has(&session_id.to_string())
        .await
    {
        return None; // Session is valid, no recovery needed
    }

    tracing::info!(
        "MCP session '{}' not found, auto-initializing new session",
        session_id
    );

    // Create initialize request to get a new session
    let init_payload = r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"auto-recovery","version":"1.0"}}}"#;

    // Create headers without the stale session ID (so SDK creates new session)
    let mut init_headers = headers.clone();
    init_headers.remove(MCP_SESSION_ID_HEADER);

    let init_request =
        McpHttpHandler::create_request(Method::POST, uri.clone(), init_headers, Some(init_payload));

    // Process initialize request
    let init_response = ext
        .http_handler
        .handle_streamable_http(init_request, ext.mcp_state.clone())
        .await
        .ok()?;

    // Extract new session ID from response headers
    let new_session_id = init_response
        .headers()
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())?;

    tracing::info!("Auto-initialized new MCP session: {}", new_session_id);

    // Create new headers with the fresh session ID
    let mut new_headers = headers.clone();
    new_headers.remove(MCP_SESSION_ID_HEADER);
    new_headers.insert(MCP_SESSION_ID_HEADER, new_session_id.parse().ok()?);

    Some(new_headers)
}

pub async fn handle_mcp_delete(
    headers: HeaderMap,
    uri: Uri,
    Extension(ext): Extension<McpExtState>,
) -> impl IntoResponse {
    let request = McpHttpHandler::create_request(Method::DELETE, uri, headers, None);
    match ext
        .http_handler
        .handle_streamable_http(request, ext.mcp_state)
        .await
    {
        Ok(res) => {
            let (parts, body) = res.into_parts();
            axum::response::Response::from_parts(parts, Body::new(body))
        }
        // Response builder with valid status/body cannot fail
        #[allow(clippy::unwrap_used)]
        Err(e) => axum::response::Response::builder()
            .status(500)
            .body(Body::from(format!("MCP error: {}", e)))
            .unwrap(),
    }
}

// ============================================================================
// Router Creation
// ============================================================================

/// Create MCP extension layer for the main Axum app
///
/// Call this to get the extension layer, then add MCP routes and the layer to your router.
pub fn create_mcp_extension(state: AppState) -> axum::Extension<McpExtState> {
    let server_details = InitializeResult {
        server_info: Implementation {
            name: "unified-hifi-control".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            title: Some("Unified Hi-Fi Control".into()),
            description: Some("Control your music system via MCP".into()),
            icons: vec![],
            website_url: Some("https://github.com/open-horizon-labs/unified-hifi-control".into()),
        },
        capabilities: ServerCapabilities {
            tools: Some(ServerCapabilitiesTools { list_changed: None }),
            ..Default::default()
        },
        meta: None,
        instructions: Some(
            "Unified Hi-Fi Control MCP Server - Control Your Music System\n\n\
            Use hifi_zones to list available zones, hifi_now_playing to see what's playing, \
            hifi_control for playback control, hifi_search to find music, and hifi_play_item to play it.\n\n\
            Note: hifi_search and hifi_play_item currently work with Roon and LMS zones only. \
            Transport controls (play/pause/next/volume) work with all zones (Roon, LMS, OpenHome, UPnP).\n\n\
            To build a playlist: call hifi_search to find tracks, then hifi_play_item for each. The first track \
            can use action='play' to start playback, then subsequent tracks use action='queue' to add to the queue."
                .into(),
        ),
        protocol_version: ProtocolVersion::V2025_11_25.into(),
    };

    let handler = HifiMcpHandler::new(state);

    // Create MCP app state (mirrors what HyperServer does internally)
    let mcp_state: Arc<McpAppState> = Arc::new(McpAppState {
        session_store: Arc::new(InMemorySessionStore::new()),
        id_generator: Arc::new(UuidGenerator {}),
        stream_id_gen: Arc::new(FastIdGenerator::new(Some("s_"))),
        server_details: Arc::new(server_details),
        handler: handler.to_mcp_server_handler(),
        ping_interval: Duration::from_secs(12),
        transport_options: Arc::new(TransportOptions::default()),
        enable_json_response: false,
        event_store: None,
        task_store: None,
        client_task_store: None,
    });

    // Create HTTP handler (no auth, no middleware)
    let http_handler = Arc::new(McpHttpHandler::new(vec![]));

    // Bundle into extension state
    let ext_state = McpExtState {
        mcp_state,
        http_handler,
    };

    tracing::info!("MCP endpoint available at /mcp (Streamable HTTP)");

    Extension(ext_state)
}
