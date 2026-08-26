//! HQPlayer page component.
//!
//! Consolidated HQPlayer control with linked zone playback controls at top.

use dioxus::prelude::*;

use crate::app::api::{
    self, HqpConfig, HqpMatrixProfilesResponse, HqpPipeline, HqpProfile, HqpStatus, NowPlaying,
    Zone, ZonesResponse,
};
use crate::app::components::{HqpMatrixSelect, HqpProfileSelect, Layout, VolumeControlsCompact};
use crate::app::sse::use_sse;

/// HQP configure request
#[derive(Clone, serde::Serialize)]
struct HqpConfigureRequest {
    host: String,
    port: u16,
    web_port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

/// Zone link response
#[derive(Clone, Debug, Default, serde::Deserialize)]
struct ZoneLinksResponse {
    links: Vec<ZoneLink>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
struct ZoneLink {
    zone_id: String,
    instance: String,
}

/// HQPlayer instances response
#[derive(Clone, Debug, Default, serde::Deserialize)]
struct InstancesResponse {
    instances: Vec<HqpInstance>,
}

#[derive(Clone, Debug, Default, PartialEq, serde::Deserialize)]
struct HqpInstance {
    name: String,
    host: Option<String>,
}

/// Zone link request
#[derive(Clone, serde::Serialize)]
struct ZoneLinkRequest {
    zone_id: String,
    instance: String,
}

/// Zone unlink request
#[derive(Clone, serde::Serialize)]
struct ZoneUnlinkRequest {
    zone_id: String,
}

#[derive(Clone, Debug, PartialEq)]
enum ZoneMatchFeedback {
    Saved,
    Removed,
    Error(String),
}

/// Control request body
#[derive(Clone, serde::Serialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
}

/// Gate slow advanced-state reads so rerenders or mutation bursts cannot overlap requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CoalescingRefresh {
    initial_requested: bool,
    in_flight: bool,
    dirty: bool,
}

impl CoalescingRefresh {
    /// `use_effect` may run after unrelated renders. Only the first mounted-page invocation is an
    /// initial read; later explicit mutations still use [`Self::request`].
    fn request_initial(&mut self) -> bool {
        if self.initial_requested {
            return false;
        }
        self.initial_requested = true;
        self.request()
    }

    /// Returns true when the caller owns the single fetch loop.
    fn request(&mut self) -> bool {
        if self.in_flight {
            self.dirty = true;
            false
        } else {
            self.in_flight = true;
            true
        }
    }

    /// Returns true when one coalesced follow-up read is required.
    fn complete(&mut self) -> bool {
        if self.dirty {
            self.dirty = false;
            true
        } else {
            self.in_flight = false;
            false
        }
    }
}

fn refresh_advanced_projection(
    mut snapshot: Signal<Option<HqpMatrixProfilesResponse>>,
    mut refresh: Signal<CoalescingRefresh>,
    initial: bool,
) {
    let should_start = if initial {
        refresh.write().request_initial()
    } else {
        refresh.write().request()
    };
    if !should_start {
        return;
    }

    spawn(async move {
        loop {
            if let Ok(next) =
                api::fetch_json::<HqpMatrixProfilesResponse>("/hqplayer/matrix/profiles").await
            {
                snapshot.set(Some(next));
            }
            if !refresh.write().complete() {
                break;
            }
        }
    });
}

/// HQPlayer page component.
#[component]
pub fn HqPlayer() -> Element {
    let sse = use_sse();

    // Form fields for config
    let mut host = use_signal(String::new);
    let mut port = use_signal(|| 4321u16);
    let mut web_port = use_signal(|| 8088u16);
    let username = use_signal(String::new);
    let password = use_signal(String::new);
    let mut has_credentials = use_signal(|| false);
    let mut config_status = use_signal(|| None::<String>);
    let mut show_config = use_signal(|| false);

    // HQP state
    let mut hqp_loading = use_signal(|| false);
    let mut hqp_error = use_signal(|| None::<String>);
    let mut zone_match_busy = use_signal(|| false);
    let mut zone_match_feedback = use_signal(|| None::<ZoneMatchFeedback>);

    // Now playing for linked zones
    let mut now_playing_map = use_signal(std::collections::HashMap::<String, NowPlaying>::new);

    // Load config resource
    let config =
        use_resource(|| async { api::fetch_json::<HqpConfig>("/hqplayer/config").await.ok() });

    // Load status resource
    let mut status =
        use_resource(|| async { api::fetch_json::<HqpStatus>("/hqp/status").await.ok() });

    // Load pipeline resource
    let mut pipeline =
        use_resource(|| async { api::fetch_json::<HqpPipeline>("/hqp/pipeline").await.ok() });

    // Load profiles resource
    let mut profiles = use_resource(|| async {
        api::fetch_json::<Vec<HqpProfile>>("/hqp/profiles")
            .await
            .ok()
    });

    // Advanced state is intentionally not a restartable Resource: its slow read is coalesced so
    // rerenders or mutation bursts cannot starve rendering or exhaust the browser's request pool.
    let matrix = use_signal(|| None::<HqpMatrixProfilesResponse>);
    let matrix_refresh = use_signal(CoalescingRefresh::default);
    use_effect(move || refresh_advanced_projection(matrix, matrix_refresh, true));

    // Load zones resource
    let mut zones =
        use_resource(|| async { api::fetch_json::<ZonesResponse>("/knob/zones").await.ok() });

    // Load zone links resource
    let mut zone_links = use_resource(|| async {
        api::fetch_json::<ZoneLinksResponse>("/hqp/zones/links")
            .await
            .ok()
    });

    // Load instances resource
    let instances = use_resource(|| async {
        api::fetch_json::<InstancesResponse>("/hqp/instances")
            .await
            .ok()
    });
    let mut zones_loaded_once = use_signal(|| false);
    let mut zone_links_loaded_once = use_signal(|| false);
    let mut instances_loaded_once = use_signal(|| false);

    let zones_state = zones.state();
    use_effect(move || {
        // `.peek()`, not the tracked call: this effect must run when `zones_state`
        // flips to Ready, not when `zones_loaded_once` (which it also writes) changes.
        // A tracked read here would subscribe the effect to its own write, causing an
        // extra self-triggered run every time the latch flips (reactive-loop-lint).
        if !*zones_loaded_once.peek() && matches!(*zones_state.read(), UseResourceState::Ready) {
            zones_loaded_once.set(true);
        }
    });
    let zone_links_state = zone_links.state();
    use_effect(move || {
        // See zones_loaded_once above: `.peek()` avoids subscribing this effect to
        // its own latch write.
        if !*zone_links_loaded_once.peek()
            && matches!(*zone_links_state.read(), UseResourceState::Ready)
        {
            zone_links_loaded_once.set(true);
        }
    });
    let instances_state = instances.state();
    use_effect(move || {
        // See zones_loaded_once above: `.peek()` avoids subscribing this effect to
        // its own latch write.
        if !*instances_loaded_once.peek()
            && matches!(*instances_state.read(), UseResourceState::Ready)
        {
            instances_loaded_once.set(true);
        }
    });

    // Sync config to form when loaded
    use_effect(move || {
        if let Some(Some(cfg)) = config.read().as_ref() {
            host.set(cfg.host.clone().unwrap_or_default());
            port.set(cfg.port.unwrap_or(4321));
            web_port.set(cfg.web_port.unwrap_or(8088));
            has_credentials.set(cfg.has_web_credentials);
        }
    });

    // Load now playing for linked zones
    let zones_list_signal = use_memo(move || {
        zones
            .read()
            .clone()
            .flatten()
            .map(|r| r.zones)
            .unwrap_or_default()
    });

    let links_signal = use_memo(move || {
        zone_links
            .read()
            .clone()
            .flatten()
            .map(|r| r.links)
            .unwrap_or_default()
    });

    // Direct HQPlayer zones and any source zones explicitly linked to HQPlayer share this surface.
    // Deduplicate by zone id so a direct zone cannot appear twice if it is also linked.
    let controlled_zones_signal = use_memo(move || {
        let all_zones = zones_list_signal();
        let links = links_signal();
        all_zones
            .into_iter()
            .filter(|zone| {
                zone.source.as_deref() == Some("hqplayer")
                    || links.iter().any(|link| link.zone_id == zone.zone_id)
            })
            .collect::<Vec<_>>()
    });

    let event_count = sse.event_count;

    // Fetch now playing for every zone controlled from this page and refresh it after aggregator
    // events. A zone's identity does not change when its playback or volume state does.
    use_effect(move || {
        let _ = event_count();
        let controlled_zones = controlled_zones_signal();
        if controlled_zones.is_empty() {
            now_playing_map.set(std::collections::HashMap::new());
            return;
        }
        spawn(async move {
            let mut np_map = std::collections::HashMap::new();
            for zone in controlled_zones {
                let url = format!(
                    "/now_playing?zone_id={}",
                    urlencoding::encode(&zone.zone_id)
                );
                if let Ok(np) = api::fetch_json::<NowPlaying>(&url).await {
                    np_map.insert(zone.zone_id, np);
                }
            }
            now_playing_map.set(np_map);
        });
    });

    // Refresh on SSE events
    use_effect(move || {
        let _ = event_count();
        if sse.should_refresh_hqp() {
            status.restart();
            pipeline.restart();
            // Do not refresh the advanced endpoint from its own compatibility events. That read
            // publishes HqpStateChanged/HqpPipelineChanged after committing its aggregator
            // snapshot, so feeding either event back into the same read creates an endless native
            // refresh loop. Advanced state is loaded on page entry and explicitly after every
            // successful mutation/configuration change below.
        }
        if sse.should_refresh_zones() {
            zones.restart();
            zone_links.restart();
            // Note: now_playing refresh happens automatically via links_signal effect
        }
    });

    // Save config handler
    let save_config = move |_| {
        let h = host();
        let p = port();
        let wp = web_port();
        let u = username();
        let pw = password();

        config_status.set(Some("Testing connection…".to_string()));

        spawn(async move {
            let req = HqpConfigureRequest {
                host: h,
                port: p,
                web_port: wp,
                username: if u.is_empty() { None } else { Some(u) },
                password: if pw.is_empty() { None } else { Some(pw) },
            };

            match api::post_json::<_, serde_json::Value>("/hqplayer/configure", &req).await {
                Ok(resp) => {
                    let connected = resp
                        .get("connected")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    if connected {
                        config_status.set(Some("Connected and verified.".to_string()));
                    } else {
                        config_status.set(Some(
                            "Saved, but HQPlayer did not answer. Check the host and control port."
                                .to_string(),
                        ));
                    }
                    status.restart();
                    pipeline.restart();
                    profiles.restart();
                    refresh_advanced_projection(matrix, matrix_refresh, false);
                }
                Err(e) => {
                    config_status.set(Some(format!(
                        "Connection failed: {e}. Check the address, ports, and web credentials."
                    )));
                }
            }
        });
    };

    // Zone control handler
    let control = move |(zone_id, action, value): (String, String, Option<f64>)| {
        hqp_error.set(None);
        spawn(async move {
            let req = ControlRequest {
                zone_id,
                action,
                value,
            };
            if let Err(error) = api::post_json_no_response("/control", &req).await {
                hqp_error.set(Some(playback_control_error(&error.to_string())));
            }
        });
    };

    // Pipeline setting handler
    let set_pipeline = move |(setting, value): (String, String)| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct PipelineRequest {
                setting: String,
                value: String,
            }
            let req = PipelineRequest { setting, value };
            if let Err(e) = api::post_json_no_response("/hqp/pipeline", &req).await {
                hqp_error.set(Some(format!("Pipeline update failed: {e}")));
            } else {
                // Server now returns fresh state after setting, so HQPlayer has processed
                // the change before we refresh
                pipeline.restart();
                refresh_advanced_projection(matrix, matrix_refresh, false);
            }
            hqp_loading.set(false);
        });
    };

    // Load profile handler
    let load_profile = move |profile: String| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct ProfileRequest {
                profile: String,
            }
            let req = ProfileRequest { profile };
            if let Err(e) = api::post_json_no_response("/hqplayer/profile", &req).await {
                hqp_error.set(Some(format!("Profile load failed: {e}")));
            } else {
                pipeline.restart();
                profiles.restart();
                refresh_advanced_projection(matrix, matrix_refresh, false);
            }
            hqp_loading.set(false);
        });
    };

    // Set matrix profile handler
    let set_matrix = move |profile_name: String| {
        hqp_error.set(None);
        hqp_loading.set(true);
        spawn(async move {
            #[derive(serde::Serialize)]
            struct MatrixRequest {
                setting: &'static str,
                value: String,
            }
            let req = MatrixRequest {
                setting: "matrix_profile",
                value: profile_name,
            };
            if let Err(e) = api::post_json_no_response("/hqp/pipeline", &req).await {
                hqp_error.set(Some(format!("Matrix profile failed: {e}")));
            } else {
                refresh_advanced_projection(matrix, matrix_refresh, false);
            }
            hqp_loading.set(false);
        });
    };

    // Zone link handler
    let link_zone = move |(zone_id, instance): (String, String)| {
        zone_match_busy.set(true);
        zone_match_feedback.set(None);
        spawn(async move {
            let req = ZoneLinkRequest { zone_id, instance };
            match api::post_json_no_response("/hqp/zones/link", &req).await {
                Ok(()) => {
                    zone_match_feedback.set(Some(ZoneMatchFeedback::Saved));
                    zone_links.restart();
                    zones.restart();
                }
                Err(error) => {
                    zone_match_feedback.set(Some(ZoneMatchFeedback::Error(format!(
                        "Could not pair this playback path: {error}"
                    ))));
                }
            }
            zone_match_busy.set(false);
        });
    };

    // Zone unlink handler
    let unlink_zone = move |zone_id: String| {
        zone_match_busy.set(true);
        zone_match_feedback.set(None);
        spawn(async move {
            let req = ZoneUnlinkRequest { zone_id };
            match api::post_json_no_response("/hqp/zones/unlink", &req).await {
                Ok(()) => {
                    zone_match_feedback.set(Some(ZoneMatchFeedback::Removed));
                    zone_links.restart();
                    zones.restart();
                }
                Err(error) => {
                    zone_match_feedback.set(Some(ZoneMatchFeedback::Error(format!(
                        "Could not remove this pairing: {error}"
                    ))));
                }
            }
            zone_match_busy.set(false);
        });
    };

    let _is_loading = config.read().is_none();
    let current_status = status.read().clone().flatten();
    let current_pipeline = pipeline.read().clone().flatten();
    let profiles_list = profiles.read().clone().flatten().unwrap_or_default();
    let matrix_data = matrix.read().clone();
    let zones_list = zones_list_signal();
    let links_list = links_signal();
    let mut zone_match_key_parts = links_list
        .iter()
        .map(|link| format!("{}={}", link.zone_id, link.instance))
        .collect::<Vec<_>>();
    zone_match_key_parts.sort();
    let zone_match_key = zone_match_key_parts.join("|");
    let instances_list = instances
        .read()
        .clone()
        .flatten()
        .map(|r| r.instances)
        .unwrap_or_default();
    let zone_match_resources_loaded =
        zones_loaded_once() && zone_links_loaded_once() && instances_loaded_once();
    let controlled_zones_loaded = zones_loaded_once() && zone_links_loaded_once();
    let np_map = now_playing_map();

    let controlled_zones = controlled_zones_signal();

    let is_connected = current_status
        .as_ref()
        .map(|s| s.connected)
        .unwrap_or(false);

    rsx! {
        Layout {
            title: "HQPlayer".to_string(),
            nav_active: "hqplayer".to_string(),

            div { class: "hqp-page-heading",
                h1 { class: "text-2xl font-bold", "HQPlayer" }
                p { class: "mt-2 max-w-2xl text-sm text-muted sm:text-base",
                    "Start playback in the app you already use, then see and shape HQPlayer's live output here."
                }
            }

            // Error display
            if let Some(ref error) = hqp_error() {
                div { class: "bg-red-900/20 border border-red-500/50 rounded-lg p-4 mb-6",
                    p { class: "text-red-400 m-0", "{error}" }
                }
            }

            // If not connected, show configuration first and prominently
            if !is_connected {
                section { id: "hqp-config", class: "mb-8",
                    div { class: "card p-6",
                        div { class: "mb-5 max-w-2xl",
                            h2 { class: "text-lg font-semibold", "Connect HQPlayer" }
                            p { class: "mt-1 text-sm text-muted",
                                "Add HQPlayer Embedded once. Unified Hi-Fi Control will verify the native engine and its web artwork endpoint."
                            }
                        }
                        ol { class: "hqp-onboarding-path mb-6",
                            li {
                                span { "1" }
                                div {
                                    strong { "Connect" }
                                    small { "Verify the engine" }
                                }
                            }
                            li {
                                span { "2" }
                                div {
                                    strong { "Pair" }
                                    small { "Name the playback path" }
                                }
                            }
                            li {
                                span { "3" }
                                div {
                                    strong { "Listen & tune" }
                                    small { "Control live DSP" }
                                }
                            }
                        }
                        ConfigForm {
                            host: host,
                            port: port,
                            web_port: web_port,
                            username: username,
                            password: password,
                            has_credentials: has_credentials(),
                            config_status: config_status(),
                            on_save: save_config,
                        }
                    }
                }
            }

            // Connected: show status bar with collapsible settings
            if is_connected {
                div { class: "hqp-connection-bar mb-8",
                    div { class: "flex min-w-0 items-center gap-3",
                        span { class: "hqp-signal", aria_hidden: "true" }
                        div { class: "min-w-0",
                            p { class: "font-semibold truncate",
                                "Connected to {current_status.as_ref().and_then(|s| s.host.as_deref()).unwrap_or(\"HQPlayer\")}"
                            }
                            p { class: "mt-0.5 text-xs text-muted sm:text-sm",
                                "Live engine reads and DSP changes are verified with HQPlayer."
                            }
                        }
                    }
                    button {
                        class: "btn btn-ghost btn-sm",
                        onclick: move |_| show_config.toggle(),
                        if show_config() { "Close connection settings" } else { "Connection settings" }
                    }
                }

                // Collapsible config when connected
                if show_config() {
                    section { id: "hqp-config", class: "mb-8",
                        div { class: "card p-6",
                            ConfigForm {
                                host: host,
                                port: port,
                                web_port: web_port,
                                username: username,
                                password: password,
                                has_credentials: has_credentials(),
                                config_status: config_status(),
                                on_save: save_config,
                            }
                        }
                    }
                }
            }

            // Every direct or linked HQPlayer zone uses the same aggregator-backed control path.
            if is_connected && !controlled_zones.is_empty() {
                section { id: "hqp-zones", class: "mb-8",
                    div { class: "mb-4 max-w-3xl",
                        h2 { class: "text-lg font-semibold", "Now playing through HQPlayer" }
                        p { class: "mt-1 text-sm text-muted",
                            "Transport comes from the playback zone; sound shaping comes from HQPlayer. Paired zones keep both together."
                        }
                    }
                    div { class: "grid gap-4 grid-cols-1",
                        for zone in controlled_zones.iter() {
                            LinkedZoneCard {
                                key: "{zone.zone_id}",
                                zone: zone.clone(),
                                now_playing: np_map.get(&zone.zone_id).cloned(),
                                on_control: control,
                            }
                        }
                    }
                }
            }

            if is_connected && controlled_zones_loaded && controlled_zones.is_empty() {
                section { id: "hqp-zones-empty", class: "hqp-empty-path mb-8",
                    span { aria_hidden: "true",
                        svg { view_box: "0 0 48 24",
                            circle { cx: "8", cy: "12", r: "5" }
                            path { d: "M14 12h18" }
                            path { d: "m27 7 5 5-5 5" }
                            circle { cx: "40", cy: "12", r: "5" }
                        }
                    }
                    div { class: "min-w-0 flex-1",
                        h2 { class: "font-semibold", "Bring your playback zone into view" }
                        p { class: "mt-1 max-w-2xl text-sm text-muted",
                            "If Roon, JPLAY, or another controller already sends this zone through HQPlayer, pair their names below. Audio routing stays exactly as configured."
                        }
                    }
                    a { class: "btn btn-primary shrink-0", href: "#hqp-zone-links", "Pair a playback zone" }
                }
            }

            // DSP Settings (only if connected)
            if is_connected {
                section { id: "hqp-dsp", class: "mb-8",
                    div { class: "mb-4 max-w-3xl",
                        h2 { class: "text-lg font-semibold", "Shape the sound" }
                        p { class: "mt-1 text-sm text-muted",
                            "Playing now is measured from HQPlayer's engine. The controls below set the pipeline and confirm what HQPlayer accepted."
                        }
                    }
                    DspSettings {
                        pipeline: current_pipeline,
                        profiles: profiles_list,
                        matrix: matrix_data,
                        loading: hqp_loading(),
                        on_set_pipeline: set_pipeline,
                        on_load_profile: load_profile,
                        on_set_matrix: set_matrix,
                    }
                }
            }

            // Pairing is useful only after HQPlayer itself is connected. Keeping it hidden during
            // first-run setup preserves the connect → pair → listen progression above.
            if is_connected {
                section { id: "hqp-zone-links", class: "mb-8",
                    div { class: "mb-4 max-w-3xl",
                        h2 { class: "text-lg font-semibold", "Pair a playback zone" }
                        p { class: "mt-1 text-sm text-muted",
                            "Tell Unified Hi-Fi Control which playback-zone name and HQPlayer instance describe the same existing signal path."
                        }
                    }
                    div { class: "card overflow-hidden",
                        ZoneLinkTable {
                            key: "{zone_match_key}",
                            zones: zones_list,
                            links: links_list,
                            instances: instances_list,
                            resources_loaded: zone_match_resources_loaded,
                            busy: zone_match_busy(),
                            feedback: zone_match_feedback(),
                            on_link: link_zone,
                            on_unlink: unlink_zone,
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
        filter_hqp_options, format_hqp_rate, hqp_live_mode_readout, hqp_live_output_readout,
        hqp_shaper_minimum_rate_hz, hqplayer_mute_control, is_zone_match_candidate,
        normalized_match_selection, playback_control_error, playback_path_label,
        zone_match_availability, CoalescingRefresh, ZoneMatchAvailability,
    };
    use crate::app::api::{HqpOption, HqpPipelineStatus, HqpSettingOptions};

    #[test]
    fn advanced_refreshes_never_overlap_and_coalesce_bursts() {
        let mut refresh = CoalescingRefresh::default();

        assert!(
            refresh.request_initial(),
            "the mounted page starts one initial fetch"
        );
        assert!(
            !refresh.request_initial(),
            "rerenders cannot restart the initial fetch"
        );
        assert!(!refresh.request(), "an in-flight fetch is not duplicated");
        assert!(
            !refresh.request(),
            "a burst still queues only one follow-up"
        );
        assert!(
            refresh.complete(),
            "one dirty follow-up starts after completion"
        );
        assert!(!refresh.complete(), "the clean follow-up settles the gate");
        assert!(
            !refresh.request_initial(),
            "settling cannot turn a rerender into another initial fetch"
        );
        assert!(refresh.request(), "a later event can start a fresh fetch");
    }

    #[test]
    fn hqplayer_mute_matches_the_native_volume_floor_semantics() {
        let audible = hqplayer_mute_control(Some(-3.0), Some(-60.0));
        assert_eq!(audible.label, "Mute to minimum volume");
        assert!(!audible.disabled);

        let floored = hqplayer_mute_control(Some(-60.0), Some(-60.0));
        assert_eq!(floored.label, "At minimum volume");
        assert!(floored.disabled);
    }

    #[test]
    fn rejected_queue_skip_explains_the_linked_transport_fallback() {
        assert_eq!(
            playback_control_error("HTTP 500: HQPlayer rejected Next (no reason given)"),
            "HQPlayer has no usable native queue for next track. Use the linked playback zone's transport controls."
        );
        assert_eq!(
            playback_control_error("HTTP 500: HQPlayer rejected Previous: empty playlist"),
            "HQPlayer has no usable native queue for previous track. Use the linked playback zone's transport controls."
        );
    }

    #[test]
    fn active_rates_are_presented_as_human_frequencies() {
        assert_eq!(format_hqp_rate(44_100), "44.1 kHz");
        assert_eq!(format_hqp_rate(384_000), "384 kHz");
        assert_eq!(format_hqp_rate(5_644_800), "5.6448 MHz");
    }

    #[test]
    fn stopped_and_paused_engines_do_not_advertise_stale_mode_or_output() {
        for state in ["Stopped", "Paused"] {
            let status = HqpPipelineStatus {
                state: Some(state.to_string()),
                mode: Some("SDM (DSD)".to_string()),
                active_mode: Some("PCM".to_string()),
                active_rate: Some(5_644_800),
                ..HqpPipelineStatus::default()
            };

            assert_eq!(hqp_live_mode_readout(&status), "—");
            assert_eq!(hqp_live_output_readout(&status), "—");
        }
    }

    #[test]
    fn playing_engine_readouts_use_only_active_mode_and_rate() {
        let status = HqpPipelineStatus {
            state: Some("Playing".to_string()),
            mode: Some("PCM".to_string()),
            active_mode: Some("SDM (DSD)".to_string()),
            active_rate: Some(5_644_800),
            ..HqpPipelineStatus::default()
        };

        assert_eq!(hqp_live_mode_readout(&status), "SDM (DSD)");
        assert_eq!(hqp_live_output_readout(&status), "5.6448 MHz");
    }

    #[test]
    fn filter_search_keeps_the_current_choice_visible_without_lying_about_matches() {
        let options = vec![
            HqpOption {
                value: "poly-sinc-gauss-long".to_string(),
                label: None,
                disabled: false,
                reason: None,
            },
            HqpOption {
                value: "poly-sinc-ext2-hires-lp".to_string(),
                label: None,
                disabled: false,
                reason: None,
            },
            HqpOption {
                value: "IIR".to_string(),
                label: None,
                disabled: false,
                reason: None,
            },
        ];

        let filtered = filter_hqp_options(&options, "hires", "poly-sinc-gauss-long");
        assert_eq!(
            filtered
                .iter()
                .map(|option| option.value.as_str())
                .collect::<Vec<_>>(),
            vec!["poly-sinc-gauss-long", "poly-sinc-ext2-hires-lp"]
        );
    }

    #[test]
    fn hqptuner_rate_gates_are_applied_to_rate_specific_modulators() {
        assert_eq!(hqp_shaper_minimum_rate_hz("DSD7 256+fs"), Some(10_240_000));
        assert_eq!(
            hqp_shaper_minimum_rate_hz("ASDM7EC-fast 512+fs"),
            Some(22_579_200)
        );
        assert_eq!(
            hqp_shaper_minimum_rate_hz("AMSDM7EC 512+fs"),
            Some(20_480_000)
        );
        assert_eq!(hqp_shaper_minimum_rate_hz("AHM7EC8B"), Some(40_960_000));
        assert_eq!(hqp_shaper_minimum_rate_hz("ASDM7EC-fast"), None);
    }

    #[test]
    fn rate_specific_modulators_stay_visible_but_explain_why_they_are_unavailable() {
        let options = HqpSettingOptions {
            options: vec![
                HqpOption {
                    value: "ASDM7EC-fast".to_string(),
                    label: Some("ASDM7EC-fast".to_string()),
                    disabled: false,
                    reason: None,
                },
                HqpOption {
                    value: "ASDM7EC-fast 512+fs".to_string(),
                    label: Some("ASDM7EC-fast 512+fs".to_string()),
                    disabled: false,
                    reason: None,
                },
            ],
            selected: None,
        };

        let guided = super::apply_hqp_shaper_rate_guidance(options, 11_289_600);
        assert!(!guided.options[0].disabled);
        assert!(guided.options[1].disabled);
        assert_eq!(
            guided.options[1].reason.as_deref(),
            Some("needs at least 22.5792 MHz")
        );
    }

    #[test]
    fn zone_matching_distinguishes_loading_from_an_empty_system() {
        assert_eq!(
            zone_match_availability(false, 0, 0),
            ZoneMatchAvailability::Loading,
            "pending zone, link, and instance reads are not an empty system"
        );
    }

    #[test]
    fn zone_matching_only_offers_unmatched_playback_zones() {
        assert!(is_zone_match_candidate(
            "roon:hqplayer-dsp",
            Some("roon"),
            false
        ));
        assert!(!is_zone_match_candidate(
            "hqplayer:default",
            Some("hqplayer"),
            false
        ));
        assert!(!is_zone_match_candidate(
            "roon:already-matched",
            Some("roon"),
            true
        ));
    }

    #[test]
    fn zone_matching_moves_selection_after_the_selected_zone_is_saved() {
        let choices = vec!["roon:bedroom".to_string(), "roon:patio".to_string()];
        assert_eq!(
            normalized_match_selection("roon:just-saved", &choices),
            "roon:bedroom",
            "a saved zone disappears from the candidate list, so the form must select a valid next choice"
        );
    }

    #[test]
    fn playback_paths_name_direct_and_paired_control_without_implying_routing() {
        assert_eq!(playback_path_label(Some("hqplayer")), "Direct HQPlayer");
        assert_eq!(playback_path_label(Some("roon")), "Roon + HQPlayer DSP");
        assert_eq!(playback_path_label(None), "Playback + HQPlayer DSP");
    }
}

/// Linked zone card with playback controls
#[derive(Debug, PartialEq, Eq)]
struct HqplayerMuteControl {
    label: &'static str,
    title: &'static str,
    disabled: bool,
}

fn hqplayer_mute_control(volume: Option<f32>, volume_min: Option<f32>) -> HqplayerMuteControl {
    let at_volume_floor = match (volume, volume_min) {
        (Some(value), Some(minimum)) => value <= minimum + 0.01,
        _ => false,
    };

    if at_volume_floor {
        HqplayerMuteControl {
            label: "At minimum volume",
            title: "HQPlayer represents mute as its minimum volume",
            disabled: true,
        }
    } else {
        HqplayerMuteControl {
            label: "Mute to minimum volume",
            title: "Mute to HQPlayer's minimum volume",
            disabled: false,
        }
    }
}

#[component]
fn LinkedZoneCard(
    zone: Zone,
    now_playing: Option<NowPlaying>,
    on_control: EventHandler<(String, String, Option<f64>)>,
) -> Element {
    let zone_id = zone.zone_id.clone();
    let zone_id_prev = zone_id.clone();
    let zone_id_play = zone_id.clone();
    let zone_id_next = zone_id.clone();
    let zone_id_stop = zone_id.clone();
    let zone_id_mute = zone_id.clone();
    let zone_id_seek = zone_id.clone();
    let zone_id_vol_down = zone_id.clone();
    let zone_id_vol_up = zone_id.clone();

    let np = now_playing.as_ref();
    let is_playing = np.map(|n| n.is_playing).unwrap_or(false);

    let volume = np.and_then(|n| n.volume);
    let volume_min = np.and_then(|n| n.volume_min);
    let volume_type = np.and_then(|n| n.volume_type.clone());
    let volume_step = np.and_then(|n| n.volume_step);
    let seek_position = np.and_then(|n| n.seek_position).unwrap_or(0).max(0) as u32;
    let length = np.and_then(|n| n.length).unwrap_or(0);
    let can_seek = length > 0;
    let can_previous = np.map(|n| n.is_previous_allowed).unwrap_or(false);
    let can_next = np.map(|n| n.is_next_allowed).unwrap_or(false);
    let mute_control = hqplayer_mute_control(volume, volume_min);

    // Album art
    let base_image_url = np.and_then(|n| n.image_url.clone()).unwrap_or_default();
    let image_key = np.and_then(|n| n.image_key.clone());
    let image_url = if let Some(key) = image_key {
        let sep = if base_image_url.contains('?') {
            "&"
        } else {
            "?"
        };
        format!("{}{}k={}", base_image_url, sep, key)
    } else {
        base_image_url
    };
    let has_image = !image_url.is_empty();
    // #581: map the origin-absolute art path onto the runtime base path so
    // it survives an ingress prefix (identity in direct mode).
    let image_url = crate::app::base_path::href(&image_url);

    let (track, artist) = np
        .map(|n| {
            if n.line1.as_deref().unwrap_or("Idle") != "Idle" {
                (
                    n.line1.clone().unwrap_or_default(),
                    n.line2.clone().unwrap_or_default(),
                )
            } else {
                (String::new(), String::new())
            }
        })
        .unwrap_or_default();
    let path_label = playback_path_label(zone.source.as_deref());

    rsx! {
        article { class: "card p-4 sm:p-5",
            div { class: "flex flex-col gap-4 sm:flex-row sm:items-start",
                // Album art
                if has_image {
                    img {
                        src: "{image_url}",
                        alt: "Album art",
                        class: "hqp-album-art w-20 h-20 sm:w-24 sm:h-24 object-cover rounded-lg bg-elevated flex-shrink-0"
                    }
                } else {
                    div { class: "w-20 h-20 sm:w-24 sm:h-24 rounded-lg bg-elevated flex items-center justify-center text-muted text-2xl flex-shrink-0",
                        "♪"
                    }
                }

                // Info + controls
                div { class: "flex-1 min-w-0",
                    div { class: "mb-2 flex flex-wrap items-center gap-2",
                        h3 { class: "text-base font-semibold truncate", "{zone.zone_name}" }
                        span { class: "badge badge-secondary", "{path_label}" }
                    }

                    if !track.is_empty() {
                        p { class: "text-sm truncate mb-0.5", "{track}" }
                        p { class: "text-sm text-muted truncate", "{artist}" }
                    } else {
                        p { class: "text-sm text-muted", "Nothing playing" }
                    }

                    // Transport controls
                    div { class: "flex flex-wrap items-center gap-2 mt-3",
                        button {
                            class: "btn btn-ghost btn-sm",
                            "aria-label": "Previous track",
                            disabled: !can_previous,
                            onclick: move |_| on_control.call((zone_id_prev.clone(), "previous".to_string(), None)),
                            svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                path { d: "M6 6h2v12H6zm3.5 6l8.5 6V6z" }
                            }
                        }
                        button {
                            class: "btn btn-primary btn-sm",
                            "aria-label": if is_playing { "Pause" } else { "Play" },
                            onclick: move |_| on_control.call((zone_id_play.clone(), "play_pause".to_string(), None)),
                            if is_playing {
                                svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                    path { d: "M6 19h4V5H6v14zm8-14v14h4V5h-4z" }
                                }
                            } else {
                                svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                    path { d: "M8 5v14l11-7z" }
                                }
                            }
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            "aria-label": "Next track",
                            disabled: !can_next,
                            onclick: move |_| on_control.call((zone_id_next.clone(), "next".to_string(), None)),
                            svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                path { d: "M6 18l8.5-6L6 6v12zM16 6v12h2V6h-2z" }
                            }
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            "aria-label": "Stop playback",
                            title: "Stop playback",
                            onclick: move |_| on_control.call((zone_id_stop.clone(), "stop".to_string(), None)),
                            svg { class: "w-4 h-4", fill: "currentColor", view_box: "0 0 24 24",
                                rect { x: "6", y: "6", width: "12", height: "12", rx: "1" }
                            }
                        }
                        button {
                            class: "btn btn-ghost btn-sm",
                            "aria-label": mute_control.label,
                            title: mute_control.title,
                            disabled: mute_control.disabled,
                            onclick: move |_| on_control.call((zone_id_mute.clone(), "mute".to_string(), None)),
                            svg { class: "w-4 h-4", fill: "none", stroke: "currentColor", stroke_width: "2", view_box: "0 0 24 24",
                                path { d: "M11 5 6 9H3v6h3l5 4V5Z" }
                                path { d: "m19 9-6 6m0-6 6 6" }
                            }
                        }

                        VolumeControlsCompact {
                            volume: volume,
                            volume_type: volume_type,
                            volume_step: volume_step,
                            on_vol_down: move |_| on_control.call((zone_id_vol_down.clone(), "vol_down".to_string(), None)),
                            on_vol_up: move |_| on_control.call((zone_id_vol_up.clone(), "vol_up".to_string(), None)),
                        }
                    }
                }
            }
            div { class: "mt-4 pt-4 border-t border-subtle",
                div { class: "flex items-center justify-between gap-4 mb-2",
                    label { class: "text-sm font-medium", r#for: "seek-{zone_id}", "Position" }
                    span { class: "text-xs text-muted tabular-nums",
                        "{format_hqp_time(seek_position)} / {format_hqp_time(length)}"
                    }
                }
                input {
                    id: "seek-{zone_id}",
                    class: "hqp-seek",
                    r#type: "range",
                    min: "0",
                    max: "{length.max(1)}",
                    value: "{seek_position.min(length)}",
                    disabled: !can_seek,
                    "aria-label": "Seek position",
                    onchange: move |event| {
                        if let Ok(position) = event.value().parse::<f64>() {
                            on_control.call((zone_id_seek.clone(), "seek".to_string(), Some(position)));
                        }
                    }
                }
                if !can_seek {
                    p { class: "text-xs text-muted mt-2", "Seek becomes available when HQPlayer reports a track duration." }
                }
            }
        }
    }
}

fn format_hqp_time(seconds: u32) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

/// Configuration form component
#[component]
fn ConfigForm(
    host: Signal<String>,
    port: Signal<u16>,
    web_port: Signal<u16>,
    username: Signal<String>,
    password: Signal<String>,
    has_credentials: bool,
    config_status: Option<String>,
    on_save: EventHandler<()>,
) -> Element {
    rsx! {
        div { class: "space-y-4",
            div {
                label { class: "block text-sm font-medium mb-1", r#for: "hqp-host", "HQPlayer host" }
                input {
                    id: "hqp-host",
                    class: "input",
                    r#type: "text",
                    placeholder: "192.168.1.100",
                    value: "{host}",
                    oninput: move |evt| host.set(evt.value())
                }
                p { class: "mt-1 text-xs text-muted", "The hostname or LAN address running HQPlayer Embedded." }
            }
            div { class: "grid grid-cols-1 sm:grid-cols-2 gap-4",
                div {
                    label { class: "block text-sm font-medium mb-1", r#for: "hqp-native-port", "Engine control port" }
                    input {
                        id: "hqp-native-port",
                        class: "input",
                        r#type: "number",
                        value: "{port}",
                        oninput: move |evt| {
                            if let Ok(p) = evt.value().parse() {
                                port.set(p);
                            }
                        }
                    }
                    p { class: "mt-1 text-xs text-muted", "Usually 4321. Used for playback and DSP control." }
                }
                div {
                    label { class: "block text-sm font-medium mb-1", r#for: "hqp-web-port", "Web and artwork port" }
                    input {
                        id: "hqp-web-port",
                        class: "input",
                        r#type: "number",
                        value: "{web_port}",
                        oninput: move |evt| {
                            if let Ok(p) = evt.value().parse() {
                                web_port.set(p);
                            }
                        }
                    }
                    p { class: "mt-1 text-xs text-muted", "Usually 8088. Used for profiles and current cover art." }
                }
            }
            div { class: "pt-1",
                p { class: "text-sm font-medium", "Web sign-in" }
                p { class: "mt-1 text-xs text-muted", "Optional. Leave blank unless HQPlayer's web interface requires credentials." }
            }
            div { class: "grid grid-cols-1 sm:grid-cols-2 gap-4",
                div {
                    label { class: "block text-sm font-medium mb-1", r#for: "hqp-username", "Username" }
                    input {
                        id: "hqp-username",
                        class: "input",
                        r#type: "text",
                        placeholder: if has_credentials { "(saved)" } else { "admin" },
                        value: "{username}",
                        oninput: move |evt| username.set(evt.value())
                    }
                }
                div {
                    label { class: "block text-sm font-medium mb-1", r#for: "hqp-password", "Password" }
                    input {
                        id: "hqp-password",
                        class: "input",
                        r#type: "password",
                        placeholder: if has_credentials { "(saved)" } else { "password" },
                        value: "{password}",
                        oninput: move |evt| password.set(evt.value())
                    }
                }
            }
            div { class: "flex flex-wrap items-center gap-4 pt-1",
                button { class: "btn btn-primary", onclick: move |_| on_save.call(()), "Save and test connection" }
                if let Some(ref msg) = config_status {
                    span { class: if msg.contains("Connected") { "status-ok" } else if msg.contains("failed") || msg.starts_with("Saved, but") { "status-err" } else { "text-muted" },
                        "{msg}"
                    }
                }
            }
        }
    }
}

/// DSP Settings component with full pipeline controls
#[component]
fn DspSettings(
    pipeline: Option<HqpPipeline>,
    profiles: Vec<HqpProfile>,
    matrix: Option<HqpMatrixProfilesResponse>,
    loading: bool,
    on_set_pipeline: EventHandler<(String, String)>,
    on_load_profile: EventHandler<String>,
    on_set_matrix: EventHandler<String>,
) -> Element {
    let Some(ref pipe) = pipeline else {
        return rsx! {
            div { class: "card p-6",
                p { class: "text-muted text-center py-4", aria_busy: "true",
                    "Loading DSP settings..."
                }
            }
        };
    };

    let settings = pipe.settings.as_ref();

    let mode_opts = settings.and_then(|s| s.mode.clone());
    let samplerate_opts = settings.and_then(|s| s.samplerate.clone());
    let filter1x_opts = settings.and_then(|s| s.filter1x.clone());
    let filter_nx_opts = settings.and_then(|s| s.filter_nx.clone());
    let shaper_opts = settings.and_then(|s| s.shaper.clone()).map(|options| {
        let selected_rate = samplerate_opts
            .as_ref()
            .and_then(|rates| rates.selected.as_ref())
            .and_then(|rate| rate.value.parse::<u32>().ok())
            .unwrap_or_default();
        apply_hqp_shaper_rate_guidance(options, selected_rate)
    });

    let has_matrix = matrix.is_some();
    let matrix_profiles = matrix
        .as_ref()
        .map(|m| m.profiles.clone())
        .unwrap_or_default();
    let matrix_current = matrix
        .as_ref()
        .and_then(|m| m.current.as_ref().map(|profile| profile.name.clone()));
    let junk_filter_opts = matrix.as_ref().and_then(|advanced| {
        if advanced.junk_filters.is_empty() {
            return None;
        }
        let selected = advanced.junk_filter.and_then(|selected_index| {
            advanced
                .junk_filters
                .iter()
                .find(|choice| choice.index == selected_index)
                .map(|choice| crate::app::api::HqpOption {
                    value: choice.name.clone(),
                    label: Some(choice.name.clone()),
                    disabled: false,
                    reason: None,
                })
        });
        Some(crate::app::api::HqpSettingOptions {
            options: advanced
                .junk_filters
                .iter()
                .map(|choice| crate::app::api::HqpOption {
                    value: choice.name.clone(),
                    label: Some(choice.name.clone()),
                    disabled: false,
                    reason: None,
                })
                .collect(),
            selected,
        })
    });
    let repeat_opts = matrix.as_ref().and_then(|advanced| {
        advanced.repeat.map(|selected| {
            let choices = [("off", "Off"), ("one", "Current track"), ("all", "All")];
            crate::app::api::HqpSettingOptions {
                options: choices
                    .iter()
                    .map(|(value, label)| crate::app::api::HqpOption {
                        value: (*value).to_string(),
                        label: Some((*label).to_string()),
                        disabled: false,
                        reason: None,
                    })
                    .collect(),
                selected: choices.get(usize::from(selected)).map(|(value, label)| {
                    crate::app::api::HqpOption {
                        value: (*value).to_string(),
                        label: Some((*label).to_string()),
                        disabled: false,
                        reason: None,
                    }
                }),
            }
        })
    });
    let convolution = matrix.as_ref().and_then(|advanced| advanced.convolution);
    let adaptive_volume = matrix
        .as_ref()
        .and_then(|advanced| advanced.adaptive_volume);
    let random = matrix.as_ref().and_then(|advanced| advanced.random);

    let shaper_label = settings
        .and_then(|settings| settings.shaper_label.clone())
        .unwrap_or_else(|| "Dither / modulator".to_string());

    rsx! {
        div { class: "card p-6",
            // Loading indicator
            if loading {
                div { class: "flex items-center gap-2 mb-4",
                    span { class: "text-muted text-sm", aria_busy: "true", "Updating..." }
                }
            }

            if let Some(status) = pipe.status.as_ref() {
                div { class: "mb-6 pb-6 border-b border-subtle",
                    div { class: "mb-3 flex flex-wrap items-baseline justify-between gap-2",
                        h3 { class: "text-sm font-semibold", "Playing now" }
                        p { class: "text-xs text-muted", "Live engine readback" }
                    }
                    dl { class: "grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-3 lg:grid-cols-5",
                        HqpReadout { label: "Engine", value: status.state.clone().unwrap_or_else(|| "Unknown".to_string()) }
                        HqpReadout { label: "Mode", value: hqp_live_mode_readout(status) }
                        HqpReadout { label: "Filter", value: status.active_filter.clone().unwrap_or_else(|| "—".to_string()) }
                        HqpReadout { label: "Dither / modulator", value: status.active_shaper.clone().unwrap_or_else(|| "—".to_string()) }
                        HqpReadout { label: "Output", value: hqp_live_output_readout(status) }
                    }
                }
            }

            // Profile selectors
            if !profiles.is_empty() || has_matrix {
                div { class: "mb-6",
                    div { class: "mb-3",
                        h3 { class: "text-sm font-semibold", "Recall a saved setup" }
                        p { class: "mt-1 text-xs text-muted", "Loading a profile can change several pipeline controls at once." }
                    }
                    div { class: "grid grid-cols-1 sm:grid-cols-2 gap-4",
                    if !profiles.is_empty() {
                        label { class: "block",
                            span { class: "block text-sm font-medium mb-1", "Profile" }
                            HqpProfileSelect {
                                profiles: profiles.clone(),
                                on_select: on_load_profile,
                                disabled: loading,
                            }
                        }
                    }
                    if has_matrix {
                        label { class: "block",
                            span { class: "block text-sm font-medium mb-1", "Matrix" }
                            HqpMatrixSelect {
                                profiles: matrix_profiles,
                                active: matrix_current,
                                on_select: on_set_matrix,
                                disabled: loading,
                            }
                        }
                    }
                    }
                }
            }

            div { class: "mb-3",
                h3 { class: "text-sm font-semibold", "Output pipeline" }
                p { class: "mt-1 text-xs text-muted",
                    "Mode and rate work as a pair. When you return to PCM or SDM, your last verified rate for that mode is restored."
                }
            }
            div { class: "grid grid-cols-1 sm:grid-cols-2 lg:grid-cols-3 gap-4",
                HqpSelect {
                    id: "hqp-mode",
                    label: "Mode",
                    setting: "mode",
                    options: mode_opts,
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-samplerate",
                    label: "Sample Rate",
                    setting: "samplerate",
                    options: samplerate_opts,
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-filter1x",
                    label: "Filter (1x)",
                    setting: "filter1x",
                    options: filter1x_opts,
                    searchable: true,
                    hint: "Base-rate sources below 50 kHz. Search by family or suffix, such as gauss, hires, -lp, -mp, or -ip.",
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-filterNx",
                    label: "Filter (Nx)",
                    setting: "filterNx",
                    options: filter_nx_opts,
                    searchable: true,
                    hint: "Sources at 2× and above. Search by family or suffix, such as gauss, hires, -lp, -mp, or -ip.",
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
                HqpSelect {
                    id: "hqp-shaper",
                    label: shaper_label,
                    setting: "shaper",
                    options: shaper_opts,
                    searchable: true,
                    hint: "PCM uses dither/noise shaping; SDM uses a sigma-delta modulator. Names ending 256+fs or 512+fs require those output-rate tiers.",
                    disabled: loading,
                    on_change: on_set_pipeline,
                }
            }

            if junk_filter_opts.is_some() || convolution.is_some() || adaptive_volume.is_some() || repeat_opts.is_some() || random.is_some() {
                div { class: "mt-6 pt-6 border-t border-subtle",
                    div { class: "mb-4",
                        h3 { class: "text-sm font-semibold", "Advanced processing" }
                        p { class: "text-sm text-muted mt-1 max-w-prose",
                            "Immediate HQPlayer engine controls. Changes are verified against the live native state before the UI refreshes."
                        }
                    }
                    div { class: "grid grid-cols-1 gap-4 sm:grid-cols-2 lg:grid-cols-3",
                        HqpSelect {
                            id: "hqp-junk-filter",
                            label: "Junk filter",
                            setting: "junk_filter",
                            options: junk_filter_opts,
                            disabled: loading,
                            on_change: on_set_pipeline,
                        }
                        HqpSelect {
                            id: "hqp-repeat",
                            label: "Repeat",
                            setting: "repeat",
                            options: repeat_opts,
                            disabled: loading,
                            on_change: on_set_pipeline,
                        }
                        if let Some(enabled) = convolution {
                            HqpToggle {
                                label: "Convolution",
                                description: "Apply the active convolution engine.",
                                setting: "convolution",
                                enabled,
                                disabled: loading,
                                on_change: on_set_pipeline,
                            }
                        }
                        if let Some(enabled) = adaptive_volume {
                            HqpToggle {
                                label: "Adaptive volume",
                                description: "Allow HQPlayer to adjust level dynamically.",
                                setting: "adaptive_volume",
                                enabled,
                                disabled: loading,
                                on_change: on_set_pipeline,
                            }
                        }
                        if let Some(enabled) = random {
                            HqpToggle {
                                label: "Random",
                                description: "Randomize HQPlayer playlist playback.",
                                setting: "random",
                                enabled,
                                disabled: loading,
                                on_change: on_set_pipeline,
                            }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn HqpReadout(label: &'static str, value: String) -> Element {
    rsx! {
        div { class: "min-w-0",
            dt { class: "text-xs text-muted", "{label}" }
            dd { class: "mt-1 text-sm font-medium truncate", title: "{value}", "{value}" }
        }
    }
}

fn format_hqp_rate(rate: u64) -> String {
    let (scaled, unit, precision) = if rate >= 1_000_000 {
        (rate as f64 / 1_000_000.0, "MHz", 4)
    } else if rate >= 1_000 {
        (rate as f64 / 1_000.0, "kHz", 3)
    } else {
        return format!("{rate} Hz");
    };
    let mut number = format!("{scaled:.precision$}");
    while number.contains('.') && number.ends_with('0') {
        number.pop();
    }
    if number.ends_with('.') {
        number.pop();
    }
    format!("{number} {unit}")
}

fn hqp_engine_is_playing(status: &crate::app::api::HqpPipelineStatus) -> bool {
    status.state.as_deref() == Some("Playing")
}

fn hqp_live_mode_readout(status: &crate::app::api::HqpPipelineStatus) -> String {
    if !hqp_engine_is_playing(status) {
        return "—".to_string();
    }
    status
        .active_mode
        .as_deref()
        .filter(|mode| !mode.trim().is_empty())
        .unwrap_or("—")
        .to_string()
}

fn hqp_live_output_readout(status: &crate::app::api::HqpPipelineStatus) -> String {
    if !hqp_engine_is_playing(status) {
        return "—".to_string();
    }
    status
        .active_rate
        .filter(|rate| *rate > 0)
        .map(format_hqp_rate)
        .unwrap_or_else(|| "—".to_string())
}

#[component]
fn HqpToggle(
    label: &'static str,
    description: &'static str,
    setting: &'static str,
    enabled: bool,
    disabled: bool,
    on_change: EventHandler<(String, String)>,
) -> Element {
    let state_label = if enabled { "on" } else { "off" };
    rsx! {
        div { class: "hqp-toggle",
            div { class: "min-w-0 pr-3",
                p { class: "text-sm font-medium", "{label}" }
                p { class: "text-xs text-muted mt-1", "{description}" }
            }
            button {
                class: if enabled { "btn btn-primary btn-sm min-w-16" } else { "btn btn-outline btn-sm min-w-16" },
                disabled,
                "aria-pressed": enabled,
                "aria-label": "{label}: {state_label}",
                onclick: move |_| on_change.call((setting.to_string(), (!enabled).to_string())),
                if enabled { "On" } else { "Off" }
            }
        }
    }
}

/// HQPlayer setting select component
#[component]
fn HqpSelect(
    id: &'static str,
    label: String,
    setting: &'static str,
    options: Option<crate::app::api::HqpSettingOptions>,
    #[props(default = false)] searchable: bool,
    #[props(default)] hint: Option<&'static str>,
    #[props(default = false)] disabled: bool,
    on_change: EventHandler<(String, String)>,
) -> Element {
    let opts_list = options
        .as_ref()
        .map(|o| o.options.clone())
        .unwrap_or_default();
    let selected = options
        .as_ref()
        .and_then(|o| o.selected.as_ref())
        .map(|s| s.value.clone())
        .unwrap_or_default();
    let mut query = use_signal(String::new);
    let visible_options = if searchable {
        filter_hqp_options(&opts_list, &query(), &selected)
    } else {
        opts_list.clone()
    };
    let total_options = opts_list.len();
    let setting_name = setting.to_string();

    rsx! {
        label {
            span { class: "block text-sm font-medium mb-1", "{label}" }
            if searchable {
                input {
                    r#type: "search",
                    class: "input mb-2",
                    value: "{query}",
                    placeholder: "Search {total_options} choices…",
                    disabled: disabled,
                    aria_label: "Search {label}",
                    oninput: move |evt| query.set(evt.value()),
                }
            }
            select {
                id: "{id}",
                class: "input",
                disabled: disabled,
                onchange: move |evt: Event<FormData>| {
                    let value = evt.value();
                    on_change.call((setting_name.clone(), value));
                },
                for opt in visible_options {
                    option {
                        value: "{opt.value}",
                        selected: opt.value == selected,
                        disabled: opt.disabled,
                        if let Some(reason) = opt.reason.as_deref() {
                            "{opt.label.as_deref().unwrap_or(&opt.value)} — {reason}"
                        } else {
                            "{opt.label.as_deref().unwrap_or(&opt.value)}"
                        }
                    }
                }
            }
            if let Some(hint) = hint {
                span { class: "block text-xs text-muted mt-1", "{hint}" }
            }
        }
    }
}

fn filter_hqp_options(
    options: &[crate::app::api::HqpOption],
    query: &str,
    selected: &str,
) -> Vec<crate::app::api::HqpOption> {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return options.to_vec();
    }
    options
        .iter()
        .filter(|option| {
            option.value == selected
                || option.value.to_lowercase().contains(&needle)
                || option
                    .label
                    .as_deref()
                    .is_some_and(|label| label.to_lowercase().contains(&needle))
        })
        .cloned()
        .collect()
}

fn hqp_shaper_minimum_rate_hz(name: &str) -> Option<u32> {
    // HQPTuner's 1.7.0 metadata keeps these choices visible and applies the documented family
    // floors. The native shaper enumeration has names but no constraint attributes, so match only
    // the explicit rate-bearing families and leave every unknown future choice available.
    if name.starts_with("AHM") {
        Some(40_960_000)
    } else if name.starts_with("AMSDM") && name.contains("512+fs") {
        Some(20_480_000)
    } else if name.contains("512+fs") {
        Some(22_579_200)
    } else if name.contains("256+fs") {
        Some(10_240_000)
    } else {
        None
    }
}

fn apply_hqp_shaper_rate_guidance(
    mut options: crate::app::api::HqpSettingOptions,
    selected_rate: u32,
) -> crate::app::api::HqpSettingOptions {
    if selected_rate == 0 {
        return options;
    }
    for option in &mut options.options {
        if let Some(minimum) = hqp_shaper_minimum_rate_hz(&option.value) {
            if selected_rate < minimum {
                option.disabled = true;
                option.reason = Some(format!(
                    "needs at least {}",
                    format_hqp_rate(u64::from(minimum))
                ));
            }
        }
    }
    options
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZoneMatchAvailability {
    Loading,
    Ready,
    NoPlaybackZones,
    NoInstances,
}

fn zone_match_availability(
    resources_loaded: bool,
    candidate_count: usize,
    instance_count: usize,
) -> ZoneMatchAvailability {
    if !resources_loaded {
        ZoneMatchAvailability::Loading
    } else if instance_count == 0 {
        ZoneMatchAvailability::NoInstances
    } else if candidate_count == 0 {
        ZoneMatchAvailability::NoPlaybackZones
    } else {
        ZoneMatchAvailability::Ready
    }
}

fn is_zone_match_candidate(zone_id: &str, source: Option<&str>, already_linked: bool) -> bool {
    !already_linked && source != Some("hqplayer") && !zone_id.starts_with("hqplayer:")
}

fn normalized_match_selection(current: &str, choices: &[String]) -> String {
    if choices.iter().any(|choice| choice == current) {
        current.to_string()
    } else {
        choices.first().cloned().unwrap_or_default()
    }
}

fn playback_source_label(source: Option<&str>) -> String {
    match source {
        Some("roon") => "Roon".to_string(),
        Some("lms") => "LMS".to_string(),
        Some("openhome") => "OpenHome".to_string(),
        Some("upnp") => "UPnP".to_string(),
        Some(other) => other.to_string(),
        None => "Playback zone".to_string(),
    }
}

fn playback_control_error(error: &str) -> String {
    for (element, action) in [("Next", "next track"), ("Previous", "previous track")] {
        if error.contains(&format!("HQPlayer rejected {element}")) {
            return format!(
                "HQPlayer has no usable native queue for {action}. Use the linked playback zone's transport controls."
            );
        }
    }
    format!("Playback control failed: {error}")
}

fn playback_path_label(source: Option<&str>) -> String {
    match source {
        Some("hqplayer") => "Direct HQPlayer".to_string(),
        None => "Playback + HQPlayer DSP".to_string(),
        _ => format!("{} + HQPlayer DSP", playback_source_label(source)),
    }
}

/// Shows every persisted source-zone match and lets the user add another one.
#[component]
fn ZoneLinkTable(
    zones: Vec<Zone>,
    links: Vec<ZoneLink>,
    instances: Vec<HqpInstance>,
    resources_loaded: bool,
    busy: bool,
    feedback: Option<ZoneMatchFeedback>,
    on_link: EventHandler<(String, String)>,
    on_unlink: EventHandler<String>,
) -> Element {
    let mut current_links = links.clone();
    current_links.sort_by(|left, right| left.zone_id.cmp(&right.zone_id));

    let mut candidates = zones
        .iter()
        .filter(|zone| {
            let already_linked = links.iter().any(|link| link.zone_id == zone.zone_id);
            is_zone_match_candidate(&zone.zone_id, zone.source.as_deref(), already_linked)
        })
        .cloned()
        .collect::<Vec<_>>();
    candidates.sort_by(|left, right| left.zone_name.cmp(&right.zone_name));
    let candidate_ids = candidates
        .iter()
        .map(|zone| zone.zone_id.clone())
        .collect::<Vec<_>>();

    let default_instance = instances
        .first()
        .map(|i| i.name.clone())
        .unwrap_or_default();
    let first_zone_id = candidates
        .first()
        .map(|zone| zone.zone_id.clone())
        .unwrap_or_default();
    let mut selected_zone = use_signal(|| first_zone_id.clone());
    let mut selected_instance = use_signal(|| default_instance.clone());
    let candidate_ids_for_click = candidate_ids.clone();
    let default_instance_for_click = default_instance.clone();
    let availability = zone_match_availability(resources_loaded, candidates.len(), instances.len());
    let feedback_copy = feedback.as_ref().map(|state| match state {
        ZoneMatchFeedback::Saved => (
            false,
            "Paired. Playback and DSP now share one control card.".to_string(),
        ),
        ZoneMatchFeedback::Removed => (
            false,
            "Pairing removed. Audio routing was not changed.".to_string(),
        ),
        ZoneMatchFeedback::Error(message) => (true, message.clone()),
    });

    rsx! {
        div { class: "px-5 py-5 sm:px-6",
            div { class: "hqp-pairing-explainer mb-6",
                div { class: "hqp-path-node",
                    span { "Playback zone" }
                    small { "Roon, JPLAY, LMS…" }
                }
                span { aria_hidden: "true",
                    svg { view_box: "0 0 40 16",
                        path { d: "M2 8h32" }
                        path { d: "m29 3 5 5-5 5" }
                    }
                }
                div { class: "hqp-path-node",
                    span { "HQPlayer" }
                    small { "DSP and output" }
                }
            }
            p { class: "mb-6 max-w-3xl text-sm text-muted",
                strong { class: "text-primary", "Pairing only changes this screen. " }
                "It does not route audio, group rooms, or change either app's configuration."
            }

            if current_links.is_empty()
                && matches!(
                    availability,
                    ZoneMatchAvailability::Ready | ZoneMatchAvailability::NoPlaybackZones
                )
            {
                div { class: "mb-5",
                    h3 { class: "font-semibold", "Nothing paired yet" }
                    p { class: "mt-1 text-sm text-muted",
                        "Choose the zone where you start playback and the HQPlayer instance it already feeds."
                    }
                }
            }

            if !current_links.is_empty() {
                div { class: "mb-6",
                    h3 { class: "font-semibold", "Paired playback paths" }
                    p { class: "mt-1 text-sm text-muted",
                        "These pairings are stored by Unified Hi-Fi Control and remain after restarts."
                    }
                    div { class: "mt-4 divide-y divide-[var(--border-default)]",
                        for link in current_links.iter() {
                            {
                let zone_name = zones
                    .iter()
                    .find(|z| z.zone_id == link.zone_id)
                    .map(|z| z.zone_name.clone())
                    .unwrap_or_else(|| link.zone_id.clone());
                                let source = zones
                                    .iter()
                                    .find(|z| z.zone_id == link.zone_id)
                                    .and_then(|z| z.source.as_deref());
                                let source_label = playback_source_label(source);
                                let instance_detail = instances
                                    .iter()
                                    .find(|instance| instance.name == link.instance)
                                    .and_then(|instance| instance.host.as_deref())
                                    .map(|host| format!("{} at {}", link.instance, host))
                                    .unwrap_or_else(|| link.instance.clone());
                let zone_id = link.zone_id.clone();
                rsx! {
                                    div { class: "flex flex-col gap-3 py-4 first:pt-0 last:pb-0 lg:flex-row lg:items-center lg:justify-between",
                                        div { class: "hqp-saved-path min-w-0 flex-1",
                                            div { class: "min-w-0",
                                                span { class: "badge badge-secondary mb-1", "{source_label}" }
                                                p { class: "font-semibold truncate", title: "{zone_name}", "{zone_name}" }
                                            }
                                            span { aria_hidden: "true",
                                                svg { view_box: "0 0 40 16",
                                                    path { d: "M2 8h32" }
                                                    path { d: "m29 3 5 5-5 5" }
                                                }
                                            }
                                            div { class: "min-w-0",
                                                span { class: "badge badge-secondary mb-1", "HQPlayer" }
                                                p { class: "font-semibold truncate", title: "{instance_detail}", "{instance_detail}" }
                                            }
                                        }
                                        button {
                                            r#type: "button",
                                            class: "btn btn-outline btn-sm shrink-0",
                                            disabled: busy,
                                            aria_label: "Remove pairing for {zone_name}",
                                            onclick: move |_| on_unlink.call(zone_id.clone()),
                                            if busy { "Updating…" } else { "Remove pairing" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            div { class: if current_links.is_empty() { "" } else { "border-t border-default pt-5" },
                match availability {
                    ZoneMatchAvailability::Loading => rsx! {
                        p { class: "text-sm text-muted", aria_live: "polite",
                            "Finding playback zones and saved pairings…"
                        }
                    },
                    ZoneMatchAvailability::NoInstances => rsx! {
                        h3 { class: "font-semibold", "Connect HQPlayer first" }
                        p { class: "mt-1 text-sm text-muted",
                            "Save an HQPlayer connection before matching it to a playback zone."
                        }
                    },
                    ZoneMatchAvailability::NoPlaybackZones => rsx! {
                        h3 { class: "font-semibold", "No playback zones available to pair" }
                        p { class: "mt-1 text-sm text-muted",
                            "Every available zone is already paired, or no playback provider is connected yet."
                        }
                    },
                    ZoneMatchAvailability::Ready => rsx! {
                        h3 { class: "font-semibold",
                            if current_links.is_empty() { "Pair this signal path" } else { "Pair another signal path" }
                        }
                        div { class: "mt-4 grid grid-cols-1 gap-4 lg:grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] lg:items-end",
                            div {
                                label { class: "mb-2 block text-sm font-medium", r#for: "zone-match-playback", "Zone where playback starts" }
                                select {
                                    id: "zone-match-playback",
                                    class: "input",
                                    value: "{selected_zone}",
                                    disabled: busy,
                                    onchange: move |evt| selected_zone.set(evt.value()),
                                    for zone in candidates.iter() {
                                        option {
                                            value: "{zone.zone_id}",
                                            selected: zone.zone_id == selected_zone(),
                                            "{zone.zone_name} ({playback_source_label(zone.source.as_deref())})"
                                        }
                                    }
                                }
                            }
                            div {
                                label { class: "mb-2 block text-sm font-medium", r#for: "zone-match-instance", "HQPlayer it already feeds" }
                                select {
                                    id: "zone-match-instance",
                                    class: "input",
                                    value: "{selected_instance}",
                                    disabled: busy,
                                    onchange: move |evt| selected_instance.set(evt.value()),
                                    for instance in instances.iter() {
                                        option {
                                            value: "{instance.name}",
                                            selected: instance.name == selected_instance(),
                                            if let Some(ref host) = instance.host {
                                                "{instance.name} ({host})"
                                            } else {
                                                "{instance.name}"
                                            }
                                        }
                                    }
                                }
                            }
                            button {
                                r#type: "button",
                                class: "btn btn-primary w-full lg:w-auto",
                                disabled: busy,
                                onclick: move |_| {
                                    let zone_id = {
                                        let selected = selected_zone();
                                        normalized_match_selection(&selected, &candidate_ids_for_click)
                                    };
                                    let instance = {
                                        let selected = selected_instance();
                                        if selected.is_empty() {
                                            default_instance_for_click.clone()
                                        } else {
                                            selected
                                        }
                                    };
                                    if !zone_id.is_empty() && !instance.is_empty() {
                                        on_link.call((zone_id, instance));
                                    }
                                },
                                if busy { "Pairing…" } else { "Pair zone" }
                            }
                        }
                        p { class: "mt-3 max-w-3xl text-xs text-muted",
                            "Not sure? Start a track in your playback app and confirm that this HQPlayer begins playing before you pair them."
                        }
                    },
                }
            }

            if let Some((is_error, message)) = feedback_copy {
                p {
                    class: if is_error { "mt-4 text-sm text-red-400" } else { "mt-4 text-sm status-ok" },
                    role: if is_error { "alert" } else { "status" },
                    aria_live: "polite",
                    "{message}"
                }
            }
        }
    }
}
