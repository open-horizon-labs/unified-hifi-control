//! HQPlayer page component.
//!
//! Using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{self, HqpConfig, HqpPipeline, HqpProfile, HqpStatus, Zone, ZonesResponse};
use crate::app::components::Layout;
use crate::app::sse::use_sse;

/// HQP configure request
#[derive(Clone, serde::Serialize)]
struct HqpConfigureRequest {
    host: String,
    port: u16,
    web_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// Zone link response
#[derive(Clone, Debug, Default, serde::Deserialize)]
struct ZoneLinksResponse {
    links: Vec<ZoneLink>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
struct ZoneLink {
    zone_id: String,
    instance: String,
}

/// HQPlayer instances response
#[derive(Clone, Debug, Default, serde::Deserialize)]
struct InstancesResponse {
    instances: Vec<HqpInstance>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
struct HqpInstance {
    name: String,
    host: Option<String>,
}

/// Zone link request
#[derive(Clone, serde::Serialize)]
struct ZoneLinkRequest {
    zone_id: String,
    instance: String,
}

/// Zone unlink request
#[derive(Clone, serde::Serialize)]
struct ZoneUnlinkRequest {
    zone_id: String,
}

/// HQPlayer page component.
#[component]
pub fn HqPlayer() -> Element {
    let sse = use_sse();

    // Form fields
    let mut host = use_signal(String::new);
    let mut port = use_signal(|| 4321u16);
    let mut web_port = use_signal(|| 8088u16);
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);
    let mut config_status = use_signal(|| None::<String>);

    // Load config resource
    let config =
        use_resource(|| async { api::fetch_json::<HqpConfig>("/hqplayer/config").await.ok() });

    // Load status resource
    let mut status =
        use_resource(|| async { api::fetch_json::<HqpStatus>("/hqp/status").await.ok() });

    // Load pipeline resource
    let mut pipeline =
        use_resource(|| async { api::fetch_json::<HqpPipeline>("/hqp/pipeline").await.ok() });

    // Load profiles resource
    let profiles = use_resource(|| async {
        api::fetch_json::<Vec<HqpProfile>>("/hqp/profiles")
            .await
            .ok()
    });

    // Load zones resource
    let mut zones =
        use_resource(|| async { api::fetch_json::<ZonesResponse>("/knob/zones").await.ok() });

    // Load zone links resource
    let mut zone_links = use_resource(|| async {
        api::fetch_json::<ZoneLinksResponse>("/hqp/zones/links")
            .await
            .ok()
    });

    // Load instances resource
    let instances = use_resource(|| async {
        api::fetch_json::<InstancesResponse>("/hqp/instances")
            .await
            .ok()
    });

    // Sync config to form when loaded
    use_effect(move || {
        if let Some(Some(cfg)) = config.read().as_ref() {
            host.set(cfg.host.clone().unwrap_or_default());
            port.set(cfg.port.unwrap_or(4321));
            web_port.set(cfg.web_port.unwrap_or(8088));
        }
    });

    // Refresh on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_hqp() {
            status.restart();
            pipeline.restart();
        }
        if sse.should_refresh_zones() {
            zones.restart();
            zone_links.restart();
        }
    });

    // Save config handler
    let save_config = move |_| {
        let h = host();
        let p = port();
        let wp = web_port();
        let u = username();
        let pw = password();

        config_status.set(Some("Saving...".to_string()));

        spawn(async move {
            let req = HqpConfigureRequest {
                host: h,
                port: p,
                web_port: wp,
                username: if u.is_empty() { None } else { Some(u) },
                password: if pw.is_empty() { None } else { Some(pw) },
            };

            match api::post_json::<_, serde_json::Value>("/hqplayer/configure", &req).await {
                Ok(resp) => {
                    let connected = resp
                        .get("connected")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if connected {
                        config_status.set(Some("Connected!".to_string()));
                    } else {
                        config_status.set(Some("Saved but not connected".to_string()));
                    }
                    status.restart();
                    pipeline.restart();
                }
                Err(e) => {
                    config_status.set(Some(format!("Error: {}", e)));
                }
            }
        });
    };

    // Zone link handler
    let link_zone = move |(zone_id, instance): (String, String)| {
        spawn(async move {
            let req = ZoneLinkRequest { zone_id, instance };
            let _ = api::post_json_no_response("/hqp/zones/link", &req).await;
            zone_links.restart();
        });
    };

    // Zone unlink handler
    let unlink_zone = move |zone_id: String| {
        spawn(async move {
            let req = ZoneUnlinkRequest { zone_id };
            let _ = api::post_json_no_response("/hqp/zones/unlink", &req).await;
            zone_links.restart();
        });
    };

    let is_loading = config.read().is_none();
    let current_status = status.read().clone().flatten();
    let current_pipeline = pipeline.read().clone().flatten();
    let profiles_list = profiles.read().clone().flatten().unwrap_or_default();
    let zones_list = zones
        .read()
        .clone()
        .flatten()
        .map(|r| r.zones)
        .unwrap_or_default();
    let links_list = zone_links
        .read()
        .clone()
        .flatten()
        .map(|r| r.links)
        .unwrap_or_default();
    let instances_list = instances
        .read()
        .clone()
        .flatten()
        .map(|r| r.instances)
        .unwrap_or_default();

    rsx! {
        Layout {
            title: "HQPlayer".to_string(),
            nav_active: "hqplayer".to_string(),

            h1 { "HQPlayer" }

            // Configuration section
            section { id: "hqp-config",
                hgroup {
                    h2 { "Configuration" }
                    p { "HQPlayer connection settings" }
                }
                article {
                    label {
                        "Host (IP or hostname)"
                        input {
                            r#type: "text",
                            placeholder: "192.168.1.100",
                            value: "{host}",
                            oninput: move |evt| host.set(evt.value())
                        }
                    }
                    div { class: "grid",
                        label {
                            "Native Port (TCP)"
                            input {
                                r#type: "number",
                                min: "1",
                                max: "65535",
                                value: "{port}",
                                oninput: move |evt| {
                                    if let Ok(p) = evt.value().parse() {
                                        port.set(p);
                                    }
                                }
                            }
                        }
                        label {
                            "Web Port (HTTP)"
                            input {
                                r#type: "number",
                                min: "1",
                                max: "65535",
                                value: "{web_port}",
                                oninput: move |evt| {
                                    if let Ok(p) = evt.value().parse() {
                                        web_port.set(p);
                                    }
                                }
                            }
                            small { "For profile loading (HQPlayer Embedded)" }
                        }
                    }
                    div { class: "grid",
                        label {
                            "Web Username"
                            input {
                                r#type: "text",
                                placeholder: "admin",
                                value: "{username}",
                                oninput: move |evt| username.set(evt.value())
                            }
                        }
                        label {
                            "Web Password"
                            input {
                                r#type: "password",
                                placeholder: "password",
                                value: "{password}",
                                oninput: move |evt| password.set(evt.value())
                            }
                        }
                    }
                    small { "Web credentials enable profile switching via HQPlayer's web UI" }
                    br {}
                    button { onclick: save_config, "Save Configuration" }
                    if let Some(ref status_msg) = config_status() {
                        span { style: "margin-left:1rem;",
                            if status_msg.contains("Connected") {
                                span { class: "status-ok", "✓ {status_msg}" }
                            } else if status_msg.starts_with("Error") {
                                span { class: "status-err", "{status_msg}" }
                            } else {
                                "{status_msg}"
                            }
                        }
                    }
                }
            }

            // Connection Status section
            section { id: "hqp-status",
                hgroup {
                    h2 { "Connection Status" }
                    p { "HQPlayer DSP engine connection" }
                }
                article {
                    if let Some(ref s) = current_status {
                        if s.connected {
                            p { class: "status-ok",
                                "✓ Connected to {s.host.as_deref().unwrap_or(\"HQPlayer\")}"
                            }
                        } else {
                            p { class: "status-err", "Not connected to HQPlayer" }
                        }
                    } else if is_loading {
                        p { aria_busy: "true", "Loading..." }
                    } else {
                        p { class: "status-err", "Not connected to HQPlayer" }
                    }
                }
            }

            // Pipeline Settings section
            section { id: "hqp-pipeline",
                hgroup {
                    h2 { "Pipeline Settings" }
                    p { "Current DSP configuration" }
                }
                article {
                    if let Some(ref pipe) = current_pipeline {
                        PipelineDisplay { pipeline: pipe.clone() }
                    } else if is_loading {
                        p { aria_busy: "true", "Loading..." }
                    } else {
                        p { class: "status-err", "Pipeline not available" }
                    }
                }
            }

            // Profiles section
            section { id: "hqp-profiles",
                hgroup {
                    h2 { "Profiles" }
                    p { "Saved configurations (requires web credentials)" }
                }
                article {
                    if profiles_list.is_empty() {
                        p { "No profiles available" }
                    } else {
                        table {
                            thead {
                                tr {
                                    th { "Profile" }
                                    th { "Action" }
                                }
                            }
                            tbody {
                                for profile in profiles_list {
                                    tr {
                                        td { "{profile.title.as_deref().or(profile.name.as_deref()).unwrap_or(\"Unknown\")}" }
                                        td {
                                            button { "Load" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Zone Linking section
            section { id: "hqp-zone-links",
                hgroup {
                    h2 { "Zone Linking" }
                    p { "Link audio zones to HQPlayer for DSP processing" }
                }
                article {
                    ZoneLinkTable {
                        zones: zones_list,
                        links: links_list,
                        instances: instances_list,
                        on_link: link_zone,
                        on_unlink: unlink_zone,
                    }
                }
            }
        }
    }
}

/// Pipeline display component
#[component]
fn PipelineDisplay(pipeline: HqpPipeline) -> Element {
    let status = pipeline.status.as_ref();
    let volume = pipeline.volume.as_ref();

    let format_rate = |r: u64| {
        if r >= 1_000_000 {
            format!("{:.1} MHz", r as f64 / 1_000_000.0)
        } else {
            format!("{:.1} kHz", r as f64 / 1_000.0)
        }
    };

    rsx! {
        table {
            tr {
                td { "Mode" }
                td { "{status.and_then(|s| s.active_mode.as_deref()).unwrap_or(\"N/A\")}" }
            }
            tr {
                td { "Filter" }
                td { "{status.and_then(|s| s.active_filter.as_deref()).unwrap_or(\"N/A\")}" }
            }
            tr {
                td { "Shaper" }
                td { "{status.and_then(|s| s.active_shaper.as_deref()).unwrap_or(\"N/A\")}" }
            }
            tr {
                td { "Sample Rate" }
                td {
                    if let Some(rate) = status.and_then(|s| s.active_rate) {
                        "{format_rate(rate)}"
                    } else {
                        "N/A"
                    }
                }
            }
            tr {
                td { "Volume" }
                td {
                    if let Some(v) = volume.and_then(|vol| vol.value) {
                        "{v} dB"
                        if volume.map(|vol| vol.is_fixed).unwrap_or(false) {
                            " (fixed)"
                        }
                    } else {
                        "N/A"
                    }
                }
            }
        }
    }
}

/// Zone link table component
#[component]
fn ZoneLinkTable(
    zones: Vec<Zone>,
    links: Vec<ZoneLink>,
    instances: Vec<HqpInstance>,
    on_link: EventHandler<(String, String)>,
    on_unlink: EventHandler<String>,
) -> Element {
    if zones.is_empty() {
        return rsx! {
            p { "No audio zones available. Check that adapters are connected." }
        };
    }

    // Build a map of zone_id -> instance
    let link_map: std::collections::HashMap<_, _> = links
        .iter()
        .map(|l| (l.zone_id.clone(), l.instance.clone()))
        .collect();

    let get_backend = |zone_id: &str| {
        if zone_id.starts_with("lms:") {
            "LMS"
        } else if zone_id.starts_with("openhome:") {
            "OpenHome"
        } else if zone_id.starts_with("upnp:") {
            "UPnP"
        } else {
            "Roon"
        }
    };

    rsx! {
        table {
            thead {
                tr {
                    th { "Zone" }
                    th { "Source" }
                    th { "HQPlayer Instance" }
                    th { "Action" }
                }
            }
            tbody {
                for zone in zones {
                    {
                        let zone_id = zone.zone_id.clone();
                        let zone_id_link = zone_id.clone();
                        let zone_id_unlink = zone_id.clone();
                        let linked = link_map.get(&zone_id).cloned();
                        let backend = get_backend(&zone_id);

                        rsx! {
                            tr {
                                td { "{zone.zone_name}" }
                                td { small { "{backend}" } }
                                td {
                                    if let Some(ref inst) = linked {
                                        strong { "{inst}" }
                                    } else {
                                        select {
                                            if instances.is_empty() {
                                                option { value: "default", "default" }
                                            } else {
                                                for inst in instances.iter() {
                                                    option {
                                                        value: "{inst.name}",
                                                        "{inst.name} ({inst.host.as_deref().unwrap_or(\"unconfigured\")})"
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                td {
                                    if linked.is_some() {
                                        button {
                                            class: "outline secondary",
                                            onclick: move |_| on_unlink.call(zone_id_unlink.clone()),
                                            "Unlink"
                                        }
                                    } else {
                                        button {
                                            onclick: move |_| on_link.call((zone_id_link.clone(), "default".to_string())),
                                            "Link"
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
