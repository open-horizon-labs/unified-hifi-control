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
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
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

/// Fetch now playing for a single zone by ID
async fn fetch_zone_now_playing(zone_id: &str) -> Option<NowPlaying> {
    let url = format!("/now_playing?zone_id={}", urlencoding::encode(zone_id));
    crate::app::api::fetch_json::<NowPlaying>(&url).await.ok()
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
            Some(
                SseEvent::ZoneUpdated { .. }
                    | SseEvent::ZoneRemoved { .. }
                    | SseEvent::RoonConnected
                    | SseEvent::RoonDisconnected
                    | SseEvent::LmsConnected
                    | SseEvent::LmsDisconnected
            )
        ) {
            zones.restart();
        }

        // Refresh now_playing on playback/volume changes
        // Use selective per-zone fetching to avoid race conditions when multiple
        // zones update rapidly (fixes issue #109 - wrong album art display)
        if let Some(ref evt) = event {
            match evt {
                // Zone-scoped events: fetch only the specific zone that changed
                // ZoneUpdated includes state changes (play/pause) that affect is_playing
                SseEvent::NowPlayingChanged { .. } | SseEvent::ZoneUpdated { .. } => {
                    if let Some(zone_id) = evt.zone_id() {
                        let zone_id = zone_id.to_string();
                        spawn(async move {
                            if let Some(np) = fetch_zone_now_playing(&zone_id).await {
                                now_playing.with_mut(|map| {
                                    map.insert(zone_id, np);
                                });
                            }
                        });
                    }
                }
                // Volume/LMS events: fetch all zones and merge atomically
                // (output_id/player_id don't map directly to zone_id)
                SseEvent::VolumeChanged { .. } | SseEvent::LmsPlayerStateChanged { .. } => {
                    let zone_list = zones_list_signal();
                    if !zone_list.is_empty() {
                        spawn(async move {
                            let np_map = fetch_all_now_playing(&zone_list).await;
                            now_playing.with_mut(|map| {
                                for (k, v) in np_map {
                                    map.insert(k, v);
                                }
                            });
                        });
                    }
                }
                // SeekPositionChanged doesn't affect track/artist/album art - no fetch needed
                _ => {}
            }
        }
    });

    // Control handler
    let control = move |(zone_id, action): (String, String)| {
        spawn(async move {
            let req = ControlRequest {
                zone_id,
                action,
                value: None,
            };
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

    // Group zones by source protocol
    let grouped_zones: Vec<(String, Vec<Zone>)> = {
        let mut groups: std::collections::HashMap<String, Vec<Zone>> =
            std::collections::HashMap::new();
        for zone in zones_list.iter() {
            let source = zone.source.clone().unwrap_or_else(|| "Other".to_string());
            groups.entry(source).or_default().push(zone.clone());
        }
        // Sort zones within each group by name for stable ordering
        for zones in groups.values_mut() {
            zones.sort_by(|a, b| a.zone_name.cmp(&b.zone_name));
        }
        // Sort groups in a sensible order: Roon, LMS, OpenHome, UPnP, then others
        let priority = |s: &str| -> i32 {
            match s.to_lowercase().as_str() {
                "roon" => 0,
                "lms" => 1,
                "openhome" => 2,
                "upnp" => 3,
                _ => 4,
            }
        };
        let mut result: Vec<_> = groups.into_iter().collect();
        result.sort_by(|a, b| priority(&a.0).cmp(&priority(&b.0)));
        result
    };

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
            for (source, group_zones) in grouped_zones {
                div { class: "mb-8",
                    h3 { class: "text-lg font-semibold mb-4 text-muted", "{source}" }
                    div { class: "grid gap-4 grid-cols-1 md:grid-cols-2 lg:grid-cols-3",
                        for zone in group_zones {
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

    let has_hqp = zone
        .dsp
        .as_ref()
        .map(|d| d.r#type.as_deref() == Some("hqplayer"))
        .unwrap_or(false);

    // Extract volume info for component
    let volume = np.and_then(|n| n.volume);
    let volume_type = np.and_then(|n| n.volume_type.clone());
    let volume_step = np.and_then(|n| n.volume_step);

    // Album art URL with cache-busting image_key
    let base_image_url = np.and_then(|n| n.image_url.clone()).unwrap_or_default();
    let image_key = np.and_then(|n| n.image_key.clone());
    let image_url = if let Some(key) = image_key {
        let sep = if base_image_url.contains('?') {
            "&"
        } else {
            "?"
        };
        format!("{}{}k={}", base_image_url, sep, key)
    } else {
        base_image_url
    };
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
            div { class: "flex gap-3 sm:gap-5 items-start overflow-hidden",
                // Album art (smaller on mobile, 96px on larger screens)
                if has_image {
                    img {
                        src: "{image_url}",
                        alt: "Album art",
                        class: "w-16 h-16 sm:w-24 sm:h-24 object-cover rounded-lg bg-elevated flex-shrink-0"
                    }
                } else {
                    div { class: "w-16 h-16 sm:w-24 sm:h-24 rounded-lg bg-elevated flex items-center justify-center text-muted text-2xl sm:text-3xl flex-shrink-0",
                        "♪"
                    }
                }

                // Zone info
                div { class: "flex-1 min-w-0",
                    // Header with zone name and HQP badge
                    h3 { class: "flex items-center gap-2 mb-2 text-base font-semibold",
                        span { class: "truncate", "{zone.zone_name}" }
                        if has_hqp {
                            span { class: "badge badge-primary", "HQP" }
                        }
                    }

                    // Now playing info
                    if !track.is_empty() {
                        p { class: "font-medium text-sm truncate mb-1", "{track}" }
                        p { class: "text-sm text-muted truncate", "{artist}" }
                    } else {
                        p { class: "text-sm text-muted", "Nothing playing" }
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
            div { class: "flex flex-wrap items-center gap-2 mt-4",
                button {
                    class: "btn btn-ghost",
                    "aria-label": "Previous track",
                    onclick: move |_| on_control.call((zone_id_prev.clone(), "previous".to_string())),
                    svg { class: "w-5 h-5", fill: "currentColor", view_box: "0 0 24 24",
                        path { d: "M6 6h2v12H6zm3.5 6l8.5 6V6z" }
                    }
                }
                button {
                    class: "btn btn-primary",
                    "aria-label": if is_playing { "Pause" } else { "Play" },
                    onclick: move |_| on_control.call((zone_id_play.clone(), "play_pause".to_string())),
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
                    "aria-label": "Next track",
                    onclick: move |_| on_control.call((zone_id_next.clone(), "next".to_string())),
                    svg { class: "w-5 h-5", fill: "currentColor", view_box: "0 0 24 24",
                        path { d: "M6 18l8.5-6L6 6v12zM16 6v12h2V6h-2z" }
                    }
                }

                VolumeControlsCompact {
                    volume: volume,
                    volume_type: volume_type,
                    volume_step: volume_step,
                    on_vol_down: move |_| on_control.call((zone_id_vol_down.clone(), "vol_down".to_string())),
                    on_vol_up: move |_| on_control.call((zone_id_vol_up.clone(), "vol_up".to_string())),
                }
            }
        }
    }
}
