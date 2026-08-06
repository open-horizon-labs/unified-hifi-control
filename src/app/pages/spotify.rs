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

    let zone_list = use_memo(move || zones.read().clone().flatten().unwrap_or_default());
    use_effect(move || {
        let list = zone_list();
        if !list.is_empty() {
            spawn(async move {
                let mut next = HashMap::new();
                for zone in list {
                    if let Some(np) = fetch_now_playing(&zone.zone_id).await {
                        next.insert(zone.zone_id, np);
                    }
                }
                now_playing.set(next);
            });
        }
    });

    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if matches!(
            sse.last_event.read().as_ref(),
            Some(
                SseEvent::ZoneDiscovered { .. }
                    | SseEvent::ZoneUpdated { .. }
                    | SseEvent::ZoneRemoved { .. }
                    | SseEvent::NowPlayingChanged { .. }
                    | SseEvent::VolumeChanged { .. }
                    | SseEvent::AdapterError { .. }
                    | SseEvent::ProviderAccountUpdated { .. }
            )
        ) {
            zones.restart();
        }
    });

    let control = move |(zone_id, action): (String, String)| {
        let mut error = error;
        spawn(async move {
            let request = ControlRequest {
                zone_id,
                action,
                value: None,
            };
            if let Err(message) = crate::app::api::post_json_no_response("/control", &request).await
            {
                error.set(Some(message));
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

    rsx! {
        Layout {
            title: "Spotify".to_string(),
            nav_active: "spotify".to_string(),

            h1 { class: "text-2xl font-bold mb-2", "Spotify" }
            p { class: "text-muted text-sm mb-6", "Spotify Connect devices controlled through UHC." }

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
                    p { class: "mt-1 text-sm text-muted", "Start Spotify on a device, select it from Spotify Connect, then refresh this page." }
                }
            } else {
                div { class: "grid gap-4 grid-cols-1 md:grid-cols-2 lg:grid-cols-3",
                    for zone in list {
                        SpotifyDeviceCard {
                            key: "{zone.zone_id}",
                            zone: zone.clone(),
                            now_playing: np.get(&zone.zone_id).cloned(),
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
    let image_url = now_playing
        .as_ref()
        .and_then(|np| np.image_url.as_deref())
        .unwrap_or_default();

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
                        p { class: "text-sm text-muted truncate", "{artist}" }
                    }
                }
            }
            div { class: "flex flex-wrap items-center gap-2 mt-4",
                button { class: "btn btn-ghost", aria_label: "Previous track", onclick: move |_| on_control.call((previous.clone(), "previous".to_string())), "◀◀" }
                button { class: "btn btn-primary", aria_label: if is_playing { "Pause" } else { "Play" }, onclick: move |_| on_control.call((play_pause.clone(), "play_pause".to_string())), if is_playing { "⏸" } else { "▶" } }
                button { class: "btn btn-ghost", aria_label: "Next track", onclick: move |_| on_control.call((next.clone(), "next".to_string())), "▶▶" }
                VolumeControlsCompact {
                    volume: now_playing.as_ref().and_then(|np| np.volume),
                    volume_type: now_playing.as_ref().and_then(|np| np.volume_type.clone()),
                    volume_step: now_playing.as_ref().and_then(|np| np.volume_step),
                    on_vol_down: move |_| on_control.call((volume_down.clone(), "vol_down".to_string())),
                    on_vol_up: move |_| on_control.call((volume_up.clone(), "vol_up".to_string())),
                }
            }
        }
    }
}
