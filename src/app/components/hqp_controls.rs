//! HQPlayer control components for profile and matrix selection.

use dioxus::prelude::*;

use crate::app::api::{HqpMatrixProfile, HqpProfile};

/// HQPlayer profile selector dropdown.
#[component]
pub fn HqpProfileSelect(
    /// Available profiles to choose from
    profiles: Vec<HqpProfile>,
    /// Called when a profile is selected
    on_select: EventHandler<String>,
    /// Optional CSS class for the select element
    #[props(default = "input".to_string())]
    class: String,
    /// Disable the select element
    #[props(default = false)]
    disabled: bool,
) -> Element {
    rsx! {
        select {
            class: "{class}",
            disabled: disabled,
            onchange: move |evt| {
                let value = evt.value();
                if !value.is_empty() {
                    on_select.call(value);
                }
            },
            option { value: "", "Profile..." }
            for profile in profiles.iter() {
                {
                    let name = profile.name.as_deref().unwrap_or_default();
                    let title = profile.title.as_deref().unwrap_or(name);
                    rsx! {
                        option {
                            key: "{name}",
                            value: "{name}",
                            "{title}"
                        }
                    }
                }
            }
        }
    }
}

/// HQPlayer matrix profile selector dropdown.
#[component]
pub fn HqpMatrixSelect(
    /// Available matrix profiles to choose from
    profiles: Vec<HqpMatrixProfile>,
    /// Currently active profile index (if any)
    active: Option<u32>,
    /// Called when a matrix profile is selected (passes the profile index)
    on_select: EventHandler<u32>,
    /// Optional CSS class for the select element
    #[props(default = "input".to_string())]
    class: String,
    /// Disable the select element
    #[props(default = false)]
    disabled: bool,
) -> Element {
    rsx! {
        select {
            class: "{class}",
            disabled: disabled,
            onchange: move |evt| {
                if let Ok(idx) = evt.value().parse::<u32>() {
                    on_select.call(idx);
                }
            },
            option { value: "", "Matrix..." }
            for profile in profiles.iter() {
                option {
                    key: "{profile.index}",
                    value: "{profile.index}",
                    selected: active == Some(profile.index),
                    "{profile.name}"
                }
            }
        }
    }
}

/// Compact HQP controls for use in cards (profile + matrix in a row).
#[component]
pub fn HqpControlsCompact(
    /// Available profiles
    profiles: Vec<HqpProfile>,
    /// Available matrix profiles
    matrix_profiles: Vec<HqpMatrixProfile>,
    /// Currently active matrix profile index
    active_matrix: Option<u32>,
    /// Called when a profile is selected
    on_profile_select: EventHandler<String>,
    /// Called when a matrix profile is selected
    on_matrix_select: EventHandler<u32>,
) -> Element {
    rsx! {
        div { class: "flex flex-wrap gap-2 mt-4",
            if !profiles.is_empty() {
                HqpProfileSelect {
                    profiles: profiles,
                    on_select: on_profile_select,
                    class: "input flex-1 min-w-0".to_string(),
                }
            }
            if !matrix_profiles.is_empty() {
                HqpMatrixSelect {
                    profiles: matrix_profiles,
                    active: active_matrix,
                    on_select: on_matrix_select,
                    class: "input flex-1 min-w-0".to_string(),
                }
            }
        }
    }
}
