//! Settings page component.
//!
//! Adapter settings and discovery status using Dioxus resources.

use dioxus::prelude::*;

use crate::app::api::{
    source_label, AdapterSettings, AppSettings, HqpStatus, LmsConfig, ManagedZone,
    ManagedZonesResponse, MoveDirection, RoonStatus, ZoneNameRequest, ZoneOrderRequest,
    ZoneVisibilityRequest,
};
use crate::app::components::ErrorAlert;
use crate::app::components::Layout;
use crate::app::settings_context::use_settings;
use crate::app::sse::use_sse;
use crate::app::theme::{use_theme, Theme};
use crate::app::McpEndpoint;

/// OpenHome status response
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
struct OpenHomeStatus {
    device_count: usize,
}

/// UPnP status response
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
struct UpnpStatus {
    renderer_count: usize,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum CopyState {
    #[default]
    Idle,
    #[cfg(target_arch = "wasm32")]
    Copying,
    #[cfg(target_arch = "wasm32")]
    Copied,
    #[cfg(target_arch = "wasm32")]
    Failed,
}

impl CopyState {
    fn label(self, idle_label: &'static str) -> &'static str {
        match self {
            Self::Idle => idle_label,
            #[cfg(target_arch = "wasm32")]
            Self::Copying => "Copying…",
            #[cfg(target_arch = "wasm32")]
            Self::Copied => "Copied",
            #[cfg(target_arch = "wasm32")]
            Self::Failed => "Copy failed",
        }
    }
}

fn copy_to_clipboard(value: String, state: Signal<CopyState>) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;

        let mut state = state;

        let Some(window) = web_sys::window() else {
            state.set(CopyState::Failed);
            return;
        };

        let clipboard = js_sys::Reflect::get(
            window.navigator().as_ref(),
            &wasm_bindgen::JsValue::from_str("clipboard"),
        )
        .ok()
        .filter(|value| !value.is_null() && !value.is_undefined())
        .and_then(|value| value.dyn_into::<web_sys::Clipboard>().ok());

        if let Some(clipboard) = clipboard {
            state.set(CopyState::Copying);
            let promise = clipboard.write_text(&value);
            wasm_bindgen_futures::spawn_local(async move {
                let next = if wasm_bindgen_futures::JsFuture::from(promise).await.is_ok() {
                    CopyState::Copied
                } else {
                    CopyState::Failed
                };
                show_copy_result(state, next);
            });
            return;
        }

        let next = if copy_to_clipboard_legacy(&window, &value) {
            CopyState::Copied
        } else {
            CopyState::Failed
        };
        show_copy_result(state, next);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // Event handlers run in the hydrated WASM client, never during SSR.
        let _ = (value, state);
    }
}

#[cfg(target_arch = "wasm32")]
fn show_copy_result(mut state: Signal<CopyState>, result: CopyState) {
    state.set(result);
    spawn(async move {
        dioxus_sdk_time::sleep(std::time::Duration::from_secs(2)).await;
        if state() == result {
            state.set(CopyState::Idle);
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn copy_to_clipboard_legacy(window: &web_sys::Window, value: &str) -> bool {
    use wasm_bindgen::JsCast;

    let result = (|| -> Result<bool, ()> {
        let document = window.document().ok_or(())?;
        let html_document = document
            .clone()
            .dyn_into::<web_sys::HtmlDocument>()
            .map_err(|_| ())?;
        let textarea = document
            .create_element("textarea")
            .map_err(|_| ())?
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .map_err(|_| ())?;

        textarea.set_value(value);
        textarea.set_attribute("readonly", "").map_err(|_| ())?;
        textarea
            .set_attribute("aria-hidden", "true")
            .map_err(|_| ())?;
        textarea
            .style()
            .set_property("position", "fixed")
            .map_err(|_| ())?;
        textarea
            .style()
            .set_property("inset-inline-start", "-9999px")
            .map_err(|_| ())?;

        let body = document.body().ok_or(())?;
        body.append_child(&textarea).map_err(|_| ())?;
        let _ = textarea.focus();
        textarea.select();

        let copied = html_document.exec_command("copy");
        textarea.remove();

        copied.map_err(|_| ())
    })();

    result.unwrap_or(false)
}

/// Settings page component.
#[component]
pub fn Settings() -> Element {
    let sse = use_sse();
    let theme_ctx = use_theme();
    let settings_ctx = use_settings();
    let mcp_endpoint = use_context::<McpEndpoint>();
    let agent_config = mcp_endpoint.agent_config();
    let config_copy_state = use_signal(CopyState::default);
    let url_copy_state = use_signal(CopyState::default);

    // Adapter toggle signals
    let mut roon_enabled = use_signal(|| true);
    let mut lms_enabled = use_signal(|| false);
    let mut openhome_enabled = use_signal(|| false);
    let mut upnp_enabled = use_signal(|| false);
    let mut hqplayer_enabled = use_signal(|| false);

    // Hide knobs signal (LMS/HQPlayer visibility follows adapter enabled state)
    let mut hide_knobs = use_signal(|| false);

    // Load settings resource
    let settings = use_resource(|| async {
        crate::app::api::fetch_json::<AppSettings>("/api/settings")
            .await
            .ok()
    });

    // Sync settings to signals when loaded
    use_effect(move || {
        if let Some(Some(s)) = settings.read().as_ref() {
            roon_enabled.set(s.adapters.roon);
            lms_enabled.set(s.adapters.lms);
            openhome_enabled.set(s.adapters.openhome);
            upnp_enabled.set(s.adapters.upnp);
            hqplayer_enabled.set(s.adapters.hqplayer);
            hide_knobs.set(s.hide_knobs_page);
            // Sync to shared context for Nav reactivity (page visibility follows adapter state)
            settings_ctx.update(s.hide_knobs_page, s.adapters.hqplayer, s.adapters.lms);
            settings_ctx.mark_loaded();
        }
    });

    // Zone management: every zone the user can manage, hidden ones included. Deliberately not
    // `/zones`, which excludes hidden zones -- this is the one view that has to show them, or
    // hiding would be a one-way door.
    let mut managed_zones = use_resource(|| async {
        crate::app::api::fetch_json::<ManagedZonesResponse>("/api/zones/visibility")
            .await
            .ok()
    });

    // Announced to assistive tech after every zone change. Without it a screen-reader user presses
    // "move up" and hears nothing at all -- the row moves silently.
    let mut zone_status = use_signal(String::new);
    let mut zone_error = use_signal(|| Option::<String>::None);
    // Bumped on a failed write. It is part of each row's key, so a failure forces Dioxus to recreate
    // the row's DOM nodes. Without that, a rejected checkbox click leaves the browser's natively
    // toggled checkbox showing the opposite of the truth: the refetched data is unchanged, so the
    // VDOM `checked` value is unchanged, so the diff emits no correcting mutation.
    let mut zone_revision = use_signal(|| 0u32);
    // In-progress name edits, by zone id.
    //
    // `value:` on a Dioxus input makes it *controlled*: after every input event the DOM value is
    // reset to whatever the VDOM says. With only an `onchange` handler -- which fires on blur, not
    // per keystroke -- nothing held the typed text, so each character was immediately overwritten by
    // the server's name and the field appeared to reject typing. Every other text input in this app
    // pairs `value:` with an `oninput` that writes to a signal; this is that signal, per row.
    //
    // A draft is cleared once the rename commits, so the field falls back to the server's answer --
    // which is what makes the "reset" button visibly restore the provider's name.
    let mut name_drafts = use_signal(std::collections::HashMap::<String, String>::new);

    let mut report_zone_failure = move |err: String| {
        zone_error.set(Some(err));
        zone_revision += 1;
    };

    let set_zone_hidden = move |zone: ManagedZone| {
        let label = zone.qualified_label();
        let now_hidden = !zone.hidden;
        spawn(async move {
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/visibility",
                &ZoneVisibilityRequest {
                    zone_id: zone.zone_id.clone(),
                    hidden: now_hidden,
                },
            )
            .await;
            match result {
                Ok(_) => {
                    zone_error.set(None);
                    zone_status.set(if now_hidden {
                        format!("{label} hidden.")
                    } else {
                        format!("{label} shown.")
                    });
                }
                Err(err) => report_zone_failure(err),
            }
            managed_zones.restart();
        });
    };

    let move_zone = move |zone: ManagedZone, direction: MoveDirection, at_boundary: bool| {
        let label = zone.qualified_label();
        spawn(async move {
            if at_boundary {
                // The control stays focusable at the boundary rather than becoming `disabled`,
                // because a disabled element cannot hold focus -- pressing "up" on the second row
                // would move the zone to first, disable the button under the user's finger, and drop
                // focus to <body>. Saying so is better than silence.
                zone_status.set(match direction {
                    MoveDirection::Up => format!("{label} is already first."),
                    MoveDirection::Down => format!("{label} is already last."),
                });
                return;
            }
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/order",
                &ZoneOrderRequest::step(zone.zone_id.clone(), direction),
            )
            .await;
            match result {
                Ok(_) => {
                    zone_error.set(None);
                    zone_status.set(match direction {
                        MoveDirection::Up => format!("{label} moved up."),
                        MoveDirection::Down => format!("{label} moved down."),
                    });
                }
                Err(err) => report_zone_failure(err),
            }
            managed_zones.restart();
        });
    };

    // Drag-and-drop state. HTML5 drag events do not fire on touch devices, so this is an
    // enhancement layered over the up/down buttons rather than a replacement for them -- the buttons
    // remain the path for phones and for keyboard users.
    let mut dragging = use_signal(|| Option::<String>::None);
    let mut drop_target = use_signal(|| Option::<String>::None);

    let drop_zone_onto = move |zone: ManagedZone, target: ManagedZone| {
        let label = zone.qualified_label();
        let target_label = target.qualified_label();
        spawn(async move {
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/order",
                &ZoneOrderRequest::drop_onto(zone.zone_id.clone(), target.zone_id.clone()),
            )
            .await;
            match result {
                Ok(_) => {
                    zone_error.set(None);
                    zone_status.set(format!("{label} moved to {target_label}'s position."));
                }
                Err(err) => report_zone_failure(err),
            }
            managed_zones.restart();
        });
    };

    let mut rename_zone = move |zone: ManagedZone, name: String| {
        let trimmed = name.trim().to_string();
        if trimmed == zone.zone_name {
            // Nothing to save, but drop the draft anyway so a field edited only by whitespace snaps
            // back to the canonical name instead of keeping the user's spacing.
            name_drafts.write().remove(&zone.zone_id);
            return;
        }
        spawn(async move {
            let cleared = trimmed.is_empty();
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/name",
                &ZoneNameRequest {
                    zone_id: zone.zone_id.clone(),
                    name: if cleared { None } else { Some(trimmed.clone()) },
                },
            )
            .await;
            match result {
                Ok(_) => {
                    zone_error.set(None);
                    zone_status.set(if cleared {
                        format!("Name reset to {}.", zone.provider_name)
                    } else {
                        format!("Renamed to {trimmed}.")
                    });
                }
                Err(err) => report_zone_failure(err),
            }
            // Either way the server is now authoritative for this row: on success it holds the new
            // name, on failure the old one, and in both cases that is what the field should show.
            name_drafts.write().remove(&zone.zone_id);
            managed_zones.restart();
        });
    };

    // Discovery status resources
    let mut roon_status = use_resource(|| async {
        crate::app::api::fetch_json::<RoonStatus>("/roon/status")
            .await
            .ok()
    });
    let mut openhome_status = use_resource(|| async {
        crate::app::api::fetch_json::<OpenHomeStatus>("/openhome/status")
            .await
            .ok()
    });
    let mut upnp_status = use_resource(|| async {
        crate::app::api::fetch_json::<UpnpStatus>("/upnp/status")
            .await
            .ok()
    });
    let mut lms_config = use_resource(|| async {
        crate::app::api::fetch_json::<LmsConfig>("/lms/config")
            .await
            .ok()
    });
    let mut hqp_status = use_resource(|| async {
        crate::app::api::fetch_json::<HqpStatus>("/hqplayer/status")
            .await
            .ok()
    });

    // Refresh discovery on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_discovery() {
            roon_status.restart();
            openhome_status.restart();
            upnp_status.restart();
            lms_config.restart();
            hqp_status.restart();
        }
        // The zone set changes while the user is on this page -- they power on a speaker precisely
        // because they are configuring it. Without this the table only updates on a manual reload.
        if sse.should_refresh_zones() {
            managed_zones.restart();
        }
    });

    // Save settings handler
    let save_settings = move || {
        let hk = hide_knobs();
        let hqp = hqplayer_enabled();
        let lms = lms_enabled();

        // Update shared context immediately for reactive Nav updates
        settings_ctx.update(hk, hqp, lms);

        let settings = AppSettings {
            adapters: AdapterSettings {
                roon: roon_enabled(),
                lms,
                openhome: openhome_enabled(),
                upnp: upnp_enabled(),
                hqplayer: hqp,
            },
            hide_knobs_page: hk,
            // These are now derived from adapter state but we keep them for API compat
            hide_hqp_page: !hqp,
            hide_lms_page: !lms,
        };
        spawn(async move {
            let _ = crate::app::api::post_json_no_response("/api/settings", &settings).await;
            // Enabling or disabling an adapter changes which zones are manageable, and the zone
            // table sits directly below these toggles. Without this it keeps listing zones from an
            // adapter the user just turned off.
            managed_zones.restart();
        });
    };

    let roon_st = roon_status.read().clone().flatten();
    let openhome_st = openhome_status.read().clone().flatten();
    let upnp_st = upnp_status.read().clone().flatten();
    let lms_cfg = lms_config.read().clone().flatten();
    let hqp_st = hqp_status.read().clone().flatten();

    rsx! {
        Layout {
            title: "Settings".to_string(),
            nav_active: "settings".to_string(),

            h1 { class: "text-2xl font-bold mb-6", "Settings" }

            // Features section (adapters + page visibility)
            section { class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Features" }
                    p { class: "text-muted text-sm", "Zone sources and page visibility" }
                }

                div { class: "card overflow-x-auto p-4 sm:p-6",
                    table { class: "w-full", id: "features-table",
                        thead {
                            tr { class: "border-b border-default",
                                th { class: "text-left py-2 px-3 font-semibold w-12", "" }
                                th { class: "text-left py-2 px-3 font-semibold", "Feature" }
                                th { class: "text-left py-2 px-3 font-semibold", "Status" }
                            }
                        }
                        tbody {
                            // Roon (adapter only, no dedicated page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable Roon",
                                            checked: roon_enabled(),
                                            onchange: move |_| {
                                                roon_enabled.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Roon" }
                                td { class: "py-2 px-3",
                                    if roon_enabled() {
                                        if let Some(ref status) = roon_st {
                                            if status.connected {
                                                if let Some(ref name) = status.core_name {
                                                    span { class: "status-ok", "✓ {name}" }
                                                } else {
                                                    span { class: "status-ok", "✓ Core" }
                                                }
                                            } else {
                                                span { class: "status-err", "✗ Not connected" }
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // OpenHome (adapter only, no dedicated page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable OpenHome",
                                            checked: openhome_enabled(),
                                            onchange: move |_| {
                                                openhome_enabled.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "OpenHome" }
                                td { class: "py-2 px-3",
                                    if openhome_enabled() {
                                        if let Some(ref status) = openhome_st {
                                            if status.device_count > 0 {
                                                span { class: "status-ok", "✓ {status.device_count} devices" }
                                            } else {
                                                "Searching..."
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // UPnP/DLNA
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable UPnP/DLNA",
                                            checked: upnp_enabled(),
                                            onchange: move |_| {
                                                upnp_enabled.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "UPnP/DLNA" }
                                td { class: "py-2 px-3",
                                    if upnp_enabled() {
                                        if let Some(ref status) = upnp_st {
                                            if status.renderer_count > 0 {
                                                span { class: "status-ok", "✓ {status.renderer_count} renderers" }
                                            } else {
                                                "Searching..."
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // LMS (adapter + page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable LMS",
                                            checked: lms_enabled(),
                                            onchange: move |_| {
                                                lms_enabled.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "LMS" }
                                td { class: "py-2 px-3",
                                    if lms_enabled() {
                                        if let Some(ref cfg) = lms_cfg {
                                            if cfg.connected {
                                                if cfg.cli_subscription_active {
                                                    span { class: "status-ok", "✓ CLI" }
                                                } else {
                                                    span { class: "text-yellow-500", "⚠ Polling" }
                                                }
                                            } else {
                                                span { class: "status-err", "✗ Not connected" }
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // HQPlayer (adapter + page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable HQPlayer",
                                            checked: hqplayer_enabled(),
                                            onchange: move |_| {
                                                hqplayer_enabled.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "HQPlayer" }
                                td { class: "py-2 px-3",
                                    if hqplayer_enabled() {
                                        if let Some(ref status) = hqp_st {
                                            if status.connected {
                                                span { class: "status-ok", "✓ Connected" }
                                            } else {
                                                span { class: "status-err", "✗ Not connected" }
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // Knobs (page only, no adapter)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Show Knobs page",
                                            checked: !hide_knobs(),
                                            onchange: move |_| {
                                                hide_knobs.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Knobs" }
                                td { class: "py-2 px-3 text-muted", "-" }
                            }
                        }
                    }
                }
            }

            // Zone management: name, visibility, and order.
            section { class: "mb-8", aria_labelledby: "zones-heading",
                div { class: "mb-4",
                    h2 { id: "zones-heading", class: "text-xl font-semibold", "Zone list" }
                    p { class: "text-muted text-sm",
                        "Name your zones, choose which ones appear, and set their order. Drag a row to move it, or use the arrows. Applies everywhere — this app, knobs, and connected assistants."
                    }
                }

                // Placed above the table, not below it. This is the sentence that defuses the
                // "am I deleting my speaker?" moment, so it has to arrive before the checkbox
                // rather than several screens below it on a long list. It also sits outside the
                // loaded-and-non-empty branch, so it is present in every state.
                p { class: "text-muted text-sm mb-4",
                    "Hiding only removes a zone from lists. Nothing is deleted, a knob already pointing at it keeps working, and an assistant asked for it by name still finds it. A hidden zone keeps its place in the order, so showing it again puts it back where you left it."
                }

                if let Some(message) = zone_error() {
                    ErrorAlert {
                        message,
                        on_dismiss: move |_| zone_error.set(None),
                    }
                }

                // Announcements for assistive tech. Visually hidden: sighted users see the row
                // move, which this exists to substitute for.
                div {
                    class: "sr-only",
                    role: "status",
                    aria_live: "polite",
                    aria_atomic: "true",
                    "{zone_status}"
                }

                div { class: "card overflow-x-auto p-4 sm:p-6",
                    match managed_zones.read().as_ref() {
                        None => rsx! {
                            p { class: "text-muted py-2", role: "status", aria_live: "polite", "Loading zones…" }
                        },
                        Some(None) => rsx! {
                            div { role: "alert",
                                p { class: "status-err py-2 mb-3", "Could not load the zone list." }
                                button {
                                    r#type: "button",
                                    class: "btn btn-outline btn-sm",
                                    onclick: move |_| managed_zones.restart(),
                                    "Try again"
                                }
                            }
                        },
                        Some(Some(response)) if response.zones.is_empty() => rsx! {
                            p { class: "text-muted py-2",
                                "No zones discovered yet. Check that an adapter is enabled above and connected."
                            }
                        },
                        Some(Some(response)) => {
                            let zones = response.zones.clone();
                            let revision = zone_revision();
                            // Positions count visible zones only, because that is the order every
                            // other surface shows. Numbering hidden zones would promise an order
                            // that nothing displays.
                            let visible_total = zones.iter().filter(|z| !z.hidden).count();
                            let mut visible_position = 0usize;
                            let mut rows = Vec::with_capacity(zones.len());
                            for zone in zones {
                                let position = if zone.hidden {
                                    None
                                } else {
                                    visible_position += 1;
                                    Some(visible_position)
                                };
                                rows.push((zone, position));
                            }
                            let first_visible = rows.iter().position(|(z, _)| !z.hidden);
                            let last_visible = rows.iter().rposition(|(z, _)| !z.hidden);
                            // A drop knows only the dragged zone's id, so the row it belongs to has
                            // to be recoverable by id from inside the drop handler.
                            let drop_lookup: std::rc::Rc<std::collections::HashMap<String, ManagedZone>> =
                                std::rc::Rc::new(
                                    rows.iter()
                                        .map(|(z, _)| (z.zone_id.clone(), z.clone()))
                                        .collect(),
                                );

                            rsx! {
                                table { class: "w-full", id: "zones-table",
                                    thead {
                                        tr { class: "border-b border-default",
                                            // Handle column. Hidden below `sm` because HTML5 drag
                                            // events never fire on touch -- a grip a phone cannot
                                            // use is a false affordance, and the arrows are the
                                            // real control there.
                                            th { class: "w-8 hidden sm:table-cell", span { class: "sr-only", "Drag to reorder" } }
                                            th { class: "text-left py-2 px-2 font-semibold w-12", "Show" }
                                            th { class: "text-left py-2 px-2 font-semibold", "Name" }
                                            th { class: "text-left py-2 px-2 font-semibold hidden sm:table-cell w-28", "Source" }
                                            th { class: "text-right py-2 px-2 font-semibold w-32", "Position" }
                                        }
                                    }
                                    tbody {
                                        for (index, (zone, position)) in rows.into_iter().enumerate() {
                                            tr {
                                                key: "{zone.zone_id}-{revision}",
                                                class: if drop_target() == Some(zone.zone_id.clone()) {
                                                    "zone-row zone-row-drop-target border-b border-default"
                                                } else if dragging() == Some(zone.zone_id.clone()) {
                                                    "zone-row zone-row-dragging border-b border-default"
                                                } else {
                                                    "zone-row border-b border-default"
                                                },
                                                ondragover: {
                                                    let zone_id = zone.zone_id.clone();
                                                    move |evt: DragEvent| {
                                                        // Without preventDefault the browser treats
                                                        // this as an invalid drop target and never
                                                        // fires ondrop.
                                                        evt.prevent_default();
                                                        if dragging().is_some() {
                                                            drop_target.set(Some(zone_id.clone()));
                                                        }
                                                    }
                                                },
                                                ondrop: {
                                                    let target = zone.clone();
                                                    let rows_for_drop = drop_lookup.clone();
                                                    move |evt: DragEvent| {
                                                        evt.prevent_default();
                                                        if let Some(dragged_id) = dragging() {
                                                            if let Some(dragged) = rows_for_drop.get(&dragged_id) {
                                                                drop_zone_onto(dragged.clone(), target.clone());
                                                            }
                                                        }
                                                        dragging.set(None);
                                                        drop_target.set(None);
                                                    }
                                                },
                                                ondragend: move |_| {
                                                    dragging.set(None);
                                                    drop_target.set(None);
                                                },
                                                // The drag handle. `draggable` lives here rather
                                                // than on the row so that selecting text in the
                                                // name field does not start a drag -- the row is
                                                // still the drop target, just not the drag source.
                                                td {
                                                    class: "zone-handle-cell hidden sm:table-cell align-middle",
                                                    draggable: "true",
                                                    ondragstart: {
                                                        let zone_id = zone.zone_id.clone();
                                                        move |_| dragging.set(Some(zone_id.clone()))
                                                    },
                                                    // aria-hidden: dragging has no keyboard
                                                    // equivalent, so the arrows carry the
                                                    // accessible path and this must not add a
                                                    // stop in the tab order that does nothing.
                                                    svg {
                                                        class: "w-4 h-4 text-muted mx-auto",
                                                        fill: "currentColor",
                                                        view_box: "0 0 24 24",
                                                        "aria-hidden": "true",
                                                        circle { cx: "9", cy: "6", r: "1.5" }
                                                        circle { cx: "15", cy: "6", r: "1.5" }
                                                        circle { cx: "9", cy: "12", r: "1.5" }
                                                        circle { cx: "15", cy: "12", r: "1.5" }
                                                        circle { cx: "9", cy: "18", r: "1.5" }
                                                        circle { cx: "15", cy: "18", r: "1.5" }
                                                    }
                                                }
                                                td { class: "py-2 px-2 align-top",
                                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-2",
                                                        input {
                                                            r#type: "checkbox",
                                                            class: "checkbox",
                                                            aria_label: "Show {zone.qualified_label()}",
                                                            checked: !zone.hidden,
                                                            onchange: {
                                                                let zone = zone.clone();
                                                                move |_| set_zone_hidden(zone.clone())
                                                            }
                                                        }
                                                    }
                                                }
                                                td { class: "py-2 px-2 align-top",
                                                    input {
                                                        r#type: "text",
                                                        class: if zone.hidden { "input w-full text-muted" } else { "input w-full" },
                                                        // The draft while the user is typing,
                                                        // otherwise the server's name. A controlled
                                                        // input needs somewhere to put keystrokes
                                                        // or the next render throws them away.
                                                        value: "{name_drafts().get(&zone.zone_id).cloned().unwrap_or_else(|| zone.zone_name.clone())}",
                                                        maxlength: "60",
                                                        aria_label: "Name for {zone.qualified_label()}",
                                                        oninput: {
                                                            let zone_id = zone.zone_id.clone();
                                                            move |evt: FormEvent| {
                                                                name_drafts.write().insert(zone_id.clone(), evt.value());
                                                            }
                                                        },
                                                        // Commits on blur and on Enter rather than
                                                        // per keystroke: each save is a disk write
                                                        // plus a full list refetch, and refetching
                                                        // mid-word would fight the cursor.
                                                        onchange: {
                                                            let zone = zone.clone();
                                                            move |evt: FormEvent| rename_zone(zone.clone(), evt.value())
                                                        }
                                                    }
                                                    // Source lives here below `sm`, where the
                                                    // column is hidden to buy back width for the
                                                    // name field on a phone.
                                                    div { class: "text-muted text-xs mt-1 sm:hidden", "{source_label(&zone.source)}" }
                                                    if zone.renamed {
                                                        div { class: "text-muted text-xs mt-1",
                                                            "{zone.provider_name} · "
                                                            button {
                                                                r#type: "button",
                                                                class: "underline",
                                                                onclick: {
                                                                    let zone = zone.clone();
                                                                    move |_| rename_zone(zone.clone(), String::new())
                                                                },
                                                                "reset"
                                                            }
                                                        }
                                                    }
                                                }
                                                td { class: "py-2 px-2 align-top text-muted hidden sm:table-cell", "{source_label(&zone.source)}" }
                                                td { class: "py-2 px-2 align-top",
                                                    div { class: "flex items-center justify-end gap-2",
                                                        span { class: "text-muted text-sm tabular-nums",
                                                            if let Some(position) = position {
                                                                "{position} of {visible_total}"
                                                            } else {
                                                                "Hidden"
                                                            }
                                                        }
                                                        {
                                                            let at_top = Some(index) == first_visible;
                                                            let at_bottom = Some(index) == last_visible;
                                                            let up_zone = zone.clone();
                                                            let down_zone = zone.clone();
                                                            rsx! {
                                                                // `aria-disabled`, not `disabled`:
                                                                // a disabled control cannot hold
                                                                // focus, so moving a zone to first
                                                                // would disable the very button
                                                                // that was just pressed and drop
                                                                // focus to <body>. These stay
                                                                // focusable and announce "already
                                                                // first" instead.
                                                                button {
                                                                    r#type: "button",
                                                                    class: if at_top { "btn btn-outline btn-sm opacity-45" } else { "btn btn-outline btn-sm" },
                                                                    aria_disabled: if at_top { "true" } else { "false" },
                                                                    aria_label: "Move {zone.qualified_label()} up",
                                                                    onclick: move |_| move_zone(up_zone.clone(), MoveDirection::Up, at_top),
                                                                    svg {
                                                                        class: "w-4 h-4",
                                                                        fill: "none",
                                                                        view_box: "0 0 24 24",
                                                                        stroke: "currentColor",
                                                                        "stroke-width": "2",
                                                                        "aria-hidden": "true",
                                                                        path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M5 15l7-7 7 7" }
                                                                    }
                                                                }
                                                                button {
                                                                    r#type: "button",
                                                                    class: if at_bottom { "btn btn-outline btn-sm opacity-45" } else { "btn btn-outline btn-sm" },
                                                                    aria_disabled: if at_bottom { "true" } else { "false" },
                                                                    aria_label: "Move {zone.qualified_label()} down",
                                                                    onclick: move |_| move_zone(down_zone.clone(), MoveDirection::Down, at_bottom),
                                                                    svg {
                                                                        class: "w-4 h-4",
                                                                        fill: "none",
                                                                        view_box: "0 0 24 24",
                                                                        stroke: "currentColor",
                                                                        "stroke-width": "2",
                                                                        "aria-hidden": "true",
                                                                        path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M19 9l-7 7-7-7" }
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
                    }
                }
            }

            // MCP discovery and agent onboarding
            section { class: "mb-8", aria_labelledby: "mcp-server-heading",
                div { class: "card overflow-hidden",
                    div { class: "px-5 py-5 sm:px-6 sm:py-6 border-b border-default flex flex-col gap-4 sm:flex-row sm:items-start sm:justify-between",
                        div { class: "max-w-3xl",
                            h2 {
                                id: "mcp-server-heading",
                                class: "text-xl font-semibold",
                                "You have an MCP server"
                            }
                            p { class: "mt-2 text-secondary max-w-2xl",
                                "MCP (Model Context Protocol) is how compatible AI agents connect to your hi-fi tools. It is already running with Unified Hi-Fi Control—there is nothing else to install."
                            }
                        }
                        span { class: "badge badge-secondary gap-2 self-start shrink-0",
                            span {
                                class: "block size-2 rounded-full bg-emerald-500",
                                aria_hidden: "true"
                            }
                            "Available now"
                        }
                    }

                    div { class: "grid lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]",
                        div { class: "min-w-0 px-5 py-5 sm:px-6 sm:py-6 lg:border-r border-default",
                            h3 { class: "font-semibold", "Your MCP address" }
                            p { class: "mt-2 text-sm text-secondary max-w-prose",
                                "Agents on your network can reach this address."
                            }
                            div { class: "mt-4",
                                p { class: "text-xs font-medium text-muted", "MCP URL" }
                                div { class: "mt-2 flex flex-col gap-2 sm:flex-row sm:items-stretch",
                                    code {
                                        class: "block min-w-0 flex-1 overflow-x-auto rounded-md bg-hover px-3 py-3 text-sm select-all",
                                        "{mcp_endpoint.url}"
                                    }
                                    button {
                                        r#type: "button",
                                        class: "btn btn-outline btn-sm shrink-0",
                                        aria_label: "Copy MCP URL",
                                        onclick: {
                                            let url = mcp_endpoint.url.clone();
                                            move |_| {
                                                let value = url.clone();
                                                copy_to_clipboard(value, url_copy_state);
                                            }
                                        },
                                        span { aria_live: "polite", "{url_copy_state().label(\"Copy URL\")}" }
                                    }
                                }
                            }
                            p { class: "mt-4 text-xs text-muted max-w-prose",
                                "Keep Unified Hi-Fi Control running, and only connect agents you trust. Connected agents can control playback on your system."
                            }
                        }

                        div { class: "min-w-0 px-5 py-5 sm:px-6 sm:py-6 bg-[var(--surface-base)]",
                            h3 { class: "font-semibold", "Ask your agent to connect" }
                            p { class: "mt-2 text-sm text-secondary max-w-prose",
                                "Copy this JSON, paste it into your agent, and say:"
                            }
                            p { class: "mt-2 text-sm font-medium text-primary max-w-prose",
                                "“Set up this MCP server for me, then confirm it works by listing my hi-fi zones.”"
                            }
                            div { class: "mt-4 w-full min-w-0 overflow-hidden rounded-md bg-hover",
                                div { class: "flex min-h-11 items-center justify-between gap-3 border-b border-default px-3 py-2",
                                    span { class: "text-xs font-medium text-muted", "Agent configuration" }
                                    button {
                                        r#type: "button",
                                        class: "btn btn-outline btn-sm shrink-0",
                                        aria_label: "Copy agent configuration JSON",
                                        onclick: {
                                            let config = agent_config.clone();
                                            move |_| {
                                                let value = config.clone();
                                                copy_to_clipboard(value, config_copy_state);
                                            }
                                        },
                                        span { aria_live: "polite", "{config_copy_state().label(\"Copy JSON\")}" }
                                    }
                                }
                                pre {
                                    class: "w-full min-w-0 overflow-x-auto p-4 text-xs leading-relaxed text-primary",
                                    aria_label: "MCP server configuration",
                                    code { "{agent_config}" }
                                }
                            }
                            p { class: "mt-4 text-sm text-secondary max-w-prose",
                                "Your agent can usually update its own MCP settings. Restart or reload it only if the new hi-fi tools do not appear."
                            }
                            p { class: "mt-3 text-xs text-muted max-w-prose",
                                "If your agent asks for a URL or uses another configuration format, give it the MCP URL shown here."
                            }
                        }
                    }
                }
            }

            // Theme Settings section
            section { class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Appearance" }
                    p { class: "text-muted text-sm", "Choose your preferred color theme" }
                }

                div { class: "card p-6",
                    div { class: "grid grid-cols-2 sm:grid-cols-4 gap-4",
                        for theme in [Theme::System, Theme::Light, Theme::Dark, Theme::Oled] {
                            button {
                                class: if theme_ctx.get() == theme { "btn-primary py-3" } else { "btn-outline py-3" },
                                onclick: move |_| theme_ctx.set(theme),
                                "{theme.label()}"
                            }
                        }
                    }
                    p { class: "mt-4 text-sm text-muted",
                        match theme_ctx.get() {
                            Theme::System => "Using your system's color scheme preference.",
                            Theme::Light => "Light theme for bright environments.",
                            Theme::Dark => "Dark theme for low-light environments.",
                            Theme::Oled => "Pure black theme for AMOLED displays.",
                        }
                    }
                }
            }

        }
    }
}
