//! Turning a button press into playback.
//!
//! One path for a real press from a controller and for the curator's "test" button, so what the
//! curator sees is what the listener gets. Playback goes through the adapter registry, not the MCP
//! `hifi_play_ref` handler (which still reaches the Roon and LMS adapters directly and is
//! grandfathered in `adapter_boundary_lint`): new code should not lean on that debt.
//!
//! The outcome says what the provider's reply supports. `accepted` means it took the command, not
//! that audio started; nothing here observes that, so nothing here claims it.
//!
//! Every outcome has a stable [`PressCode`] and a sentence for a person. A controller with no
//! screen can map the code to a light or a sound; the Controllers page shows the sentence.

use chrono::Utc;
use serde::Serialize;

use super::{LastPress, Shortcut, ShortcutTarget};
use crate::api::AppState;
use crate::mcp::collection_locations::CollectionLocation;
use crate::mcp::routing::ZoneTarget;

/// What happened to a press. Stable: controllers and tests branch on these strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressCode {
    /// The provider took the command. Not a claim that audio started.
    Accepted,
    /// The key and gesture are not bound on this controller. Not an error: most presses are not
    /// shortcuts.
    Unmapped,
    /// The binding points at a shortcut that no longer exists.
    NoShortcut,
    /// The controller has no zone yet.
    NoZone,
    /// The controller's zone is not currently discovered.
    ZoneMissing,
    /// The shortcut is for a different provider than the controller's zone.
    ProviderMismatch,
    /// The provider could no longer find the item.
    ItemUnavailable,
    /// The provider or bridge declined the play.
    Refused,
}

impl PressCode {
    pub fn is_ok(self) -> bool {
        matches!(self, Self::Accepted)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PressOutcome {
    pub ok: bool,
    pub code: PressCode,
    pub message: String,
    /// The shortcut's name, once one was found.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shortcut: Option<String>,
    /// The zone the press targeted, once one was resolved.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone_id: Option<String>,
}

impl PressOutcome {
    fn new(code: PressCode, message: impl Into<String>) -> Self {
        Self {
            ok: code.is_ok(),
            code,
            message: message.into(),
            shortcut: None,
            zone_id: None,
        }
    }

    /// A plain press on a transport key: not a shortcut, and its normal job is the controller's.
    pub fn unmapped_transport() -> Self {
        Self::new(
            PressCode::Unmapped,
            "A plain press on that key keeps its normal job and cannot be a shortcut.",
        )
    }

    fn with_shortcut(mut self, name: &str) -> Self {
        self.shortcut = Some(name.to_string());
        self
    }

    fn with_zone(mut self, zone_id: &str) -> Self {
        self.zone_id = Some(zone_id.to_string());
        self
    }
}

/// The zone a controller's press targets: the zone it was assigned, otherwise the one it last
/// reported. `None` before it has either. Deliberately not "the first visible zone": a press must
/// never land somewhere the controller was not pointed at.
pub async fn controller_zone(state: &AppState, controller_id: &str) -> Option<String> {
    let knob = state.knobs.get(controller_id).await?;
    knob.config
        .assigned_zone_id
        .filter(|id| !id.is_empty())
        .or(knob.status.zone_id.filter(|id| !id.is_empty()))
}

/// Resolve and play whatever `event` is bound to on `controller_id`, and remember that the
/// controller sent it so the editor can offer it.
pub async fn press(state: &AppState, controller_id: &str, event: &str) -> PressOutcome {
    let outcome = press_inner(state, controller_id, event).await;
    state
        .shortcuts
        .record_event(controller_id, event, code_str(outcome.code))
        .await;
    outcome
}

async fn press_inner(state: &AppState, controller_id: &str, event: &str) -> PressOutcome {
    let bindings = state.shortcuts.bindings_for(controller_id).await;
    let Some(shortcut_id) = bindings.get(event) else {
        return PressOutcome::new(PressCode::Unmapped, "No shortcut is set for that button.");
    };
    let Some(shortcut) = state.shortcuts.get(shortcut_id).await else {
        return PressOutcome::new(
            PressCode::NoShortcut,
            "That button points at a shortcut that was deleted. Choose another in Controllers.",
        );
    };
    let Some(zone_id) = controller_zone(state, controller_id).await else {
        return record(
            state,
            &shortcut,
            PressOutcome::new(
                PressCode::NoZone,
                "This controller has no zone yet. Pick a zone on it, then try again.",
            )
            .with_shortcut(&shortcut.name),
        )
        .await;
    };
    play(state, &shortcut, &zone_id).await
}

/// Play `shortcut` on `zone_id` and remember how it went.
pub async fn play(state: &AppState, shortcut: &Shortcut, zone_id: &str) -> PressOutcome {
    let outcome = play_inner(state, shortcut, zone_id)
        .await
        .with_shortcut(&shortcut.name)
        .with_zone(zone_id);
    record(state, shortcut, outcome).await
}

async fn record(state: &AppState, shortcut: &Shortcut, outcome: PressOutcome) -> PressOutcome {
    state
        .shortcuts
        .record_press(
            &shortcut.id,
            LastPress {
                at: Utc::now(),
                code: code_str(outcome.code),
                ok: outcome.ok,
                message: outcome.message.clone(),
            },
        )
        .await;
    outcome
}

fn code_str(code: PressCode) -> &'static str {
    match code {
        PressCode::Accepted => "accepted",
        PressCode::Unmapped => "unmapped",
        PressCode::NoShortcut => "no_shortcut",
        PressCode::NoZone => "no_zone",
        PressCode::ZoneMissing => "zone_missing",
        PressCode::ProviderMismatch => "provider_mismatch",
        PressCode::ItemUnavailable => "item_unavailable",
        PressCode::Refused => "refused",
    }
}

async fn play_inner(state: &AppState, shortcut: &Shortcut, zone_id: &str) -> PressOutcome {
    // Hidden zones are found here exactly like visible ones: `get_zone` is the aggregator's own
    // lookup, which `zone_list` never filters. Only a zone that is not discovered at all is
    // missing, and that is reported, never papered over with another zone.
    if state.aggregator.get_zone(zone_id).await.is_none() {
        return PressOutcome::new(
            PressCode::ZoneMissing,
            format!(
                "This controller's zone ({zone_id}) is not available right now, so nothing was \
                 played."
            ),
        );
    }

    if ZoneTarget::classify(zone_id).provider() != shortcut.target.provider() {
        return PressOutcome::new(
            PressCode::ProviderMismatch,
            format!(
                "“{}” is a {} item but this controller's zone is on a different provider.",
                shortcut.name,
                shortcut.target.provider_label()
            ),
        );
    }

    // Every provider is reached through the adapter registry, the sanctioned seam. This does not
    // go through the MCP `hifi_play_ref` handler: that one reaches the Roon and LMS adapters
    // directly (grandfathered in `adapter_boundary_lint`), and building on it would have made new
    // code lean on that debt where the lint cannot see it.
    let title = shortcut.title.as_str();
    let result: anyhow::Result<String> = match &shortcut.target {
        ShortcutTarget::Roon { location } => {
            let (session_key, item_key) =
                match resolve_roon_location(state, zone_id, location).await {
                    Ok(pair) => pair,
                    Err(error) => {
                        return PressOutcome::new(
                            PressCode::ItemUnavailable,
                            format!("Roon could not find “{title}” any more ({error})."),
                        );
                    }
                };
            let trail: Vec<&str> = location.steps().iter().map(|s| s.title.as_str()).collect();
            state
                .adapter_registry
                .library_content(
                    "roon",
                    "play_resolved",
                    &serde_json::json!({
                        "zone_id": zone_id,
                        "item_key": item_key,
                        "session_key": session_key,
                        "trail": trail,
                        "title": title,
                    }),
                )
                .await
                .map(message_of)
        }
        ShortcutTarget::Lms { target } => state
            .adapter_registry
            .library_content(
                "lms",
                "play_durable_target",
                &serde_json::json!({ "zone_id": zone_id, "target": target, "title": title }),
            )
            .await
            .map(message_of),
        ShortcutTarget::Spotify { uri } => {
            state
                .adapter_registry
                .play_library_uri("spotify", zone_id, uri)
                .await
        }
        ShortcutTarget::MusicAssistant { uri } => {
            state
                .adapter_registry
                .play_library_uri("musicassistant", zone_id, uri)
                .await
        }
    };

    // "Accepted" is what the provider's reply can honestly support: it took the command. Whether
    // audio started is not something this call observed, so it is never reported as played.
    match result {
        Ok(_) => PressOutcome::new(
            PressCode::Accepted,
            format!("Sent “{}” to play.", shortcut.name),
        ),
        Err(error) => PressOutcome::new(
            PressCode::Refused,
            format!("“{}” was not played: {error}", shortcut.name),
        ),
    }
}

/// The provider's own one-line reply, when it gave one.
fn message_of(reply: serde_json::Value) -> String {
    reply
        .get("message")
        .and_then(|m| m.as_str())
        .unwrap_or_default()
        .to_string()
}

/// Resolve a pinned Roon location in a fresh browse session, through the adapter registry like
/// every other surface (`adapter_boundary_lint` forbids reaching for the concrete adapter).
///
/// This is the resolver the Library's durable URLs use: title and subtitle identify a row, position
/// only separates genuine duplicates, and when neither identifies exactly one row it fails, so a
/// press says "not found" instead of playing a different recording.
async fn resolve_roon_location(
    state: &AppState,
    zone_id: &str,
    location: &CollectionLocation,
) -> anyhow::Result<(String, String)> {
    let reply = state
        .adapter_registry
        .library_content(
            "roon",
            "collections_resolve_location",
            &serde_json::json!({ "zone_id": zone_id, "location": location }),
        )
        .await?;
    let field = |name: &str| {
        reply
            .get(name)
            .and_then(|v| v.as_str())
            .map(str::to_string)
            .ok_or_else(|| anyhow::anyhow!("Roon returned no {name}"))
    };
    Ok((field("session_key")?, field("item_key")?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_providers_reply_is_read_without_inventing_one() {
        assert_eq!(
            message_of(serde_json::json!({ "message": "Play Now" })),
            "Play Now"
        );
        assert_eq!(message_of(serde_json::json!({})), "");
        assert_eq!(message_of(serde_json::json!(null)), "");
    }

    #[test]
    fn only_accepted_is_ok_and_nothing_claims_the_audio_started() {
        assert!(PressCode::Accepted.is_ok());
        for code in [
            PressCode::Unmapped,
            PressCode::NoShortcut,
            PressCode::NoZone,
            PressCode::ZoneMissing,
            PressCode::ProviderMismatch,
            PressCode::ItemUnavailable,
            PressCode::Refused,
        ] {
            assert!(!code.is_ok(), "{code:?}");
        }
        // The wire word says what happened to the command, not to the music.
        assert_eq!(code_str(PressCode::Accepted), "accepted");
        assert_ne!(code_str(PressCode::Accepted), "played");
    }
}
