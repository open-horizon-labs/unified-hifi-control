//! Configurator page — describe what you want, see it on the device.
//!
//! Simple interface: device preview + natural language input.
//! The LLM generates command-pattern manifests from your description.
//! Includes layout save/load and per-device condition stack management.

use dioxus::prelude::*;

use crate::app::api::{self, KnobDevice, KnobDevicesResponse};
use crate::app::components::Layout;
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
    device_type: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Deserialize)]
struct LicenseStatus {
    configured: bool,
}

// ── Layout save/load types ──────────────────────────────────────────────────

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct LayoutSummary {
    id: String,
    name: String,
    #[serde(default)]
    screens: Vec<ScreenDef>,
    #[serde(default)]
    nav: Option<NavDef>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct NavDef {
    #[serde(default)]
    order: Vec<String>,
    #[serde(default)]
    default: String,
}

/// Request body for POST /api/layouts
#[derive(Clone, Debug, Default, serde::Serialize)]
struct SaveLayoutRequest {
    name: String,
    screens: Vec<ScreenDef>,
    nav: NavDef,
}

/// Wrapper for GET /api/layouts response
#[derive(Clone, Debug, Default, serde::Deserialize)]
struct LayoutsListResponse {
    #[serde(default)]
    layouts: Vec<LayoutSummary>,
}

// ── Condition stack types ───────────────────────────────────────────────────

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct ConditionDef {
    field: String,
    op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    value: Option<String>,
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, PartialEq)]
struct StackEntryDef {
    layout_id: String,
    conditions: Vec<ConditionDef>,
    #[serde(default = "default_true_val")]
    enabled: bool,
}
fn default_true_val() -> bool {
    true
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
struct DeviceStackResponse {
    device_id: String,
    entries: Vec<StackEntryDef>,
}

/// Request body for PUT /api/devices/{id}/stack
#[derive(Clone, Debug, Default, serde::Serialize)]
struct SetStackRequest {
    entries: Vec<StackEntryDef>,
}

// ── Page ─────────────────────────────────────────────────────────────────────

#[component]
pub fn Configurator() -> Element {
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

    // Device selector — shows actual registered ESP devices
    let devices = use_resource(|| async {
        api::fetch_json::<KnobDevicesResponse>("/knob/devices")
            .await
            .ok()
    });
    let mut selected_device_id = use_signal(|| None::<String>);

    // Preview shape — default to Dial (device_type not yet in KnobDevice)
    let device_type = "Dial";

    // Layout preview — set directly from generate response (no zone fetch needed).
    // The configurator creates layout templates; zones are a runtime condition, not an input.
    let mut manifest = use_signal(|| None::<ManifestResponse>);

    // LLM chat
    let mut chat_input = use_signal(String::new);
    let mut chat_messages = use_signal(Vec::<(bool, String)>::new); // (is_user, text)
    let mut llm_loading = use_signal(|| false);

    // ── Save Layout state ───────────────────────────────────────────────
    let mut show_save_input = use_signal(|| false);
    let mut save_name_input = use_signal(String::new);

    // ── Saved layouts list ──────────────────────────────────────────────
    let mut saved_layouts = use_signal(Vec::<LayoutSummary>::new);

    // Fetch layouts on mount
    let _layouts_loader = use_resource(move || async move {
        if let Ok(resp) = api::fetch_json::<LayoutsListResponse>("/api/layouts").await {
            saved_layouts.set(resp.layouts);
        }
    });

    // ── Condition stack state ───────────────────────────────────────────
    let mut stack_entries = use_signal(Vec::<StackEntryDef>::new);
    let mut stack_loading = use_signal(|| false);
    let mut show_add_entry = use_signal(|| false);
    // New entry form fields
    let mut new_entry_layout_id = use_signal(String::new);
    let mut new_entry_conditions = use_signal(Vec::<ConditionDef>::new);
    // Condition form fields for the "add condition" sub-form
    let mut new_cond_field = use_signal(|| "zone_name".to_string());
    let mut new_cond_op = use_signal(|| "equals".to_string());
    let mut new_cond_value = use_signal(String::new);

    // Fetch stack when device changes
    use_effect(move || {
        let dev_id = selected_device_id();
        spawn(async move {
            if let Some(ref did) = dev_id {
                stack_loading.set(true);
                let url = format!("/api/devices/{}/stack", did);
                match api::fetch_json::<DeviceStackResponse>(&url).await {
                    Ok(resp) => {
                        // Guard against stale response: only update if the
                        // user hasn't switched to a different device while
                        // the fetch was in flight.
                        if selected_device_id().as_deref() == Some(did.as_str()) {
                            stack_entries.set(resp.entries);
                        }
                    }
                    Err(_) => {
                        if selected_device_id().as_deref() == Some(did.as_str()) {
                            stack_entries.set(vec![]);
                        }
                    }
                }
                stack_loading.set(false);
            } else {
                stack_entries.set(vec![]);
            }
        });
    });

    let mut send_handler = move |_| {
        let input = chat_input().trim().to_string();
        if input.is_empty() || llm_loading() {
            return;
        }
        chat_messages.write().push((true, input.clone()));
        chat_input.set(String::new());
        llm_loading.set(true);

        let dt = device_type.to_lowercase();
        spawn(async move {
            let req = GenerateRequest {
                prompt: input,
                device_type: Some(dt),
            };
            // Server returns the generated manifest directly on success.
            // On error it returns {"error": "..."} which won't deserialize as ManifestResponse
            // but will fail in post_json (non-2xx) or produce empty screens.
            match api::post_json::<_, ManifestResponse>("/api/manifest/generate", &req).await {
                Ok(m) => {
                    manifest.set(Some(m));
                    chat_messages
                        .write()
                        .push((false, "ok:Layout updated.".to_string()));
                }
                Err(e) => {
                    chat_messages.write().push((false, format!("error:{}", e)));
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

    // Volume fraction (0.0-1.0) for the outer arc
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

    // Seek fraction (0.0-1.0) for the inner arc
    let seek_fraction = fast.as_ref().and_then(|f| {
        let pos = f.seek_position? as f64;
        let len = f.length? as f64;
        if len > 0.0 {
            Some((pos / len).clamp(0.0, 1.0))
        } else {
            None
        }
    });

    // Reset handler — clears the local preview
    let reset_handler = move |_| {
        manifest.set(None);
        chat_messages
            .write()
            .push((false, "ok:Layout cleared.".to_string()));
    };

    // Save layout handler
    let mut save_layout_handler = move |_| {
        let name = save_name_input().trim().to_string();
        if name.is_empty() {
            return;
        }
        let screens = manifest()
            .as_ref()
            .map(|m| m.screens.clone())
            .unwrap_or_default();
        if screens.is_empty() {
            chat_messages
                .write()
                .push((false, "error:No manifest to save.".to_string()));
            return;
        }
        let nav = NavDef {
            order: screens.iter().map(|s| s.id.clone()).collect(),
            default: screens.first().map(|s| s.id.clone()).unwrap_or_default(),
        };
        spawn(async move {
            let req = SaveLayoutRequest {
                name: name.clone(),
                screens,
                nav,
            };
            match api::post_json::<_, LayoutSummary>("/api/layouts", &req).await {
                Ok(layout) => {
                    chat_messages
                        .write()
                        .push((false, format!("ok:Layout \"{}\" saved.", layout.name)));
                    // Add to local list
                    saved_layouts.write().push(layout);
                    show_save_input.set(false);
                    save_name_input.set(String::new());
                }
                Err(e) => {
                    chat_messages
                        .write()
                        .push((false, format!("error:Save failed: {}", e)));
                }
            }
        });
    };

    // Delete layout handler
    let delete_layout = move |id: String| {
        spawn(async move {
            let url = format!("/api/layouts/{}", id);
            match api::delete_no_response(&url).await {
                Ok(()) => {
                    saved_layouts.write().retain(|l| l.id != id);
                    chat_messages
                        .write()
                        .push((false, "ok:Layout deleted.".to_string()));
                }
                Err(e) => {
                    chat_messages
                        .write()
                        .push((false, format!("error:Delete failed: {}", e)));
                }
            }
        });
    };

    // Load layout handler — sets preview to layout's screens
    let mut load_layout = move |layout: LayoutSummary| {
        manifest.set(Some(ManifestResponse {
            screens: layout.screens,
            ..Default::default()
        }));
        chat_messages
            .write()
            .push((false, format!("ok:Loaded layout \"{}\".", layout.name)));
    };

    // Save stack handler
    let save_stack_handler = move |_| {
        let Some(did) = selected_device_id() else {
            return;
        };
        let entries = stack_entries().clone();
        spawn(async move {
            let url = format!("/api/devices/{}/stack", did);
            let req = SetStackRequest { entries };
            match api::put_json::<_, DeviceStackResponse>(&url, &req).await {
                Ok(_) => {
                    chat_messages
                        .write()
                        .push((false, "ok:Condition stack saved.".to_string()));
                }
                Err(e) => {
                    chat_messages
                        .write()
                        .push((false, format!("error:Stack save failed: {}", e)));
                }
            }
        });
    };

    // Add condition to new entry form
    let mut add_condition_to_entry = move |_| {
        let field = new_cond_field();
        let op = new_cond_op();
        let value_str = new_cond_value().trim().to_string();
        let value = if field == "default" || value_str.is_empty() {
            None
        } else {
            Some(value_str)
        };
        new_entry_conditions
            .write()
            .push(ConditionDef { field, op, value });
        new_cond_value.set(String::new());
    };

    // Add entry to stack
    let mut add_entry_to_stack = move |_| {
        let layout_id = new_entry_layout_id();
        if layout_id.is_empty() {
            return;
        }
        let conditions = new_entry_conditions().clone();
        stack_entries.write().push(StackEntryDef {
            layout_id,
            conditions,
            enabled: true,
        });
        // Reset form
        new_entry_layout_id.set(String::new());
        new_entry_conditions.set(vec![]);
        show_add_entry.set(false);
    };

    // Build a quick lookup: layout_id -> name for the stack display
    let layouts_snapshot = saved_layouts();

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

                // Left column: preview + layouts + stack
                div {
                    // Device selector
                    div { class: "mb-4",
                        label { class: "block text-xs text-muted mb-1", "Device" }
                        select {
                            class: "input text-sm",
                            value: selected_device_id().unwrap_or_default(),
                            onchange: move |e| {
                                let v = e.value();
                                selected_device_id.set(if v.is_empty() { None } else { Some(v) });
                            },
                            option { value: "", "Select a device..." }
                            if let Some(Some(ref dr)) = *devices.read() {
                                for knob in dr.knobs.iter() {
                                    option {
                                        value: "{knob.knob_id}",
                                        selected: selected_device_id() == Some(knob.knob_id.clone()),
                                        {device_display_name(knob)}
                                    }
                                }
                            }
                        }
                    }

                    // Device preview
                    ScreenPreview {
                        lines: preview_lines,
                        elements: preview_elements,
                        is_playing,
                        device_type: device_type.to_string(),
                        zone_name: None,
                        background_color: preview_bg_color,
                        volume_fraction,
                        seek_fraction,
                    }

                    // Reset + Save Layout buttons
                    if manifest().is_some() {
                        div { class: "flex gap-2 mt-3 items-center flex-wrap",
                            button {
                                class: "btn btn-sm text-xs opacity-60 hover:opacity-100",
                                onclick: reset_handler,
                                "Reset to default"
                            }
                            if !show_save_input() {
                                button {
                                    class: "btn btn-primary btn-sm text-xs",
                                    onclick: move |_| show_save_input.set(true),
                                    "Save Layout"
                                }
                            }
                        }

                        // Inline save name input
                        if show_save_input() {
                            div { class: "flex gap-2 mt-2 items-center",
                                input {
                                    class: "input text-sm flex-1",
                                    placeholder: "Layout name...",
                                    value: "{save_name_input}",
                                    oninput: move |e| save_name_input.set(e.value()),
                                    onkeypress: move |e| {
                                        if e.key() == Key::Enter {
                                            save_layout_handler(());
                                        }
                                    },
                                }
                                button {
                                    class: "btn btn-primary btn-sm text-xs",
                                    disabled: save_name_input().trim().is_empty(),
                                    onclick: move |_| save_layout_handler(()),
                                    "Save"
                                }
                                button {
                                    class: "btn btn-sm text-xs opacity-60 hover:opacity-100",
                                    onclick: move |_| {
                                        show_save_input.set(false);
                                        save_name_input.set(String::new());
                                    },
                                    "Cancel"
                                }
                            }
                        }
                    }

                    // ── Saved Layouts ────────────────────────────────────
                    div { class: "card p-3 mt-4",
                        h4 { class: "text-xs font-semibold mb-2", "Saved Layouts" }
                        if saved_layouts().is_empty() {
                            p { class: "text-xs text-muted", "No saved layouts yet." }
                        } else {
                            div { class: "space-y-1",
                                for layout in saved_layouts().iter() {
                                    {
                                        let layout_for_load = layout.clone();
                                        let layout_id_for_delete = layout.id.clone();
                                        let layout_name = layout.name.clone();
                                        rsx! {
                                            div { class: "flex items-center justify-between gap-2 py-1",
                                                span { class: "text-sm truncate flex-1", "{layout_name}" }
                                                button {
                                                    class: "btn btn-sm text-xs",
                                                    onclick: move |_| load_layout(layout_for_load.clone()),
                                                    "Load"
                                                }
                                                button {
                                                    class: "btn btn-sm text-xs text-red-400 hover:text-red-300 opacity-60 hover:opacity-100",
                                                    onclick: move |_| delete_layout(layout_id_for_delete.clone()),
                                                    "X"
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // ── Condition Stack ──────────────────────────────────
                    if selected_device_id().is_some() {
                        div { class: "card p-3 mt-4",
                            div { class: "flex items-center justify-between mb-2",
                                h4 { class: "text-xs font-semibold", "Condition Stack" }
                                button {
                                    class: "btn btn-primary btn-sm text-xs",
                                    onclick: move |_| save_stack_handler(()),
                                    "Save Stack"
                                }
                            }

                            if stack_loading() {
                                p { class: "text-xs text-muted animate-pulse", "Loading..." }
                            } else if stack_entries().is_empty() && !show_add_entry() {
                                p { class: "text-xs text-muted", "No stack entries. Add one to define conditional layouts." }
                            }

                            // Existing entries
                            if !stack_entries().is_empty() {
                                div { class: "space-y-2 mb-3",
                                    for (idx, entry) in stack_entries().iter().enumerate() {
                                        {
                                            // Resolve layout name from saved layouts
                                            let entry_layout_name = layouts_snapshot
                                                .iter()
                                                .find(|l| l.id == entry.layout_id)
                                                .map(|l| l.name.clone())
                                                .unwrap_or_else(|| entry.layout_id.clone());
                                            let conditions_summary = if entry.conditions.is_empty() {
                                                "No conditions".to_string()
                                            } else {
                                                entry
                                                    .conditions
                                                    .iter()
                                                    .map(|c| {
                                                        if let Some(ref v) = c.value {
                                                            format!("{} {} {}", c.field, c.op, v)
                                                        } else {
                                                            format!("{} {}", c.field, c.op)
                                                        }
                                                    })
                                                    .collect::<Vec<_>>()
                                                    .join(", ")
                                            };
                                            let entry_enabled = entry.enabled;
                                            rsx! {
                                                div { class: "flex items-center gap-2 py-1 border-b border-gray-700",
                                                    // Enabled toggle
                                                    input {
                                                        r#type: "checkbox",
                                                        checked: entry_enabled,
                                                        onchange: move |e: Event<FormData>| {
                                                            let checked = e.value() == "true";
                                                            stack_entries.write()[idx].enabled = checked;
                                                        },
                                                    }
                                                    div { class: "flex-1 min-w-0",
                                                        div { class: "text-sm truncate", "{entry_layout_name}" }
                                                        div { class: "text-xs text-muted truncate", "{conditions_summary}" }
                                                    }
                                                    // Remove entry
                                                    button {
                                                        class: "btn btn-sm text-xs text-red-400 hover:text-red-300 opacity-60 hover:opacity-100",
                                                        onclick: move |_| {
                                                            stack_entries.write().remove(idx);
                                                        },
                                                        "X"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }

                            // Add Entry button / form
                            if !show_add_entry() {
                                button {
                                    class: "btn btn-sm text-xs mt-1",
                                    onclick: move |_| show_add_entry.set(true),
                                    "+ Add Entry"
                                }
                            } else {
                                div { class: "border border-gray-700 rounded p-2 mt-2 space-y-2",
                                    // Layout dropdown
                                    div {
                                        label { class: "block text-xs text-muted mb-1", "Layout" }
                                        select {
                                            class: "input text-sm",
                                            value: "{new_entry_layout_id}",
                                            onchange: move |e| new_entry_layout_id.set(e.value()),
                                            option { value: "", "Select layout..." }
                                            for layout in saved_layouts().iter() {
                                                option {
                                                    value: "{layout.id}",
                                                    "{layout.name}"
                                                }
                                            }
                                        }
                                    }

                                    // Current conditions for this entry
                                    if !new_entry_conditions().is_empty() {
                                        div { class: "space-y-1",
                                            for (ci, cond) in new_entry_conditions().iter().enumerate() {
                                                {
                                                    let cond_display = if let Some(ref v) = cond.value {
                                                        format!("{} {} {}", cond.field, cond.op, v)
                                                    } else {
                                                        format!("{} {}", cond.field, cond.op)
                                                    };
                                                    rsx! {
                                                        div { class: "flex items-center gap-1 text-xs",
                                                            span { class: "flex-1 truncate", "{cond_display}" }
                                                            button {
                                                                class: "text-red-400 hover:text-red-300 text-xs",
                                                                onclick: move |_| {
                                                                    new_entry_conditions.write().remove(ci);
                                                                },
                                                                "x"
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }

                                    // Add condition sub-form
                                    div { class: "flex gap-1 items-end flex-wrap",
                                        div {
                                            label { class: "block text-xs text-muted", "Field" }
                                            select {
                                                class: "input text-xs",
                                                value: "{new_cond_field}",
                                                onchange: move |e| new_cond_field.set(e.value()),
                                                option { value: "zone_name", "zone_name" }
                                                option { value: "source", "source" }
                                                option { value: "genre", "genre" }
                                                option { value: "artist", "artist" }
                                                option { value: "duration", "duration" }
                                                option { value: "default", "default" }
                                            }
                                        }
                                        div {
                                            label { class: "block text-xs text-muted", "Op" }
                                            select {
                                                class: "input text-xs",
                                                value: "{new_cond_op}",
                                                onchange: move |e| new_cond_op.set(e.value()),
                                                option { value: "equals", "equals" }
                                                option { value: "contains", "contains" }
                                                option { value: "greater_than", "greater_than" }
                                                option { value: "always", "always" }
                                            }
                                        }
                                        div { class: "flex-1",
                                            label { class: "block text-xs text-muted", "Value" }
                                            input {
                                                class: "input text-xs w-full",
                                                placeholder: "value",
                                                value: "{new_cond_value}",
                                                oninput: move |e| new_cond_value.set(e.value()),
                                            }
                                        }
                                        button {
                                            class: "btn btn-sm text-xs",
                                            onclick: move |_| add_condition_to_entry(()),
                                            "+ Cond"
                                        }
                                    }

                                    // Add entry / cancel buttons
                                    div { class: "flex gap-2 pt-1",
                                        button {
                                            class: "btn btn-primary btn-sm text-xs",
                                            disabled: new_entry_layout_id().is_empty(),
                                            onclick: move |_| add_entry_to_stack(()),
                                            "Add Entry"
                                        }
                                        button {
                                            class: "btn btn-sm text-xs opacity-60 hover:opacity-100",
                                            onclick: move |_| {
                                                show_add_entry.set(false);
                                                new_entry_layout_id.set(String::new());
                                                new_entry_conditions.set(vec![]);
                                                new_cond_value.set(String::new());
                                            },
                                            "Cancel"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                // Right column: chat
                div { class: "card p-4",
                    h3 { class: "text-sm font-semibold mb-3", "Describe your layout" }
                    p { class: "text-xs text-muted mb-3",
                        "Describe what you want, or tap an element below to add it."
                    }

                    // ── Element Palette ──────────────────────────────────
                    ElementPalette { on_select: move |desc: String| {
                        let current = chat_input().trim().to_string();
                        if current.is_empty() {
                            chat_input.set(desc);
                        } else {
                            chat_input.set(format!("{}, {}", current, desc.to_lowercase()));
                        }
                    }}

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

// ── Element Palette ──────────────────────────────────────────────────────────

/// Available element definition for the palette.
struct PaletteElement {
    icon: &'static str,
    label: &'static str,
    description: &'static str,
    category: &'static str,
}

/// Returns the available elements grouped by category.
fn palette_elements() -> Vec<PaletteElement> {
    vec![
        // Transport
        PaletteElement {
            icon: "\u{25B6}",
            label: "Play/Pause",
            description: "Add a play/pause toggle",
            category: "Transport",
        },
        PaletteElement {
            icon: "\u{23EE}",
            label: "Previous",
            description: "Add a previous track button",
            category: "Transport",
        },
        PaletteElement {
            icon: "\u{23ED}",
            label: "Next",
            description: "Add a next track button",
            category: "Transport",
        },
        PaletteElement {
            icon: "\u{23F9}",
            label: "Stop",
            description: "Add a stop button",
            category: "Transport",
        },
        PaletteElement {
            icon: "\u{1F500}",
            label: "Shuffle",
            description: "Add a shuffle toggle",
            category: "Transport",
        },
        PaletteElement {
            icon: "\u{1F501}",
            label: "Repeat",
            description: "Add a repeat toggle",
            category: "Transport",
        },
        // Seek
        PaletteElement {
            icon: "30\u{00BB}",
            label: "+30s",
            description: "Add a 30-second forward skip",
            category: "Seek",
        },
        PaletteElement {
            icon: "10\u{00BB}",
            label: "+10s",
            description: "Add a 10-second forward skip",
            category: "Seek",
        },
        PaletteElement {
            icon: "\u{00AB}30",
            label: "-30s",
            description: "Add a 30-second replay",
            category: "Seek",
        },
        PaletteElement {
            icon: "\u{00AB}10",
            label: "-10s",
            description: "Add a 10-second replay",
            category: "Seek",
        },
        // Volume
        PaletteElement {
            icon: "\u{1F507}",
            label: "Mute",
            description: "Add a mute toggle",
            category: "Volume",
        },
    ]
}

/// Compact element palette — shows available elements the user can tap to add to their prompt.
#[component]
fn ElementPalette(on_select: EventHandler<String>) -> Element {
    let mut expanded = use_signal(|| false);

    let elements = palette_elements();
    let categories: Vec<&str> = {
        let mut cats: Vec<&str> = elements.iter().map(|e| e.category).collect();
        cats.dedup();
        cats
    };

    rsx! {
        div { class: "mb-4",
            button {
                class: "text-xs text-muted hover:text-white transition-colors flex items-center gap-1",
                onclick: move |_| expanded.toggle(),
                if expanded() {
                    span { "\u{25BE} Available elements" }
                } else {
                    span { "\u{25B8} Available elements" }
                }
            }

            if expanded() {
                div { class: "mt-2 space-y-2",
                    for cat in categories.iter() {
                        div {
                            p { class: "text-xs text-muted mb-1 font-medium", "{cat}" }
                            div { class: "flex flex-wrap gap-1",
                                for elem in elements.iter().filter(|e| e.category == *cat) {
                                    {
                                        let desc = elem.description.to_string();
                                        let label = elem.label;
                                        let icon = elem.icon;
                                        rsx! {
                                            button {
                                                class: "group flex items-center gap-1 px-2 py-1 rounded text-xs bg-gray-800 hover:bg-gray-700 border border-gray-700 hover:border-gray-500 transition-all",
                                                title: "{label}",
                                                onclick: move |_| on_select.call(desc.clone()),
                                                span { class: "text-sm", "{icon}" }
                                                span { class: "text-muted group-hover:text-white transition-colors", "{label}" }
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

/// Circular knob preview (Dial device) — pure SVG rendering.
///
/// Layout mirrors the physical HiPhi Dial: a circular body with encoder
/// ring arcs surrounding a square LCD display area, all in one SVG.
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
    let size = 280.0_f64;
    let cx = size / 2.0;
    let cy = size / 2.0;
    let vol_arc = volume_fraction
        .map(|f| arc_path(cx, cy, 134.0, f))
        .filter(|s| !s.is_empty());
    let seek_arc = seek_fraction
        .map(|f| arc_path(cx, cy, 124.0, f))
        .filter(|s| !s.is_empty());

    let art_color = background_color.as_deref().unwrap_or("#1a1a2e");

    // Build transport button text: collect icon chars + active state
    let transport_icons: Vec<(&str, bool)> = if elements.is_empty() {
        vec![
            ("\u{23EE}", false),
            (if is_playing { "\u{23F8}" } else { "\u{25B6}" }, true),
            ("\u{23ED}", false),
        ]
    } else {
        elements
            .iter()
            .filter_map(|e| {
                e.display
                    .icon
                    .as_ref()
                    .map(|icon| (icon_char(icon), e.display.active == Some(true)))
            })
            .collect()
    };

    // Build text lines for SVG — up to 3 lines
    let display_lines: Vec<(&str, &str)> = if lines.is_empty() {
        vec![("No manifest", "#6b7280")]
    } else {
        lines
            .iter()
            .take(3)
            .map(|l| {
                let color = match l.style.as_str() {
                    "title" => "#ffffff",
                    "subtitle" => "#9ca3af",
                    _ => "#6b7280",
                };
                (l.text.as_str(), color)
            })
            .collect()
    };

    // LCD display area: 170x170 centered
    let lcd_half = 85.0;

    rsx! {
        div { class: "flex flex-col items-center",
            if let Some(ref name) = zone_name {
                div { class: "text-xs text-muted mb-2 truncate max-w-xs", "{name}" }
            }

            svg {
                width: "280",
                height: "280",
                view_box: "0 0 280 280",
                style: "display: block;",

                // Circular body
                circle {
                    cx: "{cx}", cy: "{cy}", r: "139",
                    fill: "#1a1a24",
                }

                // Track rings (dim)
                circle {
                    cx: "{cx}", cy: "{cy}", r: "134",
                    fill: "none", stroke: "#2a2a3a", stroke_width: "4",
                }
                circle {
                    cx: "{cx}", cy: "{cy}", r: "124",
                    fill: "none", stroke: "#2a2a3a", stroke_width: "3",
                }

                // Volume arc (outer, blue)
                if let Some(ref path) = vol_arc {
                    path {
                        d: "{path}",
                        fill: "none", stroke: "#4a9eff",
                        stroke_width: "4", stroke_linecap: "round",
                    }
                }

                // Seek arc (inner, amber)
                if let Some(ref path) = seek_arc {
                    path {
                        d: "{path}",
                        fill: "none", stroke: "#f59e0b",
                        stroke_width: "3", stroke_linecap: "round",
                    }
                }

                // LCD display area background
                rect {
                    x: "{cx - lcd_half}", y: "{cy - lcd_half}",
                    width: "{lcd_half * 2.0}", height: "{lcd_half * 2.0}",
                    fill: "#111118",
                }

                // Album art placeholder
                rect {
                    x: "{cx - 40.0}", y: "{cy - lcd_half + 12.0}",
                    width: "80", height: "80",
                    fill: "{art_color}", opacity: "0.7",
                }

                // Text lines — positioned below art area
                for (i, (text, color)) in display_lines.iter().enumerate() {
                    text {
                        x: "{cx}", y: "{cy + 20.0 + (i as f64 * 16.0)}",
                        text_anchor: "middle",
                        fill: "{color}",
                        font_size: if i == 0 { "12" } else { "10" },
                        font_weight: if i == 0 { "600" } else { "400" },
                        font_family: "system-ui, sans-serif",
                        "{text}"
                    }
                }

                // Transport buttons at bottom of LCD
                for (i, (icon, active)) in transport_icons.iter().enumerate() {
                    {
                        let total = transport_icons.len() as f64;
                        let spacing = 28.0;
                        let start_x = cx - (total - 1.0) * spacing / 2.0;
                        let bx = start_x + (i as f64) * spacing;
                        let by = cy + lcd_half - 16.0;
                        let fill = if *active { "#60a5fa" } else { "#d1d5db" };
                        rsx! {
                            text {
                                x: "{bx}", y: "{by}",
                                text_anchor: "middle",
                                fill: "{fill}",
                                font_size: "18",
                                "{icon}"
                            }
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

/// Display name for a device in the dropdown: name, IP, or knob_id fallback.
fn device_display_name(knob: &KnobDevice) -> String {
    if let Some(ref name) = knob.name {
        if !name.is_empty() {
            return name.clone();
        }
    }
    // Fall back to IP if available
    if let Some(ref status) = knob.status {
        if let Some(ref ip) = status.ip {
            if !ip.is_empty() {
                return format!("Device @ {}", ip);
            }
        }
    }
    // Last resort: truncated knob_id
    if knob.knob_id.len() > 12 {
        format!("{}...", &knob.knob_id[..12])
    } else {
        knob.knob_id.clone()
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
