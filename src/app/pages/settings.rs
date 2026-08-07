//! Settings page component.
//!
//! Adapter settings and discovery status using Dioxus resources.

use dioxus::prelude::*;

use crate::app::api::{
    AdapterSettings, AppSettings, AppleBridgePairingResponse, AppleBridgeStatus, HqpStatus,
    LmsConfig, ProviderAuthResponse, ProviderOAuthStart, RoonStatus, SpotifyAccountResponse,
    SpotifyConfigureRequest, SpotifyConfigureResponse, ZonesResponse,
};
use crate::app::components::Layout;
use crate::app::settings_context::use_settings;
use crate::app::sse::use_sse;
use crate::app::theme::{use_theme, Theme};
use crate::app::McpEndpoint;

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

#[cfg(target_arch = "wasm32")]
fn callback_feedback() -> Option<&'static str> {
    let search = web_sys::window()?.location().search().ok()?;
    if search.contains("spotify=connected") || search.contains("oauth=success") {
        Some("Spotify connected. Refreshing available devices…")
    } else if search.contains("spotify=error") || search.contains("oauth=error") {
        Some("Spotify authorization did not complete. Try Connect again or open Client settings.")
    } else {
        None
    }
}

#[cfg(target_arch = "wasm32")]
fn default_spotify_redirect_uri() -> String {
    let fallback = "http://127.0.0.1:8088/api/providers/spotify/oauth/callback";
    let Some(window) = web_sys::window() else {
        return fallback.to_string();
    };
    let Ok(origin) = window.location().origin() else {
        return fallback.to_string();
    };
    if origin.is_empty() {
        fallback.to_string()
    } else {
        format!("{origin}/api/providers/spotify/oauth/callback")
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn default_spotify_redirect_uri() -> String {
    "http://127.0.0.1:8088/api/providers/spotify/oauth/callback".to_string()
}

#[cfg(not(target_arch = "wasm32"))]
fn initial_spotify_enabled() -> bool {
    crate::api::load_app_settings().adapters.spotify
}

#[cfg(target_arch = "wasm32")]
fn initial_spotify_enabled() -> bool {
    initial_adapter_enabled_from_ssr("spotify")
}

#[cfg(not(target_arch = "wasm32"))]
fn initial_applemusic_enabled() -> bool {
    crate::api::load_app_settings().adapters.applemusic
}

#[cfg(target_arch = "wasm32")]
fn initial_applemusic_enabled() -> bool {
    initial_adapter_enabled_from_ssr("applemusic")
}

#[cfg(target_arch = "wasm32")]
fn initial_adapter_enabled_from_ssr(adapter: &str) -> bool {
    let Some(document) = web_sys::window().and_then(|window| window.document()) else {
        return false;
    };
    let Some(marker) = document.get_element_by_id("settings-adapter-hydration") else {
        return false;
    };
    marker
        .get_attribute(&format!("data-{adapter}-enabled"))
        .is_some_and(|value| value == "true")
}

#[cfg(not(target_arch = "wasm32"))]
fn callback_feedback() -> Option<&'static str> {
    None
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

    // Adapter toggle signals
    let mut roon_enabled = use_signal(|| true);
    let mut lms_enabled = use_signal(|| false);
    let mut openhome_enabled = use_signal(|| false);
    let mut upnp_enabled = use_signal(|| false);
    let mut hqplayer_enabled = use_signal(|| false);
    let mut spotify_enabled = use_signal(initial_spotify_enabled);
    let mut applemusic_enabled = use_signal(initial_applemusic_enabled);
    let mut musicassistant_enabled = use_signal(|| false);

    // Streaming-provider onboarding state. Provider credentials are never
    // rendered or stored in the browser; the backend owns OAuth tokens.
    let mut spotify_action = use_signal(ProviderActionState::default);
    let mut spotify_error = use_signal(|| None::<String>);
    let mut spotify_local_setup_saved = use_signal(|| false);
    let mut spotify_editing = use_signal(|| false);
    let mut spotify_client_id = use_signal(String::new);
    let mut spotify_client_secret = use_signal(String::new);
    let mut spotify_redirect_uri = use_signal(default_spotify_redirect_uri);
    let mut apple_action = use_signal(ProviderActionState::default);
    let mut apple_error = use_signal(|| None::<String>);
    let mut apple_bridge_id = use_signal(|| "ios-companion".to_string());
    let mut apple_pairing = use_signal(|| None::<AppleBridgePairingResponse>);

    // Hide knobs signal (LMS/HQPlayer visibility follows adapter enabled state)
    let mut hide_knobs = use_signal(|| false);

    // Load settings resource
    let settings = use_resource(|| async {
        crate::app::api::fetch_json::<AppSettings>("/api/settings")
            .await
            .ok()
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
            hide_knobs.set(s.hide_knobs_page);
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
    let mut apple_bridge_status = use_resource(|| async {
        crate::app::api::fetch_json::<AppleBridgeStatus>("/api/bridges/applemusic/status").await
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
            apple_bridge_status.restart();
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
        spawn(async move {
            loop {
                dioxus_sdk_time::sleep(std::time::Duration::from_secs(2)).await;
                apple_bridge_status.restart();
            }
        });
    });

    let start_spotify_oauth = move |_| {
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
                    spotify_error.set(Some(error));
                }
            }
        });
    };

    let disconnect_spotify = move |_| {
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
                    spotify_error.set(Some(error));
                }
            }
        });
    };

    let save_spotify_local = move |_| {
        let client_id = spotify_client_id().trim().to_string();
        let client_secret = spotify_client_secret().trim().to_string();
        let redirect_uri = spotify_redirect_uri().trim().to_string();
        if client_id.is_empty() {
            spotify_action.set(ProviderActionState::Failed);
            spotify_error.set(Some("Enter the Spotify client ID first.".to_string()));
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
                    spotify_editing.set(false);
                }
                Ok(_) => {
                    spotify_action.set(ProviderActionState::Failed);
                    spotify_error.set(Some("Spotify configuration was not accepted.".to_string()));
                }
                Err(error) => {
                    spotify_action.set(ProviderActionState::Failed);
                    spotify_error.set(Some(error));
                }
            }
        });
    };

    let refresh_providers = move |_| {
        provider_zones.restart();
        spotify_account.restart();
        apple_bridge_status.restart();
    };

    // Save settings handler
    let save_settings = move || {
        let hk = hide_knobs();
        let hqp = hqplayer_enabled();
        let lms = lms_enabled();

        // Update shared context immediately for reactive Nav updates
        settings_ctx.update(hk, hqp, lms, spotify_enabled());

        let settings = AppSettings {
            adapters: AdapterSettings {
                roon: roon_enabled(),
                lms,
                openhome: openhome_enabled(),
                upnp: upnp_enabled(),
                hqplayer: hqp,
                spotify: spotify_enabled(),
                applemusic: applemusic_enabled(),
                musicassistant: musicassistant_enabled(),
            },
            hide_knobs_page: hk,
            // These are now derived from adapter state but we keep them for API compat
            hide_hqp_page: !hqp,
            hide_lms_page: !lms,
        };
        spawn(async move {
            let _ = crate::app::api::post_json_no_response("/api/settings", &settings).await;
        });
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
    let callback_message = callback_feedback();
    let spotify_status_is_error = spotify_error().is_some();
    let spotify_status_message =
        spotify_error().or_else(|| spotify_action().message().map(str::to_string));
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

    rsx! {
        Layout {
            title: "Settings".to_string(),
            nav_active: "settings".to_string(),

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
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable Roon",
                                            checked: roon_enabled(),
                                            onchange: move |_| {
                                                roon_enabled.toggle();
                                                save_settings();
                                            }
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
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable OpenHome",
                                            checked: openhome_enabled(),
                                            onchange: move |_| {
                                                openhome_enabled.toggle();
                                                save_settings();
                                            }
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
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable UPnP/DLNA",
                                            checked: upnp_enabled(),
                                            onchange: move |_| {
                                                upnp_enabled.toggle();
                                                save_settings();
                                            }
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
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable LMS",
                                            checked: lms_enabled(),
                                            onchange: move |_| {
                                                lms_enabled.toggle();
                                                save_settings();
                                            }
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
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable HQPlayer",
                                            checked: hqplayer_enabled(),
                                            onchange: move |_| {
                                                hqplayer_enabled.toggle();
                                                save_settings();
                                            }
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
                            // Knobs (page only, no adapter)
                            // Spotify (controller adapter; zones arrive through the bus)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable Spotify",
                                            checked: spotify_enabled(),
                                            onchange: move |_| {
                                                spotify_enabled.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Spotify" }
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
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Enable Apple Music",
                                            checked: applemusic_enabled(),
                                            onchange: move |_| {
                                                applemusic_enabled.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Apple Music" }
                                td { class: "py-2 px-3",
                                    if applemusic_enabled() {
                                        if let Some(ref status) = apple_st {
                                            if status.paired && status.has_snapshot {
                                                span { class: "status-ok", "✓ Companion live" }
                                            } else if status.paired {
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
                            // Knobs (page only, no adapter)
                            tr { class: "border-b border-default",
                                td { class: "py-2 px-3",
                                    label { class: "inline-flex min-h-11 min-w-11 items-center justify-center -my-2 -mx-3",
                                        input {
                                            r#type: "checkbox",
                                            class: "checkbox",
                                            aria_label: "Show Knobs page",
                                            checked: !hide_knobs(),
                                            onchange: move |_| {
                                                hide_knobs.toggle();
                                                save_settings();
                                            }
                                        }
                                    }
                                }
                                td { class: "py-2 px-3", "Knobs" }
                                td { class: "py-2 px-3 text-muted", "-" }
                            }
                        }
                }
            }

            // Keep a stable SSR marker and anchor so hydration cannot insert the provider
            // section into a later sibling when no provider is enabled yet.
            div {
                id: "settings-adapter-hydration",
                hidden: true,
                "data-spotify-enabled": if initial_spotify_enabled() { "true" } else { "false" },
                "data-applemusic-enabled": if initial_applemusic_enabled() { "true" } else { "false" },
            }
            div { id: "streaming-providers-anchor",
                section {
                    class: "mb-8",
                    hidden: !(spotify_enabled() || applemusic_enabled()),
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
                                h3 { id: "spotify-heading", class: "text-lg font-semibold", "Spotify Connect" }
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

                        p {
                            class: "mt-4 status-ok",
                            hidden: callback_message.is_none(),
                            aria_hidden: callback_message.is_none(),
                            role: "status",
                            aria_live: "polite",
                            "{callback_message.unwrap_or_default()}"
                        }

                        // Keep both panes in the hydrated tree. The server can know that
                        // credentials are configured before the browser's resources resolve;
                        // conditionally omitting either pane shifts every later event handler.
                        div { class: "mt-5 grid gap-5 lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]",
                            div {
                                class: "status-panel rounded-lg p-4 sm:p-5",
                                hidden: !spotify_configured || spotify_editing(),
                                aria_hidden: !spotify_configured || spotify_editing(),
                                role: "status",
                                aria_live: "polite",
                                p { class: "font-medium",
                                    if spotify_connected {
                                        "Connected · credentials stored on this UHC server"
                                    } else {
                                        "Configured on this UHC server"
                                    }
                                }
                                p { class: "mt-1 text-sm text-secondary",
                                    if spotify_connected {
                                        "OAuth credentials refresh automatically."
                                    } else {
                                        "Connect Spotify to authorize an account; secrets stay on this UHC server."
                                    }
                                }
                                div { class: "mt-4 flex flex-wrap gap-2",
                                    button {
                                        r#type: "button",
                                        class: "btn btn-primary min-h-11",
                                        disabled: spotify_action() == ProviderActionState::Loading,
                                        aria_busy: spotify_action() == ProviderActionState::Loading,
                                        onclick: start_spotify_oauth,
                                        if spotify_connected { "Reconnect Spotify" } else { "Connect Spotify" }
                                    }
                                    button {
                                        r#type: "button",
                                        class: "btn btn-ghost min-h-11",
                                        onclick: move |_| spotify_editing.set(true),
                                        "Edit client settings"
                                    }
                                }
                            }
                            div {
                                class: "lg:col-span-2 grid gap-4 sm:grid-cols-2",
                                hidden: spotify_configured && !spotify_editing(),
                                aria_hidden: spotify_configured && !spotify_editing(),
                            div { class: "rounded-md border border-default p-4",
                                h4 { class: "font-medium", "Connect Spotify" }
                                p { class: "mt-2 text-sm text-secondary", "After saving the client settings below, open Spotify’s consent page, approve playback access, and return here. Your token stays on this UHC server." }
                                button {
                                    r#type: "button",
                                    class: "btn btn-primary mt-4 min-h-11 w-full sm:w-auto",
                                    disabled: spotify_action() == ProviderActionState::Loading || !spotify_local_setup_saved(),
                                    aria_busy: spotify_action() == ProviderActionState::Loading,
                                    onclick: start_spotify_oauth,
                                    if spotify_action() == ProviderActionState::Loading { "Opening Spotify…" } else if spotify_local_setup_saved() { "Connect Spotify" } else { "Save setup first" }
                                }
                                if !spotify_local_setup_saved() {
                                    p { class: "mt-2 text-xs text-muted", "Enter and save your Spotify client ID in Client settings before connecting." }
                                }
                            }
                            div { class: "rounded-md border border-default p-4",
                                h4 { class: "font-medium", "Client settings" }
                                p { class: "mt-2 text-sm text-secondary", "Use this when UHC is self-hosted or running on another machine. UHC stores the OAuth client settings server-side and refreshes access automatically." }
                                p { class: "mt-2 text-sm text-secondary",
                                    "Create or manage your Spotify app in the "
                                    a { href: "https://developer.spotify.com/dashboard", target: "_blank", rel: "noopener noreferrer", class: "link", "Spotify Developer Dashboard" }
                                    ". Copy its Client ID and Client Secret here."
                                }
                                div { class: "mt-4 rounded-md border border-default bg-elevated p-3", aria_label: "Remote UHC setup instructions",
                                    h5 { class: "font-medium", "Using UHC from another device?" }
                                    p { class: "mt-1 text-sm text-secondary", "Start a temporary HTTPS tunnel to this UHC server, then open the tunnel URL in this browser. The callback below will follow that secure origin." }
                                    ol { class: "mt-2 list-decimal space-y-1 pl-5 text-sm text-secondary",
                                        li { "On the machine running UHC, tunnel its configured web port (8088 by default; use your configured port if different) with your provider (for example, ", code { "cloudflared tunnel --url http://127.0.0.1:8088" }, " or Tailscale Funnel)." }
                                        li { "Open the provider’s HTTPS URL here; do not continue from the server’s plain HTTP address." }
                                        li { "Register the exact callback URI below in the Spotify developer dashboard, then save and connect." }
                                        li { "Stop the tunnel after authorization. Start a new one if Spotify needs reauthorization later." }
                                    }
                                }
                                label { class: "mt-3 block text-sm font-medium", r#for: "spotify-client-id", "Client ID" }
                                input {
                                    id: "spotify-client-id",
                                    class: "input mt-1 min-h-11 w-full",
                                    value: spotify_client_id(),
                                    autocomplete: "off",
                                    oninput: move |event| {
                                        spotify_local_setup_saved.set(false);
                                        spotify_client_id.set(event.value());
                                    },
                                }
                                label { class: "mt-3 block text-sm font-medium", r#for: "spotify-client-secret", "Client secret (optional for PKCE)" }
                                input {
                                    id: "spotify-client-secret",
                                    class: "input mt-1 min-h-11 w-full",
                                    r#type: "password",
                                    value: spotify_client_secret(),
                                    autocomplete: "new-password",
                                    oninput: move |event| {
                                        spotify_local_setup_saved.set(false);
                                        spotify_client_secret.set(event.value());
                                    },
                                }
                                label { class: "mt-3 block text-sm font-medium", r#for: "spotify-redirect-uri", "Redirect URI (optional)" }
                                input {
                                    id: "spotify-redirect-uri",
                                    class: "input mt-1 min-h-11 w-full",
                                    value: spotify_redirect_uri(),
                                    placeholder: "https://your-uhc-host.example/api/providers/spotify/oauth/callback",
                                    autocomplete: "url",
                                    oninput: move |event| {
                                        spotify_local_setup_saved.set(false);
                                        spotify_redirect_uri.set(event.value());
                                    },
                                }
                                p { class: "mt-2 text-xs text-muted", "Spotify requires HTTPS when UHC is accessed remotely. Plain HTTP is accepted only on 127.0.0.1 or [::1]." }
                                button {
                                    id: "spotify-save-client-settings",
                                    r#type: "button",
                                    class: "btn btn-outline mt-4 min-h-11 w-full sm:w-auto",
                                    disabled: spotify_action() == ProviderActionState::Loading,
                                    aria_busy: spotify_action() == ProviderActionState::Loading,
                                    aria_describedby: "spotify-client-settings-status",
                                    onclick: save_spotify_local,
                                    if spotify_action() == ProviderActionState::Loading { "Saving…" } else { "Save client settings" }
                                }
                                p { class: "mt-2 text-xs text-muted", "Secrets are never returned to this page. Use Connect after saving to authorize the account." }
                            }
                            }
                        }

                        // Status panes stay mounted while their state changes so the
                        // Refresh handler cannot move during hydration or polling.
                        div {
                            class: "rounded-md border border-default bg-elevated p-4",
                            hidden: spotify_configured,
                            aria_hidden: spotify_configured,
                            role: "status",
                            aria_live: "polite",
                                p { class: "font-medium", "Spotify setup required" }
                                p { class: "mt-1 text-sm text-secondary", "Save your Spotify client settings, then connect your account to discover Connect devices." }
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

                        p {
                            id: "spotify-client-settings-status",
                            class: if spotify_status_is_error { "mt-4 status-err" } else { "mt-4 text-sm text-secondary" },
                            hidden: spotify_status_message.is_none(),
                            role: if spotify_status_is_error { "alert" } else { "status" },
                            aria_live: "polite",
                            "{spotify_status_message.as_deref().unwrap_or_default()}"
                        }

                        div {
                            class: "mt-5 flex flex-wrap gap-3 border-t border-default pt-4",
                            hidden: !(spotify_configured || spotify_connected),
                            aria_hidden: !(spotify_configured || spotify_connected),
                                button {
                                    r#type: "button",
                                    class: "btn btn-outline min-h-11",
                                    disabled: spotify_action() == ProviderActionState::Loading,
                                    onclick: disconnect_spotify,
                                    "Disconnect Spotify"
                                }
                            }
                        }
                    }

                    // Apple Music is paired through the native MusicKit app;
                    // keep this card focused on status and the next action.
                    div {
                        class: "card p-5 sm:p-6",
                        hidden: !applemusic_enabled(),
                        aria_labelledby: "apple-music-heading",
                        div { class: "flex items-start justify-between gap-3",
                            div {
                                h3 { id: "apple-music-heading", class: "text-lg font-semibold", "Apple Music" }
                                p { class: "mt-1 text-sm text-secondary", "Control Apple Music from your iPhone, iPad, or Mac through UHC." }
                            }
                            if !applemusic_enabled() {
                                span { class: "badge badge-secondary shrink-0", "Disabled" }
                            } else if apple_st.as_ref().map(|status| status.companions.iter().any(|companion| companion.has_snapshot)).unwrap_or(false) {
                                span { class: "badge badge-success shrink-0", "{apple_st.as_ref().map(|status| status.companions.len()).unwrap_or(0)} companions live" }
                            } else if apple_st.as_ref().map(|status| !status.companions.is_empty()).unwrap_or(false) {
                                span { class: "badge badge-secondary shrink-0", "Paired · waiting" }
                            } else {
                                span { class: "badge badge-secondary shrink-0", "Not paired" }
                            }
                        }
                        p { class: "mt-4 text-sm text-secondary", "Authorize Apple Music on a companion device, then pair it with UHC. Each companion is an independent Apple Music zone; your credentials stay on the device." }
                        p { class: "mt-3 text-sm text-secondary", "Catalog and library access use the same Apple Music account." }
                        if let Some(ref status) = apple_st {
                            if status.companions.is_empty() {
                                p { class: "mt-4 text-sm text-muted", role: "status", aria_live: "polite", "No companion is paired yet." }
                            } else {
                                div { class: "mt-4 grid gap-3 sm:grid-cols-2",
                                    for companion in status.companions.iter() {
                                        div { class: "rounded-lg border border-default bg-surface-muted p-4",
                                            div { class: "flex items-center justify-between gap-3",
                                                p { class: "font-medium truncate", "{companion.bridge_id}" }
                                                span { class: if companion.has_snapshot { "badge badge-success" } else { "badge badge-secondary" }, if companion.has_snapshot { "Live" } else { "Waiting" } }
                                            }
                                            p { class: "mt-2 text-xs text-secondary", "Independent Apple Music zone" }
                                        }
                                    }
                                }
                            }
                        } else {
                            p { class: "mt-4 text-sm text-muted", role: "status", aria_live: "polite", "Checking for a paired companion…" }
                        }
                        div { class: "mt-5 grid gap-3 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-end",
                                label { class: "block text-sm",
                                    span { class: "mb-1 block text-secondary", "Companion ID" }
                                    input {
                                        class: "input input-bordered w-full min-h-11",
                                        aria_label: "Apple Music companion ID",
                                        value: "{apple_bridge_id}",
                                        oninput: move |event| apple_bridge_id.set(event.value().to_string()),
                                        placeholder: "ios-companion"
                                    }
                                }
                                button {
                                    r#type: "button",
                                    class: "btn btn-primary min-h-11",
                                    disabled: apple_action() == ProviderActionState::Loading || apple_bridge_id().trim().is_empty(),
                                    onclick: move |_| {
                                        let bridge_id = apple_bridge_id().trim().to_string();
                                        apple_action.set(ProviderActionState::Loading);
                                        apple_error.set(None);
                                        apple_pairing.set(None);
                                        spawn(async move {
                                            match crate::app::api::post_json::<serde_json::Value, AppleBridgePairingResponse>(
                                                "/api/bridges/applemusic/pair",
                                                &serde_json::json!({ "bridge_id": bridge_id }),
                                            ).await {
                                                Ok(pairing) => {
                                                    apple_pairing.set(Some(pairing));
                                                    apple_action.set(ProviderActionState::Success);
                                                }
                                                Err(error) => {
                                                    apple_action.set(ProviderActionState::Failed);
                                                    apple_error.set(Some(error));
                                                }
                                            }
                                        });
                                    },
                                    "Generate pairing code"
                                }
                        }
                        if let Some(status) = apple_st.as_ref() {
                            for pending in status.pending_pairings.iter() {
                                div { class: "mt-4 rounded-lg border border-default bg-surface-muted p-4",
                                    p { class: "text-sm font-medium", "Confirm this code in the companion" }
                                    p { class: "mt-2 font-mono text-2xl tracking-[0.35em]", aria_label: "Apple Music pairing confirmation code", "{pending.pairing_code}" }
                                    p { class: "mt-2 text-xs text-secondary", "Companion: {pending.bridge_id} · The companion discovered this UHC server automatically. Confirm that both screens show the same code; nothing needs to be typed." }
                                }
                            }
                        }
                        if let Some(pairing) = apple_pairing() {
                                div { class: "mt-4 rounded-lg border border-default bg-surface-muted p-4",
                                    p { class: "text-sm font-medium", "Enter this code in the companion" }
                                    p { class: "mt-2 break-all font-mono text-lg tracking-wide", aria_label: "Apple Music pairing code", "{pairing.pairing_code}" }
                                    p { class: "mt-2 text-xs text-secondary", "Bridge ID: {pairing.bridge_id} · Expires in about 5 minutes" }
                                }
                        }
                        if let Some(error) = apple_error() {
                            p { class: "mt-4 status-err", role: "alert", "{error}" }
                        } else if apple_action() == ProviderActionState::Loading {
                            p { class: "mt-4 text-sm text-muted", role: "status", aria_live: "polite", "Updating companion status…" }
                        }
                        button {
                            r#type: "button",
                            class: "btn btn-outline mt-5 min-h-11",
                            disabled: apple_action() == ProviderActionState::Loading,
                            onclick: move |_| {
                                apple_action.set(ProviderActionState::Loading);
                                apple_error.set(None);
                                apple_bridge_status.restart();
                                apple_action.set(ProviderActionState::Success);
                            },
                            "Refresh companion status"
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
                            Theme::Dark => "Dark theme for low-light environments.",
                            Theme::Oled => "Pure black theme for AMOLED displays.",
                        }
                    }
                }
            }

        }
    }
}
