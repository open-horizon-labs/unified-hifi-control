//! HQPlayer page component.
//!
//! Consolidated HQPlayer control with linked zone playback controls at top.

use dioxus::prelude::*;

use crate::app::api::{
    self, HqpConfig, HqpMatrixProfilesResponse, HqpPipeline, HqpProfile, HqpStatus, NowPlaying,
    Zone, ZonesResponse,
};
use crate::app::components::{HqpMatrixSelect, HqpProfileSelect, Layout, VolumeControlsCompact};
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

/// Control request body
#[derive(Clone, serde::Serialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
}

/// HQPlayer page component.
#[component]
pub fn HqPlayer() -> Element {
    let sse = use_sse();

    // Form fields for config
    let mut host = use_signal(String::new);
    let mut port = use_signal(|| 4321u16);
    let mut web_port = use_signal(|| 8088u16);
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let mut has_credentials = use_signal(|| false);
    let mut config_status = use_signal(|| None::<String>);
    let mut show_config = use_signal(|| false);

    // HQP state
    let mut hqp_loading = use_signal(|| false);
    let mut hqp_error = use_signal(|| None::<String>);

    // Now playing for linked zones
    let mut now_playing_map = use_signal(std::collections::HashMap::<String, NowPlaying>::new);

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
    let mut profiles = use_resource(|| async {
        api::fetch_json::<Vec<HqpProfile>>("/hqp/profiles")
            .await
            .ok()
    });

    // Load matrix profiles
    let mut matrix = use_resource(|| async {
        api::fetch_json::<HqpMatrixProfilesResponse>("/hqplayer/matrix/profiles")
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
            has_credentials.set(cfg.has_web_credentials);
        }
    });

    // Load now playing for linked zones
    let zones_list_signal = use_memo(move || {
        zones
            .read()
            .clone()
            .flatten()
            .map(|r| r.zones)
            .unwrap_or_default()
    });

    let links_signal = use_memo(move || {
        zone_links
            .read()
            .clone()
            .flatten()
            .map(|r| r.links)
            .unwrap_or_default()
    });

    // Fetch now playing for linked zones
    use_effect(move || {
        let links = links_signal();
        if links.is_empty() {
            now_playing_map.set(std::collections::HashMap::new());
            return;
        }
        spawn(async move {
            let mut np_map = std::collections::HashMap::new();
            for link in links {
                let url = format!(
                    "/now_playing?zone_id={}",
                    urlencoding::encode(&link.zone_id)
                );
                if let Ok(np) = api::fetch_json::<NowPlaying>(&url).await {
                    np_map.insert(link.zone_id, np);
                }
            }
            now_playing_map.set(np_map);
        });
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
            // Note: now_playing refresh happens automatically via links_signal effect
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
                    profiles.restart();
                    matrix.restart();
                }
                Err(e) => {
                    config_status.set(Some(format!("Error: {}", e)));
                }
            }
        });
    };

    // Zone control handler
    let control = move |(zone_id, action): (String, String)| {
        spawn(async move {
            let req = ControlRequest {
                zone_id,
                action,
                value: None,
            };
            let _ = api::post_json_no_response("/control", &req).await;
        });
    };

    // Pipeline setting handler
    let set_pipeline = move |(setting, value): (String, String)| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct PipelineRequest {
                setting: String,
                value: String,
            }
            let req = PipelineRequest { setting, value };
            if let Err(e) = api::post_json_no_response("/hqp/pipeline", &req).await {
                hqp_error.set(Some(format!("Pipeline update failed: {e}")));
            } else {
                pipeline.restart();
            }
            hqp_loading.set(false);
        });
    };

    // Load profile handler
    let load_profile = move |profile: String| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct ProfileRequest {
                profile: String,
            }
            let req = ProfileRequest { profile };
            if let Err(e) = api::post_json_no_response("/hqplayer/profile", &req).await {
                hqp_error.set(Some(format!("Profile load failed: {e}")));
            } else {
                pipeline.restart();
            }
            hqp_loading.set(false);
        });
    };

    // Set matrix profile handler
    let set_matrix = move |profile_idx: u32| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct MatrixRequest {
                profile: u32,
            }
            let req = MatrixRequest {
                profile: profile_idx,
            };
            if let Err(e) = api::post_json_no_response("/hqplayer/matrix/profile", &req).await {
                hqp_error.set(Some(format!("Matrix profile failed: {e}")));
            } else {
                matrix.restart();
            }
            hqp_loading.set(false);
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

    let _is_loading = config.read().is_none();
    let current_status = status.read().clone().flatten();
    let current_pipeline = pipeline.read().clone().flatten();
    let profiles_list = profiles.read().clone().flatten().unwrap_or_default();
    let matrix_data = matrix.read().clone().flatten();
    let zones_list = zones_list_signal();
    let links_list = links_signal();
    let instances_list = instances
        .read()
        .clone()
        .flatten()
        .map(|r| r.instances)
        .unwrap_or_default();
    let np_map = now_playing_map();

    // Get linked zones with their data
    let linked_zones: Vec<_> = links_list
        .iter()
        .filter_map(|link| {
            zones_list
                .iter()
                .find(|z| z.zone_id == link.zone_id)
                .cloned()
        })
        .collect();

    let is_connected = current_status
        .as_ref()
        .map(|s| s.connected)
        .unwrap_or(false);

    rsx! {
        Layout {
            title: "HQPlayer".to_string(),
            nav_active: "hqplayer".to_string(),

            h1 { class: "text-2xl font-bold mb-6", "HQPlayer" }

            // Error display
            if let Some(ref error) = hqp_error() {
                div { class: "bg-red-900/20 border border-red-500/50 rounded-lg p-4 mb-6",
                    p { class: "text-red-400 m-0", "{error}" }
                }
            }

            // If not connected, show configuration first and prominently
            if !is_connected {
                section { id: "hqp-config", class: "mb-8",
                    div { class: "card p-6",
                        h2 { class: "text-lg font-semibold mb-4", "Connection" }
                        ConfigForm {
                            host: host,
                            port: port,
                            web_port: web_port,
                            username: username,
                            password: password,
                            has_credentials: has_credentials(),
                            config_status: config_status(),
                            on_save: save_config,
                        }
                    }
                }
            }

            // Connected: show status bar with collapsible settings
            if is_connected {
                div { class: "flex items-center justify-between mb-6",
                    span { class: "status-ok", "✓ Connected to {current_status.as_ref().and_then(|s| s.host.as_deref()).unwrap_or(\"HQPlayer\")}" }
                    button {
                        class: "btn btn-ghost btn-sm",
                        onclick: move |_| show_config.toggle(),
                        if show_config() { "Hide Settings" } else { "Settings" }
                    }
                }

                // Collapsible config when connected
                if show_config() {
                    section { id: "hqp-config", class: "mb-8",
                        div { class: "card p-6",
                            ConfigForm {
                                host: host,
                                port: port,
                                web_port: web_port,
                                username: username,
                                password: password,
                                has_credentials: has_credentials(),
                                config_status: config_status(),
                                on_save: save_config,
                            }
                        }
                    }
                }
            }

            // Linked Zone Controls (only if connected and has linked zones)
            if is_connected && !linked_zones.is_empty() {
                section { id: "linked-zones", class: "mb-8",
                    h2 { class: "text-lg font-semibold mb-4", "Now Playing" }
                    div { class: "grid gap-4 grid-cols-1",
                        for zone in linked_zones.iter() {
                            LinkedZoneCard {
                                key: "{zone.zone_id}",
                                zone: zone.clone(),
                                now_playing: np_map.get(&zone.zone_id).cloned(),
                                on_control: control,
                            }
                        }
                    }
                }
            }

            // DSP Settings (only if connected)
            if is_connected {
                section { id: "hqp-dsp", class: "mb-8",
                    h2 { class: "text-lg font-semibold mb-4", "DSP Settings" }
                    DspSettings {
                        pipeline: current_pipeline,
                        profiles: profiles_list,
                        matrix: matrix_data,
                        loading: hqp_loading(),
                        on_set_pipeline: set_pipeline,
                        on_load_profile: load_profile,
                        on_set_matrix: set_matrix,
                    }
                }
            }

            // Zone Linking section
            section { id: "hqp-zone-links", class: "mb-8",
                h2 { class: "text-lg font-semibold mb-4", "Zone Linking" }
                div { class: "card p-6",
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

/// Linked zone card with playback controls
#[component]
fn LinkedZoneCard(
    zone: Zone,
    now_playing: Option<NowPlaying>,
    on_control: EventHandler<(String, String)>,
) -> Element {
    let zone_id = zone.zone_id.clone();
    let zone_id_prev = zone_id.clone();
    let zone_id_play = zone_id.clone();
    let zone_id_next = zone_id.clone();
    let zone_id_vol_down = zone_id.clone();
    let zone_id_vol_up = zone_id.clone();

    let np = now_playing.as_ref();
    let is_playing = np.map(|n| n.is_playing).unwrap_or(false);

    let volume = np.and_then(|n| n.volume);
    let volume_type = np.and_then(|n| n.volume_type.clone());
    let volume_step = np.and_then(|n| n.volume_step);

    // Album art
    let base_image_url = np.and_then(|n| n.image_url.clone()).unwrap_or_default();
    let image_key = np.and_then(|n| n.image_key.clone());
    let image_url = if let Some(key) = image_key {
        let sep = if base_image_url.contains('?') {
            "&"
        } else {
            "?"
        };
        format!("{}{}k={}", base_image_url, sep, key)
    } else {
        base_image_url
    };
    let has_image = !image_url.is_empty();

    let (track, artist) = np
        .map(|n| {
            if n.line1.as_deref().unwrap_or("Idle") != "Idle" {
                (
                    n.line1.clone().unwrap_or_default(),
                    n.line2.clone().unwrap_or_default(),
                )
            } else {
                (String::new(), String::new())
            }
        })
        .unwrap_or_default();

    rsx! {
        article { class: "card p-4",
            div { class: "flex gap-4 items-start",
                // Album art
                if has_image {
                    img {
                        src: "{image_url}",
                        alt: "Album art",
                        class: "w-20 h-20 sm:w-24 sm:h-24 object-cover rounded-lg bg-elevated flex-shrink-0"
                    }
                } else {
                    div { class: "w-20 h-20 sm:w-24 sm:h-24 rounded-lg bg-elevated flex items-center justify-center text-muted text-2xl flex-shrink-0",
                        "♪"
                    }
                }

                // Info + controls
                div { class: "flex-1 min-w-0",
                    h3 { class: "text-base font-semibold truncate mb-1", "{zone.zone_name}" }

                    if !track.is_empty() {
                        p { class: "text-sm truncate mb-0.5", "{track}" }
                        p { class: "text-sm text-muted truncate", "{artist}" }
                    } else {
                        p { class: "text-sm text-muted", "Nothing playing" }
                    }

                    // Transport controls
                    div { class: "flex items-center gap-2 mt-3",
                        button {
                            class: "btn btn-ghost btn-sm",
                            "aria-label": "Previous track",
                            onclick: move |_| on_control.call((zone_id_prev.clone(), "previous".to_string())),
                            svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                path { d: "M6 6h2v12H6zm3.5 6l8.5 6V6z" }
                            }
                        }
                        button {
                            class: "btn btn-primary btn-sm",
                            "aria-label": if is_playing { "Pause" } else { "Play" },
                            onclick: move |_| on_control.call((zone_id_play.clone(), "play_pause".to_string())),
                            if is_playing {
                                svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                    path { d: "M6 19h4V5H6v14zm8-14v14h4V5h-4z" }
                                }
                            } else {
                                svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                    path { d: "M8 5v14l11-7z" }
                                }
                            }
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            "aria-label": "Next track",
                            onclick: move |_| on_control.call((zone_id_next.clone(), "next".to_string())),
                            svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                path { d: "M6 18l8.5-6L6 6v12zM16 6v12h2V6h-2z" }
                            }
                        }

                        VolumeControlsCompact {
                            volume: volume,
                            volume_type: volume_type,
                            volume_step: volume_step,
                            on_vol_down: move |_| on_control.call((zone_id_vol_down.clone(), "vol_down".to_string())),
                            on_vol_up: move |_| on_control.call((zone_id_vol_up.clone(), "vol_up".to_string())),
                        }
                    }
                }
            }
        }
    }
}

/// Configuration form component
#[component]
fn ConfigForm(
    host: Signal<String>,
    port: Signal<u16>,
    web_port: Signal<u16>,
    username: Signal<String>,
    password: Signal<String>,
    has_credentials: bool,
    config_status: Option<String>,
    on_save: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "space-y-4",
            div {
                label { class: "block text-sm font-medium mb-1", "Host" }
                input {
                    class: "input",
                    r#type: "text",
                    placeholder: "192.168.1.100",
                    value: "{host}",
                    oninput: move |evt| host.set(evt.value())
                }
            }
            div { class: "grid grid-cols-2 gap-4",
                div {
                    label { class: "block text-sm font-medium mb-1", "Native Port" }
                    input {
                        class: "input",
                        r#type: "number",
                        value: "{port}",
                        oninput: move |evt| {
                            if let Ok(p) = evt.value().parse() {
                                port.set(p);
                            }
                        }
                    }
                }
                div {
                    label { class: "block text-sm font-medium mb-1", "Web Port" }
                    input {
                        class: "input",
                        r#type: "number",
                        value: "{web_port}",
                        oninput: move |evt| {
                            if let Ok(p) = evt.value().parse() {
                                web_port.set(p);
                            }
                        }
                    }
                }
            }
            div { class: "grid grid-cols-1 sm:grid-cols-2 gap-4",
                div {
                    label { class: "block text-sm font-medium mb-1", "Username" }
                    input {
                        class: "input",
                        r#type: "text",
                        placeholder: if has_credentials { "(saved)" } else { "admin" },
                        value: "{username}",
                        oninput: move |evt| username.set(evt.value())
                    }
                }
                div {
                    label { class: "block text-sm font-medium mb-1", "Password" }
                    input {
                        class: "input",
                        r#type: "password",
                        placeholder: if has_credentials { "(saved)" } else { "password" },
                        value: "{password}",
                        oninput: move |evt| password.set(evt.value())
                    }
                }
            }
            div { class: "flex items-center gap-4",
                button { class: "btn btn-primary", onclick: move |_| on_save.call(()), "Save" }
                if let Some(ref msg) = config_status {
                    span { class: if msg.contains("Connected") { "status-ok" } else if msg.starts_with("Error") { "status-err" } else { "text-muted" },
                        "{msg}"
                    }
                }
            }
        }
    }
}

/// DSP Settings component with full pipeline controls
#[component]
fn DspSettings(
    pipeline: Option<HqpPipeline>,
    profiles: Vec<HqpProfile>,
    matrix: Option<HqpMatrixProfilesResponse>,
    loading: bool,
    on_set_pipeline: EventHandler<(String, String)>,
    on_load_profile: EventHandler<String>,
    on_set_matrix: EventHandler<u32>,
) -> Element {
    let Some(ref pipe) = pipeline else {
        return rsx! {
            div { class: "card p-6",
                p { class: "text-muted text-center py-4", aria_busy: "true",
                    "Loading DSP settings..."
                }
            }
        };
    };

    let settings = pipe.settings.as_ref();

    let mode_opts = settings.and_then(|s| s.mode.clone());
    let samplerate_opts = settings.and_then(|s| s.samplerate.clone());
    let filter1x_opts = settings.and_then(|s| s.filter1x.clone());
    let filter_nx_opts = settings.and_then(|s| s.filter_nx.clone());
    let shaper_opts = settings.and_then(|s| s.shaper.clone());

    let has_matrix = matrix
        .as_ref()
        .map(|m| !m.profiles.is_empty())
        .unwrap_or(false);
    let matrix_profiles = matrix
        .as_ref()
        .map(|m| m.profiles.clone())
        .unwrap_or_default();
    let matrix_current = matrix.as_ref().and_then(|m| m.current);

    // Dynamic shaper label
    let shaper_label = mode_opts
        .as_ref()
        .and_then(|m| m.selected.as_ref())
        .and_then(|s| s.label.as_ref())
        .map(|label| {
            let lower = label.to_lowercase();
            if lower.contains("sdm") || lower.contains("dsd") {
                "Modulator"
            } else {
                "Dither"
            }
        })
        .unwrap_or("Shaper");

    rsx! {
        div { class: "card p-6",
            // Loading indicator
            if loading {
                div { class: "flex items-center gap-2 mb-4",
                    span { class: "text-muted text-sm", aria_busy: "true", "Updating..." }
                }
            }

            // Profile selectors
            if !profiles.is_empty() || has_matrix {
                div { class: "grid grid-cols-1 sm:grid-cols-2 gap-4 mb-6",
                    if !profiles.is_empty() {
                        label { class: "block",
                            span { class: "block text-sm font-medium mb-1", "Profile" }
                            HqpProfileSelect {
                                profiles: profiles.clone(),
                                on_select: on_load_profile,
                                disabled: loading,
                            }
                        }
                    }
                    if has_matrix {
                        label { class: "block",
                            span { class: "block text-sm font-medium mb-1", "Matrix" }
                            HqpMatrixSelect {
                                profiles: matrix_profiles,
                                active: matrix_current,
                                on_select: on_set_matrix,
                                disabled: loading,
                            }
                        }
                    }
                }
            }

            // Pipeline settings grid - responsive
            div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4",
                HqpSelect {
                    id: "hqp-mode",
                    label: "Mode",
                    setting: "mode",
                    options: mode_opts,
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-samplerate",
                    label: "Sample Rate",
                    setting: "samplerate",
                    options: samplerate_opts,
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-filter1x",
                    label: "Filter (1x)",
                    setting: "filter1x",
                    options: filter1x_opts,
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-filterNx",
                    label: "Filter (Nx)",
                    setting: "filterNx",
                    options: filter_nx_opts,
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-shaper",
                    label: shaper_label,
                    setting: "shaper",
                    options: shaper_opts,
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
            }
        }
    }
}

/// HQPlayer setting select component
#[component]
fn HqpSelect(
    id: &'static str,
    label: &'static str,
    setting: &'static str,
    options: Option<crate::app::api::HqpSettingOptions>,
    #[props(default = false)] disabled: bool,
    on_change: EventHandler<(String, String)>,
) -> Element {
    let opts_list = options
        .as_ref()
        .map(|o| o.options.clone())
        .unwrap_or_default();
    let selected = options
        .as_ref()
        .and_then(|o| o.selected.as_ref())
        .map(|s| s.value.clone())
        .unwrap_or_default();
    let setting_name = setting.to_string();

    rsx! {
        label {
            span { class: "block text-sm font-medium mb-1", "{label}" }
            select {
                id: "{id}",
                class: "input",
                disabled: disabled,
                onchange: move |evt: Event<FormData>| {
                    let value = evt.value();
                    on_change.call((setting_name.clone(), value));
                },
                for opt in opts_list {
                    option {
                        value: "{opt.value}",
                        selected: opt.value == selected,
                        "{opt.label.as_deref().unwrap_or(&opt.value)}"
                    }
                }
            }
        }
    }
}

/// Zone link selector component - simplified single dropdown
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
            p { class: "text-muted", "No audio zones available." }
        };
    }

    // Find currently linked zone (if any)
    let linked_zone = links.first().cloned();
    let _linked_zone_id = linked_zone.as_ref().map(|l| l.zone_id.clone());
    let linked_instance = linked_zone.as_ref().map(|l| l.instance.clone());

    // Default instance for new links (empty string if none configured)
    let default_instance = instances
        .first()
        .map(|i| i.name.clone())
        .unwrap_or_default();

    // First zone ID (computed each render from props)
    let first_zone_id = zones.first().map(|z| z.zone_id.clone()).unwrap_or_default();

    // Selected zone - initialize to first zone if available
    let mut selected_zone = use_signal(|| first_zone_id.clone());
    let mut selected_instance =
        use_signal(|| linked_instance.clone().unwrap_or(default_instance.clone()));

    // Clone for onclick closure
    let first_zone_for_click = first_zone_id.clone();
    let default_instance_for_click = default_instance.clone();

    let has_multiple_instances = instances.len() > 1;

    rsx! {
        if let Some(ref link) = linked_zone {
            // Currently linked - show linked zone with unlink option
            {
                let zone_name = zones
                    .iter()
                    .find(|z| z.zone_id == link.zone_id)
                    .map(|z| z.zone_name.clone())
                    .unwrap_or_else(|| link.zone_id.clone());
                let zone_id = link.zone_id.clone();
                rsx! {
                    div { class: "flex items-center gap-4 flex-wrap",
                        div { class: "flex items-center gap-2",
                            span { class: "text-muted", "Linked to" }
                            span { class: "font-semibold", "{zone_name}" }
                            if has_multiple_instances {
                                span { class: "text-muted", "on" }
                                span { class: "font-semibold", "{link.instance}" }
                            }
                        }
                        button {
                            class: "btn btn-outline btn-sm",
                            onclick: move |_| on_unlink.call(zone_id.clone()),
                            "Unlink"
                        }
                    }
                }
            }
        } else {
            // Not linked - show zone dropdown and link button
            div { class: "flex items-center gap-3 flex-wrap",
                select {
                    class: "input",
                    "aria-label": "Select zone to link",
                    value: "{selected_zone}",
                    onchange: move |evt| selected_zone.set(evt.value()),
                    for zone in zones.iter() {
                        option {
                            value: "{zone.zone_id}",
                            selected: zone.zone_id == selected_zone(),
                            "{zone.zone_name}"
                        }
                    }
                }
                if has_multiple_instances {
                    select {
                        class: "input",
                        "aria-label": "Select HQPlayer instance",
                        value: "{selected_instance}",
                        onchange: move |evt| selected_instance.set(evt.value()),
                        for inst in instances.iter() {
                            option {
                                value: "{inst.name}",
                                selected: inst.name == selected_instance(),
                                "{inst.name}"
                            }
                        }
                    }
                }
                button {
                    class: "btn btn-primary",
                    onclick: move |_| {
                        // Use selected zone, or fall back to first zone if signal wasn't synced
                        let zone_id = {
                            let sel = selected_zone();
                            if sel.is_empty() {
                                first_zone_for_click.clone()
                            } else {
                                sel
                            }
                        };
                        // Use selected instance, or fall back to default if signal wasn't synced
                        let instance = {
                            let sel = selected_instance();
                            if sel.is_empty() {
                                default_instance_for_click.clone()
                            } else {
                                sel
                            }
                        };
                        if !zone_id.is_empty() {
                            on_link.call((zone_id, instance));
                        }
                    },
                    "Link Zone"
                }
            }
        }
    }
}
