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

/// Search for music
#[mcp_tool(
    name = "hifi_search",
    description = "Search for tracks, albums, or artists in Library, TIDAL, or Qobuz",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiSearchTool {
    /// Search query (e.g., "Hotel California", "Eagles", "jazz piano")
    pub query: String,
    /// Optional zone ID for context-aware results
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// Where to search: "library" (default), "tidal", or "qobuz"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Search and play music - the AI DJ command
#[mcp_tool(
    name = "hifi_play",
    description = "Search and play music - the AI DJ command. Searches and immediately plays the first matching result."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiPlayTool {
    /// What to play (e.g., "early Michael Jackson", "Dark Side of the Moon")
    pub query: String,
    /// Zone ID to play on (get from hifi_zones)
    pub zone_id: String,
    /// Where to search: "library" (default), "tidal", or "qobuz"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// What to do: "play" (default), "queue", or "radio"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Play a specific item by its item_key
#[mcp_tool(
    name = "hifi_play_item",
    description = "Play a specific item by its item_key (from hifi_search or hifi_browse results). Use this when you want to play a specific search result rather than the first match."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiPlayItemTool {
    /// The item_key from search or browse results
    pub item_key: String,
    /// Zone ID to play on (get from hifi_zones)
    pub zone_id: String,
    /// What to do: "play" (default), "queue", or "radio"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Navigate the Roon library hierarchy
#[mcp_tool(
    name = "hifi_browse",
    description = "Navigate the Roon library hierarchy (artists, albums, genres, etc). Returns items at the current level. Use session_key from previous response to maintain navigation state.",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiBrowseTool {
    /// Key of item to browse into (from previous browse or search)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_key: Option<String>,
    /// Session key from previous browse to maintain navigation state
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_key: Option<String>,
    /// Optional zone ID for context
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// Search input for this browse level
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<String>,
    /// Reset to root level
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pop_all: Option<bool>,
}

/// Check if Roon Browse service is connected
#[mcp_tool(
    name = "hifi_browse_status",
    description = "Check if the Roon Browse service is connected",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiBrowseStatusTool {}

/// Get overall bridge status
#[mcp_tool(
    name = "hifi_status",
    description = "Get overall bridge status (Roon connection, HQPlayer config)",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiStatusTool {}

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
        HifiPlayTool,
        HifiPlayItemTool,
        HifiBrowseTool,
        HifiBrowseStatusTool,
        HifiStatusTool,
        HifiHqplayerStatusTool,
        HifiHqplayerProfilesTool,
        HifiHqplayerLoadProfileTool,
        HifiHqplayerSetPipelineTool
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
struct McpSearchResult {
    title: String,
    subtitle: Option<String>,
    item_key: Option<String>,
}

#[derive(Debug, Serialize)]
struct McpBrowseResult {
    items: Vec<McpSearchResult>,
    session_key: Option<String>,
    list_title: Option<String>,
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
                use crate::adapters::roon::SearchSource;

                let source = match args.source.as_deref() {
                    Some("tidal") => SearchSource::Tidal,
                    Some("qobuz") => SearchSource::Qobuz,
                    _ => SearchSource::Library,
                };
                let zone_id = args.zone_id.as_deref();

                match self
                    .state
                    .roon
                    .search(&args.query, zone_id, Some(10), source)
                    .await
                {
                    Ok(results) => {
                        let mcp_results: Vec<McpSearchResult> = results
                            .into_iter()
                            .map(|item| McpSearchResult {
                                title: item.title,
                                subtitle: item.subtitle,
                                item_key: item.item_key,
                            })
                            .collect();
                        Ok(Self::json_result(&mcp_results))
                    }
                    Err(e) => Self::error_result(format!("Search error: {}", e)),
                }
            }

            HifiTools::HifiPlayTool(args) => {
                use crate::adapters::roon::{PlayAction, SearchSource};

                let source = match args.source.as_deref() {
                    Some("tidal") => SearchSource::Tidal,
                    Some("qobuz") => SearchSource::Qobuz,
                    _ => SearchSource::Library,
                };
                let action = PlayAction::parse(args.action.as_deref().unwrap_or("play"));

                match self
                    .state
                    .roon
                    .search_and_play(&args.query, &args.zone_id, source, action)
                    .await
                {
                    Ok(message) => Ok(Self::text_result(message)),
                    Err(e) => Self::error_result(format!("Play error: {}", e)),
                }
            }

            HifiTools::HifiPlayItemTool(args) => {
                use crate::adapters::roon::PlayAction;

                let action = PlayAction::parse(args.action.as_deref().unwrap_or("play"));

                match self
                    .state
                    .roon
                    .play_item(&args.item_key, &args.zone_id, action)
                    .await
                {
                    Ok(message) => Ok(Self::text_result(message)),
                    Err(e) => Self::error_result(format!("Play item error: {}", e)),
                }
            }

            HifiTools::HifiBrowseTool(args) => {
                use roon_api::browse::{BrowseOpts, LoadOpts};

                // Generate or use provided session key
                let session_key = args.session_key.unwrap_or_else(|| {
                    format!(
                        "mcp_browse_{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    )
                });

                let opts = BrowseOpts {
                    item_key: args.item_key,
                    multi_session_key: Some(session_key.clone()),
                    zone_or_output_id: args.zone_id,
                    input: args.input,
                    pop_all: args.pop_all.unwrap_or(false),
                    ..Default::default()
                };

                match self.state.roon.browse(opts).await {
                    Ok(result) => {
                        match self
                            .state
                            .roon
                            .load(LoadOpts {
                                multi_session_key: Some(session_key.clone()),
                                count: Some(20),
                                ..Default::default()
                            })
                            .await
                        {
                            Ok(items) => {
                                let mcp_result = McpBrowseResult {
                                    items: items
                                        .items
                                        .into_iter()
                                        .map(|item| McpSearchResult {
                                            title: item.title,
                                            subtitle: item.subtitle,
                                            item_key: item.item_key,
                                        })
                                        .collect(),
                                    session_key: Some(session_key),
                                    list_title: result.list.as_ref().map(|l| l.title.clone()),
                                };
                                Ok(Self::json_result(&mcp_result))
                            }
                            Err(e) => Self::error_result(format!("Browse load error: {}", e)),
                        }
                    }
                    Err(e) => Self::error_result(format!("Browse error: {}", e)),
                }
            }

            HifiTools::HifiBrowseStatusTool(_) => {
                let connected = self.state.roon.is_browse_connected().await;
                let json = serde_json::json!({
                    "connected": connected,
                    "service": "roon_browse"
                });
                Ok(Self::json_result(&json))
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

            HifiTools::HifiHqplayerSetPipelineTool(args) => {
                // Parse as i64 to allow negative values (e.g., -1 for PCM mode), then cast to u32
                let parse_value = |v: &str| v.parse::<i64>().ok().map(|n| n as u32);

                let result = match args.setting.as_str() {
                    "filter1x" | "filter_1x" => {
                        if let Some(v) = parse_value(&args.value) {
                            self.state.hqplayer.set_filter_1x(v).await
                        } else {
                            return Self::error_result(
                                "Invalid filter1x value (expected integer)".into(),
                            );
                        }
                    }
                    "filterNx" | "filter_nx" | "filternx" => {
                        if let Some(v) = parse_value(&args.value) {
                            self.state.hqplayer.set_filter_nx(v).await
                        } else {
                            return Self::error_result(
                                "Invalid filterNx value (expected integer)".into(),
                            );
                        }
                    }
                    "shaper" | "dither" => {
                        // shaper (DSD) and dither (PCM) use the same HQPlayer API
                        if let Some(v) = parse_value(&args.value) {
                            self.state.hqplayer.set_shaper(v).await
                        } else {
                            return Self::error_result(
                                "Invalid shaper/dither value (expected integer)".into(),
                            );
                        }
                    }
                    "rate" | "samplerate" => {
                        if let Some(v) = parse_value(&args.value) {
                            self.state.hqplayer.set_rate(v).await
                        } else {
                            return Self::error_result(
                                "Invalid rate value (expected integer)".into(),
                            );
                        }
                    }
                    "mode" => {
                        if let Some(v) = parse_value(&args.value) {
                            self.state.hqplayer.set_mode(v).await
                        } else {
                            return Self::error_result(
                                "Invalid mode value (expected integer)".into(),
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
            hifi_control for playback control, hifi_search to find music, and hifi_play to play it."
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
