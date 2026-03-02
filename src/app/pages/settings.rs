//! Settings page component.
//!
//! Adapter settings and discovery status using Dioxus resources.

use dioxus::prelude::*;

use crate::app::api::{self, AdapterSettings, AppSettings, HqpStatus, LmsConfig, RoonStatus};
use crate::app::components::Layout;
use crate::app::settings_context::use_settings;
use crate::app::sse::use_sse;
use crate::app::theme::{use_theme, Theme};

/// License status from GET /api/config/license
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
struct LicenseStatus {
    configured: bool,
}

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

/// Settings page component.
#[component]
pub fn Settings() -> Element {
    let sse = use_sse();
    let theme_ctx = use_theme();
    let settings_ctx = use_settings();

    // Adapter toggle signals
    let mut roon_enabled = use_signal(|| true);
    let mut lms_enabled = use_signal(|| false);
    let mut openhome_enabled = use_signal(|| false);
    let mut upnp_enabled = use_signal(|| false);
    let mut hqplayer_enabled = use_signal(|| false);

    // Hide knobs signal (LMS/HQPlayer visibility follows adapter enabled state)
    let mut hide_knobs = use_signal(|| false);

    // License state
    let mut license_configured = use_signal(|| false);
    let mut license_input = use_signal(String::new);
    let mut license_saving = use_signal(|| false);
    let mut license_message = use_signal(|| None::<String>);

    // Load license status
    let license_status = use_resource(|| async {
        api::fetch_json::<LicenseStatus>("/api/config/license")
            .await
            .ok()
    });

    // Sync license status to signal
    use_effect(move || {
        if let Some(Some(s)) = license_status.read().as_ref() {
            license_configured.set(s.configured);
        }
    });

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

                div { class: "card p-6",
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
                                td { class: "py-2 px-3", "Knobs" }
                                td { class: "py-2 px-3 text-muted", "-" }
                            }
                        }
                    }
                }
            }

            // Memex License section
            section { class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Memex License" }
                    p { class: "text-muted text-sm", "Required for AI Configurator and paid-tier features" }
                }

                div { class: "card p-6",
                    if license_configured() {
                        // Licensed — show status + remove button
                        div { class: "flex items-center justify-between",
                            div { class: "flex items-center gap-2",
                                span { class: "status-ok", "✓ License configured" }
                            }
                            button {
                                class: "btn btn-outline text-sm",
                                onclick: move |_| {
                                    license_saving.set(true);
                                    license_message.set(None);
                                    spawn(async move {
                                        match api::delete_no_response("/api/config/license").await {
                                            Ok(()) => {
                                                license_configured.set(false);
                                                license_message.set(Some("License removed".to_string()));
                                            }
                                            Err(e) => {
                                                license_message.set(Some(format!("Error removing license: {}", e)));
                                            }
                                        }
                                        license_saving.set(false);
                                    });
                                },
                                "Remove"
                            }
                        }
                    } else {
                        // Not licensed — show input
                        form {
                            onsubmit: move |e| {
                                e.prevent_default();
                                let key = license_input();
                                if key.trim().is_empty() {
                                    return;
                                }
                                license_saving.set(true);
                                license_message.set(None);
                                spawn(async move {
                                    let body = serde_json::json!({ "license": key.trim() });
                                    match api::post_json::<_, LicenseStatus>("/api/config/license", &body).await {
                                        Ok(resp) => {
                                            license_configured.set(resp.configured);
                                            if resp.configured {
                                                license_input.set(String::new());
                                                license_message.set(Some("License saved".to_string()));
                                            } else {
                                                license_message.set(Some("License not accepted".to_string()));
                                            }
                                        }
                                        Err(e) => {
                                            license_message.set(Some(format!("Error: {}", e)));
                                        }
                                    }
                                    license_saving.set(false);
                                });
                            },
                            div { class: "flex gap-3",
                                input {
                                    class: "input flex-1 font-mono text-sm",
                                    r#type: "password",
                                    placeholder: "Paste your Memex license key",
                                    value: "{license_input}",
                                    disabled: license_saving(),
                                    oninput: move |e| license_input.set(e.value()),
                                }
                                button {
                                    class: "btn btn-primary",
                                    r#type: "submit",
                                    disabled: license_saving() || license_input().trim().is_empty(),
                                    if license_saving() { "Saving..." } else { "Configure" }
                                }
                            }
                        }
                    }

                    if let Some(ref msg) = license_message() {
                        p { class: "text-sm text-muted mt-3", "{msg}" }
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
