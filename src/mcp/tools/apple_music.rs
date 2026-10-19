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

/// Companion-owned read-before-write state. Keep object fields intact when
/// forwarding: revision and ownership details belong to the companion protocol.
///
/// The SDK derive maps `serde_json::Value` to the invalid type "unknown".
/// A named object with an explicit schema keeps the entire tool catalog usable
/// by clients such as Gemini, even when they are calling an unrelated tool.
#[derive(Debug, Deserialize, Serialize)]
#[serde(transparent)]
pub struct AppleMusicPrecondition(pub serde_json::Map<String, serde_json::Value>);

impl AppleMusicPrecondition {
    pub fn json_schema() -> serde_json::Map<String, serde_json::Value> {
        json!({
            "type": "object",
            "properties": {
                "playlist_version": {
                    "type": "integer",
                    "description": "Playlist revision returned by a prior companion read."
                }
            },
            "additionalProperties": true
        })
        .as_object()
        .expect("precondition schema is an object")
        .clone()
    }
}

/// Content reads and account mutations are scoped to one Apple execution
/// owner. Catalog search is the sole provider-global operation; every other
/// provider request must name the companion whose account/session is used.
fn apple_music_action_requires_owner(action: &str) -> bool {
    matches!(
        action,
        "library"
            | "playlists"
            | "playlist_tracks"
            | "recent"
            | "recommendations"
            | "favorites"
            | "playlist_create"
            | "playlist_update"
            | "playlist_add"
            | "playlist_remove"
            | "favorite_add"
            | "favorite_remove"
    )
}

#[mcp_tool(
    name = "hifi_apple_music",
    description = "Apple Music content via a paired companion; authorization stays on-device. For exact playback use hifi_search/hifi_play_ref. Feedback must be explicit; never infer dislike from skips."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiAppleMusicTool {
    /// catalog_search, library, playlists, playlist_tracks, recent, recommendations, favorites, queue_plan, queue_append, play_next, context, clear_feedback, playlist_create, playlist_update, playlist_add, playlist_remove, favorite_add, favorite_remove, feedback.
    pub action: String,
    /// Content/playlist ID; fallback ref for feedback/play_next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Search text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub query: Option<String>,
    /// Content URI; opaque search ref for feedback/play_next.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
    /// applemusic:<owner>; required except catalog_search.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
    /// Playlist name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Playlist description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Explicit user confirmation; true required for account mutations, feedback, clear_feedback.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirm: Option<bool>,
    /// Required retry key for companion account mutations; reuse on retry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    /// Companion revision/ownership object from a prior read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub precondition: Option<AppleMusicPrecondition>,
    /// Maximum results.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// Zero-based result offset.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub offset: Option<u32>,
    /// Ordered opaque search refs for queue_plan/queue_append.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Vec<String>>,
    /// Explicit favorite, unfavorite, rating, skip, more_like_this, less_like_this.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signal: Option<String>,
    /// 1-5; required for signal=rating.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rating: Option<u8>,
    /// User-provided feedback reason.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Feedback source: user (default and only accepted value).
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
        "queue_append",
        "play_next",
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

    if apple_music_action_requires_owner(&args.action) {
        let envelope = if mutation {
            Envelope::write("hifi_apple_music", &args.action)
        } else {
            Envelope::read("hifi_apple_music", &args.action)
        };
        let Some(zone_id) = args.zone_id.as_deref() else {
            return envelope.refused(
                format!(
                    "{} requires an applemusic execution-owner zone_id.",
                    args.action
                ),
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "Choose the named Apple Music companion whose account/session should be used.",
                ),
            );
        };
        if !crate::bus::is_applemusic_zone_id(zone_id) {
            return envelope.refused(
                format!("{} can target only an applemusic zone.", args.action),
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "AirPlay routes are destinations, not Apple Music execution-owner zones.",
                ),
            );
        }
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

    // Feedback is UHC-owned adaptation input, not a prerequisite for a
    // provider-side MusicKit mutation. Persist the explicit user signal first
    // so an unavailable companion or unsupported provider operation cannot
    // silently discard the learning signal.
    let persisted_feedback = if let Some(record) = feedback_record {
        match state.apple_feedback.record(record).await {
            Ok(record) => Some(record),
            Err(error) => {
                return Envelope::write("hifi_apple_music", "feedback").failed(format!(
                    "Apple Music feedback could not be persisted: {error}"
                ))
            }
        }
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
        let mut companion_items = Vec::with_capacity(items.len());
        for token in items {
            let Some(crate::mcp::refs::RefTarget::AppleMusic {
                title,
                companion_id,
                handle,
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
            companion_items.push(handle);
        }
        let params = json!({
            "zone_id": zone_id,
            "items": companion_items,
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

    if matches!(args.action.as_str(), "queue_append" | "play_next") {
        let Some(zone_id) = args.zone_id.as_deref() else {
            return Envelope::write("hifi_apple_music", &args.action).refused(
                format!(
                    "{} requires an applemusic execution-owner zone_id.",
                    args.action
                ),
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "Choose the named Apple Music companion that owns playback.",
                ),
            );
        };
        if !crate::bus::is_applemusic_zone_id(zone_id) {
            return Envelope::write("hifi_apple_music", &args.action).refused(
                format!("{} can target only an applemusic zone.", args.action),
                crate::mcp::envelope::Refusal::invalid_parameter(
                    "zone_id",
                    &["applemusic:<companion>"],
                    "AirPlay routes are destinations, not execution-owner zones.",
                ),
            );
        }
        let refs = if args.action == "play_next" {
            args.uri
                .clone()
                .or_else(|| args.id.clone())
                .into_iter()
                .collect()
        } else {
            args.items.clone().unwrap_or_default()
        };
        if refs.is_empty() || (args.action == "play_next" && refs.len() != 1) {
            return Envelope::write("hifi_apple_music", &args.action).refused(
                format!(
                    "{} requires one or more opaque refs returned by hifi_search.",
                    args.action
                ),
                crate::mcp::envelope::Refusal::invalid_parameter(
                    if args.action == "play_next" {
                        "uri"
                    } else {
                        "items"
                    },
                    &["opaque Apple Music refs returned by hifi_search"],
                    "Search first, then use the refs for this execution owner.",
                ),
            );
        }
        let companion = zone_id.strip_prefix("applemusic:").unwrap_or_default();
        let mut plan_items = Vec::with_capacity(refs.len());
        let mut handles = Vec::with_capacity(refs.len());
        for token in &refs {
            let Some(crate::mcp::refs::RefTarget::AppleMusic {
                title,
                companion_id,
                handle,
            }) = state.mcp_refs.resolve(token).await
            else {
                return Envelope::write("hifi_apple_music", &args.action).refused(
                    "The Apple Music ref is unknown, expired, or from another provider.",
                    crate::mcp::envelope::Refusal::UnknownTarget {
                        parameter: if args.action == "play_next" {
                            "uri"
                        } else {
                            "items"
                        },
                        discover_with: "hifi_search",
                        detail: "Search again for a fresh owner-scoped ref.".to_string(),
                    },
                );
            };
            if companion_id != companion {
                return Envelope::write("hifi_apple_music", &args.action).refused(
                    "The Apple Music ref belongs to a different execution owner.",
                    crate::mcp::envelope::Refusal::InvalidParameter {
                        parameter: if args.action == "play_next" {
                            "uri"
                        } else {
                            "items"
                        },
                        accepted: vec![
                            "refs minted for this applemusic:<companion> zone".to_string()
                        ],
                        detail: "Search against the selected execution owner first.".to_string(),
                    },
                );
            }
            plan_items.push(crate::mcp::listening_plan::ListeningPlanItem {
                reference: token.clone(),
                title,
            });
            handles.push(handle);
        }
        let plan = if args.action == "play_next" {
            state
                .listening_plans
                .play_next(zone_id, plan_items[0].clone())
                .await
        } else {
            state.listening_plans.append(zone_id, plan_items).await
        };
        let plan = match plan {
            Ok(plan) => plan,
            Err(error) => {
                return Envelope::write("hifi_apple_music", &args.action).failed(error.to_string())
            }
        };
        let mut accepted = 0usize;
        for handle in handles {
            if let Err(error) = state
                .adapter_registry
                .library_content(
                    "applemusic",
                    "queue_uri",
                    &json!({"zone_id": zone_id, "uri": handle}),
                )
                .await
            {
                return Ok(Envelope::write("hifi_apple_music", &args.action).json_result(&json!({
                    "plan": plan,
                    "provider": {"outcome": "refused", "accepted": accepted, "detail": error.to_string()}
                })));
            }
            accepted += 1;
        }
        return Ok(
            Envelope::write("hifi_apple_music", &args.action).json_result(&json!({
                "plan": plan,
                "provider": {"outcome": "accepted", "accepted": accepted}
            })),
        );
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
        "offset": args.offset,
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
            let value = if retrieval_action(&args.action) {
                let Some(zone_id) = args.zone_id.as_deref() else {
                    return env.failed(
                        "Apple Music retrieval requires an execution-owner zone".to_string(),
                    );
                };
                normalize_retrieval_result(
                    state,
                    &args.action,
                    zone_id,
                    value,
                    args.offset.unwrap_or(0),
                    args.limit.unwrap_or(25),
                )
                .await
            } else {
                value
            };
            if let Some(record) = persisted_feedback {
                Ok(env.json_result(&json!({"companion": value, "feedback": record})))
            } else {
                Ok(env.json_result(&value))
            }
        }
        Err(error) if persisted_feedback.is_some() => Ok(env.json_result(&json!({
            "feedback": persisted_feedback,
            "provider": apple_content_failure_payload(&error).unwrap_or_else(|| json!({
                "outcome": "refused",
                "detail": error.to_string()
            }))
        }))),
        Err(error) => {
            if let Some(provider) = apple_content_failure_payload(&error) {
                Ok(env.json_result(&json!({"provider": provider})))
            } else {
                env.failed(format!("Apple Music {} failed: {}", args.action, error))
            }
        }
    }
}

fn apple_content_failure_payload(error: &anyhow::Error) -> Option<serde_json::Value> {
    let failure = error.downcast_ref::<crate::api::apple_bridge::AppleContentFailure>()?;
    Some(json!({
        "outcome": failure.outcome,
        "error": {
            "code": failure.code,
            "message": failure.message,
            "retryable": failure.retryable,
        }
    }))
}

fn retrieval_action(action: &str) -> bool {
    matches!(
        action,
        "library" | "playlists" | "playlist_tracks" | "recent" | "recommendations" | "favorites"
    )
}

fn retrieval_source_kind(action: &str) -> &'static str {
    match action {
        "library" | "favorites" => "library",
        "playlists" | "playlist_tracks" => "playlist",
        "recent" => "recent",
        "recommendations" => "recommendation",
        _ => "catalog",
    }
}

/// Convert companion-local retrieval arrays into bounded UHC content pages.
///
/// Song-like entries expose `uri` only inside the bridge today. Those handles
/// are minted into the server ref table before being returned, so the same
/// owner-scoped ref can be passed to `hifi_play_ref` or queue operations. A
/// playlist summary's existing `ref` remains a companion-local playlist handle
/// because playlist traversal is dispatched through `hifi_apple_music`, not
/// `hifi_play_ref`.
async fn normalize_retrieval_result(
    state: &AppState,
    action: &str,
    zone_id: &str,
    value: serde_json::Value,
    offset: u32,
    limit: u32,
) -> serde_json::Value {
    let source_kind = retrieval_source_kind(action);
    let Some(items) = value.as_array() else {
        return value;
    };
    let bounded_limit = limit.clamp(1, 50) as usize;
    let mut normalized = Vec::with_capacity(items.len());
    let companion_id = zone_id.strip_prefix("applemusic:").unwrap_or_default();
    for item in items.iter().take(bounded_limit) {
        let Some(mut object) = item.as_object().cloned() else {
            normalized.push(item.clone());
            continue;
        };
        object.insert("provider".to_string(), json!("applemusic"));
        object.insert("source_kind".to_string(), json!(source_kind));
        if let Some(handle) = object
            .remove("uri")
            .and_then(|uri| uri.as_str().map(str::to_owned))
        {
            let title = object
                .get("title")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("Apple Music item")
                .to_string();
            let reference = state
                .mcp_refs
                .mint(crate::mcp::refs::RefTarget::AppleMusic {
                    companion_id: companion_id.to_string(),
                    handle,
                    title,
                })
                .await;
            object.insert("ref".to_string(), json!(reference));
        }
        normalized.push(serde_json::Value::Object(object));
    }
    let count = normalized.len();
    let has_more = count == bounded_limit;
    json!({
        "items": normalized,
        "source_kind": source_kind,
        "offset": offset,
        "limit": bounded_limit,
        "has_more": has_more,
        "next_offset": has_more.then_some(offset.saturating_add(count as u32)),
    })
}

#[cfg(test)]
mod retrieval_tests {
    use super::*;

    #[test]
    fn retrieval_actions_have_stable_source_kinds() {
        assert_eq!(retrieval_source_kind("library"), "library");
        assert_eq!(retrieval_source_kind("playlist_tracks"), "playlist");
        assert_eq!(retrieval_source_kind("recent"), "recent");
        assert_eq!(retrieval_source_kind("recommendations"), "recommendation");
    }

    #[test]
    fn retrieval_action_vocabulary_is_closed() {
        assert!(retrieval_action("library"));
        assert!(retrieval_action("playlist_tracks"));
        assert!(!retrieval_action("catalog_search"));
        assert!(!retrieval_action("playlist_add"));
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn typed_content_failures_are_projected_without_losing_provider_outcome() {
        let error = anyhow::Error::new(crate::api::apple_bridge::AppleContentFailure {
            outcome: "unauthorized".to_string(),
            code: "authorization_needed".to_string(),
            message: "Authorize Apple Music on the companion.".to_string(),
            retryable: true,
        });
        let payload = super::apple_content_failure_payload(&error).expect("typed failure");
        assert_eq!(payload["outcome"], "unauthorized");
        assert_eq!(payload["error"]["code"], "authorization_needed");
        assert_eq!(payload["error"]["retryable"], true);
    }

    #[test]
    fn owner_scoped_library_actions_require_a_companion_zone() {
        for action in [
            "library",
            "playlists",
            "playlist_tracks",
            "recent",
            "recommendations",
            "favorites",
            "playlist_create",
            "playlist_update",
            "playlist_add",
            "playlist_remove",
            "favorite_add",
            "favorite_remove",
        ] {
            assert!(
                super::apple_music_action_requires_owner(action),
                "{action} must be scoped to a named Apple Music companion"
            );
        }
        assert!(!super::apple_music_action_requires_owner("catalog_search"));
    }
}
