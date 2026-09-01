//! Spotify Connect page.
//!
//! Spotify zones are owned by the adapter and read from the same aggregator
//! used by the Zones page. This page is a focused view for Spotify devices.

use crate::app::api::{AppSettings, NowPlaying, Zone, ZonesResponse};
use crate::app::components::{ErrorAlert, Layout, VolumeControlsCompact};
use crate::app::sse::{use_sse, SseEvent};
use dioxus::prelude::*;
use std::collections::HashMap;

#[derive(Clone, serde::Serialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
}

async fn fetch_now_playing(zone_id: &str) -> Option<NowPlaying> {
    let url = format!("/now_playing?zone_id={}", urlencoding::encode(zone_id));
    crate::app::api::fetch_json::<NowPlaying>(&url).await.ok()
}

async fn fetch_all_now_playing(zones: &[Zone]) -> HashMap<String, NowPlaying> {
    let mut now_playing = HashMap::new();
    for zone in zones {
        if let Some(np) = fetch_now_playing(&zone.zone_id).await {
            now_playing.insert(zone.zone_id.clone(), np);
        }
    }
    now_playing
}

/// Spotify Connect device page, visible only when the Spotify adapter is enabled.
#[component]
pub fn Spotify() -> Element {
    let sse = use_sse();
    let mut zones = use_resource(|| async {
        crate::app::api::fetch_json::<ZonesResponse>("/zones")
            .await
            .ok()
            .map(|response| {
                response
                    .zones
                    .into_iter()
                    .filter(|zone| zone.zone_id.starts_with("spotify:"))
                    .collect::<Vec<_>>()
            })
    });
    let mut now_playing = use_signal(HashMap::<String, NowPlaying>::new);
    let settings = use_resource(|| async {
        crate::app::api::fetch_json::<AppSettings>("/api/settings")
            .await
            .ok()
    });
    let mut error = use_signal(|| None::<String>);
    let mut pending_zone = use_signal(|| None::<String>);
    let mut pending_track_change = use_signal(|| false);

    let zone_list = use_memo(move || zones.read().clone().flatten().unwrap_or_default());
    use_effect(move || {
        let list = zone_list();
        if !list.is_empty() {
            spawn(async move {
                now_playing.set(fetch_all_now_playing(&list).await);
            });
        }
    });

    use_effect(move || {
        let _ = (sse.event_count)();
        let event = (sse.last_event)();
        match event.as_ref() {
            Some(SseEvent::ZoneDiscovered { .. } | SseEvent::ZoneRemoved { .. }) => {
                zones.restart();
            }
            Some(SseEvent::ZoneUpdated { .. } | SseEvent::NowPlayingChanged { .. }) => {
                if let Some(zone_id) = event.as_ref().and_then(SseEvent::zone_id) {
                    let zone_id = zone_id.to_string();
                    // `.peek()`, not the tracked call: this effect is driven by
                    // sse.event_count/sse.last_event above, not by pending_zone or
                    // pending_track_change (which it also writes below). A tracked
                    // read here would subscribe the effect to its own write and
                    // cause an extra self-triggered run (reactive-loop-lint).
                    if *pending_zone.peek() == Some(zone_id.clone()) {
                        if matches!(event, Some(SseEvent::NowPlayingChanged { .. })) {
                            pending_zone.set(None);
                            pending_track_change.set(false);
                        } else if !*pending_track_change.peek() {
                            // Play/pause and stop are reported as a normal
                            // zone update; only track-changing commands need
                            // the longer metadata retry window below.
                            pending_zone.set(None);
                        }
                    }
                    spawn(async move {
                        if let Some(np) = fetch_now_playing(&zone_id).await {
                            now_playing.with_mut(|map| {
                                map.insert(zone_id, np);
                            });
                        }
                    });
                }
            }
            Some(SseEvent::VolumeChanged { payload })
                if payload.output_id.starts_with("spotify:") =>
            {
                let zone_id = payload.output_id.clone();
                // `.peek()`, not the tracked call: see the comment above -- this
                // effect must not subscribe to its own pending_zone write.
                if *pending_zone.peek() == Some(zone_id.clone()) {
                    pending_zone.set(None);
                    pending_track_change.set(false);
                }
                spawn(async move {
                    if let Some(np) = fetch_now_playing(&zone_id).await {
                        now_playing.with_mut(|map| {
                            map.insert(zone_id, np);
                        });
                    }
                });
            }
            _ => {}
        }
    });

    let control = move |(zone_id, action): (String, String)| {
        let mut error = error;
        let mut pending_zone = pending_zone;
        let mut pending_track_change = pending_track_change;
        let mut now_playing = now_playing;
        spawn(async move {
            pending_zone.set(Some(zone_id.clone()));
            let retry_track = matches!(action.as_str(), "next" | "previous");
            pending_track_change.set(retry_track);
            let previous_track = now_playing()
                .get(&zone_id)
                .map(|np| (np.line1.clone(), np.image_key.clone()));
            let request = ControlRequest {
                zone_id: zone_id.clone(),
                action,
                value: None,
            };
            if let Err(message) = crate::app::api::post_json_no_response("/control", &request).await
            {
                pending_zone.set(None);
                pending_track_change.set(false);
                error.set(Some(message));
            } else if retry_track {
                let retry_zone_id = zone_id.clone();
                spawn(async move {
                    for delay in [250, 500, 750, 1000, 1500, 2000] {
                        dioxus_sdk_time::sleep(std::time::Duration::from_millis(delay)).await;
                        if pending_zone().as_deref() != Some(retry_zone_id.as_str()) {
                            return;
                        }
                        if let Some(np) = fetch_now_playing(&retry_zone_id).await {
                            let changed = previous_track.as_ref()
                                != Some(&(np.line1.clone(), np.image_key.clone()));
                            now_playing.with_mut(|map| {
                                map.insert(retry_zone_id.clone(), np);
                            });
                            if changed {
                                pending_zone.set(None);
                                pending_track_change.set(false);
                                return;
                            }
                        }
                    }
                    pending_zone.set(None);
                    pending_track_change.set(false);
                });
            }
        });
    };

    let enabled = settings
        .read()
        .clone()
        .flatten()
        .map(|settings| settings.adapters.spotify);
    let list = zone_list();
    let np = now_playing();
    let active_device = list.iter().find_map(|zone| {
        np.get(&zone.zone_id)
            .filter(|now_playing| now_playing.is_playing)
            .map(|_| zone.zone_name.as_str())
    });

    rsx! {
        Layout {
            title: "Spotify".to_string(),
            nav_active: "spotify".to_string(),

            h1 { class: "text-2xl font-bold mb-2", "Spotify" }
            p { class: "text-secondary text-sm mb-6", "Spotify Connect devices controlled through UHC." }

            if let Some(message) = error() {
                ErrorAlert { message, on_dismiss: move |_| error.set(None) }
            }

            if enabled == Some(false) {
                div { class: "card p-6", "Spotify is disabled. ", a { class: "link", href: "/settings", "Enable it in Settings" }, " to discover devices." }
            } else if zones.read().is_none() {
                div { class: "card p-6", aria_busy: "true", "Loading Spotify devices…" }
            } else if list.is_empty() {
                div { class: "card p-6",
                    p { class: "font-medium", "No Spotify Connect devices found" }
                    p { class: "mt-1 text-sm text-secondary", "Start Spotify on a Connect-capable player; UHC will detect it automatically." }
                    button {
                        r#type: "button",
                        class: "btn btn-outline mt-4 min-h-11",
                        onclick: move |_| zones.restart(),
                        "Refresh devices"
                    }
                }
            } else {
                div { class: "card mb-4 flex flex-wrap items-center justify-between gap-3 p-4", role: "status", aria_live: "polite",
                    div {
                        p { class: "font-medium", "Spotify Connect is live" }
                        p { class: "mt-1 text-sm text-secondary",
                            "{list.len()} device(s)"
                            if let Some(device) = active_device { " · Playing on {device}" }
                        }
                    }
                    button {
                        r#type: "button",
                        class: "btn btn-outline min-h-11",
                        onclick: move |_| zones.restart(),
                        "Refresh devices"
                    }
                }
                div { class: "grid gap-4 grid-cols-1 md:grid-cols-2 lg:grid-cols-3",
                    for zone in list {
                        SpotifyDeviceCard {
                            key: "{zone.zone_id}",
                            zone: zone.clone(),
                            now_playing: np.get(&zone.zone_id).cloned(),
                            pending: pending_zone() == Some(zone.zone_id.clone()),
                            on_control: control,
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn SpotifyDeviceCard(
    zone: Zone,
    now_playing: Option<NowPlaying>,
    pending: bool,
    on_control: EventHandler<(String, String)>,
) -> Element {
    let zone_id = zone.zone_id.clone();
    let previous = zone_id.clone();
    let play_pause = zone_id.clone();
    let next = zone_id.clone();
    let volume_down = zone_id.clone();
    let volume_up = zone_id.clone();
    let is_playing = now_playing
        .as_ref()
        .map(|np| np.is_playing)
        .unwrap_or(false);
    let title = now_playing
        .as_ref()
        .and_then(|np| np.line1.as_deref())
        .filter(|title| *title != "Idle")
        .unwrap_or("Nothing playing");
    let artist = now_playing.as_ref().and_then(|np| np.line2.as_deref());
    let base_image_url = now_playing
        .as_ref()
        .and_then(|np| np.image_url.as_deref())
        .unwrap_or_default();
    let image_url = if base_image_url.is_empty() {
        String::new()
    } else if let Some(key) = now_playing.as_ref().and_then(|np| np.image_key.as_deref()) {
        format!(
            "{base_image_url}{}k={key}",
            if base_image_url.contains('?') {
                '&'
            } else {
                '?'
            }
        )
    } else {
        base_image_url.to_string()
    };
    // #581: map the origin-absolute art path onto the runtime base path so
    // it survives an ingress prefix (identity in direct mode).
    let image_url = crate::app::base_path::href(&image_url);

    rsx! {
        article { class: "zone-card",
            div { class: "flex gap-3 items-start overflow-hidden",
                if !image_url.is_empty() {
                    img { src: "{image_url}", alt: "Album art", class: "w-16 h-16 rounded-lg object-cover bg-elevated" }
                } else {
                    div { class: "w-16 h-16 rounded-lg bg-elevated flex items-center justify-center text-2xl text-muted", "♪" }
                }
                div { class: "min-w-0 flex-1",
                    h2 { class: "font-semibold truncate", "{zone.zone_name}" }
                    p { class: "text-sm font-medium truncate mt-2", "{title}" }
                    if let Some(artist) = artist {
                        p { class: "text-sm text-secondary truncate", "{artist}" }
                    }
                }
            }
            div { class: "flex flex-wrap items-center gap-2 mt-4",
                button { class: "btn btn-ghost", disabled: pending, aria_label: "Previous track", onclick: move |_| on_control.call((previous.clone(), "previous".to_string())), "◀◀" }
                button { class: "btn btn-primary", disabled: pending, aria_busy: pending, aria_label: if is_playing { "Pause" } else { "Play" }, onclick: move |_| on_control.call((play_pause.clone(), "play_pause".to_string())), if is_playing { "⏸" } else { "▶" } }
                button { class: "btn btn-ghost", disabled: pending, aria_label: "Next track", onclick: move |_| on_control.call((next.clone(), "next".to_string())), "▶▶" }
                VolumeControlsCompact {
                    volume: now_playing.as_ref().and_then(|np| np.volume),
                    volume_type: now_playing.as_ref().and_then(|np| np.volume_type.clone()),
                    volume_step: now_playing.as_ref().and_then(|np| np.volume_step),
                    on_vol_down: move |_| on_control.call((volume_down.clone(), "vol_down".to_string())),
                    on_vol_up: move |_| on_control.call((volume_up.clone(), "vol_up".to_string())),
                }
            }
            if pending {
                p { class: "mt-2 text-sm text-secondary", role: "status", aria_live: "polite", "Updating playback…" }
            }
        }
    }
}
