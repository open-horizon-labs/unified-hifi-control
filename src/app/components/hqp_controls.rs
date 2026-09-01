//! HQPlayer control components for profile and matrix selection.

use dioxus::prelude::*;

use crate::app::api::{HqpMatrixProfile, HqpProfile};

/// HQPlayer profile selector dropdown.
#[component]
pub fn HqpProfileSelect(
    // Available profiles to choose from
    profiles: Vec<HqpProfile>,
    // Called when a profile is selected
    on_select: EventHandler<String>,
    // Optional CSS class for the select element
    #[props(default = "input".to_string())] class: String,
    // Disable the select element
    #[props(default = false)] disabled: bool,
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
            option {
                value: "",
                selected: !profiles.iter().any(|profile| profile.active),
                "(no preset)"
            }
            for profile in profiles.iter() {
                {
                    let value = profile
                        .value
                        .as_deref()
                        .or(profile.name.as_deref())
                        .unwrap_or_default();
                    let title = profile.title.as_deref().unwrap_or(value);
                    rsx! {
                        option {
                            key: "{value}",
                            value: "{value}",
                            selected: profile.active,
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
    // Available matrix profiles to choose from
    profiles: Vec<HqpMatrixProfile>,
    // Currently active profile name. `None` is the unnamed `[Default]` matrix.
    active: Option<String>,
    // Called with the native profile name; empty selects `[Default]`.
    on_select: EventHandler<String>,
    // Optional CSS class for the select element
    #[props(default = "input".to_string())] class: String,
    // Disable the select element
    #[props(default = false)] disabled: bool,
) -> Element {
    rsx! {
        select {
            class: "{class}",
            disabled: disabled,
            onchange: move |evt| {
                on_select.call(evt.value());
            },
            option { value: "", selected: active.is_none(), "[Default]" }
            for profile in profiles.iter() {
                option {
                    key: "{profile.index}",
                    value: "{profile.name}",
                    selected: active.as_deref() == Some(profile.name.as_str()),
                    "{profile.name}"
                }
            }
        }
    }
}

/// Compact HQP controls for use in cards (profile + matrix in a row).
#[component]
pub fn HqpControlsCompact(
    // Available profiles
    profiles: Vec<HqpProfile>,
    // Available matrix profiles
    matrix_profiles: Vec<HqpMatrixProfile>,
    // Whether the matrix inventory was read successfully. The unnamed `[Default]` exists even
    // when there are no saved named profiles.
    matrix_available: bool,
    // Currently active matrix profile name; `None` is the unnamed `[Default]` matrix.
    active_matrix: Option<String>,
    // Called when a profile is selected
    on_profile_select: EventHandler<String>,
    // Called when a matrix profile is selected
    on_matrix_select: EventHandler<String>,
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
            if matrix_available {
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
