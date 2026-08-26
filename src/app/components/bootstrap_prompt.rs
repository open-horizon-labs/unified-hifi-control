//! Owner bootstrap prompt (#570).
//!
//! Renders in place of a raw `HTTP 401: Controller authentication required`
//! whenever `crate::app::controller_auth::bootstrap_prompt_open()` is true.
//! Mounted once at the app root (`src/app/mod.rs`) so it covers every page's
//! owner-gated action without each page duplicating the check.

use dioxus::prelude::*;

use crate::app::controller_auth;

#[component]
pub fn BootstrapPrompt() -> Element {
    let mut token = use_signal(String::new);
    let mut busy = use_signal(|| false);
    let mut error = use_signal(|| None::<String>);
    let mut unlocked = use_signal(|| false);

    rsx! {
        if controller_auth::bootstrap_prompt_open() {
            div {
                class: "fixed inset-0 bg-black/50 flex items-center justify-center z-50 p-4",
                div {
                    class: "card p-6 max-w-lg w-full",
                    role: "dialog",
                    aria_modal: "true",
                    aria_label: "Owner setup required",

                    h2 { class: "text-xl font-semibold mb-2", "Owner setup required" }
                    p { class: "text-sm text-secondary mb-4",
                        "This is a one-time step on a fresh install. UHC generated a private owner token so only "
                        "you can change provider credentials (like Spotify) or pairing settings. Paste it below to unlock this action."
                    }

                    div { class: "card bg-elevated p-3 mb-4 text-sm",
                        p { class: "font-medium mb-1", "Where to find the token:" }
                        ul { class: "list-disc list-inside space-y-1 text-secondary",
                            li { "QNAP: open " code { "$QPKG_ROOT/unified-hifi-control.log" } " (QTS App Center → UHC → the log file icon)." }
                            li { "Synology, Docker, or a plain binary install: check the server log or console output where UHC started." }
                            li { "Look for a line starting with " code { "UHC controller bootstrap token" } "." }
                            li { "If your operator set " code { "UHC_BOOTSTRAP_TOKEN" } " in the environment, use that value instead." }
                        }
                    }

                    if unlocked() {
                        p { class: "status-ok mb-4", role: "status", "✓ Owner access unlocked. Try that action again." }
                        button {
                            r#type: "button",
                            class: "btn btn-primary w-full",
                            onclick: move |_| {
                                unlocked.set(false);
                                controller_auth::close_bootstrap_prompt();
                            },
                            "Done"
                        }
                    } else {
                        if let Some(message) = error() {
                            div { class: "card bg-error/10 border-error text-error p-3 mb-4 text-sm", role: "alert", "{message}" }
                        }
                        form {
                            onsubmit: move |e| {
                                e.prevent_default();
                                let value = token().trim().to_string();
                                if value.is_empty() {
                                    error.set(Some("Enter the owner token first.".to_string()));
                                    return;
                                }
                                busy.set(true);
                                error.set(None);
                                spawn(async move {
                                    match controller_auth::submit_bootstrap_token(&value).await {
                                        Ok(()) => {
                                            unlocked.set(true);
                                            token.set(String::new());
                                        }
                                        Err(message) => error.set(Some(message)),
                                    }
                                    busy.set(false);
                                });
                            },
                            label { class: "block text-sm font-medium mb-1", r#for: "uhc-bootstrap-token", "Owner token" }
                            input {
                                id: "uhc-bootstrap-token",
                                class: "input w-full mb-3",
                                r#type: "password",
                                autocomplete: "off",
                                // This dialog typically opens while the user's focus is
                                // elsewhere (they're copying the token from a server log or
                                // terminal in another window). Without an explicit focus
                                // target, the first click back into this browser window can
                                // go toward refocusing the window itself rather than the
                                // control under the cursor -- an OS/browser-level behavior
                                // no page script can fully prevent -- which reads as a
                                // "first click did nothing" bug on whatever happens to be
                                // clicked first (reported against Unlock). Autofocusing the
                                // token field means that returning click, wherever it lands,
                                // has somewhere useful to go from the very first attempt.
                                autofocus: true,
                                placeholder: "Paste the token from the server log",
                                value: "{token}",
                                oninput: move |e| token.set(e.value()),
                            }
                            div { class: "flex gap-2 justify-end",
                                button {
                                    r#type: "button",
                                    class: "btn btn-outline",
                                    disabled: busy(),
                                    onclick: move |_| controller_auth::close_bootstrap_prompt(),
                                    "Cancel"
                                }
                                button {
                                    r#type: "submit",
                                    class: "btn btn-primary",
                                    disabled: busy(),
                                    aria_busy: busy(),
                                    if busy() { "Unlocking…" } else { "Unlock" }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
