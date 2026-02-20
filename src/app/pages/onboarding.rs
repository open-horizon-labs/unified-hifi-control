//! First-run onboarding experience.
//!
//! Shows discovery progress, found zones, and connection guides.
//! Displayed on first visit; subsequent visits go to normal UI.

use crate::app::api::{AppSettings, Zone, ZonesResponse};
use crate::app::embedded_assets::{
    APPLE_TOUCH_ICON_DATA_URL, DX_THEME_CSS, FAVICON_DATA_URL, LOGO_DATA_URL, TAILWIND_CSS,
};
use crate::app::settings_context::use_settings;
use crate::app::sse::{use_sse, SseEvent};
use dioxus::prelude::*;

/// Onboarding wizard — renders outside the normal Router/Layout.
#[component]
pub fn Onboarding() -> Element {
    let sse = use_sse();
    let settings_ctx = use_settings();
    let mut step = use_signal(|| 0u8); // 0 = discovery, 1 = ready

    // Track zones
    let mut zones = use_resource(|| async {
        crate::app::api::fetch_json::<ZonesResponse>("/zones")
            .await
            .ok()
    });

    // Refresh zones on SSE events (zone discovery, updates, adapter changes)
    use_effect(move || {
        let _ = (sse.event_count)();
        let event = (sse.last_event)();

        if matches!(
            event.as_ref(),
            Some(
                SseEvent::ZoneDiscovered { .. }
                    | SseEvent::ZoneUpdated { .. }
                    | SseEvent::ZoneRemoved { .. }
                    | SseEvent::AdapterConnected { .. }
                    | SseEvent::AdapterDisconnected { .. }
                    | SseEvent::RoonConnected
                    | SseEvent::RoonDisconnected
                    | SseEvent::LmsConnected
                    | SseEvent::LmsDisconnected
                    | SseEvent::OpenHomeDeviceFound
                    | SseEvent::OpenHomeDeviceLost
                    | SseEvent::UpnpRendererFound
                    | SseEvent::UpnpRendererLost
            )
        ) {
            zones.restart();
        }
    });

    let zone_list: Vec<Zone> = zones
        .read()
        .clone()
        .flatten()
        .map(|r| r.zones)
        .unwrap_or_default();

    // Group zones by source for display
    let zone_groups = group_zones_by_source(&zone_list);
    let zone_count = zone_list.len();
    let source_count = zone_groups.len();
    let is_loading = zones.read().is_none();

    // Bridge address from window.location
    let bridge_host = get_bridge_host();

    // Shared completion logic: save flag to server and update local signal
    let do_complete = move || {
        spawn(async move {
            if let Ok(mut s) =
                crate::app::api::fetch_json::<AppSettings>("/api/settings").await
            {
                s.onboarding_completed = true;
                let _ =
                    crate::app::api::post_json_no_response("/api/settings", &s).await;
            }
            settings_ctx.complete_onboarding();
        });
    };
    let finish = move |_: MouseEvent| {
        do_complete();
    };
    let skip = move |_: MouseEvent| {
        do_complete();
    };

    let advance = move |_: MouseEvent| {
        step.set(1);
    };

    rsx! {
        // Head elements (since we bypass the normal Layout)
        document::Title { "Welcome — Unified Hi-Fi Control" }
        document::Meta {
            name: "viewport",
            content: "width=device-width, initial-scale=1"
        }
        document::Style { {DX_THEME_CSS} }
        document::Style { {TAILWIND_CSS} }
        document::Link {
            rel: "icon",
            href: "{*FAVICON_DATA_URL}"
        }
        document::Link {
            rel: "apple-touch-icon",
            sizes: "180x180",
            href: "{*APPLE_TOUCH_ICON_DATA_URL}"
        }

        div { class: "min-h-screen flex flex-col items-center justify-center px-4 py-8",
            // Logo + title
            div { class: "text-center mb-8",
                img {
                    src: "{*LOGO_DATA_URL}",
                    alt: "Unified Hi-Fi Control",
                    class: "w-16 h-16 mx-auto mb-4 rounded-xl"
                }
                h1 { class: "text-2xl font-bold", "Unified Hi\u{2011}Fi Control" }
                p { class: "text-muted mt-1", "Multi-source music bridge" }
            }

            // Step indicator
            div { class: "flex items-center gap-2 mb-8",
                span {
                    class: if step() == 0 { "w-2.5 h-2.5 rounded-full bg-primary" } else { "w-2.5 h-2.5 rounded-full bg-muted" }
                }
                span { class: "w-6 h-px bg-border" }
                span {
                    class: if step() == 1 { "w-2.5 h-2.5 rounded-full bg-primary" } else { "w-2.5 h-2.5 rounded-full bg-muted" }
                }
            }

            // Step content
            div { class: "w-full max-w-md",
                match step() {
                    0 => rsx! {
                        DiscoveryStep {
                            zone_groups: zone_groups,
                            zone_count: zone_count,
                            is_loading: is_loading,
                            on_continue: advance,
                            on_skip: skip,
                        }
                    },
                    _ => rsx! {
                        ReadyStep {
                            zone_count: zone_count,
                            source_count: source_count,
                            bridge_host: bridge_host,
                            on_complete: finish,
                        }
                    },
                }
            }
        }
    }
}

// =============================================================================
// Step components
// =============================================================================

#[component]
fn DiscoveryStep(
    zone_groups: Vec<(String, Vec<Zone>)>,
    zone_count: usize,
    is_loading: bool,
    on_continue: EventHandler<MouseEvent>,
    on_skip: EventHandler<MouseEvent>,
) -> Element {
    rsx! {
        div { class: "space-y-6",
            // Heading
            div { class: "text-center",
                h2 { class: "text-xl font-semibold mb-1", "Discovering your music sources" }
                p { class: "text-muted text-sm",
                    if is_loading {
                        "Searching for music servers on your network..."
                    } else if zone_count == 0 {
                        "Searching... zones will appear as they are found."
                    } else {
                        "Zones appear in real-time as they are discovered."
                    }
                }
            }

            // Loading indicator
            if is_loading || zone_count == 0 {
                div { class: "flex items-center justify-center gap-3 py-6 text-muted",
                    // Animated spinner
                    svg {
                        class: "w-5 h-5 animate-spin",
                        fill: "none",
                        view_box: "0 0 24 24",
                        circle {
                            class: "opacity-25",
                            cx: "12",
                            cy: "12",
                            r: "10",
                            stroke: "currentColor",
                            stroke_width: "4",
                        }
                        path {
                            class: "opacity-75",
                            fill: "currentColor",
                            d: "M4 12a8 8 0 018-8V0C5.373 0 0 5.373 0 12h4z",
                        }
                    }
                    span { "Searching..." }
                }
            }

            // Discovered zones grouped by source
            if zone_count > 0 {
                div { class: "space-y-4",
                    p { class: "text-sm font-medium",
                        "Found {zone_count} zone{plural(zone_count)}:"
                    }
                    for (source, zones) in zone_groups.iter() {
                        div { class: "card p-3",
                            div { class: "flex items-center gap-2 mb-2",
                                span { class: "w-2 h-2 rounded-full bg-green-500" }
                                span { class: "text-sm font-medium", "{source}" }
                                span { class: "text-xs text-muted",
                                    "({zones.len()} zone{plural(zones.len())})"
                                }
                            }
                            ul { class: "space-y-1 ml-4",
                                for zone in zones.iter() {
                                    li {
                                        key: "{zone.zone_id}",
                                        class: "text-sm text-muted flex items-center gap-2",
                                        span { "♪" }
                                        span { class: "truncate", "{zone.zone_name}" }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            // Action buttons
            div { class: "flex justify-between items-center pt-4",
                button {
                    class: "btn btn-ghost text-sm",
                    onclick: move |e| on_skip.call(e),
                    "Skip"
                }
                button {
                    class: "btn btn-primary",
                    onclick: move |e| on_continue.call(e),
                    if zone_count > 0 {
                        "Continue"
                    } else {
                        "Continue anyway"
                    }
                }
            }
        }
    }
}

#[component]
fn ReadyStep(
    zone_count: usize,
    source_count: usize,
    bridge_host: String,
    on_complete: EventHandler<MouseEvent>,
) -> Element {
    let mcp_url = format!("http://{}/mcp", bridge_host);

    rsx! {
        div { class: "space-y-6",
            // Heading
            div { class: "text-center",
                h2 { class: "text-xl font-semibold mb-1", "You're ready!" }
                if zone_count > 0 {
                    p { class: "text-muted text-sm",
                        "Found {zone_count} zone{plural(zone_count)} from {source_count} source{plural(source_count)}."
                    }
                } else {
                    p { class: "text-muted text-sm",
                        "No zones found yet — they'll appear once your music servers are online."
                    }
                }
            }

            // Connection guide
            div { class: "space-y-3",
                p { class: "text-sm font-medium", "Control your music with:" }

                // Web
                ConnectionCard {
                    icon: "🌐",
                    title: "Web Browser",
                    description: "You're already here — bookmark this page.",
                }

                // Knob
                ConnectionCard {
                    icon: "🎛",
                    title: "Hardware Knob",
                    description: format!("Point your roon-knob at {bridge_host}"),
                }

                // Phone
                ConnectionCard {
                    icon: "📱",
                    title: "iPhone / Apple Watch",
                    description: "Download HiFi Control from the App Store.".to_string(),
                }

                // AI
                ConnectionCard {
                    icon: "🤖",
                    title: "AI Assistant (MCP)",
                    description: format!("Add to Claude or ChatGPT: {mcp_url}"),
                }
            }

            // Get started button
            div { class: "pt-4",
                button {
                    class: "btn btn-primary w-full",
                    onclick: move |e| on_complete.call(e),
                    "Get Started"
                }
            }
        }
    }
}

#[component]
fn ConnectionCard(icon: &'static str, title: &'static str, description: String) -> Element {
    rsx! {
        div { class: "card p-3 flex items-start gap-3",
            span { class: "text-xl flex-shrink-0", "{icon}" }
            div {
                p { class: "text-sm font-medium", "{title}" }
                p { class: "text-xs text-muted mt-0.5", "{description}" }
            }
        }
    }
}

// =============================================================================
// Helpers
// =============================================================================

/// Group zones by source, sorted (Roon, LMS, OpenHome, UPnP, Other).
fn group_zones_by_source(zones: &[Zone]) -> Vec<(String, Vec<Zone>)> {
    let mut groups: std::collections::HashMap<String, Vec<Zone>> =
        std::collections::HashMap::new();
    for zone in zones {
        let source = zone.source.clone().unwrap_or_else(|| "Other".to_string());
        groups.entry(source).or_default().push(zone.clone());
    }
    for v in groups.values_mut() {
        v.sort_by(|a, b| a.zone_name.cmp(&b.zone_name));
    }
    let priority = |s: &str| -> i32 {
        match s.to_lowercase().as_str() {
            "roon" => 0,
            "lms" => 1,
            "openhome" => 2,
            "upnp" => 3,
            _ => 4,
        }
    };
    let mut result: Vec<_> = groups.into_iter().collect();
    result.sort_by(|a, b| priority(&a.0).cmp(&priority(&b.0)));
    result
}

/// Plural suffix helper
fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

/// Get bridge host (hostname:port) from window.location (WASM) or fallback.
fn get_bridge_host() -> String {
    #[cfg(target_arch = "wasm32")]
    {
        if let Some(window) = web_sys::window() {
            // host() returns "hostname:port" (or just "hostname" if default port)
            if let Ok(host) = window.location().host() {
                if !host.is_empty() {
                    return host;
                }
            }
        }
    }
    "uhc.local:8088".to_string()
}
