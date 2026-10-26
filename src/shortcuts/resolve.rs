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

use super::{LastPress, Shortcut};
use crate::api::AppState;
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
    let bindings = state.shortcuts.bindings_for(controller_id).await;
    let shortcut_id = bindings.get(event);
    let outcome = press_inner(state, controller_id, shortcut_id.map(String::as_str)).await;
    if let Some(shortcut_id) = shortcut_id {
        state
            .shortcuts
            .record_binding_press(
                controller_id,
                event,
                shortcut_id,
                outcome.zone_id.as_deref(),
                LastPress {
                    at: Utc::now(),
                    code: code_str(outcome.code),
                    ok: outcome.ok,
                    message: outcome.message.clone(),
                },
            )
            .await;
    }
    state
        .shortcuts
        .record_event(controller_id, event, code_str(outcome.code))
        .await;
    outcome
}

async fn press_inner(
    state: &AppState,
    controller_id: &str,
    shortcut_id: Option<&str>,
) -> PressOutcome {
    let Some(shortcut_id) = shortcut_id else {
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

    match crate::library_playback::play(
        state.reliable_commands.as_ref(),
        zone_id,
        &shortcut.target,
        &shortcut.title,
    )
    .await
    {
        Ok(()) => PressOutcome::new(
            PressCode::Accepted,
            format!("Sent “{}” to play.", shortcut.name),
        ),
        Err(error) if error.is::<crate::library_playback::ItemUnavailable>() => {
            PressOutcome::new(PressCode::ItemUnavailable, error.to_string())
        }
        Err(error) => PressOutcome::new(
            PressCode::Refused,
            format!(
                "Could not establish acceptance of “{}”: {error}",
                shortcut.name
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
