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
//! Source, view, and the durable provider-neutral collection location use
//! canonical path segments. Provider paths and Roon browse-session keys stay
//! behind `/api/collections`; the browser sees only a short `loc_…` identity.

use crate::app::api::{
    self, source_label, CollectionBreadcrumb, CollectionItem, CollectionsRequest, NowPlaying,
    PlayRefRequest, SearchRequest, SearchResult, Zone, ZonesResponse,
};
use crate::app::components::{Layout, ZonesStrip};
use crate::app::sse::{use_sse, SseEvent};
use crate::app::Route;
use dioxus::prelude::*;
use std::collections::HashMap;

const PAGE_LIMIT: u32 = 30;
const SEARCH_DEBOUNCE_MS: u64 = 300;
#[cfg(target_arch = "wasm32")]
const LIBRARY_LAYOUT_STORAGE_KEY: &str = "uhc.library.view";
#[cfg(target_arch = "wasm32")]
const ARMED_ZONE_STORAGE_KEY: &str = "uhc.armed_zone";

fn library_route(source: Option<String>, view: Option<String>, location: Option<String>) -> Route {
    let Some(source) = source else {
        return Route::LibraryHome {};
    };
    let view = view.filter(|value| !value.is_empty());
    match (view, location) {
        (Some(view), Some(location)) => Route::LibraryLocation {
            source,
            view,
            location,
        },
        (Some(view), None) if view != "browse" => Route::LibraryView { source, view },
        _ => Route::LibrarySource { source },
    }
}

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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum LibraryLayout {
    #[default]
    Auto,
    List,
    Cards,
}

impl LibraryLayout {
    #[cfg(target_arch = "wasm32")]
    fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::List => "list",
            Self::Cards => "cards",
        }
    }

    #[cfg(target_arch = "wasm32")]
    fn parse(value: &str) -> Self {
        match value {
            "list" => Self::List,
            "cards" => Self::Cards,
            _ => Self::Auto,
        }
    }
}

#[cfg(target_arch = "wasm32")]
fn load_library_layout() -> LibraryLayout {
    web_sys::window()
        .and_then(|window| window.local_storage().ok().flatten())
        .and_then(|storage| storage.get_item(LIBRARY_LAYOUT_STORAGE_KEY).ok().flatten())
        .map(|value| LibraryLayout::parse(&value))
        .unwrap_or_default()
}

#[cfg(target_arch = "wasm32")]
fn save_library_layout(layout: LibraryLayout) {
    if let Some(Ok(Some(storage))) = web_sys::window().map(|window| window.local_storage()) {
        let _ = storage.set_item(LIBRARY_LAYOUT_STORAGE_KEY, layout.as_str());
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn save_library_layout(_layout: LibraryLayout) {}

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

/// Which tabs to render for the current browse zone (#573 defect 6).
///
/// `library_tabs` is the server's per-provider list (`Zone::library_tabs`),
/// derived from the same capability facts the matrix reports -- a Roon zone
/// sends `["browse", "playlists"]`, so Favorites and Radio (which Roon's
/// collections surface refuses on every call) are never rendered. An empty
/// list means "no information" (a response from a build predating the
/// field): degrade to showing every tab rather than none.
fn visible_tabs_for(library_tabs: &[String]) -> Vec<Tab> {
    if library_tabs.is_empty() {
        return TABS.to_vec();
    }
    TABS.iter()
        .copied()
        .filter(|tab| library_tabs.iter().any(|name| name == tab.as_str()))
        .collect()
}

/// The tab actually shown: the route's tab when this provider serves it,
/// otherwise Browse -- so a deep link to `/library/roon/favorites`
/// lands on a working tab instead of a permanent refusal page.
fn effective_tab(requested: Tab, visible: &[Tab]) -> Tab {
    if visible.contains(&requested) {
        requested
    } else {
        Tab::Browse
    }
}

/// #573 defect 2 pin: the API's `image` fields (`CollectionItem::image`,
/// `SearchResult::image`) are complete origin-absolute URLs
/// (`/api/collections/image?ref=...`) and are used **verbatim** as
/// `<img src>`. This helper is the single place a row image becomes a `src`;
/// the double-prefix regression ("/api/collections/image?ref={image}" around
/// an already-full path, 404ing every image) cannot re-enter through rsx
/// string interpolation as long as rendering goes through here.
///
/// #581: under an ingress/subpath proxy the browser must issue the URL with
/// the runtime base path prepended -- `base_path::href` is the same single
/// resolver every fetch helper uses, and it is the identity in direct mode,
/// so the verbatim pin above still holds where it was minted.
fn image_src(image: &str) -> String {
    crate::app::base_path::href(image)
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
pub(crate) fn save_last_zone(zone_id: &str) {
    if let Some(Ok(Some(storage))) = web_sys::window().map(|w| w.local_storage()) {
        let _ = storage.set_item(ARMED_ZONE_STORAGE_KEY, zone_id);
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn save_last_zone(_zone_id: &str) {}

/// Resolve which zone is armed: the last zone the user picked wins, then
/// whichever zone is currently playing, then simply the first zone. Zone
/// selection is client preference state and deliberately never enters the
/// canonical library URL.
fn resolve_armed_zone<'a>(
    zones: &'a [Zone],
    now_playing: &HashMap<String, NowPlaying>,
) -> Option<&'a Zone> {
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

/// Whether an SSE event should trigger `zones.restart()` -- i.e. a fresh
/// `/zones` fetch (#557).
///
/// Deliberately narrower than [`crate::app::sse::SseContext::should_refresh_zones`]
/// and than `pages::zones`'s own restart condition: STRUCTURAL changes only
/// (a zone appearing/disappearing, or a provider connecting/disconnecting).
/// `ZoneUpdated` is excluded on purpose -- a playing zone's `ZoneUpdated`
/// stream is effectively continuous, and restarting the zones resource on
/// every one of them is exactly the fetch loop #557 reports (see this
/// module's doc comment). This page has no need for the fresh `Zone.state`
/// a restart would buy: `is_playing` is read from `now_playing`, which is
/// refreshed on `ZoneUpdated` separately (a single per-zone `GET`, not a
/// zones-list refetch).
fn should_restart_zones(event: Option<&SseEvent>) -> bool {
    matches!(
        event,
        Some(
            SseEvent::ZoneDiscovered { .. }
                | SseEvent::ZoneRemoved { .. }
                | SseEvent::RoonConnected
                | SseEvent::RoonDisconnected
                | SseEvent::LmsConnected
                | SseEvent::LmsDisconnected
        )
    )
}

/// Zones present in `list` with no entry yet in `now_playing` -- the ones
/// worth fetching (#557).
///
/// Used instead of unconditionally re-fetching every zone's now-playing
/// state whenever `zones_list()` changes: `Zone` carries a `state` field
/// that legitimately changes on every `ZoneUpdated`, so a naive "refetch
/// everything when the zone list changes" effect reruns on every such
/// event even though no zone was actually added or removed. Filtering to
/// zones missing from `now_playing` makes a rerun with no new zones a
/// true no-op -- no request, no whole-map replace, no downstream
/// re-render cascade.
fn missing_now_playing_zones(
    list: &[Zone],
    now_playing: &HashMap<String, NowPlaying>,
) -> Vec<Zone> {
    list.iter()
        .filter(|z| !now_playing.contains_key(&z.zone_id))
        .cloned()
        .collect()
}

/// A failed level load, split by what went wrong (#573 visual pass V3).
///
/// `unreachable` is true only when the HTTP request itself failed -- the one
/// case "«source» isn't reachable right now." is true of. A well-formed
/// refusal envelope (a capability gap, an unknown location, ...) reaches the
/// server fine; rendering it under unreachability copy told users the
/// provider was down when it wasn't.
#[derive(Clone, Debug, PartialEq)]
struct LevelError {
    message: String,
    unreachable: bool,
}

/// Select the offset for a level load. Continuation pages must consume the
/// provider's advertised cursor; the previous request offset is not a cursor
/// and would fetch the same page again.
fn next_request_offset(append: bool, next_offset: Option<u32>) -> Option<u32> {
    if append {
        next_offset
    } else {
        Some(0)
    }
}

/// Fetch one page of the current level and apply it.
#[allow(clippy::too_many_arguments)]
async fn load_page(
    zone_id: String,
    action: String,
    media_type: Option<String>,
    location: Option<String>,
    request_offset: u32,
    append: bool,
    mut items: Signal<Vec<CollectionItem>>,
    mut next_offset: Signal<Option<u32>>,
    mut loading: Signal<bool>,
    mut error: Signal<Option<LevelError>>,
    mut breadcrumbs: Signal<Vec<CollectionBreadcrumb>>,
) {
    loading.set(true);
    error.set(None);
    let req = CollectionsRequest {
        zone_id,
        action,
        path: None,
        location,
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
                breadcrumbs.set(page.breadcrumbs);
            }
            next_offset.set(page.next_offset);
        }
        Ok(env) => {
            error.set(Some(LevelError {
                message: env.error_detail(),
                unreachable: false,
            }));
            if !append {
                // #573 visual pass V2: clear the whole level, not just the
                // items -- a stale `next_offset` would render "Load more"
                // under the error panel.
                items.set(Vec::new());
                next_offset.set(None);
                breadcrumbs.set(Vec::new());
            }
        }
        Err(message) => {
            error.set(Some(LevelError {
                message,
                unreachable: true,
            }));
            if !append {
                items.set(Vec::new());
                next_offset.set(None);
                breadcrumbs.set(Vec::new());
            }
        }
    }
    loading.set(false);
}

/// The Library page.
#[component]
fn Library(source: Option<String>, tab: Option<String>, location: Option<String>) -> Element {
    let route_source = source.clone();
    let route_tab = tab.clone().unwrap_or_default();
    let route_location = location.clone();

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

    // Fetch now_playing for any zone we don't have it for yet: the initial
    // load, or a zone newly added by a structural SSE event. Selective
    // inserts (`with_mut`) rather than a whole-map `.set()` -- #557: `Zone`
    // carries a `state` field that legitimately changes on every
    // `ZoneUpdated`, so `zones_list()` (and this effect, if it depended on
    // the full zone content) would rerun on every such event even though no
    // zone was added or removed. Filtering to zones missing from
    // `now_playing` makes a rerun with no new zones a no-op: no request, no
    // map replace, no downstream re-render cascade. See this module's doc
    // comment and #557's issue body for the loop this replaces.
    use_effect(move || {
        let list = zones_list();
        let missing = missing_now_playing_zones(&list, &now_playing.peek());
        if !missing.is_empty() {
            spawn(async move {
                for zone in missing {
                    let url = format!(
                        "/now_playing?zone_id={}",
                        urlencoding::encode(&zone.zone_id)
                    );
                    if let Ok(np) = api::fetch_json::<NowPlaying>(&url).await {
                        now_playing.with_mut(|map| {
                            map.insert(zone.zone_id, np);
                        });
                    }
                }
            });
        }
    });

    // Refresh on SSE events (structural + playback), mirroring pages::zones
    // -- but stricter (#557): `zones.restart()` only on STRUCTURAL events
    // (a zone appearing/disappearing, or a provider connecting/
    // disconnecting), never on `ZoneUpdated`. A playing zone's `ZoneUpdated`
    // stream is effectively continuous (play/pause, track, position-adjacent
    // state), and this page has no need for the fresh `Zone.state` a restart
    // would buy -- is_playing comes from `now_playing`, refreshed below on
    // exactly this event.
    use_effect(move || {
        let _ = (sse.event_count)();
        let event = (sse.last_event)();
        if should_restart_zones(event.as_ref()) {
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

    // Armed target zone is client preference state. Persist it locally so
    // refreshes keep the user's target without polluting shareable URLs.
    let mut armed_zone_id = use_signal(|| None::<String>);
    use_effect(move || {
        let list = zones_list();
        if list.is_empty() {
            return;
        }
        let resolved = resolve_armed_zone(&list, &now_playing());
        if let Some(zone) = resolved {
            if armed_zone_id.peek().as_deref() != Some(zone.zone_id.as_str()) {
                armed_zone_id.set(Some(zone.zone_id.clone()));
            }
            save_last_zone(&zone.zone_id);
        }
    });

    // Selected browse source. `use_reactive!` on `route_source` is
    // load-bearing: `source`
    // is a plain route prop, so without it this memo recomputes only when
    // the zone list or armed zone changes. A source-picker click
    // (`select_source`) changes only the `source` route segment -- the memo
    // kept returning the old source and the click read as a no-op.
    let selected_source = use_memo(use_reactive!(|route_source| {
        let list = zones_list();
        let armed = armed_zone_id().and_then(|id| list.iter().find(|z| z.zone_id == id).cloned());
        resolve_source(route_source.as_deref(), armed.as_ref(), &browsable_zones())
    }));

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

    // Tabs the current browse zone's provider serves (#573 defect 6), and
    // the tab actually rendered (the route's tab, downgraded to Browse when
    // this provider does not serve it).
    let visible_tabs = use_memo(move || {
        let list = zones_list();
        let tabs = browse_zone_id()
            .and_then(|id| list.iter().find(|z| z.zone_id == id))
            .map(|z| z.library_tabs.clone())
            .unwrap_or_default();
        visible_tabs_for(&tabs)
    });
    let current_tab = effective_tab(Tab::parse(&route_tab), &visible_tabs());
    // Breadcrumb labels are reconstructed by the server from the durable
    // location. They are display data, not URL state.
    let breadcrumbs = use_signal(Vec::<CollectionBreadcrumb>::new);

    let items = use_signal(Vec::<CollectionItem>::new);
    let next_offset = use_signal(|| None::<u32>);
    let mut library_layout = use_signal(LibraryLayout::default);
    #[cfg(target_arch = "wasm32")]
    use_effect(move || {
        library_layout.set(load_library_layout());
    });
    let loading = use_signal(|| false);
    let error = use_signal(|| None::<LevelError>);
    let mut status = use_signal(|| None::<String>);
    // Monotonic guard so a delayed auto-clear never erases a NEWER toast.
    let mut status_generation = use_signal(|| 0u64);

    let route_location_for_refresh = route_location.clone();
    let refresh = use_callback(move |append: bool| {
        let Some(zone_id) = browse_zone_id() else {
            return;
        };
        let location = route_location_for_refresh.clone();
        // #573 (visual pass V2b root cause): a tab's action ("playlists",
        // "favorites") lists that tab's ROOT and ignores navigation state -- so once
        // the user opened a playlist, every reload re-listed the parent
        // under an advanced breadcrumb (an empty playlist looked like the
        // playlists list itself). Any level below a tab's root is reached
        // by its durable `location`, and continuing into one uses the
        // `browse` action, whatever tab it started from.
        let (action, media_type) = if location.is_some() {
            ("browse".to_string(), None)
        } else {
            (
                current_tab.action().to_string(),
                current_tab.media_type().map(ToOwned::to_owned),
            )
        };
        // A fresh route load must not subscribe this callback/effect to the
        // pagination cursor: load_page updates `next_offset`, and that update
        // would otherwise retrigger the route effect and erase appended rows
        // with a new offset-0 request. The append path is an event handler, so
        // it may read the cursor without creating that subscription.
        let continuation = if append { *next_offset.peek() } else { None };
        let Some(request_offset) = next_request_offset(append, continuation) else {
            // A stale click can arrive after the previous page has completed
            // and cleared the continuation. Never turn that into offset zero,
            // which would append the first page a second time.
            return;
        };
        spawn(load_page(
            zone_id,
            action,
            media_type,
            location,
            request_offset,
            append,
            items,
            next_offset,
            loading,
            error,
            breadcrumbs,
        ));
    });

    // Reload the level whenever source, tab, browse zone, or durable
    // location changes.
    //
    // `use_reactive!` on `route_tab` is load-bearing (same class as the
    // source route effect above): `tab` is a plain route prop, and the old
    // `let _ = route_tab.clone();` captured it without tracking anything. A
    // tab switch at the breadcrumb root (e.g. Browse -> Playlists, stack
    // [] == [], source and zone unchanged) changed no tracked dependency,
    // so this effect never reran and the old tab's items stayed on screen
    // under the new tab's header.
    let route_location_for_effect = route_location.clone();
    use_effect(use_reactive!(|(route_tab, route_location_for_effect)| {
        let _ = selected_source();
        let _ = browse_zone_id();
        let _ = route_tab;
        let _ = route_location_for_effect;
        // The play-status toast ("Playing", "Added to queue") describes an
        // action taken on the level the user was on -- it must not follow
        // them around the library (live report: a lone "Playing" haunting
        // every page). Write-only here: `status` is never read in this
        // effect, so no self-subscription.
        status.set(None);
        // Tracked so the level refetches when the served-tab set arrives and
        // downgrades a deep-linked unsupported tab to Browse (#573 defect 6).
        let _ = visible_tabs();
        if browse_zone_id().is_some() {
            refresh(false);
        }
    }));

    // Navigate with canonical route state only. The armed zone remains a
    // client-side preference and provider-native paths remain server-side.
    let navigate =
        move |new_source: Option<String>, new_tab: Option<String>, new_location: Option<String>| {
            navigator.push(library_route(new_source, new_tab, new_location));
        };

    // The search box's text. Declared before `open_folder` (its consumers --
    // the input binding and the search effect -- live in the "Unified search"
    // block below) because opening a folder must clear it.
    let mut search_query = use_signal(String::new);

    let open_folder = {
        move |(_title, location): (String, String)| {
            // #566 (live probe): a non-empty query keeps the search-results
            // view mounted, so opening a search hit navigated the route (URL
            // and breadcrumbs updated) while the visible page stayed on the
            // results list -- the click read as a no-op, the exact dead end
            // this issue exists to remove. Opening a folder is a statement
            // that the search is done: clear the query so the browse level
            // the navigation lands on is actually shown. A no-op for browse
            // rows opened outside a search (the query is already empty).
            // (Signals are Copy: the shadow keeps this closure `Fn` -- setting
            // the captured copy directly would make it `FnMut`, which the row
            // components' handler props don't accept.)
            let mut search_query = search_query;
            search_query.set(String::new());
            navigate(
                selected_source(),
                Some(current_tab.as_str().to_string()),
                Some(location),
            );
        }
    };

    // A search hit from the "Everywhere" results is NOT a child of the level
    // the user happens to be standing on -- appending it to the current
    // breadcrumb stack fabricates a lineage that never existed (live report:
    // "Library / ... / 10,000 Maniacs / Michael Jackson") and walking those
    // stale crumbs afterwards desyncs from the provider's browse session.
    // Opening a search hit starts a FRESH trail rooted at the hit itself.
    let open_search_hit = {
        move |(_title, location): (String, String)| {
            let mut search_query = search_query;
            search_query.set(String::new());
            navigate(
                selected_source(),
                Some(current_tab.as_str().to_string()),
                Some(location),
            );
        }
    };

    let go_to_crumb = {
        move |index: usize| {
            let location = index
                .checked_sub(1)
                .and_then(|crumb_index| breadcrumbs().get(crumb_index).cloned())
                .map(|crumb| crumb.location);
            navigate(
                selected_source(),
                Some(current_tab.as_str().to_string()),
                location,
            );
        }
    };

    let select_source = {
        move |new_source: String| {
            navigate(
                Some(new_source),
                Some(current_tab.as_str().to_string()),
                None,
            );
        }
    };

    let select_tab = {
        move |new_tab: Tab| {
            navigate(selected_source(), Some(new_tab.as_str().to_string()), None);
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
            armed_zone_id.set(Some(zone_id.clone()));
            save_last_zone(&zone_id);
            navigate(source, Some(current_tab.as_str().to_string()), None);
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
            let generation = status_generation.peek().wrapping_add(1);
            status_generation.set(generation);
            dioxus_sdk_time::sleep(std::time::Duration::from_secs(5)).await;
            if *status_generation.peek() == generation {
                status.set(None);
            }
        });
    });

    // ---- Unified search (#550): local filter + debounced global search ----
    // (Declared above `open_folder` so opening a folder can clear it -- see
    // there. Kept in this block so the search wiring reads as one unit.)
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
                // `peek()`, not `()`: reading `search_generation` the tracked
                // way here would subscribe THIS effect to its own signal, and
                // the `.set()` two lines down would then re-trigger this same
                // effect on every run -- a synchronous self-referential loop
                // that pegs the wasm main thread at mount (#560) before any
                // network request even completes (`query` is empty on the
                // first run, so this isn't data-driven). `peek()` reads the
                // current value without creating that subscription; the
                // async block below still re-reads `search_generation()` the
                // tracked way to detect supersession, which is fine since
                // that read happens off the render/effect call stack.
                let next = *g.peek() + 1;
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
    let current_layout = library_layout();
    let cards_active = library_uses_cards(current_layout, &current_items);
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
    } else if let Some(level_error) = current_error.clone() {
        // #573 visual pass V3: unreachability copy is reserved for actual
        // network failures; a genuine refusal renders its own reason instead
        // of claiming the provider is down. Durable locations are retried in
        // place; the UI never silently throws the user back to the root.
        let heading = if level_error.unreachable {
            format!(
                "{} isn't reachable right now.",
                source_label(current_source.as_deref().unwrap_or("this source"))
            )
        } else {
            "This view isn't available.".to_string()
        };
        let detail = level_error.message.clone();
        rsx! {
            div { class: "library-provider-down",
                p { "{heading}" }
                p { class: "text-sm text-muted", "{detail}" }
                div { class: "flex flex-wrap items-center justify-center gap-2",
                    button {
                        class: "btn btn-outline",
                        r#type: "button",
                        onclick: move |_| {
                            refresh(false);
                        },
                        "Retry"
                    }
                    // Retrying in place is right for a level that is merely
                    // stale -- the server re-walks the saved trail. A node
                    // that is permanently gone would strand the user there,
                    // so this is the deliberate way out, not an automatic one.
                    button {
                        class: "btn btn-ghost",
                        r#type: "button",
                        onclick: move |_| {
                            // Same rule as `open_folder`: leaving for the root
                            // ends the search, or the results view would stay
                            // mounted over the level we just navigated to.
                            let mut search_query = search_query;
                            search_query.set(String::new());
                            navigate(selected_source(), Some(current_tab.as_str().to_string()), None);
                        },
                        "Back to library"
                    }
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
                            layout: current_layout,
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
                                    // #573 visual pass V1: the whole result
                                    // row opens a navigable hit, same as
                                    // browse rows.
                                    onclick: {
                                        let location = result.location.clone();
                                        let title = result.title.clone();
                                        move |_| {
                                            if let Some(location) = location.clone() {
                                                open_search_hit((title.clone(), location));
                                            }
                                        }
                                    },
                                    div { class: "min-w-0 flex items-center gap-2",
                                        // #573 defect 10: search hits carry
                                        // artwork where the provider supplies
                                        // it -- same verbatim-URL contract as
                                        // browse rows (see `image_src`).
                                        if let Some(image) = result.image.clone() {
                                            img {
                                                class: "library-row-thumb",
                                                src: "{image_src(&image)}",
                                                alt: "",
                                                loading: "lazy",
                                            }
                                        }
                                        div { class: "min-w-0",
                                            p { class: "font-medium truncate", "{result.title}" }
                                            if let Some(subtitle) = result.subtitle.clone() {
                                                p { class: "text-sm text-muted truncate", "{subtitle}" }
                                            }
                                        }
                                    }
                                    div { class: "library-row-actions",
                                        if let Some(item_ref) = result.item_ref.clone() {
                                            button {
                                                class: "library-play-btn",
                                                aria_label: "Play {result.title}",
                                                r#type: "button",
                                                onclick: move |evt: Event<MouseData>| {
                                                    evt.stop_propagation();
                                                    play_item((item_ref.clone(), "play"));
                                                },
                                                "▶"
                                            }
                                        }
                                        // #566: a navigable result (a real
                                        // hit like an artist, or a grouping
                                        // row like "Albums · 35 Results")
                                        // carries a durable `location` -- open it the
                                        // same way a browse row does: push a
                                        // breadcrumb and navigate into it,
                                        // rather than leaving the row a dead
                                        // end. Independent of the play
                                        // button above: a result can have
                                        // either, both, or neither.
                                        if let Some(location) = result.location.clone() {
                                            button {
                                                class: "library-chevron-btn",
                                                aria_label: "Open {result.title}",
                                                r#type: "button",
                                                onclick: {
                                                    let title = result.title.clone();
                                                    let location = location.clone();
                                                    // stop_propagation: the row's
                                                    // own click does the same
                                                    // navigation.
                                                    move |evt: Event<MouseData>| {
                                                        evt.stop_propagation();
                                                        open_search_hit((title.clone(), location.clone()));
                                                    }
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
                layout: current_layout,
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
                        for candidate in visible_tabs() {
                            {
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

                    div { class: "library-view-toggle", role: "group", aria_label: "Library layout",
                        button {
                            class: if !cards_active { "library-view-toggle-btn library-view-toggle-btn--active" } else { "library-view-toggle-btn" },
                            r#type: "button",
                            aria_label: "List view",
                            aria_pressed: !cards_active,
                            onclick: move |_| {
                                library_layout.set(LibraryLayout::List);
                                save_library_layout(LibraryLayout::List);
                            },
                            "List"
                        }
                        button {
                            class: if cards_active { "library-view-toggle-btn library-view-toggle-btn--active" } else { "library-view-toggle-btn" },
                            r#type: "button",
                            aria_label: "Cards view",
                            aria_pressed: cards_active,
                            onclick: move |_| {
                                library_layout.set(LibraryLayout::Cards);
                                save_library_layout(LibraryLayout::Cards);
                            },
                            "Cards"
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

#[component]
pub fn LibraryHome() -> Element {
    rsx! { Library { source: None, tab: None, location: None } }
}

#[component]
pub fn LibrarySource(source: String) -> Element {
    rsx! { Library { source: Some(source), tab: None, location: None } }
}

#[component]
pub fn LibraryView(source: String, view: String) -> Element {
    rsx! { Library { source: Some(source), tab: Some(view), location: None } }
}

#[component]
pub fn LibraryLocation(source: String, view: String, location: String) -> Element {
    rsx! { Library { source: Some(source), tab: Some(view), location: Some(location) } }
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

fn library_uses_cards(layout: LibraryLayout, items: &[CollectionItem]) -> bool {
    match layout {
        LibraryLayout::Cards => true,
        LibraryLayout::List => false,
        LibraryLayout::Auto => {
            let playable_count = items.iter().filter(|item| item.item_ref.is_some()).count();
            !items.is_empty() && playable_count * 2 >= items.len()
        }
    }
}

/// Rows for one page of browse/playlists/favorites/radio results.
///
/// Per-level layout follows the design brief's "art-first, list otherwise"
/// split: a page made mostly of playable leaves (tracks/albums, `ref` set)
/// renders as an artwork-forward grid; a page made mostly of folders
/// (`location` set, no `ref`) renders as compact navigable rows. `CollectionItem`
/// carries artwork when a provider has it; the grid tile falls back to a
/// text placeholder for rows without art.
#[component]
fn LibraryRows(
    items: Vec<CollectionItem>,
    layout: LibraryLayout,
    tab: Tab,
    currently_playing_title: Option<String>,
    on_open: EventHandler<(String, String)>,
    on_play: EventHandler<(String, &'static str)>,
) -> Element {
    let grid_shaped = library_uses_cards(layout, &items);

    if grid_shaped {
        rsx! {
            ul { class: "library-grid",
                for (index , item) in items.iter().cloned().enumerate() {
                    LibraryTile {
                        key: "{item.title}-{item.location.clone().unwrap_or_default()}-{item.item_ref.clone().unwrap_or_default()}",
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
                        key: "{item.title}-{item.location.clone().unwrap_or_default()}-{item.item_ref.clone().unwrap_or_default()}",
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
            // #573 visual pass V1: the whole row is the navigation target
            // for a navigable item, matching the row-wide hover affordance
            // -- not just the right-edge chevron. The play/overflow/chevron
            // buttons stop propagation so their own actions stay separate
            // hit areas.
            onclick: {
                let location = item.location.clone();
                let title = item.title.clone();
                move |_| {
                    if let Some(location) = location.clone() {
                        on_open((title.clone(), location.clone()));
                    }
                }
            },
            div { class: "min-w-0 flex items-center gap-2",
                if let Some(image) = item.image.clone() {
                    img {
                        class: "library-row-thumb",
                        // #573 defect 2: `image` is already the complete URL;
                        // it must be used verbatim (see `image_src`).
                        src: "{image_src(&image)}",
                        alt: "",
                        loading: "lazy",
                    }
                }
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
                            move |evt: Event<MouseData>| {
                                evt.stop_propagation();
                                on_play((item_ref.clone(), "play"));
                            }
                        },
                        "▶"
                    }
                    div { class: "library-overflow",
                        button {
                            class: "library-overflow-trigger",
                            aria_label: "More actions for {item.title}",
                            r#type: "button",
                            onclick: move |evt: Event<MouseData>| {
                                evt.stop_propagation();
                                menu_open.toggle();
                            },
                            "⋯"
                        }
                        if menu_open() {
                            div { class: "library-overflow-menu",
                                button {
                                    r#type: "button",
                                    onclick: {
                                        let item_ref = item_ref.clone();
                                        move |evt: Event<MouseData>| {
                                            evt.stop_propagation();
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
                                        move |evt: Event<MouseData>| {
                                            evt.stop_propagation();
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
                if let Some(location) = item.location.clone() {
                    button {
                        class: "library-chevron-btn",
                        aria_label: "Open {item.title}",
                        r#type: "button",
                        onclick: {
                            let title = item.title.clone();
                            let location = location.clone();
                            // stop_propagation so the row's own click
                            // handler (same navigation) doesn't fire twice.
                            move |evt: Event<MouseData>| {
                                evt.stop_propagation();
                                on_open((title.clone(), location.clone()));
                            }
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
    let has_location = item.location.is_some();

    rsx! {
        li {
            class: "library-tile library-tile--reveal",
            style: "{stagger_style}",
            // #573 visual pass V1: the whole tile navigates (art, title,
            // subtitle alike), not just the art block -- the play button
            // below stops propagation to stay its own hit area.
            onclick: {
                let location = item.location.clone();
                let title = item.title.clone();
                move |_| {
                    if let Some(location) = location.clone() {
                        on_open((title.clone(), location.clone()));
                    }
                }
            },
            div {
                class: "library-tile-art",
                if let Some(image) = item.image.clone() {
                    img {
                        class: "library-tile-art-img",
                        // #573 defect 2: `image` is already the complete URL;
                        // it must be used verbatim (see `image_src`).
                        src: "{image_src(&image)}",
                        alt: "",
                        loading: "lazy",
                    }
                } else {
                    span { class: "library-tile-placeholder", "♪" }
                }
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
                if has_location {
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

/// #557 regression guards for the fetch/render loop that pinned the
/// browser: the render-loop root cause lived in two effects
/// (`use_effect` bodies that spawn network requests on `zones_list()` and
/// SSE event changes), which can't be driven directly without a full
/// Dioxus VDOM test harness. These tests instead exercise the pure
/// decision functions those effects delegate to -- `should_restart_zones`
/// gates the `/zones` refetch, `missing_now_playing_zones` gates the
/// `/now_playing` fan-out -- so the loop-breaking logic itself has direct
/// coverage even though the effects' wiring does not. A scripted
/// fullstack run against `tests/mock_servers` (seek events on a playing
/// zone, counted `/zones` + `/now_playing` hits over time) would cover the
/// wiring too, but is infeasible in this sandbox (no browser/wasm runtime
/// to drive the SSE + effect loop end-to-end); see #557's PR description
/// for this substitution.
#[cfg(test)]
mod loop_regression_tests {
    use super::*;
    use crate::app::sse::{VolumePayload, ZonePayload};

    fn zone(id: &str) -> Zone {
        Zone {
            zone_id: id.to_string(),
            ..Zone::default()
        }
    }

    fn zone_payload(zone_id: &str) -> ZonePayload {
        ZonePayload {
            zone_id: zone_id.to_string(),
        }
    }

    // ---- should_restart_zones: structural events only ----

    #[test]
    fn structural_events_restart_zones() {
        for event in [
            SseEvent::ZoneRemoved {
                payload: zone_payload("roon:1"),
            },
            SseEvent::RoonConnected,
            SseEvent::RoonDisconnected,
            SseEvent::LmsConnected,
            SseEvent::LmsDisconnected,
        ] {
            assert!(
                should_restart_zones(Some(&event)),
                "expected {event:?} to restart the zones resource"
            );
        }
    }

    /// The exact regression #557 reports: a playing zone's `ZoneUpdated`
    /// stream must NOT restart the zones resource, or every one of those
    /// events re-triggers the `/zones` -> `/now_playing`-fan-out chain --
    /// the fetch loop that pinned the browser.
    #[test]
    fn zone_updated_does_not_restart_zones() {
        let event = SseEvent::ZoneUpdated {
            payload: zone_payload("roon:1"),
        };
        assert!(!should_restart_zones(Some(&event)));
    }

    #[test]
    fn playback_and_volume_events_do_not_restart_zones() {
        for event in [
            SseEvent::NowPlayingChanged {
                payload: zone_payload("roon:1"),
            },
            SseEvent::SeekPositionChanged {
                payload: zone_payload("roon:1"),
            },
            SseEvent::VolumeChanged {
                payload: VolumePayload {
                    output_id: "roon:1".to_string(),
                    value: 10.0,
                    is_muted: false,
                },
            },
        ] {
            assert!(
                !should_restart_zones(Some(&event)),
                "expected {event:?} not to restart the zones resource"
            );
        }
    }

    #[test]
    fn no_event_does_not_restart_zones() {
        assert!(!should_restart_zones(None));
    }

    // ---- missing_now_playing_zones: the loop-breaking no-op guard ----

    #[test]
    fn all_zones_missing_on_first_load() {
        let list = vec![zone("roon:1"), zone("roon:2")];
        let now_playing = HashMap::new();
        let missing = missing_now_playing_zones(&list, &now_playing);
        assert_eq!(missing.len(), 2);
    }

    /// The other half of #557's regression: once every known zone has a
    /// `now_playing` entry, a `zones_list()` rerun triggered by a restart
    /// (e.g. a genuine `ZoneDiscovered` for an unrelated zone) must not
    /// re-fetch zones that were already fetched -- that whole-map refetch
    /// on every rerun was the other engine of the loop.
    #[test]
    fn already_known_zones_are_not_refetched() {
        let list = vec![zone("roon:1"), zone("roon:2")];
        let mut now_playing = HashMap::new();
        now_playing.insert("roon:1".to_string(), NowPlaying::default());
        now_playing.insert("roon:2".to_string(), NowPlaying::default());
        let missing = missing_now_playing_zones(&list, &now_playing);
        assert!(missing.is_empty());
    }

    #[test]
    fn only_the_newly_discovered_zone_is_fetched() {
        let list = vec![zone("roon:1"), zone("roon:2")];
        let mut now_playing = HashMap::new();
        now_playing.insert("roon:1".to_string(), NowPlaying::default());
        let missing = missing_now_playing_zones(&list, &now_playing);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].zone_id, "roon:2");
    }

    #[test]
    fn empty_zone_list_fetches_nothing() {
        let missing = missing_now_playing_zones(&[], &HashMap::new());
        assert!(missing.is_empty());
    }
}

/// #573 pins for the Library page's pure helpers.
#[cfg(test)]
mod library_defect_pins {
    use super::*;

    /// Defect 2 pin: the API's `image` field is the complete `<img src>`
    /// value. It must be used verbatim -- prefixing it again
    /// (`/api/collections/image?ref={image}` around an already-full path)
    /// is the double-prefix regression that 404ed every image.
    #[test]
    fn api_image_url_is_used_verbatim_as_img_src() {
        let api_value = "/api/collections/image?ref=ref_abc123";
        let src = image_src(api_value);
        assert_eq!(src, api_value, "the API value must pass through verbatim");
        assert!(
            !src.contains("?ref=/api/"),
            "src must never be double-prefixed"
        );
    }

    /// Defect 6 pin: a Roon zone's served-tab list hides Favorites and
    /// Radio; the rendered set is exactly what the server reports.
    #[test]
    fn roon_tab_list_hides_favorites_and_radio() {
        let served = vec!["browse".to_string(), "playlists".to_string()];
        assert_eq!(visible_tabs_for(&served), vec![Tab::Browse, Tab::Playlists]);
    }

    /// Defect 6 pin: no tab information (a `/zones` response from a build
    /// predating `library_tabs`) degrades to showing every tab, never none.
    #[test]
    fn missing_tab_information_shows_every_tab() {
        assert_eq!(visible_tabs_for(&[]), TABS.to_vec());
    }

    /// Defect 6 pin: a deep link to a tab this provider does not serve
    /// lands on Browse instead of a permanent refusal page.
    #[test]
    fn unsupported_deep_linked_tab_falls_back_to_browse() {
        let visible = vec![Tab::Browse, Tab::Playlists];
        assert_eq!(effective_tab(Tab::Favorites, &visible), Tab::Browse);
        assert_eq!(effective_tab(Tab::Radio, &visible), Tab::Browse);
        assert_eq!(effective_tab(Tab::Playlists, &visible), Tab::Playlists);
    }

    /// The Bridge's Load more button must use the continuation returned by the
    /// provider. Reusing the request offset makes every click fetch page one.
    #[test]
    fn loaded_page_advances_to_provider_continuation() {
        let mut continuation = None;
        let mut requests = Vec::new();

        for append in [false, true, true, true, false] {
            let Some(offset) = next_request_offset(append, continuation) else {
                continue;
            };
            requests.push(offset);
            continuation = match offset {
                0 => Some(30),
                30 => Some(60),
                60 => None,
                _ => unreachable!("unexpected test page offset"),
            };
        }

        assert_eq!(requests, vec![0, 30, 60, 0]);
        assert_eq!(next_request_offset(true, None), None);
        assert_eq!(next_request_offset(false, None), Some(0));
        assert_eq!(next_request_offset(false, Some(60)), Some(0));
    }

    #[test]
    fn layout_preference_overrides_automatic_shape_without_changing_items() {
        let folders = vec![CollectionItem {
            title: "Folder".to_string(),
            location: Some("loc_folder".to_string()),
            ..CollectionItem::default()
        }];
        let tracks = vec![CollectionItem {
            title: "Track".to_string(),
            item_ref: Some("ref_track".to_string()),
            ..CollectionItem::default()
        }];

        assert!(!library_uses_cards(LibraryLayout::Auto, &folders));
        assert!(library_uses_cards(LibraryLayout::Auto, &tracks));
        assert!(library_uses_cards(LibraryLayout::Cards, &folders));
        assert!(!library_uses_cards(LibraryLayout::List, &tracks));
    }
}
