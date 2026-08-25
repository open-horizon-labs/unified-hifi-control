//! `hifi_zone_group`: multiroom grouping, generalized across every adapter that
//! implements the `multiroom_status` / `multiroom_set_members` /
//! `multiroom_ungroup` contract (issue #517).
//!
//! # Three providers, one contract
//!
//! Music Assistant, Roon (#509/#515) and LMS (#510/#513) each implement the
//! same three-operation content-library contract
//! (`src/adapters/{musicassistant,roon,lms}.rs`, `impl LibraryAdapter for
//! ...::content`). This module owns the provider-neutral routing that used to
//! be hardwired to Music Assistant alone: `join`/`leave` resolve the owning
//! adapter from the zone id's prefix via [`ZoneTarget`], exactly like every
//! other MCP tool.
//!
//! # The `status` design decision
//!
//! Before #517, `status` took no zone id at all, which implicitly assumed a
//! single grouping-capable provider. With three, that question has two honest
//! answers and this tool supports both explicitly rather than picking one at
//! the cost of the other:
//!
//! - **`zone_id` present**: scoped to that zone's provider, exactly like every
//!   other zone-scoped tool infers its provider. Refused if the zone's prefix
//!   names no multiroom-capable provider.
//! - **`zone_id` absent**: aggregate every provider's groups into one
//!   `groups` list, each entry tagged with its `provider`. A provider whose
//!   call fails (not configured, not connected, ...) is reported in `errors`
//!   rather than failing the whole read — one backend being unreachable must
//!   not hide the other two's groups. `errors` is omitted entirely when every
//!   provider answered.
//!
//! Aggregation is the default (no `zone_id`) because `status` is the one
//! read-only action here and a client asking "what is grouped right now"
//! almost always means "everywhere", not "guess which provider I meant". The
//! explicit `zone_id` form exists for a client that already knows which
//! provider it cares about and does not want to pay for the other two calls
//! (or parse `errors` for a provider it does not use).
//!
//! # Two member-id conventions, tolerated not normalized
//!
//! `multiroom_status`'s `member_zone_ids` mean different things per adapter,
//! and this tool passes them through verbatim rather than pretending they
//! agree:
//!
//! - **Roon** merges outputs into one zone on grouping and retires the
//!   member zones' own ids, so a member is reported (and must be addressed
//!   again) as `roon:<output_id>`, not a zone id `hifi_zones` ever listed
//!   independently once grouped.
//! - **LMS** sync groups are leaderless — Squeezebox sync has no
//!   server-side concept of "leader" — so `leader_zone_id` in an LMS group is
//!   a stable-but-arbitrary UHC convention (the first id LMS's `syncgroups ?`
//!   reports for that group), not an LMS fact.
//!
//! Both are documented on the tool description below so a client does not
//! read either as a UHC bug.
//!
//! # Cross-provider grouping is refused, not attempted
//!
//! `join` and `leave` require every zone id involved (leader plus every
//! member) to classify to the *same* multiroom-capable provider. Mixing
//! providers is refused as an invalid parameter before any adapter is
//! called — there is no protocol that groups a Roon output with an LMS
//! player, so attempting it would only surface as a confusing adapter-side
//! failure instead of a clear client-side one.

use crate::api::AppState;
use crate::mcp::envelope::{Envelope, Refusal, Scope};
use crate::mcp::routing::ZoneTarget;
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Every [`ZoneTarget`] that implements the multiroom contract, in the order
/// `status`'s aggregate reports them.
const MULTIROOM_TARGETS: &[ZoneTarget] = &[
    ZoneTarget::Roon,
    ZoneTarget::Lms,
    ZoneTarget::MusicAssistant,
];

/// The accepted zone-id prefixes, for refusals that must say what a valid
/// input looks like.
const MULTIROOM_PREFIXES: &[&str] = &["roon:", "lms:", "musicassistant:"];

/// Classify `zone_id` and accept it only if its provider implements the
/// multiroom contract.
fn multiroom_target(zone_id: &str) -> Option<ZoneTarget> {
    let target = ZoneTarget::classify(zone_id);
    MULTIROOM_TARGETS.contains(&target).then_some(target)
}

#[mcp_tool(
    name = "hifi_zone_group",
    description = "Inspect or change multiroom zone groups for any provider that supports \
                    synchronised playback (Roon, LMS, Music Assistant). action=status is \
                    read-only: pass zone_id to scope it to one provider, or omit zone_id to \
                    aggregate every provider's groups (a provider that cannot be reached is \
                    reported in `errors` rather than failing the whole read). join and leave \
                    require confirm=true because grouping changes playback topology, and every \
                    zone id involved (leader plus every member) must belong to the same \
                    provider -- cross-provider groups are refused. Two provider-specific member \
                    conventions: Roon retires a grouped zone's own id and reports/accepts its \
                    members as roon:<output_id>; LMS sync groups are leaderless, so \
                    leader_zone_id there is UHC's stable-but-arbitrary pick of the first member \
                    id, not an LMS fact."
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiZoneGroupTool {
    /// status, join, or leave.
    pub action: String,
    /// Scopes action=status to one provider. Omit to aggregate every
    /// multiroom-capable provider's groups.
    #[serde(default)]
    pub zone_id: Option<String>,
    /// Leader zone for join. Must be the same provider as every member_zone_id.
    #[serde(default)]
    pub leader_zone_id: Option<String>,
    /// Member zones to add (join) or remove (leave). Must all share one
    /// provider, matching leader_zone_id for join.
    #[serde(default)]
    pub member_zone_ids: Option<Vec<String>>,
    /// Required true for topology-changing join and leave actions.
    #[serde(default)]
    pub confirm: Option<bool>,
}

pub async fn handle_zone_group(
    state: &AppState,
    args: HifiZoneGroupTool,
) -> Result<CallToolResult, CallToolError> {
    let write = matches!(args.action.as_str(), "join" | "leave");
    let env = if write {
        Envelope::write("hifi_zone_group", "multiroom_sync")
    } else {
        Envelope::read("hifi_zone_group", "multiroom_sync")
    };
    let member_zone_ids = args.member_zone_ids.unwrap_or_default();
    if write && args.confirm != Some(true) {
        return env.refused(
            "Zone grouping requires explicit confirm=true.",
            Refusal::invalid_parameter(
                "confirm",
                &["true"],
                "Joining or leaving zones can change their shared playback topology.",
            ),
        );
    }
    if write && member_zone_ids.is_empty() {
        return env.refused(
            "join and leave require at least one member_zone_id.",
            Refusal::invalid_parameter(
                "member_zone_ids",
                MULTIROOM_PREFIXES,
                "Specify one or more zones to change, all from the same provider.",
            ),
        );
    }
    match args.action.as_str() {
        "status" => handle_status(state, env, args.zone_id).await,
        "join" => handle_join(state, env, args.leader_zone_id, member_zone_ids).await,
        "leave" => handle_leave(state, env, member_zone_ids).await,
        _ => env.refused(
            "Unknown zone group action.",
            Refusal::invalid_parameter(
                "action",
                &["status", "join", "leave"],
                "Choose a documented zone group action.",
            ),
        ),
    }
}

async fn handle_join(
    state: &AppState,
    env: Envelope,
    leader_zone_id: Option<String>,
    member_zone_ids: Vec<String>,
) -> Result<CallToolResult, CallToolError> {
    let leader_zone_id = match leader_zone_id.filter(|id| !id.is_empty()) {
        Some(id) => id,
        None => {
            return env.refused(
                "join requires a leader_zone_id.",
                Refusal::invalid_parameter(
                    "leader_zone_id",
                    MULTIROOM_PREFIXES,
                    "Groups are addressed by the leader's zone id.",
                ),
            )
        }
    };
    let leader_target =
        match multiroom_target(&leader_zone_id) {
            Some(target) => target,
            None => return env.refused(
                "join requires a leader_zone_id from a provider that supports multiroom \
                 grouping.",
                Refusal::invalid_parameter(
                    "leader_zone_id",
                    MULTIROOM_PREFIXES,
                    "Groups are owned by Roon, LMS or Music Assistant; other zone types cannot \
                     lead a group.",
                ),
            ),
        };
    if member_zone_ids
        .iter()
        .any(|id| ZoneTarget::classify(id) != leader_target)
    {
        return env.refused(
            format!(
                "Every member_zone_id must be a {} zone, matching the leader.",
                leader_target.label()
            ),
            Refusal::invalid_parameter(
                "member_zone_ids",
                &[leader_target.prefix().unwrap_or_default()],
                "Cross-provider zone groups are not supported.",
            ),
        );
    }
    let params =
        serde_json::json!({"leader_zone_id": leader_zone_id, "member_zone_ids": member_zone_ids});
    match state
        .adapter_registry
        .library_content(leader_target.label(), "multiroom_set_members", &params)
        .await
    {
        Ok(value) => Ok(env
            .scope(Scope::provider_only(leader_target.provider()))
            .json_result(&value)),
        Err(error) => env.failed(format!("Zone group error: {error}")),
    }
}

async fn handle_leave(
    state: &AppState,
    env: Envelope,
    member_zone_ids: Vec<String>,
) -> Result<CallToolResult, CallToolError> {
    let first_target =
        match member_zone_ids.first().and_then(|id| multiroom_target(id)) {
            Some(target) => target,
            None => return env.refused(
                "leave requires member_zone_ids from a provider that supports multiroom \
                 grouping.",
                Refusal::invalid_parameter(
                    "member_zone_ids",
                    MULTIROOM_PREFIXES,
                    "Groups are owned by Roon, LMS or Music Assistant; other zone types cannot \
                     be group members.",
                ),
            ),
        };
    if member_zone_ids
        .iter()
        .any(|id| ZoneTarget::classify(id) != first_target)
    {
        return env.refused(
            format!(
                "Every member_zone_id must be a {} zone.",
                first_target.label()
            ),
            Refusal::invalid_parameter(
                "member_zone_ids",
                &[first_target.prefix().unwrap_or_default()],
                "Cross-provider zone groups are not supported.",
            ),
        );
    }
    let params = serde_json::json!({"member_zone_ids": member_zone_ids});
    match state
        .adapter_registry
        .library_content(first_target.label(), "multiroom_ungroup", &params)
        .await
    {
        Ok(value) => Ok(env
            .scope(Scope::provider_only(first_target.provider()))
            .json_result(&value)),
        Err(error) => env.failed(format!("Zone group error: {error}")),
    }
}

/// One provider's `multiroom_status` call failing during aggregation. Reported
/// alongside the groups every other provider did answer, rather than failing
/// the whole read for one unreachable backend.
#[derive(Debug, Serialize)]
struct ProviderError {
    provider: &'static str,
    detail: String,
}

/// The aggregate payload for `status` with no `zone_id`: every reachable
/// provider's groups, each tagged with `provider`, plus the providers that
/// could not be reached.
#[derive(Debug, Serialize)]
struct AggregateMultiroomStatus {
    groups: Vec<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    errors: Vec<ProviderError>,
}

async fn handle_status(
    state: &AppState,
    env: Envelope,
    zone_id: Option<String>,
) -> Result<CallToolResult, CallToolError> {
    match zone_id.filter(|id| !id.is_empty()) {
        Some(zone_id) => {
            let target = match multiroom_target(&zone_id) {
                Some(target) => target,
                None => {
                    return env.refused(
                        "zone_id must be from a provider that supports multiroom grouping.",
                        Refusal::invalid_parameter(
                            "zone_id",
                            MULTIROOM_PREFIXES,
                            "Only Roon, LMS and Music Assistant zones report multiroom status.",
                        ),
                    )
                }
            };
            match state
                .adapter_registry
                .library_content(target.label(), "multiroom_status", &serde_json::json!({}))
                .await
            {
                Ok(value) => Ok(env
                    .scope(Scope::provider_only(target.provider()))
                    .json_result(&value)),
                Err(error) => env.failed(format!("Zone group error: {error}")),
            }
        }
        None => {
            let mut groups = Vec::new();
            let mut errors = Vec::new();
            for target in MULTIROOM_TARGETS {
                match state
                    .adapter_registry
                    .library_content(target.label(), "multiroom_status", &serde_json::json!({}))
                    .await
                {
                    Ok(value) => {
                        let provider_groups = value
                            .get("groups")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default();
                        for mut group in provider_groups {
                            if let Value::Object(map) = &mut group {
                                map.insert(
                                    "provider".to_string(),
                                    Value::String(target.label().to_string()),
                                );
                            }
                            groups.push(group);
                        }
                    }
                    Err(error) => errors.push(ProviderError {
                        provider: target.label(),
                        detail: error.to_string(),
                    }),
                }
            }
            Ok(env.json_result(&AggregateMultiroomStatus { groups, errors }))
        }
    }
}
