//! Settings page component.
//!
//! Adapter settings and discovery status using Dioxus resources.

use dioxus::prelude::*;

use crate::app::api::{AdapterSettings, AppSettings, HqpStatus, LmsConfig, RoonStatus};
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
                                            aria_label: "Show Controllers page",
                                            checked: !hide_knobs(),
                                            onchange: move |_| {
                                                hide_knobs.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Controllers" }
                                td { class: "py-2 px-3 text-muted", "-" }
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
                                "MCP (Model Context Protocol) is how compatible AI agents connect to your hi-fi tools. It is already running with Unified Hi-Fi Control by Open Horizon Labs—there is nothing else to install."
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
                                "Keep Unified Hi-Fi Control by Open Horizon Labs running, and only connect agents you trust. Connected agents can control playback on your system."
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
