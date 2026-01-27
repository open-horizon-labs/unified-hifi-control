//! Knobs page component.
//!
//! Knob device management using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{
    self, FetchFirmwareResponse, FirmwareVersion, KnobConfig, KnobConfigResponse, KnobDevice,
    KnobDevicesResponse, PowerModeConfig, Zone, ZonesResponse,
};
use crate::app::components::Layout;
use crate::app::sse::use_sse;

/// Knobs page component.
#[component]
pub fn Knobs() -> Element {
    let sse = use_sse();

    // Modal state
    let mut modal_open = use_signal(|| false);
    let mut current_knob_id = use_signal(|| None::<String>);
    let mut config_loading = use_signal(|| false);

    // Config form state
    let mut config_name = use_signal(String::new);
    let mut config_rotation_charging = use_signal(|| 180i32);
    let mut config_rotation_not_charging = use_signal(|| 0i32);
    let mut save_status = use_signal(|| None::<String>);

    // Power mode state (charging)
    let mut art_mode_charging = use_signal(|| PowerModeConfig {
        enabled: true,
        timeout_sec: 60,
    });
    let mut dim_charging = use_signal(|| PowerModeConfig {
        enabled: true,
        timeout_sec: 120,
    });
    let mut sleep_charging = use_signal(|| PowerModeConfig {
        enabled: false,
        timeout_sec: 0,
    });
    let mut deep_sleep_charging = use_signal(|| PowerModeConfig {
        enabled: false,
        timeout_sec: 0,
    });

    // Power mode state (battery)
    let mut art_mode_battery = use_signal(|| PowerModeConfig {
        enabled: true,
        timeout_sec: 30,
    });
    let mut dim_battery = use_signal(|| PowerModeConfig {
        enabled: true,
        timeout_sec: 30,
    });
    let mut sleep_battery = use_signal(|| PowerModeConfig {
        enabled: true,
        timeout_sec: 60,
    });
    let mut deep_sleep_battery = use_signal(|| PowerModeConfig {
        enabled: true,
        timeout_sec: 300,
    });

    // Advanced power settings
    let mut wifi_power_save = use_signal(|| false);
    let mut cpu_freq_scaling = use_signal(|| false);
    let mut sleep_poll_stopped = use_signal(|| 60u32);

    // Firmware fetch state
    let mut fw_fetching = use_signal(|| false);
    let mut fw_message = use_signal(|| None::<(bool, String)>); // (is_error, message)

    // Load knobs resource
    let mut knobs = use_resource(|| async {
        api::fetch_json::<KnobDevicesResponse>("/knob/devices")
            .await
            .ok()
    });

    // Load zones resource
    let mut zones =
        use_resource(|| async { api::fetch_json::<ZonesResponse>("/zones").await.ok() });

    // Load firmware version resource
    let mut firmware_version = use_resource(|| async {
        api::fetch_json::<FirmwareVersion>("/firmware/version")
            .await
            .ok()
    });

    // Refresh on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_knobs() {
            knobs.restart();
            zones.restart();
        }
    });

    // Open config modal
    let open_config = move |knob_id: String| {
        current_knob_id.set(Some(knob_id.clone()));
        modal_open.set(true);
        config_loading.set(true);
        save_status.set(None);

        spawn(async move {
            let url = format!("/knob/config?knob_id={}", urlencoding::encode(&knob_id));
            match api::fetch_json::<KnobConfigResponse>(&url).await {
                Ok(resp) => {
                    if let Some(cfg) = resp.config {
                        config_name.set(cfg.name.unwrap_or_default());
                        config_rotation_charging.set(cfg.rotation_charging.unwrap_or(180));
                        config_rotation_not_charging.set(cfg.rotation_not_charging.unwrap_or(0));
                        // Load power modes (charging)
                        art_mode_charging.set(cfg.art_mode_charging.unwrap_or(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 60,
                        }));
                        dim_charging.set(cfg.dim_charging.unwrap_or(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 120,
                        }));
                        sleep_charging.set(cfg.sleep_charging.unwrap_or(PowerModeConfig {
                            enabled: false,
                            timeout_sec: 0,
                        }));
                        deep_sleep_charging.set(cfg.deep_sleep_charging.unwrap_or(
                            PowerModeConfig {
                                enabled: false,
                                timeout_sec: 0,
                            },
                        ));
                        // Load power modes (battery)
                        art_mode_battery.set(cfg.art_mode_battery.unwrap_or(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 30,
                        }));
                        dim_battery.set(cfg.dim_battery.unwrap_or(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 30,
                        }));
                        sleep_battery.set(cfg.sleep_battery.unwrap_or(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 60,
                        }));
                        deep_sleep_battery.set(cfg.deep_sleep_battery.unwrap_or(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 300,
                        }));
                        // Load advanced settings
                        wifi_power_save.set(cfg.wifi_power_save_enabled.unwrap_or(false));
                        cpu_freq_scaling.set(cfg.cpu_freq_scaling_enabled.unwrap_or(false));
                        sleep_poll_stopped.set(cfg.sleep_poll_stopped_sec.unwrap_or(60));
                    } else {
                        config_name.set(String::new());
                        config_rotation_charging.set(180);
                        config_rotation_not_charging.set(0);
                        // Reset power modes to defaults
                        art_mode_charging.set(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 60,
                        });
                        dim_charging.set(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 120,
                        });
                        sleep_charging.set(PowerModeConfig {
                            enabled: false,
                            timeout_sec: 0,
                        });
                        deep_sleep_charging.set(PowerModeConfig {
                            enabled: false,
                            timeout_sec: 0,
                        });
                        art_mode_battery.set(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 30,
                        });
                        dim_battery.set(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 30,
                        });
                        sleep_battery.set(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 60,
                        });
                        deep_sleep_battery.set(PowerModeConfig {
                            enabled: true,
                            timeout_sec: 300,
                        });
                        wifi_power_save.set(false);
                        cpu_freq_scaling.set(false);
                        sleep_poll_stopped.set(60);
                    }
                }
                Err(e) => {
                    save_status.set(Some(format!("Error: {}", e)));
                }
            }
            config_loading.set(false);
        });
    };

    // Save config handler
    let save_config = move |_| {
        if let Some(knob_id) = current_knob_id() {
            let name = config_name();
            let rot_c = config_rotation_charging();
            let rot_nc = config_rotation_not_charging();
            // Capture power modes (charging)
            let art_c = art_mode_charging();
            let dim_c = dim_charging();
            let sleep_c = sleep_charging();
            let deep_c = deep_sleep_charging();
            // Capture power modes (battery)
            let art_b = art_mode_battery();
            let dim_b = dim_battery();
            let sleep_b = sleep_battery();
            let deep_b = deep_sleep_battery();
            // Capture advanced settings
            let wifi_ps = wifi_power_save();
            let cpu_fs = cpu_freq_scaling();
            let poll_stopped = sleep_poll_stopped();

            save_status.set(Some("Saving...".to_string()));

            spawn(async move {
                let cfg = KnobConfig {
                    name: if name.is_empty() { None } else { Some(name) },
                    rotation_charging: Some(rot_c),
                    rotation_not_charging: Some(rot_nc),
                    art_mode_charging: Some(art_c),
                    dim_charging: Some(dim_c),
                    sleep_charging: Some(sleep_c),
                    deep_sleep_charging: Some(deep_c),
                    art_mode_battery: Some(art_b),
                    dim_battery: Some(dim_b),
                    sleep_battery: Some(sleep_b),
                    deep_sleep_battery: Some(deep_b),
                    wifi_power_save_enabled: Some(wifi_ps),
                    cpu_freq_scaling_enabled: Some(cpu_fs),
                    sleep_poll_stopped_sec: Some(poll_stopped),
                };

                let url = format!("/knob/config?knob_id={}", urlencoding::encode(&knob_id));
                match api::post_json::<_, serde_json::Value>(&url, &cfg).await {
                    Ok(_) => {
                        modal_open.set(false);
                        knobs.restart();
                    }
                    Err(e) => {
                        save_status.set(Some(format!("Error: {}", e)));
                    }
                }
            });
        }
    };

    // Fetch firmware handler
    let fetch_firmware = move |_| {
        fw_fetching.set(true);
        fw_message.set(None);

        spawn(async move {
            match api::post_json::<_, FetchFirmwareResponse>("/admin/fetch-firmware", &()).await {
                Ok(resp) => {
                    if let Some(version) = resp.version {
                        fw_message.set(Some((false, format!("Downloaded v{}", version))));
                        firmware_version.restart();
                    } else if let Some(err) = resp.error {
                        fw_message.set(Some((true, err)));
                    }
                }
                Err(e) => {
                    fw_message.set(Some((true, e)));
                }
            }
            fw_fetching.set(false);
        });
    };

    let is_loading = knobs.read().is_none();
    let knobs_list = knobs
        .read()
        .clone()
        .flatten()
        .map(|r| r.knobs)
        .unwrap_or_default();
    let zones_list = zones
        .read()
        .clone()
        .flatten()
        .map(|r| r.zones)
        .unwrap_or_default();
    let fw_version = firmware_version.read().clone().flatten().map(|r| r.version);

    rsx! {
        Layout {
            title: "Knobs".to_string(),
            nav_active: "knobs".to_string(),

            h1 { class: "text-2xl font-bold mb-6", "Knob Devices" }

            p { class: "mb-6 text-muted",
                a {
                    class: "link",
                    href: "https://community.roonlabs.com/t/50-esp32-s3-knob-roon-controller/311363",
                    target: "_blank",
                    rel: "noopener",
                    "Knob Community Thread"
                }
                " - build info, firmware updates, discussion"
            }

            // Knobs section
            section { id: "knobs-section", class: "mb-8",
                if is_loading {
                    div { class: "card p-6", aria_busy: "true", "Loading knobs..." }
                } else if knobs_list.is_empty() {
                    div { class: "card p-6 text-muted", "No knobs registered. Connect a knob to see it here." }
                } else {
                    div { class: "card p-6 overflow-x-auto",
                        table { class: "w-full",
                            thead {
                                tr { class: "border-b border-default",
                                    th { class: "text-left py-2 text-sm", "Name" }
                                    th { class: "text-left py-2 text-sm", "Version" }
                                    th { class: "text-left py-2 text-sm", "IP" }
                                    th { class: "text-left py-2 text-sm", "Zone" }
                                    th { class: "text-left py-2 text-sm", "Battery" }
                                    th { class: "text-left py-2 text-sm", "Last Seen" }
                                    th { class: "text-left py-2 text-sm" }
                                }
                            }
                            tbody {
                                for knob in knobs_list {
                                    KnobRow {
                                        knob: knob.clone(),
                                        zones: zones_list.clone(),
                                        on_config: open_config,
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Firmware section
            section { id: "firmware-section", class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Firmware" }
                    p { class: "text-muted text-sm", "Manage knob firmware updates" }
                }
                div { class: "card p-6",
                    p { class: "mb-4",
                        "Current: "
                        span { class: "font-semibold",
                            if let Some(ref v) = fw_version {
                                "v{v}"
                            } else {
                                "Not installed"
                            }
                        }
                    }
                    div { class: "flex items-center gap-4",
                        button {
                            id: "fetch-btn",
                            class: "btn btn-primary",
                            disabled: fw_fetching(),
                            aria_busy: if fw_fetching() { "true" } else { "false" },
                            onclick: fetch_firmware,
                            "Fetch Latest from GitHub"
                        }
                        a { class: "link", href: "/knobs/flash", "Flash a new knob" }
                        if let Some((is_err, ref msg)) = fw_message() {
                            if is_err {
                                span { class: "status-err", "{msg}" }
                            } else {
                                span { class: "status-ok", "✓ {msg}" }
                            }
                        }
                    }
                }
            }

            // Config modal
            if modal_open() {
                ConfigModal {
                    loading: config_loading(),
                    name: config_name(),
                    rotation_charging: config_rotation_charging(),
                    rotation_not_charging: config_rotation_not_charging(),
                    // Power modes (charging)
                    art_mode_charging: art_mode_charging(),
                    dim_charging: dim_charging(),
                    sleep_charging: sleep_charging(),
                    deep_sleep_charging: deep_sleep_charging(),
                    // Power modes (battery)
                    art_mode_battery: art_mode_battery(),
                    dim_battery: dim_battery(),
                    sleep_battery: sleep_battery(),
                    deep_sleep_battery: deep_sleep_battery(),
                    // Advanced settings
                    wifi_power_save: wifi_power_save(),
                    cpu_freq_scaling: cpu_freq_scaling(),
                    sleep_poll_stopped: sleep_poll_stopped(),
                    save_status: save_status(),
                    on_name_change: move |v| config_name.set(v),
                    on_rotation_charging_change: move |v| config_rotation_charging.set(v),
                    on_rotation_not_charging_change: move |v| config_rotation_not_charging.set(v),
                    // Power mode change handlers (charging)
                    on_art_mode_charging_change: move |v| art_mode_charging.set(v),
                    on_dim_charging_change: move |v| dim_charging.set(v),
                    on_sleep_charging_change: move |v| sleep_charging.set(v),
                    on_deep_sleep_charging_change: move |v| deep_sleep_charging.set(v),
                    // Power mode change handlers (battery)
                    on_art_mode_battery_change: move |v| art_mode_battery.set(v),
                    on_dim_battery_change: move |v| dim_battery.set(v),
                    on_sleep_battery_change: move |v| sleep_battery.set(v),
                    on_deep_sleep_battery_change: move |v| deep_sleep_battery.set(v),
                    // Advanced settings change handlers
                    on_wifi_power_save_change: move |v| wifi_power_save.set(v),
                    on_cpu_freq_scaling_change: move |v| cpu_freq_scaling.set(v),
                    on_sleep_poll_stopped_change: move |v| sleep_poll_stopped.set(v),
                    on_save: save_config,
                    on_close: move |_| modal_open.set(false),
                }
            }
        }
    }
}

/// Format time ago from ISO timestamp
fn format_ago(timestamp: Option<&str>) -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(ts) = timestamp {
            let now = js_sys::Date::now();
            let date = js_sys::Date::new(&wasm_bindgen::JsValue::from_str(ts));
            let diff_ms = now - date.get_time();
            let secs = (diff_ms / 1000.0) as i64;

            // Handle clock skew (negative values) or very recent times
            if secs <= 0 {
                "just now".to_string()
            } else if secs < 60 {
                format!("{}s ago", secs)
            } else if secs < 3600 {
                format!("{}m ago", secs / 60)
            } else if secs < 86400 {
                format!("{}h ago", secs / 3600)
            } else {
                format!("{}d ago", secs / 86400)
            }
        } else {
            "never".to_string()
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        timestamp
            .map(|_| "recently".to_string())
            .unwrap_or_else(|| "never".to_string())
    }
}

/// Format knob display name
fn knob_display_name(knob: &KnobDevice) -> String {
    if let Some(ref name) = knob.name {
        if !name.is_empty() {
            return name.clone();
        }
    }

    let suffix = knob
        .knob_id
        .replace(['-', ':'], "")
        .chars()
        .rev()
        .take(6)
        .collect::<String>()
        .chars()
        .rev()
        .collect::<String>()
        .to_lowercase();

    if suffix.is_empty() {
        "unnamed".to_string()
    } else {
        format!("roon-knob-{}", suffix)
    }
}

/// Knob row component
#[component]
fn KnobRow(knob: KnobDevice, zones: Vec<Zone>, on_config: EventHandler<String>) -> Element {
    let status = knob.status.as_ref();
    let knob_id = knob.knob_id.clone();

    let battery = status
        .and_then(|s| {
            s.battery_level.map(|level| {
                let charging = s.battery_charging.unwrap_or(false);
                if charging {
                    format!("{}% ⚡", level)
                } else {
                    format!("{}%", level)
                }
            })
        })
        .unwrap_or_else(|| "—".to_string());

    let zone_name = status
        .and_then(|s| s.zone_id.as_ref())
        .and_then(|zone_id| zones.iter().find(|z| &z.zone_id == zone_id))
        .map(|z| z.zone_name.clone())
        .unwrap_or_else(|| "—".to_string());

    let ip = status
        .and_then(|s| s.ip.clone())
        .unwrap_or_else(|| "—".to_string());

    let version = knob.version.clone().unwrap_or_else(|| "—".to_string());
    let display_name = knob_display_name(&knob);
    let last_seen = format_ago(knob.last_seen.as_deref());

    rsx! {
        tr { class: "border-b border-default",
            td { class: "py-2", "{display_name}" }
            td { class: "py-2", "{version}" }
            td { class: "py-2", "{ip}" }
            td { class: "py-2", "{zone_name}" }
            td { class: "py-2", "{battery}" }
            td { class: "py-2 text-sm text-muted", "{last_seen}" }
            td { class: "py-2",
                button {
                    class: "btn btn-outline btn-sm",
                    onclick: move |_| on_config.call(knob_id.clone()),
                    "Config"
                }
            }
        }
    }
}

/// Compact power mode input for side-by-side layout
#[component]
fn PowerModeInputCompact(
    label: &'static str,
    hint: &'static str,
    config: PowerModeConfig,
    on_change: EventHandler<PowerModeConfig>,
) -> Element {
    let timeout_sec = config.timeout_sec;
    rsx! {
        div { class: "flex items-center justify-between gap-2 py-1",
            div { class: "flex-1 min-w-0",
                span { class: "text-sm block", "{label}" }
                span { class: "text-xs text-muted block truncate", "{hint}" }
            }
            div { class: "flex items-center gap-1 flex-shrink-0",
                input {
                    class: "input w-16 text-center text-sm py-1",
                    r#type: "number",
                    min: "0",
                    max: "3600",
                    value: "{timeout_sec}",
                    oninput: move |e| {
                        if let Ok(v) = e.value().parse::<u32>() {
                            on_change.call(PowerModeConfig { enabled: v > 0, timeout_sec: v });
                        }
                    }
                }
                span { class: "text-xs text-muted", "s" }
            }
        }
    }
}

/// Format timeout for display (e.g., "60s", "2m", "20m")
fn format_timeout(secs: u32) -> String {
    if secs == 0 {
        "skip".to_string()
    } else if secs < 60 {
        format!("{}s", secs)
    } else if secs < 3600 {
        let mins = secs / 60;
        let rem = secs % 60;
        if rem == 0 {
            format!("{}m", mins)
        } else {
            format!("{}m{}s", mins, rem)
        }
    } else {
        let hrs = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            format!("{}h", hrs)
        } else {
            format!("{}h{}m", hrs, mins)
        }
    }
}

/// State cascade preview component - shows how timeouts resolve as a visual diagram
#[component]
fn StateCascadePreview(
    is_charging: bool,
    art_mode: PowerModeConfig,
    dim: PowerModeConfig,
    sleep: PowerModeConfig,
    deep_sleep: PowerModeConfig,
) -> Element {
    // Build the cascade: each enabled state adds to cumulative time
    // (name, timeout_to_reach, icon, is_power_off)
    let mut states: Vec<(&str, u32, &str, bool)> = vec![("Normal", 0, "☀", false)];

    if art_mode.enabled && art_mode.timeout_sec > 0 {
        states.push(("Art", art_mode.timeout_sec, "🎨", false));
    }
    if dim.enabled && dim.timeout_sec > 0 {
        states.push(("Dim", dim.timeout_sec, "🌙", false));
    }
    if sleep.enabled && sleep.timeout_sec > 0 {
        states.push(("Sleep", sleep.timeout_sec, "💤", false));
    }
    if deep_sleep.enabled && deep_sleep.timeout_sec > 0 {
        states.push(("Off", deep_sleep.timeout_sec, "⏻", true));
    }

    let final_is_power_off = states.last().map(|(_, _, _, off)| *off).unwrap_or(false);

    rsx! {
        div { class: "mt-4 p-3 bg-black/10 dark:bg-black/20 rounded-lg",
            // State flow diagram - more prominent
            div { class: "flex items-center gap-1.5 flex-wrap justify-center",
                for (i, (name, timeout, icon, is_off)) in states.iter().enumerate() {
                    // Arrow and time label (before state, except first)
                    if i > 0 {
                        div { class: "flex flex-col items-center mx-1",
                            span { class: "text-xs text-secondary leading-none", "{format_timeout(*timeout)}" }
                            span { class: "text-secondary", "→" }
                        }
                    }
                    // State box - larger and more visible
                    div {
                        class: format!(
                            "flex flex-col items-center px-3 py-1.5 rounded text-xs leading-tight {}",
                            if i == states.len() - 1 {
                                if *is_off { "bg-red-500/20 text-red-700 dark:text-red-200 font-medium" } else { "bg-green-500/20 text-green-700 dark:text-green-200 font-medium" }
                            } else {
                                "bg-black/5 dark:bg-white/10 text-gray-700 dark:text-gray-200"
                            }
                        ),
                        span { class: "text-base", "{icon}" }
                        span { "{name}" }
                    }
                }
            }
            // Summary line
            p { class: "text-xs text-center text-secondary mt-2",
                if final_is_power_off {
                    "Powers off after inactivity. Rotate to wake."
                } else {
                    "Display stays on."
                }
            }
        }
    }
}

/// Config modal component
#[component]
fn ConfigModal(
    loading: bool,
    name: String,
    rotation_charging: i32,
    rotation_not_charging: i32,
    // Power modes (charging)
    art_mode_charging: PowerModeConfig,
    dim_charging: PowerModeConfig,
    sleep_charging: PowerModeConfig,
    deep_sleep_charging: PowerModeConfig,
    // Power modes (battery)
    art_mode_battery: PowerModeConfig,
    dim_battery: PowerModeConfig,
    sleep_battery: PowerModeConfig,
    deep_sleep_battery: PowerModeConfig,
    // Advanced settings
    wifi_power_save: bool,
    cpu_freq_scaling: bool,
    sleep_poll_stopped: u32,
    save_status: Option<String>,
    on_name_change: EventHandler<String>,
    on_rotation_charging_change: EventHandler<i32>,
    on_rotation_not_charging_change: EventHandler<i32>,
    // Power mode change handlers (charging)
    on_art_mode_charging_change: EventHandler<PowerModeConfig>,
    on_dim_charging_change: EventHandler<PowerModeConfig>,
    on_sleep_charging_change: EventHandler<PowerModeConfig>,
    on_deep_sleep_charging_change: EventHandler<PowerModeConfig>,
    // Power mode change handlers (battery)
    on_art_mode_battery_change: EventHandler<PowerModeConfig>,
    on_dim_battery_change: EventHandler<PowerModeConfig>,
    on_sleep_battery_change: EventHandler<PowerModeConfig>,
    on_deep_sleep_battery_change: EventHandler<PowerModeConfig>,
    // Advanced settings change handlers
    on_wifi_power_save_change: EventHandler<bool>,
    on_cpu_freq_scaling_change: EventHandler<bool>,
    on_sleep_poll_stopped_change: EventHandler<u32>,
    on_save: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| on_close.call(()),

            div {
                class: "card p-6 max-w-3xl w-full mx-4 max-h-[90vh] overflow-y-auto",
                onclick: move |e| e.stop_propagation(),

                // Header
                div { class: "flex items-center justify-between mb-6",
                    h2 { class: "text-xl font-semibold", "Knob Configuration" }
                    button {
                        class: "text-muted hover:text-white text-xl",
                        aria_label: "Close",
                        onclick: move |_| on_close.call(()),
                        "×"
                    }
                }

                if loading {
                    p { class: "text-muted", aria_busy: "true", "Loading configuration..." }
                } else {
                    form {
                        onsubmit: move |e| {
                            e.prevent_default();
                            on_save.call(());
                        },

                        div { class: "mb-4",
                            label { class: "block text-sm font-medium mb-1", "Name" }
                            input {
                                class: "input",
                                r#type: "text",
                                placeholder: "Living Room Knob",
                                value: "{name}",
                                oninput: move |e| on_name_change.call(e.value())
                            }
                        }

                        // Power Settings - Side by Side (includes rotation)
                        fieldset { class: "mb-6",
                            legend { class: "text-sm font-medium mb-2", "Power Management" }
                            p { class: "text-sm text-muted mb-3",
                                "State transitions after inactivity. Timeouts are cumulative. 0 = skip."
                            }

                            div { class: "grid grid-cols-2 gap-6",
                                // Charging column
                                div { class: "bg-elevated rounded-lg p-4",
                                    h4 { class: "text-base font-bold mb-4 flex items-center gap-2",
                                        "⚡ Charging"
                                    }
                                    div { class: "space-y-3",
                                        // Rotation
                                        div { class: "flex items-center justify-between gap-2 py-1",
                                            div { class: "flex-1",
                                                span { class: "text-sm block", "Rotation" }
                                                span { class: "text-xs text-muted", "Display orientation" }
                                            }
                                            select {
                                                class: "input w-20 text-center text-sm py-1",
                                                value: "{rotation_charging}",
                                                onchange: move |e| {
                                                    if let Ok(v) = e.value().parse() {
                                                        on_rotation_charging_change.call(v);
                                                    }
                                                },
                                                option { value: "0", selected: rotation_charging == 0, "0°" }
                                                option { value: "180", selected: rotation_charging == 180, "180°" }
                                            }
                                        }
                                        PowerModeInputCompact {
                                            label: "Art Mode",
                                            hint: "Hide controls, show album art",
                                            config: art_mode_charging.clone(),
                                            on_change: on_art_mode_charging_change,
                                        }
                                        PowerModeInputCompact {
                                            label: "Dim",
                                            hint: "Reduce to 30% brightness",
                                            config: dim_charging.clone(),
                                            on_change: on_dim_charging_change,
                                        }
                                        PowerModeInputCompact {
                                            label: "Sleep",
                                            hint: "Turn off display",
                                            config: sleep_charging.clone(),
                                            on_change: on_sleep_charging_change,
                                        }
                                        PowerModeInputCompact {
                                            label: "Deep Sleep",
                                            hint: "Full power down, encoder wakes",
                                            config: deep_sleep_charging.clone(),
                                            on_change: on_deep_sleep_charging_change,
                                        }
                                    }
                                    StateCascadePreview {
                                        is_charging: true,
                                        art_mode: art_mode_charging.clone(),
                                        dim: dim_charging.clone(),
                                        sleep: sleep_charging.clone(),
                                        deep_sleep: deep_sleep_charging.clone(),
                                    }
                                }

                                // Battery column
                                div { class: "bg-elevated rounded-lg p-4",
                                    h4 { class: "text-base font-bold mb-4 flex items-center gap-2",
                                        "🔋 Battery"
                                    }
                                    div { class: "space-y-3",
                                        // Rotation
                                        div { class: "flex items-center justify-between gap-2 py-1",
                                            div { class: "flex-1",
                                                span { class: "text-sm block", "Rotation" }
                                                span { class: "text-xs text-muted", "Display orientation" }
                                            }
                                            select {
                                                class: "input w-20 text-center text-sm py-1",
                                                value: "{rotation_not_charging}",
                                                onchange: move |e| {
                                                    if let Ok(v) = e.value().parse() {
                                                        on_rotation_not_charging_change.call(v);
                                                    }
                                                },
                                                option { value: "0", selected: rotation_not_charging == 0, "0°" }
                                                option { value: "180", selected: rotation_not_charging == 180, "180°" }
                                            }
                                        }
                                        PowerModeInputCompact {
                                            label: "Art Mode",
                                            hint: "Hide controls, show album art",
                                            config: art_mode_battery.clone(),
                                            on_change: on_art_mode_battery_change,
                                        }
                                        PowerModeInputCompact {
                                            label: "Dim",
                                            hint: "Reduce to 30% brightness",
                                            config: dim_battery.clone(),
                                            on_change: on_dim_battery_change,
                                        }
                                        PowerModeInputCompact {
                                            label: "Sleep",
                                            hint: "Turn off display",
                                            config: sleep_battery.clone(),
                                            on_change: on_sleep_battery_change,
                                        }
                                        PowerModeInputCompact {
                                            label: "Deep Sleep",
                                            hint: "Full power down, encoder wakes",
                                            config: deep_sleep_battery.clone(),
                                            on_change: on_deep_sleep_battery_change,
                                        }
                                    }
                                    StateCascadePreview {
                                        is_charging: false,
                                        art_mode: art_mode_battery.clone(),
                                        dim: dim_battery.clone(),
                                        sleep: sleep_battery.clone(),
                                        deep_sleep: deep_sleep_battery.clone(),
                                    }
                                }
                            }
                        }

                        // Advanced Settings
                        fieldset { class: "mb-6",
                            legend { class: "text-sm font-medium mb-2", "Advanced Settings" }

                            div { class: "space-y-3",
                                label { class: "flex items-center gap-3",
                                    input {
                                        r#type: "checkbox",
                                        class: "checkbox",
                                        checked: wifi_power_save,
                                        onchange: move |_| on_wifi_power_save_change.call(!wifi_power_save)
                                    }
                                    div {
                                        span { class: "block text-sm font-medium", "WiFi Power Save" }
                                        span { class: "block text-xs text-muted", "Saves power, adds latency" }
                                    }
                                }
                                label { class: "flex items-center gap-3",
                                    input {
                                        r#type: "checkbox",
                                        class: "checkbox",
                                        checked: cpu_freq_scaling,
                                        onchange: move |_| on_cpu_freq_scaling_change.call(!cpu_freq_scaling)
                                    }
                                    div {
                                        span { class: "block text-sm font-medium", "CPU Frequency Scaling" }
                                        span { class: "block text-xs text-muted", "Dynamic frequency for power savings" }
                                    }
                                }
                                div { class: "flex items-center gap-4",
                                    div { class: "flex-1",
                                        span { class: "block text-sm font-medium", "Sleep Poll Interval" }
                                        span { class: "block text-xs text-muted", "Update check frequency in sleep" }
                                    }
                                    div { class: "flex items-center gap-2",
                                        input {
                                            class: "input w-20 text-center",
                                            r#type: "number",
                                            min: "10",
                                            max: "600",
                                            value: "{sleep_poll_stopped}",
                                            oninput: move |e| {
                                                if let Ok(v) = e.value().parse::<u32>() {
                                                    on_sleep_poll_stopped_change.call(v);
                                                }
                                            }
                                        }
                                        span { class: "text-sm text-muted", "sec" }
                                    }
                                }
                            }
                        }

                        div { class: "flex items-center gap-4 justify-end",
                            if let Some(ref status) = save_status {
                                span { class: "mr-auto",
                                    if status.starts_with("Error") {
                                        span { class: "status-err", "{status}" }
                                    } else {
                                        span { class: "text-muted", "{status}" }
                                    }
                                }
                            }
                            button {
                                r#type: "button",
                                class: "btn btn-outline",
                                onclick: move |_| on_close.call(()),
                                "Cancel"
                            }
                            button { class: "btn btn-primary", r#type: "submit", "Save" }
                        }
                    }
                }
            }
        }
    }
}
