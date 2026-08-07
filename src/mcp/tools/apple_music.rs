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
    description = "Apple Music catalog, library, playlist, feedback, and bounded adaptation context through a paired native companion. Actions include catalog_search, library, playlists, playlist_tracks, recent, recommendations, favorites, queue_plan, context, clear_feedback, playlist_create, playlist_update, playlist_add, playlist_remove, favorite_add/remove, and feedback. Feedback accepts explicit user signals only; skips and manual changes are not inferred as dislike. Apple authorization stays on the companion; operations are limited to documented MusicKit capabilities and may be refused when the companion or account cannot perform them. Use hifi_search/hifi_play for exact content selection and playback."
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
    /// Stable retry key required for Apple Music account mutations.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Read-before-write revision or ownership precondition.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precondition: Option<serde_json::Value>,
    /// Maximum number of entries to return.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Ordered short-lived opaque refs for queue_plan.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<String>>,
    /// Explicit feedback signal: favorite, unfavorite, rating, skip,
    /// more_like_this, or less_like_this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// Rating value from 1 through 5 when signal is rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    /// Optional human-provided reason for explicit feedback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Provenance of the signal; feedback_record accepts user only for now.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
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
        "clear_feedback",
        "context",
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
            | "clear_feedback"
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

    if args.action == "context" {
        let Some(zone_id) = args.zone_id.as_deref() else {
            return Envelope::read("hifi_apple_music", "context").refused(
                "context requires an applemusic execution-owner zone_id.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "Choose the named Apple Music companion whose context is needed.",
                ),
            );
        };
        if !crate::bus::is_applemusic_zone_id(zone_id) {
            return Envelope::read("hifi_apple_music", "context").refused(
                "context can target only an applemusic zone.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "AirPlay routes are destinations, not execution-owner zones.",
                ),
            );
        }

        let limit = args.limit.unwrap_or(10) as usize;
        let feedback = state.apple_feedback.recent(zone_id, limit).await;
        let plan = state.listening_plans.get(zone_id).await;
        let observed_playback = state
            .aggregator
            .observed_playback_history(zone_id, limit)
            .await;
        return Ok(Envelope::read("hifi_apple_music", "context")
            .param("action", "context")
            .param("zone_id", zone_id)
            .json_result(&json!({
                "zone_id": zone_id,
                "feedback": feedback,
                "listening_plan": plan,
                "observed_playback": observed_playback,
            })));
    }

    if args.action == "clear_feedback" {
        let Some(zone_id) = args.zone_id.as_deref() else {
            return Envelope::write("hifi_apple_music", "clear_feedback").refused(
                "clear_feedback requires an applemusic execution-owner zone_id.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "Choose the companion whose bounded feedback history should be deleted.",
                ),
            );
        };
        if !crate::bus::is_applemusic_zone_id(zone_id) {
            return Envelope::write("hifi_apple_music", "clear_feedback").refused(
                "clear_feedback can target only an applemusic zone.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "AirPlay routes are destinations, not feedback owners.",
                ),
            );
        }
        let env = Envelope::write("hifi_apple_music", "clear_feedback")
            .param("action", "clear_feedback")
            .param("zone_id", zone_id);
        return match state.apple_feedback.clear_zone(zone_id).await {
            Ok(()) => Ok(env.json_result(&json!({"cleared": true, "zone_id": zone_id}))),
            Err(error) => env.failed(format!(
                "Apple Music feedback could not be deleted: {error}"
            )),
        };
    }

    let feedback_record = if args.action == "feedback" {
        let Some(zone_id) = args.zone_id.as_deref() else {
            return Envelope::write("hifi_apple_music", "feedback").refused(
                "feedback requires an applemusic execution-owner zone_id.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "Choose the named Apple Music companion receiving the signal.",
                ),
            );
        };
        let Some(signal) = args.signal.as_deref() else {
            return Envelope::write("hifi_apple_music", "feedback").refused(
                "feedback requires an explicit signal.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "signal",
                    &[
                        "favorite",
                        "unfavorite",
                        "rating",
                        "skip",
                        "more_like_this",
                        "less_like_this",
                    ],
                    "Do not infer dislike from an absent signal or a skip.",
                ),
            );
        };
        let signal = match signal {
            "favorite" => crate::mcp::feedback::FeedbackSignal::Favorite,
            "unfavorite" => crate::mcp::feedback::FeedbackSignal::Unfavorite,
            "rating" => crate::mcp::feedback::FeedbackSignal::Rating,
            "skip" => crate::mcp::feedback::FeedbackSignal::Skip,
            "more_like_this" => crate::mcp::feedback::FeedbackSignal::MoreLikeThis,
            "less_like_this" => crate::mcp::feedback::FeedbackSignal::LessLikeThis,
            _ => {
                return Envelope::write("hifi_apple_music", "feedback").refused(
                    "feedback signal is not recognized.",
                    crate::mcp::envelope::Refusal::invalid_parameter(
                        "signal",
                        &[
                            "favorite",
                            "unfavorite",
                            "rating",
                            "skip",
                            "more_like_this",
                            "less_like_this",
                        ],
                        "Use an explicit signal; skips are observations, not inferred dislike.",
                    ),
                )
            }
        };
        if args.source.as_deref().unwrap_or("user") != "user" {
            return Envelope::write("hifi_apple_music", "feedback").refused(
                "Only explicit user feedback can be recorded by this surface.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "source",
                    &["user"],
                    "Observed skips and manual changes will be modeled separately from explicit feedback.",
                ),
            );
        }
        Some(crate::mcp::feedback::FeedbackRecord {
            event_id: crate::mcp::feedback::next_event_id(),
            zone_id: zone_id.to_string(),
            reference: args
                .uri
                .clone()
                .or_else(|| args.id.clone())
                .unwrap_or_default(),
            signal,
            source: crate::mcp::feedback::FeedbackSource::User,
            rating: args.rating,
            reason: args.reason.clone(),
            explicit: true,
            confidence: crate::mcp::feedback::FeedbackConfidence::Explicit,
            recorded_at: crate::mcp::feedback::now_secs(),
        })
    } else {
        None
    };

    if args.action == "queue_plan" {
        let Some(zone_id) = args.zone_id.as_deref() else {
            return Envelope::write("hifi_apple_music", "queue_plan").refused(
                "queue_plan requires an applemusic execution-owner zone_id.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "Choose the named Apple Music companion that owns playback.",
                ),
            );
        };
        if !crate::bus::is_applemusic_zone_id(zone_id) {
            return Envelope::write("hifi_apple_music", "queue_plan").refused(
                "queue_plan can target only an applemusic zone.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "AirPlay routes are destinations, not execution-owner zones.",
                ),
            );
        }
        let Some(items) = args.items.as_ref() else {
            return Envelope::write("hifi_apple_music", "queue_plan").refused(
                "queue_plan requires an ordered items array of opaque Apple Music refs.",
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "items",
                    &["an array of refs returned by hifi_search"],
                    "Search first, then pass the selected refs in the desired order.",
                ),
            );
        };
        let mut plan_items = Vec::with_capacity(items.len());
        for token in items {
            let Some(crate::mcp::refs::RefTarget::AppleMusic {
                title,
                companion_id,
                ..
            }) = state.mcp_refs.resolve(token).await
            else {
                return Envelope::write("hifi_apple_music", "queue_plan").refused(
                    "queue_plan contains an unknown, expired, or cross-provider ref.",
                    crate::mcp::envelope::Refusal::UnknownTarget {
                        parameter: "items",
                        discover_with: "hifi_search",
                        detail:
                            "Search again for fresh Apple Music refs before replacing the plan."
                                .to_string(),
                    },
                );
            };
            let zone_companion = zone_id.strip_prefix("applemusic:").unwrap_or_default();
            if companion_id != zone_companion {
                return Envelope::write("hifi_apple_music", "queue_plan").refused(
                    "queue_plan contains a ref minted for a different Apple Music companion.",
                    crate::mcp::envelope::Refusal::InvalidParameter {
                        parameter: "items",
                        accepted: vec!["refs returned for this applemusic:<companion> zone".to_string()],
                        detail: "Search again against the selected Apple Music execution owner before replacing its plan.".to_string(),
                    },
                );
            }
            plan_items.push(crate::mcp::listening_plan::ListeningPlanItem {
                reference: token.clone(),
                title,
            });
        }
        let params = json!({
            "zone_id": zone_id,
            "items": items,
            "confirm": confirmed,
        });
        let env = Envelope::write("hifi_apple_music", "queue_plan")
            .param("action", "queue_plan")
            .param("zone_id", zone_id);
        // The plan is UHC-owned intent, distinct from the provider's native
        // queue. Persist it first so a companion refusal cannot erase the
        // model/user's requested listening plan. The response reports the two
        // outcomes separately and never claims that the provider accepted it.
        let plan = match state.listening_plans.replace(zone_id, plan_items).await {
            Ok(plan) => plan,
            Err(error) => {
                return env.failed(format!(
                    "Apple Music listening plan could not be persisted: {error}"
                ));
            }
        };
        match state
            .adapter_registry
            .library_content("applemusic", "queue_plan", &params)
            .await
        {
            Ok(value) => {
                return Ok(env.json_result(&json!({
                    "plan": plan,
                    "provider": {"outcome": "accepted", "result": value}
                })))
            }
            Err(error) => {
                return Ok(env.json_result(&json!({
                    "plan": plan,
                    "provider": {
                        "outcome": "refused",
                        "detail": error.to_string()
                    }
                })))
            }
        }
    }

    let params = json!({
        "id": args.id,
        "query": args.query,
        "uri": args.uri,
        "zone_id": args.zone_id,
        "name": args.name,
        "description": args.description,
        "confirm": confirmed,
        "idempotency_key": args.idempotency_key,
        "precondition": args.precondition,
        "limit": args.limit,
        "items": args.items,
        "signal": args.signal,
        "rating": args.rating,
        "reason": args.reason,
        "source": args.source,
    });
    let env =
        Envelope::write("hifi_apple_music", "apple_music_content").param("action", &*args.action);
    match state
        .adapter_registry
        .library_content("applemusic", &args.action, &params)
        .await
    {
        Ok(value) => {
            if let Some(record) = feedback_record {
                match state.apple_feedback.record(record).await {
                    Ok(record) => {
                        Ok(env.json_result(&json!({"companion": value, "feedback": record})))
                    }
                    Err(error) => env.failed(format!(
                        "Apple Music feedback was applied but could not be persisted: {error}"
                    )),
                }
            } else {
                Ok(env.json_result(&value))
            }
        }
        Err(e) => env.failed(format!("Apple Music {} failed: {}", args.action, e)),
    }
}
