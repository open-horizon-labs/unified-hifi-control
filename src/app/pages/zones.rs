//! Zones listing page component.
//!
//! Shows all available zones using Dioxus resources.

use crate::app::api::{HqpMatrixProfilesResponse, HqpProfile, NowPlaying, Zone, ZonesResponse};
use crate::app::components::{ErrorAlert, HqpControlsCompact, Layout, VolumeControlsCompact};
use crate::app::sse::{use_sse, SseEvent};
use dioxus::prelude::*;
use std::collections::HashMap;

/// Control request body
#[derive(Clone, serde::Serialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
}

/// Fetch now playing for all zones
async fn fetch_all_now_playing(zones: &[Zone]) -> HashMap<String, NowPlaying> {
    let mut np_map = HashMap::new();
    for zone in zones {
        let url = format!(
            "/now_playing?zone_id={}",
            urlencoding::encode(&zone.zone_id)
        );
        if let Ok(np) = crate::app::api::fetch_json::<NowPlaying>(&url).await {
            np_map.insert(zone.zone_id.clone(), np);
        }
    }
    np_map
}

/// Zones listing page component.
#[component]
pub fn Zones() -> Element {
    let sse = use_sse();

    // Load zones resource
    let mut zones = use_resource(|| async {
        crate::app::api::fetch_json::<ZonesResponse>("/zones")
            .await
            .ok()
    });

    // Now playing state (populated after zones load and refreshed on SSE events)
    let mut now_playing = use_signal(HashMap::<String, NowPlaying>::new);

    // Track zones list for now_playing refresh
    let zones_list_signal = use_memo(move || {
        zones
            .read()
            .clone()
            .flatten()
            .map(|r| r.zones)
            .unwrap_or_default()
    });

    // Load now playing for each zone when zones change
    use_effect(move || {
        let zone_list = zones_list_signal();
        if !zone_list.is_empty() {
            spawn(async move {
                let np_map = fetch_all_now_playing(&zone_list).await;
                now_playing.set(np_map);
            });
        }
    });

    // Refresh on SSE events
    use_effect(move || {
        let _ = (sse.event_count)();
        let event = (sse.last_event)();

        // Refresh zones list on structural changes
        if matches!(
            event.as_ref(),
            Some(SseEvent::ZoneUpdated { .. })
                | Some(SseEvent::ZoneRemoved { .. })
                | Some(SseEvent::RoonConnected)
                | Some(SseEvent::RoonDisconnected)
                | Some(SseEvent::LmsConnected)
                | Some(SseEvent::LmsDisconnected)
        ) {
            zones.restart();
        }

        // Refresh now_playing on playback/volume changes (without reloading zones)
        if matches!(
            event.as_ref(),
            Some(SseEvent::NowPlayingChanged { .. })
                | Some(SseEvent::VolumeChanged { .. })
                | Some(SseEvent::LmsPlayerStateChanged { .. })
        ) {
            let zone_list = zones_list_signal();
            if !zone_list.is_empty() {
                spawn(async move {
                    let np_map = fetch_all_now_playing(&zone_list).await;
                    now_playing.set(np_map);
                });
            }
        }
    });

    // Control handler
    let control = move |(zone_id, action): (String, String)| {
        spawn(async move {
            let req = ControlRequest { zone_id, action };
            if let Err(e) = crate::app::api::post_json_no_response("/control", &req).await {
                #[cfg(target_arch = "wasm32")]
                web_sys::console::warn_1(&format!("Control request failed: {e}").into());
                #[cfg(not(target_arch = "wasm32"))]
                tracing::warn!("Control request failed: {e}");
            }
        });
    };

    // HQPlayer state (shared across all HQP zones)
    let mut hqp_profiles = use_signal(Vec::<HqpProfile>::new);
    let mut hqp_matrix = use_signal(|| None::<HqpMatrixProfilesResponse>);
    let mut hqp_error = use_signal(|| None::<String>);

    // Check if any zone has HQP
    let has_any_hqp = use_memo(move || {
        zones_list_signal().iter().any(|z| {
            z.dsp
                .as_ref()
                .map(|d| d.r#type.as_deref() == Some("hqplayer"))
                .unwrap_or(false)
        })
    });

    // Fetch HQP profiles/matrix when there are HQP zones
    use_effect(move || {
        if has_any_hqp() {
            spawn(async move {
                if let Ok(profiles) =
                    crate::app::api::fetch_json::<Vec<HqpProfile>>("/hqplayer/profiles").await
                {
                    hqp_profiles.set(profiles);
                }
                if let Ok(matrix) = crate::app::api::fetch_json::<HqpMatrixProfilesResponse>(
                    "/hqplayer/matrix/profiles",
                )
                .await
                {
                    hqp_matrix.set(Some(matrix));
                }
            });
        }
    });

    // Load profile handler
    let load_profile = move |profile: String| {
        hqp_error.set(None);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct ProfileRequest {
                profile: String,
            }
            let req = ProfileRequest { profile };
            if let Err(e) = crate::app::api::post_json_no_response("/hqplayer/profile", &req).await
            {
                hqp_error.set(Some(format!("Profile load failed: {e}")));
            }
        });
    };

    // Set matrix profile handler
    let set_matrix = move |profile_idx: u32| {
        hqp_error.set(None);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct MatrixRequest {
                profile: u32,
            }
            let req = MatrixRequest {
                profile: profile_idx,
            };
            match crate::app::api::post_json_no_response("/hqplayer/matrix/profile", &req).await {
                Ok(_) => {
                    // Refresh matrix after change
                    if let Ok(matrix) = crate::app::api::fetch_json::<HqpMatrixProfilesResponse>(
                        "/hqplayer/matrix/profiles",
                    )
                    .await
                    {
                        hqp_matrix.set(Some(matrix));
                    }
                }
                Err(e) => {
                    hqp_error.set(Some(format!("Matrix profile failed: {e}")));
                }
            }
        });
    };

    let is_loading = zones.read().is_none();
    let zones_list = zones
        .read()
        .clone()
        .flatten()
        .map(|r| r.zones)
        .unwrap_or_default();
    let np_map = now_playing();

    let profiles = hqp_profiles();
    let matrix = hqp_matrix();

    let content = if is_loading {
        rsx! {
            div { class: "card p-6", aria_busy: "true", "Loading zones..." }
        }
    } else if zones_list.is_empty() {
        rsx! {
            div { class: "card p-6", "No zones available. Check that adapters are connected." }
        }
    } else {
        rsx! {
            div { class: "zone-grid",
                for zone in zones_list {
                    ZoneCard {
                        key: "{zone.zone_id}",
                        zone: zone.clone(),
                        now_playing: np_map.get(&zone.zone_id).cloned(),
                        hqp_profiles: profiles.clone(),
                        hqp_matrix: matrix.clone(),
                        on_control: control,
                        on_load_profile: load_profile,
                        on_set_matrix: set_matrix,
                    }
                }
            }
        }
    };

    rsx! {
        Layout {
            title: "Zones".to_string(),
            nav_active: "zones".to_string(),

            h1 { class: "text-2xl font-bold mb-6", "Zones" }

            // HQP error display
            if let Some(error) = hqp_error() {
                ErrorAlert {
                    message: error,
                    on_dismiss: move |_| hqp_error.set(None),
                }
            }

            section { id: "zones",
                {content}
            }
        }
    }
}

/// Zone card component
#[component]
fn ZoneCard(
    zone: Zone,
    now_playing: Option<NowPlaying>,
    hqp_profiles: Vec<HqpProfile>,
    hqp_matrix: Option<HqpMatrixProfilesResponse>,
    on_control: EventHandler<(String, String)>,
    on_load_profile: EventHandler<String>,
    on_set_matrix: EventHandler<u32>,
) -> Element {
    let zone_id = zone.zone_id.clone();
    let zone_id_prev = zone_id.clone();
    let zone_id_play = zone_id.clone();
    let zone_id_next = zone_id.clone();
    let zone_id_vol_down = zone_id.clone();
    let zone_id_vol_up = zone_id.clone();

    let np = now_playing.as_ref();
    let is_playing = np.map(|n| n.is_playing).unwrap_or(false);
    let play_icon = if is_playing { "⏸︎" } else { "▶" };

    let has_hqp = zone
        .dsp
        .as_ref()
        .map(|d| d.r#type.as_deref() == Some("hqplayer"))
        .unwrap_or(false);

    // Extract volume info for component
    let volume = np.and_then(|n| n.volume);
    let volume_type = np.and_then(|n| n.volume_type.clone());

    // Album art URL
    let image_url = np.and_then(|n| n.image_url.clone()).unwrap_or_default();
    let has_image = !image_url.is_empty();

    // Now playing display
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

    // HQP matrix info
    let has_matrix = hqp_matrix
        .as_ref()
        .map(|m| !m.profiles.is_empty())
        .unwrap_or(false);
    let matrix_profiles = hqp_matrix
        .as_ref()
        .map(|m| m.profiles.clone())
        .unwrap_or_default();
    let matrix_current = hqp_matrix.as_ref().and_then(|m| m.current);

    rsx! {
        article { class: "zone-card",
            // Main content with album art and info (same layout as zone detail)
            div { class: "flex gap-4 items-start",
                // Album art
                if has_image {
                    img {
                        src: "{image_url}",
                        alt: "Album art",
                        class: "w-20 h-20 object-cover rounded-lg bg-elevated flex-shrink-0"
                    }
                } else {
                    div { class: "w-20 h-20 rounded-lg bg-elevated flex items-center justify-center text-muted text-2xl flex-shrink-0",
                        "♪"
                    }
                }

                // Zone info
                div { class: "flex-1 min-w-0",
                    // Header with zone name and badges
                    h3 { class: "flex items-center gap-2 mb-1",
                        span { class: "truncate", "{zone.zone_name}" }
                        if has_hqp {
                            span { class: "badge badge-primary", "HQP" }
                        }
                        if let Some(ref source) = zone.source {
                            span { class: "badge badge-secondary", "{source}" }
                        }
                    }
                    p { class: "text-sm text-muted mb-2",
                        if is_playing { "playing" } else { "stopped" }
                    }

                    // Now playing info
                    if !track.is_empty() {
                        p { class: "font-medium text-sm truncate mb-0", "{track}" }
                        p { class: "text-sm text-muted truncate mb-0", "{artist}" }
                    } else {
                        p { class: "text-sm text-muted mb-0", "Nothing playing" }
                    }
                }
            }

            // HQP controls (for HQP zones only)
            if has_hqp && (!hqp_profiles.is_empty() || has_matrix) {
                HqpControlsCompact {
                    profiles: hqp_profiles,
                    matrix_profiles: matrix_profiles,
                    active_matrix: matrix_current,
                    on_profile_select: on_load_profile,
                    on_matrix_select: on_set_matrix,
                }
            }

            // Transport controls
            div { class: "flex items-center gap-2 mt-4",
                button {
                    class: "btn btn-ghost",
                    onclick: move |_| on_control.call((zone_id_prev.clone(), "previous".to_string())),
                    "◀◀"
                }
                button {
                    class: "btn btn-primary",
                    onclick: move |_| on_control.call((zone_id_play.clone(), "play_pause".to_string())),
                    "{play_icon}"
                }
                button {
                    class: "btn btn-ghost",
                    onclick: move |_| on_control.call((zone_id_next.clone(), "next".to_string())),
                    "▶▶"
                }

                VolumeControlsCompact {
                    volume: volume,
                    volume_type: volume_type,
                    on_vol_down: move |_| on_control.call((zone_id_vol_down.clone(), "vol_down".to_string())),
                    on_vol_up: move |_| on_control.call((zone_id_vol_up.clone(), "vol_up".to_string())),
                }
            }
        }
    }
}
