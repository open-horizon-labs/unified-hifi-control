//! Playback devices page (#774, **alpha**): receivers and amplifiers, plus the
//! opt-in, local-only device-mismatch report.
//!
//! The page talks only to the frozen `/devices` HTTP surface. Nothing it does sends data off the
//! server: a report is previewed here, and the user submits it on GitHub themselves.
//!
//! Hydration note: every conditional that depends on fetched data sits behind a `use_resource`
//! that starts as `None` on both SSR and the first client render, so both render the same
//! "Loading" topology before data arrives. The report panel mounts only after a click.

use dioxus::prelude::*;

use crate::app::api::{self, DeviceInfo, DeviceInventory, MismatchPreview, ReportSectionInfo};
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
                "The hardware your zones play into: power, volume and input as the device reports them. "
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
                        onclick: move |_| inventory.restart(),
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
                            p { class: "text-muted text-sm",
                                "Receiver and amplifier control is off by default. Turn on "
                                em { "Enable receiver and amplifier control" }
                                " in "
                                Link { class: "link", to: Route::Settings {}, "Settings" }
                                ", then give discovery a minute to find your hardware."
                            }
                        }
                    } else {
                        if !inv.mismatch_recording {
                            p { class: "text-sm text-muted mb-3",
                                "Mismatch recording is off, so nothing is being captured. Turn it on above to collect evidence you can report."
                            }
                        }
                        div { class: "card overflow-x-auto",
                            table { class: "w-full text-sm",
                                thead {
                                    tr { class: "border-b border-default text-left",
                                        th { class: "py-2 px-3 font-medium", "Name" }
                                        th { class: "py-2 px-3 font-medium", "Vendor / model" }
                                        th { class: "py-2 px-3 font-medium", "Power" }
                                        th { class: "py-2 px-3 font-medium", "Input" }
                                        th { class: "py-2 px-3 font-medium", "Volume" }
                                        th { class: "py-2 px-3 font-medium", "Linked zones" }
                                        th { class: "py-2 px-3 font-medium", "Mismatches" }
                                        th { class: "py-2 px-3 font-medium", span { class: "sr-only", "Actions" } }
                                    }
                                }
                                tbody {
                                    for device in inv.devices.iter() {
                                        DeviceRow {
                                            key: "{device.device_id}",
                                            device: device.clone(),
                                            zone_names: linked_zone_names(inv, device),
                                            mismatch_count: inv.mismatch_counts.get(&device.device_id).copied().unwrap_or(0),
                                            recording: inv.mismatch_recording,
                                            on_report: move |(id, name): (String, String)| reporting.set(Some((id, name))),
                                        }
                                    }
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

/// Zone names for a device's links, falling back to the zone id when the zone is not present.
fn linked_zone_names(inventory: &DeviceInventory, device: &DeviceInfo) -> Vec<String> {
    device
        .linked_zone_ids
        .iter()
        .map(|zone_id| {
            inventory
                .links
                .iter()
                .find(|link| link.zone_id == *zone_id && link.device_id == device.device_id)
                .and_then(|link| link.zone_name.clone())
                .unwrap_or_else(|| zone_id.clone())
        })
        .collect()
}

/// One table row. "Report mismatch" is always enabled, deliberately: the detector only catches
/// what it knows to look for, and a user who sees a wrong value (volume off, input mislabelled)
/// can still send the device facts. The preview says when nothing has been recorded.
#[component]
fn DeviceRow(
    device: DeviceInfo,
    zone_names: Vec<String>,
    mismatch_count: usize,
    recording: bool,
    on_report: EventHandler<(String, String)>,
) -> Element {
    let display_name = if device.name.is_empty() {
        device.device_id.clone()
    } else {
        device.name.clone()
    };
    // Model names often carry the vendor already ("WiiM Ultra"); don't print it twice.
    let model = if device
        .model
        .to_lowercase()
        .starts_with(&device.vendor.to_lowercase())
    {
        device.model.clone()
    } else {
        format!("{} {}", device.vendor, device.model)
    };
    let (power_class, power_text) = match device.power.as_str() {
        "on" => ("badge badge-success", "On"),
        "standby" => ("badge badge-secondary", "Standby"),
        _ => ("badge badge-secondary", "Unknown"),
    };
    let input = device.input_label();
    let volume = device.volume_label();
    let zones = zone_names.join(", ");
    let id = device.device_id.clone();
    let name_for_report = display_name.clone();

    rsx! {
        tr { class: "border-b border-default",
            td { class: "py-2 px-3",
                div { class: "font-medium", "{display_name}" }
                div { class: "text-xs text-muted", "{device.host}" }
            }
            td { class: "py-2 px-3", "{model}" }
            td { class: "py-2 px-3", span { class: "{power_class}", "{power_text}" } }
            td { class: "py-2 px-3",
                if let Some(ref input) = input {
                    "{input}"
                } else {
                    span { class: "text-muted", "-" }
                }
            }
            td { class: "py-2 px-3",
                if let Some(ref volume) = volume {
                    "{volume}"
                } else {
                    span { class: "text-muted", "-" }
                }
            }
            td { class: "py-2 px-3",
                if zones.is_empty() {
                    span { class: "text-muted", "none linked" }
                } else {
                    "{zones}"
                }
            }
            td { class: "py-2 px-3",
                if mismatch_count > 0 {
                    span { class: "badge badge-primary", "{mismatch_count}" }
                } else if recording {
                    span { class: "text-muted", "0" }
                } else {
                    span { class: "text-muted", "-" }
                }
            }
            td { class: "py-2 px-3 text-right",
                button {
                    r#type: "button",
                    class: "btn btn-outline btn-sm",
                    onclick: move |_| on_report.call((id.clone(), name_for_report.clone())),
                    "Report mismatch"
                }
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
