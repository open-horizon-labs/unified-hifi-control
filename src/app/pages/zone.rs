//! Zone page component - single zone control view.
//!
//! Using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{
    HqpMatrixProfilesResponse, HqpPipeline, HqpProfile, HqpStatus, NowPlaying, Zone as ZoneData,
    ZonesResponse,
};
use crate::app::components::{ErrorAlert, HqpMatrixSelect, HqpProfileSelect, Layout};
use crate::app::sse::use_sse;

/// Control request body
#[derive(Clone, serde::Serialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<i32>,
}

/// Pipeline setting request
#[derive(Clone, serde::Serialize)]
#[allow(dead_code)]
struct PipelineRequest {
    setting: String,
    value: String,
}

/// Zone page component.
#[component]
pub fn Zone() -> Element {
    let sse = use_sse();

    // Selected zone ID
    let mut selected_zone_id = use_signal(|| None::<String>);

    // Load zones resource
    let mut zones = use_resource(|| async {
        crate::app::api::fetch_json::<ZonesResponse>("/zones")
            .await
            .ok()
    });

    // Now playing (depends on selected zone)
    let mut now_playing = use_signal(|| None::<NowPlaying>);

    // HQPlayer pipeline (depends on selected zone having HQP)
    let mut hqp_pipeline = use_signal(|| None::<HqpPipeline>);

    // HQPlayer profiles
    let mut hqp_profiles = use_signal(Vec::<HqpProfile>::new);
    let mut hqp_matrix = use_signal(|| None::<HqpMatrixProfilesResponse>);
    let mut hqp_error = use_signal(|| None::<String>);
    let mut hqp_status = use_signal(|| None::<HqpStatus>);
    let mut hqp_loading = use_signal(|| false);

    // Restore selected zone from localStorage on mount
    use_effect(move || {
        #[cfg(target_arch = "wasm32")]
        {
            if let Some(window) = web_sys::window() {
                if let Ok(Some(storage)) = window.local_storage() {
                    if let Ok(Some(saved_zone)) = storage.get_item("hifi-zone") {
                        selected_zone_id.set(Some(saved_zone));
                    }
                }
            }
        }
    });

    // Load now playing when zone changes
    use_effect(move || {
        if let Some(zone_id) = selected_zone_id() {
            let zones_data = zones.read().clone().flatten();
            spawn(async move {
                let url = format!("/now_playing?zone_id={}", urlencoding::encode(&zone_id));
                if let Ok(np) = crate::app::api::fetch_json::<NowPlaying>(&url).await {
                    now_playing.set(Some(np));
                }

                // Check if zone has HQP
                if let Some(ref resp) = zones_data {
                    if let Some(zone) = resp.zones.iter().find(|z| z.zone_id == zone_id) {
                        if zone
                            .dsp
                            .as_ref()
                            .map(|d| d.r#type.as_deref() == Some("hqplayer"))
                            .unwrap_or(false)
                        {
                            // Fetch HQP status
                            if let Ok(status) =
                                crate::app::api::fetch_json::<HqpStatus>("/hqplayer/status").await
                            {
                                hqp_status.set(Some(status));
                            }
                            // Fetch pipeline
                            if let Ok(pipeline) =
                                crate::app::api::fetch_json::<HqpPipeline>("/hqp/pipeline").await
                            {
                                hqp_pipeline.set(Some(pipeline));
                            }
                            // Fetch profiles
                            if let Ok(profiles) =
                                crate::app::api::fetch_json::<Vec<HqpProfile>>("/hqplayer/profiles")
                                    .await
                            {
                                hqp_profiles.set(profiles);
                            }
                            // Fetch matrix profiles
                            if let Ok(matrix) =
                                crate::app::api::fetch_json::<HqpMatrixProfilesResponse>(
                                    "/hqplayer/matrix/profiles",
                                )
                                .await
                            {
                                hqp_matrix.set(Some(matrix));
                            }
                        } else {
                            hqp_pipeline.set(None);
                            hqp_profiles.set(Vec::new());
                            hqp_matrix.set(None);
                            hqp_status.set(None);
                        }
                    }
                }
            });
        } else {
            now_playing.set(None);
            hqp_pipeline.set(None);
            hqp_profiles.set(Vec::new());
            hqp_matrix.set(None);
            hqp_status.set(None);
        }
    });

    // Refresh on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_zones() {
            zones.restart();
            if let Some(zone_id) = selected_zone_id() {
                spawn(async move {
                    let url = format!("/now_playing?zone_id={}", urlencoding::encode(&zone_id));
                    if let Ok(np) = crate::app::api::fetch_json::<NowPlaying>(&url).await {
                        now_playing.set(Some(np));
                    }
                });
            }
        }
        if sse.should_refresh_hqp() && hqp_pipeline().is_some() {
            spawn(async move {
                if let Ok(pipeline) =
                    crate::app::api::fetch_json::<HqpPipeline>("/hqp/pipeline").await
                {
                    hqp_pipeline.set(Some(pipeline));
                }
            });
        }
    });

    // Control handler
    let control = move |(action, value): (&'static str, Option<i32>)| {
        if let Some(zone_id) = selected_zone_id() {
            let action = action.to_string();
            spawn(async move {
                let req = ControlRequest {
                    zone_id,
                    action,
                    value,
                };
                let _ = crate::app::api::post_json_no_response("/control", &req).await;
            });
        }
    };

    // Pipeline setting handler
    let set_pipeline = move |(setting, value): (String, String)| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct PipelineRequest {
                setting: String,
                value: String,
            }
            let req = PipelineRequest { setting, value };
            if let Err(e) = crate::app::api::post_json_no_response("/hqp/pipeline", &req).await {
                hqp_error.set(Some(format!("Pipeline update failed: {e}")));
            } else {
                // Refresh pipeline after successful change
                if let Ok(pipeline) =
                    crate::app::api::fetch_json::<HqpPipeline>("/hqp/pipeline").await
                {
                    hqp_pipeline.set(Some(pipeline));
                }
            }
            hqp_loading.set(false);
        });
    };

    // Load profile handler
    let load_profile = move |profile: String| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct ProfileRequest {
                profile: String,
            }
            let req = ProfileRequest { profile };
            if let Err(e) = crate::app::api::post_json_no_response("/hqplayer/profile", &req).await
            {
                hqp_error.set(Some(format!("Profile load failed: {e}")));
            } else {
                // Refresh pipeline after profile load
                if let Ok(pipeline) =
                    crate::app::api::fetch_json::<HqpPipeline>("/hqp/pipeline").await
                {
                    hqp_pipeline.set(Some(pipeline));
                }
            }
            hqp_loading.set(false);
        });
    };

    // Set matrix profile handler
    let set_matrix_profile = move |profile_idx: u32| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct MatrixRequest {
                profile: u32,
            }
            let req = MatrixRequest {
                profile: profile_idx,
            };
            if let Err(e) =
                crate::app::api::post_json_no_response("/hqplayer/matrix/profile", &req).await
            {
                hqp_error.set(Some(format!("Matrix profile failed: {e}")));
            } else {
                // Refresh matrix after change
                if let Ok(matrix) = crate::app::api::fetch_json::<HqpMatrixProfilesResponse>(
                    "/hqplayer/matrix/profiles",
                )
                .await
                {
                    hqp_matrix.set(Some(matrix));
                }
            }
            hqp_loading.set(false);
        });
    };

    // Zone selection handler
    let on_zone_select = move |evt: Event<FormData>| {
        let value = evt.value();
        if value.is_empty() {
            selected_zone_id.set(None);
        } else {
            selected_zone_id.set(Some(value.clone()));

            // Save to localStorage
            #[cfg(target_arch = "wasm32")]
            {
                if let Some(window) = web_sys::window() {
                    if let Ok(Some(storage)) = window.local_storage() {
                        let _ = storage.set_item("hifi-zone", &value);
                    }
                }
            }
        }
    };

    let np = now_playing();
    let zones_list = zones
        .read()
        .clone()
        .flatten()
        .map(|r| r.zones)
        .unwrap_or_default();
    let selected = selected_zone_id();
    let selected_zone = selected
        .as_ref()
        .and_then(|id| zones_list.iter().find(|z| &z.zone_id == id));

    let has_hqp = selected_zone
        .and_then(|z| z.dsp.as_ref())
        .map(|d| d.r#type.as_deref() == Some("hqplayer"))
        .unwrap_or(false);

    rsx! {
        Layout {
            title: "Zone".to_string(),
            nav_active: "zone".to_string(),

            h1 { "Zone Control" }
            p { small { "Select a zone for focused listening and DSP control." } }

            // Zone selector
            label { r#for: "zone-select",
                "Zone"
                select {
                    id: "zone-select",
                    onchange: on_zone_select,
                    option { value: "", "-- Select Zone --" }
                    for zone in zones_list.iter() {
                        option {
                            value: "{zone.zone_id}",
                            selected: selected.as_ref() == Some(&zone.zone_id),
                            "{zone.zone_name}"
                            if zone.dsp.as_ref().map(|d| d.r#type.as_deref() == Some("hqplayer")).unwrap_or(false) {
                                " [HQP]"
                            }
                            if let Some(ref source) = zone.source {
                                " ({source})"
                            }
                        }
                    }
                }
            }

            // HQP error display
            if let Some(error) = hqp_error() {
                ErrorAlert {
                    message: error,
                    on_dismiss: move |_| hqp_error.set(None),
                }
            }

            // Zone display (only shown when zone selected)
            if let Some(zone) = selected_zone {
                ZoneDisplay {
                    zone: zone.clone(),
                    now_playing: np,
                    on_control: control,
                }
            }

            // HQPlayer section (only shown when zone has HQP)
            if has_hqp {
                HqpSection {
                    pipeline: hqp_pipeline(),
                    profiles: hqp_profiles(),
                    matrix: hqp_matrix(),
                    status: hqp_status(),
                    loading: hqp_loading(),
                    on_set_pipeline: set_pipeline,
                    on_load_profile: load_profile,
                    on_set_matrix: set_matrix_profile,
                }
            }
        }
    }
}

/// Zone display component
#[component]
fn ZoneDisplay(
    zone: ZoneData,
    now_playing: Option<NowPlaying>,
    on_control: EventHandler<(&'static str, Option<i32>)>,
) -> Element {
    let np = now_playing.as_ref();
    let is_playing = np.map(|n| n.is_playing).unwrap_or(false);
    let play_icon = if is_playing { "⏸︎" } else { "▶" };

    // Check if zone has HQPlayer DSP
    let has_hqp = zone
        .dsp
        .as_ref()
        .map(|d| d.r#type.as_deref() == Some("hqplayer"))
        .unwrap_or(false);

    let (track, artist, album) = np
        .map(|n| {
            if n.line1.as_deref().unwrap_or("Idle") != "Idle" {
                (
                    n.line1.clone().unwrap_or_default(),
                    n.line2.clone().unwrap_or_default(),
                    n.line3.clone().unwrap_or_default(),
                )
            } else {
                (String::new(), String::new(), String::new())
            }
        })
        .unwrap_or_default();

    let image_url = np.and_then(|n| n.image_url.clone()).unwrap_or_default();

    // Volume type handling:
    // - "db": show value with " dB" suffix
    // - "number": show value with no suffix (0-100 scale)
    // - "incremental": hide value, only show +/- buttons
    // - None (fixed volume): hide volume controls entirely
    let volume_type = np.and_then(|n| n.volume_type.clone());
    let has_volume = np.map(|n| n.volume.is_some()).unwrap_or(false);
    let is_incremental = volume_type.as_deref() == Some("incremental");

    let volume_display = if is_incremental {
        String::new()
    } else {
        np.and_then(|n| {
            n.volume.map(|v| {
                let suffix = if n.volume_type.as_deref() == Some("db") {
                    " dB"
                } else {
                    ""
                };
                format!("{}{}", v.round() as i32, suffix)
            })
        })
        .unwrap_or_else(|| "—".to_string())
    };

    let can_prev = np.map(|n| n.is_previous_allowed).unwrap_or(false);
    let can_next = np.map(|n| n.is_next_allowed).unwrap_or(false);

    rsx! {
        article { id: "zone-display",
            div { class: "flex gap-6 items-start flex-wrap",
                img {
                    id: "zone-art",
                    src: "{image_url}",
                    alt: "Album art",
                    class: "w-[200px] h-[200px] object-cover rounded-lg bg-elevated"
                }
                div { class: "flex-1 min-w-[200px]",
                    h2 { id: "zone-name", class: "mb-1 flex items-center gap-2",
                        "{zone.zone_name}"
                        if has_hqp {
                            span { class: "badge badge-primary text-sm", "HQP" }
                        }
                        if let Some(ref source) = zone.source {
                            span { class: "badge badge-secondary text-sm", "{source}" }
                        }
                    }
                    p { class: "m-0",
                        small { if is_playing { "playing" } else { "stopped" } }
                    }
                    hr {}
                    p { class: "my-2",
                        if !track.is_empty() {
                            strong { "{track}" }
                        } else {
                            strong { "Nothing playing" }
                        }
                    }
                    if !artist.is_empty() {
                        p { class: "m-0", small { "{artist}" } }
                    }
                    if !album.is_empty() {
                        p { class: "text-muted m-0", small { "{album}" } }
                    }
                    hr {}
                    div { class: "flex gap-2 items-center my-4",
                        button {
                            disabled: !can_prev,
                            onclick: move |_| on_control.call(("previous", None)),
                            "◀◀"
                        }
                        button {
                            onclick: move |_| on_control.call(("play_pause", None)),
                            "{play_icon}"
                        }
                        button {
                            disabled: !can_next,
                            onclick: move |_| on_control.call(("next", None)),
                            "▶▶"
                        }
                        // Volume controls (hidden for fixed volume outputs)
                        if has_volume || is_incremental {
                            if !is_incremental {
                                span { class: "ml-4", "Volume: ", strong { "{volume_display}" } }
                            } else {
                                span { class: "ml-4", "Volume:" }
                            }
                            button {
                                class: "w-10",
                                onclick: move |_| on_control.call(("vol_down", Some(2))),
                                "−"
                            }
                            button {
                                class: "w-10",
                                onclick: move |_| on_control.call(("vol_up", Some(2))),
                                "+"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// HQPlayer DSP section
#[component]
fn HqpSection(
    pipeline: Option<HqpPipeline>,
    profiles: Vec<HqpProfile>,
    matrix: Option<HqpMatrixProfilesResponse>,
    status: Option<HqpStatus>,
    loading: bool,
    on_set_pipeline: EventHandler<(String, String)>,
    on_load_profile: EventHandler<String>,
    on_set_matrix: EventHandler<u32>,
) -> Element {
    let Some(ref pipe) = pipeline else {
        return rsx! {
            section { id: "hqp-section",
                hgroup {
                    h2 { "HQPlayer DSP" }
                    p { "Pipeline controls for zone-linked HQPlayer" }
                }
                article { aria_busy: "true", "Loading DSP settings..." }
            }
        };
    };

    let settings = pipe.settings.as_ref();

    // Extract options for each setting
    let mode_opts = settings.and_then(|s| s.mode.clone());
    let samplerate_opts = settings.and_then(|s| s.samplerate.clone());
    let filter1x_opts = settings.and_then(|s| s.filter1x.clone());
    let filter_nx_opts = settings.and_then(|s| s.filter_nx.clone());
    let shaper_opts = settings.and_then(|s| s.shaper.clone());

    // Matrix profile info
    let has_matrix = matrix
        .as_ref()
        .map(|m| !m.profiles.is_empty())
        .unwrap_or(false);
    let matrix_profiles = matrix
        .as_ref()
        .map(|m| m.profiles.clone())
        .unwrap_or_default();
    let matrix_current = matrix.as_ref().and_then(|m| m.current);

    // Dynamic shaper label based on mode (SDM/DSD uses "Modulator", PCM uses "Dither")
    let shaper_label = mode_opts
        .as_ref()
        .and_then(|m| m.selected.as_ref())
        .and_then(|s| s.label.as_ref())
        .map(|label| {
            let lower = label.to_lowercase();
            if lower.contains("sdm") || lower.contains("dsd") {
                "Modulator"
            } else {
                "Dither"
            }
        })
        .unwrap_or("Shaper");

    rsx! {
        section { id: "hqp-section",
            hgroup {
                h2 { "HQPlayer DSP" }
                p { class: "flex items-center gap-2",
                    "Pipeline controls for zone-linked HQPlayer"
                    // Status indicator
                    if let Some(ref st) = status {
                        if st.connected {
                            span { class: "status-ok text-sm", "✓ Connected" }
                        } else {
                            span { class: "status-err text-sm", "✗ Disconnected" }
                        }
                    }
                    // Loading indicator
                    if loading {
                        span { class: "text-muted text-sm ml-2", aria_busy: "true", "Updating..." }
                    }
                }
            }
            article {
                // Profile and Matrix selectors side by side
                if !profiles.is_empty() || has_matrix {
                    div { class: "grid grid-cols-2 gap-4 mb-4",
                        if !profiles.is_empty() {
                            label {
                                "Profile"
                                HqpProfileSelect {
                                    profiles: profiles.clone(),
                                    on_select: on_load_profile,
                                    disabled: loading,
                                }
                            }
                        }
                        if has_matrix {
                            label {
                                "Matrix Profile"
                                HqpMatrixSelect {
                                    profiles: matrix_profiles,
                                    active: matrix_current,
                                    on_select: on_set_matrix,
                                    disabled: loading,
                                }
                            }
                        }
                    }
                }

                // Pipeline settings in a 2-column grid
                div { class: "grid grid-cols-2 gap-4",
                    HqpSelect {
                        id: "hqp-mode",
                        label: "Mode",
                        setting: "mode",
                        options: mode_opts,
                        disabled: loading,
                        on_change: on_set_pipeline,
                    }
                    HqpSelect {
                        id: "hqp-samplerate",
                        label: "Sample Rate",
                        setting: "samplerate",
                        options: samplerate_opts,
                        disabled: loading,
                        on_change: on_set_pipeline,
                    }
                    HqpSelect {
                        id: "hqp-filter1x",
                        label: "Filter (1x)",
                        setting: "filter1x",
                        options: filter1x_opts,
                        disabled: loading,
                        on_change: on_set_pipeline,
                    }
                    HqpSelect {
                        id: "hqp-filterNx",
                        label: "Filter (Nx)",
                        setting: "filterNx",
                        options: filter_nx_opts,
                        disabled: loading,
                        on_change: on_set_pipeline,
                    }
                    HqpSelect {
                        id: "hqp-shaper",
                        label: shaper_label,
                        setting: "shaper",
                        options: shaper_opts,
                        disabled: loading,
                        on_change: on_set_pipeline,
                    }
                }
            }
        }
    }
}

/// HQPlayer setting select component
#[component]
fn HqpSelect(
    id: &'static str,
    label: &'static str,
    setting: &'static str,
    options: Option<crate::app::api::HqpSettingOptions>,
    #[props(default = false)] disabled: bool,
    on_change: EventHandler<(String, String)>,
) -> Element {
    let opts_list = options
        .as_ref()
        .map(|o| o.options.clone())
        .unwrap_or_default();
    let selected = options
        .as_ref()
        .and_then(|o| o.selected.as_ref())
        .map(|s| s.value.clone())
        .unwrap_or_default();
    let setting_name = setting.to_string();

    rsx! {
        label {
            span { class: "block text-sm font-medium mb-1", "{label}" }
            select {
                id: "{id}",
                class: "input",
                disabled: disabled,
                onchange: move |evt: Event<FormData>| {
                    let value = evt.value();
                    on_change.call((setting_name.clone(), value));
                },
                for opt in opts_list {
                    option {
                        value: "{opt.value}",
                        selected: opt.value == selected,
                        "{opt.label.as_deref().unwrap_or(&opt.value)}"
                    }
                }
            }
        }
    }
}
