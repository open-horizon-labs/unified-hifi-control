//! Playback devices page (#774, **alpha**): receivers and amplifiers, the zones that feed them,
//! and the opt-in, local-only device-mismatch report.
//!
//! The page talks only to the frozen `/devices` HTTP surface. Nothing it does sends data off the
//! server: a report is previewed here, and the user submits it on GitHub themselves.
//!
//! Two promises the copy here has to keep. Linking a zone hands its volume to the device (knob,
//! Watch, Home Assistant and MCP all follow), and a user ceiling is optional and can only lower
//! the device's own maximum. A link may record the input a zone arrives on, but UHC only shows
//! when the device is elsewhere: it never switches inputs by itself.
//!
//! Hydration note: every conditional that depends on fetched data sits behind a `use_resource`
//! that starts as `None` on both SSR and the first client render, so both render the same
//! "Loading" topology before data arrives. The report panel mounts only after a click.

use dioxus::prelude::*;

use crate::app::api::{
    self, DeviceCommandResult, DeviceInfo, DeviceInventory, DeviceLinkInfo, LinkStoreInfo,
    MismatchPreview, ReportSectionInfo, Zone, ZonesResponse,
};
use crate::app::base_path;
use crate::app::components::Layout;
use crate::app::Route;
use crate::components::collapsible::{Collapsible, CollapsibleContent, CollapsibleTrigger};

/// The literal marker the server redactor writes in place of a removed value.
const REDACTED_MARK: &str = "**REDACTED**";

#[derive(Clone, serde::Serialize)]
struct RecordingRequest {
    enabled: bool,
}

#[derive(Clone, serde::Serialize)]
struct ClearRequest {
    device_id: String,
}

/// Devices page component.
#[component]
pub fn Devices() -> Element {
    let mut inventory = use_resource(|| async {
        api::fetch_json::<DeviceInventory>("/devices")
            .await
            .map_err(|error| error.to_string())
    });
    // Zones are fetched once per page and handed to every card's link form.
    let mut zones = use_resource(|| async {
        api::fetch_json::<ZonesResponse>("/zones")
            .await
            .map(|response| response.zones)
            .map_err(|error| error.to_string())
    });

    // The device whose report preview is open: (device_id, display name).
    let mut reporting = use_signal(|| None::<(String, String)>);
    let mut toggle_busy = use_signal(|| false);
    let mut toggle_error = use_signal(|| None::<String>);

    let snapshot = inventory.read().clone();
    let loaded = snapshot.as_ref().and_then(|result| result.as_ref().ok());
    let load_error = snapshot
        .as_ref()
        .and_then(|result| result.as_ref().err().cloned());
    let recording = loaded.is_some_and(|inv| inv.mismatch_recording);
    let recording_aria = if recording { "true" } else { "false" };
    let zone_list: Vec<Zone> = zones
        .read()
        .as_ref()
        .and_then(|result| result.as_ref().ok().cloned())
        .unwrap_or_default();

    let set_recording = move |_| {
        if toggle_busy() {
            return;
        }
        let enabled = !recording;
        toggle_busy.set(true);
        toggle_error.set(None);
        spawn(async move {
            let result = api::post_json::<_, serde_json::Value>(
                "/devices/recording",
                &RecordingRequest { enabled },
            )
            .await;
            match result {
                Ok(_) => {
                    // Opting out clears recordings server-side, so a preview of them is stale.
                    if !enabled {
                        reporting.set(None);
                    }
                    inventory.restart();
                }
                Err(error) => toggle_error.set(Some(error)),
            }
            toggle_busy.set(false);
        });
    };

    rsx! {
        Layout {
            title: "Devices".to_string(),
            nav_active: "devices".to_string(),

            div { class: "flex items-center gap-3 mb-2",
                h1 { class: "text-2xl font-bold", "Receivers & amplifiers" }
                span { class: "badge badge-secondary", "Alpha" }
            }
            p { class: "text-muted text-sm mb-6",
                "Link a zone to the amplifier it plays through; the zone's volume then drives the amplifier. "
                "Alpha: support varies by model, and mismatch reports help us fix that."
            }

            // Mismatch recording opt-in
            section { id: "device-recording", class: "mb-6",
                div { class: "card p-4",
                    div { class: "flex items-start gap-4",
                        button {
                            r#type: "button",
                            class: "feature-toggle shrink-0 mt-0.5",
                            role: "switch",
                            aria_label: "Record device mismatches (local only)",
                            aria_checked: recording_aria,
                            disabled: toggle_busy(),
                            onclick: set_recording,
                            span { class: "feature-toggle__knob", aria_hidden: "true" }
                            span { class: "sr-only", "Record device mismatches (local only)" }
                        }
                        div {
                            p { class: "font-medium", "Record device mismatches (local only)" }
                            p { class: "text-sm text-muted",
                                "Off by default. When on, cases where a device's behavior does not match what UHC expects are kept in memory on this server. "
                                "Nothing is ever sent anywhere unless you open a report and submit it on GitHub yourself. Turning this off clears what was recorded."
                            }
                            if let Some(ref error) = toggle_error() {
                                p { class: "status-err text-sm mt-1", role: "alert", "{error}" }
                            }
                        }
                    }
                }
            }

            // Device list
            section { id: "device-list", class: "mb-8",
                div { class: "flex items-center justify-between mb-3",
                    h2 { class: "text-xl font-semibold", "Devices" }
                    button {
                        r#type: "button",
                        class: "btn btn-outline btn-sm",
                        onclick: move |_| {
                            inventory.restart();
                            zones.restart();
                        },
                        "Refresh"
                    }
                }
                if let Some(ref error) = load_error {
                    div { class: "card p-6",
                        p { class: "status-err", role: "alert", "Could not load devices: {error}" }
                    }
                } else if let Some(inv) = loaded {
                    if inv.devices.is_empty() {
                        div { class: "card p-6",
                            p { class: "font-medium mb-1", "No devices found" }
                            p { class: "text-muted text-sm mb-2",
                                "Receiver and amplifier control is off by default. Turn on "
                                em { "Enable receiver and amplifier control" }
                                " in "
                                Link { class: "link", to: Route::Settings {}, "Settings" }
                                ", then give discovery a minute to find your hardware."
                            }
                            p { class: "text-muted text-sm",
                                "Discovery listens for Denon/Marantz, Sonos, Yamaha, Arcam, Hegel, Lyngdorf and Eversolo over SSDP, "
                                "WiiM, BluOS and Cambridge over mDNS, and Onkyo/Integra/Pioneer over eISCP; "
                                "Home Assistant media players appear when its API is configured. "
                                "Anthem and Trinnov need their address in "
                                code { class: "font-mono text-xs", "manual_devices" }
                                " in device-links.json. Use Refresh above to look again."
                            }
                        }
                    } else {
                        if !inv.mismatch_recording {
                            p { class: "text-sm text-muted mb-3",
                                "Mismatch recording is off, so nothing is being captured. Turn it on above to collect evidence you can report."
                            }
                        }
                        div { class: "space-y-4",
                            for device in inv.devices.iter() {
                                DeviceCard {
                                    key: "{device.device_id}",
                                    device: device.clone(),
                                    links: inv
                                        .links
                                        .iter()
                                        .filter(|link| link.device_id == device.device_id)
                                        .cloned()
                                        .collect::<Vec<_>>(),
                                    linked_zone_ids: inv.links.iter().map(|link| link.zone_id.clone()).collect::<Vec<_>>(),
                                    zones: zone_list.clone(),
                                    link_store: inv.link_store.clone(),
                                    mismatch_count: inv.mismatch_counts.get(&device.device_id).copied().unwrap_or(0),
                                    on_report: move |(id, name): (String, String)| reporting.set(Some((id, name))),
                                    on_changed: move |_| {
                                        inventory.restart();
                                        zones.restart();
                                    },
                                }
                            }
                        }
                    }
                } else {
                    div { class: "card p-6", aria_busy: "true", "Loading devices..." }
                }
            }

            if let Some((device_id, device_name)) = reporting() {
                ReportPanel {
                    key: "{device_id}",
                    device_id: device_id.clone(),
                    device_name: device_name.clone(),
                    on_close: move |_| reporting.set(None),
                    on_cleared: move |_| {
                        reporting.set(None);
                        inventory.restart();
                    },
                }
            }
        }
    }
}

/// One-line outcome of a device command, from what the server read back afterwards.
fn describe_result(result: &DeviceCommandResult) -> String {
    let volume = &result.observed.volume;
    if result.confirmation == "confirmed_different" {
        let now = volume
            .value
            .map(|value| volume.format(value))
            .unwrap_or_else(|| "—".to_string());
        format!("The device applied its own limit: now {now}.")
    } else if result.clamped {
        let sent = result
            .sent_volume
            .map(|value| volume.format(value))
            .unwrap_or_else(|| "—".to_string());
        format!("Lowered to the maximum ({sent}).")
    } else {
        "Confirmed.".to_string()
    }
}

/// Why a typed ceiling cannot be sent, or the value to send. A limit above the device's own
/// maximum would change nothing, so it is refused here rather than saved as a no-op.
fn validate_ceiling(text: &str, device: &DeviceInfo) -> Result<f32, String> {
    let volume = &device.volume;
    let value: f32 = text
        .trim()
        .parse()
        .ok()
        .filter(|value: &f32| value.is_finite())
        .ok_or_else(|| "Enter a number, or use Clear limit to remove the limit.".to_string())?;
    if let Some(max) = volume.hardware_max {
        if value > max {
            return Err(format!(
                "{} is above the device's own maximum ({}), so it would change nothing. Enter a lower value.",
                volume.format(value),
                volume.format(max)
            ));
        }
    }
    if value < volume.min {
        return Err(format!(
            "{} is below the device's minimum ({}).",
            volume.format(value),
            volume.format(volume.min)
        ));
    }
    Ok(value)
}

/// One device: header, live controls, optional max volume, and the zones it feeds.
/// "Report mismatch" is always enabled, deliberately: the detector only catches
/// what it knows to look for, and a user who sees a wrong value (volume off, input mislabelled)
/// can still send the device facts. The preview says when nothing has been recorded.
#[component]
fn DeviceCard(
    device: DeviceInfo,
    links: Vec<DeviceLinkInfo>,
    linked_zone_ids: Vec<String>,
    zones: Vec<Zone>,
    link_store: LinkStoreInfo,
    mismatch_count: usize,
    on_report: EventHandler<(String, String)>,
    on_changed: EventHandler<()>,
) -> Element {
    let display_name = if device.name.is_empty() {
        device.device_id.clone()
    } else {
        device.name.clone()
    };
    let (power_class, power_text) = match device.power.as_str() {
        "on" => ("badge badge-success", "On"),
        "standby" => ("badge badge-secondary", "Standby"),
        _ => ("badge badge-secondary", "Unknown"),
    };
    let connection = match device.connection.control_channel.as_str() {
        "push" => "push updates",
        "polling" => "polling",
        _ => "no control channel",
    };
    let id = device.device_id.clone();
    let name_for_report = display_name.clone();

    // Controls. `busy` disables the whole row while a command is in flight.
    let mut busy = use_signal(|| false);
    let mut message = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);
    let command_id = device.device_id.clone();
    let run = use_callback(
        move |(action, value): (String, Option<serde_json::Value>)| {
            if busy() {
                return;
            }
            busy.set(true);
            message.set(None);
            error.set(None);
            let id = command_id.clone();
            spawn(async move {
                match api::control_device(&id, &action, value).await {
                    Ok(result) => message.set(Some(describe_result(&result))),
                    Err(failure) => error.set(Some(failure)),
                }
                // Re-read either way: the select must show what the device is actually on.
                on_changed.call(());
                busy.set(false);
            });
        },
    );

    let caps = &device.capabilities;
    let power_on = device.power == "on";
    let power_label = if power_on { "Standby" } else { "Turn on" };
    let power_action = if power_on { "power_off" } else { "power_on" };
    let power_title = (!caps.power.supported).then(|| caps.power.detail.clone());
    let input_blocked = !caps.input_select.supported || device.inputs.is_empty();
    let input_title = input_blocked.then(|| caps.input_select.detail.clone());
    let volume_title = (!caps.volume.supported).then(|| caps.volume.detail.clone());
    let muted = device.volume.is_muted == Some(true);
    let mute_title = (!caps.mute.supported).then(|| caps.mute.detail.clone());
    let mute_label = if muted { "Unmute" } else { "Mute" };
    let mute_action = if muted { "unmute" } else { "mute" };
    let volume_text = device.volume_label().unwrap_or_else(|| "—".to_string());
    let has_current_input = device.input.is_some();
    let card_id = format!("device-{}", device.device_id);

    rsx! {
        div { id: "{card_id}", class: "card p-4",
            // Header
            div { class: "flex flex-wrap items-start justify-between gap-3 mb-3",
                div {
                    div { class: "flex flex-wrap items-center gap-2",
                        h3 { class: "font-semibold text-lg", "{display_name}" }
                        span { class: "{power_class}", "{power_text}" }
                        span { class: "text-xs text-muted", "{connection}" }
                    }
                    div { class: "text-xs text-muted", "{device.vendor_model()} · {device.host}" }
                }
                div { class: "flex items-center gap-2",
                    if mismatch_count > 0 {
                        span { class: "badge badge-primary", "{mismatch_count} mismatches" }
                    }
                    button {
                        r#type: "button",
                        class: "btn btn-outline btn-sm",
                        onclick: move |_| on_report.call((id.clone(), name_for_report.clone())),
                        "Report mismatch"
                    }
                }
            }

            // Controls
            div { class: "flex flex-wrap items-center gap-3 mb-1",
                button {
                    r#type: "button",
                    class: "btn btn-outline btn-sm",
                    disabled: busy() || !caps.power.supported,
                    title: power_title,
                    onclick: move |_| run.call((power_action.to_string(), None)),
                    "{power_label}"
                }
                select {
                    class: "input w-auto min-w-48",
                    aria_label: "Input",
                    disabled: busy() || input_blocked,
                    title: input_title,
                    onchange: move |event| {
                        let input = event.value();
                        if !input.is_empty() {
                            run.call(("select_input".to_string(), Some(serde_json::Value::String(input))));
                        }
                    },
                    if !has_current_input {
                        option { value: "", disabled: true, selected: true, "Input unknown" }
                    }
                    for input in device.inputs.iter() {
                        option {
                            key: "{input.id}",
                            value: "{input.id}",
                            selected: device.input.as_deref() == Some(input.id.as_str()),
                            if input.available {
                                "{input.label()}"
                            } else {
                                "{input.label()} (nothing connected)"
                            }
                        }
                    }
                }
                div { class: "flex items-center gap-2",
                    button {
                        r#type: "button",
                        class: "btn btn-outline btn-sm",
                        aria_label: "Volume down",
                        disabled: busy() || !caps.volume.supported,
                        title: volume_title.clone(),
                        onclick: move |_| run.call(("volume_down".to_string(), None)),
                        "−"
                    }
                    span { class: "text-sm min-w-16 text-center", "{volume_text}" }
                    button {
                        r#type: "button",
                        class: "btn btn-outline btn-sm",
                        aria_label: "Volume up",
                        disabled: busy() || !caps.volume.supported,
                        title: volume_title,
                        onclick: move |_| run.call(("volume_up".to_string(), None)),
                        "+"
                    }
                    button {
                        r#type: "button",
                        class: "btn btn-ghost btn-sm",
                        disabled: busy() || !caps.mute.supported,
                        title: mute_title,
                        onclick: move |_| run.call((mute_action.to_string(), None)),
                        "{mute_label}"
                    }
                }
            }
            if let Some(ref text) = message() {
                p { class: "text-sm text-muted mt-1", role: "status", "{text}" }
            }
            if let Some(ref text) = error() {
                p { class: "status-err text-sm mt-1", role: "alert", "{text}" }
            }

            if caps.volume.supported {
                CeilingSection {
                    device: device.clone(),
                    on_changed: move |_| on_changed.call(()),
                }
            }

            ZoneLinks {
                device: device.clone(),
                links,
                linked_zone_ids,
                zones,
                link_store,
                on_changed: move |_| on_changed.call(()),
            }
        }
    }
}

/// The zone's provider as the user knows it, for the takeover line.
fn takeover_source(zone: &Zone) -> String {
    zone.source
        .as_deref()
        .map(|source| api::source_label(source).to_string())
        .unwrap_or_else(|| "its music provider".to_string())
}

/// The optional user maximum. It only ever lowers the device's own maximum.
#[component]
fn CeilingSection(device: DeviceInfo, on_changed: EventHandler<()>) -> Element {
    let volume = device.volume.clone();
    let mut text = use_signal(|| {
        volume
            .ceiling
            .map(|value| value.to_string())
            .unwrap_or_default()
    });
    let mut busy = use_signal(|| false);
    let mut message = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);

    let hardware = volume
        .hardware_max
        .map(|value| volume.format(value))
        .unwrap_or_else(|| "not reported".to_string());
    let limit = volume
        .ceiling
        .map(|value| volume.format(value))
        .unwrap_or_else(|| "none".to_string());
    let effective = volume
        .effective_max()
        .map(|value| volume.format(value))
        .unwrap_or_else(|| "the device's own range".to_string());
    let trigger = match (volume.hardware_max, volume.ceiling) {
        (Some(max), Some(ceiling)) => format!(
            "Max volume · limit {} (device max {})",
            volume.format(ceiling),
            volume.format(max)
        ),
        (Some(max), None) => format!("Max volume · device max {}", volume.format(max)),
        _ => "Max volume".to_string(),
    };
    let step = if volume.step > 0.0 { volume.step } else { 0.5 };
    let unit = volume.unit();

    let save = use_callback({
        let device = device.clone();
        move |clear: bool| {
            if busy() {
                return;
            }
            message.set(None);
            error.set(None);
            let ceiling = if clear {
                None
            } else {
                match validate_ceiling(&text(), &device) {
                    Ok(value) => Some(value),
                    Err(reason) => {
                        error.set(Some(reason));
                        return;
                    }
                }
            };
            busy.set(true);
            let id = device.device_id.clone();
            spawn(async move {
                match api::set_device_ceiling(&id, ceiling).await {
                    Ok(()) => {
                        if ceiling.is_none() {
                            text.set(String::new());
                            message.set(Some("Limit cleared.".to_string()));
                        } else {
                            message.set(Some("Limit saved.".to_string()));
                        }
                        on_changed.call(());
                    }
                    Err(failure) => error.set(Some(failure)),
                }
                busy.set(false);
            });
        }
    });

    rsx! {
        div { class: "mt-3",
            Collapsible { default_open: false,
                CollapsibleTrigger { "{trigger}" }
                CollapsibleContent {
                    div { class: "pt-2",
                        p { class: "text-sm",
                            "Device maximum: {hardware} · Your limit: {limit} → levels are clamped to {effective}"
                        }
                        div { class: "flex flex-wrap items-center gap-2 mt-2",
                            input {
                                class: "input w-28",
                                r#type: "number",
                                step: "{step}",
                                aria_label: "Volume limit",
                                value: "{text}",
                                disabled: busy(),
                                oninput: move |event| text.set(event.value()),
                            }
                            if !unit.is_empty() {
                                span { class: "text-sm text-muted", "{unit.trim()}" }
                            }
                            button {
                                r#type: "button",
                                class: "btn btn-outline btn-sm",
                                disabled: busy(),
                                onclick: move |_| save.call(false),
                                "Set limit"
                            }
                            button {
                                r#type: "button",
                                class: "btn btn-ghost btn-sm",
                                disabled: busy() || volume.ceiling.is_none(),
                                onclick: move |_| save.call(true),
                                "Clear limit"
                            }
                        }
                        p { class: "text-xs text-muted mt-1",
                            "Optional. A limit only lowers the device's own maximum; volume works without one."
                        }
                        if let Some(ref text) = message() {
                            p { class: "text-sm text-muted mt-1", role: "status", "{text}" }
                        }
                        if let Some(ref text) = error() {
                            p { class: "status-err text-sm mt-1", role: "alert", "{text}" }
                        }
                    }
                }
            }
        }
    }
}

/// The zones this device feeds, and the form that links another. Always visible: this is the
/// onboarding.
#[component]
fn ZoneLinks(
    device: DeviceInfo,
    links: Vec<DeviceLinkInfo>,
    linked_zone_ids: Vec<String>,
    zones: Vec<Zone>,
    link_store: LinkStoreInfo,
    on_changed: EventHandler<()>,
) -> Element {
    let mut chosen_zone = use_signal(String::new);
    let mut chosen_input = use_signal(String::new);
    let mut form_open = use_signal(|| false);
    let mut busy = use_signal(|| false);
    let mut message = use_signal(|| None::<String>);
    let mut error = use_signal(|| None::<String>);

    let device_name = if device.name.is_empty() {
        device.device_id.clone()
    } else {
        device.name.clone()
    };
    let writable = link_store.writable;
    let store_detail = link_store
        .detail
        .clone()
        .unwrap_or_else(|| "Links cannot be saved right now.".to_string());

    // Only zones no device feeds yet: a zone has one volume owner.
    let candidates: Vec<Zone> = zones
        .iter()
        .filter(|zone| zone.device.is_none() && !linked_zone_ids.contains(&zone.zone_id))
        .cloned()
        .collect();
    let picked = candidates
        .iter()
        .find(|zone| zone.zone_id == chosen_zone())
        .cloned();

    let remove_device_name = device_name.clone();
    let remove = use_callback(move |(zone_id, label): (String, String)| {
        if busy() {
            return;
        }
        busy.set(true);
        message.set(None);
        error.set(None);
        let device_name = remove_device_name.clone();
        spawn(async move {
            match api::unlink_device(&zone_id).await {
                Ok(()) => {
                    message.set(Some(format!(
                        "Link removed. {label} is back on its own volume; nothing on {device_name} changed."
                    )));
                    on_changed.call(());
                }
                Err(failure) => error.set(Some(failure)),
            }
            busy.set(false);
        });
    });

    let link_id = device.device_id.clone();
    let link_name = device_name.clone();
    let link_candidates = candidates.clone();
    let link = move |_| {
        if busy() {
            return;
        }
        let zone_id = chosen_zone();
        let Some(zone) = link_candidates.iter().find(|zone| zone.zone_id == zone_id) else {
            error.set(Some("Choose a zone to link.".to_string()));
            return;
        };
        let zone_name = zone.zone_name.clone();
        let source = chosen_input();
        busy.set(true);
        message.set(None);
        error.set(None);
        let device_id = link_id.clone();
        let device_name = link_name.clone();
        spawn(async move {
            let source_id = (!source.is_empty()).then_some(source.as_str());
            match api::link_device(&zone_id, &device_id, source_id).await {
                Ok(()) => {
                    message.set(Some(format!(
                        "Linked. {zone_name}'s volume now controls {device_name}."
                    )));
                    chosen_zone.set(String::new());
                    chosen_input.set(String::new());
                    form_open.set(false);
                    on_changed.call(());
                }
                Err(failure) => error.set(Some(failure)),
            }
            busy.set(false);
        });
    };

    rsx! {
        div { class: "mt-4 pt-3 border-t border-default",
            h4 { class: "font-medium mb-2", "Zones playing through this device" }
            if !links.is_empty() {
                ul { class: "mb-2 space-y-1",
                    for link in links.iter() {
                        {
                            let zone_label = link.zone_name.clone().unwrap_or_else(|| link.zone_id.clone());
                            let arrives_on = link.source_id.as_deref().map(|source| {
                                device
                                    .inputs
                                    .iter()
                                    .find(|input| input.id == source)
                                    .map(|input| input.label().to_string())
                                    .unwrap_or_else(|| source.to_string())
                            });
                            let zone_id = link.zone_id.clone();
                            let label_for_remove = zone_label.clone();
                            rsx! {
                                li { key: "{link.zone_id}", class: "flex flex-wrap items-center gap-2 text-sm",
                                    span { class: "font-medium", "{zone_label}" }
                                    if let Some(ref input) = arrives_on {
                                        span { class: "text-muted", "arrives on {input}" }
                                    }
                                    if !link.zone_present {
                                        span { class: "text-muted", "(zone offline)" }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "btn btn-ghost btn-sm",
                                        disabled: busy() || !writable,
                                        onclick: move |_| remove.call((zone_id.clone(), label_for_remove.clone())),
                                        "Remove"
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Link form: folded until asked for, so the list above stays the page's focus.
            if !form_open() {
                div { class: "flex flex-wrap items-center gap-2",
                    if links.is_empty() {
                        p { class: "text-sm text-muted",
                            "No zones yet. Link a zone so its volume controls this device."
                        }
                    }
                    button {
                        r#type: "button",
                        class: "btn btn-outline btn-sm",
                        onclick: move |_| form_open.set(true),
                        "Link a zone"
                    }
                }
            } else {
                div { class: "space-y-2",
                    if !writable {
                        p { class: "status-err text-sm", role: "alert", "{store_detail}" }
                    }
                    div { class: "grid gap-3 sm:grid-cols-2",
                        label { class: "block text-sm",
                            "Zone"
                            select {
                                class: "input w-full",
                                disabled: busy() || !writable,
                                onchange: move |event| chosen_zone.set(event.value()),
                                option { value: "", selected: chosen_zone().is_empty(), "Choose a zone" }
                                for zone in candidates.iter() {
                                    option {
                                        key: "{zone.zone_id}",
                                        value: "{zone.zone_id}",
                                        selected: chosen_zone() == zone.zone_id,
                                        if let Some(ref source) = zone.source {
                                            "{zone.zone_name} ({api::source_label(source)})"
                                        } else {
                                            "{zone.zone_name}"
                                        }
                                    }
                                }
                            }
                        }
                        label { class: "block text-sm",
                            "Arrives on input (optional)"
                            select {
                                class: "input w-full",
                                disabled: busy() || !writable,
                                onchange: move |event| chosen_input.set(event.value()),
                                option { value: "", selected: chosen_input().is_empty(), "Not specified" }
                                for input in device.inputs.iter() {
                                    option {
                                        key: "{input.id}",
                                        value: "{input.id}",
                                        selected: chosen_input() == input.id,
                                        "{input.label()}"
                                    }
                                }
                            }
                            span { class: "block text-xs text-muted mt-1",
                                "The input this zone arrives on. UHC records it and shows when the device is on another input. It never switches inputs for you."
                            }
                        }
                    }
                    if let Some(zone) = picked {
                        p { class: "text-sm",
                            "After linking, volume on {zone.zone_name} controls {device_name} (knob, Watch, Home Assistant, MCP) instead of {takeover_source(&zone)}."
                        }
                    }
                    div { class: "flex items-center gap-2",
                        button {
                            r#type: "button",
                            class: "btn btn-primary btn-sm",
                            disabled: busy() || !writable || chosen_zone().is_empty(),
                            onclick: link,
                            "Link zone"
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-ghost btn-sm",
                            onclick: move |_| form_open.set(false),
                            "Cancel"
                        }
                    }
                }
            }
            if let Some(ref text) = message() {
                p { class: "text-sm text-muted mt-2", role: "status", "{text}" }
            }
            if let Some(ref text) = error() {
                p { class: "status-err text-sm mt-2", role: "alert", "{text}" }
            }
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum ReportTab {
    Preview,
    Raw,
}

/// The structured report preview for one device.
#[component]
fn ReportPanel(
    device_id: String,
    device_name: String,
    on_close: EventHandler<()>,
    on_cleared: EventHandler<()>,
) -> Element {
    let mut preview = use_signal(|| None::<Result<MismatchPreview, String>>);
    // The section ids the user has ticked. `None` until the first preview supplies the
    // server's defaults (everything except raw excerpts).
    let mut selected = use_signal(|| None::<Vec<String>>);
    let mut tab = use_signal(|| ReportTab::Preview);
    let mut confirm_clear = use_signal(|| false);
    let mut status = use_signal(|| None::<String>);
    // Guards against an older refetch landing after a newer one.
    let mut request_seq = use_signal(|| 0u32);

    let initial_id = device_id.clone();
    let _initial = use_resource(move || {
        let id = initial_id.clone();
        async move {
            let result = api::fetch_device_report(&id, None).await;
            if let Ok(report) = &result {
                selected.set(Some(report.included_sections.clone()));
            }
            preview.set(Some(result));
        }
    });

    // Refetch with `?include=` so issue_url, markdown and omitted_from_link match the ticks.
    let refetch_id = device_id.clone();
    let toggle_section = use_callback(move |(section_id, on): (String, bool)| {
        let Some(mut next) = selected.peek().clone() else {
            return;
        };
        next.retain(|id| *id != section_id);
        if on {
            next.push(section_id);
        }
        selected.set(Some(next.clone()));
        let seq = *request_seq.peek() + 1;
        request_seq.set(seq);
        let id = refetch_id.clone();
        spawn(async move {
            let result = api::fetch_device_report(&id, Some(&next)).await;
            if *request_seq.peek() != seq {
                return;
            }
            match result {
                Ok(report) => {
                    status.set(None);
                    preview.set(Some(Ok(report)));
                }
                Err(error) => status.set(Some(format!("Could not refresh the preview: {error}"))),
            }
        });
    });

    let clear_id = device_id.clone();
    let clear = move |_| {
        if !confirm_clear() {
            confirm_clear.set(true);
            return;
        }
        let id = clear_id.clone();
        spawn(async move {
            let result = api::post_json::<_, serde_json::Value>(
                "/devices/report/clear",
                &ClearRequest { device_id: id },
            )
            .await;
            match result {
                Ok(_) => on_cleared.call(()),
                Err(error) => {
                    confirm_clear.set(false);
                    status.set(Some(format!("Could not clear recordings: {error}")));
                }
            }
        });
    };

    let preview_state = preview.read().clone();
    let ticked = selected().unwrap_or_default();
    let preview_tab_class = if tab() == ReportTab::Preview {
        "btn btn-primary btn-sm"
    } else {
        "btn btn-outline btn-sm"
    };
    let raw_tab_class = if tab() == ReportTab::Raw {
        "btn btn-primary btn-sm"
    } else {
        "btn btn-outline btn-sm"
    };
    let preview_selected = if tab() == ReportTab::Preview {
        "true"
    } else {
        "false"
    };
    let raw_selected = if tab() == ReportTab::Raw {
        "true"
    } else {
        "false"
    };
    let clear_label = if confirm_clear() {
        "Confirm: clear recordings"
    } else {
        "Clear recordings"
    };

    rsx! {
        section { id: "device-report", class: "mb-8",
            div { class: "flex items-center justify-between mb-3",
                h2 { class: "text-xl font-semibold", "Report mismatch: {device_name}" }
                button {
                    r#type: "button",
                    class: "btn btn-ghost btn-sm",
                    onclick: move |_| on_close.call(()),
                    "Close"
                }
            }
            match preview_state {
                None => rsx! {
                    div { class: "card p-6", aria_busy: "true", "Building report preview..." }
                },
                Some(Err(error)) => rsx! {
                    div { class: "card p-6",
                        p { class: "status-err", role: "alert", "Could not build the report: {error}" }
                    }
                },
                Some(Ok(report)) => rsx! {
                    p { class: "text-sm text-muted mb-3",
                        "Review what would be shared. Tick the sections to include; the GitHub link updates to match. "
                        "Names, addresses and identifiers are removed first."
                    }
                    if report.report.anomaly_count == 0 {
                        div { class: "card p-3 mb-3", role: "status",
                            p { class: "text-sm",
                                "No mismatches have been recorded for this device yet, so this report only carries its details. "
                                "Turn recording on and use the device, then come back, to capture evidence."
                            }
                        }
                    }

                    div { class: "mb-3",
                        p { class: "text-sm font-medium", "{report.report.title}" }
                        p { class: "text-xs text-muted",
                            "Evidence: {report.report.evidence}"
                        }
                    }

                    // Redactions: say what was removed, and where to see it.
                    div { class: "card p-3 mb-3",
                        if report.report.redactions.is_empty() {
                            p { class: "text-sm", "Nothing needed removing from this report." }
                        } else {
                            p { class: "text-sm",
                                span { class: "font-medium", "Removed before sharing: " }
                                for item in report.report.redactions.iter() {
                                    span { class: "badge badge-secondary mr-1", key: "{item}", "{item}" }
                                }
                            }
                            p { class: "text-xs text-muted mt-1",
                                "Each removed value is shown as "
                                span { class: "badge badge-primary", "{REDACTED_MARK}" }
                                " in the sections below."
                            }
                        }
                    }

                    // Preview / Raw tabs
                    div { class: "flex gap-2 mb-3", role: "tablist", aria_label: "Report view",
                        button {
                            r#type: "button",
                            role: "tab",
                            aria_selected: preview_selected,
                            class: "{preview_tab_class}",
                            onclick: move |_| tab.set(ReportTab::Preview),
                            "Preview"
                        }
                        button {
                            r#type: "button",
                            role: "tab",
                            aria_selected: raw_selected,
                            class: "{raw_tab_class}",
                            onclick: move |_| tab.set(ReportTab::Raw),
                            "Raw"
                        }
                    }

                    if tab() == ReportTab::Preview {
                        for section in report.report.sections.iter() {
                            SectionCard {
                                key: "{section.id}",
                                section: section.clone(),
                                checked: ticked.contains(&section.id),
                                on_toggle: toggle_section,
                            }
                        }
                    } else {
                        div { class: "mb-3",
                            p { class: "text-xs text-muted mb-1",
                                "Exactly the text that will be placed in the GitHub issue form (ticked sections only)."
                            }
                            pre {
                                class: "card p-3 font-mono text-xs overflow-auto max-h-96 whitespace-pre-wrap",
                                tabindex: "0",
                                aria_label: "Report markdown",
                                "{report.markdown}"
                            }
                        }
                    }

                    if !report.omitted_from_link.is_empty() {
                        div { class: "card p-3 mb-3", role: "status",
                            p { class: "text-sm",
                                span { class: "font-medium", "Too large for a link: " }
                                "{omitted_titles(&report.report.sections, &report.omitted_from_link)} "
                                "left out of the GitHub link. Download the bundle and attach it to the issue."
                            }
                        }
                    }

                    if let Some(ref message) = status() {
                        p { class: "status-err text-sm mb-3", role: "alert", "{message}" }
                    }

                    div { class: "flex flex-wrap items-center gap-2 mb-2",
                        a {
                            class: "btn btn-outline",
                            href: "{report.search_url}",
                            target: "_blank",
                            rel: "noopener noreferrer",
                            "Search existing reports"
                        }
                        a {
                            class: "btn btn-outline",
                            href: base_path::href(&report.bundle_url),
                            download: "",
                            "Download bundle"
                        }
                        a {
                            class: "btn btn-primary",
                            href: "{report.issue_url}",
                            target: "_blank",
                            rel: "noopener noreferrer",
                            "Open GitHub issue"
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-ghost ml-auto",
                            onclick: clear,
                            "{clear_label}"
                        }
                    }
                    p { class: "text-xs text-muted",
                        "Nothing is sent by UHC. Opening the issue shows GitHub's form pre-filled with the ticked sections, and nothing is submitted until you press Submit on GitHub. "
                        "Search first to see if someone already reported this device."
                    }
                },
            }
        }
    }
}

/// Titles for the section ids the link dropped, for the notice text.
fn omitted_titles(sections: &[ReportSectionInfo], omitted: &[String]) -> String {
    omitted
        .iter()
        .map(|id| {
            sections
                .iter()
                .find(|section| section.id == *id)
                .map(|section| section.title.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// One collapsible card per report section, with its include checkbox.
#[component]
fn SectionCard(
    section: ReportSectionInfo,
    checked: bool,
    on_toggle: EventHandler<(String, bool)>,
) -> Element {
    let monospace = matches!(section.id.as_str(), "device_state" | "excerpts");
    // Short, human-scale sections open by default; bulky dumps stay folded.
    let open_by_default = !monospace;
    // A folded section must say what it holds, or it reads as empty.
    let trigger_label = if open_by_default || section.lines.is_empty() {
        "Details".to_string()
    } else {
        format!("Show {} lines", section.lines.len())
    };
    let input_id = format!("report-section-{}", section.id);
    let section_id = section.id.clone();
    let body_class = if checked { "" } else { "opacity-60" };
    let state_label = if checked { "Included" } else { "Not included" };
    let empty_text = match section.id.as_str() {
        "anomalies" => "No mismatches recorded.",
        "excerpts" => "No protocol excerpts were captured.",
        _ => "Nothing to show.",
    };

    rsx! {
        div { class: "card p-4 mb-3",
            div { class: "flex items-center gap-3",
                input {
                    id: "{input_id}",
                    r#type: "checkbox",
                    class: "h-5 w-5 shrink-0",
                    checked: checked,
                    onchange: move |event| on_toggle.call((section_id.clone(), event.checked())),
                }
                label { r#for: "{input_id}", class: "font-medium flex-1", "{section.title}" }
                span { class: "badge badge-secondary", "{state_label}" }
            }
            if section.id == "excerpts" && !checked {
                p { class: "text-xs text-muted mt-1",
                    "Raw protocol exchanges are the most revealing part, so they are off unless you tick this."
                }
            }
            Collapsible { default_open: open_by_default,
                CollapsibleTrigger { "{trigger_label}" }
                CollapsibleContent {
                    div { class: "{body_class}",
                        if section.lines.is_empty() {
                            p { class: "text-sm text-muted", "{empty_text}" }
                        } else if monospace {
                            pre {
                                class: "font-mono text-xs overflow-auto max-h-64 p-3 rounded border border-default whitespace-pre",
                                tabindex: "0",
                                for (index, line) in section.lines.iter().enumerate() {
                                    div { key: "{index}", {marked_text(line)} }
                                }
                            }
                        } else {
                            ul { class: "list-disc pl-5 text-sm space-y-1",
                                for (index, line) in section.lines.iter().enumerate() {
                                    li { key: "{index}", {inline_markdown(line)} }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Render text with every redaction marker highlighted, so removed values are visible in place.
fn marked_text(text: &str) -> Element {
    let parts: Vec<&str> = text.split(REDACTED_MARK).collect();
    let last = parts.len().saturating_sub(1);
    rsx! {
        for (index, part) in parts.into_iter().enumerate() {
            span { key: "{index}", "{part}" }
            if index < last {
                span { class: "badge badge-primary", "{REDACTED_MARK}" }
            }
        }
    }
}

/// A single report list line: strip the markdown bullet and render `code` spans, keeping
/// redaction markers highlighted. The server emits only these two constructs in list sections.
fn inline_markdown(line: &str) -> Element {
    let text = line.trim_start().strip_prefix("- ").unwrap_or(line);
    let segments: Vec<&str> = text.split('`').collect();
    rsx! {
        for (index, segment) in segments.into_iter().enumerate() {
            if index % 2 == 1 {
                code { key: "{index}", class: "font-mono text-xs", {marked_text(segment)} }
            } else {
                span { key: "{index}", {marked_text(segment)} }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_titles_name_sections_and_fall_back_to_ids() {
        let sections = vec![ReportSectionInfo {
            id: "device_state".into(),
            title: "Device state".into(),
            lines: vec![],
            included_by_default: true,
        }];
        assert_eq!(
            omitted_titles(&sections, &["excerpts".into(), "device_state".into()]),
            "excerpts, Device state"
        );
    }

    #[test]
    fn device_labels_use_the_devices_own_scale_and_input_names() {
        let device: DeviceInfo = serde_json::from_value(serde_json::json!({
            "device_id": "marantz:0006781E0AB8",
            "input": "MPLAY",
            "inputs": [{"id": "MPLAY", "name": "Media Player", "available": true}],
            "volume": {"scale": "decibel", "value": -40.5, "is_muted": true},
        }))
        .unwrap();
        assert_eq!(device.input_label().as_deref(), Some("Media Player"));
        assert_eq!(device.volume_label().as_deref(), Some("-40.5 dB, muted"));
    }

    #[test]
    fn unread_volume_and_input_have_no_label() {
        let device: DeviceInfo =
            serde_json::from_value(serde_json::json!({"device_id": "wiim:ABC"})).unwrap();
        assert_eq!(device.input_label(), None);
        assert_eq!(device.volume_label(), None);
    }
}
