//! Reusable form input components.

use dioxus::prelude::*;

use crate::app::api::PowerModeConfig;

/// A labeled power mode timeout input with description.
#[component]
pub fn PowerModeInput(
    /// Input label
    label: &'static str,
    /// Description text shown below label
    description: &'static str,
    /// Current configuration
    config: PowerModeConfig,
    /// Called when the value changes
    on_change: EventHandler<PowerModeConfig>,
) -> Element {
    let timeout_sec = config.timeout_sec;

    rsx! {
        div { class: "flex items-center gap-4",
            div { class: "flex-1",
                label { class: "block text-sm font-medium", "{label}" }
                p { class: "text-xs text-muted", "{description}" }
            }
            div { class: "flex items-center gap-2",
                input {
                    class: "input w-20 text-center",
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
                span { class: "text-sm text-muted", "sec" }
            }
        }
    }
}

/// A labeled toggle switch with description.
#[component]
pub fn ToggleInput(
    /// Input label
    label: &'static str,
    /// Description text shown below label
    description: &'static str,
    /// Current checked state
    checked: bool,
    /// Called when the toggle changes
    on_change: EventHandler<bool>,
) -> Element {
    rsx! {
        div { class: "flex items-center gap-4",
            div { class: "flex-1",
                label { class: "block text-sm font-medium", "{label}" }
                p { class: "text-xs text-muted", "{description}" }
            }
            input {
                class: "toggle",
                r#type: "checkbox",
                checked: checked,
                onchange: move |e| on_change.call(e.checked()),
            }
        }
    }
}
