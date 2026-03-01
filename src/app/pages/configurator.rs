//! Configurator page — describe what you want, see it on the device.
//!
//! Simple interface: device preview + natural language input.
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
    #[serde(default)]
    background_color: Option<String>,
    #[serde(default)]
    image_url: Option<String>,
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
    #[serde(default)]
    volume_min: Option<f64>,
    #[serde(default)]
    volume_max: Option<f64>,
    #[serde(default)]
    seek_position: Option<i64>,
    #[serde(default)]
    length: Option<u32>,
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
    #[serde(default)]
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
    // Deserialized from server response but not displayed in this component
    #[serde(default)]
    #[allow(dead_code)]
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

    // Device selector
    let mut device_type = use_signal(|| "Dial".to_string());

    // Zone selector (optional — for previewing a specific zone's manifest)
    let mut selected_zone = use_signal(|| None::<String>);
    let zones = use_resource(|| async { api::fetch_json::<ZonesResponse>("/zones").await.ok() });

    // Manifest preview — fetched when zone changes or SSE event arrives
    let mut manifest = use_signal(|| None::<ManifestResponse>);

    let fetch_manifest = move |zid: String| {
        spawn(async move {
            let url = format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
            if let Ok(m) = api::fetch_json::<ManifestResponse>(&url).await {
                manifest.set(Some(m));
            }
        });
    };

    let zone_for_fetch = selected_zone();
    use_effect(move || {
        if let Some(zid) = zone_for_fetch.clone() {
            fetch_manifest(zid);
        }
    });

    // Refresh manifest on SSE events (only when a zone is selected)
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if let Some(zid) = selected_zone() {
            fetch_manifest(zid);
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

        // Capture zone_id and device_type at dispatch time so post-generate refresh
        // targets the same zone even if the user changes selection mid-flight.
        let zone_id = selected_zone();
        let dt = device_type().to_lowercase();
        spawn(async move {
            let req = GenerateRequest {
                prompt: input,
                zone_id: zone_id.clone(),
                device_type: Some(dt),
            };
            match api::post_json::<_, GenerateResponse>("/api/manifest/generate", &req).await {
                Ok(resp) => {
                    let failed =
                        resp.error.is_some() || (resp.status != "ok" && !resp.status.is_empty());
                    if failed {
                        let msg = resp
                            .error
                            .unwrap_or_else(|| format!("server status: {}", resp.status));
                        chat_messages
                            .write()
                            .push((false, format!("error:{}", msg)));
                    } else {
                        chat_messages.write().push((
                            false,
                            "ok:Layout updated. Device will refresh on next poll.".to_string(),
                        ));
                        // Refresh manifest preview for the same zone the request targeted
                        if let Some(ref zid) = zone_id {
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
                        .push((false, format!("error:Request failed: {}", e)));
                }
            }
            llm_loading.set(false);
        });
    };

    // Preview data — extracted from manifest
    let media_screen = manifest()
        .as_ref()
        .and_then(|m| m.screens.iter().find(|s| s.screen_type == "media").cloned());

    let preview_lines: Vec<LineDef> = media_screen
        .as_ref()
        .map(|s| s.lines.clone())
        .unwrap_or_default();

    let preview_elements: Vec<ElementDef> = media_screen
        .as_ref()
        .and_then(|s| s.elements.clone())
        .unwrap_or_default();

    let preview_bg_color: Option<String> = media_screen
        .as_ref()
        .and_then(|s| s.background_color.clone());

    let fast = manifest().as_ref().and_then(|m| m.fast.clone());
    let is_playing = fast.as_ref().map(|f| f.is_playing).unwrap_or(false);

    // Volume fraction (0.0–1.0) for the outer arc
    let volume_fraction = fast.as_ref().and_then(|f| {
        let v = f.volume?;
        let max = f.volume_max.unwrap_or(100.0);
        let min = f.volume_min.unwrap_or(0.0);
        if max > min {
            Some(((v - min) / (max - min)).clamp(0.0, 1.0))
        } else {
            None
        }
    });

    // Seek fraction (0.0–1.0) for the inner arc
    let seek_fraction = fast.as_ref().and_then(|f| {
        let pos = f.seek_position? as f64;
        let len = f.length? as f64;
        if len > 0.0 {
            Some((pos / len).clamp(0.0, 1.0))
        } else {
            None
        }
    });

    // Zone name for the preview header
    let selected_zone_name: Option<String> = selected_zone().and_then(|zid| {
        zones
            .read()
            .as_ref()
            .and_then(|r| r.as_ref())
            .and_then(|zr| {
                zr.zones
                    .iter()
                    .find(|z| z.zone_id == zid)
                    .map(|z| z.zone_name.clone())
            })
    });

    // Reset handler
    let reset_handler = move |_| {
        if let Some(ref zid) = selected_zone() {
            let zid = zid.clone();
            spawn(async move {
                let url = format!("/knob/manifest?zone_id={}", urlencoding::encode(&zid));
                #[cfg(target_arch = "wasm32")]
                {
                    use wasm_bindgen_futures::JsFuture;
                    use web_sys::{Request, RequestInit};
                    let delete_result: Result<(), String> = (|| {
                        let window = web_sys::window().ok_or("no window context")?;
                        let opts = RequestInit::new();
                        opts.set_method("DELETE");
                        let request = Request::new_with_str_and_init(&url, &opts)
                            .map_err(|e| format!("{:?}", e))?;
                        let _ = window.fetch_with_request(&request);
                        Ok(())
                    })();
                    if let Err(e) = delete_result {
                        chat_messages
                            .write()
                            .push((false, format!("error:Reset failed: {}", e)));
                        return;
                    }
                    // Give the DELETE a tick to complete before re-fetching
                    let _ = JsFuture::from(js_sys::Promise::resolve(&wasm_bindgen::JsValue::NULL))
                        .await;
                }
                if let Ok(m) = api::fetch_json::<ManifestResponse>(&url).await {
                    manifest.set(Some(m));
                }
                chat_messages
                    .write()
                    .push((false, "ok:Reset to default layout.".to_string()));
            });
        }
    };

    rsx! {
        Layout {
            title: "Configurator".to_string(),
            nav_active: "configurator".to_string(),

            h1 { class: "text-2xl font-bold mb-2", "Layout Configurator" }
            p { class: "text-muted mb-6 text-sm",
                "Describe what you want your device to show. The AI builds it."
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
                    // Controls row: zone selector + device selector
                    div { class: "flex gap-3 mb-4 items-end flex-wrap",
                        div {
                            label { class: "block text-xs text-muted mb-1", "Zone (optional)" }
                            select {
                                class: "input text-sm",
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
                        div {
                            label { class: "block text-xs text-muted mb-1", "Device" }
                            select {
                                class: "input text-sm",
                                value: "{device_type}",
                                onchange: move |e| device_type.set(e.value()),
                                option { value: "Dial", "Dial" }
                                option { value: "Frame", "Frame" }
                                option { value: "Tough", "Tough" }
                            }
                        }
                    }

                    // Device preview
                    ScreenPreview {
                        lines: preview_lines,
                        elements: preview_elements,
                        is_playing,
                        device_type: device_type(),
                        zone_name: selected_zone_name,
                        background_color: preview_bg_color,
                        volume_fraction,
                        seek_fraction,
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
                                class: if *is_user { "text-sm text-right" } else { "text-sm" },
                                if *is_user {
                                    span { class: "inline-block bg-blue-900 rounded px-2 py-1", "{msg}" }
                                } else if let Some(text) = msg.strip_prefix("error:") {
                                    span { class: "inline-block bg-red-900 text-red-200 rounded px-2 py-1", "{text}" }
                                } else if let Some(text) = msg.strip_prefix("ok:") {
                                    span { class: "inline-block bg-green-900 text-green-200 rounded px-2 py-1", "{text}" }
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
                            placeholder: "What should the device display?",
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
fn ScreenPreview(
    lines: Vec<LineDef>,
    elements: Vec<ElementDef>,
    is_playing: bool,
    device_type: String,
    zone_name: Option<String>,
    background_color: Option<String>,
    volume_fraction: Option<f64>,
    seek_fraction: Option<f64>,
) -> Element {
    match device_type.as_str() {
        "Frame" | "Tough" => rsx! {
            RectPreview {
                lines,
                elements,
                is_playing,
                zone_name,
                background_color,
                rounded: device_type == "Tough",
            }
        },
        _ => rsx! {
            DialPreview {
                lines,
                elements,
                is_playing,
                zone_name,
                background_color,
                volume_fraction,
                seek_fraction,
            }
        },
    }
}

/// Circular knob preview (Dial device)
#[component]
fn DialPreview(
    lines: Vec<LineDef>,
    elements: Vec<ElementDef>,
    is_playing: bool,
    zone_name: Option<String>,
    background_color: Option<String>,
    volume_fraction: Option<f64>,
    seek_fraction: Option<f64>,
) -> Element {
    let cx = 120.0_f64;
    let cy = 120.0_f64;
    let vol_arc = volume_fraction
        .map(|f| arc_path(cx, cy, 112.0, f))
        .filter(|s| !s.is_empty());
    let seek_arc = seek_fraction
        .map(|f| arc_path(cx, cy, 96.0, f))
        .filter(|s| !s.is_empty());

    let art_color = background_color.as_deref().unwrap_or("#1a1a2e");

    rsx! {
        div { class: "flex flex-col items-center",
            // Zone name above preview
            if let Some(ref name) = zone_name {
                div { class: "text-xs text-muted mb-2 truncate max-w-xs", "{name}" }
            }

            div {
                class: "relative mx-auto",
                style: "width: 240px; height: 240px;",

                // SVG layer — arcs
                svg {
                    class: "absolute inset-0",
                    width: "240",
                    height: "240",
                    view_box: "0 0 240 240",

                    // Track rings (dim background)
                    circle {
                        cx: "120", cy: "120", r: "112",
                        fill: "none",
                        stroke: "#2a2a3a",
                        stroke_width: "4",
                    }
                    circle {
                        cx: "120", cy: "120", r: "96",
                        fill: "none",
                        stroke: "#2a2a3a",
                        stroke_width: "3",
                    }

                    // Volume arc (outer, blue)
                    if let Some(ref path) = vol_arc {
                        path {
                            d: "{path}",
                            fill: "none",
                            stroke: "#4a9eff",
                            stroke_width: "4",
                            stroke_linecap: "round",
                        }
                    }

                    // Seek arc (inner, amber)
                    if let Some(ref path) = seek_arc {
                        path {
                            d: "{path}",
                            fill: "none",
                            stroke: "#f59e0b",
                            stroke_width: "3",
                            stroke_linecap: "round",
                        }
                    }
                }

                // Circular knob background
                div {
                    class: "absolute rounded-full",
                    style: "left: 16px; top: 16px; width: 208px; height: 208px; background: #111118; border: 1px solid #333;",

                    // Album art placeholder (colored circle in center)
                    div {
                        class: "absolute rounded-full",
                        style: "left: 54px; top: 28px; width: 100px; height: 100px; background: {art_color}; opacity: 0.7;",
                    }

                    // Text lines
                    div {
                        class: "absolute flex flex-col items-center justify-end px-6",
                        style: "left: 0; right: 0; top: 100px; bottom: 40px;",
                        for line in lines.iter() {
                            p {
                                class: match line.style.as_str() {
                                    "title" => "text-white text-xs font-semibold text-center w-full truncate",
                                    "subtitle" => "text-gray-400 text-xs text-center w-full truncate",
                                    _ => "text-gray-500 text-xs text-center w-full truncate",
                                },
                                "{line.text}"
                            }
                        }
                        if lines.is_empty() {
                            p { class: "text-gray-500 text-xs", "No manifest" }
                        }
                    }

                    // Transport buttons at bottom
                    div {
                        class: "absolute flex justify-center gap-3",
                        style: "bottom: 16px; left: 0; right: 0;",
                        for elem in elements.iter() {
                            if let Some(ref icon) = elem.display.icon {
                                span {
                                    class: if elem.display.active == Some(true) { "text-base text-blue-400" } else { "text-base text-gray-300" },
                                    title: elem.on_tap.as_ref().map(|a| a.action.as_str()).unwrap_or(""),
                                    {icon_char(icon)}
                                }
                            }
                        }
                        if elements.is_empty() {
                            span { class: "text-base text-gray-500", "\u{23EE}" }
                            span { class: "text-base text-gray-300",
                                if is_playing { "\u{23F8}" } else { "\u{25B6}" }
                            }
                            span { class: "text-base text-gray-500", "\u{23ED}" }
                        }
                    }
                }
            }
        }
    }
}

/// Rectangular preview for Frame / Tough devices
#[component]
fn RectPreview(
    lines: Vec<LineDef>,
    elements: Vec<ElementDef>,
    is_playing: bool,
    zone_name: Option<String>,
    background_color: Option<String>,
    rounded: bool,
) -> Element {
    let art_color = background_color.as_deref().unwrap_or("#1a1a2e");
    let border_radius = if rounded { "border-radius: 16px;" } else { "" };

    rsx! {
        div { class: "flex flex-col items-center",
            if let Some(ref name) = zone_name {
                div { class: "text-xs text-muted mb-2 truncate max-w-xs", "{name}" }
            }
            div {
                class: "relative mx-auto bg-gray-900 border border-gray-700",
                style: "width: 240px; height: 160px; {border_radius}",

                // Album art strip on left
                div {
                    class: "absolute top-0 left-0 bottom-0",
                    style: "width: 60px; background: {art_color}; opacity: 0.7; {border_radius}",
                }

                // Text lines
                div {
                    class: "absolute flex flex-col justify-center px-3 gap-1",
                    style: "left: 68px; right: 0; top: 16px; bottom: 40px;",
                    for line in lines.iter() {
                        p {
                            class: match line.style.as_str() {
                                "title" => "text-white text-sm font-semibold truncate",
                                "subtitle" => "text-gray-400 text-xs truncate",
                                _ => "text-gray-500 text-xs truncate",
                            },
                            "{line.text}"
                        }
                    }
                    if lines.is_empty() {
                        p { class: "text-gray-500 text-xs", "No manifest" }
                    }
                }

                // Transport buttons at bottom
                div {
                    class: "absolute flex gap-3 items-center",
                    style: "left: 68px; bottom: 12px;",
                    for elem in elements.iter() {
                        if let Some(ref icon) = elem.display.icon {
                            span {
                                class: if elem.display.active == Some(true) { "text-base text-blue-400" } else { "text-base text-gray-300" },
                                title: elem.on_tap.as_ref().map(|a| a.action.as_str()).unwrap_or(""),
                                {icon_char(icon)}
                            }
                        }
                    }
                    if elements.is_empty() {
                        span { class: "text-base text-gray-500", "\u{23EE}" }
                        span { class: "text-base text-gray-300",
                            if is_playing { "\u{23F8}" } else { "\u{25B6}" }
                        }
                        span { class: "text-base text-gray-500", "\u{23ED}" }
                    }
                }
            }
        }
    }
}

/// Compute an SVG arc path starting at 12 o'clock (top), going clockwise by `fraction` of a full circle.
/// Returns an empty string if fraction is 0.
fn arc_path(cx: f64, cy: f64, r: f64, fraction: f64) -> String {
    if fraction <= 0.0 {
        return String::new();
    }
    // Full circle: two half-arcs to avoid degenerate SVG arc
    if fraction >= 1.0 {
        return format!(
            "M {cx:.1},{y1:.1} A {r:.1},{r:.1} 0 0 1 {cx:.1},{y2:.1} A {r:.1},{r:.1} 0 0 1 {cx:.1},{y1:.1}",
            cx = cx, y1 = cy - r, y2 = cy + r, r = r
        );
    }
    use std::f64::consts::PI;
    let angle = fraction * 2.0 * PI;
    // Start at top (angle = -PI/2 from standard math convention)
    let x1 = cx;
    let y1 = cy - r;
    let x2 = cx + r * (angle - PI / 2.0).cos();
    let y2 = cy + r * (angle - PI / 2.0).sin();
    let large_arc = if angle > PI { 1 } else { 0 };
    format!(
        "M {x1:.1},{y1:.1} A {r:.1},{r:.1} 0 {large_arc} 1 {x2:.1},{y2:.1}",
        x1 = x1,
        y1 = y1,
        r = r,
        large_arc = large_arc,
        x2 = x2,
        y2 = y2
    )
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
