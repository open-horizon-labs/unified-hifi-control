//! MCP (Model Context Protocol) server for AI assistant integration
//!
//! Provides an HTTP endpoint at `/mcp` for mobile MCP clients like BoltAI iOS.
//! Uses the official rmcp SDK with Streamable HTTP transport.

use crate::api::AppState;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler,
};
use serde::{Deserialize, Serialize};

/// MCP server for Hi-Fi control
#[derive(Clone)]
pub struct HifiMcpServer {
    state: AppState,
    tool_router: ToolRouter<HifiMcpServer>,
}

/// Zone info returned by hifi_zones
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct McpZone {
    pub zone_id: String,
    pub zone_name: String,
    pub state: String,
    pub volume: Option<f64>,
    pub is_muted: Option<bool>,
}

/// Now playing info returned by hifi_now_playing
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct McpNowPlaying {
    pub zone_id: String,
    pub zone_name: String,
    pub state: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub volume: Option<f64>,
    pub is_muted: Option<bool>,
}

/// Arguments for hifi_now_playing
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct NowPlayingArgs {
    /// The zone ID to query (get from hifi_zones)
    pub zone_id: String,
}

/// Arguments for hifi_control
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ControlArgs {
    /// The zone ID to control
    pub zone_id: String,
    /// Action: play, pause, next, previous, volume_set, volume_up, volume_down
    pub action: String,
    /// For volume actions: the level (0-100 for volume_set) or amount to change
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<f64>,
}

/// Arguments for hifi_search
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Search query (e.g., "Hotel California", "Eagles", "jazz piano")
    pub query: String,
    /// Optional zone ID for context-aware results
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// Where to search: "library" (default), "tidal", or "qobuz"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
}

/// Search result item (simplified from Roon's BrowseItem)
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct McpSearchResult {
    pub title: String,
    pub subtitle: Option<String>,
    pub item_key: Option<String>,
}

/// Arguments for hifi_play
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlayArgs {
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

/// Arguments for hifi_play_item
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct PlayItemArgs {
    /// The item_key from search or browse results
    pub item_key: String,
    /// Zone ID to play on (get from hifi_zones)
    pub zone_id: String,
    /// What to do: "play" (default), "queue", or "radio"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
}

/// Arguments for hifi_browse
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct BrowseArgs {
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

/// Browse result item
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct McpBrowseResult {
    pub items: Vec<McpSearchResult>,
    pub session_key: Option<String>,
    pub list_title: Option<String>,
}

/// Arguments for hifi_hqplayer_set_pipeline
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HqpSetPipelineArgs {
    /// Setting to change: mode, samplerate, filter1x, filterNx, shaper, dither
    pub setting: String,
    /// New value for the setting
    pub value: String,
}

/// Arguments for hifi_hqplayer_load_profile
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct HqpLoadProfileArgs {
    /// Configuration name to load (get from hifi_hqplayer_profiles)
    pub profile: String,
}

/// HQPlayer status response
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct McpHqpStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub pipeline: Option<McpPipelineStatus>,
}

/// Pipeline status
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct McpPipelineStatus {
    pub state: String,
    pub filter: String,
    pub shaper: String,
    pub rate: u32,
}

#[tool_router]
impl HifiMcpServer {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    /// List all available playback zones
    #[tool(description = "List all available playback zones (Roon, LMS, OpenHome, UPnP)")]
    async fn hifi_zones(&self) -> Result<CallToolResult, McpError> {
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

        let json = serde_json::to_string_pretty(&mcp_zones).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Get current playback state for a zone
    #[tool(
        description = "Get current playback state for a zone (track, artist, album, play state, volume)"
    )]
    async fn hifi_now_playing(
        &self,
        Parameters(args): Parameters<NowPlayingArgs>,
    ) -> Result<CallToolResult, McpError> {
        let zone = self.state.aggregator.get_zone(&args.zone_id).await;

        match zone {
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
                let json = serde_json::to_string_pretty(&np).unwrap_or_else(|_| "{}".to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            None => Ok(CallToolResult::error(vec![Content::text(format!(
                "Zone not found: {}",
                args.zone_id
            ))])),
        }
    }

    /// Control playback: play, pause, next, previous, or adjust volume
    #[tool(description = "Control playback: play, pause, next, previous, or adjust volume")]
    async fn hifi_control(
        &self,
        Parameters(args): Parameters<ControlArgs>,
    ) -> Result<CallToolResult, McpError> {
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
                return Ok(CallToolResult::error(vec![Content::text(
                    "volume_set requires a value (0-100)",
                )]));
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
                    let json =
                        serde_json::to_string_pretty(&np).unwrap_or_else(|_| "{}".to_string());
                    Ok(CallToolResult::success(vec![Content::text(format!(
                        "Action '{}' executed.\n\nCurrent state:\n{}",
                        args.action, json
                    ))]))
                } else {
                    Ok(CallToolResult::success(vec![Content::text(format!(
                        "Action '{}' executed.",
                        args.action
                    ))]))
                }
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Control error: {}",
                e
            ))])),
        }
    }

    /// Search for tracks, albums, or artists
    #[tool(description = "Search for tracks, albums, or artists in Library, TIDAL, or Qobuz")]
    async fn hifi_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
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
                // Convert BrowseItems to serializable McpSearchResults
                let mcp_results: Vec<McpSearchResult> = results
                    .into_iter()
                    .map(|item| McpSearchResult {
                        title: item.title,
                        subtitle: item.subtitle,
                        item_key: item.item_key,
                    })
                    .collect();
                let json =
                    serde_json::to_string_pretty(&mcp_results).unwrap_or_else(|_| "[]".to_string());
                Ok(CallToolResult::success(vec![Content::text(json)]))
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Search error: {}",
                e
            ))])),
        }
    }

    /// Search and play music - the AI DJ command
    #[tool(
        description = "Search and play music - the AI DJ command. Searches and immediately plays the first matching result."
    )]
    async fn hifi_play(
        &self,
        Parameters(args): Parameters<PlayArgs>,
    ) -> Result<CallToolResult, McpError> {
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
            Ok(message) => Ok(CallToolResult::success(vec![Content::text(message)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Play error: {}",
                e
            ))])),
        }
    }

    /// Play a specific item by its item_key
    #[tool(
        description = "Play a specific item by its item_key (from hifi_search or hifi_browse results). Use this when you want to play a specific search result rather than the first match."
    )]
    async fn hifi_play_item(
        &self,
        Parameters(args): Parameters<PlayItemArgs>,
    ) -> Result<CallToolResult, McpError> {
        use crate::adapters::roon::PlayAction;

        let action = PlayAction::parse(args.action.as_deref().unwrap_or("play"));

        match self
            .state
            .roon
            .play_item(&args.item_key, &args.zone_id, action)
            .await
        {
            Ok(message) => Ok(CallToolResult::success(vec![Content::text(message)])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Play item error: {}",
                e
            ))])),
        }
    }

    /// Navigate the Roon library hierarchy
    #[tool(
        description = "Navigate the Roon library hierarchy (artists, albums, genres, etc). Returns items at the current level. Use session_key from previous response to maintain navigation state."
    )]
    async fn hifi_browse(
        &self,
        Parameters(args): Parameters<BrowseArgs>,
    ) -> Result<CallToolResult, McpError> {
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
                // Load items using the same session key
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
                        let json = serde_json::to_string_pretty(&mcp_result)
                            .unwrap_or_else(|_| "{}".to_string());
                        Ok(CallToolResult::success(vec![Content::text(json)]))
                    }
                    Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                        "Browse load error: {}",
                        e
                    ))])),
                }
            }
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Browse error: {}",
                e
            ))])),
        }
    }

    /// Check if the Roon Browse service is connected
    #[tool(description = "Check if the Roon Browse service is connected")]
    async fn hifi_browse_status(&self) -> Result<CallToolResult, McpError> {
        let connected = self.state.roon.is_browse_connected().await;
        let json = serde_json::json!({
            "connected": connected,
            "service": "roon_browse"
        });
        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&json).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }

    /// Get overall bridge status
    #[tool(description = "Get overall bridge status (Roon connection, HQPlayer config)")]
    async fn hifi_status(&self) -> Result<CallToolResult, McpError> {
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

        Ok(CallToolResult::success(vec![Content::text(
            serde_json::to_string_pretty(&status).unwrap_or_else(|_| "{}".to_string()),
        )]))
    }

    /// Get HQPlayer Embedded status and current pipeline settings
    #[tool(description = "Get HQPlayer Embedded status and current pipeline settings")]
    async fn hifi_hqplayer_status(&self) -> Result<CallToolResult, McpError> {
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

        let json = serde_json::to_string_pretty(&mcp_status).unwrap_or_else(|_| "{}".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// List available HQPlayer Embedded configurations
    #[tool(description = "List available HQPlayer Embedded configurations")]
    async fn hifi_hqplayer_profiles(&self) -> Result<CallToolResult, McpError> {
        let profiles = self.state.hqplayer.get_cached_profiles().await;
        let profile_names: Vec<String> = profiles.into_iter().map(|p| p.title).collect();
        let json =
            serde_json::to_string_pretty(&profile_names).unwrap_or_else(|_| "[]".to_string());
        Ok(CallToolResult::success(vec![Content::text(json)]))
    }

    /// Load an HQPlayer Embedded configuration
    #[tool(description = "Load an HQPlayer Embedded configuration (will restart HQPlayer)")]
    async fn hifi_hqplayer_load_profile(
        &self,
        Parameters(args): Parameters<HqpLoadProfileArgs>,
    ) -> Result<CallToolResult, McpError> {
        match self.state.hqplayer.load_profile(&args.profile).await {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Loaded profile: {}",
                args.profile
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Failed to load profile: {}",
                e
            ))])),
        }
    }

    /// Change an HQPlayer pipeline setting
    #[tool(
        description = "Change an HQPlayer pipeline setting (mode, samplerate, filter1x, filterNx, shaper, dither)"
    )]
    async fn hifi_hqplayer_set_pipeline(
        &self,
        Parameters(args): Parameters<HqpSetPipelineArgs>,
    ) -> Result<CallToolResult, McpError> {
        // Parse as i64 to allow negative values (e.g., -1 for PCM mode), then cast to u32
        // This matches the HTTP /hqp/pipeline handler behavior
        let parse_value = |v: &str| v.parse::<i64>().ok().map(|n| n as u32);

        let result = match args.setting.as_str() {
            "filter1x" | "filter_1x" => {
                if let Some(v) = parse_value(&args.value) {
                    self.state.hqplayer.set_filter_1x(v).await
                } else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Invalid filter1x value (expected integer)",
                    )]));
                }
            }
            "filterNx" | "filter_nx" | "filternx" => {
                if let Some(v) = parse_value(&args.value) {
                    self.state.hqplayer.set_filter_nx(v).await
                } else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Invalid filterNx value (expected integer)",
                    )]));
                }
            }
            "shaper" | "dither" => {
                // shaper (DSD) and dither (PCM) use the same HQPlayer API
                if let Some(v) = parse_value(&args.value) {
                    self.state.hqplayer.set_shaper(v).await
                } else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Invalid shaper/dither value (expected integer)",
                    )]));
                }
            }
            "rate" | "samplerate" => {
                if let Some(v) = parse_value(&args.value) {
                    self.state.hqplayer.set_rate(v).await
                } else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Invalid rate value (expected integer)",
                    )]));
                }
            }
            "mode" => {
                if let Some(v) = parse_value(&args.value) {
                    self.state.hqplayer.set_mode(v).await
                } else {
                    return Ok(CallToolResult::error(vec![Content::text(
                        "Invalid mode value (expected integer)",
                    )]));
                }
            }
            _ => {
                return Ok(CallToolResult::error(vec![Content::text(format!(
                    "Unknown setting: {}. Valid settings: mode, samplerate, filter1x, filterNx, shaper, dither",
                    args.setting
                ))]));
            }
        };

        match result {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Set {} to {}",
                args.setting, args.value
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Failed to set {}: {}",
                args.setting, e
            ))])),
        }
    }
}

impl HifiMcpServer {
    // Helper method for volume control (not a tool, just internal)
    async fn set_volume(
        &self,
        zone_id: &str,
        value: f64,
        relative: bool,
    ) -> Result<CallToolResult, McpError> {
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
            return Ok(CallToolResult::error(vec![Content::text(
                "Volume control not supported for this zone type",
            )]));
        };

        match result {
            Ok(()) => Ok(CallToolResult::success(vec![Content::text(format!(
                "Volume {}",
                if relative { "adjusted" } else { "set" }
            ))])),
            Err(e) => Ok(CallToolResult::error(vec![Content::text(format!(
                "Volume error: {}",
                e
            ))])),
        }
    }
}

#[tool_handler]
impl ServerHandler for HifiMcpServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::LATEST,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation::from_build_env(),
            instructions: Some(
                "Unified Hi-Fi Control MCP Server - Control Your Music System\n\n\
                Use hifi_zones to list available zones, hifi_now_playing to see what's playing, \
                hifi_control for playback control, hifi_search to find music, and hifi_play to play it."
                    .to_string(),
            ),
        }
    }
}

/// Create the MCP service for mounting on the axum router
///
/// Returns a service that can be mounted with `router.nest_service("/mcp", mcp_service)`
pub fn create_mcp_service(state: AppState) -> StreamableHttpService {
    use rmcp::transport::streamable_http_server::{
        session::local::LocalSessionManager, StreamableHttpServerConfig,
        StreamableHttpService as SHS,
    };

    let state_clone = state.clone();
    SHS::new(
        move || Ok(HifiMcpServer::new(state_clone.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default(),
    )
}

/// Type alias for the MCP service
pub type StreamableHttpService =
    rmcp::transport::streamable_http_server::StreamableHttpService<HifiMcpServer>;
