//! Turning a button press into playback.
//!
//! One path for a real press from a controller and for the curator's "test" button, so what the
//! curator sees is what the listener gets. Playback itself is `handle_play_ref`, the same code the
//! Library's own Play button runs, so capability honesty, provider checks and per-provider quirks
//! are not reimplemented here.
//!
//! Every outcome has a stable [`PressCode`] and a sentence for a person. A controller with no
//! screen can map the code to a light or a sound; the Controllers page shows the sentence.

use chrono::Utc;
use serde::Serialize;

use super::{LastPress, Shortcut, ShortcutTarget};
use crate::api::AppState;
use crate::mcp::refs::{RefTarget, RoonRefTarget};
use crate::mcp::routing::ZoneTarget;
use crate::mcp::tools::library::{handle_play_ref, HifiPlayRefTool};

/// What happened to a press. Stable: controllers and tests branch on these strings.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PressCode {
    Played,
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
        matches!(self, Self::Played)
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

/// Resolve and play whatever `event` is bound to on `controller_id`.
pub async fn press(state: &AppState, controller_id: &str, event: &str) -> PressOutcome {
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
        PressCode::Played => "played",
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

    let title = shortcut.title.clone();
    let ref_target = match &shortcut.target {
        ShortcutTarget::Roon { trail } => match state.roon.resolve_trail(zone_id, trail).await {
            Ok((multi_session_key, item_key)) => RefTarget::Roon {
                target: RoonRefTarget {
                    item_key,
                    multi_session_key,
                    trail: trail.clone(),
                },
                title,
            },
            Err(error) => {
                return PressOutcome::new(
                    PressCode::ItemUnavailable,
                    format!(
                        "Roon could not find “{}” any more ({error}).",
                        shortcut.title
                    ),
                );
            }
        },
        ShortcutTarget::Lms { target } => RefTarget::Lms {
            target: target.to_play_target(),
            title,
        },
        ShortcutTarget::Spotify { uri } => RefTarget::Spotify {
            uri: uri.clone(),
            title,
        },
        ShortcutTarget::MusicAssistant { uri } => RefTarget::MusicAssistant {
            uri: uri.clone(),
            title,
        },
    };

    let token = state.mcp_refs.mint(ref_target).await;
    let result = handle_play_ref(
        state,
        HifiPlayRefTool {
            r#ref: token,
            zone_id: zone_id.to_string(),
            action: Some("play".to_string()),
        },
    )
    .await;

    match result {
        Ok(call) => interpret(&shortcut.name, call.structured_content.as_ref()),
        Err(error) => PressOutcome::new(PressCode::Refused, error.to_string()),
    }
}

/// Read the play envelope's verdict. `ok` and `accepted` are the two outcomes that mean the
/// provider took the command; anything else carries a refusal whose detail is already worded for
/// a person.
fn interpret(name: &str, body: Option<&serde_json::Value>) -> PressOutcome {
    let outcome = body
        .and_then(|b| b.get("outcome"))
        .and_then(|o| o.as_str())
        .unwrap_or("");
    if matches!(outcome, "ok" | "accepted") {
        return PressOutcome::new(PressCode::Played, format!("Playing “{name}”."));
    }
    let detail = body
        .and_then(|b| b.get("refusal"))
        .and_then(|r| r.get("detail"))
        .and_then(|d| d.as_str())
        .filter(|d| !d.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| format!("The provider did not play “{name}” ({outcome})."));
    PressOutcome::new(PressCode::Refused, detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepted_and_ok_mean_played_and_nothing_else_does() {
        for outcome in ["ok", "accepted"] {
            let result = interpret("Jazz", Some(&json!({ "outcome": outcome })));
            assert!(result.ok);
            assert_eq!(result.code, PressCode::Played);
        }
        for outcome in ["unsupported", "invalid", "error", ""] {
            let result = interpret("Jazz", Some(&json!({ "outcome": outcome })));
            assert!(!result.ok, "{outcome:?} must not read as played");
            assert_eq!(result.code, PressCode::Refused);
        }
        assert!(!interpret("Jazz", None).ok);
    }

    #[test]
    fn a_refusal_reaches_the_person_in_its_own_words() {
        let body = json!({
            "outcome": "unsupported",
            "refusal": { "detail": "Roon refused: zone is grouped." }
        });
        assert_eq!(
            interpret("Jazz", Some(&body)).message,
            "Roon refused: zone is grouped."
        );
    }
}
