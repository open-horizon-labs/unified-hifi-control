//! The MCP `ServerHandler`: advertise tools, and dispatch calls to them.
//!
//! Dispatch only. Every arm delegates to a handler in
//! [`crate::mcp::tools`]; no tool behavior lives here. #397 extends this impl
//! with the resource methods.

use crate::api::{load_app_settings, AppState};
use crate::mcp::envelope::{Envelope, Refusal};
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
        // Argument parsing happens before any tool runs, and its failures are the
        // commonest `invalid` a real client produces — a required parameter left
        // out. The SDK turns them into `content: [text], isError: true`, which a
        // client cannot tell apart from a tool's own result, so #395's envelope
        // has to reach them too. `requested_tool_name` is captured first because
        // `try_from` consumes `params`.
        let requested = params.name.clone();
        let tool: HifiTools = match HifiTools::try_from(params) {
            Ok(tool) => tool,
            Err(e) => {
                let error = CallToolError::new(e);
                return Ok(match tools::static_name(&requested) {
                    // A known tool with bad arguments: the client can fix this,
                    // and the envelope says which parameter and how.
                    Some(name) => invalid_arguments_envelope(name, &error).errored_result(error),
                    // An unknown tool name has no tool to scope an envelope to,
                    // and inventing one would claim a surface that does not exist.
                    // Deliberately left bare; asserted in the contract tests so
                    // the line is a decision rather than an oversight.
                    None => CallToolResult::with_error(error),
                });
            }
        };
        let state = &self.state;

        match tool {
            HifiTools::HifiZonesTool(_) => tools::zones::handle_zones(state).await,
            HifiTools::HifiNowPlayingTool(args) => {
                tools::zones::handle_now_playing(state, args).await
            }
            HifiTools::HifiControlTool(args) => tools::transport::handle_control(state, args).await,
            HifiTools::HifiSearchTool(args) => tools::library::handle_search(state, args).await,
            HifiTools::HifiPlayTool(args) => tools::library::handle_play(state, args).await,
            HifiTools::HifiStatusTool(_) => tools::status::handle_status(state).await,
            HifiTools::HifiHqplayerStatusTool(_) => tools::hqplayer::handle_status(state).await,
            HifiTools::HifiHqplayerProfilesTool(_) => tools::hqplayer::handle_profiles(state).await,
            HifiTools::HifiHqplayerLoadProfileTool(args) => {
                tools::hqplayer::handle_load_profile(state, args).await
            }
            HifiTools::HifiHqplayerSetPipelineTool(args) => {
                tools::hqplayer::handle_set_pipeline(state, args).await
            }
            HifiTools::HifiCapabilitiesTool(args) => {
                tools::capabilities::handle_capabilities(state, args).await
            }
        }
    }
}

/// The envelope for a call whose arguments did not deserialize.
///
/// The parameter name has to be recovered from the deserializer's message
/// (`"missing field \`zone_id\`"`, `"invalid type: ..."`) because that is all the
/// SDK surfaces — `try_from` returns a `serde` error, not a structured field path.
/// So the parameter is reported when it can be identified against the tool's
/// declared inputs and omitted when it cannot, rather than guessed at.
fn invalid_arguments_envelope(tool: &'static str, error: &CallToolError) -> Envelope {
    let message = error.to_string();
    let parameter = tools::static_param(tool, &message);

    // `write` rather than `read` even for the read-only tools. Unobservable —
    // `refuse` overwrites the outcome and no envelope field records read vs.
    // write — but stated because a future field derived from that distinction
    // would be wrong here, and because a parse failure genuinely is neither.
    let env = Envelope::write(tool, "parse_arguments");
    match parameter {
        Some(parameter) => env.refuse(Refusal::InvalidParameter {
            parameter,
            accepted: vec![format!("see the {tool} inputSchema in tools/list")],
            detail: format!(
                "The arguments for {tool} did not deserialize: {message}. Fix {parameter} \
                 and call again."
            ),
        }),
        None => env.refuse(Refusal::InvalidParameter {
            parameter: "arguments",
            accepted: vec![format!("see the {tool} inputSchema in tools/list")],
            detail: format!(
                "The arguments for {tool} did not deserialize: {message}. UHC could not \
                 identify which parameter from the deserializer's message."
            ),
        }),
    }
}
