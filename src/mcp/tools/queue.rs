//! Provider queue reads and explicitly-scoped mutations.

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
    description = "Read or edit a playback queue. action='read' is the default. Music Assistant supports jump, reorder, remove, clear, and transfer against its active queue; each mutation returns a fresh queue readback. transfer moves the active queue from zone_id to target_zone_id (both must be Music Assistant zones). Queue add is hifi_play action='queue'."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiQueueTool {
    /// Zone ID from hifi_zones.
    pub zone_id: String,
    /// read (default), jump, reorder, remove, clear, or transfer.
    #[serde(default = "default_action")]
    pub action: String,
    /// Queue item id for jump, reorder, or remove.
    #[serde(default)]
    pub item_id: Option<String>,
    /// Target zero-based position for reorder.
    #[serde(default)]
    pub position: Option<i64>,
    /// Destination zone ID for transfer. Must belong to the same provider as zone_id.
    #[serde(default)]
    pub target_zone_id: Option<String>,
}

fn default_action() -> String {
    "read".to_string()
}

pub async fn handle_queue(
    state: &AppState,
    args: HifiQueueTool,
) -> Result<CallToolResult, CallToolError> {
    let target = ZoneTarget::classify(&args.zone_id);
    let operation = match args.action.as_str() {
        "read" => "queue_read",
        "jump" => "queue_jump",
        "reorder" => "queue_reorder",
        "remove" => "queue_remove",
        "clear" => "queue_clear",
        "transfer" => "queue_transfer",
        _ => {
            return Envelope::write("hifi_queue", "queue").refused(
                "Unknown queue action.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "action",
                    &["read", "jump", "reorder", "remove", "clear", "transfer"],
                    "Choose a documented queue action.",
                ),
            )
        }
    };
    let mut env = if operation == "queue_read" {
        Envelope::read("hifi_queue", operation)
    } else {
        Envelope::write("hifi_queue", operation)
    }
    .param("zone_id", &*args.zone_id)
    .param("action", &*args.action)
    .scope(Scope::for_zone(state, &args.zone_id, target.provider()).await);
    if let Some(target_zone_id) = args.target_zone_id.as_deref() {
        env = env.param("target_zone_id", target_zone_id);
    }
    let capability = match operation {
        "queue_read" => Capability::QueueRead,
        "queue_jump" => Capability::QueueJump,
        "queue_reorder" => Capability::QueueReorder,
        "queue_remove" => Capability::QueueRemove,
        "queue_transfer" => Capability::QueueTransfer,
        _ => Capability::QueueClear,
    };
    if !matches!(support(target, capability), Support::Supported) {
        return env.failed(format!(
            "Queue read is not available for {} zones",
            target.label()
        ));
    }
    if operation == "queue_transfer" {
        let target_zone_id = match args.target_zone_id.as_deref() {
            Some(id) if !id.trim().is_empty() => id,
            _ => {
                return env.refused(
                    "transfer requires target_zone_id.",
                    crate::mcp::envelope::Refusal::invalid_parameter(
                        "target_zone_id",
                        &["a zone_id from hifi_zones"],
                        "Provide the destination zone's zone_id.",
                    ),
                )
            }
        };
        let target_target = ZoneTarget::classify(target_zone_id);
        if !matches!(target.for_library(), LibraryRoute::MusicAssistant)
            || !matches!(target_target.for_library(), LibraryRoute::MusicAssistant)
        {
            return env.refused(
                "transfer requires both zones to be Music Assistant zones.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "target_zone_id",
                    &["a Music Assistant zone_id from hifi_zones"],
                    "Queue transfer is only implemented between two Music Assistant zones.",
                ),
            );
        }
        return match state
            .adapter_registry
            .library_content(
                "musicassistant",
                operation,
                &serde_json::json!({"zone_id": args.zone_id, "target_zone_id": target_zone_id}),
            )
            .await
        {
            Ok(value) => Ok(env.json_result(&value)),
            Err(e) => env.failed(format!("Queue error: {e}")),
        };
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
        LibraryRoute::MusicAssistant => match if operation == "queue_read" {
            state
                .adapter_registry
                .read_library_queue("musicassistant", &args.zone_id)
                .await
        } else {
            let mut params = serde_json::json!({"zone_id": args.zone_id});
            if let Some(item_id) = args.item_id {
                params["item_id"] = item_id.into();
            }
            if let Some(position) = args.position {
                params["position"] = position.into();
            }
            state
                .adapter_registry
                .library_content("musicassistant", operation, &params)
                .await
        } {
            Ok(value) => Ok(env.json_result(&value)),
            Err(e) => env.failed(format!("Queue error: {e}")),
        },
        _ => env.failed("Queue read is not implemented for this provider"),
    }
}
