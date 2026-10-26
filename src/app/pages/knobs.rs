//! Knobs page component.
//!
//! Knob device management using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{
    self, FetchFirmwareResponse, FirmwareVersion, KnobConfig, KnobConfigResponse, KnobDevice,
    KnobDevicesResponse, ManagedZone, ManagedZonesResponse, PowerModeConfig, ShortcutInfo,
    ShortcutsResponse, Zone, ZonesResponse,
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

    // Volume step override (0.0 = use zone default)
    let mut volume_step_override = use_signal(|| 0.0f64);
    // Zones this controller does not list (per-controller hide list)
    let mut hidden_zone_ids = use_signal(Vec::<String>::new);

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

    // Every zone, hidden ones included: the per-controller hide list is edited against this, not
    // `/zones`, which already omits zones hidden globally.
    let mut managed_zones = use_resource(|| async {
        api::fetch_json::<ManagedZonesResponse>("/api/zones/visibility")
            .await
            .ok()
    });

    // Pinned shortcuts: listed under the controllers table and offered in each controller's map.
    let mut shortcuts = use_resource(|| async {
        api::fetch_json::<ShortcutsResponse>("/api/shortcuts")
            .await
            .ok()
    });

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
            managed_zones.restart();
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
                        volume_step_override.set(cfg.volume_step_override.unwrap_or(0.0));
                        hidden_zone_ids.set(cfg.hidden_zone_ids.unwrap_or_default());
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
                        volume_step_override.set(0.0);
                        hidden_zone_ids.set(Vec::new());
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
            let vol_step = volume_step_override();
            let hidden_ids = hidden_zone_ids();

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
                    volume_step_override: Some(vol_step),
                    hidden_zone_ids: Some(hidden_ids),
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
            title: "Controllers".to_string(),
            nav_active: "knobs".to_string(),

            h1 { class: "text-2xl font-bold mb-6", "Controllers" }

            p { class: "mb-6 text-muted",
                a {
                    class: "link inline-flex min-h-11 items-center -my-2",
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
                    div { class: "card p-6", aria_busy: "true", "Loading controllers..." }
                } else if knobs_list.is_empty() {
                    div { class: "card p-6 text-muted", "No controllers registered. Connect a controller to see it here." }
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

            PinnedShortcuts {
                shortcuts: shortcuts
                    .read()
                    .as_ref()
                    .and_then(|r| r.as_ref())
                    .map(|r| r.shortcuts.clone())
                    .unwrap_or_default(),
                on_changed: move |_| shortcuts.restart(),
            }

            // Firmware section
            section { id: "firmware-section", class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Firmware" }
                    p { class: "text-muted text-sm", "Local OTA firmware for registered controllers" }
                }
                div { class: "card p-6",
                    p { class: "mb-4 text-sm text-muted", "OTA availability depends on each controller’s reported device type and a matching application image on this Bridge. The version below is for the legacy knob target. Refresh downloads matching published images into the Bridge cache; it does not flash a controller. Use the firmware site for initial installation and USB-only devices." }
                    p { class: "mb-4",
                        "Legacy knob image on this Bridge: "
                        span { class: "font-semibold",
                            if let Some(ref v) = fw_version {
                                "v{v}"
                            } else {
                                "Not available"
                            }
                        }
                    }
                    div { class: "flex flex-col items-stretch gap-3 sm:flex-row sm:items-center sm:gap-4",
                        button {
                            id: "fetch-btn",
                            class: "btn btn-primary",
                            disabled: fw_fetching(),
                            aria_busy: if fw_fetching() { "true" } else { "false" },
                            onclick: fetch_firmware,
                            "Refresh OTA images"
                        }
                        a {
                            class: "link inline-flex min-h-11 items-center",
                            href: crate::app::KNOB_FLASHER_URL,
                            target: "_blank",
                            rel: "noopener noreferrer",
                            "Firmware releases and installation"
                        }
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
                    volume_step_override: volume_step_override(),
                    managed_zones: managed_zones
                        .read()
                        .as_ref()
                        .and_then(|r| r.as_ref())
                        .map(|r| r.zones.clone())
                        .unwrap_or_default(),
                    controller_id: current_knob_id().unwrap_or_default(),
                    shortcuts: shortcuts
                        .read()
                        .as_ref()
                        .and_then(|r| r.as_ref())
                        .map(|r| r.shortcuts.clone())
                        .unwrap_or_default(),
                    on_shortcuts_changed: move |_| shortcuts.restart(),
                    hidden_zone_ids: hidden_zone_ids(),
                    current_zone_id: current_knob_id()
                        .and_then(|id| {
                            knobs
                                .read()
                                .as_ref()
                                .and_then(|r| r.as_ref())
                                .and_then(|r| r.knobs.iter().find(|k| k.knob_id == id).cloned())
                        })
                        .and_then(|k| k.status.and_then(|st| st.zone_id)),
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
                    on_volume_step_override_change: move |v| volume_step_override.set(v),
                    on_hidden_zone_ids_change: move |v| hidden_zone_ids.set(v),
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

fn controller_type_name(device_type: Option<&str>) -> &'static str {
    match device_type {
        Some(value) => crate::firmware_catalog::FirmwareTarget::parse(value)
            .map(|target| target.name())
            .unwrap_or("Unknown controller type"),
        None => "Device type not reported",
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
    let device_name = controller_type_name(knob.device_type.as_deref());
    let last_seen = format_ago(knob.last_seen.as_deref());

    rsx! {
        tr { class: "border-b border-default",
            td { class: "py-2",
                div { "{display_name}" }
                div { class: "text-sm text-muted", "{device_name}" }
            }
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

/// One line per zone this controller can list, hidden ones included.
///
/// What it edits is the controller's *own* hide list, which stacks on the global one in Settings:
/// a zone hidden for every controller is shown here as such and cannot be switched on from this
/// screen, because that decision belongs to Settings. Hiding is a list filter only (`zone_list`),
/// so the controller's current zone keeps playing even when it is hidden here; the row says so
/// rather than leaving a person to wonder why nothing stopped.
#[component]
fn ControllerZones(
    zones: Vec<ManagedZone>,
    hidden_zone_ids: Vec<String>,
    current_zone_id: Option<String>,
    on_change: EventHandler<Vec<String>>,
) -> Element {
    let rows = zone_rows(&zones, &hidden_zone_ids);
    let listed_here = rows
        .iter()
        .filter(|r| !r.hidden_everywhere && !r.hidden_here)
        .count();
    let total = rows.iter().filter(|r| !r.hidden_everywhere).count();
    let any_hidden_here = hidden_zone_ids.iter().any(|id| !id.is_empty());

    rsx! {
        fieldset { class: "mb-6",
            legend { class: "text-sm font-medium mb-2", "Zones on this controller" }
            p { class: "text-sm text-muted mb-3",
                "Untick a zone to keep it out of this controller's zone list. Other controllers are unaffected, and a hidden zone still plays if the controller is already on it."
            }
            if rows.is_empty() {
                p { class: "text-sm text-muted", "No zones discovered yet." }
            } else {
                div { class: "bg-elevated rounded-lg p-3",
                    div { class: "flex items-center justify-between gap-3 mb-2",
                        span { class: "text-sm text-secondary", "Showing {listed_here} of {total}" }
                        if any_hidden_here {
                            button {
                                r#type: "button",
                                class: "btn btn-outline btn-sm",
                                onclick: move |_| on_change.call(Vec::new()),
                                "Show all"
                            }
                        }
                    }
                    ul { class: "space-y-1",
                        for row in rows.iter() {
                            {
                                let zone_id = row.zone_id.clone();
                                let is_current = current_zone_id.as_deref() == Some(row.zone_id.as_str());
                                let hidden_now = hidden_zone_ids.clone();
                                let checked = !row.hidden_here && !row.hidden_everywhere;
                                let disabled = row.hidden_everywhere;
                                rsx! {
                                    li { key: "{row.zone_id}", class: "flex flex-wrap items-center gap-x-3 gap-y-1 min-h-11",
                                        label { class: "flex items-center gap-3 flex-1 min-w-0",
                                            input {
                                                r#type: "checkbox",
                                                class: "checkbox",
                                                checked: checked,
                                                disabled: disabled,
                                                onchange: move |_| {
                                                    on_change.call(toggled(&hidden_now, &zone_id));
                                                }
                                            }
                                            span { class: "truncate", "{row.label}" }
                                        }
                                        if row.hidden_everywhere {
                                            span { class: "text-xs text-muted", "Hidden for all controllers (Settings)" }
                                        } else if row.unavailable {
                                            span { class: "text-xs text-muted", "Not currently available" }
                                        } else if is_current && row.hidden_here {
                                            span { class: "text-xs text-muted", "Playing here now; hidden from the list only" }
                                        } else if is_current {
                                            span { class: "badge badge-secondary", "This controller's zone" }
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

/// A zone as the per-controller list shows it.
#[derive(Debug, Clone, PartialEq)]
struct ZoneRow {
    zone_id: String,
    label: String,
    hidden_here: bool,
    hidden_everywhere: bool,
    /// In this controller's hide list but no longer discovered. Kept visible so the entry can be
    /// cleared; otherwise a zone that comes back later would still be hidden with no way to see why.
    unavailable: bool,
}

/// Rows for every zone the bridge manages, then any hide-list entries it no longer knows.
fn zone_rows(zones: &[ManagedZone], hidden_here: &[String]) -> Vec<ZoneRow> {
    let mut rows: Vec<ZoneRow> = zones
        .iter()
        .map(|z| ZoneRow {
            zone_id: z.zone_id.clone(),
            label: z.qualified_label(),
            hidden_here: hidden_here.contains(&z.zone_id),
            hidden_everywhere: z.hidden,
            unavailable: false,
        })
        .collect();
    for id in hidden_here {
        if !id.is_empty() && !zones.iter().any(|z| &z.zone_id == id) {
            rows.push(ZoneRow {
                zone_id: id.clone(),
                label: id.clone(),
                hidden_here: true,
                hidden_everywhere: false,
                unavailable: true,
            });
        }
    }
    rows
}

/// The hide list with `zone_id` flipped, keeping the order of the rest.
fn toggled(hidden: &[String], zone_id: &str) -> Vec<String> {
    if hidden.iter().any(|h| h == zone_id) {
        hidden.iter().filter(|h| *h != zone_id).cloned().collect()
    } else {
        let mut next = hidden.to_vec();
        next.push(zone_id.to_string());
        next
    }
}

// =============================================================================
// Shortcuts: what a remote's or controller's buttons play
// =============================================================================

/// A button as a person names it: "Home, long press". Keys this UI has no friendly name for stay
/// recognisable (`usage_0192` becomes "Key 0192") so a remote's unusual button is still mappable.
fn key_label(key: &str) -> String {
    match key {
        "play_pause" => "Play/Pause".to_string(),
        "fast_forward" => "Fast-forward".to_string(),
        other => match other.strip_prefix("usage_") {
            Some(code) if !code.is_empty() => format!("Key {code}"),
            _ => {
                let spaced = other.replace('_', " ");
                let mut chars = spaced.chars();
                match chars.next() {
                    Some(first) => first.to_uppercase().chain(chars).collect(),
                    None => String::new(),
                }
            }
        },
    }
}

fn gesture_label(gesture: &str) -> &'static str {
    match gesture {
        "long" => "long press",
        "double" => "double press",
        _ => "press",
    }
}

fn event_label(event: &str) -> String {
    match event.split_once(':') {
        Some((key, gesture)) => format!("{}, {}", key_label(key), gesture_label(gesture)),
        None => key_label(event),
    }
}

/// Keys that can only carry a shortcut on a long press, and keys that cannot at all, mirroring the
/// bridge's rules (`crate::shortcuts::event_key`) so the form never offers what the bridge refuses.
fn gestures_for_key(key: &str) -> &'static [&'static str] {
    match key {
        "volume_up" | "volume_down" | "mute" => &[],
        "play_pause" | "play" | "pause" | "next" | "previous" => &["long"],
        _ => &["press", "long", "double"],
    }
}

/// The chip for one binding: a class, and words, because colour alone is not a state.
fn binding_chip(status: &str) -> (&'static str, &'static str) {
    match status {
        "ok" => ("badge badge-success", "Worked last time"),
        "untested" => ("badge badge-secondary", "Not tried yet"),
        "last_failed" => ("badge status-err", "Last press failed"),
        "no_zone" => ("badge status-err", "No zone yet"),
        "zone_missing" => ("badge status-err", "Zone unavailable"),
        "provider_mismatch" => ("badge status-err", "Wrong provider for this zone"),
        _ => ("badge badge-secondary", "Unknown"),
    }
}

/// The sentence under "Plays on", or `None` when the zone is fine and unremarkable.
fn plays_on_note(plays_on: &api::PlaysOnInfo) -> Option<String> {
    match plays_on.state.as_str() {
        "none" => Some(
            "This controller has no zone yet. Choose one on the controller, or assign one over MQTT, then buttons will play there."
                .to_string(),
        ),
        "missing" => Some(format!(
            "{} is not available right now, so a press will say so instead of playing somewhere else.",
            plays_on
                .zone_id
                .clone()
                .unwrap_or_else(|| "The zone".to_string())
        )),
        _ if plays_on.hidden_globally => Some(
            "Hidden for all controllers in Settings. It still plays; it just is not listed."
                .to_string(),
        ),
        _ if plays_on.hidden_here => Some(
            "Hidden from this controller's zone list above. It still plays; it just is not listed."
                .to_string(),
        ),
        _ => None,
    }
}

/// This controller's button map. Saves each change as it is made (the map lives on the bridge, not
/// in the device's config), so "Test" always plays what is shown.
#[component]
fn ControllerShortcuts(
    controller_id: String,
    shortcuts: Vec<ShortcutInfo>,
    on_changed: EventHandler<()>,
) -> Element {
    let url = format!(
        "/api/controllers/{}/shortcuts",
        urlencoding::encode(&controller_id)
    );
    let fetch_url = url.clone();
    let mut data = use_resource(move || {
        let url = fetch_url.clone();
        async move {
            api::fetch_json::<api::ControllerShortcutsResponse>(&url)
                .await
                .ok()
        }
    });
    // (is_error, text)
    let mut note = use_signal(|| None::<(bool, String)>);
    let mut new_key = use_signal(String::new);
    let mut new_gesture = use_signal(|| "press".to_string());
    let mut new_shortcut = use_signal(String::new);

    // Replace the whole map, then reload so the chips reflect what the bridge now has. Declared
    // before the early return below: hooks must run on every render.
    let save_map = use_callback({
        let url = url.clone();
        move |map: std::collections::BTreeMap<String, String>| {
            let url = url.clone();
            spawn(async move {
                let req = api::ControllerMapRequest { bindings: map };
                match api::put_json::<_, serde_json::Value>(&url, &req).await {
                    Ok(_) => note.set(Some((false, "Saved.".to_string()))),
                    Err(message) => note.set(Some((true, message))),
                }
                data.restart();
                on_changed.call(());
            });
        }
    });

    let test_press = use_callback({
        let url = url.clone();
        move |event: String| {
            let url = format!("{url}/test");
            spawn(async move {
                let (key, gesture) = event
                    .split_once(':')
                    .map(|(k, g)| (k.to_string(), g.to_string()))
                    .unwrap_or((event.clone(), "press".to_string()));
                let req = api::ControllerTestRequest { key, gesture };
                match api::post_json::<_, api::PressOutcomeInfo>(&url, &req).await {
                    Ok(outcome) => note.set(Some((!outcome.ok, outcome.message))),
                    Err(message) => note.set(Some((true, message))),
                }
                data.restart();
                on_changed.call(());
            });
        }
    });

    let loaded = data.read().as_ref().and_then(|r| r.clone());
    let Some(current) = loaded else {
        return rsx! {
            div { class: "mt-6 pt-6 border-t border-default",
                h3 { class: "text-base font-semibold mb-2", "Buttons and shortcuts" }
                p { class: "text-sm text-muted", aria_busy: "true", "Loading…" }
            }
        };
    };

    let bound: std::collections::BTreeMap<String, String> = current
        .bindings
        .iter()
        .map(|b| (b.event.clone(), b.shortcut_id.clone()))
        .collect();

    let plays_on = current.plays_on.clone();
    let zone_text = plays_on
        .zone_name
        .clone()
        .or_else(|| plays_on.zone_id.clone())
        .unwrap_or_else(|| "no zone yet".to_string());
    let zone_note = plays_on_note(&plays_on);

    // Events heard recently that nothing is bound to yet: what a person most likely wants to map.
    let unbound_recent: Vec<api::RecentEventInfo> = current
        .recent_events
        .iter()
        .filter(|e| !bound.contains_key(&e.event) && !e.event.is_empty())
        .cloned()
        .collect();

    let chosen_key = new_key();
    let allowed_gestures = gestures_for_key(chosen_key.trim());
    let can_add = !chosen_key.trim().is_empty()
        && allowed_gestures.contains(&new_gesture().as_str())
        && shortcuts.iter().any(|s| s.id == new_shortcut());

    rsx! {
        div { class: "mt-6 pt-6 border-t border-default",
            h3 { class: "text-base font-semibold mb-1", "Buttons and shortcuts" }
            p { class: "text-sm text-muted mb-3",
                "Saved as you change them. A press plays the shortcut on this controller's own zone, and says so if it cannot."
            }
            p { class: "text-sm mb-1",
                span { class: "text-secondary", "Plays on: " }
                span { class: "font-medium", "{zone_text}" }
            }
            if let Some(text) = zone_note {
                p { class: "text-xs text-muted mb-3", "{text}" }
            }

            if shortcuts.is_empty() {
                p { class: "text-sm text-muted mb-3",
                    "Nothing is pinned yet. In the Library, open the "
                    span { class: "font-medium", "⋯" }
                    " menu on an album, playlist or station and choose "
                    span { class: "font-medium", "Pin as shortcut" }
                    ", then come back here."
                }
            }

            if current.bindings.is_empty() {
                p { class: "text-sm text-muted mb-3", "No buttons are mapped on this controller." }
            } else {
                ul { class: "space-y-2 mb-4",
                    for binding in current.bindings.clone() {
                        {
                            let (chip_class, chip_text) = binding_chip(&binding.status);
                            let event = binding.event.clone();
                            let label = event_label(&binding.event);
                            let detail = binding
                                .last_press
                                .as_ref()
                                .filter(|p| !p.ok)
                                .map(|p| p.message.clone());
                            let current_id = binding.shortcut_id.clone();
                            let options = shortcuts.clone();
                            let bound_for_change = bound.clone();
                            let bound_for_remove = bound.clone();
                            let change_event = event.clone();
                            let remove_event = event.clone();
                            let test_event = event.clone();
                            rsx! {
                                li { key: "{binding.event}", class: "bg-elevated rounded-lg p-3",
                                    div { class: "flex flex-wrap items-center gap-x-3 gap-y-2",
                                        span { class: "font-medium min-w-32", "{label}" }
                                        select {
                                            class: "input flex-1 min-w-40 text-sm py-1",
                                            aria_label: "Shortcut for {label}",
                                            onchange: move |e| {
                                                let mut next = bound_for_change.clone();
                                                next.insert(change_event.clone(), e.value());
                                                save_map(next);
                                            },
                                            for option in options {
                                                option {
                                                    value: "{option.id}",
                                                    selected: option.id == current_id,
                                                    "{option.name}"
                                                }
                                            }
                                        }
                                        span { class: "{chip_class}", "{chip_text}" }
                                        button {
                                            r#type: "button",
                                            class: "btn btn-outline btn-sm",
                                            onclick: move |_| test_press(test_event.clone()),
                                            "Test"
                                        }
                                        button {
                                            r#type: "button",
                                            class: "btn btn-outline btn-sm",
                                            aria_label: "Remove {label}",
                                            onclick: move |_| {
                                                let mut next = bound_for_remove.clone();
                                                next.remove(&remove_event);
                                                save_map(next);
                                            },
                                            "Remove"
                                        }
                                    }
                                    if let Some(text) = detail {
                                        p { class: "text-xs status-err mt-1", "{text}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if !shortcuts.is_empty() {
                div { class: "bg-elevated rounded-lg p-3",
                    h4 { class: "text-sm font-medium mb-2", "Map a button" }
                    if !unbound_recent.is_empty() {
                        div { class: "mb-3",
                            p { class: "text-xs text-muted mb-1",
                                "Press a button on the remote, then choose it here:"
                            }
                            div { class: "flex flex-wrap gap-2",
                                for recent in unbound_recent {
                                    {
                                        let event = recent.event.clone();
                                        let label = event_label(&recent.event);
                                        rsx! {
                                            button {
                                                key: "{recent.event}",
                                                r#type: "button",
                                                class: "btn btn-outline btn-sm",
                                                onclick: move |_| {
                                                    if let Some((key, gesture)) = event.split_once(':') {
                                                        new_key.set(key.to_string());
                                                        new_gesture.set(gesture.to_string());
                                                    }
                                                },
                                                "{label}"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    div { class: "flex flex-wrap items-end gap-3",
                        label { class: "flex flex-col text-xs text-muted",
                            "Button"
                            input {
                                class: "input text-sm py-1 w-40",
                                r#type: "text",
                                placeholder: "home, usage_0192…",
                                value: "{chosen_key}",
                                oninput: move |e| new_key.set(e.value().to_lowercase()),
                                // Enter must add the button, not submit the settings form above.
                                onkeydown: move |e| {
                                    if e.key() == Key::Enter {
                                        e.prevent_default();
                                    }
                                }
                            }
                        }
                        label { class: "flex flex-col text-xs text-muted",
                            "Gesture"
                            select {
                                class: "input text-sm py-1",
                                onchange: move |e| new_gesture.set(e.value()),
                                for gesture in ["press", "long", "double"] {
                                    option {
                                        value: "{gesture}",
                                        selected: new_gesture() == gesture,
                                        disabled: !allowed_gestures.contains(&gesture) && !chosen_key.trim().is_empty(),
                                        "{gesture_label(gesture)}"
                                    }
                                }
                            }
                        }
                        label { class: "flex flex-col text-xs text-muted flex-1 min-w-40",
                            "Plays"
                            select {
                                class: "input text-sm py-1",
                                onchange: move |e| new_shortcut.set(e.value()),
                                option { value: "", selected: new_shortcut().is_empty(), "Choose a shortcut…" }
                                for option in shortcuts.clone() {
                                    option {
                                        value: "{option.id}",
                                        selected: new_shortcut() == option.id,
                                        "{option.name}"
                                    }
                                }
                            }
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-primary btn-sm",
                            disabled: !can_add,
                            onclick: {
                                let bound = bound.clone();
                                move |_| {
                                    let key = new_key().trim().to_string();
                                    let mut next = bound.clone();
                                    next.insert(format!("{key}:{}", new_gesture()), new_shortcut());
                                    save_map(next);
                                    new_key.set(String::new());
                                    new_shortcut.set(String::new());
                                }
                            },
                            "Add"
                        }
                    }
                    if !chosen_key.trim().is_empty() && allowed_gestures.is_empty() {
                        p { class: "text-xs status-err mt-2",
                            "Volume and mute always mean volume and mute, so they cannot start a shortcut."
                        }
                    } else if allowed_gestures == ["long"] {
                        p { class: "text-xs text-muted mt-2",
                            "This key keeps its normal job on a press, so only a long press can start a shortcut. The normal action still runs first."
                        }
                    }
                }
            }

            if let Some((is_error, text)) = note() {
                p {
                    class: if is_error { "text-sm status-err mt-3" } else { "text-sm text-muted mt-3" },
                    role: if is_error { "alert" } else { "status" },
                    "{text}"
                }
            }
        }
    }
}

/// Every pinned shortcut, with rename and delete. Deleting unbinds it everywhere, which the
/// confirmation says before it happens.
#[component]
fn PinnedShortcuts(shortcuts: Vec<ShortcutInfo>, on_changed: EventHandler<()>) -> Element {
    let mut message = use_signal(|| None::<(bool, String)>);
    let mut confirming = use_signal(|| None::<String>);

    rsx! {
        section { id: "shortcuts-section", class: "mb-8",
            div { class: "mb-4",
                h2 { class: "text-xl font-semibold", "Pinned shortcuts" }
                p { class: "text-muted text-sm",
                    "Library items pinned from the Library. Map them to buttons in each controller's configuration."
                }
            }
            div { class: "card p-6",
                if shortcuts.is_empty() {
                    p { class: "text-muted",
                        "Nothing pinned yet. In the Library, use ⋯ → Pin as shortcut on an album, playlist or station."
                    }
                } else {
                    ul { class: "divide-y divide-default",
                        for shortcut in shortcuts.clone() {
                            {
                                let id = shortcut.id.clone();
                                let rename_id = shortcut.id.clone();
                                let delete_id = shortcut.id.clone();
                                let confirm_id = shortcut.id.clone();
                                let is_confirming = confirming().as_deref() == Some(shortcut.id.as_str());
                                let result = shortcut.last_press.clone();
                                rsx! {
                                    li { key: "{id}", class: "py-3 flex flex-wrap items-center gap-x-4 gap-y-2",
                                        div { class: "flex-1 min-w-48",
                                            input {
                                                class: "input text-sm py-1 w-full",
                                                aria_label: "Name of shortcut {shortcut.name}",
                                                value: "{shortcut.name}",
                                                onchange: move |e| {
                                                    let name = e.value();
                                                    let id = rename_id.clone();
                                                    spawn(async move {
                                                        let url = format!("/api/shortcuts/{}", urlencoding::encode(&id));
                                                        let body = serde_json::json!({ "name": name });
                                                        match api::patch_json::<_, serde_json::Value>(&url, &body).await {
                                                            Ok(_) => message.set(None),
                                                            Err(error) => message.set(Some((true, error))),
                                                        }
                                                        on_changed.call(());
                                                    });
                                                }
                                            }
                                            p { class: "text-xs text-muted mt-1",
                                                "{shortcut.title} · {api::source_label(&shortcut.provider)}"
                                            }
                                        }
                                        if let Some(result) = result {
                                            span {
                                                class: if result.ok { "badge badge-success" } else { "badge status-err" },
                                                title: "{result.message}",
                                                if result.ok { "Worked last time" } else { "Last press failed" }
                                            }
                                        }
                                        if is_confirming {
                                            span { class: "text-sm", "Delete and unmap it everywhere?" }
                                            button {
                                                r#type: "button",
                                                class: "btn btn-outline btn-sm",
                                                onclick: move |_| {
                                                    let id = delete_id.clone();
                                                    confirming.set(None);
                                                    spawn(async move {
                                                        let url = format!("/api/shortcuts/{}", urlencoding::encode(&id));
                                                        match api::delete_json::<serde_json::Value>(&url).await {
                                                            Ok(_) => message.set(None),
                                                            Err(error) => message.set(Some((true, error))),
                                                        }
                                                        on_changed.call(());
                                                    });
                                                },
                                                "Delete"
                                            }
                                            button {
                                                r#type: "button",
                                                class: "btn btn-outline btn-sm",
                                                onclick: move |_| confirming.set(None),
                                                "Keep"
                                            }
                                        } else {
                                            button {
                                                r#type: "button",
                                                class: "btn btn-outline btn-sm",
                                                aria_label: "Delete shortcut {shortcut.name}",
                                                onclick: move |_| confirming.set(Some(confirm_id.clone())),
                                                "Delete"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some((true, text)) = message() {
                    p { class: "text-sm status-err mt-3", role: "alert", "{text}" }
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
        div { class: "mt-4 p-3 bg-elevated rounded-lg",
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
                                if *is_off { "bg-hover status-err font-medium" } else { "bg-hover status-ok font-medium" }
                            } else {
                                "bg-hover text-secondary"
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
    volume_step_override: f64,
    // Zones offered to this controller
    managed_zones: Vec<ManagedZone>,
    hidden_zone_ids: Vec<String>,
    current_zone_id: Option<String>,
    // Shortcut map (saved by its own API as it is edited, not by this form's Save)
    controller_id: String,
    shortcuts: Vec<ShortcutInfo>,
    on_shortcuts_changed: EventHandler<()>,
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
    on_volume_step_override_change: EventHandler<f64>,
    on_hidden_zone_ids_change: EventHandler<Vec<String>>,
    on_save: EventHandler<()>,
    on_close: EventHandler<()>,
) -> Element {
    let medium_step_display = format!("{:.1}", volume_step_override * 3.0);
    let large_step_display = format!("{:.1}", volume_step_override * 5.0);

    rsx! {
        div {
            class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50",
            onclick: move |_| on_close.call(()),

            div {
                class: "card p-6 max-w-3xl w-full mx-4 max-h-[90vh] overflow-y-auto",
                onclick: move |e| e.stop_propagation(),

                // Header
                div { class: "flex items-center justify-between mb-6",
                    h2 { class: "text-xl font-semibold", "Controller Configuration" }
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
                                placeholder: "Living Room Controller",
                                value: "{name}",
                                oninput: move |e| on_name_change.call(e.value())
                            }
                        }

                        ControllerZones {
                            zones: managed_zones.clone(),
                            hidden_zone_ids: hidden_zone_ids.clone(),
                            current_zone_id: current_zone_id.clone(),
                            on_change: on_hidden_zone_ids_change,
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

                        // Volume Step Override
                        fieldset { class: "mb-6",
                            legend { class: "text-sm font-medium mb-2", "Volume Step Override" }

                            div { class: "space-y-3",
                                div { class: "flex items-center gap-4",
                                    div { class: "flex-1",
                                        span { class: "block text-sm font-medium", "Base Step" }
                                        span { class: "block text-xs text-muted", "0 = use zone default" }
                                    }
                                    div { class: "flex items-center gap-2",
                                        input {
                                            class: "input w-20 text-center",
                                            r#type: "number",
                                            min: "0",
                                            max: "100",
                                            step: "0.5",
                                            value: format!("{volume_step_override}"),
                                            oninput: move |e| {
                                                let v = e.value().parse::<f64>().unwrap_or(0.0);
                                                on_volume_step_override_change.call(if v < 0.0 { 0.0 } else { v });
                                            }
                                        }
                                    }
                                }

                                if volume_step_override > 0.0 {
                                    div { class: "bg-elevated rounded-lg p-3 text-sm space-y-1",
                                        div { class: "flex justify-between",
                                            span { class: "text-muted", "Slow rotation (1 tick)" }
                                            span { class: "font-medium", "{volume_step_override:.1}" }
                                        }
                                        div { class: "flex justify-between",
                                            span { class: "text-muted", "Medium rotation (2 ticks)" }
                                            span { class: "font-medium",
                                                "{medium_step_display}"
                                            }
                                        }
                                        div { class: "flex justify-between",
                                            span { class: "text-muted", "Fast rotation (3+ ticks)" }
                                            span { class: "font-medium",
                                                "{large_step_display}"
                                            }
                                        }
                                        p { class: "text-xs text-muted mt-2",
                                            "The knob multiplies your base step by rotation speed. Faster turning = larger volume jumps."
                                        }
                                    }
                                } else {
                                    div { class: "bg-elevated rounded-lg p-3 text-sm text-muted",
                                        "Using zone default step size"
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

                    // Mounted only while the config has loaded (`loading` swaps this branch out),
                    // so opening another controller always starts the editor fresh.
                    ControllerShortcuts {
                        controller_id: controller_id.clone(),
                        shortcuts: shortcuts.clone(),
                        on_changed: on_shortcuts_changed,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod controller_identity_tests {
    use super::{controller_type_name, KnobDevice};

    #[test]
    fn older_registration_without_device_type_still_renders() {
        let device: KnobDevice = serde_json::from_str(r#"{"knob_id":"legacy"}"#).unwrap();
        assert_eq!(device.device_type, None);
        assert_eq!(
            controller_type_name(device.device_type.as_deref()),
            "Device type not reported"
        );
    }

    #[test]
    fn reported_type_survives_wire_parsing_and_uses_catalog_name() {
        let device: KnobDevice =
            serde_json::from_str(r#"{"knob_id":"frame1","device_type":"frame"}"#).unwrap();
        assert_eq!(device.device_type.as_deref(), Some("frame"));
        assert_eq!(
            controller_type_name(device.device_type.as_deref()),
            "HiPhi Frame"
        );
        assert_eq!(
            controller_type_name(Some("stackchan")),
            "Kizz Playback Companion"
        );
        assert_eq!(
            controller_type_name(Some("future-model")),
            "Unknown controller type"
        );
    }
}

#[cfg(test)]
mod controller_zone_tests {
    use super::{toggled, zone_rows};
    use crate::app::api::ManagedZone;

    fn zone(id: &str, name: &str, hidden: bool) -> ManagedZone {
        ManagedZone {
            zone_id: id.into(),
            zone_name: name.into(),
            source: "roon".into(),
            hidden,
            ..Default::default()
        }
    }

    #[test]
    fn globally_hidden_zone_is_marked_and_not_a_controller_choice() {
        let rows = zone_rows(
            &[zone("roon:a", "A", true), zone("roon:b", "B", false)],
            &[],
        );
        assert!(rows[0].hidden_everywhere && !rows[0].hidden_here);
        assert!(!rows[1].hidden_everywhere && !rows[1].hidden_here);
    }

    #[test]
    fn hide_entry_for_a_zone_no_longer_discovered_stays_listed_so_it_can_be_cleared() {
        let rows = zone_rows(&[zone("roon:a", "A", false)], &["roon:gone".to_string()]);
        let gone = rows.iter().find(|r| r.zone_id == "roon:gone").unwrap();
        assert!(gone.unavailable && gone.hidden_here);
    }

    #[test]
    fn toggling_adds_then_removes_without_disturbing_the_rest() {
        let start = vec!["roon:a".to_string(), "lms:b".to_string()];
        let added = toggled(&start, "roon:c");
        assert_eq!(added, ["roon:a", "lms:b", "roon:c"]);
        assert_eq!(toggled(&added, "roon:a"), ["lms:b", "roon:c"]);
    }
}

#[cfg(test)]
mod shortcut_ui_tests {
    use super::{binding_chip, event_label, gestures_for_key, key_label, plays_on_note};
    use crate::app::api::PlaysOnInfo;

    #[test]
    fn buttons_read_as_a_person_would_say_them() {
        assert_eq!(event_label("home:long"), "Home, long press");
        assert_eq!(event_label("play_pause:long"), "Play/Pause, long press");
        assert_eq!(
            event_label("fast_forward:double"),
            "Fast-forward, double press"
        );
        // A key this UI has no name for is still recognisable and mappable.
        assert_eq!(event_label("usage_0192:press"), "Key 0192, press");
        assert_eq!(key_label("preset_1"), "Preset 1");
        assert_eq!(key_label(""), "");
    }

    #[test]
    fn the_form_offers_only_the_gestures_the_bridge_accepts() {
        assert!(gestures_for_key("volume_up").is_empty());
        assert!(gestures_for_key("mute").is_empty());
        assert_eq!(gestures_for_key("next"), ["long"]);
        assert_eq!(gestures_for_key("play_pause"), ["long"]);
        assert_eq!(gestures_for_key("home"), ["press", "long", "double"]);
        assert_eq!(gestures_for_key("usage_0192"), ["press", "long", "double"]);
    }

    #[test]
    fn every_failing_state_says_so_in_words() {
        for status in [
            "last_failed",
            "no_zone",
            "zone_missing",
            "provider_mismatch",
        ] {
            let (class, text) = binding_chip(status);
            assert!(class.contains("status-err"), "{status}");
            assert!(!text.is_empty());
        }
        assert_eq!(binding_chip("ok").1, "Worked last time");
        assert_eq!(binding_chip("untested").1, "Not tried yet");
        // A status from a newer bridge is shown as unknown, never as working.
        assert_eq!(binding_chip("from_the_future").1, "Unknown");
        assert!(!binding_chip("from_the_future").0.contains("success"));
    }

    fn plays_on(state: &str) -> PlaysOnInfo {
        PlaysOnInfo {
            zone_id: Some("roon:abc".into()),
            zone_name: Some("Office".into()),
            state: state.into(),
            ..PlaysOnInfo::default()
        }
    }

    #[test]
    fn an_unusable_zone_is_explained_and_never_hints_at_a_fallback() {
        assert!(plays_on_note(&plays_on("none"))
            .unwrap()
            .contains("no zone yet"));
        let missing = plays_on_note(&plays_on("missing")).unwrap();
        assert!(
            missing.contains("roon:abc") && missing.contains("instead of playing somewhere else")
        );
        assert!(plays_on_note(&plays_on("ok")).is_none());
    }

    #[test]
    fn a_hidden_zone_is_noted_as_still_playing() {
        let mut here = plays_on("ok");
        here.hidden_here = true;
        assert!(plays_on_note(&here).unwrap().contains("still plays"));
        let mut everywhere = plays_on("ok");
        everywhere.hidden_globally = true;
        assert!(plays_on_note(&everywhere).unwrap().contains("still plays"));
    }
}
