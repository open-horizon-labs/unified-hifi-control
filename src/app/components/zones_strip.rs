//! Persistent play-target strip (#550).
//!
//! Zones used to be the home page: a grid of cards, each with its own
//! transport, volume, and (for Music Assistant) an embedded library browse
//! panel. #550 inverts that -- the Library page is now home, and this strip
//! is what replaces the *play-target* half of a zone card: which zone is
//! armed, its now-playing state, transport/volume, and (Music Assistant
//! only) queue transfer. Full zone management (rename, hide, reorder) stays
//! on the dedicated Zones page; this strip only picks a target and drives it.

use crate::app::api::{NowPlaying, QueueRequest, Zone};
use crate::app::components::VolumeControlsCompact;
use dioxus::prelude::*;

#[derive(Clone, PartialEq, Props)]
pub struct ZonesStripProps {
    /// Every known zone, in the user's configured order.
    pub zones: Vec<Zone>,
    /// Now-playing state for zones that have one loaded yet.
    pub now_playing: std::collections::HashMap<String, NowPlaying>,
    /// The currently-armed play target, if resolved.
    pub armed_zone_id: Option<String>,
    /// Fired when the user picks a different target zone from the strip.
    pub on_arm: EventHandler<String>,
    /// Fired for transport/volume actions: `(zone_id, action)`, same
    /// vocabulary as `/control` (`play_pause`, `next`, `previous`,
    /// `vol_up`, `vol_down`).
    pub on_control: EventHandler<(String, String)>,
}

/// Queue transfer is a Music Assistant-only capability today (the backend
/// refuses any pair that isn't both Music Assistant zones -- see
/// `src/mcp/tools/queue.rs`), so the strip only offers it when the armed
/// zone is one.
fn is_musicassistant(zone: &Zone) -> bool {
    zone.zone_id.starts_with("musicassistant:")
}

/// Verbatim-URL contract, same as the armed target's art: `image_url` is
/// already a complete same-origin URL; `image_key` is only ever appended as a
/// cache-busting query param, never substituted in. Shared by the armed
/// target and every enriched picker row (#585) so the two don't drift.
fn resolve_image_url(np: Option<&NowPlaying>) -> String {
    let base_image_url = np.and_then(|n| n.image_url.clone()).unwrap_or_default();
    let image_key = np.and_then(|n| n.image_key.clone());
    if let Some(key) = image_key {
        let sep = if base_image_url.contains('?') {
            "&"
        } else {
            "?"
        };
        format!("{}{}k={}", base_image_url, sep, key)
    } else {
        base_image_url
    }
}

/// A picker row's now-playing summary: `None` when the zone is idle (no
/// `NowPlaying` entry yet, or its `line1` reads "Idle" -- same idle signal
/// the armed target uses), keeping idle rows compact per #585's design.
struct RowNowPlaying {
    track: String,
    is_playing: bool,
    image_url: String,
    has_image: bool,
    volume: Option<f32>,
    volume_type: Option<String>,
    volume_step: Option<f32>,
}

fn row_now_playing(np: Option<&NowPlaying>) -> Option<RowNowPlaying> {
    let np = np?;
    let track = np.line1.clone().unwrap_or_default();
    if track.is_empty() || track == "Idle" {
        return None;
    }
    let image_url = resolve_image_url(Some(np));
    Some(RowNowPlaying {
        track,
        is_playing: np.is_playing,
        has_image: !image_url.is_empty(),
        image_url,
        volume: np.volume,
        volume_type: np.volume_type.clone(),
        volume_step: np.volume_step,
    })
}

#[component]
pub fn ZonesStrip(props: ZonesStripProps) -> Element {
    let mut picker_open = use_signal(|| false);
    let mut transfer_target = use_signal(|| None::<String>);
    let mut transfer_status = use_signal(|| None::<String>);

    let armed = props
        .armed_zone_id
        .as_ref()
        .and_then(|id| props.zones.iter().find(|z| &z.zone_id == id));

    let Some(armed) = armed else {
        // No zones at all -- nothing to arm. The Library page's own empty
        // state already explains this; the strip just stays out of the way.
        return rsx! {};
    };

    let np = props.now_playing.get(&armed.zone_id);
    let is_playing = np.map(|n| n.is_playing).unwrap_or(false);
    let (track, artist) = np
        .map(|n| {
            if n.line1.as_deref().unwrap_or("Idle") != "Idle" {
                (
                    n.line1.clone().unwrap_or_default(),
                    n.line2.clone().unwrap_or_default(),
                )
            } else {
                (String::new(), String::new())
            }
        })
        .unwrap_or_default();

    let image_url = resolve_image_url(np);
    let has_image = !image_url.is_empty();

    let volume = np.and_then(|n| n.volume);
    let volume_type = np.and_then(|n| n.volume_type.clone());
    let volume_step = np.and_then(|n| n.volume_step);

    let armed_id = armed.zone_id.clone();
    let armed_id_prev = armed_id.clone();
    let armed_id_play = armed_id.clone();
    let armed_id_next = armed_id.clone();
    let armed_id_vol_down = armed_id.clone();
    let armed_id_vol_up = armed_id.clone();
    let armed_id_transfer = armed_id.clone();

    let can_transfer = is_musicassistant(armed);
    let transfer_targets: Vec<(String, String)> = if can_transfer {
        props
            .zones
            .iter()
            .filter(|z| is_musicassistant(z) && z.zone_id != armed.zone_id)
            .map(|z| (z.zone_id.clone(), z.zone_name.clone()))
            .collect()
    } else {
        Vec::new()
    };
    if can_transfer && transfer_target.read().is_none() {
        if let Some((id, _)) = transfer_targets.first() {
            transfer_target.set(Some(id.clone()));
        }
    }

    let do_transfer = move |_| {
        let Some(target) = transfer_target() else {
            return;
        };
        let zone_id = armed_id_transfer.clone();
        transfer_status.set(Some("Transferring…".to_string()));
        spawn(async move {
            let req = QueueRequest {
                zone_id,
                action: "transfer".to_string(),
                item_id: None,
                position: None,
                target_zone_id: Some(target),
            };
            let message = match crate::app::api::post_queue_action(&req).await {
                Ok(env) if env.is_ok() => "Queue transferred".to_string(),
                Ok(env) => env.error_detail(),
                Err(message) => message,
            };
            transfer_status.set(Some(message));
        });
    };

    let on_control = props.on_control;
    let on_arm = props.on_arm;
    let other_zones: Vec<Zone> = props
        .zones
        .iter()
        .filter(|z| z.zone_id != armed.zone_id)
        .cloned()
        .collect();

    rsx! {
        div { class: "zones-strip",
            // #573 (visual pass V4): Escape closes the zone picker. Key
            // events bubble from whichever picker control has focus, so the
            // strip container is the one place that hears them all.
            onkeydown: move |evt| {
                if evt.key() == Key::Escape {
                    picker_open.set(false);
                }
            },
            div { class: "zones-strip-inner",
                // Art + now playing, doubles as the picker trigger on small
                // screens. Transport/volume below are siblings of this
                // button, not descendants, so they already can't trigger the
                // picker toggle -- the "whole header row" the owner asked
                // about is this button, and it already is the full
                // clickable target with no propagation hazard from the
                // controls (owner feedback on #585).
                button {
                    class: "zones-strip-target",
                    r#type: "button",
                    aria_label: if picker_open() { "Collapse zone picker" } else { "Expand zone picker" },
                    title: if picker_open() { "Collapse" } else { "Expand" },
                    aria_expanded: picker_open(),
                    onclick: move |_| picker_open.toggle(),
                    if has_image {
                        img {
                            class: "zones-strip-art",
                            src: "{image_url}",
                            alt: "Album art"
                        }
                    } else {
                        div { class: "zones-strip-art zones-strip-art--empty", "♪" }
                    }
                    div { class: "zones-strip-meta",
                        span { class: "zones-strip-zone-name", "{armed.zone_name}" }
                        if !track.is_empty() {
                            span { class: "zones-strip-track", "{track}" }
                            span { class: "zones-strip-artist", "{artist}" }
                        } else {
                            span { class: "zones-strip-track zones-strip-track--idle", "Nothing playing" }
                        }
                    }
                    // Owner feedback on #585: the chevron read as pure
                    // decoration. It now rotates with `picker_open` (same
                    // legibility signal as `aria_expanded`) and follows the
                    // library-play-btn convention -- visible at reduced
                    // opacity always (so touch devices see it), full
                    // strength on hover/focus rather than hover-only.
                    svg {
                        class: if picker_open() { "zones-strip-chevron zones-strip-chevron--open" } else { "zones-strip-chevron" },
                        fill: "none", view_box: "0 0 24 24", stroke: "currentColor", "stroke-width": "2",
                        path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M6 9l6 6 6-6" }
                    }
                }

                // Transport
                div { class: "zones-strip-transport",
                    button {
                        class: "btn btn-ghost",
                        aria_label: "Previous track",
                        onclick: move |_| on_control.call((armed_id_prev.clone(), "previous".to_string())),
                        svg { class: "w-5 h-5", fill: "currentColor", view_box: "0 0 24 24",
                            path { d: "M6 6h2v12H6zm3.5 6l8.5 6V6z" }
                        }
                    }
                    button {
                        class: "btn btn-primary",
                        aria_label: if is_playing { "Pause" } else { "Play" },
                        onclick: move |_| on_control.call((armed_id_play.clone(), "play_pause".to_string())),
                        if is_playing {
                            svg { class: "w-5 h-5", fill: "currentColor", view_box: "0 0 24 24",
                                path { d: "M6 19h4V5H6v14zm8-14v14h4V5h-4z" }
                            }
                        } else {
                            svg { class: "w-5 h-5", fill: "currentColor", view_box: "0 0 24 24",
                                path { d: "M8 5v14l11-7z" }
                            }
                        }
                    }
                    button {
                        class: "btn btn-ghost",
                        aria_label: "Next track",
                        onclick: move |_| on_control.call((armed_id_next.clone(), "next".to_string())),
                        svg { class: "w-5 h-5", fill: "currentColor", view_box: "0 0 24 24",
                            path { d: "M6 18l8.5-6L6 6v12zM16 6v12h2V6h-2z" }
                        }
                    }
                }

                VolumeControlsCompact {
                    volume: volume,
                    volume_type: volume_type,
                    volume_step: volume_step,
                    on_vol_down: move |_| on_control.call((armed_id_vol_down.clone(), "vol_down".to_string())),
                    on_vol_up: move |_| on_control.call((armed_id_vol_up.clone(), "vol_up".to_string())),
                }
            }

            if picker_open() {
                div { class: "zones-strip-picker",
                    if other_zones.is_empty() {
                        p { class: "text-sm text-muted p-3", "This is the only zone." }
                    } else {
                        ul { class: "zones-strip-picker-list",
                            for zone in other_zones {
                                {
                                    let zone_id = zone.zone_id.clone();
                                    let zone_id_arm = zone_id.clone();
                                    let zone_id_play = zone_id.clone();
                                    let zone_id_vol_down = zone_id.clone();
                                    let zone_id_vol_up = zone_id.clone();
                                    let source = zone.source.clone().unwrap_or_default();
                                    // Each row's own now-playing summary, read
                                    // straight from the map the Library page
                                    // already threads through props (#585) --
                                    // no new fetch, no new prop, selective
                                    // lookup per row like the armed target
                                    // above.
                                    let row_np = row_now_playing(props.now_playing.get(&zone.zone_id));
                                    rsx! {
                                        li { key: "{zone.zone_id}",
                                            div { class: "zones-strip-picker-row",
                                                // Row click arms the zone. A real <button>
                                                // (not a div with a click handler) so the row
                                                // stays keyboard-operable; the controls below
                                                // are a sibling, not a nested button, and stop
                                                // propagation so pause/volume don't also
                                                // re-arm/navigate (#585).
                                                button {
                                                    class: "zones-strip-picker-item",
                                                    r#type: "button",
                                                    onclick: move |_| {
                                                        on_arm.call(zone_id_arm.clone());
                                                        picker_open.set(false);
                                                    },
                                                    if let Some(np) = &row_np {
                                                        if np.has_image {
                                                            img {
                                                                class: "zones-strip-picker-art",
                                                                src: "{np.image_url}",
                                                                alt: "Album art"
                                                            }
                                                        } else {
                                                            div { class: "zones-strip-picker-art zones-strip-picker-art--empty", "♪" }
                                                        }
                                                    }
                                                    div { class: "zones-strip-picker-meta",
                                                        div { class: "zones-strip-picker-name-row",
                                                            span { "{zone.zone_name}" }
                                                            span { class: "badge badge-secondary", "{crate::app::api::source_label(&source)}" }
                                                        }
                                                        if let Some(np) = &row_np {
                                                            div { class: "zones-strip-picker-np",
                                                                if np.is_playing {
                                                                    span { class: "library-eq", aria_label: "Playing",
                                                                        span {} span {} span {}
                                                                    }
                                                                }
                                                                span { class: "zones-strip-picker-track", "{np.track}" }
                                                            }
                                                        }
                                                    }
                                                }
                                                if let Some(np) = row_np {
                                                    div {
                                                        class: "zones-strip-picker-controls",
                                                        // stop_propagation: these buttons
                                                        // control the row's zone directly, they
                                                        // must not also arm it (#585).
                                                        onclick: move |evt: Event<MouseData>| evt.stop_propagation(),
                                                        button {
                                                            class: "btn btn-outline btn-sm",
                                                            r#type: "button",
                                                            aria_label: if np.is_playing { "Pause" } else { "Play" },
                                                            onclick: move |_| on_control.call((zone_id_play.clone(), "play_pause".to_string())),
                                                            if np.is_playing {
                                                                svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                                                    path { d: "M6 19h4V5H6v14zm8-14v14h4V5h-4z" }
                                                                }
                                                            } else {
                                                                svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                                                    path { d: "M8 5v14l11-7z" }
                                                                }
                                                            }
                                                        }
                                                        VolumeControlsCompact {
                                                            volume: np.volume,
                                                            volume_type: np.volume_type,
                                                            volume_step: np.volume_step,
                                                            on_vol_down: move |_| on_control.call((zone_id_vol_down.clone(), "vol_down".to_string())),
                                                            on_vol_up: move |_| on_control.call((zone_id_vol_up.clone(), "vol_up".to_string())),
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    if can_transfer && !transfer_targets.is_empty() {
                        div { class: "zones-strip-transfer",
                            span { class: "text-sm text-muted", "Transfer queue to:" }
                            select {
                                class: "form-select text-sm",
                                onchange: move |evt| transfer_target.set(Some(evt.value())),
                                for (target_id, target_name) in transfer_targets.iter().cloned() {
                                    option { key: "{target_id}", value: "{target_id}", "{target_name}" }
                                }
                            }
                            button {
                                class: "btn btn-ghost text-sm",
                                r#type: "button",
                                onclick: do_transfer,
                                "Transfer"
                            }
                            if let Some(message) = transfer_status() {
                                span { class: "text-xs text-muted", "{message}" }
                            }
                        }
                    }
                }
            }
        }
    }
}
