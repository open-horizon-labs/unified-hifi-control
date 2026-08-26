//! Client-side controller-auth state (#570).
//!
//! `src/api/controller_auth.rs` gates provider/credential routes (Spotify
//! client settings, OAuth, the tunnel endpoints, Apple Music pairing) behind
//! a one-time owner bootstrap token. Before this module existed, a client
//! that hit that gate saw the raw `HTTP 401: Controller authentication
//! required` string -- the gating itself was correct by design, but nothing
//! told the user a bootstrap flow exists or where the token lives.
//!
//! This module is the single place that tracks "should the bootstrap prompt
//! be showing" and "what CSRF token do state-changing requests need to
//! attach", as `GlobalSignal`s rather than a `use_context` provider: the
//! fetch helpers in `crate::app::api` are plain async functions called from
//! many unrelated pages, not hooks, so they cannot consume a context that
//! only exists inside a component subtree. A `GlobalSignal` is reachable
//! from anywhere once the app has mounted once, which is what lets
//! `crate::app::api::response_error` open the prompt as a side effect of any
//! fetch helper's error path (`fetch_json`, `post_json`,
//! `post_json_no_response`) without every call site duplicating the check.

use dioxus::prelude::*;

use crate::app::api::ControllerBootstrapResponse;

#[cfg(target_arch = "wasm32")]
const CSRF_STORAGE_KEY: &str = "uhc_controller_csrf";

/// CSRF token from the last successful bootstrap. The controller session
/// cookie is `HttpOnly` (deliberately unreadable by JS -- see
/// `src/api/controller_auth.rs`), so this double-submit token is the only
/// piece of bootstrap state the client can see, and the server hands it out
/// exactly once, in the `POST /api/controller/bootstrap` response body.
/// Nothing re-issues it, so it is mirrored into `localStorage` and reloaded
/// on startup -- otherwise every page refresh after a successful bootstrap
/// would 403 on the first state-changing request even though the session
/// cookie (good for 30 days) is still valid.
static CONTROLLER_CSRF_TOKEN: GlobalSignal<Option<String>> = Signal::global(load_stored_csrf_token);

/// Whether the bootstrap prompt overlay should currently be rendered.
static CONTROLLER_BOOTSTRAP_PROMPT_OPEN: GlobalSignal<bool> = Signal::global(|| false);

fn load_stored_csrf_token() -> Option<String> {
    #[cfg(target_arch = "wasm32")]
    {
        web_sys::window()?
            .local_storage()
            .ok()??
            .get_item(CSRF_STORAGE_KEY)
            .ok()?
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        None
    }
}

#[cfg(target_arch = "wasm32")]
fn persist_csrf_token(token: &str) {
    if let Some(storage) = web_sys::window().and_then(|w| w.local_storage().ok().flatten()) {
        let _ = storage.set_item(CSRF_STORAGE_KEY, token);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn persist_csrf_token(_token: &str) {}

/// The CSRF token to attach as `x-uhc-csrf-token` on state-changing
/// requests, once bootstrap has completed (this session or a prior one that
/// persisted the token to `localStorage`).
pub fn current_csrf_token() -> Option<String> {
    CONTROLLER_CSRF_TOKEN()
}

/// Whether the bootstrap prompt should currently be shown.
pub fn bootstrap_prompt_open() -> bool {
    CONTROLLER_BOOTSTRAP_PROMPT_OPEN()
}

/// Open the bootstrap prompt. Called from
/// `crate::app::api::response_error` whenever any fetch helper receives a
/// `controller_unauthorized` 401, and optionally by pages that want to check
/// `GET /api/controller/status` proactively on mount rather than waiting for
/// a failed request.
pub fn open_bootstrap_prompt() {
    *CONTROLLER_BOOTSTRAP_PROMPT_OPEN.write() = true;
}

/// Close the prompt without necessarily having bootstrapped (e.g. Cancel).
pub fn close_bootstrap_prompt() {
    *CONTROLLER_BOOTSTRAP_PROMPT_OPEN.write() = false;
}

/// Submit a bootstrap token, store the resulting CSRF token (in memory and
/// `localStorage`), and close the prompt on success. The caller is
/// responsible for re-attempting whatever owner-gated action originally
/// opened the prompt -- see `src/app/components/bootstrap_prompt.rs`, which
/// tells the user to retry it once this returns `Ok`.
pub async fn submit_bootstrap_token(token: &str) -> Result<(), String> {
    let response: ControllerBootstrapResponse =
        crate::app::api::bootstrap_controller(token).await?;
    persist_csrf_token(&response.csrf_token);
    *CONTROLLER_CSRF_TOKEN.write() = Some(response.csrf_token);
    close_bootstrap_prompt();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_stored_csrf_token_is_none_off_the_browser() {
        // Native/SSR builds have no `localStorage`; this must not panic and
        // must not fabricate a token.
        assert_eq!(load_stored_csrf_token(), None);
    }
}
