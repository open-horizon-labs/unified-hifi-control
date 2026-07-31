//! The MCP `ServerHandler`: advertise tools, and dispatch calls to them.
//!
//! Dispatch only. Every arm delegates to a handler in
//! [`crate::mcp::tools`]; no tool behavior lives here. #397 extends this impl
//! with the resource methods.

use crate::api::{load_app_settings, AppState};
use crate::mcp::tools::{self, HifiTools};
use async_trait::async_trait;
use rust_mcp_sdk::{
    mcp_server::ServerHandler,
    schema::{
        schema_utils::CallToolError, CallToolRequestParams, CallToolResult, ListToolsResult,
        PaginatedRequestParams, RpcError,
    },
    McpServer,
};
use std::sync::Arc;

/// MCP server handler with access to app state.
pub struct HifiMcpHandler {
    state: AppState,
}

impl HifiMcpHandler {
    pub fn new(state: AppState) -> Self {
        Self { state }
    }
}

#[async_trait]
impl ServerHandler for HifiMcpHandler {
    async fn handle_list_tools_request(
        &self,
        _params: Option<PaginatedRequestParams>,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<ListToolsResult, RpcError> {
        // Read settings per request: the operator can toggle the HQPlayer
        // adapter while the server is running, and the advertised tool list has
        // to follow.
        let settings = load_app_settings();

        Ok(ListToolsResult {
            meta: None,
            next_cursor: None,
            tools: tools::list_tools(settings.adapters.hqplayer),
        })
    }

    async fn handle_call_tool_request(
        &self,
        params: CallToolRequestParams,
        _runtime: Arc<dyn McpServer>,
    ) -> Result<CallToolResult, CallToolError> {
        let tool: HifiTools = HifiTools::try_from(params).map_err(CallToolError::new)?;
        let state = &self.state;

        match tool {
            HifiTools::HifiZonesTool(_) => tools::zones::handle_zones(state).await,
            HifiTools::HifiNowPlayingTool(args) => {
                tools::zones::handle_now_playing(state, args).await
            }
            HifiTools::HifiControlTool(args) => {
                tools::transport::handle_control(state, args).await
            }
            HifiTools::HifiSearchTool(args) => tools::library::handle_search(state, args).await,
            HifiTools::HifiPlayTool(args) => tools::library::handle_play(state, args).await,
            HifiTools::HifiStatusTool(_) => tools::status::handle_status(state).await,
            HifiTools::HifiHqplayerStatusTool(_) => tools::hqplayer::handle_status(state).await,
            HifiTools::HifiHqplayerProfilesTool(_) => {
                tools::hqplayer::handle_profiles(state).await
            }
            HifiTools::HifiHqplayerLoadProfileTool(args) => {
                tools::hqplayer::handle_load_profile(state, args).await
            }
            HifiTools::HifiHqplayerSetPipelineTool(args) => {
                tools::hqplayer::handle_set_pipeline(state, args).await
            }
        }
    }
}
