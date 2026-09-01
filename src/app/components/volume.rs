//! Volume control component with support for different volume types.
//!
//! Handles:
//! - "db": dB scale with suffix
//! - "number": 0-100 numeric scale
//! - "incremental": blind control (+/- only, no value)
//! - Fixed volume: hides controls entirely

use dioxus::prelude::*;

/// Volume type enum for cleaner pattern matching
#[derive(Clone, Copy, PartialEq)]
pub enum VolumeType {
    /// dB scale (typically negative values, 0 is max)
    Db,
    /// Numeric scale (typically 0-100)
    Number,
    /// Incremental/blind control (no absolute value)
    Incremental,
    /// Fixed volume (no control available)
    Fixed,
}

impl VolumeType {
    /// Parse volume type from API response
    pub fn from_api(volume: Option<f32>, volume_type: Option<&str>) -> Self {
        match (volume, volume_type) {
            // Explicit "fixed" type from API
            (_, Some("fixed")) => VolumeType::Fixed,
            // Legacy: no volume means fixed
            (None, _) => VolumeType::Fixed,
            (Some(_), Some("db")) => VolumeType::Db,
            (Some(_), Some("incremental")) => VolumeType::Incremental,
            (Some(_), _) => VolumeType::Number, // Default to number for "number" or unknown
        }
    }
}

/// Format volume for display, showing decimals only when step is fractional
fn format_volume(volume: f32, step: Option<f32>, suffix: &str) -> String {
    // Show decimals if step is fractional (not a whole number)
    let show_decimal = step.map(|s| s.fract() != 0.0).unwrap_or(false);
    if show_decimal {
        format!("{:.1}{}", volume, suffix)
    } else {
        format!("{}{}", volume.round() as i32, suffix)
    }
}

/// Compact volume controls for zone cards
#[component]
pub fn VolumeControlsCompact(
    volume: Option<f32>,
    volume_type: Option<String>,
    volume_step: Option<f32>,
    on_vol_down: EventHandler<()>,
    on_vol_up: EventHandler<()>,
) -> Element {
    let vol_type = VolumeType::from_api(volume, volume_type.as_deref());

    // Don't render anything for fixed volume
    if vol_type == VolumeType::Fixed {
        return rsx! {};
    }

    let volume_display = match vol_type {
        VolumeType::Db => volume
            .map(|v| format_volume(v, volume_step, " dB"))
            .unwrap_or_default(),
        VolumeType::Number => volume
            .map(|v| format_volume(v, volume_step, ""))
            .unwrap_or_default(),
        VolumeType::Incremental | VolumeType::Fixed => String::new(),
    };

    rsx! {
        div { class: "ml-auto flex items-center gap-1",
            button {
                class: "btn btn-outline btn-sm",
                onclick: move |_| on_vol_down.call(()),
                "−"
            }
            if vol_type != VolumeType::Incremental {
                span { class: "min-w-[3.5rem] text-center text-sm",
                    "{volume_display}"
                }
            }
            button {
                class: "btn btn-outline btn-sm",
                onclick: move |_| on_vol_up.call(()),
                "+"
            }
        }
    }
}

/// Full volume controls with label for zone detail view
#[component]
pub fn VolumeControlsFull(
    volume: Option<f32>,
    volume_type: Option<String>,
    volume_step: Option<f32>,
    on_vol_down: EventHandler<()>,
    on_vol_up: EventHandler<()>,
) -> Element {
    let vol_type = VolumeType::from_api(volume, volume_type.as_deref());

    // Don't render anything for fixed volume
    if vol_type == VolumeType::Fixed {
        return rsx! {};
    }

    let volume_display = match vol_type {
        VolumeType::Db => volume
            .map(|v| format_volume(v, volume_step, " dB"))
            .unwrap_or_default(),
        VolumeType::Number => volume
            .map(|v| format_volume(v, volume_step, ""))
            .unwrap_or_default(),
        VolumeType::Incremental | VolumeType::Fixed => String::new(),
    };

    rsx! {
        if vol_type != VolumeType::Incremental {
            span { style: "margin-left:1rem;", "Volume: ", strong { "{volume_display}" } }
        } else {
            span { style: "margin-left:1rem;", "Volume:" }
        }
        button {
            style: "width:2.5rem;",
            onclick: move |_| on_vol_down.call(()),
            "−"
        }
        button {
            style: "width:2.5rem;",
            onclick: move |_| on_vol_up.call(()),
            "+"
        }
    }
}
