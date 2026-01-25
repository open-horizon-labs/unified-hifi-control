//! LMS (Logitech Media Server) page component.
//!
//! Using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{AppSettings, LmsConfig, LmsPlayer, LmsPlayersResponse};
use crate::app::components::Layout;
use crate::app::sse::use_sse;

/// LMS configure request
#[derive(Clone, serde::Serialize)]
struct LmsConfigureRequest {
    host: String,
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// LMS control request
#[derive(Clone, serde::Serialize)]
struct LmsControlRequest {
    player_id: String,
    action: String,
}

/// LMS page component.
#[component]
pub fn Lms() -> Element {
    let sse = use_sse();

    // Config state
    let mut show_form = use_signal(|| false);
    let mut save_status = use_signal(|| None::<String>);

    // Form fields
    let mut host = use_signal(String::new);
    let mut port = use_signal(|| 9000u16);
    let mut username = use_signal(String::new);
    let mut password = use_signal(String::new);

    // Load config resource
    let mut config = use_resource(|| async {
        crate::app::api::fetch_json::<LmsConfig>("/lms/config")
            .await
            .ok()
    });

    // Load players resource (API returns { players: [...] })
    let mut players = use_resource(|| async {
        crate::app::api::fetch_json::<LmsPlayersResponse>("/lms/players")
            .await
            .ok()
            .map(|r| r.players)
    });

    // Check if LMS is enabled
    let settings = use_resource(|| async {
        crate::app::api::fetch_json::<AppSettings>("/api/settings")
            .await
            .ok()
    });

    // Sync config to form when loaded
    use_effect(move || {
        if let Some(Some(cfg)) = config.read().as_ref() {
            if cfg.configured {
                host.set(cfg.host.clone().unwrap_or_default());
                port.set(cfg.port.unwrap_or(9000));
            }
        }
    });

    // Refresh on SSE events
    let event_count = sse.event_count;
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_lms() {
            config.restart();
            players.restart();
        }
    });

    // Save config handler
    let save_config = move |_| {
        let h = host();
        let p = port();
        let u = username();
        let pw = password();

        if h.is_empty() {
            save_status.set(Some("Host is required".to_string()));
            return;
        }

        save_status.set(Some("Connecting...".to_string()));

        spawn(async move {
            let req = LmsConfigureRequest {
                host: h,
                port: p,
                username: if u.is_empty() { None } else { Some(u) },
                password: if pw.is_empty() { None } else { Some(pw) },
            };

            match crate::app::api::post_json::<_, serde_json::Value>("/lms/configure", &req).await {
                Ok(_) => {
                    save_status.set(Some("Connected!".to_string()));
                    show_form.set(false);
                    config.restart();
                    players.restart();
                }
                Err(e) => {
                    save_status.set(Some(format!("Error: {}", e)));
                }
            }
        });
    };

    // Control handler
    let control = move |(player_id, action): (String, String)| {
        spawn(async move {
            let req = LmsControlRequest { player_id, action };
            let _ = crate::app::api::post_json_no_response("/lms/control", &req).await;
        });
    };

    let cfg = config.read().clone().flatten();
    let settings_loading = settings.read().is_none();
    let lms_enabled = settings.read().clone().flatten().map(|s| s.adapters.lms);
    let players_list = players.read().clone().flatten().unwrap_or_default();
    let is_loading = config.read().is_none();

    rsx! {
        Layout {
            title: "LMS".to_string(),
            nav_active: "lms".to_string(),

            h1 { class: "text-2xl font-bold mb-6", "Logitech Media Server" }

            // Server Configuration section
            section { id: "lms-config", class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Server Configuration" }
                    p { class: "text-muted text-sm", "Configure connection to your Squeezebox server" }
                }
                div { class: "card p-6",
                    // Status line
                    div { class: "mb-4",
                        if let Some(ref c) = cfg {
                            if c.configured && c.connected {
                                div {
                                    span { class: "status-ok",
                                        "✓ Connected to {c.host.as_deref().unwrap_or(\"\")}:{c.port.unwrap_or(9000)}"
                                    }
                                }
                                // Debug info: CLI subscription and polling
                                div { class: "text-sm text-muted mt-1",
                                    if c.cli_subscription_active {
                                        span { class: "text-green-600", "CLI: active" }
                                    } else {
                                        span { class: "text-yellow-600", "CLI: inactive (polling only)" }
                                    }
                                    span { class: "mx-2", "•" }
                                    span { "Poll: {c.poll_interval_secs}s" }
                                }
                            } else if c.configured {
                                span { class: "status-err",
                                    "✗ Configured but not connected ({c.host.as_deref().unwrap_or(\"\")}:{c.port.unwrap_or(9000)})"
                                }
                            } else {
                                span { class: "text-muted", "Not configured" }
                            }
                        } else {
                            span { class: "text-muted", "Checking..." }
                        }
                    }

                    // Config form (shown when not configured or reconfiguring)
                    if show_form() || cfg.as_ref().map(|c| !c.configured).unwrap_or(true) {
                        div { class: "mt-4",
                            div { class: "form-grid mb-4",
                                div {
                                    label { class: "block text-sm font-medium mb-1", "Host" }
                                    input {
                                        class: "input",
                                        r#type: "text",
                                        placeholder: "192.168.1.x or hostname",
                                        value: "{host}",
                                        oninput: move |evt| host.set(evt.value())
                                    }
                                }
                                div {
                                    label { class: "block text-sm font-medium mb-1", "Port" }
                                    input {
                                        class: "input",
                                        r#type: "number",
                                        min: "1",
                                        max: "65535",
                                        value: "{port}",
                                        oninput: move |evt| {
                                            if let Ok(p) = evt.value().parse() {
                                                port.set(p);
                                            }
                                        }
                                    }
                                }
                            }
                            div { class: "form-grid mb-4",
                                div {
                                    label { class: "block text-sm font-medium mb-1", "Username (optional)" }
                                    input {
                                        class: "input",
                                        r#type: "text",
                                        placeholder: "Leave blank if not required",
                                        value: "{username}",
                                        oninput: move |evt| username.set(evt.value())
                                    }
                                }
                                div {
                                    label { class: "block text-sm font-medium mb-1", "Password (optional)" }
                                    input {
                                        class: "input",
                                        r#type: "password",
                                        placeholder: "Leave blank if not required",
                                        value: "{password}",
                                        oninput: move |evt| password.set(evt.value())
                                    }
                                }
                            }
                            div { class: "flex items-center gap-4",
                                button { class: "btn btn-primary", onclick: save_config, "Save & Connect" }
                                if let Some(ref status) = save_status() {
                                    if status.starts_with("Error") || status.contains("required") {
                                        span { class: "status-err", "{status}" }
                                    } else if status.contains("Connected") {
                                        span { class: "status-ok", "✓ {status}" }
                                    } else {
                                        span { class: "text-muted", "{status}" }
                                    }
                                }
                            }
                        }
                    }

                    // Reconfigure button (shown when configured)
                    if cfg.as_ref().map(|c| c.configured).unwrap_or(false) && !show_form() {
                        button {
                            class: "btn btn-outline mt-4",
                            onclick: move |_| show_form.set(true),
                            "Reconfigure"
                        }
                    }
                }
            }

            // Players section
            section { id: "lms-players", class: "mb-8",
                div { class: "mb-4",
                    h2 { class: "text-xl font-semibold", "Players" }
                    p { class: "text-muted text-sm", "Connected Squeezebox players" }
                }

                if settings_loading {
                    div { class: "card p-6", aria_busy: "true", "Loading settings..." }
                } else if matches!(lms_enabled, Some(false)) {
                    div { class: "card p-6",
                        p { class: "text-muted",
                            "LMS adapter is disabled. "
                            a { class: "link", href: "/settings", "Enable it in Settings" }
                            " to discover players."
                        }
                    }
                } else if is_loading {
                    div { class: "card p-6", aria_busy: "true", "Loading..." }
                } else if players_list.is_empty() {
                    div { class: "card p-6",
                        p { class: "text-muted", "No players found. Make sure your Squeezebox server is configured and reachable." }
                    }
                } else {
                    div { class: "zone-grid",
                        for player in players_list {
                            PlayerCard {
                                player: player.clone(),
                                on_control: control,
                            }
                        }
                    }
                }
            }
        }
    }
}

/// Player card component
#[component]
fn PlayerCard(player: LmsPlayer, on_control: EventHandler<(String, String)>) -> Element {
    let player_id = player.player_id.clone();
    let player_id_prev = player_id.clone();
    let player_id_play = player_id.clone();
    let player_id_next = player_id.clone();

    let play_icon = if player.mode == "play" {
        "⏸︎"
    } else {
        "▶"
    };

    rsx! {
        div { class: "card p-4",
            // Header
            div { class: "flex items-center gap-2 mb-3",
                span { class: "font-semibold text-lg", "{player.name}" }
                span { class: "badge badge-secondary", "{player.mode}" }
            }

            // Now playing
            div { class: "min-h-[40px] overflow-hidden mb-4",
                if let Some(ref title) = player.current_title {
                    p { class: "font-medium text-sm truncate", "{title}" }
                    if let Some(ref artist) = player.artist {
                        p { class: "text-sm text-muted truncate", "{artist}" }
                    }
                } else {
                    p { class: "text-sm text-muted", "Nothing playing" }
                }
            }

            // Transport controls
            div { class: "flex items-center gap-2",
                button {
                    class: "btn btn-ghost",
                    onclick: move |_| on_control.call((player_id_prev.clone(), "previous".to_string())),
                    "◀◀"
                }
                button {
                    class: "btn btn-primary",
                    onclick: move |_| on_control.call((player_id_play.clone(), "play_pause".to_string())),
                    "{play_icon}"
                }
                button {
                    class: "btn btn-ghost",
                    onclick: move |_| on_control.call((player_id_next.clone(), "next".to_string())),
                    "▶▶"
                }
                span { class: "ml-auto text-sm text-muted", "Volume: {player.volume}%" }
            }
        }
    }
}
