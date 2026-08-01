//! MCP resources: readable bridge state addressable by URI (issue #397).
//!
//! # What this is, and is not
//!
//! Every value here is already reachable through a `read_only_hint` tool —
//! that tool keeps working exactly as it does today. Resources are a second,
//! address-based path to the *same* state, for clients that model ambient
//! state (what is playing, is the bridge connected) as something to read
//! rather than something to ask for by calling a tool. This is the least
//! load-bearing issue in epic #392: nothing here is new capability, only a new
//! shape for capability that already exists.
//!
//! # URI scheme
//!
//! | URI                        | Equivalent tool           |
//! |-----------------------------|---------------------------|
//! | `hifi://zones`              | `hifi_zones`               |
//! | `hifi://zones/{zone_id}`    | `hifi_now_playing`         |
//! | `hifi://status`             | `hifi_status`              |
//! | `hifi://hqplayer/status`    | `hifi_hqplayer_status`     |
//! | `hifi://hqplayer/profiles`  | `hifi_hqplayer_profiles`   |
//!
//! `zone_id` already carries its own provider prefix (`roon:`, `lms:`, ...), so
//! `hifi://zones/roon:1601a5d4` is a valid URI despite the second colon — no
//! escaping is applied or required, because URIs are matched by prefix-stripping
//! rather than parsed as RFC 3986 authority/path components.
//!
//! # No second source of truth
//!
//! Each resource's payload is built by the exact function its equivalent tool
//! calls — [`crate::mcp::tools::zones::zones_payload`],
//! [`crate::mcp::tools::zones::now_playing_payload`],
//! [`crate::mcp::tools::status::status_payload`],
//! [`crate::mcp::tools::hqplayer::hqp_status_payload`] and
//! [`crate::mcp::tools::hqplayer::hqp_profiles_payload`]. There is exactly one
//! place that reads the aggregator or the adapter accessors for each kind of
//! state, so a resource and its tool cannot compute different answers for the
//! same underlying state. `tests/mcp_contract.rs` asserts this directly for
//! zones and now-playing (including a live LMS zone, not just the empty case).
//!
//! # Live per-zone enumeration, not a template
//!
//! [`list_resources`] reads `ZoneAggregator` fresh on every call, so a zone
//! discovered after a client's last `resources/list` is addressable on the very
//! next one — no restart, no resource template needed. A single `hifi://zones`
//! document was rejected in the solution-space gate: a zone inside a bigger
//! document has no URI of its own, which fails addressability outright.
//!
//! # HQPlayer resources respect the same settings gate as HQPlayer tools
//!
//! [`list_resources`] takes the same `hqplayer_enabled` flag
//! [`crate::mcp::tools::list_tools`] does, and hides the two HQPlayer resources
//! on the same condition. As with tools, this only gates *advertisement* —
//! [`read_resource`] does not re-check settings, matching how a HQPlayer tool
//! called directly (unlisted) still executes.
//!
//! # `listChanged`: best-effort, wired off the events the aggregator already
//! consumes
//!
//! [`spawn_list_changed_notifier`] subscribes to the same bus
//! [`crate::aggregator::ZoneAggregator`] already consumes and sends
//! `notifications/resources/list_changed` on any event that changes the *set*
//! of zones (`ZoneDiscovered`, `ZoneRemoved`, `ZonesFlushed`) — not on every
//! `NowPlayingChanged`, which would be noise for a signal that only means "the
//! zone list changed, re-list". This is genuinely best-effort: with
//! `event_store: None` (`src/mcp/mod.rs`), a client with no open GET/SSE stream
//! simply does not receive it, and there is no replay. That is why `subscribe`
//! is not advertised at all — see [`crate::mcp::server::server_details`].
//!
//! # Reading an unknown or stale URI
//!
//! [`read_resource`] never panics and never returns an empty success for a URI
//! it does not recognise or a zone the aggregator no longer holds. Both produce
//! a JSON-RPC error with the MCP-conventional "resource not found" code
//! (`-32002`), carrying the offending URI in `data` so a client can log which
//! one it asked for.

use crate::api::AppState;
use crate::bus::{BusEvent, SharedBus};
use crate::mcp::tools::{hqplayer, status, zones};
use rust_mcp_sdk::{
    schema::{ReadResourceContent, ReadResourceResult, Resource, RpcError, TextResourceContents},
    McpServer,
};
use serde::Serialize;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// `hifi_zones`'s resource equivalent: every zone, in one document.
const ZONES_URI: &str = "hifi://zones";
/// Prefix for a per-zone `hifi_now_playing` equivalent. The full URI appends the
/// zone id verbatim, e.g. `hifi://zones/roon:1601a5d4`.
const ZONE_PREFIX: &str = "hifi://zones/";
/// `hifi_status`'s resource equivalent.
const STATUS_URI: &str = "hifi://status";
/// `hifi_hqplayer_status`'s resource equivalent. Listed only when the HQPlayer
/// adapter is enabled in settings.
const HQP_STATUS_URI: &str = "hifi://hqplayer/status";
/// `hifi_hqplayer_profiles`'s resource equivalent. Listed only when the
/// HQPlayer adapter is enabled in settings.
const HQP_PROFILES_URI: &str = "hifi://hqplayer/profiles";

/// The MIME type every resource here declares: each payload is exactly what the
/// equivalent tool's structured content carries, rendered as JSON text.
const JSON_MIME_TYPE: &str = "application/json";

/// Render `payload` as the resource's one content entry.
///
/// Uses [`serde_json::to_string_pretty`] rather than `to_value`'s `BTreeMap`
/// ordering, for the same reason [`crate::mcp::envelope::Envelope::json_result`]
/// does: the payload struct serializes in declaration order, so the resource's
/// text matches what a human (or a model) reading the equivalent tool's text
/// would see, key for key.
fn text_resource(uri: &str, payload: &impl Serialize) -> ReadResourceContent {
    let text = serde_json::to_string_pretty(payload).unwrap_or_else(|_| "{}".to_string());
    ReadResourceContent::TextResourceContents(TextResourceContents {
        meta: None,
        mime_type: Some(JSON_MIME_TYPE.to_string()),
        text,
        uri: uri.to_string(),
    })
}

/// The MCP-conventional "Resource not found" error (`-32002`), carrying the
/// offending URI and a discovery hint, so a client can tell which of possibly
/// several requests failed and how to recover — the resource-side equivalent of
/// [`crate::mcp::envelope::Refusal::UnknownTarget`]'s `discover_with`, which
/// names `hifi_zones` for the same underlying fact (an unrecognised or stale
/// zone id) when a tool hits it instead of a resource read.
///
/// `RpcError` (the type every `ServerHandler` resource method returns) has no
/// built-in constructor for this code — only `SdkError`, a different type used
/// internally by the transport, does — so this builds the struct directly. Its
/// three fields are public for exactly this reason.
fn resource_not_found(uri: &str) -> RpcError {
    RpcError {
        code: -32002,
        message: "Resource not found".to_string(),
        data: Some(serde_json::json!({
            "uri": uri,
            "hint": "call resources/list for the current set of valid URIs",
        })),
    }
}

fn zone_resource(zone_id: &str, zone_name: &str) -> Resource {
    Resource {
        annotations: None,
        description: Some(format!(
            "Current playback state for zone '{zone_id}' ({zone_name}): track, artist, \
             album, play state, volume. Same payload as the hifi_now_playing tool."
        )),
        icons: vec![],
        meta: None,
        mime_type: Some(JSON_MIME_TYPE.to_string()),
        name: format!("now_playing:{zone_id}"),
        size: None,
        title: Some(format!("Now playing: {zone_name}")),
        uri: format!("{ZONE_PREFIX}{zone_id}"),
    }
}

/// Every resource UHC advertises right now, enumerated live so a zone
/// discovered since the last call is included without a restart.
///
/// `hqplayer_enabled` gates the two HQPlayer resources exactly as
/// [`crate::mcp::tools::list_tools`] gates the four HQPlayer tools; the caller
/// (`HifiMcpHandler::handle_list_resources_request`) supplies it from the same
/// `load_app_settings()` read.
pub async fn list_resources(state: &AppState, hqplayer_enabled: bool) -> Vec<Resource> {
    let mut resources = vec![
        Resource {
            annotations: None,
            description: Some(
                "All available playback zones (Roon, LMS, OpenHome, UPnP, HQPlayer), with \
                 state, volume and mute. Same payload as the hifi_zones tool."
                    .to_string(),
            ),
            icons: vec![],
            meta: None,
            mime_type: Some(JSON_MIME_TYPE.to_string()),
            name: "zones".to_string(),
            size: None,
            title: Some("All zones".to_string()),
            uri: ZONES_URI.to_string(),
        },
        Resource {
            annotations: None,
            description: Some(
                "Overall bridge status (Roon connection, HQPlayer config). Same payload as \
                 the hifi_status tool."
                    .to_string(),
            ),
            icons: vec![],
            meta: None,
            mime_type: Some(JSON_MIME_TYPE.to_string()),
            name: "status".to_string(),
            size: None,
            title: Some("Bridge status".to_string()),
            uri: STATUS_URI.to_string(),
        },
    ];

    for zone in state.aggregator.get_zones().await {
        resources.push(zone_resource(&zone.zone_id, &zone.zone_name));
    }

    if hqplayer_enabled {
        resources.push(Resource {
            annotations: None,
            description: Some(
                "HQPlayer Embedded status and current pipeline settings. Same payload as \
                 the hifi_hqplayer_status tool."
                    .to_string(),
            ),
            icons: vec![],
            meta: None,
            mime_type: Some(JSON_MIME_TYPE.to_string()),
            name: "hqplayer_status".to_string(),
            size: None,
            title: Some("HQPlayer status".to_string()),
            uri: HQP_STATUS_URI.to_string(),
        });
        resources.push(Resource {
            annotations: None,
            description: Some(
                "Available HQPlayer Embedded configurations. Same payload as the \
                 hifi_hqplayer_profiles tool."
                    .to_string(),
            ),
            icons: vec![],
            meta: None,
            mime_type: Some(JSON_MIME_TYPE.to_string()),
            name: "hqplayer_profiles".to_string(),
            size: None,
            title: Some("HQPlayer profiles".to_string()),
            uri: HQP_PROFILES_URI.to_string(),
        });
    }

    resources
}

/// Read one resource by URI.
///
/// An unknown scheme, an unknown fixed URI, or a `hifi://zones/{zone_id}` whose
/// zone the aggregator does not (or no longer) hold all produce
/// [`resource_not_found`] rather than a panic or an empty success.
pub async fn read_resource(state: &AppState, uri: &str) -> Result<ReadResourceResult, RpcError> {
    let content = match uri {
        ZONES_URI => text_resource(uri, &zones::zones_payload(state).await),
        STATUS_URI => text_resource(uri, &status::status_payload(state).await),
        HQP_STATUS_URI => text_resource(uri, &hqplayer::hqp_status_payload(state).await),
        HQP_PROFILES_URI => text_resource(uri, &hqplayer::hqp_profiles_payload(state).await),
        _ => {
            let zone_id = uri
                .strip_prefix(ZONE_PREFIX)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| resource_not_found(uri))?;
            let payload = zones::now_playing_payload(state, zone_id)
                .await
                .ok_or_else(|| resource_not_found(uri))?;
            text_resource(uri, &payload)
        }
    };

    Ok(ReadResourceResult {
        contents: vec![content],
        meta: None,
    })
}

/// Whether a bus event changes the *set* of zones, and therefore what
/// `resources/list` would return next.
///
/// `NowPlayingChanged`, `VolumeChanged` and friends change a resource's
/// *contents*, not the *list* — and since `subscribe` is not advertised, this
/// server has no channel for content-level staleness at all. Only listing
/// changes get a signal.
fn changes_the_zone_list(event: &BusEvent) -> bool {
    matches!(
        event,
        BusEvent::ZoneDiscovered { .. }
            | BusEvent::ZoneRemoved { .. }
            | BusEvent::ZonesFlushed { .. }
    )
}

/// Best-effort `notifications/resources/list_changed`, for one session.
///
/// Spawned from [`crate::mcp::handler::HifiMcpHandler::on_initialized`], which
/// fires once per session with that session's own `runtime` — so this
/// subscribes to the bus once per connected client and stops when sending to
/// that client starts failing (session torn down, stream gone), or when the
/// server itself shuts down (`shutdown`, the same [`CancellationToken`] every
/// other background loop in this crate selects on — see
/// `tests/spawn_cancellation_lint.rs`, which fails a `tokio::spawn` loop with no
/// cancellation arm).
///
/// This is deliberately best-effort and undocumented as anything stronger: with
/// `event_store: None`, a client with no open GET/SSE stream never receives
/// this notification and there is no way to replay it later. A client that
/// wants to be sure it has the current zone list should still call
/// `resources/list` itself; this notification is only ever a hint to do so
/// sooner.
pub fn spawn_list_changed_notifier(
    bus: SharedBus,
    runtime: Arc<dyn McpServer>,
    shutdown: CancellationToken,
) {
    let mut rx = bus.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                event = rx.recv() => match event {
                    Ok(event) if changes_the_zone_list(&event) => {
                        if runtime.notify_resource_list_changed(None).await.is_err() {
                            // The client's stream is gone (or never existed); best-effort
                            // means giving up quietly rather than looping forever against
                            // a session nothing is listening on.
                            break;
                        }
                    }
                    Ok(_) => continue,
                    // Missed events under load: keep going, the next one may still
                    // matter. A closed bus means the server is shutting down.
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zone_list_events_are_recognised_and_state_events_are_not() {
        use crate::bus::{PlaybackState, Zone};

        let zone = Zone {
            zone_id: "roon:x".to_string(),
            zone_name: "Test".to_string(),
            state: PlaybackState::Stopped,
            volume_control: None,
            now_playing: None,
            source: "roon".to_string(),
            is_controllable: true,
            is_seekable: false,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        };

        assert!(changes_the_zone_list(&BusEvent::ZoneDiscovered { zone }));
        assert!(changes_the_zone_list(&BusEvent::ZoneRemoved {
            zone_id: crate::bus::PrefixedZoneId::parse("roon:x").expect("valid prefix"),
        }));
        assert!(changes_the_zone_list(&BusEvent::ZonesFlushed {
            adapter: "roon".to_string(),
            zone_ids: vec!["roon:x".to_string()],
        }));
        assert!(!changes_the_zone_list(&BusEvent::VolumeChanged {
            output_id: "roon:x".to_string(),
            value: 50.0,
            is_muted: false,
        }));
    }

    /// The parsing rule the resource reader relies on: a zone id is recovered
    /// by stripping the prefix, and an empty remainder (`hifi://zones/`) is
    /// treated as absent rather than as a valid, empty zone id.
    #[test]
    fn zone_uri_parsing_rejects_an_empty_zone_id() {
        assert_eq!(
            "hifi://zones/roon:x".strip_prefix(ZONE_PREFIX),
            Some("roon:x")
        );
        assert_eq!(
            "hifi://zones/"
                .strip_prefix(ZONE_PREFIX)
                .filter(|id| !id.is_empty()),
            None
        );
    }
}
