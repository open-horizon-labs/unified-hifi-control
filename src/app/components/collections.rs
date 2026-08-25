//! Library browse + queue transfer panel for a zone card (#507).
//!
//! Calls the same `/api/collections`, `/api/queue` and `/api/play_ref`
//! endpoints the MCP tools back (see `src/api/browse.rs`), so this panel and
//! an MCP agent see the same capability surface: browse/playlists/favorites,
//! opaque playable refs, and queue transfer between Music Assistant zones.
//! Only rendered for zones whose adapter advertises collections support
//! (Music Assistant today) -- callers gate that, this component assumes it.

use crate::app::api::{
    self, CollectionItem, CollectionsRequest, PlayRefRequest, QueueRequest,
};
use dioxus::prelude::*;

const PAGE_LIMIT: u32 = 20;

#[derive(Clone, PartialEq, Props)]
pub struct CollectionsBrowserProps {
    /// The zone this panel browses and plays into.
    pub zone_id: String,
    /// Other zones a queue can be transferred to: `(zone_id, zone_name)`.
    #[props(default)]
    pub transfer_targets: Vec<(String, String)>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Browse,
    Playlists,
    Favorites,
    Radio,
}

impl Tab {
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

/// Fetch one page of a collection and apply it to the panel's signals.
/// A free function rather than a captured closure: `Signal<T>` is `Copy`, so
/// this can be called from any number of independent event handlers without
/// fighting move-closure ownership.
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

/// Expandable collections browser + queue transfer control for one zone.
#[component]
pub fn CollectionsBrowser(props: CollectionsBrowserProps) -> Element {
    // A `Signal` is `Copy`, unlike the `String` in `props`, so every handler
    // closure below can capture it independently without fighting move
    // semantics over a single owned `String`.
    let zone_id = use_signal(|| props.zone_id.clone());
    let mut expanded = use_signal(|| false);
    let mut tab = use_signal(|| Tab::Browse);
    let mut path_stack = use_signal(Vec::<(String, Option<String>)>::new);
    let items = use_signal(Vec::<CollectionItem>::new);
    let mut offset = use_signal(|| 0u32);
    let next_offset = use_signal(|| None::<u32>);
    let loading = use_signal(|| false);
    let error = use_signal(|| None::<String>);
    let mut status = use_signal(|| None::<String>);
    let mut transfer_target = use_signal({
        let first = props.transfer_targets.first().map(|(id, _)| id.clone());
        move || first.clone()
    });

    // `use_callback` closes over props/state by reference and is itself
    // `Copy`, so every handler below can hold and call the same instance.
    let refresh = use_callback(move |append: bool| {
        let zone_id = zone_id();
        let action = tab.read().action().to_string();
        let media_type = tab.read().media_type().map(ToOwned::to_owned);
        let path = path_stack.read().last().and_then(|(_, p)| p.clone());
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

    let toggle_open = move |_| {
        let opening = !expanded();
        expanded.set(opening);
        if opening && items.read().is_empty() {
            refresh(false);
        }
    };

    let select_tab = use_callback(move |new_tab: Tab| {
        tab.set(new_tab);
        path_stack.set(Vec::new());
        offset.set(0);
        refresh(false);
    });

    let open_folder = use_callback(move |(title, path): (String, String)| {
        path_stack.with_mut(|stack| stack.push((title, Some(path))));
        offset.set(0);
        refresh(false);
    });

    let go_back = move |_| {
        path_stack.with_mut(|stack| {
            stack.pop();
        });
        offset.set(0);
        refresh(false);
    };

    let load_more = move |_| refresh(true);

    let play_item = use_callback(move |(item_ref, action): (String, &'static str)| {
        let zone_id = zone_id();
        status.set(Some("Working...".to_string()));
        spawn(async move {
            let req = PlayRefRequest {
                item_ref,
                zone_id,
                action: action.to_string(),
            };
            let message = match api::post_play_ref(&req).await {
                Ok(env) if env.is_ok() => {
                    if action == "queue" {
                        "Added to queue".to_string()
                    } else {
                        "Playing".to_string()
                    }
                }
                Ok(env) => env.error_detail(),
                Err(message) => message,
            };
            status.set(Some(message));
        });
    });

    let do_transfer = move |_| {
        let Some(target) = transfer_target() else {
            return;
        };
        let zone_id = zone_id();
        status.set(Some("Transferring queue...".to_string()));
        spawn(async move {
            let req = QueueRequest {
                zone_id,
                action: "transfer".to_string(),
                item_id: None,
                position: None,
                target_zone_id: Some(target),
            };
            let message = match api::post_queue_action(&req).await {
                Ok(env) if env.is_ok() => "Queue transferred".to_string(),
                Ok(env) => env.error_detail(),
                Err(message) => message,
            };
            status.set(Some(message));
        });
    };

    let breadcrumb_labels: Vec<String> = path_stack.read().iter().map(|(t, _)| t.clone()).collect();
    let has_history = !path_stack.read().is_empty();
    let current_tab = tab();
    let current_items = items();
    let is_loading = loading();
    let current_error = error();
    let current_status = status();
    let can_load_more = next_offset.read().is_some();
    let has_transfer_targets = !props.transfer_targets.is_empty();
    let transfer_targets = props.transfer_targets.clone();

    rsx! {
        div { class: "mt-3 border-t border-subtle pt-3",
            button {
                class: "btn btn-ghost text-sm",
                r#type: "button",
                onclick: toggle_open,
                if expanded() { "Hide library" } else { "Browse library" }
            }

            if expanded() {
                div { class: "mt-3 space-y-3",
                    // Tabs
                    div { class: "flex flex-wrap gap-2",
                        for candidate in TABS {
                            {
                                let candidate = *candidate;
                                let active = candidate == current_tab;
                                rsx! {
                                    button {
                                        key: "{candidate.label()}",
                                        class: if active { "badge badge-primary" } else { "badge" },
                                        r#type: "button",
                                        onclick: move |_| select_tab(candidate),
                                        "{candidate.label()}"
                                    }
                                }
                            }
                        }
                    }

                    // Breadcrumb / back
                    if has_history {
                        div { class: "flex items-center gap-2 text-sm text-muted",
                            button { class: "btn btn-ghost", r#type: "button", onclick: go_back, "< Back" }
                            span { "{breadcrumb_labels.join(\" / \")}" }
                        }
                    }

                    if let Some(message) = current_status.clone() {
                        p { class: "text-sm text-muted", "{message}" }
                    }
                    if let Some(message) = current_error.clone() {
                        p { class: "text-sm text-error", "{message}" }
                    }
                    if is_loading && current_items.is_empty() {
                        p { class: "text-sm text-muted", "Loading..." }
                    } else if current_items.is_empty() {
                        p { class: "text-sm text-muted", "Nothing here." }
                    } else {
                        ul { class: "space-y-2",
                            for item in current_items.iter().cloned() {
                                li {
                                    key: "{item.title}-{item.path.clone().unwrap_or_default()}-{item.item_ref.clone().unwrap_or_default()}",
                                    class: "flex items-center justify-between gap-2",
                                    div { class: "min-w-0",
                                        p { class: "text-sm font-medium truncate", "{item.title}" }
                                        if let Some(subtitle) = item.subtitle.clone() {
                                            p { class: "text-xs text-muted truncate", "{subtitle}" }
                                        }
                                    }
                                    div { class: "flex gap-2 flex-shrink-0",
                                        if let Some(path) = item.path.clone() {
                                            button {
                                                class: "btn btn-ghost text-sm",
                                                r#type: "button",
                                                onclick: {
                                                    let title = item.title.clone();
                                                    let path = path.clone();
                                                    move |_| open_folder((title.clone(), path.clone()))
                                                },
                                                "Open"
                                            }
                                        }
                                        if let Some(item_ref) = item.item_ref.clone() {
                                            button {
                                                class: "btn btn-primary text-sm",
                                                r#type: "button",
                                                onclick: {
                                                    let item_ref = item_ref.clone();
                                                    move |_| play_item((item_ref.clone(), "play"))
                                                },
                                                "Play"
                                            }
                                            button {
                                                class: "btn btn-ghost text-sm",
                                                r#type: "button",
                                                onclick: {
                                                    let item_ref = item_ref.clone();
                                                    move |_| play_item((item_ref.clone(), "queue"))
                                                },
                                                "Queue"
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        if can_load_more {
                            button {
                                class: "btn btn-ghost text-sm",
                                r#type: "button",
                                onclick: load_more,
                                "Load more"
                            }
                        }
                    }

                    if has_transfer_targets {
                        div { class: "flex items-center gap-2 pt-2 border-t border-subtle",
                            span { class: "text-sm text-muted", "Transfer queue to:" }
                            select {
                                class: "form-select text-sm",
                                onchange: move |evt| transfer_target.set(Some(evt.value())),
                                for (target_id, target_name) in transfer_targets.iter().cloned() {
                                    option { key: "{target_id}", value: "{target_id}", "{target_name}" }
                                }
                            }
                            button {
                                class: "btn btn-ghost text-sm",
                                r#type: "button",
                                onclick: do_transfer,
                                "Transfer"
                            }
                        }
                    }
                }
            }
        }
    }
}
