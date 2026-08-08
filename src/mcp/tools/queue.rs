//! Provider queue reads. Queue mutation is intentionally exposed through
//! `hifi_play action=queue`; this tool keeps readback separate and truthful.

use crate::api::AppState;
use crate::mcp::capabilities::{support, Capability, Support};
use crate::mcp::envelope::{Envelope, Scope};
use crate::mcp::routing::{LibraryRoute, ZoneTarget};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

#[mcp_tool(
    name = "hifi_queue",
    description = "Read the current playback queue for a zone. Spotify returns its account playback queue. Music Assistant returns the active player queue and its first 100 items. Queue add is hifi_play action='queue'. Queue edit, remove, reorder, and clear are not claimed unless a provider exposes them."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiQueueTool {
    /// Zone ID from hifi_zones.
    pub zone_id: String,
}

pub async fn handle_queue(
    state: &AppState,
    args: HifiQueueTool,
) -> Result<CallToolResult, CallToolError> {
    let target = ZoneTarget::classify(&args.zone_id);
    let env = Envelope::read("hifi_queue", "queue_read")
        .param("zone_id", &*args.zone_id)
        .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
    if !matches!(support(target, Capability::QueueRead), Support::Supported) {
        return env.failed(format!(
            "Queue read is not available for {} zones",
            target.label()
        ));
    }
    match target.for_library() {
        LibraryRoute::Spotify => match state
            .adapter_registry
            .read_library_queue("spotify", &args.zone_id)
            .await
        {
            Ok(value) => Ok(env.json_result(&value)),
            Err(e) => env.failed(format!("Queue error: {e}")),
        },
        LibraryRoute::AppleMusic => match state
            .adapter_registry
            .read_library_queue("applemusic", &args.zone_id)
            .await
        {
            Ok(value) => Ok(env.json_result(&value)),
            Err(e) => env.failed(format!("Queue error: {e}")),
        },
        LibraryRoute::MusicAssistant => match state
            .adapter_registry
            .read_library_queue("musicassistant", &args.zone_id)
            .await
        {
            Ok(value) => Ok(env.json_result(&value)),
            Err(e) => env.failed(format!("Queue error: {e}")),
        },
        _ => env.failed("Queue read is not implemented for this provider"),
    }
}
