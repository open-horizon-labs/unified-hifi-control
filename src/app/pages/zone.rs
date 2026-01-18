//! Zone page component - single zone control view.
//!
//! Using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{HqpPipeline, NowPlaying, Zone as ZoneData, ZonesResponse};
use crate::app::components::Layout;
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
                            if let Ok(pipeline) =
                                crate::app::api::fetch_json::<HqpPipeline>("/hqp/pipeline").await
                            {
                                hqp_pipeline.set(Some(pipeline));
                            }
                        } else {
                            hqp_pipeline.set(None);
                        }
                    }
                }
            });
        } else {
            now_playing.set(None);
            hqp_pipeline.set(None);
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
        if sse.should_refresh_hqp() {
            if hqp_pipeline().is_some() {
                spawn(async move {
                    if let Ok(pipeline) =
                        crate::app::api::fetch_json::<HqpPipeline>("/hqp/pipeline").await
                    {
                        hqp_pipeline.set(Some(pipeline));
                    }
                });
            }
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
    let set_pipeline = move |(_setting, _value): (&'static str, &'static str)| {
        // TODO: Implement pipeline setting
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

            // Zone display (only shown when zone selected)
            if selected_zone.is_some() {
                ZoneDisplay {
                    zone: selected_zone.unwrap().clone(),
                    now_playing: np,
                    on_control: control,
                }
            }

            // HQPlayer section (only shown when zone has HQP)
            if has_hqp {
                HqpSection {
                    pipeline: hqp_pipeline(),
                    on_set_pipeline: set_pipeline,
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

    let volume_display = np
        .and_then(|n| {
            n.volume.map(|v| {
                let suffix = if n.volume_type.as_deref() == Some("db") {
                    " dB"
                } else {
                    ""
                };
                format!("{}{}", v.round() as i32, suffix)
            })
        })
        .unwrap_or_else(|| "—".to_string());

    let can_prev = np.map(|n| n.is_previous_allowed).unwrap_or(false);
    let can_next = np.map(|n| n.is_next_allowed).unwrap_or(false);

    rsx! {
        article { id: "zone-display",
            div { style: "display:flex;gap:1.5rem;align-items:flex-start;flex-wrap:wrap;",
                img {
                    id: "zone-art",
                    src: "{image_url}",
                    alt: "Album art",
                    style: "width:200px;height:200px;object-fit:cover;border-radius:8px;background:#222;"
                }
                div { style: "flex:1;min-width:200px;",
                    h2 { id: "zone-name", style: "margin-bottom:0.25rem;", "{zone.zone_name}" }
                    p { style: "margin:0;",
                        small { if is_playing { "playing" } else { "stopped" } }
                    }
                    hr {}
                    p { style: "margin:0.5rem 0;",
                        if !track.is_empty() {
                            strong { "{track}" }
                        } else {
                            strong { "Nothing playing" }
                        }
                    }
                    if !artist.is_empty() {
                        p { style: "margin:0;", small { "{artist}" } }
                    }
                    if !album.is_empty() {
                        p { style: "margin:0;color:var(--pico-muted-color);", small { "{album}" } }
                    }
                    hr {}
                    div { style: "display:flex;gap:0.5rem;align-items:center;margin:1rem 0;",
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
                        span { style: "margin-left:1rem;", "Volume: ", strong { "{volume_display}" } }
                        button {
                            style: "width:2.5rem;",
                            onclick: move |_| on_control.call(("vol_down", Some(2))),
                            "−"
                        }
                        button {
                            style: "width:2.5rem;",
                            onclick: move |_| on_control.call(("vol_up", Some(2))),
                            "+"
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
    on_set_pipeline: EventHandler<(&'static str, &'static str)>,
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

    // Helper to render a select
    let render_select = |id: &'static str,
                         label_text: &str,
                         opts: Option<&crate::app::api::HqpSettingOptions>,
                         _setting_name: &'static str| {
        let options = opts.map(|o| o.options.clone()).unwrap_or_default();
        let selected = opts
            .and_then(|o| o.selected.as_ref())
            .map(|s| s.value.clone())
            .unwrap_or_default();

        rsx! {
            label {
                "{label_text}"
                select {
                    id: "{id}",
                    onchange: move |_evt: Event<FormData>| {
                        // TODO: Implement pipeline change
                    },
                    for opt in options {
                        option {
                            value: "{opt.value}",
                            selected: opt.value == selected,
                            "{opt.label.as_deref().unwrap_or(&opt.value)}"
                        }
                    }
                }
            }
        }
    };

    rsx! {
        section { id: "hqp-section",
            hgroup {
                h2 { "HQPlayer DSP" }
                p { "Pipeline controls for zone-linked HQPlayer" }
            }
            article {
                div { class: "grid",
                    {render_select("hqp-mode", "Mode", settings.and_then(|s| s.mode.as_ref()), "mode")}
                    {render_select("hqp-samplerate", "Sample Rate", settings.and_then(|s| s.samplerate.as_ref()), "samplerate")}
                }
                div { class: "grid",
                    {render_select("hqp-filter1x", "Filter (1x)", settings.and_then(|s| s.filter1x.as_ref()), "filter1x")}
                    {render_select("hqp-filterNx", "Filter (Nx)", settings.and_then(|s| s.filter_nx.as_ref()), "filterNx")}
                }
                div { class: "grid",
                    {render_select("hqp-shaper", "Shaper", settings.and_then(|s| s.shaper.as_ref()), "shaper")}
                }
            }
        }
    }
}
