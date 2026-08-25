//! Library page (#550): the app's home page.
//!
//! Browsing used to live in a 230px panel bolted onto each Music
//! Assistant-capable zone card. This page inverts that: browse/search is
//! now the primary surface, and [`crate::app::components::ZonesStrip`] is
//! the persistent play-target picker docked to the bottom. Both call the
//! same `/api/collections`, `/api/queue`, `/api/play_ref` and (new in this
//! issue) `/api/search` endpoints the old panel and the `hifi_*` MCP tools
//! already share -- see `src/api/browse.rs`.
//!
//! # URL-addressability
//!
//! Every piece of state a user would want to refresh, go back through, or
//! share -- source, tab, breadcrumb path, armed zone -- lives in the route's
//! query string (`Route::Library`). `path` is a base64url-encoded JSON
//! breadcrumb stack rather than raw segments: a provider browse path is an
//! opaque token (see `crate::app::api::CollectionItem::path`) that may not
//! be a safe URL path segment, and a flat segment list would lose each
//! breadcrumb's title on a fresh (deep-linked) load.

use crate::app::api::{
    self, source_label, CollectionItem, CollectionsRequest, NowPlaying, PlayRefRequest,
    SearchRequest, SearchResult, Zone, ZonesResponse,
};
use crate::app::components::{Layout, ZonesStrip};
use crate::app::sse::{use_sse, SseEvent};
use crate::app::Route;
use base64::Engine;
use dioxus::prelude::*;
use std::collections::HashMap;

const PAGE_LIMIT: u32 = 30;
const SEARCH_DEBOUNCE_MS: u64 = 300;
#[cfg(target_arch = "wasm32")]
const ARMED_ZONE_STORAGE_KEY: &str = "uhc.armed_zone";

/// Control request body, mirrors `pages::zones`.
#[derive(Clone, serde::Serialize)]
struct ControlRequest {
    zone_id: String,
    action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    value: Option<f64>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Tab {
    Browse,
    Playlists,
    Favorites,
    Radio,
}

impl Tab {
    fn parse(value: &str) -> Self {
        match value {
            "playlists" => Tab::Playlists,
            "favorites" => Tab::Favorites,
            "radio" => Tab::Radio,
            _ => Tab::Browse,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Tab::Browse => "browse",
            Tab::Playlists => "playlists",
            Tab::Favorites => "favorites",
            Tab::Radio => "radio",
        }
    }

    /// `/api/collections` verb. Favorites and Radio share the `favorites`
    /// action; `media_type` is what tells them apart (same as the old panel).
    fn action(self) -> &'static str {
        match self {
            Tab::Browse => "browse",
            Tab::Playlists => "playlists",
            Tab::Favorites | Tab::Radio => "favorites",
        }
    }

    fn media_type(self) -> Option<&'static str> {
        match self {
            Tab::Radio => Some("radio"),
            Tab::Favorites => Some("tracks"),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Tab::Browse => "Browse",
            Tab::Playlists => "Playlists",
            Tab::Favorites => "Favorites",
            Tab::Radio => "Radio",
        }
    }
}

const TABS: &[Tab] = &[Tab::Browse, Tab::Playlists, Tab::Favorites, Tab::Radio];

/// One breadcrumb: what the user picked, and the opaque path it opened.
/// The root ("Library") is implicit and never stored -- an empty stack
/// means "at the root of this tab".
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
struct BreadcrumbEntry {
    title: String,
    path: Option<String>,
}

fn encode_path(stack: &[BreadcrumbEntry]) -> Option<String> {
    if stack.is_empty() {
        return None;
    }
    let json = serde_json::to_string(stack).ok()?;
    Some(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json))
}

fn decode_path(encoded: Option<&str>) -> Vec<BreadcrumbEntry> {
    let Some(encoded) = encoded.filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(encoded)
        .ok()
        .and_then(|bytes| serde_json::from_slice(&bytes).ok())
        .unwrap_or_default()
}

/// Last-used armed zone, read only from client-side effects (never as part
/// of a signal's *initial* value) so SSR and the pre-hydration WASM render
/// stay identical -- see this module's doc comment and the pattern already
/// used by `pages::zones`'s own effect-driven state.
#[cfg(target_arch = "wasm32")]
fn load_last_zone() -> Option<String> {
    web_sys::window()?
        .local_storage()
        .ok()??
        .get_item(ARMED_ZONE_STORAGE_KEY)
        .ok()?
}

#[cfg(not(target_arch = "wasm32"))]
fn load_last_zone() -> Option<String> {
    None
}

#[cfg(target_arch = "wasm32")]
fn save_last_zone(zone_id: &str) {
    if let Some(Ok(Some(storage))) = web_sys::window().map(|w| w.local_storage()) {
        let _ = storage.set_item(ARMED_ZONE_STORAGE_KEY, zone_id);
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn save_last_zone(_zone_id: &str) {}

/// Resolve which zone is armed: the URL wins (deep link), then the last
/// zone the user picked, then whichever zone is currently playing, then
/// simply the first zone. Never `None` when `zones` is non-empty.
fn resolve_armed_zone<'a>(
    route_zone: Option<&str>,
    zones: &'a [Zone],
    now_playing: &HashMap<String, NowPlaying>,
) -> Option<&'a Zone> {
    if let Some(id) = route_zone {
        if let Some(zone) = zones.iter().find(|z| z.zone_id == id) {
            return Some(zone);
        }
    }
    if let Some(last) = load_last_zone() {
        if let Some(zone) = zones.iter().find(|z| z.zone_id == last) {
            return Some(zone);
        }
    }
    if let Some(playing) = zones.iter().find(|z| {
        now_playing
            .get(&z.zone_id)
            .map(|n| n.is_playing)
            .unwrap_or(false)
    }) {
        return Some(playing);
    }
    zones.first()
}

/// Resolve which provider's library the page browses: the URL wins, then
/// the armed zone's provider (if it can browse), then the first
/// browse-capable provider going.
fn resolve_source(
    route_source: Option<&str>,
    armed: Option<&Zone>,
    browsable: &[Zone],
) -> Option<String> {
    if let Some(source) = route_source.filter(|s| !s.is_empty()) {
        if browsable
            .iter()
            .any(|z| z.source.as_deref() == Some(source))
        {
            return Some(source.to_string());
        }
    }
    if let Some(armed) = armed {
        if armed.browse_supported {
            if let Some(source) = armed.source.clone() {
                return Some(source);
            }
        }
    }
    browsable.first().and_then(|z| z.source.clone())
}

/// Fetch one page of the current level and apply it.
#[allow(clippy::too_many_arguments)]
async fn load_page(
    zone_id: String,
    action: String,
    media_type: Option<String>,
    path: Option<String>,
    request_offset: u32,
    append: bool,
    mut items: Signal<Vec<CollectionItem>>,
    mut next_offset: Signal<Option<u32>>,
    mut offset: Signal<u32>,
    mut loading: Signal<bool>,
    mut error: Signal<Option<String>>,
) {
    loading.set(true);
    error.set(None);
    let req = CollectionsRequest {
        zone_id,
        action,
        path,
        media_type,
        limit: Some(PAGE_LIMIT),
        offset: Some(request_offset),
    };
    match api::fetch_collections(&req).await {
        Ok(env) if env.is_ok() => {
            let page = env.data.unwrap_or_default();
            if append {
                items.with_mut(|list| list.extend(page.items));
            } else {
                items.set(page.items);
            }
            next_offset.set(page.next_offset);
            offset.set(request_offset);
        }
        Ok(env) => {
            error.set(Some(env.error_detail()));
            if !append {
                items.set(Vec::new());
            }
        }
        Err(message) => {
            error.set(Some(message));
            if !append {
                items.set(Vec::new());
            }
        }
    }
    loading.set(false);
}

/// The Library page.
#[component]
pub fn Library(
    source: Option<String>,
    tab: Option<String>,
    path: Option<String>,
    zone: Option<String>,
) -> Element {
    let route_source = source.clone();
    let route_tab = tab.clone().unwrap_or_default();
    let route_path = path.clone();
    let route_zone = zone.clone();

    let sse = use_sse();
    let navigator = use_navigator();

    // Zones + now playing (same pattern as pages::zones).
    let mut zones =
        use_resource(|| async { api::fetch_json::<ZonesResponse>("/zones").await.ok() });
    let mut now_playing = use_signal(HashMap::<String, NowPlaying>::new);
    let zones_list = use_memo(move || {
        zones
            .read()
            .clone()
            .flatten()
            .map(|r| r.zones)
            .unwrap_or_default()
    });
    let browsable_zones = use_memo(move || {
        zones_list()
            .into_iter()
            .filter(|z| z.browse_supported)
            .collect::<Vec<_>>()
    });

    use_effect(move || {
        let list = zones_list();
        if !list.is_empty() {
            spawn(async move {
                let mut map = HashMap::new();
                for zone in list {
                    let url = format!(
                        "/now_playing?zone_id={}",
                        urlencoding::encode(&zone.zone_id)
                    );
                    if let Ok(np) = api::fetch_json::<NowPlaying>(&url).await {
                        map.insert(zone.zone_id, np);
                    }
                }
                now_playing.set(map);
            });
        }
    });

    // Refresh on SSE events (structural + playback), mirroring pages::zones.
    use_effect(move || {
        let _ = (sse.event_count)();
        let event = (sse.last_event)();
        if matches!(
            event.as_ref(),
            Some(
                SseEvent::ZoneDiscovered { .. }
                    | SseEvent::ZoneUpdated { .. }
                    | SseEvent::ZoneRemoved { .. }
                    | SseEvent::RoonConnected
                    | SseEvent::RoonDisconnected
                    | SseEvent::LmsConnected
                    | SseEvent::LmsDisconnected
            )
        ) {
            zones.restart();
        }
        if let Some(evt) = event {
            match evt {
                SseEvent::NowPlayingChanged { .. } | SseEvent::ZoneUpdated { .. } => {
                    if let Some(zone_id) = evt.zone_id() {
                        let zone_id = zone_id.to_string();
                        spawn(async move {
                            let url =
                                format!("/now_playing?zone_id={}", urlencoding::encode(&zone_id));
                            if let Ok(np) = api::fetch_json::<NowPlaying>(&url).await {
                                now_playing.with_mut(|map| {
                                    map.insert(zone_id, np);
                                });
                            }
                        });
                    }
                }
                SseEvent::VolumeChanged { .. } | SseEvent::LmsPlayerStateChanged { .. } => {
                    let list = zones_list();
                    if !list.is_empty() {
                        spawn(async move {
                            let mut map = HashMap::new();
                            for zone in list {
                                let url = format!(
                                    "/now_playing?zone_id={}",
                                    urlencoding::encode(&zone.zone_id)
                                );
                                if let Ok(np) = api::fetch_json::<NowPlaying>(&url).await {
                                    map.insert(zone.zone_id, np);
                                }
                            }
                            now_playing.with_mut(|merged| {
                                for (k, v) in map {
                                    merged.insert(k, v);
                                }
                            });
                        });
                    }
                }
                _ => {}
            }
        }
    });

    // Armed target zone: resolved from the URL / last-used / now-playing,
    // written back into the URL and localStorage so it survives a refresh
    // and shows up in shared links.
    let mut armed_zone_id = use_signal(|| route_zone.clone());
    use_effect(move || {
        let list = zones_list();
        if list.is_empty() {
            return;
        }
        let resolved = resolve_armed_zone(route_zone.as_deref(), &list, &now_playing());
        if let Some(zone) = resolved {
            if armed_zone_id.peek().as_deref() != Some(zone.zone_id.as_str()) {
                armed_zone_id.set(Some(zone.zone_id.clone()));
            }
            save_last_zone(&zone.zone_id);
        }
    });

    // Selected browse source.
    let selected_source = use_memo(move || {
        let list = zones_list();
        let armed = armed_zone_id().and_then(|id| list.iter().find(|z| z.zone_id == id).cloned());
        resolve_source(route_source.as_deref(), armed.as_ref(), &browsable_zones())
    });

    // The zone this page issues /api/collections and /api/play_ref calls
    // against: prefer the armed zone if it matches the selected source,
    // otherwise the first browse-capable zone of that source.
    let browse_zone_id = use_memo(move || {
        let list = zones_list();
        let source = selected_source();
        let armed_id = armed_zone_id();
        let armed_matches = armed_id.as_ref().and_then(|id| {
            list.iter()
                .find(|z| &z.zone_id == id && z.source == source && z.browse_supported)
        });
        if let Some(z) = armed_matches {
            return Some(z.zone_id.clone());
        }
        list.iter()
            .find(|z| z.browse_supported && z.source == source)
            .map(|z| z.zone_id.clone())
    });

    let current_tab = Tab::parse(&route_tab);
    let breadcrumbs = use_signal(|| decode_path(route_path.as_deref()));

    // Re-sync local breadcrumb signal when navigation (back/forward, or a
    // deep link) changes the route's `path` out from under us.
    {
        let mut breadcrumbs = breadcrumbs;
        let route_path_for_effect = path.clone();
        use_effect(move || {
            let decoded = decode_path(route_path_for_effect.as_deref());
            if *breadcrumbs.peek() != decoded {
                breadcrumbs.set(decoded);
            }
        });
    }

    let items = use_signal(Vec::<CollectionItem>::new);
    let offset = use_signal(|| 0u32);
    let next_offset = use_signal(|| None::<u32>);
    let loading = use_signal(|| false);
    let error = use_signal(|| None::<String>);
    let mut status = use_signal(|| None::<String>);

    let refresh = use_callback(move |append: bool| {
        let Some(zone_id) = browse_zone_id() else {
            return;
        };
        let action = current_tab.action().to_string();
        let media_type = current_tab.media_type().map(ToOwned::to_owned);
        let path = breadcrumbs.read().last().and_then(|e| e.path.clone());
        let request_offset = if append { offset() } else { 0 };
        spawn(load_page(
            zone_id,
            action,
            media_type,
            path,
            request_offset,
            append,
            items,
            next_offset,
            offset,
            loading,
            error,
        ));
    });

    // Reload the level whenever source, tab, browse zone, or breadcrumb
    // path changes.
    use_effect(move || {
        let _ = selected_source();
        let _ = browse_zone_id();
        let _ = route_tab.clone();
        let _ = breadcrumbs.read().clone();
        if browse_zone_id().is_some() {
            refresh(false);
        }
    });

    // Navigate helper: push a fresh URL for the given state, keeping the
    // rest of the query intact.
    let navigate = move |new_source: Option<String>,
                         new_tab: Option<String>,
                         new_path: Option<String>,
                         new_zone: Option<String>| {
        navigator.push(Route::Library {
            source: new_source,
            tab: new_tab,
            path: new_path,
            zone: new_zone,
        });
    };

    let open_folder = {
        move |(title, folder_path): (String, String)| {
            let mut stack = breadcrumbs();
            stack.push(BreadcrumbEntry {
                title,
                path: Some(folder_path),
            });
            navigate(
                selected_source(),
                Some(current_tab.as_str().to_string()),
                encode_path(&stack),
                armed_zone_id(),
            );
        }
    };

    let go_to_crumb = {
        move |index: usize| {
            let mut stack = breadcrumbs();
            stack.truncate(index);
            navigate(
                selected_source(),
                Some(current_tab.as_str().to_string()),
                encode_path(&stack),
                armed_zone_id(),
            );
        }
    };

    let select_source = {
        move |new_source: String| {
            navigate(
                Some(new_source),
                Some(current_tab.as_str().to_string()),
                None,
                armed_zone_id(),
            );
        }
    };

    let select_tab = {
        move |new_tab: Tab| {
            navigate(
                selected_source(),
                Some(new_tab.as_str().to_string()),
                None,
                armed_zone_id(),
            );
        }
    };

    let arm_zone = {
        move |zone_id: String| {
            let list = zones_list();
            let zone_source = list
                .iter()
                .find(|z| z.zone_id == zone_id)
                .and_then(|z| z.source.clone());
            // `Memo<Option<String>>`'s callable sugar isn't a real `FnOnce`,
            // so clippy's "use the value directly" suggestion doesn't
            // type-check here -- the closure is required.
            #[allow(clippy::redundant_closure)]
            let source = zone_source.or_else(|| selected_source());
            navigate(
                source,
                Some(current_tab.as_str().to_string()),
                None,
                Some(zone_id),
            );
        }
    };

    let control = move |(zone_id, action): (String, String)| {
        spawn(async move {
            let req = ControlRequest {
                zone_id,
                action,
                value: None,
            };
            let _ = api::post_json_no_response::<ControlRequest>("/control", &req).await;
        });
    };

    let play_item = use_callback(move |(item_ref, action): (String, &'static str)| {
        let Some(zone_id) = armed_zone_id() else {
            return;
        };
        status.set(Some("Working…".to_string()));
        spawn(async move {
            let req = PlayRefRequest {
                item_ref,
                zone_id,
                action: action.to_string(),
            };
            let message = match api::post_play_ref(&req).await {
                Ok(env) if env.is_ok() => match action {
                    "queue" => "Added to queue".to_string(),
                    "next" => "Playing next".to_string(),
                    _ => "Playing".to_string(),
                },
                Ok(env) => env.error_detail(),
                Err(message) => message,
            };
            status.set(Some(message));
        });
    });

    // ---- Unified search (#550): local filter + debounced global search ----
    let mut search_query = use_signal(String::new);
    let search_generation = use_signal(|| 0u64);
    let global_results = use_signal(Vec::<SearchResult>::new);
    let global_searching = use_signal(|| false);
    let global_error = use_signal(|| None::<String>);

    {
        let mut global_results = global_results;
        let mut global_searching = global_searching;
        let mut global_error = global_error;
        use_effect(move || {
            let query = search_query();
            let generation = {
                let mut g = search_generation;
                let next = g() + 1;
                g.set(next);
                next
            };
            if query.trim().is_empty() {
                global_results.set(Vec::new());
                global_searching.set(false);
                return;
            }
            let zone_id = browse_zone_id();
            spawn(async move {
                dioxus_sdk_time::sleep(std::time::Duration::from_millis(SEARCH_DEBOUNCE_MS)).await;
                if search_generation() != generation {
                    return; // superseded by a newer keystroke
                }
                global_searching.set(true);
                let req = SearchRequest {
                    query,
                    zone_id,
                    source: None,
                };
                let result = api::fetch_search(&req).await;
                if search_generation() != generation {
                    return;
                }
                match result {
                    Ok(env) if env.is_ok() => {
                        global_results.set(env.data.unwrap_or_default());
                        global_error.set(None);
                    }
                    Ok(env) => {
                        global_results.set(Vec::new());
                        global_error.set(Some(env.error_detail()));
                    }
                    Err(message) => {
                        global_results.set(Vec::new());
                        global_error.set(Some(message));
                    }
                }
                global_searching.set(false);
            });
        });
    }

    let query_text = search_query();
    let is_searching_locally = !query_text.trim().is_empty();
    let local_matches: Vec<CollectionItem> = if is_searching_locally {
        let needle = query_text.to_lowercase();
        items()
            .into_iter()
            .filter(|item| {
                item.title.to_lowercase().contains(&needle)
                    || item
                        .subtitle
                        .as_deref()
                        .map(|s| s.to_lowercase().contains(&needle))
                        .unwrap_or(false)
            })
            .collect()
    } else {
        Vec::new()
    };

    let sources: Vec<String> = {
        let mut seen = Vec::new();
        for zone in browsable_zones() {
            if let Some(source) = zone.source {
                if !seen.contains(&source) {
                    seen.push(source);
                }
            }
        }
        seen
    };

    let current_source = selected_source();
    let current_error = error();
    let is_loading = loading();
    let current_items = items();
    let can_load_more = next_offset.read().is_some();
    let current_status = status();
    let crumbs = breadcrumbs();
    let armed = armed_zone_id();
    let now_playing_map = now_playing();
    let currently_playing_title = armed
        .as_ref()
        .and_then(|id| now_playing_map.get(id))
        .and_then(|np| np.line1.clone());

    let content: Element = if sources.is_empty() {
        rsx! {
            div { class: "library-empty",
                h2 { "No library sources yet" }
                p { class: "text-muted", "Connect Roon, LMS, or Music Assistant in Settings to start browsing." }
                Link { class: "btn btn-primary", to: Route::Settings {}, "Go to Settings" }
            }
        }
    } else if let Some(message) = current_error.clone() {
        let retry_source = current_source.clone();
        rsx! {
            div { class: "library-provider-down",
                p { "{source_label(current_source.as_deref().unwrap_or(\"this source\"))} isn't reachable right now." }
                p { class: "text-sm text-muted", "{message}" }
                button {
                    class: "btn btn-outline",
                    r#type: "button",
                    onclick: move |_| {
                        let _ = retry_source.clone();
                        refresh(false);
                    },
                    "Retry"
                }
            }
        }
    } else if is_loading && current_items.is_empty() {
        rsx! {
            ul { class: "library-list", aria_busy: "true",
                for i in 0..6u32 {
                    li { key: "{i}", class: "library-skeleton-row" }
                }
            }
        }
    } else if is_searching_locally {
        let everywhere_loading = global_searching();
        let everywhere = global_results();
        let everywhere_error = global_error();
        rsx! {
            div { class: "library-search-results",
                section {
                    h3 { class: "library-search-heading", "In this folder" }
                    if local_matches.is_empty() {
                        p { class: "text-sm text-muted", "No matches in this folder." }
                    } else {
                        LibraryRows {
                            items: local_matches.clone(),
                            tab: current_tab,
                            currently_playing_title: currently_playing_title.clone(),
                            on_open: open_folder,
                            on_play: play_item,
                        }
                    }
                }
                section { class: "mt-6",
                    h3 { class: "library-search-heading", "Everywhere" }
                    if everywhere_loading {
                        p { class: "text-sm text-muted", "Searching…" }
                    } else if let Some(message) = everywhere_error {
                        p { class: "text-sm text-error", "{message}" }
                    } else if everywhere.is_empty() {
                        p { class: "text-sm text-muted", "No matches anywhere." }
                    } else {
                        ul { class: "library-list",
                            for result in everywhere.iter().cloned() {
                                li {
                                    key: "{result.title}-{result.subtitle.clone().unwrap_or_default()}",
                                    class: "library-row",
                                    div { class: "min-w-0",
                                        p { class: "font-medium truncate", "{result.title}" }
                                        if let Some(subtitle) = result.subtitle.clone() {
                                            p { class: "text-sm text-muted truncate", "{subtitle}" }
                                        }
                                    }
                                    if let Some(item_ref) = result.item_ref.clone() {
                                        button {
                                            class: "library-play-btn",
                                            aria_label: "Play {result.title}",
                                            r#type: "button",
                                            onclick: move |_| play_item((item_ref.clone(), "play")),
                                            "▶"
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    } else if current_items.is_empty() {
        rsx! {
            div { class: "library-empty",
                h2 { "{empty_state_heading(current_tab)}" }
                p { class: "text-muted", "{empty_state_body(current_tab)}" }
            }
        }
    } else {
        rsx! {
            LibraryRows {
                items: current_items.clone(),
                tab: current_tab,
                currently_playing_title: currently_playing_title.clone(),
                on_open: open_folder,
                on_play: play_item,
            }
            if can_load_more {
                button {
                    class: "btn btn-ghost text-sm mt-3",
                    r#type: "button",
                    onclick: move |_| refresh(true),
                    "Load more"
                }
            }
        }
    };

    rsx! {
        Layout {
            title: "Library".to_string(),
            nav_active: "library".to_string(),

            div { class: "library-page",
                h1 { class: "text-2xl font-bold mb-4", "Library" }

                input {
                    class: "input library-search-input",
                    r#type: "search",
                    placeholder: "Search this folder and everywhere…",
                    value: "{query_text}",
                    oninput: move |evt| search_query.set(evt.value()),
                }

                if !sources.is_empty() {
                    div { class: "library-source-tabs",
                        for src in sources.iter().cloned() {
                            {
                                let active = Some(src.clone()) == current_source;
                                let src_for_click = src.clone();
                                rsx! {
                                    button {
                                        key: "{src}",
                                        class: if active { "badge badge-primary" } else { "badge" },
                                        r#type: "button",
                                        onclick: move |_| select_source(src_for_click.clone()),
                                        "{source_label(&src)}"
                                    }
                                }
                            }
                        }
                    }

                    div { class: "library-tabs",
                        for candidate in TABS {
                            {
                                let candidate = *candidate;
                                let active = candidate == current_tab;
                                rsx! {
                                    button {
                                        key: "{candidate.label()}",
                                        class: if active { "library-tab library-tab--active" } else { "library-tab" },
                                        r#type: "button",
                                        onclick: move |_| select_tab(candidate),
                                        "{candidate.label()}"
                                    }
                                }
                            }
                        }
                    }

                    nav { class: "library-breadcrumbs", aria_label: "Breadcrumb",
                        button {
                            class: "library-breadcrumb-item",
                            r#type: "button",
                            onclick: move |_| go_to_crumb(0),
                            "Library"
                        }
                        for (index , crumb) in crumbs.iter().cloned().enumerate() {
                            span { key: "{index}",
                                span { class: "library-breadcrumb-sep", "/" }
                                button {
                                    class: "library-breadcrumb-item",
                                    r#type: "button",
                                    onclick: move |_| go_to_crumb(index + 1),
                                    "{crumb.title}"
                                }
                            }
                        }
                    }
                }

                if let Some(message) = current_status.clone() {
                    p { class: "text-sm text-muted mb-2", role: "status", "{message}" }
                }

                section { class: "library-content", {content} }
            }

            // Bottom padding so the last row isn't hidden behind the fixed strip.
            div { class: "library-strip-spacer" }
        }

        ZonesStrip {
            zones: zones_list(),
            now_playing: now_playing_map,
            armed_zone_id: armed,
            on_arm: arm_zone,
            on_control: control,
        }
    }
}

fn empty_state_heading(tab: Tab) -> &'static str {
    match tab {
        Tab::Browse => "Nothing here yet",
        Tab::Playlists => "No playlists yet",
        Tab::Favorites => "No favorites yet",
        Tab::Radio => "No radio stations yet",
    }
}

fn empty_state_body(tab: Tab) -> &'static str {
    match tab {
        Tab::Browse => "This folder is empty.",
        Tab::Playlists => "Playlists you create in this provider will show up here.",
        Tab::Favorites => "The ♥ on any album or track lands here.",
        Tab::Radio => "Radio stations you add will show up here.",
    }
}

/// Rows for one page of browse/playlists/favorites/radio results.
///
/// Per-level layout follows the design brief's "art-first, list otherwise"
/// split: a page made mostly of playable leaves (tracks/albums, `ref` set)
/// renders as an artwork-forward grid; a page made mostly of folders
/// (`path` set, no `ref`) renders as compact navigable rows. `CollectionItem`
/// has no artwork field yet (that's the sibling companion issue, #549) so
/// the grid tile falls back to a text placeholder until it does.
#[component]
fn LibraryRows(
    items: Vec<CollectionItem>,
    tab: Tab,
    currently_playing_title: Option<String>,
    on_open: EventHandler<(String, String)>,
    on_play: EventHandler<(String, &'static str)>,
) -> Element {
    let playable_count = items.iter().filter(|i| i.item_ref.is_some()).count();
    let grid_shaped = !items.is_empty() && playable_count * 2 >= items.len();

    if grid_shaped {
        rsx! {
            ul { class: "library-grid",
                for (index , item) in items.iter().cloned().enumerate() {
                    LibraryTile {
                        key: "{item.title}-{item.path.clone().unwrap_or_default()}-{item.item_ref.clone().unwrap_or_default()}",
                        item: item,
                        index: index,
                        tab: tab,
                        is_playing: currently_playing_title.as_deref() == Some(items.get(index).map(|i| i.title.as_str()).unwrap_or_default()),
                        on_open: on_open,
                        on_play: on_play,
                    }
                }
            }
        }
    } else {
        rsx! {
            ul { class: "library-list",
                for (index , item) in items.iter().cloned().enumerate() {
                    LibraryRow {
                        key: "{item.title}-{item.path.clone().unwrap_or_default()}-{item.item_ref.clone().unwrap_or_default()}",
                        item: item,
                        index: index,
                        tab: tab,
                        is_playing: currently_playing_title.as_deref() == Some(items.get(index).map(|i| i.title.as_str()).unwrap_or_default()),
                        on_open: on_open,
                        on_play: on_play,
                    }
                }
            }
        }
    }
}

#[component]
fn LibraryRow(
    item: CollectionItem,
    index: usize,
    tab: Tab,
    is_playing: bool,
    on_open: EventHandler<(String, String)>,
    on_play: EventHandler<(String, &'static str)>,
) -> Element {
    let mut menu_open = use_signal(|| false);
    let stagger_style = format!("--library-stagger-index: {index};");

    rsx! {
        li {
            class: "library-row library-row--reveal",
            style: "{stagger_style}",
            div { class: "min-w-0 flex items-center gap-2",
                if is_playing {
                    span { class: "library-eq", aria_label: "Currently playing",
                        span {} span {} span {}
                    }
                }
                div { class: "min-w-0",
                    p { class: "font-medium truncate", "{item.title}" }
                    if let Some(subtitle) = item.subtitle.clone() {
                        p { class: "text-sm text-muted truncate", "{subtitle}" }
                    }
                }
                if tab == Tab::Radio {
                    span { class: "library-live-tick", "LIVE" }
                }
            }
            div { class: "library-row-actions",
                if let Some(item_ref) = item.item_ref.clone() {
                    button {
                        class: "library-play-btn",
                        aria_label: "Play {item.title}",
                        r#type: "button",
                        onclick: {
                            let item_ref = item_ref.clone();
                            move |_| on_play((item_ref.clone(), "play"))
                        },
                        "▶"
                    }
                    div { class: "library-overflow",
                        button {
                            class: "library-overflow-trigger",
                            aria_label: "More actions for {item.title}",
                            r#type: "button",
                            onclick: move |_| menu_open.toggle(),
                            "⋯"
                        }
                        if menu_open() {
                            div { class: "library-overflow-menu",
                                button {
                                    r#type: "button",
                                    onclick: {
                                        let item_ref = item_ref.clone();
                                        move |_| {
                                            menu_open.set(false);
                                            on_play((item_ref.clone(), "next"));
                                        }
                                    },
                                    "Play Next"
                                }
                                button {
                                    r#type: "button",
                                    onclick: {
                                        let item_ref = item_ref.clone();
                                        move |_| {
                                            menu_open.set(false);
                                            on_play((item_ref.clone(), "queue"));
                                        }
                                    },
                                    "Queue"
                                }
                            }
                        }
                    }
                }
                if let Some(path) = item.path.clone() {
                    button {
                        class: "library-chevron-btn",
                        aria_label: "Open {item.title}",
                        r#type: "button",
                        onclick: {
                            let title = item.title.clone();
                            let path = path.clone();
                            move |_| on_open((title.clone(), path.clone()))
                        },
                        svg { class: "w-5 h-5", fill: "none", view_box: "0 0 24 24", stroke: "currentColor", "stroke-width": "2",
                            path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M9 5l7 7-7 7" }
                        }
                    }
                }
            }
        }
    }
}

#[component]
fn LibraryTile(
    item: CollectionItem,
    index: usize,
    tab: Tab,
    is_playing: bool,
    on_open: EventHandler<(String, String)>,
    on_play: EventHandler<(String, &'static str)>,
) -> Element {
    let stagger_style = format!("--library-stagger-index: {index};");
    let has_ref = item.item_ref.is_some();
    let has_path = item.path.is_some();

    rsx! {
        li {
            class: "library-tile library-tile--reveal",
            style: "{stagger_style}",
            div {
                class: "library-tile-art",
                onclick: {
                    let path = item.path.clone();
                    let title = item.title.clone();
                    move |_| {
                        if let Some(path) = path.clone() {
                            on_open((title.clone(), path.clone()));
                        }
                    }
                },
                span { class: "library-tile-placeholder", "♪" }
                if is_playing {
                    span { class: "library-eq library-eq--tile", aria_label: "Currently playing",
                        span {} span {} span {}
                    }
                }
                if tab == Tab::Radio {
                    span { class: "library-live-tick library-live-tick--tile", "LIVE" }
                }
                if has_ref {
                    button {
                        class: "library-tile-play",
                        aria_label: "Play {item.title}",
                        r#type: "button",
                        onclick: {
                            let item_ref = item.item_ref.clone().unwrap_or_default();
                            move |evt| {
                                evt.stop_propagation();
                                on_play((item_ref.clone(), "play"));
                            }
                        },
                        "▶"
                    }
                }
                if has_path {
                    span { class: "library-tile-chevron",
                        svg { class: "w-4 h-4", fill: "none", view_box: "0 0 24 24", stroke: "currentColor", "stroke-width": "2",
                            path { "stroke-linecap": "round", "stroke-linejoin": "round", d: "M9 5l7 7-7 7" }
                        }
                    }
                }
            }
            div { class: "library-tile-meta",
                p { class: "font-medium truncate text-sm", "{item.title}" }
                if let Some(subtitle) = item.subtitle.clone() {
                    p { class: "text-xs text-muted truncate", "{subtitle}" }
                }
            }
        }
    }
}
