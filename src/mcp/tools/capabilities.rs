//! Per-zone capability discovery (issue #398).
//!
//! One read-only tool so a client can ask what a zone supports **before** trying
//! it, instead of learning limits by failing. Every value comes from
//! [`crate::mcp::capabilities`], which derives `supported` from the routing layer
//! and cannot hand-write it.
//!
//! # Why a new tool rather than fields on `hifi_zones`
//!
//! `hifi_zones`'s human-readable text *is* its JSON payload, and
//! `tests/fixtures/mcp_tool_text.json` pins that text byte-for-byte because epic
//! #392 is additive-only. Adding a field to `McpZone` would change it. A new tool
//! adds one key to `tools/list` and leaves every existing key identical — the only
//! shape that satisfies the constraint. It is also the right shape on its own
//! merits: capability data is pre-flight, and every zone listing would otherwise
//! pay for it.
//!
//! # Why the payload has two sections
//!
//! Capability state is a function of *provider*, not of zone, so inlining ~18
//! identical entries per zone would bloat the answer and imply a per-zone
//! precision UHC does not have. `zones` names each zone's provider; `providers`
//! carries each provider's table. A single-zone query returns one of each, so the
//! join is trivial in the case a model uses most.

use crate::api::AppState;
use crate::mcp::capabilities::{
    provider_capabilities, McpCapabilityReport, McpCapabilityZone, McpProviderCapabilities,
};
use crate::mcp::envelope::{Envelope, Scope};
use crate::mcp::routing::{unplaceable_zone_refusal, unplaceable_zone_text, ZoneTarget};
use rust_mcp_sdk::{
    macros::{mcp_tool, JsonSchema},
    schema::{schema_utils::CallToolError, CallToolResult},
};
use serde::{Deserialize, Serialize};

/// Discover what each zone's provider can and cannot do
#[mcp_tool(
    name = "hifi_capabilities",
    description = "Discover what a zone supports before acting on it. Every capability reports one of three states: 'supported' (a working path exists), 'unsupported' (the provider's protocol cannot do it - never retry), or 'not_implemented' (the provider can, UHC has not wired it yet - tracked_by names the issue). Omit zone_id for every zone and every provider. States are per provider, not per device: a fixed-volume output still reports volume as supported, so check has_volume_control alongside it.",
    read_only_hint = true
)]
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct HifiCapabilitiesTool {
    /// A zone ID to narrow the report to (get from hifi_zones). Omit for all zones.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
}

pub async fn handle_capabilities(
    state: &AppState,
    args: HifiCapabilitiesTool,
) -> Result<CallToolResult, CallToolError> {
    let env = Envelope::read("hifi_capabilities", "describe_capabilities")
        .param_opt("zone_id", args.zone_id.as_deref());

    match args.zone_id.as_deref() {
        Some(zone_id) => one_zone(state, env, zone_id).await,
        None => Ok(every_zone(state, env).await),
    }
}

/// The report for one zone id.
///
/// The prefix alone determines the provider, so a zone the aggregator does not
/// hold still gets a truthful capability answer — with `zone_name` absent, which
/// is how a client tells "offline" from "typo". A zone id UHC cannot place gets
/// the same refusal every other tool gives it, because the accepted-prefix
/// contract is one contract.
async fn one_zone(
    state: &AppState,
    env: Envelope,
    zone_id: &str,
) -> Result<CallToolResult, CallToolError> {
    let target = ZoneTarget::classify(zone_id);
    let env = env.scope(Scope::for_zone(state, zone_id, target.provider()).await);

    if target.prefix().is_none() {
        return env.refused(
            unplaceable_zone_text(zone_id, target),
            unplaceable_zone_refusal(target),
        );
    }

    let zone = state.aggregator.get_zone(zone_id).await;
    let report = McpCapabilityReport {
        zones: vec![McpCapabilityZone {
            zone_id: zone_id.to_string(),
            zone_name: zone.as_ref().map(|z| z.zone_name.clone()),
            provider: target.provider(),
            has_volume_control: zone.as_ref().map(|z| z.volume_control.is_some()),
        }],
        providers: vec![provider_capabilities(target)],
    };
    Ok(env.json_result(&report))
}

/// The report for everything.
///
/// `providers` lists all five whether or not a zone of that type is online, so a
/// client can plan for a zone type before it appears — and so the AGENTS.md matrix
/// is exactly this payload rather than a subset of it.
async fn every_zone(state: &AppState, env: Envelope) -> CallToolResult {
    // Same visibility policy as `hifi_zones`. This report is how a client discovers what it can do
    // with each zone, so listing a hidden zone here would put it back in front of the assistant
    // that `hifi_zones` just withheld it from.
    let zones: Vec<McpCapabilityZone> = crate::zone_list::visible_zones(state)
        .await
        .into_iter()
        .map(|zone| McpCapabilityZone {
            provider: ZoneTarget::classify(&zone.zone_id).provider(),
            has_volume_control: Some(zone.volume_control.is_some()),
            zone_id: zone.zone_id,
            zone_name: Some(zone.zone_name),
        })
        .collect();

    let providers: Vec<McpProviderCapabilities> = ZoneTarget::PROVIDERS
        .iter()
        .map(|target| provider_capabilities(*target))
        .collect();

    // No scope: this report spans every provider, so naming one would be a lie —
    // the same reasoning as `hifi_zones`.
    env.json_result(&McpCapabilityReport { zones, providers })
}
