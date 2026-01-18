//! LMS (Logitech Media Server) page component.
//!
//! Using Dioxus resources for async data fetching.

use dioxus::prelude::*;

use crate::app::api::{AppSettings, LmsConfig, LmsPlayer};
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

    // Load players resource
    let mut players = use_resource(|| async {
        crate::app::api::fetch_json::<Vec<LmsPlayer>>("/lms/players")
            .await
            .ok()
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
    let lms_enabled = settings
        .read()
        .clone()
        .flatten()
        .map(|s| s.adapters.lms)
        .unwrap_or(false);
    let players_list = players.read().clone().flatten().unwrap_or_default();
    let is_loading = config.read().is_none();

    rsx! {
        Layout {
            title: "LMS".to_string(),
            nav_active: "lms".to_string(),

            h1 { "Logitech Media Server" }

            // Server Configuration section
            section { id: "lms-config",
                hgroup {
                    h2 { "Server Configuration" }
                    p { "Configure connection to your Squeezebox server" }
                }
                article {
                    // Status line
                    div {
                        if let Some(ref c) = cfg {
                            if c.configured && c.connected {
                                span { class: "status-ok",
                                    "✓ Connected to {c.host.as_deref().unwrap_or(\"\")}:{c.port.unwrap_or(9000)}"
                                }
                            } else if c.configured {
                                span { class: "status-err",
                                    "✗ Configured but not connected ({c.host.as_deref().unwrap_or(\"\")}:{c.port.unwrap_or(9000)})"
                                }
                            } else {
                                "Not configured"
                            }
                        } else {
                            "Checking..."
                        }
                    }

                    // Config form (shown when not configured or reconfiguring)
                    if show_form() || cfg.as_ref().map(|c| !c.configured).unwrap_or(true) {
                        div { style: "margin-top:1rem;",
                            div { class: "grid",
                                label {
                                    "Host"
                                    input {
                                        r#type: "text",
                                        placeholder: "192.168.1.x or hostname",
                                        value: "{host}",
                                        oninput: move |evt| host.set(evt.value())
                                    }
                                }
                                label {
                                    "Port"
                                    input {
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
                            div { class: "grid",
                                label {
                                    "Username (optional)"
                                    input {
                                        r#type: "text",
                                        placeholder: "Leave blank if not required",
                                        value: "{username}",
                                        oninput: move |evt| username.set(evt.value())
                                    }
                                }
                                label {
                                    "Password (optional)"
                                    input {
                                        r#type: "password",
                                        placeholder: "Leave blank if not required",
                                        value: "{password}",
                                        oninput: move |evt| password.set(evt.value())
                                    }
                                }
                            }
                            button { onclick: save_config, "Save & Connect" }
                            if let Some(ref status) = save_status() {
                                span { style: "margin-left:1rem;",
                                    if status.starts_with("Error") || status.contains("required") {
                                        span { class: "status-err", "{status}" }
                                    } else if status.contains("Connected") {
                                        span { class: "status-ok", "✓ {status}" }
                                    } else {
                                        "{status}"
                                    }
                                }
                            }
                        }
                    }

                    // Reconfigure button (shown when configured)
                    if cfg.as_ref().map(|c| c.configured).unwrap_or(false) && !show_form() {
                        button {
                            style: "margin-top:1rem;",
                            onclick: move |_| show_form.set(true),
                            "Reconfigure"
                        }
                    }
                }
            }

            // Players section
            section { id: "lms-players",
                hgroup {
                    h2 { "Players" }
                    p { "Connected Squeezebox players" }
                }

                if !lms_enabled {
                    article {
                        p {
                            "LMS adapter is disabled. "
                            a { href: "/settings", "Enable it in Settings" }
                            " to discover players."
                        }
                    }
                } else if is_loading {
                    article { aria_busy: "true", "Loading..." }
                } else if players_list.is_empty() {
                    article {
                        p { "No players found. Make sure your Squeezebox server is configured and reachable." }
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

    let play_icon = if player.mode == "play" { "⏸" } else { "▶" };

    rsx! {
        article {
            header {
                strong { "{player.name}" }
                small { " ({player.mode})" }
            }
            p {
                if let Some(ref title) = player.current_title {
                    "{title}"
                    if let Some(ref artist) = player.artist {
                        br {}
                        small { "{artist}" }
                    }
                } else {
                    small { "Nothing playing" }
                }
            }
            footer {
                div { class: "controls",
                    button {
                        onclick: move |_| on_control.call((player_id_prev.clone(), "previous".to_string())),
                        "◀◀"
                    }
                    button {
                        onclick: move |_| on_control.call((player_id_play.clone(), "play_pause".to_string())),
                        "{play_icon}"
                    }
                    button {
                        onclick: move |_| on_control.call((player_id_next.clone(), "next".to_string())),
                        "▶▶"
                    }
                }
                p { "Volume: {player.volume}%" }
            }
        }
    }
}
