//! Settings page using Dioxus signals.
//!
//! Replaces inline JavaScript with idiomatic Dioxus patterns:
//! - use_signal() for reactive state
//! - use_resource() for async data fetching
//! - Rust event handlers (onclick, onchange)

use dioxus::prelude::*;
use serde::{Deserialize, Serialize};

use crate::app::components::Layout;

/// Adapter settings from /api/settings
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AdapterSettings {
    pub roon: bool,
    pub lms: bool,
    pub openhome: bool,
    pub upnp: bool,
}

/// Full app settings
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct AppSettings {
    pub adapters: AdapterSettings,
}

/// Discovery status for each protocol
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct DiscoveryStatus {
    pub roon_connected: bool,
    pub roon_core_name: Option<String>,
    pub openhome_device_count: usize,
    pub upnp_renderer_count: usize,
}

/// Server function to fetch settings
#[server]
pub async fn get_settings() -> Result<AppSettings, ServerFnError> {
    let client = reqwest::Client::new();
    let resp = client
        .get("http://127.0.0.1:8088/api/settings")
        .send()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;

    resp.json::<AppSettings>()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))
}

/// Server function to save settings
#[server]
pub async fn save_settings(settings: AppSettings) -> Result<(), ServerFnError> {
    let client = reqwest::Client::new();
    client
        .post("http://127.0.0.1:8088/api/settings")
        .json(&settings)
        .send()
        .await
        .map_err(|e| ServerFnError::new(e.to_string()))?;
    Ok(())
}

/// Server function to get discovery status
#[server]
pub async fn get_discovery_status() -> Result<DiscoveryStatus, ServerFnError> {
    let client = reqwest::Client::new();

    let (roon, openhome, upnp) = tokio::join!(
        client.get("http://127.0.0.1:8088/roon/status").send(),
        client.get("http://127.0.0.1:8088/openhome/status").send(),
        client.get("http://127.0.0.1:8088/upnp/status").send(),
    );

    let mut status = DiscoveryStatus::default();

    if let Ok(resp) = roon {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            status.roon_connected = json["connected"].as_bool().unwrap_or(false);
            status.roon_core_name = json["core_name"].as_str().map(String::from);
        }
    }

    if let Ok(resp) = openhome {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            status.openhome_device_count = json["device_count"].as_u64().unwrap_or(0) as usize;
        }
    }

    if let Ok(resp) = upnp {
        if let Ok(json) = resp.json::<serde_json::Value>().await {
            status.upnp_renderer_count = json["renderer_count"].as_u64().unwrap_or(0) as usize;
        }
    }

    Ok(status)
}

/// Settings page component
#[component]
pub fn Settings() -> Element {
    // Reactive state for adapter toggles
    let mut roon_enabled = use_signal(|| true);
    let mut lms_enabled = use_signal(|| false);
    let mut openhome_enabled = use_signal(|| false);
    let mut upnp_enabled = use_signal(|| false);

    // Discovery status
    let mut discovery = use_signal(DiscoveryStatus::default);

    // Load initial settings
    let settings_resource = use_resource(move || async move { get_settings().await });

    // Update signals when settings load
    use_effect(move || {
        if let Some(Ok(settings)) = settings_resource.read().as_ref() {
            roon_enabled.set(settings.adapters.roon);
            lms_enabled.set(settings.adapters.lms);
            openhome_enabled.set(settings.adapters.openhome);
            upnp_enabled.set(settings.adapters.upnp);
        }
    });

    // Load discovery status
    let discovery_resource = use_resource(move || async move { get_discovery_status().await });

    use_effect(move || {
        if let Some(Ok(status)) = discovery_resource.read().as_ref() {
            discovery.set(status.clone());
        }
    });

    // Save settings handler
    let save = move |_| {
        spawn(async move {
            let settings = AppSettings {
                adapters: AdapterSettings {
                    roon: roon_enabled(),
                    lms: lms_enabled(),
                    openhome: openhome_enabled(),
                    upnp: upnp_enabled(),
                },
            };
            let _ = save_settings(settings).await;
        });
    };

    rsx! {
        Layout {
            title: "Settings".to_string(),
            nav_active: "settings".to_string(),

            h1 { "Settings" }

            // Adapter Settings section
            section {
                h2 { "Adapter Settings" }
                p { "Enable or disable zone sources" }

                article {
                    div { style: "display:flex;flex-wrap:wrap;gap:1.5rem;",
                        label {
                            input {
                                r#type: "checkbox",
                                checked: roon_enabled(),
                                onchange: move |_| {
                                    roon_enabled.toggle();
                                    save(());
                                }
                            }
                            " Roon"
                        }
                        label {
                            input {
                                r#type: "checkbox",
                                checked: lms_enabled(),
                                onchange: move |_| {
                                    lms_enabled.toggle();
                                    save(());
                                }
                            }
                            " LMS"
                        }
                        label {
                            input {
                                r#type: "checkbox",
                                checked: openhome_enabled(),
                                onchange: move |_| {
                                    openhome_enabled.toggle();
                                    save(());
                                }
                            }
                            " OpenHome"
                        }
                        label {
                            input {
                                r#type: "checkbox",
                                checked: upnp_enabled(),
                                onchange: move |_| {
                                    upnp_enabled.toggle();
                                    save(());
                                }
                            }
                            " UPnP/DLNA"
                        }
                    }
                    p { style: "margin-top:0.5rem;",
                        small { "Changes take effect immediately. Disabled adapters won't contribute zones." }
                    }
                }
            }

            // Discovery Status section
            section {
                h2 { "Auto-Discovery" }
                p { "Devices found via SSDP (no configuration needed)" }

                article {
                    table {
                        thead {
                            tr {
                                th { "Protocol" }
                                th { "Status" }
                                th { "Devices" }
                            }
                        }
                        tbody {
                            // Roon row
                            tr {
                                td { "Roon" }
                                td { class: if discovery().roon_connected { "status-ok" } else { "status-err" },
                                    if !roon_enabled() {
                                        span { class: "status-disabled", "Disabled" }
                                    } else if discovery().roon_connected {
                                        "✓ Connected"
                                    } else {
                                        "✗ Not connected"
                                    }
                                }
                                td {
                                    if discovery().roon_connected {
                                        {discovery().roon_core_name.clone().unwrap_or_else(|| "Core".to_string())}
                                    } else {
                                        "-"
                                    }
                                }
                            }
                            // OpenHome row
                            tr {
                                td { "OpenHome" }
                                td {
                                    if !openhome_enabled() {
                                        span { class: "status-disabled", "Disabled" }
                                    } else if discovery().openhome_device_count > 0 {
                                        span { class: "status-ok", "✓ Active" }
                                    } else {
                                        "Searching..."
                                    }
                                }
                                td {
                                    if openhome_enabled() {
                                        "{discovery().openhome_device_count} device(s)"
                                    } else {
                                        "-"
                                    }
                                }
                            }
                            // UPnP row
                            tr {
                                td { "UPnP/DLNA" }
                                td {
                                    if !upnp_enabled() {
                                        span { class: "status-disabled", "Disabled" }
                                    } else if discovery().upnp_renderer_count > 0 {
                                        span { class: "status-ok", "✓ Active" }
                                    } else {
                                        "Searching..."
                                    }
                                }
                                td {
                                    if upnp_enabled() {
                                        "{discovery().upnp_renderer_count} renderer(s)"
                                    } else {
                                        "-"
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
