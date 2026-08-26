//! Settings page component.
//!
//! Adapter settings and discovery status using Dioxus resources.

use dioxus::prelude::*;

use crate::app::api::{
    source_label, AppSettings, AppleBridgeStatus, HqpStatus, LmsConfig, ManagedZone,
    ManagedZonesResponse, MoveDirection, MqttConfigureRequest, MqttStatusResponse,
    MusicAssistantConfigureRequest, MusicAssistantStatusResponse, ProviderAuthResponse,
    ProviderOAuthStart, RoonStatus, SpotifyAccountResponse, SpotifyConfigureRequest,
    SpotifyConfigureResponse, SpotifyTunnelStatus, ZoneNameRequest, ZoneOrderRequest,
    ZoneVisibilityRequest, ZonesResponse,
};
use crate::app::components::{ErrorAlert, Layout};
use crate::app::settings_context::{initial_app_settings, use_settings};
use crate::app::sse::use_sse;
use crate::app::theme::{use_theme, Theme};
use crate::app::{McpEndpoint, Route};

/// OpenHome status response
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
struct OpenHomeStatus {
    device_count: usize,
}

/// UPnP status response
#[derive(Clone, Debug, Default, serde::Deserialize, PartialEq)]
struct UpnpStatus {
    renderer_count: usize,
}

#[derive(Clone, Copy, Default, PartialEq)]
enum CopyState {
    #[default]
    Idle,
    #[cfg(target_arch = "wasm32")]
    Copying,
    #[cfg(target_arch = "wasm32")]
    Copied,
    #[cfg(target_arch = "wasm32")]
    Failed,
}

impl CopyState {
    fn label(self, idle_label: &'static str) -> &'static str {
        match self {
            Self::Idle => idle_label,
            #[cfg(target_arch = "wasm32")]
            Self::Copying => "Copying…",
            #[cfg(target_arch = "wasm32")]
            Self::Copied => "Copied",
            #[cfg(target_arch = "wasm32")]
            Self::Failed => "Copy failed",
        }
    }
}

fn copy_to_clipboard(value: String, state: Signal<CopyState>) {
    #[cfg(target_arch = "wasm32")]
    {
        use wasm_bindgen::JsCast;

        let mut state = state;

        let Some(window) = web_sys::window() else {
            state.set(CopyState::Failed);
            return;
        };

        let clipboard = js_sys::Reflect::get(
            window.navigator().as_ref(),
            &wasm_bindgen::JsValue::from_str("clipboard"),
        )
        .ok()
        .filter(|value| !value.is_null() && !value.is_undefined())
        .and_then(|value| value.dyn_into::<web_sys::Clipboard>().ok());

        if let Some(clipboard) = clipboard {
            state.set(CopyState::Copying);
            let promise = clipboard.write_text(&value);
            wasm_bindgen_futures::spawn_local(async move {
                let next = if wasm_bindgen_futures::JsFuture::from(promise).await.is_ok() {
                    CopyState::Copied
                } else {
                    CopyState::Failed
                };
                show_copy_result(state, next);
            });
            return;
        }

        let next = if copy_to_clipboard_legacy(&window, &value) {
            CopyState::Copied
        } else {
            CopyState::Failed
        };
        show_copy_result(state, next);
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        // Event handlers run in the hydrated WASM client, never during SSR.
        let _ = (value, state);
    }
}

#[derive(Clone, Copy, Default, PartialEq)]
enum ProviderActionState {
    #[default]
    Idle,
    Loading,
    Success,
    Failed,
}

/// A Settings switch is persisted server-side before the page is refreshed.
/// This keeps adapter lifecycle changes transactional and gives the next page
/// render one authoritative configuration to hydrate from.
#[derive(Clone, Copy)]
enum SettingsToggle {
    Adapter(AdapterToggle, bool),
    HideKnobs(bool),
}

#[derive(Clone, Copy)]
enum AdapterToggle {
    Roon,
    Lms,
    OpenHome,
    Upnp,
    HqPlayer,
    Spotify,
    AppleMusic,
    MusicAssistant,
    Mqtt,
}

fn settings_with_toggle(mut settings: AppSettings, toggle: SettingsToggle) -> AppSettings {
    match toggle {
        SettingsToggle::Adapter(AdapterToggle::Roon, enabled) => settings.adapters.roon = enabled,
        SettingsToggle::Adapter(AdapterToggle::Lms, enabled) => settings.adapters.lms = enabled,
        SettingsToggle::Adapter(AdapterToggle::OpenHome, enabled) => {
            settings.adapters.openhome = enabled
        }
        SettingsToggle::Adapter(AdapterToggle::Upnp, enabled) => settings.adapters.upnp = enabled,
        SettingsToggle::Adapter(AdapterToggle::HqPlayer, enabled) => {
            settings.adapters.hqplayer = enabled;
            settings.hide_hqp_page = !enabled;
        }
        SettingsToggle::Adapter(AdapterToggle::Spotify, enabled) => {
            settings.adapters.spotify = enabled
        }
        SettingsToggle::Adapter(AdapterToggle::AppleMusic, enabled) => {
            settings.adapters.applemusic = enabled
        }
        SettingsToggle::Adapter(AdapterToggle::MusicAssistant, enabled) => {
            settings.adapters.musicassistant = enabled
        }
        SettingsToggle::Adapter(AdapterToggle::Mqtt, enabled) => settings.adapters.mqtt = enabled,
        SettingsToggle::HideKnobs(enabled) => settings.hide_knobs_page = !enabled,
    }
    settings
}

#[cfg(target_arch = "wasm32")]
fn reload_settings_page() {
    if let Some(window) = web_sys::window() {
        let _ = window.location().reload();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn reload_settings_page() {}

#[cfg(target_arch = "wasm32")]
fn show_settings_save_error(error: &str) {
    if let Some(window) = web_sys::window() {
        let _ = window.alert_with_message(&format!(
            "UHC could not apply that change. Your previous setting is still active.\n\n{error}"
        ));
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn show_settings_save_error(_error: &str) {}

fn persist_settings_then_reload(requested: AppSettings) {
    spawn(async move {
        let result = crate::app::api::post_json::<AppSettings, serde_json::Value>(
            "/api/settings",
            &requested,
        )
        .await
        .and_then(|response| settings_write_error(&response).map_or(Ok(()), Err));

        if let Err(error) = result {
            show_settings_save_error(&error);
        }
        reload_settings_page();
    });
}

#[component]
fn FeatureToggle(label: &'static str, enabled: bool, onclick: EventHandler<MouseEvent>) -> Element {
    rsx! {
        button {
            r#type: "button",
            class: "feature-toggle",
            role: "switch",
            aria_label: label,
            // ARIA attributes are string-valued. Keeping this explicit avoids
            // Dioxus interpreting a Rust bool differently between SSR and the
            // browser's hydration pass.
            aria_checked: if enabled { "true" } else { "false" },
            "data-settings-toggle": match label {
                "Enable Roon" => "adapters.roon",
                "Enable OpenHome" => "adapters.openhome",
                "Enable UPnP/DLNA" => "adapters.upnp",
                "Enable LMS" => "adapters.lms",
                "Enable HQPlayer" => "adapters.hqplayer",
                "Enable Spotify" => "adapters.spotify",
                "Enable Apple Music" => "adapters.applemusic",
                "Enable MQTT/Home Assistant" => "adapters.mqtt",
                "Show Controllers page" => "hide_knobs_page",
                _ => "",
            },
            onclick: move |event| onclick.call(event),
            span { class: "feature-toggle__knob", aria_hidden: "true" }
            span { class: "sr-only", "{label}" }
        }
    }
}

fn settings_write_error(response: &serde_json::Value) -> Option<String> {
    if response.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        return None;
    }
    Some(
        response
            .get("error")
            .and_then(serde_json::Value::as_str)
            .filter(|message| !message.is_empty())
            .unwrap_or("UHC could not apply that change. Your previous setting was restored.")
            .to_string(),
    )
}

impl ProviderActionState {
    fn message(self) -> Option<&'static str> {
        match self {
            Self::Idle | Self::Loading => None,
            Self::Success => Some("Updated."),
            Self::Failed => Some("Something went wrong. Try again or open Client settings."),
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn redirect_to(url: &str) -> Result<(), String> {
    web_sys::window()
        .ok_or_else(|| "Browser window is unavailable".to_string())?
        .location()
        .set_href(url)
        .map_err(|error| format!("Could not open provider authorization: {error:?}"))
}

#[cfg(not(target_arch = "wasm32"))]
fn redirect_to(_url: &str) -> Result<(), String> {
    Err("Provider authorization is available in the browser".to_string())
}

/// Bring an element into view by id. Used after "Save client settings":
/// saving collapses the client-settings editor, and without an explicit
/// scroll the browser anchors somewhere below the Connect button, forcing
/// the user to scroll back up and reorient before the very next step.
#[cfg(target_arch = "wasm32")]
fn scroll_to_element(id: &str) {
    if let Some(element) = web_sys::window()
        .and_then(|window| window.document())
        .and_then(|document| document.get_element_by_id(id))
    {
        element.scroll_into_view();
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn scroll_to_element(_id: &str) {}

#[cfg(target_arch = "wasm32")]
fn current_location_search() -> Option<String> {
    let window = web_sys::window()?;
    if let Ok(search) = window.location().search() {
        if !search.is_empty() {
            return Some(search);
        }
    }
    // The router normalizes the URL -- dropping the OAuth-return query --
    // before this page's first client render, so `location.search` is
    // already empty by the time anything here can read it (#597 probe
    // finding). The browser keeps the document's original navigation URL in
    // the performance timeline; recover the query from there. Memoized
    // (idempotently -- hydration can initialize the page's hooks more than
    // once) so every reader in this document sees the same value; a real
    // reload of the normalized URL naturally clears it, because the fresh
    // navigation entry carries no query.
    use std::sync::OnceLock;
    static NAVIGATION_QUERY: OnceLock<Option<String>> = OnceLock::new();
    NAVIGATION_QUERY
        .get_or_init(|| {
            let entries = window.performance()?.get_entries_by_type("navigation");
            let entry = entries.get(0);
            use wasm_bindgen::JsCast;
            let entry = entry.dyn_ref::<web_sys::PerformanceEntry>()?;
            let name = entry.name();
            let (_, query) = name.split_once('?')?;
            Some(format!("?{query}"))
        })
        .clone()
}

#[cfg(not(target_arch = "wasm32"))]
fn current_location_search() -> Option<String> {
    // Event handlers and the initial location query only exist in the
    // hydrated WASM client; the SSR render always shows no callback feedback.
    None
}

fn callback_feedback() -> Option<CallbackFeedback> {
    spotify_callback_feedback(&current_location_search()?)
}

const DEFAULT_SPOTIFY_REDIRECT_URI: &str =
    "http://127.0.0.1:8088/api/providers/spotify/oauth/callback";

fn default_spotify_redirect_uri() -> String {
    DEFAULT_SPOTIFY_REDIRECT_URI.to_string()
}

/// Per-step info (ⓘ) disclosure panel for the Spotify stepper (#597).
/// Written as full literals so Tailwind's `.rs` source scan keeps every
/// utility. The closed variant still reveals on hover (`group-hover`) for
/// mouse users; touch and keyboard users pin it open with the ⓘ button,
/// which swaps in the open variant and sets `aria-expanded`.
const SPOTIFY_INFO_PANEL_OPEN: &str = "absolute right-0 top-full z-20 mt-1 w-72 max-w-[85vw] rounded-md border border-default bg-elevated p-3 text-left text-xs text-secondary shadow-lg";
const SPOTIFY_INFO_PANEL_CLOSED: &str = "absolute right-0 top-full z-20 mt-1 w-72 max-w-[85vw] rounded-md border border-default bg-elevated p-3 text-left text-xs text-secondary shadow-lg hidden group-hover:block";

/// The same ⓘ disclosure for the MQTT section (#605), which needs one panel
/// rather than the stepper's one-per-step. Anchored `left-0` because it
/// hangs off a heading at the left edge, where the Spotify panel's
/// right-anchoring would push it off-screen.
const MQTT_INFO_PANEL_OPEN: &str = "absolute left-0 top-full z-20 mt-1 w-80 max-w-[85vw] rounded-md border border-default bg-elevated p-3 text-left text-xs text-secondary shadow-lg";
const MQTT_INFO_PANEL_CLOSED: &str = "absolute left-0 top-full z-20 mt-1 w-80 max-w-[85vw] rounded-md border border-default bg-elevated p-3 text-left text-xs text-secondary shadow-lg hidden group-hover:block";

/// Whether *this browser* is talking to UHC over loopback. Drives which
/// variant of the Spotify callback step (#570 follow-up) renders: a
/// loopback browser can use the plain HTTP loopback callback directly, but
/// every other origin -- a NAS accessed from another device, which is the
/// common case -- needs the HTTPS tunnel, so showing the loopback URL as
/// the primary instruction there just walks a beginner into registering a
/// callback Spotify will never redirect to.
#[cfg(target_arch = "wasm32")]
fn browser_is_loopback_origin() -> bool {
    web_sys::window()
        .and_then(|window| window.location().hostname().ok())
        .map(|hostname| matches!(hostname.as_str(), "127.0.0.1" | "localhost" | "::1"))
        .unwrap_or(false)
}

/// Feedback for the Spotify OAuth redirect back to Settings. Kept separate
/// from `callback_feedback`'s `web_sys` lookup so the query-string parsing and
/// per-error-code messaging stay covered by ordinary (non-wasm) unit tests.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CallbackFeedback {
    message: String,
    is_error: bool,
}

/// Parse the `?spotify=...&reason=...` query string Spotify's OAuth callback
/// redirects to (see `oauth_callback` in `src/api/provider_auth.rs`) into a
/// message a non-technical user can act on. Every `code` value produced by
/// that handler must have an arm below so a new failure mode never falls back
/// to the generic message silently.
fn spotify_callback_feedback(search: &str) -> Option<CallbackFeedback> {
    if search.contains("spotify=connected") || search.contains("oauth=success") {
        return Some(CallbackFeedback {
            message: "Spotify connected. Refreshing available devices…".to_string(),
            is_error: false,
        });
    }
    if search.contains("spotify=error") || search.contains("oauth=error") {
        let reason = query_param(search, "reason");
        return Some(CallbackFeedback {
            message: spotify_oauth_error_message(reason.as_deref()),
            is_error: true,
        });
    }
    None
}

/// Extract one query-string parameter's value without pulling in a full URL
/// parser; the callback redirect only ever carries simple ASCII tokens.
fn query_param(search: &str, key: &str) -> Option<String> {
    search
        .trim_start_matches('?')
        .split('&')
        .find_map(|pair| pair.strip_prefix(key)?.strip_prefix('='))
        .map(|value| urlencoding::decode(value).unwrap_or_default().into_owned())
}

/// Map an `oauth_callback` error `code` to an actionable message. Codes come
/// from `error(...)` calls in `src/api/provider_auth.rs::oauth_callback_json`.
fn spotify_oauth_error_message(reason: Option<&str>) -> String {
    match reason {
        Some("invalid_state") | Some("expired_state") => {
            "This Spotify sign-in link expired or was already used. Click Connect Spotify below to start a fresh authorization.".to_string()
        }
        Some("provider_denied") => {
            "Spotify authorization was declined. Click Connect Spotify to try again.".to_string()
        }
        Some("provider_oauth_error") | Some("missing_authorization_code") => {
            "Spotify did not return an authorization code. Click Connect Spotify to try again.".to_string()
        }
        Some("token_exchange_failed") => {
            "Spotify rejected the sign-in exchange. This usually means the address registered in your Spotify app does not exactly match the one saved in step 2. Fix the mismatch, save, and Connect again.".to_string()
        }
        Some("token_storage_failed") => {
            "Spotify authorized, but the token could not be saved on this UHC server. Check the server's credential storage and try Connect again.".to_string()
        }
        Some("oauth_not_configured") | Some("invalid_client_configuration") => {
            "Spotify client settings are missing or invalid. Enter and save the Client ID (and Secret, if used) below before connecting.".to_string()
        }
        Some("adapter_unavailable") | Some("adapter_start_failed") => {
            "Spotify authorized, but the adapter could not start. Refresh this page and try again.".to_string()
        }
        Some("companion_required") => {
            "This provider is authorized through its companion app, not this OAuth flow.".to_string()
        }
        _ => "Spotify authorization did not complete. Try Connect again or open Client settings.".to_string(),
    }
}

fn zone_state_label(zone: &crate::app::api::Zone) -> &str {
    zone.state.as_deref().unwrap_or("unknown")
}

fn spotify_device_state_label(zone: &crate::app::api::Zone) -> &str {
    match zone_state_label(zone) {
        "playing" => "Playing",
        "paused" => "Paused",
        "unknown" => "No playback reported",
        state => state,
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AppleMusicStatusState {
    Unpaired,
    PairedWaiting,
    Live,
}

fn apple_music_status_state(status: Option<&AppleBridgeStatus>) -> AppleMusicStatusState {
    let Some(status) = status else {
        return AppleMusicStatusState::Unpaired;
    };

    let companion_paired = status.companions.iter().any(|companion| companion.paired);
    let companion_live = status
        .companions
        .iter()
        .any(|companion| companion.paired && companion.live);

    if status.live || companion_live {
        AppleMusicStatusState::Live
    } else if status.paired || companion_paired {
        AppleMusicStatusState::PairedWaiting
    } else {
        AppleMusicStatusState::Unpaired
    }
}

fn apple_music_live_companion_count(status: Option<&AppleBridgeStatus>) -> usize {
    status
        .map(|status| {
            status
                .companions
                .iter()
                .filter(|companion| companion.paired && companion.live)
                .count()
        })
        .unwrap_or(0)
}

#[cfg(target_arch = "wasm32")]
fn browser_confirm(message: &str) -> bool {
    web_sys::window()
        .and_then(|window| window.confirm_with_message(message).ok())
        .unwrap_or(false)
}

#[cfg(not(target_arch = "wasm32"))]
fn browser_confirm(_message: &str) -> bool {
    false
}

#[cfg(target_arch = "wasm32")]
fn browser_prompt(message: &str, default: &str) -> Option<String> {
    web_sys::window()
        .and_then(|window| {
            window
                .prompt_with_message_and_default(message, default)
                .ok()
        })
        .flatten()
}

#[cfg(not(target_arch = "wasm32"))]
fn browser_prompt(_message: &str, _default: &str) -> Option<String> {
    None
}

#[cfg(target_arch = "wasm32")]
fn show_copy_result(mut state: Signal<CopyState>, result: CopyState) {
    state.set(result);
    spawn(async move {
        dioxus_sdk_time::sleep(std::time::Duration::from_secs(2)).await;
        if state() == result {
            state.set(CopyState::Idle);
        }
    });
}

#[cfg(target_arch = "wasm32")]
fn copy_to_clipboard_legacy(window: &web_sys::Window, value: &str) -> bool {
    use wasm_bindgen::JsCast;

    let result = (|| -> Result<bool, ()> {
        let document = window.document().ok_or(())?;
        let html_document = document
            .clone()
            .dyn_into::<web_sys::HtmlDocument>()
            .map_err(|_| ())?;
        let textarea = document
            .create_element("textarea")
            .map_err(|_| ())?
            .dyn_into::<web_sys::HtmlTextAreaElement>()
            .map_err(|_| ())?;

        textarea.set_value(value);
        textarea.set_attribute("readonly", "").map_err(|_| ())?;
        textarea
            .set_attribute("aria-hidden", "true")
            .map_err(|_| ())?;
        textarea
            .style()
            .set_property("position", "fixed")
            .map_err(|_| ())?;
        textarea
            .style()
            .set_property("inset-inline-start", "-9999px")
            .map_err(|_| ())?;

        let body = document.body().ok_or(())?;
        body.append_child(&textarea).map_err(|_| ())?;
        let _ = textarea.focus();
        textarea.select();

        let copied = html_document.exec_command("copy");
        textarea.remove();

        copied.map_err(|_| ())
    })();

    result.unwrap_or(false)
}

/// Settings page component.
#[component]
pub fn Settings() -> Element {
    let sse = use_sse();
    let theme_ctx = use_theme();
    let settings_ctx = use_settings();
    let mcp_endpoint = use_context::<McpEndpoint>();
    let agent_config = mcp_endpoint.agent_config();
    let config_copy_state = use_signal(CopyState::default);
    let url_copy_state = use_signal(CopyState::default);

    let initial_settings = initial_app_settings();
    // Adapter toggle signals all begin from the same app-root snapshot SSR
    // used, so the first client tree has the same topology as the server tree.
    let mut roon_enabled = use_signal(|| initial_settings.adapters.roon);
    let mut lms_enabled = use_signal(|| initial_settings.adapters.lms);
    let mut openhome_enabled = use_signal(|| initial_settings.adapters.openhome);
    let mut upnp_enabled = use_signal(|| initial_settings.adapters.upnp);
    let mut hqplayer_enabled = use_signal(|| initial_settings.adapters.hqplayer);
    let mut spotify_enabled = use_signal(|| initial_settings.adapters.spotify);
    let mut applemusic_enabled = use_signal(|| initial_settings.adapters.applemusic);
    let mut musicassistant_enabled = use_signal(|| initial_settings.adapters.musicassistant);
    let mut mqtt_enabled = use_signal(|| initial_settings.adapters.mqtt);

    // Streaming-provider onboarding state. Provider credentials are never
    // rendered or stored in the browser; the backend owns OAuth tokens.
    let mut spotify_action = use_signal(ProviderActionState::default);
    let mut spotify_error = use_signal(|| None::<String>);
    let mut spotify_local_setup_saved = use_signal(|| false);
    // Spotify onboarding stepper (#597). Steps 1 and 2 happen inside
    // Spotify's developer dashboard where UHC cannot observe completion,
    // so each carries a one-tap local attestation; steps 3 and 4 derive
    // purely from server state (client settings saved / account
    // connected), so landing on the page mid-setup resumes at the right
    // step. `spotify_reopen_step` re-expands a completed step without
    // changing its completion.
    let mut spotify_step_app_done = use_signal(|| false);
    let mut spotify_step_callback_done = use_signal(|| false);
    let mut spotify_reopen_step = use_signal(|| None::<u8>);
    // Which step's info (ⓘ) disclosure is pinned open by click/tap; hover
    // reveal is CSS-only (group-hover) so touch and mouse both work.
    let mut spotify_info_open = use_signal(|| None::<u8>);
    // Which action the shared status message belongs to, so errors render
    // inside their own step (3 = save, 4 = connect, 5 = disconnect)
    // rather than as a distant banner.
    let mut spotify_action_scope = use_signal(|| 0u8);
    // True while the redirect URI shown in the browser diverges from what
    // was last saved (manual edits, tunnel fix-up); sends the stepper back
    // to Save before Connect even when the server is already configured.
    let mut spotify_resave_needed = use_signal(|| false);
    let mut spotify_client_id = use_signal(String::new);
    let mut spotify_client_secret = use_signal(String::new);
    let mut spotify_redirect_uri = use_signal(default_spotify_redirect_uri);
    let spotify_callback_copy_state = use_signal(CopyState::default);
    // Temporary HTTPS tunnel for the Spotify OAuth callback (#538). Kept
    // separate from `spotify_action`/`spotify_error` above -- those track
    // saving client settings and connecting, this tracks a short-lived
    // background process with its own poll loop.
    let mut spotify_tunnel = use_signal(SpotifyTunnelStatus::default);
    let mut spotify_tunnel_busy = use_signal(|| false);
    let spotify_tunnel_url_copy_state = use_signal(CopyState::default);
    // Guards the while-active status poll loop against duplicate spawns.
    let mut spotify_tunnel_polling = use_signal(|| false);
    // Defaults to the loopback-primary layout (today's behavior, and what
    // SSR renders) so hydration has nothing to reconcile; a mount-only
    // effect below flips it once the real browser origin is known. No
    // tracked signal is read in that effect, so -- like the controller-auth
    // status check above -- it runs exactly once (reactive-loop-lint).
    let spotify_remote_origin = use_signal(|| false);
    let mut musicassistant_action = use_signal(ProviderActionState::default);
    let mut musicassistant_error = use_signal(|| None::<String>);
    let mut musicassistant_host = use_signal(String::new);
    let mut musicassistant_port = use_signal(|| "8095".to_string());
    let mut musicassistant_token = use_signal(String::new);
    let mut musicassistant_tls = use_signal(|| true);
    let mut musicassistant_insecure_http = use_signal(|| false);
    let mut mqtt_action = use_signal(ProviderActionState::default);
    let mut mqtt_error = use_signal(|| None::<String>);
    let mut mqtt_host = use_signal(String::new);
    let mut mqtt_port = use_signal(String::new);
    let mut mqtt_username = use_signal(String::new);
    let mut mqtt_password = use_signal(String::new);
    let mut mqtt_base_topic = use_signal(|| "unified-hifi".to_string());
    let mut mqtt_discovery_prefix = use_signal(|| "homeassistant".to_string());
    let mut mqtt_tls = use_signal(|| false);
    // ⓘ disclosure for what the Home Assistant side has to do (#605).
    let mut mqtt_info_open = use_signal(|| false);
    // When the add-on supplies the broker there is nothing for the user to
    // fill in, so the manual form starts collapsed rather than sitting there
    // pre-filled and inviting them to re-enter settings they never chose.
    let mut mqtt_manual_open = use_signal(|| false);
    let mut confirmed_settings = use_signal(|| None::<AppSettings>);

    // Hide knobs signal (LMS/HQPlayer visibility follows adapter enabled state)
    let mut hide_knobs = use_signal(|| initial_settings.hide_knobs_page);

    // Load settings resource
    let settings = use_resource(|| async {
        crate::app::api::fetch_json::<AppSettings>("/api/settings")
            .await
            .ok()
    });

    // Proactively check controller-auth status once on mount (#570): the
    // Settings page hosts every owner-gated action (Spotify client
    // settings, its tunnel, Music Assistant, Apple Music pairing), so a
    // fresh NAS install can be routed into the bootstrap prompt before the
    // user even attempts a save, not just after it 401s. No tracked signal
    // is read here, so -- like `settings_context::use_settings_provider`'s
    // equivalent effect -- this runs exactly once per mount rather than
    // looping (reactive-loop-lint).
    #[cfg(target_arch = "wasm32")]
    use_effect(move || {
        spawn(async move {
            if let Ok(status) = crate::app::api::fetch_controller_status().await {
                if status.auth_required && status.bootstrap_required && !status.authenticated {
                    crate::app::controller_auth::open_bootstrap_prompt();
                }
            }
        });
    });

    // Determine the real browser origin once mounted (see
    // `browser_is_loopback_origin`'s doc comment for why this can't just be
    // computed inline during render: SSR has no origin to read, so the
    // signal starts at the loopback-primary default and this corrects it
    // after hydration).
    #[cfg(target_arch = "wasm32")]
    use_effect(move || {
        let mut spotify_remote_origin = spotify_remote_origin;
        if !browser_is_loopback_origin() {
            spotify_remote_origin.set(true);
        }
    });

    // Sync settings to signals when loaded
    use_effect(move || {
        if let Some(Some(s)) = settings.read().as_ref() {
            roon_enabled.set(s.adapters.roon);
            lms_enabled.set(s.adapters.lms);
            openhome_enabled.set(s.adapters.openhome);
            upnp_enabled.set(s.adapters.upnp);
            hqplayer_enabled.set(s.adapters.hqplayer);
            spotify_enabled.set(s.adapters.spotify);
            applemusic_enabled.set(s.adapters.applemusic);
            musicassistant_enabled.set(s.adapters.musicassistant);
            mqtt_enabled.set(s.adapters.mqtt);
            hide_knobs.set(s.hide_knobs_page);
            confirmed_settings.set(Some(s.clone()));
            // Sync to shared context for Nav reactivity (page visibility follows adapter state)
            settings_ctx.update(
                s.hide_knobs_page,
                s.adapters.hqplayer,
                s.adapters.lms,
                s.adapters.spotify,
            );
            settings_ctx.mark_loaded();
        }
    });

    // Zone management: every zone the user can manage, hidden ones included. Deliberately not
    // `/zones`, which excludes hidden zones -- this is the one view that has to show them, or
    // hiding would be a one-way door.
    let mut managed_zones = use_resource(|| async {
        crate::app::api::fetch_json::<ManagedZonesResponse>("/api/zones/visibility")
            .await
            .ok()
    });

    // Announced to assistive tech after every zone change. Without it a screen-reader user presses
    // "move up" and hears nothing at all -- the row moves silently.
    let mut zone_status = use_signal(String::new);
    let mut zone_error = use_signal(|| Option::<String>::None);
    // Bumped on a failed write. It is part of each row's key, so a failure forces Dioxus to recreate
    // the row's DOM nodes. Without that, a rejected checkbox click leaves the browser's natively
    // toggled checkbox showing the opposite of the truth: the refetched data is unchanged, so the
    // VDOM `checked` value is unchanged, so the diff emits no correcting mutation.
    let mut zone_revision = use_signal(|| 0u32);
    // In-progress name edits, by zone id.
    //
    // `value:` on a Dioxus input makes it *controlled*: after every input event the DOM value is
    // reset to whatever the VDOM says. With only an `onchange` handler -- which fires on blur, not
    // per keystroke -- nothing held the typed text, so each character was immediately overwritten by
    // the server's name and the field appeared to reject typing. Every other text input in this app
    // pairs `value:` with an `oninput` that writes to a signal; this is that signal, per row.
    //
    // A draft is cleared once the rename commits, so the field falls back to the server's answer --
    // which is what makes the "reset" button visibly restore the provider's name.
    let mut name_drafts = use_signal(std::collections::HashMap::<String, String>::new);

    let mut report_zone_failure = move |err: String| {
        zone_error.set(Some(err));
        zone_revision += 1;
    };

    let set_zone_hidden = move |zone: ManagedZone| {
        let label = zone.qualified_label();
        let now_hidden = !zone.hidden;
        spawn(async move {
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/visibility",
                &ZoneVisibilityRequest {
                    zone_id: zone.zone_id.clone(),
                    hidden: now_hidden,
                },
            )
            .await;
            match result {
                Ok(_) => {
                    zone_error.set(None);
                    zone_status.set(if now_hidden {
                        format!("{label} hidden.")
                    } else {
                        format!("{label} shown.")
                    });
                }
                Err(err) => report_zone_failure(err),
            }
            managed_zones.restart();
        });
    };

    let move_zone = move |zone: ManagedZone, direction: MoveDirection, at_boundary: bool| {
        let label = zone.qualified_label();
        spawn(async move {
            if at_boundary {
                // The control stays focusable at the boundary rather than becoming `disabled`,
                // because a disabled element cannot hold focus -- pressing "up" on the second row
                // would move the zone to first, disable the button under the user's finger, and drop
                // focus to <body>. Saying so is better than silence.
                zone_status.set(match direction {
                    MoveDirection::Up => format!("{label} is already first."),
                    MoveDirection::Down => format!("{label} is already last."),
                });
                return;
            }
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/order",
                &ZoneOrderRequest::step(zone.zone_id.clone(), direction),
            )
            .await;
            match result {
                Ok(body) => {
                    zone_error.set(None);
                    // The server reports whether a swap actually happened. `at_boundary` above is
                    // computed from the first/last *visible* rows, so a hidden row never matches it
                    // and always sends a request -- and a hidden row that is already first has
                    // nothing to swap with. Announcing "moved up" there would tell a screen-reader
                    // user, who has no other feedback for this action, something untrue.
                    let moved = body
                        .get("moved")
                        .and_then(serde_json::Value::as_bool)
                        .unwrap_or(true);
                    zone_status.set(match (moved, direction) {
                        (true, MoveDirection::Up) => format!("{label} moved up."),
                        (true, MoveDirection::Down) => format!("{label} moved down."),
                        (false, MoveDirection::Up) => format!("{label} is already first."),
                        (false, MoveDirection::Down) => format!("{label} is already last."),
                    });
                }
                Err(err) => report_zone_failure(err),
            }
            managed_zones.restart();
        });
    };

    // Drag-and-drop state. HTML5 drag events do not fire on touch devices, so this is an
    // enhancement layered over the up/down buttons rather than a replacement for them -- the buttons
    // remain the path for phones and for keyboard users.
    let mut dragging = use_signal(|| Option::<String>::None);
    let mut drop_target = use_signal(|| Option::<String>::None);

    let drop_zone_onto = move |zone: ManagedZone, target: ManagedZone| {
        let label = zone.qualified_label();
        let target_label = target.qualified_label();
        spawn(async move {
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/order",
                &ZoneOrderRequest::drop_onto(zone.zone_id.clone(), target.zone_id.clone()),
            )
            .await;
            match result {
                Ok(_) => {
                    zone_error.set(None);
                    zone_status.set(format!("{label} moved to {target_label}'s position."));
                }
                Err(err) => report_zone_failure(err),
            }
            managed_zones.restart();
        });
    };

    let mut rename_zone = move |zone: ManagedZone, name: String| {
        let trimmed = name.trim().to_string();
        if trimmed == zone.zone_name {
            // Nothing to save, but drop the draft anyway so a field edited only by whitespace snaps
            // back to the canonical name instead of keeping the user's spacing.
            name_drafts.write().remove(&zone.zone_id);
            return;
        }
        spawn(async move {
            let cleared = trimmed.is_empty();
            let result = crate::app::api::post_json::<_, serde_json::Value>(
                "/api/zones/name",
                &ZoneNameRequest {
                    zone_id: zone.zone_id.clone(),
                    name: if cleared { None } else { Some(trimmed.clone()) },
                },
            )
            .await;
            match result {
                Ok(_) => {
                    zone_error.set(None);
                    zone_status.set(if cleared {
                        format!("Name reset to {}.", zone.provider_name)
                    } else {
                        format!("Renamed to {trimmed}.")
                    });
                }
                Err(err) => report_zone_failure(err),
            }
            // The server is now authoritative for this row -- but only for the text we submitted.
            // The user can start typing again while the request is in flight, so drop the draft
            // only if it still matches what was sent; otherwise the newer keystrokes would vanish
            // and the field would snap back to the name they had just moved on from.
            let stale = name_drafts
                .read()
                .get(&zone.zone_id)
                .is_some_and(|draft| draft.trim() == trimmed);
            if stale {
                name_drafts.write().remove(&zone.zone_id);
            }
            managed_zones.restart();
        });
    };

    // Discovery status resources
    let mut roon_status = use_resource(|| async {
        crate::app::api::fetch_json::<RoonStatus>("/roon/status")
            .await
            .ok()
    });
    let mut openhome_status = use_resource(|| async {
        crate::app::api::fetch_json::<OpenHomeStatus>("/openhome/status")
            .await
            .ok()
    });
    let mut upnp_status = use_resource(|| async {
        crate::app::api::fetch_json::<UpnpStatus>("/upnp/status")
            .await
            .ok()
    });
    let mut lms_config = use_resource(|| async {
        crate::app::api::fetch_json::<LmsConfig>("/lms/config")
            .await
            .ok()
    });
    let mut hqp_status = use_resource(|| async {
        crate::app::api::fetch_json::<HqpStatus>("/hqplayer/status")
            .await
            .ok()
    });
    let mut provider_zones = use_resource(|| async {
        crate::app::api::fetch_json::<ZonesResponse>("/zones")
            .await
            .map(|response| response.zones)
    });
    let mut spotify_account = use_resource(|| async {
        crate::app::api::fetch_json::<SpotifyAccountResponse>("/api/providers/spotify/account")
            .await
            .ok()
    });
    // A tunnel started before a page reload (or from another browser tab)
    // is still running server-side; pick up its status once on load rather
    // than assuming idle. Ongoing progress after a user-initiated start is
    // polled directly by `start_spotify_tunnel` instead of through this
    // resource.
    let spotify_tunnel_initial = use_resource(|| async {
        crate::app::api::fetch_json::<SpotifyTunnelStatus>("/api/providers/spotify/tunnel/status")
            .await
            .ok()
    });
    use_effect(move || {
        if let Some(Some(status)) = spotify_tunnel_initial.read().as_ref() {
            spotify_tunnel.set(status.clone());
        }
    });
    let mut musicassistant_status = use_resource(|| async {
        crate::app::api::fetch_json::<MusicAssistantStatusResponse>(
            "/api/providers/musicassistant/status",
        )
        .await
        .ok()
    });
    let mut apple_bridge_status = use_resource(|| async {
        crate::app::api::fetch_json::<AppleBridgeStatus>("/api/bridges/applemusic/status").await
    });
    let mut mqtt_status = use_resource(|| async {
        crate::app::api::fetch_json::<MqttStatusResponse>("/api/mqtt/status")
            .await
            .ok()
    });

    use_effect(move || {
        if let Some(Some(status)) = musicassistant_status.read().as_ref() {
            if let Some(endpoint) = status.endpoint.as_ref() {
                musicassistant_host.set(endpoint.host.clone());
                musicassistant_port.set(endpoint.port.to_string());
                musicassistant_tls.set(endpoint.tls);
                musicassistant_insecure_http.set(endpoint.allow_insecure_http);
            }
        }
    });

    // Once the tunnel is live, offer its callback address for registration.
    // The Redirect URI field is only pre-filled automatically while it still
    // holds the loopback default (or is empty): a value the user already
    // entered or saved is NEVER silently replaced (#592) -- tunnel providers
    // mint a new subdomain on every start, so silently swapping the field
    // out from under an address that is already registered in the Spotify
    // dashboard guarantees a "redirect_uri: Not matching configuration"
    // rejection at Spotify. A divergence is surfaced as a render-derived
    // warning (`spotify_tunnel_mismatch` below) with an explicit "Use this
    // address" action instead, and the server refuses oauth/start outright
    // while the live tunnel and the saved Redirect URI disagree.
    use_effect(move || {
        if let Some(url) = spotify_tunnel().url {
            let tunnel_callback = format!("{url}/api/providers/spotify/oauth/callback");
            let current = spotify_redirect_uri.peek().clone();
            if current != tunnel_callback
                && (current.trim().is_empty() || current == default_spotify_redirect_uri())
            {
                spotify_redirect_uri.set(tunnel_callback);
                spotify_local_setup_saved.set(false);
                // Deliberately NOT `spotify_resave_needed.set(true)` here:
                // this branch also runs when Settings reloads while a tunnel
                // whose address was already saved is still live (the field
                // rehydrates to the loopback default and re-aligns to the
                // tunnel), and forcing the stepper back to Save there would
                // reopen step 3 on every mid-setup reload (#597). The one
                // real stale case -- configured under the loopback default,
                // then a tunnel started without re-saving -- is refused
                // server-side by oauth/start's tunnel_redirect_mismatch
                // guard, which renders inside the Connect step.
            }
        }
    });

    // While a tunnel is active, keep its status fresh (10s cadence): the
    // expiry countdown stays honest, the reachability self-check's result
    // arrives without a reload, and a tunnel that dies early (relay drop,
    // ssh exit) surfaces here as an error banner instead of the user finding
    // out from Spotify's dead redirect (#592).
    use_effect(move || {
        if !spotify_tunnel().is_active() || *spotify_tunnel_polling.peek() {
            return;
        }
        spotify_tunnel_polling.set(true);
        spawn(async move {
            loop {
                dioxus_sdk_time::sleep(std::time::Duration::from_secs(10)).await;
                match crate::app::api::fetch_json::<SpotifyTunnelStatus>(
                    "/api/providers/spotify/tunnel/status",
                )
                .await
                {
                    Ok(status) => {
                        let still_active = status.is_active();
                        spotify_tunnel.set(status);
                        if !still_active {
                            break;
                        }
                    }
                    Err(_) => break,
                }
            }
            spotify_tunnel_polling.set(false);
        });
    });

    use_effect(move || {
        if let Some(Some(status)) = mqtt_status.read().as_ref() {
            if let Some(host) = status.host.as_ref() {
                mqtt_host.set(host.clone());
            }
            if let Some(port) = status.port {
                mqtt_port.set(port.to_string());
            }
            if let Some(tls) = status.tls {
                mqtt_tls.set(tls);
            }
            if let Some(base_topic) = status.base_topic.as_ref() {
                mqtt_base_topic.set(base_topic.clone());
            }
            if let Some(discovery_prefix) = status.discovery_prefix.as_ref() {
                mqtt_discovery_prefix.set(discovery_prefix.clone());
            }
        }
    });

    // Refresh discovery on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_discovery() {
            roon_status.restart();
            openhome_status.restart();
            upnp_status.restart();
            lms_config.restart();
            hqp_status.restart();
            provider_zones.restart();
            spotify_account.restart();
            musicassistant_status.restart();
            apple_bridge_status.restart();
            mqtt_status.restart();
        }
        // The zone set changes while the user is on this page -- they power on a speaker precisely
        // because they are configuring it. Without this the table only updates on a manual reload.
        if sse.should_refresh_zones() {
            managed_zones.restart();
        }
    });

    // A companion creates its pairing request through the Bonjour discovery
    // endpoint. That request does not produce a playback SSE event, so keep
    // the Settings view's pending-code display fresh while Apple Music is
    // enabled. This also covers a companion being opened on another device
    // after this page has loaded.
    use_effect(move || {
        if !applemusic_enabled() {
            return;
        }
        let enabled = applemusic_enabled;
        spawn(async move {
            loop {
                dioxus_sdk_time::sleep(std::time::Duration::from_secs(2)).await;
                if !enabled() {
                    break;
                }
                apple_bridge_status.restart();
            }
        });
    });

    let start_spotify_oauth = move |_| {
        spotify_action_scope.set(4);
        spotify_action.set(ProviderActionState::Loading);
        spotify_error.set(None);
        spawn(async move {
            match crate::app::api::fetch_json::<ProviderOAuthStart>(
                "/api/providers/spotify/oauth/start",
            )
            .await
            {
                Ok(response) if !response.authorization_url.is_empty() => {
                    if let Err(error) = redirect_to(&response.authorization_url) {
                        spotify_action.set(ProviderActionState::Failed);
                        spotify_error.set(Some(error));
                    }
                }
                Ok(_) => {
                    spotify_action.set(ProviderActionState::Failed);
                    spotify_error.set(Some(
                        "Spotify did not return an authorization URL.".to_string(),
                    ));
                }
                Err(error) => {
                    spotify_action.set(ProviderActionState::Failed);
                    // A `controller_unauthorized` 401 already opened the
                    // bootstrap prompt (#570); don't also show its raw text.
                    spotify_error.set(crate::app::api::suppress_controller_unauthorized(error));
                }
            }
        });
    };

    let disconnect_spotify = move |_| {
        spotify_action_scope.set(5);
        spotify_action.set(ProviderActionState::Loading);
        spotify_error.set(None);
        spawn(async move {
            match crate::app::api::post_json::<serde_json::Value, ProviderAuthResponse>(
                "/api/providers/spotify/oauth/revoke",
                &serde_json::json!({}),
            )
            .await
            {
                Ok(_) => {
                    spotify_action.set(ProviderActionState::Success);
                    provider_zones.restart();
                }
                Err(error) => {
                    spotify_action.set(ProviderActionState::Failed);
                    spotify_error.set(crate::app::api::suppress_controller_unauthorized(error));
                }
            }
        });
    };

    let save_spotify_local = move |_| {
        spotify_action_scope.set(3);
        let client_id = spotify_client_id().trim().to_string();
        let client_secret = spotify_client_secret().trim().to_string();
        let redirect_uri = spotify_redirect_uri().trim().to_string();
        if client_id.is_empty() {
            spotify_action.set(ProviderActionState::Failed);
            spotify_error.set(Some("Enter your Client ID first.".to_string()));
            return;
        }
        spotify_action.set(ProviderActionState::Loading);
        spotify_error.set(None);
        spawn(async move {
            let request = SpotifyConfigureRequest {
                client_id,
                client_secret: (!client_secret.is_empty()).then_some(client_secret),
                redirect_uri: (!redirect_uri.is_empty()).then_some(redirect_uri),
            };
            match crate::app::api::post_json::<SpotifyConfigureRequest, SpotifyConfigureResponse>(
                "/api/providers/spotify/configure",
                &request,
            )
            .await
            {
                Ok(response) if response.configured => {
                    spotify_action.set(ProviderActionState::Success);
                    spotify_local_setup_saved.set(true);
                    spotify_resave_needed.set(false);
                    spotify_reopen_step.set(None);
                    // Saving collapses the editor; land the user on the next
                    // step (Connect) instead of wherever the browser anchors.
                    scroll_to_element("spotify-connect-button");
                }
                Ok(_) => {
                    spotify_action.set(ProviderActionState::Failed);
                    spotify_error.set(Some("Spotify configuration was not accepted.".to_string()));
                }
                Err(error) => {
                    spotify_action.set(ProviderActionState::Failed);
                    spotify_error.set(crate::app::api::suppress_controller_unauthorized(error));
                }
            }
        });
    };

    // Starting the tunnel kicks off a background `ssh` process server-side
    // and returns immediately with a "starting" status; poll until it
    // settles into "active" or "error" so the button and URL update without
    // a page reload.
    let start_spotify_tunnel = move |_| {
        spotify_tunnel_busy.set(true);
        spawn(async move {
            match crate::app::api::post_json::<serde_json::Value, SpotifyTunnelStatus>(
                "/api/providers/spotify/tunnel/start",
                &serde_json::json!({}),
            )
            .await
            {
                Ok(status) => {
                    let starting = status.is_starting();
                    spotify_tunnel.set(status);
                    if starting {
                        loop {
                            dioxus_sdk_time::sleep(std::time::Duration::from_millis(1500)).await;
                            match crate::app::api::fetch_json::<SpotifyTunnelStatus>(
                                "/api/providers/spotify/tunnel/status",
                            )
                            .await
                            {
                                Ok(status) => {
                                    let still_starting = status.is_starting();
                                    spotify_tunnel.set(status);
                                    if !still_starting {
                                        break;
                                    }
                                }
                                Err(_) => break,
                            }
                        }
                    }
                }
                Err(error) => {
                    // A `controller_unauthorized` 401 already opened the
                    // bootstrap prompt (#570); leave the tunnel panel idle
                    // rather than also reporting a (confusingly permanent
                    // sounding) tunnel error banner underneath it.
                    if let Some(error) = crate::app::api::suppress_controller_unauthorized(error) {
                        spotify_tunnel.set(SpotifyTunnelStatus {
                            phase: "error".to_string(),
                            message: Some(error),
                            ..Default::default()
                        });
                    }
                }
            }
            spotify_tunnel_busy.set(false);
        });
    };

    let stop_spotify_tunnel = move |_| {
        spotify_tunnel_busy.set(true);
        spawn(async move {
            if let Ok(status) =
                crate::app::api::post_json::<serde_json::Value, SpotifyTunnelStatus>(
                    "/api/providers/spotify/tunnel/stop",
                    &serde_json::json!({}),
                )
                .await
            {
                spotify_tunnel.set(status);
            }
            spotify_tunnel_busy.set(false);
        });
    };

    let refresh_providers = move |_| {
        provider_zones.restart();
        spotify_account.restart();
        apple_bridge_status.restart();
        musicassistant_status.restart();
    };

    let save_musicassistant = move |_| {
        let host = musicassistant_host().trim().to_string();
        let port = musicassistant_port().trim().parse::<u16>().unwrap_or(0);
        let token = musicassistant_token().trim().to_string();
        if host.is_empty() || port == 0 {
            musicassistant_action.set(ProviderActionState::Failed);
            musicassistant_error.set(Some(
                "Enter a Music Assistant host and valid port.".to_string(),
            ));
            return;
        }
        musicassistant_action.set(ProviderActionState::Loading);
        musicassistant_error.set(None);
        let tls = musicassistant_tls();
        let allow_insecure_http = musicassistant_insecure_http();
        spawn(async move {
            let request = MusicAssistantConfigureRequest {
                host,
                port,
                token: (!token.is_empty()).then_some(token),
                tls,
                allow_insecure_http,
            };
            match crate::app::api::post_json::<MusicAssistantConfigureRequest, serde_json::Value>(
                "/api/providers/musicassistant/configure",
                &request,
            )
            .await
            {
                Ok(_) => {
                    musicassistant_action.set(ProviderActionState::Success);
                    musicassistant_token.set(String::new());
                    musicassistant_status.restart();
                    provider_zones.restart();
                }
                Err(error) => {
                    musicassistant_action.set(ProviderActionState::Failed);
                    musicassistant_error
                        .set(crate::app::api::suppress_controller_unauthorized(error));
                }
            }
        });
    };

    let save_mqtt = move |_| {
        let host = mqtt_host().trim().to_string();
        if host.is_empty() {
            mqtt_action.set(ProviderActionState::Failed);
            mqtt_error.set(Some("Enter an MQTT broker host.".to_string()));
            return;
        }
        let port = mqtt_port().trim().parse::<u16>().ok();
        let username = mqtt_username().trim().to_string();
        let password = mqtt_password().trim().to_string();
        let base_topic = mqtt_base_topic().trim().to_string();
        let discovery_prefix = mqtt_discovery_prefix().trim().to_string();
        let tls = mqtt_tls();
        mqtt_action.set(ProviderActionState::Loading);
        mqtt_error.set(None);
        spawn(async move {
            let request = MqttConfigureRequest {
                host,
                port,
                tls,
                username: (!username.is_empty()).then_some(username),
                password: (!password.is_empty()).then_some(password),
                base_topic: (!base_topic.is_empty()).then_some(base_topic),
                discovery_prefix: (!discovery_prefix.is_empty()).then_some(discovery_prefix),
            };
            match crate::app::api::post_json::<MqttConfigureRequest, serde_json::Value>(
                "/api/mqtt/configure",
                &request,
            )
            .await
            {
                Ok(_) => {
                    mqtt_action.set(ProviderActionState::Success);
                    mqtt_password.set(String::new());
                    mqtt_status.restart();
                }
                Err(error) => {
                    mqtt_action.set(ProviderActionState::Failed);
                    mqtt_error.set(Some(error));
                }
            }
        });
    };

    let toggle_setting = move |toggle: SettingsToggle| {
        let Some(settings) = confirmed_settings() else {
            return;
        };
        persist_settings_then_reload(settings_with_toggle(settings, toggle));
    };

    let roon_st = roon_status.read().clone().flatten();
    let openhome_st = openhome_status.read().clone().flatten();
    let upnp_st = upnp_status.read().clone().flatten();
    let lms_cfg = lms_config.read().clone().flatten();
    let hqp_st = hqp_status.read().clone().flatten();
    let provider_zones_result = provider_zones.read().clone();
    let spotify_account_result = spotify_account.read().clone().flatten();
    let spotify_devices = provider_zones_result
        .as_ref()
        .and_then(|result| result.as_ref().ok())
        .map(|zones| {
            zones
                .iter()
                .filter(|zone| zone.zone_id.starts_with("spotify:"))
                .cloned()
                .collect::<Vec<_>>()
        });
    let spotify_connected = spotify_account_result
        .as_ref()
        .and_then(|response| response.account.as_ref())
        .is_some();
    let spotify_configured = spotify_account_result
        .as_ref()
        .map(|response| response.configured)
        .unwrap_or(false)
        || spotify_connected
        || spotify_local_setup_saved();
    let apple_st = apple_bridge_status.read().clone().and_then(Result::ok);
    let apple_music_state = apple_music_status_state(apple_st.as_ref());
    let apple_music_live_count = apple_music_live_companion_count(apple_st.as_ref());
    // Snapshot the OAuth-return query once, on the first client render: the
    // router normalizes the URL (dropping `?spotify=...&reason=...`) shortly
    // after hydration, so re-reading `location.search` every render made the
    // feedback vanish as soon as any signal re-rendered the page (#597 probe
    // finding). SSR still renders no feedback (the non-wasm helper returns
    // None), and only `hidden` attributes differ on hydration.
    // Captured in a mount-only effect -- not a signal initializer -- so the
    // read happens strictly in the hydrated client, matching the
    // remote-origin pattern above (#570): no tracked signal is read inside,
    // so it runs exactly once (reactive-loop-lint).
    let mut spotify_callback_snapshot = use_signal(|| None::<CallbackFeedback>);
    use_effect(move || {
        if let Some(feedback) = callback_feedback() {
            spotify_callback_snapshot.set(Some(feedback));
        }
    });
    let callback_message = spotify_callback_snapshot();
    let spotify_status_is_error = spotify_error().is_some();
    let spotify_status_message =
        spotify_error().or_else(|| spotify_action().message().map(str::to_string));
    let spotify_tunnel_status = spotify_tunnel();
    let spotify_tunnel_provider_label = spotify_tunnel_status
        .provider
        .clone()
        .unwrap_or_else(|| "a public relay".to_string());
    let spotify_tunnel_error_message = spotify_tunnel_status
        .is_error()
        .then(|| spotify_tunnel_status.message.clone())
        .flatten();
    let spotify_tunnel_minutes_remaining = spotify_tunnel_status
        .seconds_remaining
        .map(|secs| (secs / 60).max(1));
    // The tunnel's own callback address, straight from the live tunnel
    // status -- deliberately NOT the Redirect URI field, which the user may
    // have registered with Spotify under an earlier tunnel URL.
    let spotify_tunnel_callback = spotify_tunnel_status
        .url
        .clone()
        .map(|url| format!("{url}/api/providers/spotify/oauth/callback"))
        .unwrap_or_default();
    let spotify_tunnel_callback_for_copy = spotify_tunnel_callback.clone();
    let spotify_tunnel_callback_for_use = spotify_tunnel_callback.clone();
    let spotify_tunnel_verified = spotify_tunnel_status.verified;
    // The live tunnel's callback address no longer matches the Redirect URI
    // field: surfaced as a warning instead of silently rewriting the field
    // (#592), since the old address may already be registered with Spotify.
    // Render-derived (not a stored signal) so hand-editing the field
    // recomputes it immediately.
    let spotify_tunnel_redirect_value = spotify_redirect_uri();
    let spotify_tunnel_mismatch = spotify_tunnel_status.url.is_some()
        && spotify_tunnel_redirect_value != spotify_tunnel_callback
        && !spotify_tunnel_redirect_value.trim().is_empty()
        && spotify_tunnel_redirect_value != default_spotify_redirect_uri();
    let spotify_account = spotify_account_result
        .as_ref()
        .and_then(|response| response.account.as_ref());
    let spotify_account_display = spotify_account
        .and_then(|account| {
            account
                .display_name
                .as_deref()
                .filter(|name| !name.is_empty())
        })
        .or_else(|| spotify_account.map(|account| account.id.as_str()))
        .unwrap_or("Not connected");
    let spotify_account_id = spotify_account
        .map(|account| account.id.as_str())
        .unwrap_or("");
    let spotify_account_email = spotify_account
        .and_then(|account| account.email.as_deref().filter(|email| !email.is_empty()))
        .unwrap_or("Email unavailable; reconnect Spotify to grant profile access.");
    let spotify_account_error = spotify_account_result
        .as_ref()
        .and_then(|response| response.error.as_deref());
    let spotify_account_error_hint = spotify_account_error
        .map(|error| error.contains("not registered for this application"))
        .unwrap_or(false);

    // Spotify onboarding stepper state (#597). Steps 3 and 4 come from
    // server state; steps 1 and 2 are dashboard-side actions UHC cannot
    // observe, so they complete via a one-tap attestation -- or implicitly
    // once the server is configured (a saved config proves the user got
    // through the dashboard). A live tunnel whose address no longer matches
    // the redirect URI reopens step 2 so the warning is never hidden inside
    // a collapsed step.
    let spotify_step1_complete = spotify_configured || spotify_step_app_done();
    let spotify_step2_complete =
        (spotify_configured || spotify_step_callback_done()) && !spotify_tunnel_mismatch;
    let spotify_step3_complete = spotify_configured && !spotify_resave_needed();
    let spotify_step4_complete = spotify_connected;
    let spotify_current_step: u8 = if !spotify_step1_complete {
        1
    } else if !spotify_step2_complete {
        2
    } else if !spotify_step3_complete {
        3
    } else if !spotify_step4_complete {
        4
    } else {
        5
    };
    let spotify_reopened_step = spotify_reopen_step();
    let spotify_step1_open = spotify_current_step == 1 || spotify_reopened_step == Some(1);
    let spotify_step2_open = spotify_current_step == 2 || spotify_reopened_step == Some(2);
    let spotify_step3_open = spotify_current_step == 3 || spotify_reopened_step == Some(3);
    let spotify_step4_open = spotify_current_step == 4 || spotify_reopened_step == Some(4);
    let spotify_info_pinned = spotify_info_open();
    let spotify_scope = spotify_action_scope();
    let spotify_step_class = |n: u8, complete: bool| {
        if spotify_current_step == n {
            "relative rounded-md border border-default bg-elevated p-4"
        } else if complete {
            "relative rounded-md border border-default p-4"
        } else {
            "relative rounded-md border border-default p-4 opacity-60"
        }
    };
    let spotify_info_panel_class = |n: u8| {
        if spotify_info_pinned == Some(n) {
            SPOTIFY_INFO_PANEL_OPEN
        } else {
            SPOTIFY_INFO_PANEL_CLOSED
        }
    };
    let mqtt_info_panel_class = if mqtt_info_open() {
        MQTT_INFO_PANEL_OPEN
    } else {
        MQTT_INFO_PANEL_CLOSED
    };
    // The Home Assistant add-on hands the broker over from the Supervisor
    // (#605). When it has, there is nothing here for the user to fill in, so
    // the manual form collapses behind an opt-in rather than presenting
    // itself as the setup step.
    let mqtt_env_managed = mqtt_status
        .read()
        .clone()
        .flatten()
        .is_some_and(|status| status.is_environment_managed());
    let mqtt_show_manual_form = !mqtt_env_managed || mqtt_manual_open();

    rsx! {
        Layout {
            title: "Settings".to_string(),
            nav_active: "settings".to_string(),
            // Disabled adapters must not leak dead top-level tabs while the
            // browser finishes loading its Settings context.
            hide_hqp: !hqplayer_enabled(),
            hide_lms: !lms_enabled(),
            hide_spotify: !spotify_enabled(),
            hide_knobs: hide_knobs(),

            h1 { class: "text-2xl font-bold mb-6", "Settings" }

            // Features section (adapters + page visibility)
            section { class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Features" }
                    p { class: "text-muted text-sm", "Zone sources and page visibility" }
                }

                div { class: "card overflow-x-auto p-4 sm:p-6",
                    table { class: "w-full", id: "features-table",
                        thead {
                            tr { class: "border-b border-default",
                                th { class: "text-left py-2 px-3 font-semibold w-12", "" }
                                th { class: "text-left py-2 px-3 font-semibold", "Feature" }
                                th { class: "text-left py-2 px-3 font-semibold", "Status" }
                            }
                        }
                        tbody {
                            // Roon (adapter only, no dedicated page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable Roon",
                                        enabled: roon_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::Roon, !roon_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Roon" }
                                td { class: "py-2 px-3",
                                    if roon_enabled() {
                                        if let Some(ref status) = roon_st {
                                            if status.connected {
                                                if let Some(ref name) = status.core_name {
                                                    span { class: "status-ok", "✓ {name}" }
                                                } else {
                                                    span { class: "status-ok", "✓ Core" }
                                                }
                                            } else {
                                                span { class: "status-err", "✗ Not connected" }
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // OpenHome (adapter only, no dedicated page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable OpenHome",
                                        enabled: openhome_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::OpenHome, !openhome_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "OpenHome" }
                                td { class: "py-2 px-3",
                                    if openhome_enabled() {
                                        if let Some(ref status) = openhome_st {
                                            if status.device_count > 0 {
                                                span { class: "status-ok", "✓ {status.device_count} devices" }
                                            } else {
                                                "Searching..."
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // UPnP/DLNA
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable UPnP/DLNA",
                                        enabled: upnp_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::Upnp, !upnp_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "UPnP/DLNA" }
                                td { class: "py-2 px-3",
                                    if upnp_enabled() {
                                        if let Some(ref status) = upnp_st {
                                            if status.renderer_count > 0 {
                                                span { class: "status-ok", "✓ {status.renderer_count} renderers" }
                                            } else {
                                                "Searching..."
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // LMS (adapter + page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable LMS",
                                        enabled: lms_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::Lms, !lms_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "LMS" }
                                td { class: "py-2 px-3",
                                    if lms_enabled() {
                                        if let Some(ref cfg) = lms_cfg {
                                            if cfg.connected {
                                                if cfg.cli_subscription_active {
                                                    span { class: "status-ok", "✓ CLI" }
                                                } else {
                                                    span { class: "text-yellow-500", "⚠ Polling" }
                                                }
                                            } else {
                                                span { class: "status-err", "✗ Not connected" }
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // HQPlayer (adapter + page)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable HQPlayer",
                                        enabled: hqplayer_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::HqPlayer, !hqplayer_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "HQPlayer" }
                                td { class: "py-2 px-3",
                                    if hqplayer_enabled() {
                                        if let Some(ref status) = hqp_st {
                                            if status.connected {
                                                span { class: "status-ok", "✓ Connected" }
                                            } else {
                                                span { class: "status-err", "✗ Not connected" }
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // Music Assistant is an optional outbound peer adapter.
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable Music Assistant",
                                        enabled: musicassistant_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::MusicAssistant, !musicassistant_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3",
                                    div { class: "flex items-center gap-2",
                                        "Music Assistant"
                                        span { class: "badge badge-secondary", "Alpha" }
                                    }
                                }
                                td { class: "py-2 px-3",
                                    if musicassistant_enabled() {
                                        if let Some(status) = musicassistant_status.read().clone().flatten() {
                                            if status.running {
                                                span { class: "status-ok", "✓ Connected" }
                                            } else if status.configured {
                                                span { class: "text-yellow-500", "Configured · waiting" }
                                            } else {
                                                span { class: "text-muted", "Setup required" }
                                            }
                                        } else { "..." }
                                    } else { span { class: "text-muted", "-" } }
                                }
                            }
                            // Knobs (page only, no adapter)
                            // Spotify (controller adapter; zones arrive through the bus)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable Spotify",
                                        enabled: spotify_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::Spotify, !spotify_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3",
                                    div { class: "flex items-center gap-2",
                                        "Spotify"
                                        span { class: "badge badge-secondary", "Alpha" }
                                    }
                                }
                                td { class: "py-2 px-3",
                                    if spotify_enabled() {
                                        if let Some(ref devices) = spotify_devices {
                                            if devices.is_empty() {
                                                span { class: "status-err", "✗ No Connect devices" }
                                            } else {
                                                span { class: "status-ok", "✓ {devices.len()} device(s)" }
                                            }
                                        } else {
                                            "..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // Apple Music (native companion adapter; zones arrive through the bus)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable Apple Music",
                                        enabled: applemusic_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::AppleMusic, !applemusic_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3",
                                    div { class: "flex items-center gap-2",
                                        "Apple Music"
                                        span { class: "badge badge-secondary", "Alpha" }
                                    }
                                }
                                td { class: "py-2 px-3",
                                    if applemusic_enabled() {
                                        if apple_st.is_some() {
                                            if matches!(apple_music_state, AppleMusicStatusState::Live) {
                                                span { class: "status-ok", "✓ Companion live" }
                                            } else if matches!(apple_music_state, AppleMusicStatusState::PairedWaiting) {
                                                span { class: "text-yellow-500", "⚠ Paired · waiting" }
                                            } else {
                                                span { class: "text-muted", "Not paired" }
                                            }
                                        } else {
                                            "Checking..."
                                        }
                                    } else {
                                        span { class: "text-muted", "-" }
                                    }
                                }
                            }
                            // MQTT/Home Assistant discovery publisher (bus consumer, not a zone adapter)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Enable MQTT/Home Assistant",
                                        enabled: mqtt_enabled(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::Adapter(AdapterToggle::Mqtt, !mqtt_enabled()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3",
                                    div { class: "flex items-center gap-2",
                                        "MQTT/Home Assistant"
                                        span { class: "badge badge-secondary", "Alpha" }
                                    }
                                }
                                // The broker form lives in its own section far
                                // below this table, so a user who flips the
                                // toggle with no broker saved would otherwise
                                // get silence. Say what is missing here, and
                                // offer the jump (#605).
                                td { class: "py-2 px-3",
                                    if mqtt_enabled() {
                                        if let Some(status) = mqtt_status.read().clone().flatten() {
                                            if status.running {
                                                if status.is_environment_managed() {
                                                    span { class: "status-ok", "✓ Connected · via add-on" }
                                                } else {
                                                    span { class: "status-ok", "✓ Connected" }
                                                }
                                            } else if status.configured {
                                                span { class: "text-yellow-500", "Configured · waiting" }
                                            } else {
                                                button {
                                                    r#type: "button",
                                                    class: "text-yellow-500 underline",
                                                    onclick: move |_| scroll_to_element("mqtt-anchor"),
                                                    "No broker yet — set one up"
                                                }
                                            }
                                        } else { "..." }
                                    } else {
                                        span { class: "text-muted", "Off — no Home Assistant entities" }
                                    }
                                }
                            }
                            // Controllers (page only, no adapter)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    FeatureToggle {
                                        label: "Show Controllers page",
                                        enabled: !hide_knobs(),
                                        onclick: move |_| {
                                            toggle_setting(SettingsToggle::HideKnobs(hide_knobs()));
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Controllers" }
                                td { class: "py-2 px-3 text-muted", "-" }
                            }
                        }
                }
            }

            div { id: "streaming-providers-anchor",
                section {
                    class: "mb-8",
                    hidden: !(spotify_enabled() || applemusic_enabled() || musicassistant_enabled()),
                    aria_labelledby: "streaming-heading",
                div { class: "mb-4",
                    h2 { id: "streaming-heading", class: "text-xl font-semibold", "Streaming providers" }
                    p { class: "text-muted text-sm", "Connect providers without sharing credentials with the browser." }
                }

                // Provider cards occupy the full settings column. Their own
                // setup panes remain responsive two-column layouts; keeping
                // the outer grid single-column prevents Spotify's configuration
                // form from being needlessly squeezed beside another provider.
                div { class: "grid gap-4",
                    // Keep authorization and client settings as separate
                    // actions so the credential boundary stays explicit.
                    div {
                        class: "card p-5 sm:p-6",
                        hidden: !spotify_enabled(),
                        aria_labelledby: "spotify-heading",
                        div { class: "flex flex-col gap-3 sm:flex-row sm:items-start sm:justify-between",
                            div {
                                h3 { id: "spotify-heading", class: "text-lg font-semibold flex items-center gap-2",
                                    "Spotify Connect"
                                    span { class: "badge badge-secondary", "Alpha" }
                                }
                                p { class: "mt-1 text-sm text-secondary", "Control existing Spotify Connect devices. UHC does not act as a receiver." }
                                div {
                                    class: "mt-2",
                                    hidden: spotify_account.is_none(),
                                    aria_hidden: spotify_account.is_none(),
                                    p { class: "text-sm text-secondary", "Signed in as "
                                        strong { class: "text-primary", "{spotify_account_display}" }
                                    }
                                    details { class: "mt-1 text-xs text-muted",
                                        summary { class: "cursor-pointer", "Show account details" }
                                        div { class: "mt-1 space-y-0.5",
                                            p { "Spotify ID: {spotify_account_id}" }
                                            p { "Email: {spotify_account_email}" }
                                        }
                                    }
                                }
                                div {
                                    class: "mt-3",
                                    hidden: spotify_account_error.is_none(),
                                    aria_hidden: spotify_account_error.is_none(),
                                    p { class: "status-err", role: "alert", "Spotify could not refresh this connection: {spotify_account_error.unwrap_or_default()}" }
                                    p {
                                        class: "mt-2 text-sm text-secondary",
                                        hidden: !spotify_account_error_hint,
                                        "Add the Spotify account email to this app's Development-mode Users list, then reconnect."
                                    }
                                }
                            }
                            if !spotify_enabled() {
                                span { class: "badge badge-secondary shrink-0", "Disabled" }
                            } else if spotify_connected {
                                span { class: "badge badge-success shrink-0", "Connected" }
                            } else if spotify_configured {
                                span { class: "badge badge-secondary shrink-0", "Configured" }
                            } else {
                                span { class: "badge badge-secondary shrink-0", "Setup required" }
                            }
                        }

                        // Success feedback from the Spotify return trip stays a
                        // card-level status line; error feedback renders inside
                        // the Connect step (#597: errors live in their step).
                        p {
                            class: "mt-4 status-ok",
                            hidden: callback_message.as_ref().is_none_or(|feedback| feedback.is_error),
                            aria_hidden: callback_message.as_ref().is_none_or(|feedback| feedback.is_error),
                            role: "status",
                            aria_live: "polite",
                            "{callback_message.as_ref().filter(|feedback| !feedback.is_error).map(|feedback| feedback.message.as_str()).unwrap_or_default()}"
                        }

                        // Onboarding stepper (#597): exactly one step's action is
                        // visible at a time; completed steps collapse to a checked
                        // line that reopens on click; upcoming steps stay dimmed
                        // and inert. Every step body stays mounted and toggles
                        // `hidden` so hydration and event handlers never shift and
                        // step transitions cause no scroll jumps.
                        ol { class: "mt-5 grid list-none gap-3 p-0", aria_label: "Spotify setup steps",

                            // Step 1 -- create the Spotify app (dashboard-side; completes
                            // via attestation or implicitly once the server is configured).
                            li { class: spotify_step_class(1, spotify_step1_complete),
                                div { class: "flex items-start gap-3",
                                    span {
                                        class: if spotify_step1_complete { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs status-ok" } else { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs text-muted" },
                                        aria_hidden: true,
                                        if spotify_step1_complete { "✓" } else { "1" }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "flex min-w-0 flex-1 cursor-pointer flex-col items-start text-left disabled:cursor-default",
                                        disabled: !spotify_step1_complete,
                                        aria_expanded: spotify_step1_open,
                                        aria_controls: "spotify-step-1-body",
                                        onclick: move |_| {
                                            let reopened = *spotify_reopen_step.peek();
                                            spotify_reopen_step.set(if reopened == Some(1) { None } else { Some(1) });
                                        },
                                        span { class: "font-medium", "Create a Spotify app" }
                                        span {
                                            class: "text-xs text-muted",
                                            hidden: !spotify_step1_complete || spotify_step1_open,
                                            "Done -- using your Spotify app"
                                        }
                                    }
                                    span { class: "group relative shrink-0",
                                        button {
                                            r#type: "button",
                                            class: "flex h-8 w-8 items-center justify-center rounded-full text-muted",
                                            aria_expanded: spotify_info_pinned == Some(1),
                                            aria_controls: "spotify-step-1-info",
                                            aria_label: "More about creating a Spotify app",
                                            onclick: move |_| {
                                                let pinned = *spotify_info_open.peek();
                                                spotify_info_open.set(if pinned == Some(1) { None } else { Some(1) });
                                            },
                                            "ⓘ"
                                        }
                                        div { id: "spotify-step-1-info", class: spotify_info_panel_class(1), role: "note",
                                            p { "Spotify only lets an app you own control your players, so you create a free one to act as your personal key. In the dashboard, choose \"Create app\", give it any name, and accept Spotify's terms. Nothing gets published anywhere." }
                                            p { class: "mt-2", "New apps run in development mode: the Spotify accounts allowed to sign in must be listed on the app's User Management page." }
                                        }
                                    }
                                }
                                div {
                                    id: "spotify-step-1-body",
                                    hidden: !spotify_step1_open,
                                    aria_hidden: !spotify_step1_open,
                                    p { class: "mt-2 text-sm text-secondary", "Create a free app in Spotify's developer dashboard -- it takes about a minute." }
                                    div { class: "mt-3 flex flex-wrap gap-2",
                                        a {
                                            class: "btn btn-primary min-h-11 inline-flex items-center",
                                            href: "https://developer.spotify.com/dashboard",
                                            target: "_blank",
                                            rel: "noopener noreferrer",
                                            "Open the Spotify dashboard"
                                        }
                                        button {
                                            r#type: "button",
                                            class: "btn btn-outline min-h-11",
                                            onclick: move |_| {
                                                spotify_step_app_done.set(true);
                                                spotify_reopen_step.set(None);
                                            },
                                            "I have an app"
                                        }
                                    }
                                }
                            }

                            // Step 2 -- register the callback address. The tunnel
                            // button is the primary path when the browser is not on
                            // the UHC machine; manual entry lives under Advanced.
                            li { class: spotify_step_class(2, spotify_step2_complete),
                                div { class: "flex items-start gap-3",
                                    span {
                                        class: if spotify_step2_complete { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs status-ok" } else { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs text-muted" },
                                        aria_hidden: true,
                                        if spotify_step2_complete { "✓" } else { "2" }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "flex min-w-0 flex-1 cursor-pointer flex-col items-start text-left disabled:cursor-default",
                                        disabled: !spotify_step2_complete,
                                        aria_expanded: spotify_step2_open,
                                        aria_controls: "spotify-step-2-body",
                                        onclick: move |_| {
                                            let reopened = *spotify_reopen_step.peek();
                                            spotify_reopen_step.set(if reopened == Some(2) { None } else { Some(2) });
                                        },
                                        span { class: "font-medium", "Tell Spotify where to send you back" }
                                        span {
                                            class: "text-xs text-muted",
                                            hidden: !spotify_step2_complete || spotify_step2_open,
                                            "Done -- callback address registered"
                                        }
                                    }
                                    span { class: "group relative shrink-0",
                                        button {
                                            r#type: "button",
                                            class: "flex h-8 w-8 items-center justify-center rounded-full text-muted",
                                            aria_expanded: spotify_info_pinned == Some(2),
                                            aria_controls: "spotify-step-2-info",
                                            aria_label: "More about the callback address",
                                            onclick: move |_| {
                                                let pinned = *spotify_info_open.peek();
                                                spotify_info_open.set(if pinned == Some(2) { None } else { Some(2) });
                                            },
                                            "ⓘ"
                                        }
                                        div { id: "spotify-step-2-info", class: spotify_info_panel_class(2), role: "note",
                                            p { "After you approve access, Spotify sends your browser back to UHC -- but only to an address on your app's \"Redirect URIs\" list, and (except on the exact machine running UHC) only to a secure https address." }
                                            p { class: "mt-2", "The address UHC opens for you stays up for about 15 minutes and closes itself once you finish connecting. Connecting again later issues a new address, which then needs to be added to your Spotify app too." }
                                        }
                                    }
                                }
                                div {
                                    id: "spotify-step-2-body",
                                    hidden: !spotify_step2_open,
                                    aria_hidden: !spotify_step2_open,
                                    if spotify_remote_origin() {
                                        // Remote/LAN origin -- the common case (a NAS or any
                                        // machine other than the one running the browser).
                                        // Spotify rejects plain-HTTP addresses here, so the
                                        // tunnel button is THE action (#570/#592).
                                        div {
                                            p { class: "mt-2 text-sm text-secondary", "Get a secure address for this server, then paste it into your Spotify app under \"Redirect URIs\"." }
                                            if spotify_tunnel_status.is_active() {
                                                div { class: "mt-2 rounded-md border border-default bg-hover p-3",
                                                    div { class: "flex flex-col gap-2 sm:flex-row sm:items-stretch",
                                                        code {
                                                            id: "spotify-callback-url-display",
                                                            class: "block min-w-0 flex-1 overflow-x-auto break-all rounded-md bg-elevated px-3 py-3 text-xs select-all",
                                                            "{spotify_tunnel_callback}"
                                                        }
                                                        button {
                                                            r#type: "button",
                                                            class: "btn btn-outline btn-sm shrink-0",
                                                            aria_label: "Copy tunnel callback URL",
                                                            onclick: move |_| copy_to_clipboard(spotify_tunnel_callback_for_copy.clone(), spotify_tunnel_url_copy_state),
                                                            span { aria_live: "polite", "{spotify_tunnel_url_copy_state().label(\"Copy URL\")}" }
                                                        }
                                                    }
                                                    match spotify_tunnel_verified {
                                                        Some(true) => rsx! {
                                                            p { class: "mt-2 text-xs status-ok", "✓ Checked: this address answers from the internet." }
                                                        },
                                                        Some(false) => rsx! {
                                                            p { class: "mt-2 text-xs status-err", role: "alert",
                                                                "This address did not answer when UHC checked it from the internet. Stop it and try again before adding it to Spotify."
                                                            }
                                                        },
                                                        None => rsx! {
                                                            p { class: "mt-2 text-xs text-muted", "Checking that this address answers from the internet…" }
                                                        },
                                                    }
                                                    if spotify_tunnel_mismatch {
                                                        div { class: "mt-2 rounded-md border border-default p-2", role: "alert",
                                                            p { class: "text-xs status-err",
                                                                "Your secure address changed and no longer matches the one saved below. Add this new address to your Spotify app, then apply it here and save again before connecting."
                                                            }
                                                            button {
                                                                r#type: "button",
                                                                class: "btn btn-outline btn-sm mt-2",
                                                                onclick: move |_| {
                                                                    spotify_redirect_uri.set(spotify_tunnel_callback_for_use.clone());
                                                                    spotify_local_setup_saved.set(false);
                                                                    spotify_resave_needed.set(true);
                                                                },
                                                                "Use this address"
                                                            }
                                                        }
                                                    }
                                                    if let Some(minutes) = spotify_tunnel_minutes_remaining {
                                                        p { class: "mt-2 text-xs text-muted",
                                                            "Closes by itself in about {minutes} minute(s), or right after you connect."
                                                        }
                                                    }
                                                    p { class: "mt-2 text-xs text-muted", "Live via {spotify_tunnel_provider_label}. While open, this server is briefly reachable from the internet at that address; only the in-progress Spotify sign-in is accepted through it." }
                                                    button {
                                                        r#type: "button",
                                                        class: "btn btn-ghost btn-sm mt-2",
                                                        disabled: spotify_tunnel_busy(),
                                                        aria_busy: spotify_tunnel_busy(),
                                                        onclick: stop_spotify_tunnel,
                                                        "Stop this address"
                                                    }
                                                }
                                            } else {
                                                div { class: "mt-3",
                                                    button {
                                                        r#type: "button",
                                                        class: "btn btn-primary min-h-11 w-full sm:w-auto",
                                                        disabled: spotify_tunnel_busy() || spotify_tunnel_status.is_starting(),
                                                        aria_busy: spotify_tunnel_busy() || spotify_tunnel_status.is_starting(),
                                                        onclick: start_spotify_tunnel,
                                                        if spotify_tunnel_status.is_starting() {
                                                            "Opening an address via {spotify_tunnel_provider_label}…"
                                                        } else {
                                                            "Get an HTTPS address"
                                                        }
                                                    }
                                                    if let Some(message) = spotify_tunnel_error_message.clone() {
                                                        p { class: "mt-2 status-err", role: "alert", "{message}" }
                                                    }
                                                }
                                            }
                                        }
                                    } else {
                                        // Loopback origin -- this browser and UHC are the same
                                        // machine, so the plain HTTP loopback callback just works.
                                        div {
                                            p { class: "mt-2 text-sm text-secondary", "Paste this address into your Spotify app under \"Redirect URIs\", exactly as written." }
                                            div { class: "mt-2 flex flex-col gap-2 sm:flex-row sm:items-stretch",
                                                code {
                                                    id: "spotify-callback-url-display",
                                                    class: "block min-w-0 flex-1 overflow-x-auto break-all rounded-md bg-hover px-3 py-3 text-xs select-all",
                                                    "{spotify_redirect_uri()}"
                                                }
                                                button {
                                                    r#type: "button",
                                                    class: "btn btn-outline btn-sm shrink-0",
                                                    aria_label: "Copy Spotify callback URL",
                                                    onclick: move |_| copy_to_clipboard(spotify_redirect_uri(), spotify_callback_copy_state),
                                                    span { aria_live: "polite", "{spotify_callback_copy_state().label(\"Copy URL\")}" }
                                                }
                                            }
                                        }
                                    }
                                    button {
                                        id: "spotify-callback-registered",
                                        r#type: "button",
                                        class: "btn btn-outline mt-3 min-h-11",
                                        onclick: move |_| {
                                            spotify_step_callback_done.set(true);
                                            spotify_reopen_step.set(None);
                                        },
                                        "I've added it to Spotify"
                                    }
                                    details { class: "mt-3",
                                        summary { class: "cursor-pointer text-sm text-secondary select-none", "Advanced: use your own address" }
                                        div { class: "mt-2 space-y-2",
                                            if spotify_remote_origin() {
                                                div {
                                                    p { class: "text-sm text-secondary", "UHC's built-in address below only works when your browser runs on the exact same machine as UHC:" }
                                                    code {
                                                        class: "mt-2 block min-w-0 overflow-x-auto break-all rounded-md bg-hover px-3 py-3 text-xs select-all",
                                                        "{DEFAULT_SPOTIFY_REDIRECT_URI}"
                                                    }
                                                }
                                            }
                                            p { class: "text-sm text-secondary", "Already have your own secure route to this server (for example ", code { "cloudflared tunnel --url http://127.0.0.1:8088" }, " or Tailscale Funnel)? Put its callback address here and register that with Spotify instead." }
                                            label { class: "block text-sm font-medium", r#for: "spotify-redirect-uri", "Redirect URI" }
                                            input {
                                                id: "spotify-redirect-uri",
                                                class: "input mt-1 min-h-11 w-full",
                                                value: spotify_redirect_uri(),
                                                placeholder: "https://your-uhc-host.example/api/providers/spotify/oauth/callback",
                                                autocomplete: "url",
                                                oninput: move |event| {
                                                    spotify_local_setup_saved.set(false);
                                                    spotify_resave_needed.set(true);
                                                    spotify_redirect_uri.set(event.value());
                                                },
                                            }
                                            p { class: "text-xs text-muted", "Saved together with the Client ID in the next step. Plain http is accepted only on 127.0.0.1 or [::1]." }
                                        }
                                    }
                                }
                            }

                            // Step 3 -- Client ID (server state: configured). The
                            // secret stays behind Advanced; errors render here.
                            li { class: spotify_step_class(3, spotify_step3_complete),
                                div { class: "flex items-start gap-3",
                                    span {
                                        class: if spotify_step3_complete { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs status-ok" } else { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs text-muted" },
                                        aria_hidden: true,
                                        if spotify_step3_complete { "✓" } else { "3" }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "flex min-w-0 flex-1 cursor-pointer flex-col items-start text-left disabled:cursor-default",
                                        disabled: !spotify_step3_complete,
                                        aria_expanded: spotify_step3_open,
                                        aria_controls: "spotify-step-3-body",
                                        onclick: move |_| {
                                            let reopened = *spotify_reopen_step.peek();
                                            spotify_reopen_step.set(if reopened == Some(3) { None } else { Some(3) });
                                        },
                                        span { class: "font-medium", "Enter your Client ID" }
                                        span {
                                            class: "text-xs text-muted",
                                            hidden: !spotify_step3_complete || spotify_step3_open,
                                            "Done -- saved on this UHC server"
                                        }
                                    }
                                    span { class: "group relative shrink-0",
                                        button {
                                            r#type: "button",
                                            class: "flex h-8 w-8 items-center justify-center rounded-full text-muted",
                                            aria_expanded: spotify_info_pinned == Some(3),
                                            aria_controls: "spotify-step-3-info",
                                            aria_label: "More about the Client ID",
                                            onclick: move |_| {
                                                let pinned = *spotify_info_open.peek();
                                                spotify_info_open.set(if pinned == Some(3) { None } else { Some(3) });
                                            },
                                            "ⓘ"
                                        }
                                        div { id: "spotify-step-3-info", class: spotify_info_panel_class(3), role: "note",
                                            p { "The Client ID is the long code on your app's page in the Spotify dashboard. It identifies your app; it is not a password, and it is stored on this UHC server -- never in the browser." }
                                            p { class: "mt-2", "Most setups can leave the Client Secret blank -- UHC signs in securely without it. Only fill it in if you specifically created your app to require one." }
                                        }
                                    }
                                }
                                div {
                                    id: "spotify-step-3-body",
                                    hidden: !spotify_step3_open,
                                    aria_hidden: !spotify_step3_open,
                                    p { class: "mt-2 text-sm text-secondary", "Copy the Client ID from your app's page in the Spotify dashboard and save it here." }
                                    label { class: "mt-3 block text-sm font-medium", r#for: "spotify-client-id", "Client ID" }
                                    input {
                                        id: "spotify-client-id",
                                        class: "input mt-1 min-h-11 w-full",
                                        value: spotify_client_id(),
                                        autocomplete: "off",
                                        oninput: move |event| {
                                            spotify_local_setup_saved.set(false);
                                            spotify_resave_needed.set(true);
                                            spotify_client_id.set(event.value());
                                        },
                                    }
                                    details { class: "mt-3",
                                        summary { class: "cursor-pointer text-sm font-medium select-none", "Advanced: client secret" }
                                        div { class: "mt-2",
                                            label { class: "block text-sm font-medium", r#for: "spotify-client-secret", "Client secret" }
                                            input {
                                                id: "spotify-client-secret",
                                                class: "input mt-1 min-h-11 w-full",
                                                r#type: "password",
                                                value: spotify_client_secret(),
                                                autocomplete: "new-password",
                                                oninput: move |event| {
                                                    spotify_local_setup_saved.set(false);
                                                    spotify_resave_needed.set(true);
                                                    spotify_client_secret.set(event.value());
                                                },
                                            }
                                            p { class: "mt-1 text-xs text-muted", "Usually blank. Never shown back to this page once saved." }
                                        }
                                    }
                                    button {
                                        id: "spotify-save-client-settings",
                                        r#type: "button",
                                        class: "btn btn-primary mt-4 min-h-11 w-full sm:w-auto",
                                        disabled: spotify_action() == ProviderActionState::Loading,
                                        aria_busy: spotify_action() == ProviderActionState::Loading,
                                        aria_describedby: "spotify-client-settings-status",
                                        onclick: save_spotify_local,
                                        if spotify_action() == ProviderActionState::Loading { "Saving…" } else { "Save" }
                                    }
                                    p {
                                        id: "spotify-client-settings-status",
                                        class: if spotify_status_is_error { "mt-2 status-err" } else { "mt-2 text-sm text-secondary" },
                                        hidden: spotify_scope != 3 || spotify_status_message.is_none(),
                                        role: if spotify_status_is_error { "alert" } else { "status" },
                                        aria_live: "polite",
                                        "{spotify_status_message.as_deref().unwrap_or_default()}"
                                    }
                                }
                            }

                            // Step 4 -- connect the account (server state: connected).
                            li { class: spotify_step_class(4, spotify_step4_complete),
                                div { class: "flex items-start gap-3",
                                    span {
                                        class: if spotify_step4_complete { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs status-ok" } else { "flex h-6 w-6 shrink-0 items-center justify-center rounded-full border border-default text-xs text-muted" },
                                        aria_hidden: true,
                                        if spotify_step4_complete { "✓" } else { "4" }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "flex min-w-0 flex-1 cursor-pointer flex-col items-start text-left disabled:cursor-default",
                                        disabled: !spotify_step4_complete,
                                        aria_expanded: spotify_step4_open,
                                        aria_controls: "spotify-step-4-body",
                                        onclick: move |_| {
                                            let reopened = *spotify_reopen_step.peek();
                                            spotify_reopen_step.set(if reopened == Some(4) { None } else { Some(4) });
                                        },
                                        span { class: "font-medium", "Connect your account" }
                                        span {
                                            class: "text-xs status-ok",
                                            hidden: !spotify_step4_complete || spotify_step4_open,
                                            "Connected as {spotify_account_display}"
                                        }
                                    }
                                    span { class: "group relative shrink-0",
                                        button {
                                            r#type: "button",
                                            class: "flex h-8 w-8 items-center justify-center rounded-full text-muted",
                                            aria_expanded: spotify_info_pinned == Some(4),
                                            aria_controls: "spotify-step-4-info",
                                            aria_label: "More about connecting",
                                            onclick: move |_| {
                                                let pinned = *spotify_info_open.peek();
                                                spotify_info_open.set(if pinned == Some(4) { None } else { Some(4) });
                                            },
                                            "ⓘ"
                                        }
                                        div { id: "spotify-step-4-info", class: spotify_info_panel_class(4), role: "note",
                                            p { "Connect opens Spotify's approval page for the app you created. Once you approve, Spotify sends you straight back here and UHC starts discovering your players." }
                                            p { class: "mt-2", "Your sign-in lives on this UHC server and renews itself automatically; the browser never sees it. Reconnecting later repeats this step -- and needs a fresh address from step 2 first." }
                                        }
                                    }
                                }
                                div {
                                    id: "spotify-step-4-body",
                                    hidden: !spotify_step4_open,
                                    aria_hidden: !spotify_step4_open,
                                    p { class: "mt-2 text-sm text-secondary", "Approve access on Spotify's page -- you'll land right back here." }
                                    p {
                                        class: "mt-2 status-err",
                                        hidden: !callback_message.as_ref().is_some_and(|feedback| feedback.is_error),
                                        aria_hidden: !callback_message.as_ref().is_some_and(|feedback| feedback.is_error),
                                        role: "alert",
                                        aria_live: "polite",
                                        "{callback_message.as_ref().filter(|feedback| feedback.is_error).map(|feedback| feedback.message.as_str()).unwrap_or_default()}"
                                    }
                                    button {
                                        id: "spotify-connect-button",
                                        r#type: "button",
                                        class: "btn btn-primary mt-3 min-h-11 w-full sm:w-auto",
                                        disabled: spotify_action() == ProviderActionState::Loading || !spotify_step3_complete,
                                        aria_busy: spotify_action() == ProviderActionState::Loading,
                                        aria_describedby: "spotify-connect-status",
                                        onclick: start_spotify_oauth,
                                        if spotify_action() == ProviderActionState::Loading { "Opening Spotify…" } else if spotify_connected { "Reconnect Spotify" } else if spotify_step3_complete { "Connect Spotify" } else { "Finish the steps above first" }
                                    }
                                    p {
                                        id: "spotify-connect-status",
                                        class: if spotify_status_is_error { "mt-2 status-err" } else { "mt-2 text-sm text-secondary" },
                                        hidden: spotify_scope != 4 || spotify_status_message.is_none(),
                                        role: if spotify_status_is_error { "alert" } else { "status" },
                                        aria_live: "polite",
                                        "{spotify_status_message.as_deref().unwrap_or_default()}"
                                    }
                                }
                            }
                        }
                        div {
                            class: "flex justify-end",
                            hidden: !spotify_configured,
                            aria_hidden: !spotify_configured,
                            button {
                                r#type: "button",
                                class: "btn btn-outline btn-sm min-h-11",
                                onclick: refresh_providers,
                                aria_label: "Refresh Spotify devices",
                                "Refresh devices"
                            }
                        }
                        div {
                            hidden: !(spotify_configured && spotify_devices.is_some()),
                            aria_hidden: !(spotify_configured && spotify_devices.is_some()),
                            div { class: "flex items-center justify-between gap-3",
                                h4 { class: "font-medium",
                                    if let Some(ref devices) = spotify_devices {
                                        "Available devices ({devices.len()})"
                                    } else {
                                        "Available devices"
                                    }
                                }
                            }
                            div {
                                class: "rounded-md border border-default bg-elevated p-4",
                                hidden: !spotify_devices.as_ref().is_some_and(Vec::is_empty),
                                aria_hidden: !spotify_devices.as_ref().is_some_and(Vec::is_empty),
                                role: "status",
                                aria_live: "polite",
                                p { class: "font-medium", "No Spotify devices found" }
                                p { class: "mt-1 text-sm text-secondary", "Start Spotify on a Connect-capable player; UHC will detect it automatically." }
                            }
                            div {
                                hidden: spotify_devices.as_ref().is_none_or(Vec::is_empty),
                                aria_hidden: spotify_devices.as_ref().is_none_or(Vec::is_empty),
                                ul { class: "mt-3 grid gap-2 sm:grid-cols-2", aria_label: "Spotify devices",
                                    if let Some(ref devices) = spotify_devices {
                                        for device in devices {
                                            li { class: "flex min-h-14 items-center justify-between gap-3 rounded-md border border-default bg-elevated px-3 py-2",
                                                div { class: "min-w-0",
                                                    p { class: "font-medium", "{device.zone_name}" }
                                                    details { class: "mt-1 text-xs text-muted",
                                                        summary { class: "cursor-pointer", "Show device ID" }
                                                        code { class: "mt-1 block break-all", "{device.zone_id}" }
                                                    }
                                                }
                                                span { class: if zone_state_label(device) == "unknown" { "text-muted text-sm" } else { "status-ok text-sm" }, "{spotify_device_state_label(device)}" }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        div {
                            class: "rounded-md border border-default bg-elevated p-4",
                            hidden: !(spotify_configured && provider_zones_result.is_none()),
                            aria_hidden: !(spotify_configured && provider_zones_result.is_none()),
                            role: "status",
                            aria_live: "polite",
                            "Loading Spotify devices…"
                        }
                        div {
                            class: "rounded-md border border-default bg-elevated p-4 status-err",
                            hidden: !(spotify_configured && provider_zones_result.as_ref().is_some_and(|result| result.is_err())),
                            aria_hidden: !(spotify_configured && provider_zones_result.as_ref().is_some_and(|result| result.is_err())),
                            role: "alert",
                            "Unable to load Spotify devices. Refresh and try again."
                        }

                        div {
                            class: "mt-5 flex flex-wrap items-center gap-3 border-t border-default pt-4",
                            hidden: !(spotify_configured || spotify_connected),
                            aria_hidden: !(spotify_configured || spotify_connected),
                                button {
                                    r#type: "button",
                                    class: "btn btn-outline min-h-11",
                                    disabled: spotify_action() == ProviderActionState::Loading,
                                    aria_describedby: "spotify-disconnect-status",
                                    onclick: disconnect_spotify,
                                    "Disconnect Spotify"
                                }
                                p {
                                    id: "spotify-disconnect-status",
                                    class: if spotify_status_is_error { "status-err" } else { "text-sm text-secondary" },
                                    hidden: spotify_scope != 5 || spotify_status_message.is_none(),
                                    role: if spotify_status_is_error { "alert" } else { "status" },
                                    aria_live: "polite",
                                    "{spotify_status_message.as_deref().unwrap_or_default()}"
                                }
                            }
                        }
                    }

                    div {
                        class: "card p-5 sm:p-6",
                        hidden: !musicassistant_enabled(),
                        aria_labelledby: "musicassistant-heading",
                        h3 { id: "musicassistant-heading", class: "text-lg font-semibold flex items-center gap-2",
                            "Music Assistant"
                            span { class: "badge badge-secondary", "Alpha" }
                        }
                        p { class: "mt-1 text-sm text-secondary", "Connect UHC directly to a Music Assistant server. Its access token is encrypted and never returned to this page." }
                        if let Some(status) = musicassistant_status.read().clone().flatten() {
                            div { class: "mt-3 text-sm",
                                if status.running {
                                    p { class: "status-ok", "Connected" }
                                    p { class: "mt-1 text-secondary", "Your Music Assistant zones are ready to use in UHC." }
                                    Link { class: "link mt-2 inline-flex min-h-11 items-center", to: Route::Zones {}, "View discovered zones" }
                                }
                                else if status.configured { p { class: "text-yellow-500", "Configured, not currently connected" } }
                                else { p { class: "text-muted", "Setup required" } }
                                if let Some(endpoint) = status.endpoint {
                                    p { class: "mt-1 text-secondary",
                                        "Current endpoint: "
                                        if endpoint.tls { "HTTPS" } else { "HTTP" }
                                        "://{endpoint.host}:{endpoint.port}"
                                    }
                                }
                                if let Some(message) = status.error {
                                    p { class: "mt-2 status-err", role: "alert", "{message}" }
                                }
                            }
                        }
                        if musicassistant_status
                            .read()
                            .clone()
                            .flatten()
                            .is_none_or(|status| !status.configured)
                        {
                            div { class: "mt-4 border-y border-default py-4",
                                h4 { class: "font-medium", "Before you connect" }
                                p { class: "mt-1 text-sm text-secondary", "Create a long-lived access token in Music Assistant, then paste it here." }
                                p { class: "mt-1 text-sm text-muted", "You need the server address and token; the default port is 8095." }
                            }
                        }
                        div { class: "mt-4 grid gap-4 sm:grid-cols-2",
                            div {
                                label { class: "block text-sm font-medium", r#for: "musicassistant-host", "Server host" }
                                input { id: "musicassistant-host", class: "input mt-1 min-h-11 w-full", value: musicassistant_host(), autocomplete: "url", placeholder: "music-assistant.local", oninput: move |event| musicassistant_host.set(event.value()) }
                            }
                            div {
                                label { class: "block text-sm font-medium", r#for: "musicassistant-port", "Port" }
                                input { id: "musicassistant-port", class: "input mt-1 min-h-11 w-full", r#type: "number", value: musicassistant_port(), oninput: move |event| musicassistant_port.set(event.value()) }
                            }
                            div { class: "sm:col-span-2",
                                label { class: "block text-sm font-medium", r#for: "musicassistant-token", "Long-lived access token" }
                                input { id: "musicassistant-token", class: "input mt-1 min-h-11 w-full", r#type: "password", value: musicassistant_token(), autocomplete: "new-password", placeholder: "Leave blank to keep the saved token", oninput: move |event| musicassistant_token.set(event.value()) }
                            }
                        }
                        label { class: "mt-4 flex items-center gap-2 text-sm",
                            input { r#type: "checkbox", checked: musicassistant_tls(), onchange: move |event| musicassistant_tls.set(event.checked()) }
                            "Use HTTPS (recommended)"
                        }
                        details { class: "mt-4 border-t border-default pt-4", open: musicassistant_insecure_http(),
                            summary { class: "cursor-pointer text-sm font-medium text-secondary", "Advanced connection options" }
                            p { class: "mt-2 text-sm text-muted", "Only use plaintext HTTP when UHC and Music Assistant communicate over a trusted local or private network." }
                            label { class: "mt-3 flex items-center gap-2 text-sm",
                                input { r#type: "checkbox", checked: musicassistant_insecure_http(), onchange: move |event| musicassistant_insecure_http.set(event.checked()) }
                                "Allow plaintext HTTP for this trusted local/private peer"
                            }
                        }
                        button { id: "musicassistant-save-settings", r#type: "button", class: "btn btn-primary mt-5 min-h-11", disabled: musicassistant_action() == ProviderActionState::Loading, aria_busy: musicassistant_action() == ProviderActionState::Loading, onclick: save_musicassistant,
                            if musicassistant_action() == ProviderActionState::Loading { "Checking connection…" } else { "Save and connect" }
                        }
                        p { class: "mt-2 text-sm status-err", hidden: musicassistant_error().is_none(), role: "alert", "{musicassistant_error().unwrap_or_default()}" }
                    }

                    // Apple Music pairing is initiated by the companion after
                    // Bonjour discovery. Settings is a calm confirmation
                    // surface, never a second setup form.
                    div {
                        class: "card p-5 sm:p-6",
                        hidden: !applemusic_enabled(),
                        aria_labelledby: "apple-music-heading",
                        div { class: "flex items-start justify-between gap-3",
                            div {
                                h3 { id: "apple-music-heading", class: "text-lg font-semibold flex items-center gap-2",
                                    "Apple Music"
                                    span { class: "badge badge-secondary", "Alpha" }
                                }
                                p { class: "mt-1 text-sm text-secondary", "Connect Apple Music on a Mac, iPhone, or iPad, then control that session from UHC." }
                            }
                            if !applemusic_enabled() {
                                span { class: "badge badge-secondary shrink-0", "Disabled" }
                            } else if matches!(apple_music_state, AppleMusicStatusState::Live) {
                                span { class: "badge badge-success shrink-0", "{apple_music_live_count} ", if apple_music_live_count == 1 { "companion live" } else { "companions live" } }
                            } else if matches!(apple_music_state, AppleMusicStatusState::PairedWaiting) {
                                span { class: "badge badge-secondary shrink-0", "Paired · waiting" }
                            } else {
                                span { class: "badge badge-secondary shrink-0", "Not paired" }
                            }
                        }
                        if let Some(ref status) = apple_st {
                            if let Some(pending) = status.pending_pairings.first() {
                                div { class: "mt-5 rounded-lg bg-surface-muted p-5",
                                    p { class: "font-medium", "Confirm the code on your companion" }
                                    p { class: "mt-2 text-sm text-secondary", "UHC found a companion on your local network. Check that this code matches the code on the companion, then confirm there." }
                                    p { class: "mt-4 font-mono text-4xl font-semibold tracking-[0.22em] text-primary", aria_label: "Apple Music pairing confirmation code", "{pending.pairing_code}" }
                                    p { class: "mt-3 text-xs text-muted", "Waiting for {pending.bridge_id} to confirm. This request expires automatically if it is not confirmed." }
                                }
                            } else if status.companions.is_empty() {
                                if status.paired {
                                    div { class: "mt-5 rounded-lg bg-surface-muted p-5",
                                        p { class: "font-medium", "Paired; waiting for companion" }
                                        p { class: "mt-2 text-sm text-secondary", "Pairing is saved. UHC is waiting for the companion to reconnect over Bonjour." }
                                    }
                                } else {
                                    div { class: "mt-5 rounded-lg bg-surface-muted p-5",
                                        p { class: "font-medium", "Pair a companion" }
                                        p { class: "mt-2 text-sm text-secondary", "Open the UHC Apple Music Companion on your iPhone, iPad, or Mac. Authorize Apple Music, then choose Find UHC and show code. The matching code will appear here." }
                                    }
                                }
                            } else {
                                div { class: "mt-4 grid gap-3 sm:grid-cols-2",
                                    for companion in status.companions.iter() {
                                        div { class: "rounded-lg bg-surface-muted p-4",
                                            div { class: "flex items-center justify-between gap-3",
                                                p { class: "font-medium truncate", "{companion.display_name}" }
                                                span { class: if companion.paired && companion.live { "badge badge-success" } else { "badge badge-secondary" }, if companion.paired && companion.live { "Live" } else { "Offline · paired" } }
                                            }
                                            p { class: "mt-2 text-xs text-secondary", if companion.paired && companion.live { "Ready for playback through UHC." } else if companion.paired { "Pairing is saved. Open the companion app to reconnect it to UHC." } else { "Waiting for this companion to finish pairing." } }
                                            div { class: "mt-3 flex flex-wrap items-center gap-3",
                                                button {
                                                    r#type: "button",
                                                    class: "btn btn-outline min-h-9 px-3 text-sm",
                                                    onclick: {
                                                        let bridge_id = companion.bridge_id.clone();
                                                        let current_name = companion.display_name.clone();
                                                        move |_| {
                                                            let Some(display_name) = browser_prompt("Name this Apple Music companion", &current_name) else { return; };
                                                            let mut apple_bridge_status = apple_bridge_status;
                                                            let bridge_id = bridge_id.clone();
                                                            spawn(async move {
                                                                let _ = crate::app::api::post_json_no_response(
                                                                    "/api/bridges/applemusic/rename",
                                                                    &serde_json::json!({ "bridge_id": bridge_id, "display_name": display_name }),
                                                                ).await;
                                                                apple_bridge_status.restart();
                                                            });
                                                        }
                                                    },
                                                    "Rename"
                                                }
                                                button {
                                                    r#type: "button",
                                                    class: "btn btn-ghost min-h-9 px-3 text-sm text-danger",
                                                    onclick: {
                                                        let bridge_id = companion.bridge_id.clone();
                                                        let display_name = companion.display_name.clone();
                                                        move |_| {
                                                            if !browser_confirm(&format!("Remove {display_name} from UHC? This revokes its saved pairing.")) { return; }
                                                            let mut apple_bridge_status = apple_bridge_status;
                                                            let bridge_id = bridge_id.clone();
                                                            spawn(async move {
                                                                let _ = crate::app::api::post_json_no_response(
                                                                    "/api/bridges/applemusic/remove",
                                                                    &serde_json::json!({ "bridge_id": bridge_id }),
                                                                ).await;
                                                                apple_bridge_status.restart();
                                                            });
                                                        }
                                                    },
                                                    "Remove companion"
                                                }
                                            }
                                            p { class: "mt-3 text-[11px] text-muted", "Device ID: {companion.bridge_id}" }
                                        }
                                    }
                                }
                            }
                        } else {
                            p { class: "mt-5 text-sm text-muted", role: "status", aria_live: "polite", "Checking for Apple Music companions…" }
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-outline mt-5 min-h-11",
                            onclick: move |_| {
                                apple_bridge_status.restart();
                            },
                            "Check again"
                        }
                }
                }
                }
            }

            div { id: "mqtt-anchor",
                section {
                    class: "mb-8",
                    hidden: !mqtt_enabled(),
                    aria_labelledby: "mqtt-heading",
                    div { class: "mb-4",
                        div { class: "flex items-center gap-2",
                            h2 { id: "mqtt-heading", class: "text-xl font-semibold flex items-center gap-2",
                                "MQTT / Home Assistant"
                                span { class: "badge badge-secondary", "Alpha" }
                            }
                            // One-line summary above, the rest behind ⓘ - the
                            // same disclosure the Spotify stepper uses (#597),
                            // so the setup path never becomes a wall of text.
                            span { class: "group relative shrink-0",
                                button {
                                    r#type: "button",
                                    class: "flex h-8 w-8 items-center justify-center rounded-full text-muted",
                                    aria_expanded: mqtt_info_open(),
                                    aria_controls: "mqtt-info",
                                    aria_label: "More about Home Assistant entities",
                                    onclick: move |_| {
                                        let open = *mqtt_info_open.peek();
                                        mqtt_info_open.set(!open);
                                    },
                                    "ⓘ"
                                }
                                div { id: "mqtt-info", class: mqtt_info_panel_class, role: "note",
                                    p { "Home Assistant discovers the zones by itself, so there is nothing to add per zone - no YAML, no entity list. Set up Home Assistant's MQTT integration, point it at the same broker you enter here, and the entities appear." }
                                    p { class: "mt-2", "Each zone becomes a media player you can play, pause, and set the volume on. Each hardware controller becomes its own device." }
                                    p { class: "mt-2", "Advanced: the discovery prefix below has to match the one Home Assistant's MQTT integration uses. Leave it at \"homeassistant\" unless you changed it there." }
                                }
                            }
                        }
                        p { class: "text-muted text-sm mt-1", "Your zones and controllers show up in Home Assistant as entities. Point Home Assistant's MQTT integration at the same broker and they appear on their own." }
                    }
                    div { class: "card p-5 sm:p-6",
                        if let Some(status) = mqtt_status.read().clone().flatten() {
                            div { class: "text-sm",
                                if status.is_environment_managed() {
                                    // The add-on already did this. Saying so
                                    // is what keeps the form below from
                                    // reading as an unfinished setup step.
                                    p { class: "font-medium", "Set up by the Home Assistant add-on" }
                                    if status.running {
                                        p { class: "mt-1 status-ok", "Connected — your zones are in Home Assistant" }
                                    } else {
                                        p { class: "mt-1 text-yellow-500", "Not connected to the broker yet" }
                                    }
                                } else if status.running {
                                    p { class: "status-ok", "Connected — your zones are in Home Assistant" }
                                } else if status.configured {
                                    p { class: "text-yellow-500", "Configured, not currently connected" }
                                } else {
                                    // The failure this whole section existed
                                    // to explain: unconfigured used to read
                                    // "Setup required" and leave the user to
                                    // work out what they were missing out on.
                                    p { class: "font-medium", "No broker yet — nothing is being published" }
                                    p { class: "mt-1 text-secondary", "Add your broker below and your zones and controllers show up in Home Assistant as entities." }
                                }
                                if let Some(host) = status.host.as_ref() {
                                    p { class: "mt-1 text-secondary",
                                        "Current broker: "
                                        if status.tls.unwrap_or(false) { "mqtts://" } else { "mqtt://" }
                                        "{host}:{status.port.unwrap_or_default()}"
                                    }
                                }
                            }
                        }
                        if mqtt_env_managed && !mqtt_show_manual_form {
                            button {
                                id: "mqtt-use-own-broker",
                                r#type: "button",
                                class: "btn btn-secondary mt-4 min-h-11",
                                onclick: move |_| mqtt_manual_open.set(true),
                                "Use a different broker"
                            }
                            p { class: "mt-2 text-xs text-muted", "Broker settings you save here replace the add-on's and are kept across restarts." }
                        }
                        div { class: "mt-4 grid gap-4 sm:grid-cols-2",
                            hidden: !mqtt_show_manual_form,
                            div {
                                label { class: "block text-sm font-medium", r#for: "mqtt-host", "Broker host" }
                                input { id: "mqtt-host", class: "input mt-1 min-h-11 w-full", value: mqtt_host(), autocomplete: "url", placeholder: "homeassistant.local", oninput: move |event| mqtt_host.set(event.value()) }
                            }
                            div {
                                label { class: "block text-sm font-medium", r#for: "mqtt-port", "Port" }
                                input { id: "mqtt-port", class: "input mt-1 min-h-11 w-full", r#type: "number", value: mqtt_port(), placeholder: "1883", oninput: move |event| mqtt_port.set(event.value()) }
                            }
                            div {
                                label { class: "block text-sm font-medium", r#for: "mqtt-username", "Username (optional)" }
                                input { id: "mqtt-username", class: "input mt-1 min-h-11 w-full", value: mqtt_username(), autocomplete: "username", oninput: move |event| mqtt_username.set(event.value()) }
                            }
                            div {
                                label { class: "block text-sm font-medium", r#for: "mqtt-password", "Password (optional)" }
                                input { id: "mqtt-password", class: "input mt-1 min-h-11 w-full", r#type: "password", value: mqtt_password(), autocomplete: "new-password", placeholder: "Leave blank to keep the saved password", oninput: move |event| mqtt_password.set(event.value()) }
                            }
                            div {
                                label { class: "block text-sm font-medium", r#for: "mqtt-base-topic", "Base topic" }
                                input { id: "mqtt-base-topic", class: "input mt-1 min-h-11 w-full", value: mqtt_base_topic(), oninput: move |event| mqtt_base_topic.set(event.value()) }
                            }
                            div {
                                label { class: "block text-sm font-medium", r#for: "mqtt-discovery-prefix", "Discovery prefix" }
                                input { id: "mqtt-discovery-prefix", class: "input mt-1 min-h-11 w-full", value: mqtt_discovery_prefix(), oninput: move |event| mqtt_discovery_prefix.set(event.value()) }
                            }
                        }
                        label { class: "mt-4 flex items-center gap-2 text-sm",
                            hidden: !mqtt_show_manual_form,
                            input { r#type: "checkbox", checked: mqtt_tls(), onchange: move |event| mqtt_tls.set(event.checked()) }
                            "Use TLS"
                        }
                        button { id: "mqtt-save-settings", r#type: "button", class: "btn btn-primary mt-5 min-h-11", hidden: !mqtt_show_manual_form, disabled: mqtt_action() == ProviderActionState::Loading, aria_busy: mqtt_action() == ProviderActionState::Loading, onclick: save_mqtt,
                            if mqtt_action() == ProviderActionState::Loading { "Saving…" } else { "Save and connect" }
                        }
                        p { class: "mt-2 text-xs text-muted", hidden: !mqtt_show_manual_form, "The broker password is encrypted and never sent back to this page." }
                        p { class: "mt-2 text-sm status-err", hidden: mqtt_error().is_none(), role: "alert", "{mqtt_error().unwrap_or_default()}" }
                    }
                }
            }

            // Zone management: name, visibility, and order.
            section { class: "mb-8", aria_labelledby: "zones-heading",
                div { class: "mb-4",
                    h2 { id: "zones-heading", class: "text-xl font-semibold", "Zone list" }
                    p { class: "text-muted text-sm",
                        "Name your zones, choose which ones appear, and set their order. Drag a row to move it, or use the arrows. Applies everywhere — this app, knobs, and connected assistants."
                    }
                }

                // Placed above the table, not below it. This is the sentence that defuses the
                // "am I deleting my speaker?" moment, so it has to arrive before the checkbox
                // rather than several screens below it on a long list. It also sits outside the
                // loaded-and-non-empty branch, so it is present in every state.
                p { class: "text-muted text-sm mb-4",
                    "Hiding only removes a zone from lists. Nothing is deleted, a knob already pointing at it keeps working, and an assistant asked for it by name still finds it. A hidden zone keeps its place in the order, so showing it again puts it back where you left it."
                }

                if let Some(message) = zone_error() {
                    ErrorAlert {
                        message,
                        on_dismiss: move |_| zone_error.set(None),
                    }
                }

                // Announcements for assistive tech. Visually hidden: sighted users see the row
                // move, which this exists to substitute for.
                div {
                    class: "sr-only",
                    role: "status",
                    aria_live: "polite",
                    aria_atomic: "true",
                    "{zone_status}"
                }

                div { class: "card overflow-x-auto p-4 sm:p-6",
                    match managed_zones.read().as_ref() {
                        None => rsx! {
                            p { class: "text-muted py-2", role: "status", aria_live: "polite", "Loading zones…" }
                        },
                        Some(None) => rsx! {
                            div { role: "alert",
                                p { class: "status-err py-2 mb-3", "Could not load the zone list." }
                                button {
                                    r#type: "button",
                                    class: "btn btn-outline btn-sm",
                                    onclick: move |_| managed_zones.restart(),
                                    "Try again"
                                }
                            }
                        },
                        Some(Some(response)) if response.zones.is_empty() => rsx! {
                            p { class: "text-muted py-2",
                                "No zones discovered yet. Check that an adapter is enabled above and connected."
                            }
                        },
                        Some(Some(response)) => {
                            let zones = response.zones.clone();
                            let revision = zone_revision();
                            // Positions count visible zones only, because that is the order every
                            // other surface shows. Numbering hidden zones would promise an order
                            // that nothing displays.
                            let visible_total = zones.iter().filter(|z| !z.hidden).count();
                            let mut visible_position = 0usize;
                            let mut rows = Vec::with_capacity(zones.len());
                            for zone in zones {
                                let position = if zone.hidden {
                                    None
                                } else {
                                    visible_position += 1;
                                    Some(visible_position)
                                };
                                rows.push((zone, position));
                            }
                            let first_visible = rows.iter().position(|(z, _)| !z.hidden);
                            let last_visible = rows.iter().rposition(|(z, _)| !z.hidden);
                            // A drop knows only the dragged zone's id, so the row it belongs to has
                            // to be recoverable by id from inside the drop handler.
                            let drop_lookup: std::rc::Rc<std::collections::HashMap<String, ManagedZone>> =
                                std::rc::Rc::new(
                                    rows.iter()
                                        .map(|(z, _)| (z.zone_id.clone(), z.clone()))
                                        .collect(),
                                );

                            rsx! {
                                table { class: "w-full", id: "zones-table",
                                    thead {
                                        tr { class: "border-b border-default",
                                            // Handle column. Hidden below `sm` because HTML5 drag
                                            // events never fire on touch -- a grip a phone cannot
                                            // use is a false affordance, and the arrows are the
                                            // real control there.
                                            th { class: "w-8 hidden sm:table-cell", span { class: "sr-only", "Drag to reorder" } }
                                            th { class: "text-left py-2 px-2 font-semibold w-12", "Show" }
                                            th { class: "text-left py-2 px-2 font-semibold", "Name" }
                                            th { class: "text-left py-2 px-2 font-semibold hidden sm:table-cell w-28", "Source" }
                                            th { class: "text-right py-2 px-2 font-semibold w-32", "Position" }
                                        }
                                    }
                                    tbody {
                                        for (index, (zone, position)) in rows.into_iter().enumerate() {
                                            tr {
                                                key: "{zone.zone_id}-{revision}",
                                                class: if drop_target() == Some(zone.zone_id.clone()) {
                                                    "zone-row zone-row-drop-target border-b border-default"
                                                } else if dragging() == Some(zone.zone_id.clone()) {
                                                    "zone-row zone-row-dragging border-b border-default"
                                                } else {
                                                    "zone-row border-b border-default"
                                                },
                                                ondragover: {
                                                    let zone_id = zone.zone_id.clone();
                                                    move |evt: DragEvent| {
                                                        // Without preventDefault the browser treats
                                                        // this as an invalid drop target and never
                                                        // fires ondrop.
                                                        evt.prevent_default();
                                                        if dragging().is_some() {
                                                            drop_target.set(Some(zone_id.clone()));
                                                        }
                                                    }
                                                },
                                                ondrop: {
                                                    let target = zone.clone();
                                                    let rows_for_drop = drop_lookup.clone();
                                                    move |evt: DragEvent| {
                                                        evt.prevent_default();
                                                        if let Some(dragged_id) = dragging() {
                                                            if let Some(dragged) = rows_for_drop.get(&dragged_id) {
                                                                drop_zone_onto(dragged.clone(), target.clone());
                                                            }
                                                        }
                                                        dragging.set(None);
                                                        drop_target.set(None);
                                                    }
                                                },
                                                ondragend: move |_| {
                                                    dragging.set(None);
                                                    drop_target.set(None);
                                                },
                                                // The drag handle. `draggable` lives here rather
                                                // than on the row so that selecting text in the
                                                // name field does not start a drag -- the row is
                                                // still the drop target, just not the drag source.
                                                td {
                                                    class: "zone-handle-cell hidden sm:table-cell align-middle",
                                                    draggable: "true",
                                                    ondragstart: {
                                                        let zone_id = zone.zone_id.clone();
                                                        move |_| dragging.set(Some(zone_id.clone()))
                                                    },
                                                    // aria-hidden: dragging has no keyboard
                                                    // equivalent, so the arrows carry the
                                                    // accessible path and this must not add a
                                                    // stop in the tab order that does nothing.
                                                    svg {
                                                        class: "w-4 h-4 text-muted mx-auto",
                                                        fill: "currentColor",
                                                        view_box: "0 0 24 24",
                                                        "aria-hidden": "true",
                                                        circle { cx: "9", cy: "6", r: "1.5" }
                                                        circle { cx: "15", cy: "6", r: "1.5" }
                                                        circle { cx: "9", cy: "12", r: "1.5" }
                                                        circle { cx: "15", cy: "12", r: "1.5" }
                                                        circle { cx: "9", cy: "18", r: "1.5" }
                                                        circle { cx: "15", cy: "18", r: "1.5" }
                                                    }
                                                }
                                                td { class: "py-2 px-2 align-top",
                                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-2",
                                                        input {
                                                            r#type: "checkbox",
                                                            class: "checkbox",
                                                            aria_label: "Show {zone.qualified_label()}",
                                                            checked: !zone.hidden,
                                                            onchange: {
                                                                let zone = zone.clone();
                                                                move |_| set_zone_hidden(zone.clone())
                                                            }
                                                        }
                                                    }
                                                }
                                                td { class: "py-2 px-2 align-top",
                                                    input {
                                                        r#type: "text",
                                                        class: if zone.hidden { "input w-full text-muted" } else { "input w-full" },
                                                        // The draft while the user is typing,
                                                        // otherwise the server's name. A controlled
                                                        // input needs somewhere to put keystrokes
                                                        // or the next render throws them away.
                                                        value: "{name_drafts().get(&zone.zone_id).cloned().unwrap_or_else(|| zone.zone_name.clone())}",
                                                        maxlength: "60",
                                                        aria_label: "Name for {zone.qualified_label()}",
                                                        oninput: {
                                                            let zone_id = zone.zone_id.clone();
                                                            move |evt: FormEvent| {
                                                                name_drafts.write().insert(zone_id.clone(), evt.value());
                                                            }
                                                        },
                                                        // Commits on blur and on Enter rather than
                                                        // per keystroke: each save is a disk write
                                                        // plus a full list refetch, and refetching
                                                        // mid-word would fight the cursor.
                                                        onchange: {
                                                            let zone = zone.clone();
                                                            move |evt: FormEvent| rename_zone(zone.clone(), evt.value())
                                                        }
                                                    }
                                                    // Source lives here below `sm`, where the
                                                    // column is hidden to buy back width for the
                                                    // name field on a phone.
                                                    div { class: "text-muted text-xs mt-1 sm:hidden", "{source_label(&zone.source)}" }
                                                    if zone.renamed {
                                                        div { class: "text-muted text-xs mt-1",
                                                            "{zone.provider_name} · "
                                                            button {
                                                                r#type: "button",
                                                                class: "underline",
                                                                onclick: {
                                                                    let zone = zone.clone();
                                                                    move |_| rename_zone(zone.clone(), String::new())
                                                                },
                                                                "reset"
                                                            }
                                                        }
                                                    }
                                                }
                                                td { class: "py-2 px-2 align-top text-muted hidden sm:table-cell", "{source_label(&zone.source)}" }
                                                td { class: "py-2 px-2 align-top",
                                                    div { class: "flex items-center justify-end gap-2",
                                                        span { class: "text-muted text-sm tabular-nums",
                                                            if let Some(position) = position {
                                                                "{position} of {visible_total}"
                                                            } else {
                                                                "Hidden"
                                                            }
                                                        }
                                                        {
                                                            let at_top = Some(index) == first_visible;
                                                            let at_bottom = Some(index) == last_visible;
                                                            let up_zone = zone.clone();
                                                            let down_zone = zone.clone();
                                                            rsx! {
                                                                // `aria-disabled`, not `disabled`:
                                                                // a disabled control cannot hold
                                                                // focus, so moving a zone to first
                                                                // would disable the very button
                                                                // that was just pressed and drop
                                                                // focus to <body>. These stay
                                                                // focusable and announce "already
                                                                // first" instead.
                                                                button {
                                                                    r#type: "button",
                                                                    class: if at_top { "btn btn-outline btn-sm opacity-45" } else { "btn btn-outline btn-sm" },
                                                                    aria_disabled: if at_top { "true" } else { "false" },
                                                                    aria_label: "Move {zone.qualified_label()} up",
                                                                    onclick: move |_| move_zone(up_zone.clone(), MoveDirection::Up, at_top),
                                                                    svg {
                                                                        class: "w-4 h-4",
                                                                        fill: "none",
                                                                        view_box: "0 0 24 24",
                                                                        stroke: "currentColor",
                                                                        "stroke-width": "2",
                                                                        "aria-hidden": "true",
                                                                        path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M5 15l7-7 7 7" }
                                                                    }
                                                                }
                                                                button {
                                                                    r#type: "button",
                                                                    class: if at_bottom { "btn btn-outline btn-sm opacity-45" } else { "btn btn-outline btn-sm" },
                                                                    aria_disabled: if at_bottom { "true" } else { "false" },
                                                                    aria_label: "Move {zone.qualified_label()} down",
                                                                    onclick: move |_| move_zone(down_zone.clone(), MoveDirection::Down, at_bottom),
                                                                    svg {
                                                                        class: "w-4 h-4",
                                                                        fill: "none",
                                                                        view_box: "0 0 24 24",
                                                                        stroke: "currentColor",
                                                                        "stroke-width": "2",
                                                                        "aria-hidden": "true",
                                                                        path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M19 9l-7 7-7-7" }
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
                        }
                    }
                }
            }

            // MCP discovery and agent onboarding
            section { class: "mb-8", aria_labelledby: "mcp-server-heading",
                div { class: "card overflow-hidden",
                    div { class: "px-5 py-5 sm:px-6 sm:py-6 border-b border-default flex flex-col gap-4 sm:flex-row sm:items-start sm:justify-between",
                        div { class: "max-w-3xl",
                            h2 {
                                id: "mcp-server-heading",
                                class: "text-xl font-semibold",
                                "You have an MCP server"
                            }
                            p { class: "mt-2 text-secondary max-w-2xl",
                                "MCP (Model Context Protocol) is how compatible AI agents connect to your hi-fi tools. It is already running with Unified Hi-Fi Control—there is nothing else to install."
                            }
                        }
                        span { class: "badge badge-secondary gap-2 self-start shrink-0",
                            span {
                                class: "block size-2 rounded-full bg-emerald-500",
                                aria_hidden: "true"
                            }
                            "Available now"
                        }
                    }

                    div { class: "grid lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]",
                        div { class: "min-w-0 px-5 py-5 sm:px-6 sm:py-6 lg:border-r border-default",
                            h3 { class: "font-semibold", "Your MCP address" }
                            p { class: "mt-2 text-sm text-secondary max-w-prose",
                                "Agents on your network can reach this address."
                            }
                            div { class: "mt-4",
                                p { class: "text-xs font-medium text-muted", "MCP URL" }
                                div { class: "mt-2 flex flex-col gap-2 sm:flex-row sm:items-stretch",
                                    code {
                                        class: "block min-w-0 flex-1 overflow-x-auto rounded-md bg-hover px-3 py-3 text-sm select-all",
                                        "{mcp_endpoint.url}"
                                    }
                                    button {
                                        r#type: "button",
                                        class: "btn btn-outline btn-sm shrink-0",
                                        aria_label: "Copy MCP URL",
                                        onclick: {
                                            let url = mcp_endpoint.url.clone();
                                            move |_| {
                                                let value = url.clone();
                                                copy_to_clipboard(value, url_copy_state);
                                            }
                                        },
                                        span { aria_live: "polite", "{url_copy_state().label(\"Copy URL\")}" }
                                    }
                                }
                            }
                            p { class: "mt-4 text-xs text-muted max-w-prose",
                                "Keep Unified Hi-Fi Control running, and only connect agents you trust. Connected agents can control playback on your system."
                            }
                        }

                        div { class: "min-w-0 px-5 py-5 sm:px-6 sm:py-6 bg-[var(--surface-base)]",
                            h3 { class: "font-semibold", "Ask your agent to connect" }
                            p { class: "mt-2 text-sm text-secondary max-w-prose",
                                "Copy this JSON, paste it into your agent, and say:"
                            }
                            p { class: "mt-2 text-sm font-medium text-primary max-w-prose",
                                "“Set up this MCP server for me, then confirm it works by listing my hi-fi zones.”"
                            }
                            div { class: "mt-4 w-full min-w-0 overflow-hidden rounded-md bg-hover",
                                div { class: "flex min-h-11 items-center justify-between gap-3 border-b border-default px-3 py-2",
                                    span { class: "text-xs font-medium text-muted", "Agent configuration" }
                                    button {
                                        r#type: "button",
                                        class: "btn btn-outline btn-sm shrink-0",
                                        aria_label: "Copy agent configuration JSON",
                                        onclick: {
                                            let config = agent_config.clone();
                                            move |_| {
                                                let value = config.clone();
                                                copy_to_clipboard(value, config_copy_state);
                                            }
                                        },
                                        span { aria_live: "polite", "{config_copy_state().label(\"Copy JSON\")}" }
                                    }
                                }
                                pre {
                                    class: "w-full min-w-0 overflow-x-auto p-4 text-xs leading-relaxed text-primary",
                                    aria_label: "MCP server configuration",
                                    code { "{agent_config}" }
                                }
                            }
                            p { class: "mt-4 text-sm text-secondary max-w-prose",
                                "Your agent can usually update its own MCP settings. Restart or reload it only if the new hi-fi tools do not appear."
                            }
                            p { class: "mt-3 text-xs text-muted max-w-prose",
                                "If your agent asks for a URL or uses another configuration format, give it the MCP URL shown here."
                            }
                        }
                    }
                }
            }

            // Theme Settings section
            section { class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Appearance" }
                    p { class: "text-muted text-sm", "Choose your preferred color theme" }
                }

                div { class: "card p-6",
                    div { class: "grid grid-cols-2 sm:grid-cols-4 gap-4",
                        for theme in [Theme::System, Theme::Light, Theme::Dark, Theme::Oled] {
                            button {
                                class: if theme_ctx.get() == theme { "btn-primary py-3" } else { "btn-outline py-3" },
                                onclick: move |_| theme_ctx.set(theme),
                                "{theme.label()}"
                            }
                        }
                    }
                    p { class: "mt-4 text-sm text-muted",
                        match theme_ctx.get() {
                            Theme::System => "Using your system's color scheme preference.",
                            Theme::Light => "Light theme for bright environments.",
                            Theme::Dark => "The HiPhi look: navy surfaces, cyan accent. Default theme.",
                            Theme::Oled => "Pure black theme for AMOLED displays.",
                        }
                    }
                }
            }

        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apple_music_live_companion_count, apple_music_status_state, AppleMusicStatusState,
    };
    use super::{query_param, spotify_callback_feedback, spotify_oauth_error_message};
    use super::{settings_with_toggle, settings_write_error, AdapterToggle, SettingsToggle};
    use crate::app::api::{
        AdapterSettings, AppSettings, AppleBridgeCompanionStatus, AppleBridgeStatus,
    };

    fn settings_fixture() -> AppSettings {
        AppSettings {
            adapters: AdapterSettings {
                roon: true,
                lms: false,
                openhome: false,
                upnp: true,
                hqplayer: false,
                spotify: false,
                applemusic: true,
                musicassistant: false,
                mqtt: false,
            },
            hide_knobs_page: false,
            hide_hqp_page: true,
            hide_lms_page: true,
        }
    }

    #[test]
    fn settings_write_requires_an_explicit_success_acknowledgement() {
        assert_eq!(settings_write_error(&serde_json::json!({"ok": true})), None);
        assert_eq!(
            settings_write_error(
                &serde_json::json!({"ok": false, "error": "Apple Music could not start"})
            ),
            Some("Apple Music could not start".to_string())
        );
        assert!(settings_write_error(&serde_json::json!({})).is_some());
    }

    #[test]
    fn provider_toggle_changes_only_the_requested_provider() {
        let updated = settings_with_toggle(
            settings_fixture(),
            SettingsToggle::Adapter(AdapterToggle::AppleMusic, false),
        );

        assert!(!updated.adapters.applemusic);
        assert!(updated.adapters.roon);
        assert!(updated.adapters.upnp);
        assert!(!updated.adapters.spotify);
    }

    #[test]
    fn hqplayer_toggle_keeps_its_derived_page_visibility_in_sync() {
        let updated = settings_with_toggle(
            settings_fixture(),
            SettingsToggle::Adapter(AdapterToggle::HqPlayer, true),
        );

        assert!(updated.adapters.hqplayer);
        assert!(!updated.hide_hqp_page);
    }

    #[test]
    fn apple_music_status_projection_distinguishes_unpaired_from_saved_pairing() {
        let unpaired = AppleBridgeStatus::default();
        assert_eq!(
            apple_music_status_state(Some(&unpaired)),
            AppleMusicStatusState::Unpaired
        );

        let paired_waiting = AppleBridgeStatus {
            paired: true,
            ..AppleBridgeStatus::default()
        };
        assert_eq!(
            apple_music_status_state(Some(&paired_waiting)),
            AppleMusicStatusState::PairedWaiting
        );
    }

    #[test]
    fn apple_music_status_projection_uses_live_companion_state_not_snapshot_presence() {
        let waiting = AppleBridgeStatus {
            paired: true,
            companions: vec![AppleBridgeCompanionStatus {
                bridge_id: "mac".to_string(),
                paired: true,
                live: false,
                has_snapshot: true,
                ..AppleBridgeCompanionStatus::default()
            }],
            ..AppleBridgeStatus::default()
        };
        assert_eq!(
            apple_music_status_state(Some(&waiting)),
            AppleMusicStatusState::PairedWaiting
        );
        assert_eq!(apple_music_live_companion_count(Some(&waiting)), 0);

        let live = AppleBridgeStatus {
            paired: true,
            live: true,
            companions: vec![AppleBridgeCompanionStatus {
                bridge_id: "mac".to_string(),
                paired: true,
                live: true,
                ..AppleBridgeCompanionStatus::default()
            }],
            ..AppleBridgeStatus::default()
        };
        assert_eq!(
            apple_music_status_state(Some(&live)),
            AppleMusicStatusState::Live
        );
        assert_eq!(apple_music_live_companion_count(Some(&live)), 1);
    }

    #[test]
    fn query_param_extracts_and_decodes_value() {
        assert_eq!(
            query_param("?spotify=error&reason=expired_state", "reason"),
            Some("expired_state".to_string())
        );
        assert_eq!(
            query_param("?spotify=error&reason=token_exchange%5Ffailed", "reason"),
            Some("token_exchange_failed".to_string())
        );
        assert_eq!(query_param("?spotify=connected", "reason"), None);
        assert_eq!(query_param("", "reason"), None);
    }

    #[test]
    fn callback_feedback_reports_success_without_error_styling() {
        let feedback = spotify_callback_feedback("?spotify=connected").unwrap();
        assert!(!feedback.is_error);
        assert!(feedback.message.contains("connected"));

        let legacy = spotify_callback_feedback("?oauth=success").unwrap();
        assert!(!legacy.is_error);
    }

    #[test]
    fn callback_feedback_is_none_without_a_recognized_marker() {
        assert!(spotify_callback_feedback("?tab=devices").is_none());
        assert!(spotify_callback_feedback("").is_none());
    }

    #[test]
    fn callback_feedback_reports_errors_with_a_per_reason_actionable_message() {
        let expired = spotify_callback_feedback("?spotify=error&reason=expired_state").unwrap();
        assert!(expired.is_error);
        assert!(expired.message.contains("expired"));

        let mismatch =
            spotify_callback_feedback("?spotify=error&reason=token_exchange_failed").unwrap();
        assert!(mismatch.is_error);
        assert!(mismatch.message.contains("does not exactly match"));

        let unknown_reason = spotify_callback_feedback("?spotify=error&reason=made_up").unwrap();
        assert!(unknown_reason.is_error);
        assert!(unknown_reason.message.contains("Try Connect again"));

        let no_reason = spotify_callback_feedback("?spotify=error").unwrap();
        assert!(no_reason.is_error);
    }

    #[test]
    fn every_oauth_callback_error_code_has_a_distinct_actionable_message() {
        // Every `code` produced by `oauth_callback_json` in
        // `src/api/provider_auth.rs` must be handled explicitly here so a new
        // failure mode never silently falls back to the generic message.
        let codes = [
            "invalid_state",
            "expired_state",
            "provider_denied",
            "provider_oauth_error",
            "missing_authorization_code",
            "token_exchange_failed",
            "token_storage_failed",
            "oauth_not_configured",
            "invalid_client_configuration",
            "adapter_unavailable",
            "adapter_start_failed",
            "companion_required",
        ];
        let generic = spotify_oauth_error_message(Some("unmapped_code_xyz"));
        for code in codes {
            let message = spotify_oauth_error_message(Some(code));
            assert_ne!(
                message, generic,
                "code {code} should not fall back to the generic message"
            );
        }
    }
}
