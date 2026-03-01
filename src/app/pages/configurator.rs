//! Configurator page — describe what you want, see it on the knob.
//!
//! Simple interface: knob preview + natural language input.
//! The LLM generates command-pattern manifests from your description.

use dioxus::prelude::*;

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
    elements: Option<Vec<ElementDef>>,
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
struct LineDef {
    #[serde(default)]
    text: String,
    #[serde(default)]
    style: String,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct FastState {
    #[serde(default)]
    zone_id: String,
    #[serde(default)]
    is_playing: bool,
    #[serde(default)]
    volume: Option<f64>,
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

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct GenerateRequest {
    prompt: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    zone_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    device_type: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct GenerateResponse {
    status: String,
    error: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct ZonesResponse {
    zones: Vec<ZoneInfo>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct ZoneInfo {
    zone_id: String,
    zone_name: String,
    #[serde(default)]
    state: String,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct LicenseStatus {
    configured: bool,
}

// ── Page ─────────────────────────────────────────────────────────────────────

#[component]
pub fn Configurator() -> Element {
    let sse = use_sse();

    // License check
    let license_status = use_resource(|| async {
        api::fetch_json::<LicenseStatus>("/api/config/license")
            .await
            .ok()
    });
    let is_licensed = license_status
        .read()
        .as_ref()
        .and_then(|r| r.as_ref())
        .map(|s| s.configured)
        .unwrap_or(false);

    // Zone selector (optional — for previewing a specific zone's manifest)
    let mut selected_zone = use_signal(|| None::<String>);
    let zones = use_resource(|| async { api::fetch_json::<ZonesResponse>("/zones").await.ok() });

    // Manifest preview — fetched when zone changes
    let mut manifest = use_signal(|| None::<ManifestResponse>);
    let zone_for_fetch = selected_zone();
    use_effect(move || {
        let zone_id = zone_for_fetch.clone();
        if let Some(ref zid) = zone_id {
            let zid = zid.clone();
            spawn(async move {
                let url = format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
                if let Ok(m) = api::fetch_json::<ManifestResponse>(&url).await {
                    manifest.set(Some(m));
                }
            });
        }
    });

    // Refresh manifest on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if let Some(ref zid) = selected_zone() {
            let zid = zid.clone();
            spawn(async move {
                let url = format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
                if let Ok(m) = api::fetch_json::<ManifestResponse>(&url).await {
                    manifest.set(Some(m));
                }
            });
        }
    });

    // LLM chat
    let mut chat_input = use_signal(String::new);
    let mut chat_messages = use_signal(Vec::<(bool, String)>::new); // (is_user, text)
    let mut llm_loading = use_signal(|| false);

    let mut send_handler = move |_| {
        let input = chat_input().trim().to_string();
        if input.is_empty() || llm_loading() {
            return;
        }
        chat_messages.write().push((true, input.clone()));
        chat_input.set(String::new());
        llm_loading.set(true);

        let zone_id = selected_zone();
        spawn(async move {
            let req = GenerateRequest {
                prompt: input,
                zone_id,
                device_type: Some("knob".into()),
            };
            match api::post_json::<_, GenerateResponse>("/api/manifest/generate", &req).await {
                Ok(resp) => {
                    if let Some(err) = resp.error {
                        chat_messages
                            .write()
                            .push((false, format!("Error: {}", err)));
                    } else {
                        chat_messages.write().push((
                            false,
                            "Done. The knob will update on the next poll.".to_string(),
                        ));
                        // Refresh manifest preview
                        if let Some(ref zid) = selected_zone() {
                            let url =
                                format!("/knob/manifest?zone_id={}", urlencoding::encode(zid));
                            if let Ok(m) = api::fetch_json::<ManifestResponse>(&url).await {
                                manifest.set(Some(m));
                            }
                        }
                    }
                }
                Err(e) => {
                    chat_messages
                        .write()
                        .push((false, format!("Failed: {}", e)));
                }
            }
            llm_loading.set(false);
        });
    };

    // Preview data
    let preview_lines: Vec<LineDef> = manifest()
        .as_ref()
        .and_then(|m| m.screens.iter().find(|s| s.screen_type == "media"))
        .map(|s| s.lines.clone())
        .unwrap_or_default();

    let preview_elements: Vec<ElementDef> = manifest()
        .as_ref()
        .and_then(|m| m.screens.iter().find(|s| s.screen_type == "media"))
        .and_then(|s| s.elements.clone())
        .unwrap_or_default();

    let is_playing = manifest()
        .as_ref()
        .and_then(|m| m.fast.as_ref())
        .map(|f| f.is_playing)
        .unwrap_or(false);

    // Reset handler
    let reset_handler = move |_| {
        if let Some(ref zid) = selected_zone() {
            let zid = zid.clone();
            spawn(async move {
                let url = format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
                #[cfg(target_arch = "wasm32")]
                {
                    use wasm_bindgen::JsCast;
                    use wasm_bindgen_futures::JsFuture;
                    use web_sys::{Request, RequestInit};
                    let window = web_sys::window().unwrap();
                    let opts = RequestInit::new();
                    opts.set_method("DELETE");
                    let request = Request::new_with_str_and_init(&url, &opts).unwrap();
                    let _ = JsFuture::from(window.fetch_with_request(&request)).await;
                }
                // Refresh
                if let Ok(m) = api::fetch_json::<ManifestResponse>(&url).await {
                    manifest.set(Some(m));
                }
                chat_messages
                    .write()
                    .push((false, "Reset to default layout.".to_string()));
            });
        }
    };

    rsx! {
        Layout {
            title: "Configurator".to_string(),
            nav_active: "configurator".to_string(),

            h1 { class: "text-2xl font-bold mb-2", "Layout Configurator" }
            p { class: "text-muted mb-6 text-sm",
                "Describe what you want your knob to show. The AI builds it."
            }

            if !is_licensed {
                div { class: "card p-8 text-center",
                    div { class: "mb-4", span { class: "text-4xl", "\u{1F512}" } }
                    h2 { class: "text-lg font-semibold mb-2", "Memex License Required" }
                    p { class: "text-muted mb-4",
                        "Add your license key in Settings to enable the Configurator."
                    }
                    Link {
                        class: "btn btn-primary inline-block",
                        to: Route::Settings {},
                        "Go to Settings"
                    }
                }
            } else {

            div { class: "grid grid-cols-1 lg:grid-cols-2 gap-6",

                // Left column: preview
                div {
                    // Zone selector (optional)
                    div { class: "mb-4",
                        label { class: "block text-xs text-muted mb-1", "Zone (optional)" }
                        select {
                            class: "input max-w-xs text-sm",
                            value: selected_zone().unwrap_or_default(),
                            onchange: move |e| {
                                let v = e.value();
                                selected_zone.set(if v.is_empty() { None } else { Some(v) });
                            },
                            option { value: "", "All zones (default)" }
                            if let Some(Some(ref zr)) = *zones.read() {
                                for z in zr.zones.iter() {
                                    option {
                                        value: "{z.zone_id}",
                                        selected: selected_zone() == Some(z.zone_id.clone()),
                                        "{z.zone_name}"
                                    }
                                }
                            }
                        }
                    }

                    // Knob preview
                    ScreenPreview {
                        lines: preview_lines,
                        elements: preview_elements,
                        is_playing: is_playing,
                    }

                    // Reset button
                    if selected_zone().is_some() {
                        button {
                            class: "btn btn-sm text-xs mt-3 opacity-60 hover:opacity-100",
                            onclick: reset_handler,
                            "Reset to default"
                        }
                    }
                }

                // Right column: chat
                div { class: "card p-4",
                    h3 { class: "text-sm font-semibold mb-3", "Describe your layout" }
                    p { class: "text-xs text-muted mb-3",
                        "Examples: \"Add a mute button\", \"Show only play/pause for podcasts\", \"Add 30-second skip for long tracks\""
                    }

                    // Chat history
                    div { class: "space-y-2 mb-4 max-h-64 overflow-y-auto",
                        for (is_user, msg) in chat_messages.read().iter() {
                            div {
                                class: if *is_user { "text-sm text-right" } else { "text-sm text-muted" },
                                if *is_user {
                                    span { class: "inline-block bg-blue-900 rounded px-2 py-1", "{msg}" }
                                } else {
                                    span { class: "inline-block bg-base-200 rounded px-2 py-1", "{msg}" }
                                }
                            }
                        }
                        if llm_loading() {
                            div { class: "text-sm text-muted animate-pulse", "Thinking..." }
                        }
                    }

                    // Input
                    div { class: "flex gap-2",
                        input {
                            class: "input flex-1 text-sm",
                            placeholder: "What should the knob show?",
                            value: "{chat_input}",
                            oninput: move |e| chat_input.set(e.value()),
                            onkeypress: move |e| {
                                if e.key() == Key::Enter {
                                    send_handler(());
                                }
                            },
                        }
                        button {
                            class: "btn btn-primary btn-sm",
                            disabled: llm_loading() || chat_input().trim().is_empty(),
                            onclick: move |_| send_handler(()),
                            if llm_loading() { "..." } else { "Send" }
                        }
                    }
                }
            }

            } // end licensed
        }
    }
}

// ── Preview ──────────────────────────────────────────────────────────────────

#[component]
fn ScreenPreview(lines: Vec<LineDef>, elements: Vec<ElementDef>, is_playing: bool) -> Element {
    rsx! {
        div {
            class: "relative mx-auto",
            style: "width: 240px; height: 240px;",

            // Circular knob background
            div {
                class: "absolute inset-0 rounded-full bg-gray-900 border-2 border-gray-700",
                style: "width: 240px; height: 240px;",

                // Text lines
                div {
                    class: "absolute inset-0 flex flex-col items-center justify-center px-8",
                    style: "padding-top: 50px;",
                    for line in lines.iter() {
                        p {
                            class: match line.style.as_str() {
                                "title" => "text-white text-sm font-semibold text-center truncate w-full",
                                "subtitle" => "text-gray-400 text-xs text-center truncate w-full",
                                _ => "text-gray-500 text-xs text-center truncate w-full",
                            },
                            "{line.text}"
                        }
                    }
                    if lines.is_empty() {
                        p { class: "text-gray-500 text-xs", "No manifest loaded" }
                    }
                }

                // Transport buttons at bottom
                div {
                    class: "absolute bottom-8 left-0 right-0 flex justify-center gap-3",
                    for elem in elements.iter() {
                        if let Some(ref icon) = elem.display.icon {
                            span {
                                class: "text-lg text-gray-300",
                                title: elem.on_tap.as_ref().map(|a| a.action.as_str()).unwrap_or(""),
                                {icon_char(icon)}
                            }
                        }
                    }
                    if elements.is_empty() {
                        span { class: "text-lg text-gray-500", "\u{23EE}" }
                        span { class: "text-lg text-gray-300",
                            if is_playing { "\u{23F8}" } else { "\u{25B6}" }
                        }
                        span { class: "text-lg text-gray-500", "\u{23ED}" }
                    }
                }
            }
        }
    }
}

fn icon_char(name: &str) -> &'static str {
    match name {
        "skip_previous" => "\u{23EE}",
        "play_arrow" => "\u{25B6}",
        "pause" => "\u{23F8}",
        "skip_next" => "\u{23ED}",
        "stop" => "\u{23F9}",
        "volume_off" | "volume_mute" => "\u{1F507}",
        "forward_30" => "30\u{00BB}",
        "forward_10" => "10\u{00BB}",
        "replay_30" => "\u{00AB}30",
        "replay_10" => "\u{00AB}10",
        "shuffle" => "\u{1F500}",
        "repeat" => "\u{1F501}",
        _ => "\u{25CF}",
    }
}
