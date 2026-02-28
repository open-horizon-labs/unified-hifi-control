//! Configurator page — visual manifest editor with live knob preview.
//!
//! Free users: form-based element list + encoder mapping.
//! Paid users: natural language text input → LLM generates manifest.
//! Both: live preview of what the knob screen will look like.

use dioxus::prelude::*;
use std::collections::HashMap;

use crate::app::api;
use crate::app::components::Layout;
use crate::app::sse::use_sse;
use crate::app::Route;

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct ManifestResponse {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    sha: String,
    #[serde(default)]
    screens: Vec<ScreenDef>,
    #[serde(default)]
    nav: NavDef,
    #[serde(default)]
    interactions: Option<HashMap<String, String>>,
    #[serde(default)]
    fast: Option<FastState>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct ScreenDef {
    #[serde(default, rename = "type")]
    screen_type: String,
    #[serde(default)]
    id: String,
    #[serde(default)]
    lines: Vec<LineDef>,
    #[serde(default)]
    controls: Option<Vec<String>>, // v1 deprecated
    #[serde(default)]
    elements: Option<Vec<ElementDef>>, // v2 command-pattern
    #[serde(default)]
    encoder: Option<EncoderDef>, // v2 per-screen encoder
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    items: Option<Vec<serde_json::Value>>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct ElementDef {
    display: DisplayDef,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_tap: Option<ActionDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    on_long_press: Option<ActionDef>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct DisplayDef {
    #[serde(skip_serializing_if = "Option::is_none")]
    icon: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<bool>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct ActionDef {
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct EncoderDef {
    cw: ActionDef,
    ccw: ActionDef,
    #[serde(skip_serializing_if = "Option::is_none")]
    press: Option<ActionDef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    long_press: Option<ActionDef>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct LineDef {
    #[serde(default)]
    text: String,
    #[serde(default)]
    style: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct NavDef {
    #[serde(default)]
    order: Vec<String>,
    #[serde(default)]
    default: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct FastState {
    #[serde(default)]
    zone_id: String,
    #[serde(default)]
    is_playing: bool,
    #[serde(default)]
    volume: Option<f64>,
    #[serde(default)]
    volume_min: Option<f64>,
    #[serde(default)]
    volume_max: Option<f64>,
    #[serde(default)]
    transport: Option<TransportState>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct TransportState {
    #[serde(default)]
    play: bool,
    #[serde(default)]
    pause: bool,
    #[serde(default)]
    next: bool,
    #[serde(default)]
    prev: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
struct GenerateRequest {
    prompt: String,
    zone_id: String,
    #[serde(default = "default_device_type")]
    device_type: String,
}

fn default_device_type() -> String {
    "knob".to_string()
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct GenerateResponse {
    #[serde(default)]
    status: String,
    #[serde(default)]
    error: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct ZonesResponse {
    #[serde(default)]
    zones: Vec<ZoneInfo>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct ZoneInfo {
    #[serde(default)]
    zone_id: String,
    #[serde(default)]
    zone_name: String,
    #[serde(default)]
    state: String,
}

/// License status from GET /api/config/license
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
struct LicenseStatus {
    configured: bool,
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Convert v1 controls list to command-pattern elements for the editor.
fn controls_to_elements(controls: &[String]) -> Vec<ElementDef> {
    controls
        .iter()
        .filter_map(|c| match c.as_str() {
            "prev" => Some(ElementDef {
                display: DisplayDef {
                    icon: Some("skip_previous".into()),
                    label: None,
                    active: None,
                },
                on_tap: Some(ActionDef {
                    action: "previous".into(),
                    params: None,
                }),
                on_long_press: None,
            }),
            "play" => Some(ElementDef {
                display: DisplayDef {
                    icon: Some("play_arrow".into()),
                    label: None,
                    active: None,
                },
                on_tap: Some(ActionDef {
                    action: "toggle_playback".into(),
                    params: None,
                }),
                on_long_press: Some(ActionDef {
                    action: "stop".into(),
                    params: None,
                }),
            }),
            "next" => Some(ElementDef {
                display: DisplayDef {
                    icon: Some("skip_next".into()),
                    label: None,
                    active: None,
                },
                on_tap: Some(ActionDef {
                    action: "next".into(),
                    params: None,
                }),
                on_long_press: None,
            }),
            "mute" => Some(ElementDef {
                display: DisplayDef {
                    icon: Some("volume_off".into()),
                    label: None,
                    active: None,
                },
                on_tap: Some(ActionDef {
                    action: "toggle_mute".into(),
                    params: None,
                }),
                on_long_press: None,
            }),
            _ => None,
        })
        .collect()
}

/// Convert command-pattern elements back to v1 controls for backward compat.
fn elements_to_controls(elements: &[ElementDef]) -> Vec<String> {
    elements
        .iter()
        .filter_map(|e| match e.on_tap.as_ref()?.action.as_str() {
            "previous" => Some("prev".into()),
            "toggle_playback" | "play" | "pause" => Some("play".into()),
            "next" => Some("next".into()),
            "toggle_mute" | "mute" => Some("mute".into()),
            _ => None,
        })
        .collect()
}

fn default_encoder() -> EncoderDef {
    EncoderDef {
        cw: ActionDef {
            action: "volume_up".into(),
            params: None,
        },
        ccw: ActionDef {
            action: "volume_down".into(),
            params: None,
        },
        press: Some(ActionDef {
            action: "toggle_playback".into(),
            params: None,
        }),
        long_press: Some(ActionDef {
            action: "show_zone_picker".into(),
            params: None,
        }),
    }
}

// ── Page Component ───────────────────────────────────────────────────────────

#[component]
pub fn Configurator() -> Element {
    let sse = use_sse();

    // License gate — configurator requires a valid license
    let license = use_resource(|| async {
        api::fetch_json::<LicenseStatus>("/api/config/license")
            .await
            .ok()
    });
    let is_licensed = license
        .read()
        .as_ref()
        .and_then(|o| o.as_ref())
        .map(|s| s.configured)
        .unwrap_or(false);

    // Zone selection
    let mut selected_zone = use_signal(|| None::<String>);

    // Command-pattern element list and encoder config
    let mut elements = use_signal(Vec::<ElementDef>::new);
    let mut encoder_config = use_signal(default_encoder);

    // LLM chat state
    let mut chat_input = use_signal(String::new);
    let mut chat_messages: Signal<Vec<(bool, String)>> = use_signal(Vec::new); // (is_user, text)
    let mut llm_loading = use_signal(|| false);
    let mut llm_error = use_signal(|| None::<String>);

    // Current manifest from bridge (live state)
    let mut manifest: Signal<Option<ManifestResponse>> = use_signal(|| None);
    let mut manifest_loading = use_signal(|| false);

    // Status
    let mut apply_status = use_signal(|| None::<String>);

    // Fetch zones
    let mut zones =
        use_resource(|| async { api::fetch_json::<ZonesResponse>("/zones").await.ok() });

    // Refresh zones on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_zones() {
            zones.restart();
        }
    });

    // Fetch manifest when zone changes
    let zone_for_fetch = selected_zone();
    use_effect(move || {
        let zone_id = zone_for_fetch.clone();
        if let Some(ref zid) = zone_id {
            let zid = zid.clone();
            manifest_loading.set(true);
            spawn(async move {
                let url = format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
                match api::fetch_json::<ManifestResponse>(&url).await {
                    Ok(m) => {
                        // Sync element/encoder state from fetched manifest
                        if let Some(media) = m.screens.iter().find(|s| s.screen_type == "media") {
                            if let Some(ref elems) = media.elements {
                                elements.set(elems.clone());
                            } else if let Some(ref controls) = media.controls {
                                // v1 compat: convert flat controls list to elements
                                elements.set(controls_to_elements(controls));
                            } else {
                                // No controls = default elements (prev, play, next)
                                elements.set(controls_to_elements(&[
                                    "prev".to_string(),
                                    "play".to_string(),
                                    "next".to_string(),
                                ]));
                            }
                            if let Some(ref enc) = media.encoder {
                                encoder_config.set(enc.clone());
                            } else {
                                encoder_config.set(default_encoder());
                            }
                        }
                        manifest.set(Some(m));
                    }
                    Err(_) => {
                        manifest.set(None);
                    }
                }
                manifest_loading.set(false);
            });
        }
    });

    // Get text lines from current manifest (or defaults)
    let lines_for_preview = manifest()
        .as_ref()
        .and_then(|m| m.screens.iter().find(|s| s.screen_type == "media"))
        .map(|s| s.lines.clone())
        .unwrap_or_else(|| {
            vec![
                LineDef {
                    text: "No track".to_string(),
                    style: "title".to_string(),
                },
                LineDef {
                    text: "Select a zone".to_string(),
                    style: "subtitle".to_string(),
                },
            ]
        });

    let is_playing = manifest()
        .as_ref()
        .and_then(|m| m.fast.as_ref())
        .map(|f| f.is_playing)
        .unwrap_or(false);

    // Apply handler — push manifest to bridge
    let apply_handler = move |_| {
        if let Some(ref zid) = selected_zone() {
            let zid = zid.clone();
            let elems = elements.read().clone();
            let enc = encoder_config.read().clone();
            // Capture live lines from the fetched manifest; fall back to a sensible default.
            let current_lines: Vec<serde_json::Value> = manifest
                .read()
                .as_ref()
                .and_then(|m| m.screens.iter().find(|s| s.screen_type == "media"))
                .map(|s| {
                    s.lines
                        .iter()
                        .map(|l| serde_json::json!({"text": l.text, "style": l.style}))
                        .collect()
                })
                .unwrap_or_else(|| {
                    vec![serde_json::json!({"text": "Now Playing", "style": "title"})]
                });
            apply_status.set(Some("Applying...".to_string()));

            spawn(async move {
                // Build backward-compat v1 controls from elements
                let v1_controls = elements_to_controls(&elems);

                let screens_json = serde_json::json!([{
                    "type": "media",
                    "id": "now_playing",
                    "lines": current_lines,
                    "elements": elems,
                    "encoder": enc,
                    // Backward-compat for v1 firmware
                    "controls": v1_controls,
                }]);
                let body = serde_json::json!({
                    "zone_id": zid,
                    "screens": screens_json,
                    "nav": {"order": ["now_playing"], "default": "now_playing"},
                });
                match api::post_json_no_response("/knob/manifest", &body).await {
                    Ok(_) => {
                        apply_status
                            .set(Some("Applied — knob will update on next poll".to_string()));
                    }
                    Err(e) => {
                        apply_status.set(Some(format!("Error: {}", e)));
                    }
                }
            });
        }
    };

    // Reset handler — clear pushed manifest
    let reset_handler = move |_| {
        if let Some(ref zid) = selected_zone() {
            let zid = zid.clone();
            apply_status.set(Some("Resetting...".to_string()));

            spawn(async move {
                let delete_url = format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
                // Use DELETE method via fetch
                #[cfg(target_arch = "wasm32")]
                {
                    use wasm_bindgen_futures::JsFuture;
                    use web_sys::{Request, RequestInit};

                    let window = web_sys::window().unwrap();
                    let opts = RequestInit::new();
                    opts.set_method("DELETE");
                    if let Ok(request) = Request::new_with_str_and_init(&delete_url, &opts) {
                        let _ = JsFuture::from(window.fetch_with_request(&request)).await;
                    }
                }
                #[cfg(not(target_arch = "wasm32"))]
                {
                    let _ = delete_url;
                }
                apply_status.set(Some("Reset to default manifest".to_string()));
            });
        }
    };

    // LLM generate handler
    let mut send_to_llm = move |_| {
        let prompt = chat_input();
        if prompt.trim().is_empty() {
            return;
        }
        if let Some(ref zid) = selected_zone() {
            let zid = zid.clone();
            let prompt_text = prompt.clone();
            chat_messages.write().push((true, prompt_text.clone()));
            chat_input.set(String::new());
            llm_loading.set(true);
            llm_error.set(None);

            spawn(async move {
                let req = GenerateRequest {
                    prompt: prompt_text,
                    zone_id: zid.clone(),
                    device_type: "knob".to_string(),
                };
                match api::post_json::<_, GenerateResponse>("/api/manifest/generate", &req).await {
                    Ok(resp) => {
                        if let Some(err) = resp.error {
                            chat_messages
                                .write()
                                .push((false, format!("Error: {}", err)));
                            llm_error.set(Some(err));
                        } else {
                            chat_messages
                                .write()
                                .push((false, "Layout updated. Check the preview.".to_string()));
                            // Refresh manifest to pick up LLM changes
                            let url =
                                format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
                            if let Ok(m) = api::fetch_json::<ManifestResponse>(&url).await {
                                // Sync elements + encoder from new manifest
                                if let Some(media) =
                                    m.screens.iter().find(|s| s.screen_type == "media")
                                {
                                    if let Some(ref elems) = media.elements {
                                        elements.set(elems.clone());
                                    } else if let Some(ref controls) = media.controls {
                                        elements.set(controls_to_elements(controls));
                                    }
                                    if let Some(ref enc) = media.encoder {
                                        encoder_config.set(enc.clone());
                                    }
                                }
                                manifest.set(Some(m));
                            }
                        }
                    }
                    Err(e) => {
                        let msg = if e.contains("403") {
                            "Paid tier required for AI configuration.".to_string()
                        } else {
                            format!("Error: {}", e)
                        };
                        chat_messages.write().push((false, msg.clone()));
                        llm_error.set(Some(msg));
                    }
                }
                llm_loading.set(false);
            });
        }
    };

    let zones_list = zones
        .read()
        .clone()
        .flatten()
        .map(|r| r.zones)
        .unwrap_or_default();

    let has_zone = selected_zone().is_some();

    rsx! {
        Layout {
            title: "Configurator".to_string(),
            nav_active: "configurator".to_string(),

            h1 { class: "text-2xl font-bold mb-2", "Layout Configurator" }
            p { class: "text-muted mb-6 text-sm",
                "Preview and customize what your knob displays. Changes apply on the next poll cycle (~2s)."
            }

            if !is_licensed {
                // License required gate
                div { class: "card p-8 text-center",
                    div { class: "mb-4",
                        span { class: "text-4xl", "\u{1F512}" }
                    }
                    h2 { class: "text-lg font-semibold mb-2", "Memex License Required" }
                    p { class: "text-muted mb-4",
                        "The Layout Configurator requires a Memex license. Add your license key in Settings to enable this feature."
                    }
                    Link {
                        class: "btn btn-primary inline-block",
                        to: Route::Settings {},
                        "Go to Settings"
                    }
                }
            } else {

            // Zone selector
            div { class: "mb-6",
                label { class: "block text-sm font-medium mb-1", "Zone" }
                select {
                    class: "input max-w-xs",
                    value: selected_zone().unwrap_or_default(),
                    onchange: move |e| {
                        let v = e.value();
                        if v.is_empty() {
                            selected_zone.set(None);
                        } else {
                            selected_zone.set(Some(v));
                        }
                    },
                    option { value: "", "Select a zone..." }
                    for zone in zones_list.iter() {
                        option {
                            value: "{zone.zone_id}",
                            "{zone.zone_name} ({zone.state})"
                        }
                    }
                }
            }

            if has_zone {
                // Two-column layout: controls left, preview right
                div { class: "grid grid-cols-1 lg:grid-cols-2 gap-6",

                    // ── Left: Controls ────────────────────────────────
                    div {
                        // Transport Elements editor
                        div { class: "card p-4 mb-4",
                            h3 { class: "text-sm font-semibold mb-1", "Transport Elements" }
                            p { class: "text-xs text-muted mb-3",
                                "Elements shown on the media screen. Each carries an icon and action."
                            }
                            div { class: "space-y-2",
                                for (idx, elem) in elements.read().iter().enumerate() {
                                    div { class: "flex items-center gap-2 p-2 bg-base-200 rounded",
                                        // Icon display
                                        span { class: "font-mono text-sm w-24",
                                            "{elem.display.icon.as_deref().unwrap_or(\"-\")}"
                                        }
                                        // Tap action
                                        span { class: "text-xs text-muted", "tap:" }
                                        span { class: "text-sm",
                                            "{elem.on_tap.as_ref().map(|a| a.action.as_str()).unwrap_or(\"none\")}"
                                        }
                                        // Long press action (if any)
                                        if let Some(ref lp) = elem.on_long_press {
                                            span { class: "text-xs text-muted ml-2", "hold:" }
                                            span { class: "text-sm", "{lp.action}" }
                                        }
                                        // Remove button
                                        button {
                                            class: "btn btn-xs btn-ghost ml-auto",
                                            onclick: move |_| {
                                                elements.write().remove(idx);
                                            },
                                            "x"
                                        }
                                    }
                                }
                            }
                            // Add element preset buttons
                            div { class: "flex gap-2 mt-3 flex-wrap",
                                button {
                                    class: "btn btn-xs btn-outline",
                                    onclick: move |_| {
                                        elements.write().push(ElementDef {
                                            display: DisplayDef {
                                                icon: Some("skip_previous".into()),
                                                label: None,
                                                active: None,
                                            },
                                            on_tap: Some(ActionDef {
                                                action: "previous".into(),
                                                params: None,
                                            }),
                                            on_long_press: None,
                                        });
                                    },
                                    "+ Prev"
                                }
                                button {
                                    class: "btn btn-xs btn-outline",
                                    onclick: move |_| {
                                        elements.write().push(ElementDef {
                                            display: DisplayDef {
                                                icon: Some("play_arrow".into()),
                                                label: None,
                                                active: None,
                                            },
                                            on_tap: Some(ActionDef {
                                                action: "toggle_playback".into(),
                                                params: None,
                                            }),
                                            on_long_press: Some(ActionDef {
                                                action: "stop".into(),
                                                params: None,
                                            }),
                                        });
                                    },
                                    "+ Play"
                                }
                                button {
                                    class: "btn btn-xs btn-outline",
                                    onclick: move |_| {
                                        elements.write().push(ElementDef {
                                            display: DisplayDef {
                                                icon: Some("skip_next".into()),
                                                label: None,
                                                active: None,
                                            },
                                            on_tap: Some(ActionDef {
                                                action: "next".into(),
                                                params: None,
                                            }),
                                            on_long_press: None,
                                        });
                                    },
                                    "+ Next"
                                }
                                button {
                                    class: "btn btn-xs btn-outline",
                                    onclick: move |_| {
                                        elements.write().push(ElementDef {
                                            display: DisplayDef {
                                                icon: Some("volume_off".into()),
                                                label: None,
                                                active: None,
                                            },
                                            on_tap: Some(ActionDef {
                                                action: "toggle_mute".into(),
                                                params: None,
                                            }),
                                            on_long_press: None,
                                        });
                                    },
                                    "+ Mute"
                                }
                            }
                        }

                        // Encoder editor
                        div { class: "card p-4 mb-4",
                            h3 { class: "text-sm font-semibold mb-1", "Encoder" }
                            p { class: "text-xs text-muted mb-3",
                                "Encoder rotation always controls volume (safety requirement)."
                            }
                            div { class: "grid grid-cols-2 gap-2",
                                // CW — locked to volume_up
                                div { class: "flex items-center gap-2",
                                    span { class: "text-xs w-20", "Rotate CW" }
                                    span { class: "text-sm text-muted", "volume_up (locked)" }
                                }
                                // CCW — locked to volume_down
                                div { class: "flex items-center gap-2",
                                    span { class: "text-xs w-20", "Rotate CCW" }
                                    span { class: "text-sm text-muted", "volume_down (locked)" }
                                }
                                // Press — configurable
                                div { class: "flex items-center gap-2 col-span-2 sm:col-span-1",
                                    span { class: "text-xs w-20", "Press" }
                                    select {
                                        class: "select select-xs select-bordered flex-1",
                                        value: encoder_config.read().press.as_ref().map(|a| a.action.clone()).unwrap_or_default(),
                                        onchange: move |evt: Event<FormData>| {
                                            let v = evt.value();
                                            let mut enc = encoder_config.write();
                                            if v.is_empty() {
                                                enc.press = None;
                                            } else {
                                                enc.press = Some(ActionDef { action: v, params: None });
                                            }
                                        },
                                        option { value: "", "(none)" }
                                        option { value: "toggle_playback", "Play / Pause" }
                                        option { value: "toggle_mute", "Toggle Mute" }
                                        option { value: "next", "Next Track" }
                                        option { value: "previous", "Previous Track" }
                                        option { value: "show_zone_picker", "Zone Picker" }
                                    }
                                }
                                // Long press — configurable
                                div { class: "flex items-center gap-2 col-span-2 sm:col-span-1",
                                    span { class: "text-xs w-20", "Long Press" }
                                    select {
                                        class: "select select-xs select-bordered flex-1",
                                        value: encoder_config.read().long_press.as_ref().map(|a| a.action.clone()).unwrap_or_default(),
                                        onchange: move |evt: Event<FormData>| {
                                            let v = evt.value();
                                            let mut enc = encoder_config.write();
                                            if v.is_empty() {
                                                enc.long_press = None;
                                            } else {
                                                enc.long_press = Some(ActionDef { action: v, params: None });
                                            }
                                        },
                                        option { value: "", "(none)" }
                                        option { value: "show_zone_picker", "Zone Picker" }
                                        option { value: "toggle_mute", "Toggle Mute" }
                                        option { value: "stop", "Stop" }
                                        option { value: "toggle_playback", "Play / Pause" }
                                    }
                                }
                            }
                        }

                        // LLM chat (paid tier)
                        div { class: "card p-4 mb-4",
                            div { class: "flex items-center gap-2 mb-3",
                                h3 { class: "text-sm font-semibold", "AI Configurator" }
                                span { class: "text-xs px-1.5 py-0.5 rounded bg-purple-500/20 text-purple-300", "Paid" }
                            }
                            p { class: "text-xs text-muted mb-3",
                                "Describe changes in plain English. The AI will update the layout."
                            }

                            // Chat history
                            if !chat_messages().is_empty() {
                                div { class: "mb-3 max-h-48 overflow-y-auto space-y-2 text-sm",
                                    for (is_user, msg) in chat_messages().iter() {
                                        div {
                                            class: if *is_user { "text-right" } else { "" },
                                            span {
                                                class: if *is_user {
                                                    "inline-block px-3 py-1.5 rounded-lg bg-blue-500/20 text-blue-200"
                                                } else {
                                                    "inline-block px-3 py-1.5 rounded-lg bg-elevated"
                                                },
                                                "{msg}"
                                            }
                                        }
                                    }
                                    if llm_loading() {
                                        div {
                                            span { class: "inline-block px-3 py-1.5 rounded-lg bg-elevated text-muted",
                                                aria_busy: "true",
                                                "Thinking..."
                                            }
                                        }
                                    }
                                }
                            }

                            // Input
                            form {
                                class: "flex gap-2",
                                onsubmit: move |e| {
                                    e.prevent_default();
                                    send_to_llm(());
                                },
                                input {
                                    class: "input flex-1",
                                    r#type: "text",
                                    placeholder: "e.g. \"hide prev, add mute, press encoder = toggle mute\"",
                                    value: "{chat_input}",
                                    disabled: llm_loading(),
                                    oninput: move |e| chat_input.set(e.value()),
                                }
                                button {
                                    class: "btn btn-primary",
                                    r#type: "submit",
                                    disabled: llm_loading() || chat_input().trim().is_empty(),
                                    "Send"
                                }
                            }
                        }

                        // Apply / Reset
                        div { class: "flex items-center gap-3",
                            button {
                                class: "btn btn-primary",
                                disabled: selected_zone().is_none(),
                                onclick: apply_handler,
                                "Apply to Device"
                            }
                            button {
                                class: "btn btn-outline",
                                disabled: selected_zone().is_none(),
                                onclick: reset_handler,
                                "Reset to Default"
                            }
                            if let Some(ref status) = apply_status() {
                                span { class: "text-sm text-muted", "{status}" }
                            }
                        }
                    }

                    // ── Right: Live Preview ──────────────────────────
                    div {
                        // Knob display preview
                        div { class: "card p-4 mb-4",
                            h3 { class: "text-sm font-semibold mb-3", "Display Preview" }
                            ScreenPreview {
                                lines: lines_for_preview.clone(),
                                elements: elements.read().clone(),
                                is_playing: is_playing,
                            }
                        }

                        // Input mapping summary
                        div { class: "card p-4 mb-4",
                            h3 { class: "text-sm font-semibold mb-3", "Input Mapping" }
                            InputMappingPreview {
                                encoder: encoder_config.read().clone(),
                                elements: elements.read().clone(),
                            }
                        }

                        // State cascade (display power lifecycle)
                        div { class: "card p-4",
                            h3 { class: "text-sm font-semibold mb-3", "Display Power Lifecycle" }
                            PowerCascadePreview {}
                        }
                    }
                }
            } else {
                div { class: "card p-8 text-center text-muted",
                    "Select a zone above to configure your knob layout."
                }
            }

            } // end is_licensed else
        }
    }
}

// ── Subcomponents ────────────────────────────────────────────────────────────

/// Circular knob display mockup — renders from command-pattern elements.
#[component]
fn ScreenPreview(lines: Vec<LineDef>, elements: Vec<ElementDef>, is_playing: bool) -> Element {
    // Map element icons to display symbols
    let button_symbols: Vec<&'static str> = elements
        .iter()
        .map(|elem| {
            match elem.display.icon.as_deref().unwrap_or("") {
                "skip_previous" => "\u{23EE}", // ⏮
                "play_arrow" => {
                    if is_playing {
                        "\u{23F8}"
                    } else {
                        "\u{25B6}"
                    } // ⏸ or ▶
                }
                "pause" => "\u{23F8}",                       // ⏸
                "skip_next" => "\u{23ED}",                   // ⏭
                "volume_off" | "volume_mute" => "\u{1F507}", // 🔇
                "forward_30" => "30\u{00BB}",
                _ => "\u{25CF}", // bullet fallback
            }
        })
        .collect();

    rsx! {
        div { class: "flex justify-center",
            div {
                class: "w-56 h-56 rounded-full border-2 border-default bg-black relative overflow-hidden flex flex-col items-center justify-center",

                // Album art placeholder
                div { class: "absolute inset-4 rounded-full bg-gradient-to-br from-gray-800 to-gray-900 opacity-60" }

                // Text lines
                div { class: "relative z-10 text-center px-6 mb-4",
                    for line in lines.iter() {
                        p {
                            class: match line.style.as_str() {
                                "title" => "text-white text-sm font-bold truncate",
                                "subtitle" => "text-gray-300 text-xs truncate",
                                _ => "text-gray-400 text-xs truncate",
                            },
                            "{line.text}"
                        }
                    }
                }

                // Transport elements
                if !elements.is_empty() {
                    div { class: "relative z-10 flex items-center justify-center gap-4",
                        for symbol in button_symbols.iter() {
                            span {
                                class: "text-white text-lg cursor-default select-none hover:text-blue-400 transition-colors",
                                "{symbol}"
                            }
                        }
                    }
                }

                // Volume arc indicator (decorative)
                div { class: "absolute top-1 left-1/2 -translate-x-1/2 text-xs text-gray-600", "vol" }
            }
        }

        // Elements label below
        if elements.is_empty() {
            p { class: "text-center text-xs text-muted mt-2", "Default layout (all buttons)" }
        } else {
            p { class: "text-center text-xs text-muted mt-2",
                "{elements.len()} element(s) configured"
            }
        }
    }
}

/// Input mapping summary table — shows encoder config + element tap actions.
#[component]
fn InputMappingPreview(encoder: EncoderDef, elements: Vec<ElementDef>) -> Element {
    rsx! {
        div { class: "space-y-1",
            // Encoder CW — locked
            div { class: "flex items-center justify-between text-sm py-1",
                span { class: "text-secondary",
                    "Rotate CW"
                    span { class: "text-xs text-yellow-500 ml-1", "(locked)" }
                }
                span { class: "font-mono text-xs px-2 py-0.5 rounded bg-elevated",
                    "{format_action(&encoder.cw.action)}"
                }
            }
            // Encoder CCW — locked
            div { class: "flex items-center justify-between text-sm py-1",
                span { class: "text-secondary",
                    "Rotate CCW"
                    span { class: "text-xs text-yellow-500 ml-1", "(locked)" }
                }
                span { class: "font-mono text-xs px-2 py-0.5 rounded bg-elevated",
                    "{format_action(&encoder.ccw.action)}"
                }
            }
            // Encoder press — configurable
            div { class: "flex items-center justify-between text-sm py-1",
                span { class: "text-secondary", "Press Encoder" }
                span { class: "font-mono text-xs px-2 py-0.5 rounded bg-elevated",
                    "{encoder.press.as_ref().map(|a| format_action(&a.action)).unwrap_or_else(|| \"none\".into())}"
                }
            }
            // Encoder long press — configurable
            div { class: "flex items-center justify-between text-sm py-1",
                span { class: "text-secondary", "Long Press Encoder" }
                span { class: "font-mono text-xs px-2 py-0.5 rounded bg-elevated",
                    "{encoder.long_press.as_ref().map(|a| format_action(&a.action)).unwrap_or_else(|| \"none\".into())}"
                }
            }

            // Element tap actions
            if !elements.is_empty() {
                div { class: "border-t border-default mt-2 pt-2" }
                for elem in elements.iter() {
                    if let Some(ref tap) = elem.on_tap {
                        div { class: "flex items-center justify-between text-sm py-1",
                            span { class: "text-secondary",
                                "Tap {elem.display.icon.as_deref().unwrap_or(\"?\")}"
                            }
                            span { class: "font-mono text-xs px-2 py-0.5 rounded bg-elevated",
                                "{format_action(&tap.action)}"
                            }
                        }
                    }
                    if let Some(ref lp) = elem.on_long_press {
                        div { class: "flex items-center justify-between text-sm py-1",
                            span { class: "text-secondary",
                                "Hold {elem.display.icon.as_deref().unwrap_or(\"?\")}"
                            }
                            span { class: "font-mono text-xs px-2 py-0.5 rounded bg-elevated",
                                "{format_action(&lp.action)}"
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Simplified power lifecycle cascade (read-only, shows current device config)
#[component]
fn PowerCascadePreview() -> Element {
    // Static defaults — in a real integration this would read from knob config
    let states = vec![
        ("Normal", "full brightness", "active"),
        ("Art Mode", "controls hidden", "info"),
        ("Dim", "30% brightness", "warning"),
        ("Sleep", "screen off", "muted"),
    ];

    rsx! {
        div { class: "flex items-center gap-1.5 flex-wrap justify-center",
            for (i, (name, desc, _style)) in states.iter().enumerate() {
                if i > 0 {
                    div { class: "flex flex-col items-center mx-1",
                        span { class: "text-secondary text-lg", "\u{2192}" }
                    }
                }
                div { class: "flex flex-col items-center px-3 py-1.5 rounded text-xs bg-elevated",
                    span { class: "font-medium", "{name}" }
                    span { class: "text-muted", "{desc}" }
                }
            }
        }
        p { class: "text-xs text-center text-muted mt-2",
            "Any input resets to Normal. Configure timeouts in Knob Config."
        }
    }
}

/// Format action name for display
fn format_action(action: &str) -> String {
    match action {
        "volume_up" => "Volume Up".to_string(),
        "volume_down" => "Volume Down".to_string(),
        "toggle_playback" => "Play / Pause".to_string(),
        "toggle_mute" => "Toggle Mute".to_string(),
        "next" => "Next Track".to_string(),
        "previous" => "Prev Track".to_string(),
        "play" => "Play".to_string(),
        "pause" => "Pause".to_string(),
        "mute" => "Mute".to_string(),
        "unmute" => "Unmute".to_string(),
        "stop" => "Stop".to_string(),
        "show_zone_picker" => "Zone Picker".to_string(),
        other => other.to_string(),
    }
}
