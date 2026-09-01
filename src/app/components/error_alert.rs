//! Dismissable error alert component.

use dioxus::prelude::*;

/// A dismissable error alert that displays an error message with a close button.
#[component]
pub fn ErrorAlert(
    /// The error message to display
    message: String,
    /// Called when the dismiss button is clicked
    on_dismiss: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "card bg-error/10 border-error text-error p-3 mb-4",
            "{message}"
            button {
                class: "btn btn-ghost btn-sm ml-2",
                onclick: move |_| on_dismiss.call(()),
                "×"
            }
        }
    }
}
