//! HTTP surface for controller shortcuts: pinning from the Library, per-controller maps, the
//! curator's test press, and the endpoint a controller posts its button events to.
//!
//! Errors keep one shape, `{ "error", "error_code" }`, with `error` already worded for the person
//! reading it. Nothing here reports a write as saved unless the store confirmed it.

use std::collections::BTreeMap;

use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    Json,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::api::AppState;
use crate::shortcuts::resolve::{self, PressOutcome};
use crate::shortcuts::{
    event_key, split_event_key, BindingError, LastPress, PinRefusal, Shortcut, ShortcutTarget,
};

type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

/// `error` carries the sentence, the field every client helper already reads from a failed
/// response; `error_code` is the stable value to branch on.
fn err(status: StatusCode, code: &str, message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        status,
        Json(json!({ "error": message.into(), "error_code": code })),
    )
}

fn storage_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    tracing::warn!(%error, "Shortcut request failed");
    err(
        StatusCode::INTERNAL_SERVER_ERROR,
        "storage",
        "Shortcuts could not be saved or loaded; check the bridge logs.",
    )
}

#[derive(Serialize)]
struct ShortcutView {
    id: String,
    name: String,
    title: String,
    provider: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_press: Option<LastPress>,
}

async fn view(state: &AppState, shortcut: &Shortcut) -> ShortcutView {
    ShortcutView {
        id: shortcut.id.clone(),
        name: shortcut.name.clone(),
        title: shortcut.title.clone(),
        provider: shortcut.target.provider_label(),
        last_press: state.shortcuts.last_press(&shortcut.id).await,
    }
}

#[derive(Deserialize, Default)]
pub struct ListQuery {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// Legacy callers may omit pagination. Controller projections are bounded to 12 rows.
pub async fn list_handler(
    State(state): State<AppState>,
    Query(query): Query<ListQuery>,
) -> ApiResult {
    let all = state.shortcuts.list().await;
    let offset = query.offset.unwrap_or(0);
    let limit = if query.offset.is_some() || query.limit.is_some() {
        query.limit.unwrap_or(12).clamp(1, 12)
    } else {
        all.len()
    };
    let paged = query.offset.is_some() || query.limit.is_some();
    let mut shortcuts = Vec::new();
    for shortcut in all.iter().skip(offset).take(limit) {
        if paged {
            shortcuts.push(json!({
                "id": shortcut.id,
                "name": shortcut.name.chars().take(160).collect::<String>(),
                "provider": shortcut.target.provider_label(),
            }));
        } else {
            shortcuts.push(
                serde_json::to_value(view(&state, shortcut).await)
                    .map_err(|error| storage_error(error.into()))?,
            );
        }
    }
    let end = offset.saturating_add(shortcuts.len());
    let next_offset = if end < all.len() { Some(end) } else { None };
    Ok(Json(
        json!({ "version": 1, "shortcuts": shortcuts, "next_offset": next_offset }),
    ))
}

#[derive(Deserialize)]
pub struct PlayRequest {
    /// Explicit target, captured when the listener tapped. Never fall back to another zone.
    pub zone_id: String,
}

/// Direct selection and remote bindings share the same durable provider resolver.
/// Hardware identity adds a current-zone precondition; it is not a button mapping.
pub async fn play_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(req): Json<PlayRequest>,
) -> ApiResult {
    if req.zone_id.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "no_zone",
            "Choose a zone first.",
        ));
    }
    if let Some(controller_id) = crate::knobs::routes::extract_knob_id(&headers, None) {
        if state.knobs.get(&controller_id).await.is_none() {
            return Err(err(
                StatusCode::NOT_FOUND,
                "unknown_controller",
                "Controller is not registered yet.",
            ));
        }
        if resolve::controller_zone(&state, &controller_id)
            .await
            .as_deref()
            != Some(req.zone_id.as_str())
        {
            return Err(err(
                StatusCode::CONFLICT,
                "zone_changed",
                "Zone changed. Open Shortcuts again.",
            ));
        }
    }
    let shortcut = state.shortcuts.get(&id).await.ok_or_else(|| {
        err(
            StatusCode::NOT_FOUND,
            "unknown_shortcut",
            "Shortcut removed. Refresh Shortcuts.",
        )
    })?;
    Ok(Json(outcome_json(
        resolve::play(&state, &shortcut, &req.zone_id).await,
    )))
}

#[derive(Deserialize)]
pub struct PinRequest {
    /// A ref the Library minted for a playable item.
    #[serde(rename = "ref")]
    pub item_ref: String,
    pub name: Option<String>,
}

/// `POST /api/shortcuts` -- pin the item behind a Library ref.
///
/// The ref is resolved here and only the durable part is stored; the client never sees provider
/// internals. A ref that expired while the page sat open is a 410 with the way out spelled out.
pub async fn pin_handler(State(state): State<AppState>, Json(req): Json<PinRequest>) -> ApiResult {
    let Some(ref_target) = state.mcp_refs.resolve(&req.item_ref).await else {
        let refusal = PinRefusal::UnknownRef(
            "That item's link expired while the page was open. Reload the Library and pin it again."
                .into(),
        );
        return Err(err(StatusCode::GONE, refusal.code(), refusal.message()));
    };
    let target = ShortcutTarget::from_ref(&ref_target).map_err(|refusal| {
        err(
            StatusCode::UNPROCESSABLE_ENTITY,
            refusal.code(),
            refusal.message(),
        )
    })?;
    let (shortcut, already_pinned) = state
        .shortcuts
        .pin(target, ref_target.title().to_string(), req.name)
        .await
        .map_err(storage_error)?;
    Ok(Json(json!({
        "shortcut": view(&state, &shortcut).await,
        "already_pinned": already_pinned,
    })))
}

#[derive(Deserialize)]
pub struct RenameRequest {
    pub name: String,
}

/// `PATCH /api/shortcuts/{id}`
pub async fn rename_handler(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(req): Json<RenameRequest>,
) -> ApiResult {
    if req.name.trim().is_empty() {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "empty_name",
            "A shortcut needs a name.",
        ));
    }
    match state
        .shortcuts
        .rename(&id, &req.name)
        .await
        .map_err(storage_error)?
    {
        Some(shortcut) => Ok(Json(json!({ "shortcut": view(&state, &shortcut).await }))),
        None => Err(err(
            StatusCode::NOT_FOUND,
            "unknown_shortcut",
            "No such shortcut.",
        )),
    }
}

/// `DELETE /api/shortcuts/{id}` -- also unbinds it from every controller.
pub async fn delete_handler(State(state): State<AppState>, Path(id): Path<String>) -> ApiResult {
    if state.shortcuts.delete(&id).await.map_err(storage_error)? {
        Ok(Json(json!({ "ok": true })))
    } else {
        Err(err(
            StatusCode::NOT_FOUND,
            "unknown_shortcut",
            "No such shortcut.",
        ))
    }
}

/// How one controller's map reads against current zone state.
async fn controller_view(state: &AppState, controller_id: &str) -> Option<Value> {
    let knob = state.knobs.get(controller_id).await?;
    let zone_id = resolve::controller_zone(state, controller_id).await;

    let global_hidden = crate::api::load_app_settings()
        .hidden_zone_ids()
        .iter()
        .any(|hidden| Some(hidden) == zone_id.as_ref());
    let hidden_here = zone_id
        .as_ref()
        .is_some_and(|id| knob.config.hidden_zone_ids.contains(id));
    let zone = match &zone_id {
        Some(id) => state.aggregator.get_zone(id).await,
        None => None,
    };
    let zone_state = match (&zone_id, &zone) {
        (None, _) => "none",
        (Some(_), None) => "missing",
        (Some(_), Some(_)) => "ok",
    };

    let mut bindings = Vec::new();
    for (event, shortcut_id) in state.shortcuts.bindings_for(controller_id).await {
        let Some(shortcut) = state.shortcuts.get(&shortcut_id).await else {
            continue;
        };
        let (key, gesture) = split_event_key(&event).unwrap_or((event.as_str(), ""));
        let last = state
            .shortcuts
            .last_binding_press(controller_id, &event, &shortcut_id, zone_id.as_deref())
            .await;
        let mismatch = zone_id.as_deref().is_some_and(|id| {
            crate::mcp::routing::ZoneTarget::classify(id).provider() != shortcut.target.provider()
        });
        // The chip a person reads: the first thing that would stop this button working, then what
        // the last real press did, then "untested". A hidden zone is a note, not a failure.
        let status = match zone_state {
            "none" => "no_zone",
            "missing" => "zone_missing",
            _ if mismatch => "provider_mismatch",
            _ => match &last {
                Some(press) if !press.ok => "last_failed",
                Some(_) => "ok",
                None => "untested",
            },
        };
        bindings.push(json!({
            "event": event,
            "key": key,
            "gesture": gesture,
            "shortcut_id": shortcut.id,
            "shortcut_name": shortcut.name,
            "provider": shortcut.target.provider_label(),
            "status": status,
            "last_press": last,
        }));
    }

    Some(json!({
        "controller_id": controller_id,
        "plays_on": {
            "zone_id": zone_id,
            "zone_name": zone.as_ref().map(|z| z.zone_name.clone()),
            "state": zone_state,
            "hidden_globally": global_hidden,
            "hidden_here": hidden_here,
        },
        "bindings": bindings,
        "recent_events": state.shortcuts.recent_events(controller_id).await,
    }))
}

/// `GET /api/controllers/{id}/shortcuts`
pub async fn controller_get_handler(
    State(state): State<AppState>,
    Path(controller_id): Path<String>,
) -> ApiResult {
    controller_view(&state, &controller_id)
        .await
        .map(Json)
        .ok_or_else(|| {
            err(
                StatusCode::NOT_FOUND,
                "unknown_controller",
                "No such controller.",
            )
        })
}

#[derive(Deserialize)]
pub struct MapRequest {
    /// Event key (`"<key>:<gesture>"`) to shortcut id. Replaces the whole map.
    pub bindings: BTreeMap<String, String>,
}

/// `PUT /api/controllers/{id}/shortcuts`
pub async fn controller_put_handler(
    State(state): State<AppState>,
    Path(controller_id): Path<String>,
    Json(req): Json<MapRequest>,
) -> ApiResult {
    if state.knobs.get(&controller_id).await.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            "unknown_controller",
            "No such controller.",
        ));
    }
    state
        .shortcuts
        .set_bindings(&controller_id, req.bindings)
        .await
        .map_err(|error| match error {
            BindingError::Event { event, refusal } => err(
                StatusCode::UNPROCESSABLE_ENTITY,
                refusal.code(),
                format!("{event}: {}", refusal.message()),
            ),
            BindingError::UnknownShortcut(id) => err(
                StatusCode::UNPROCESSABLE_ENTITY,
                "unknown_shortcut",
                format!("Shortcut {id} no longer exists."),
            ),
            BindingError::Storage(error) => storage_error(error),
        })?;
    controller_get_handler(State(state), Path(controller_id)).await
}

#[derive(Deserialize)]
pub struct TestRequest {
    pub key: String,
    pub gesture: String,
}

/// `POST /api/controllers/{id}/shortcuts/test` -- press a button from the UI. Real playback on the
/// controller's own zone, through the same path a physical press takes.
pub async fn controller_test_handler(
    State(state): State<AppState>,
    Path(controller_id): Path<String>,
    Json(req): Json<TestRequest>,
) -> ApiResult {
    if state.knobs.get(&controller_id).await.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            "unknown_controller",
            "No such controller.",
        ));
    }
    let event = event_key(&req.key, &req.gesture)
        .map_err(|r| err(StatusCode::UNPROCESSABLE_ENTITY, r.code(), r.message()))?;
    Ok(Json(outcome_json(
        resolve::press(&state, &controller_id, &event).await,
    )))
}

#[derive(Deserialize)]
pub struct InputRequest {
    pub key: String,
    pub gesture: String,
}

/// `POST /knob/input` -- a controller reports a button press it recognised locally.
///
/// Always answers 200 with an outcome the controller can show, including `unmapped`, because a
/// press that is not a shortcut is the common case and not a fault. Malformed input is 4xx.
pub async fn input_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<InputRequest>,
) -> ApiResult {
    let Some(controller_id) = crate::knobs::routes::extract_knob_id(&headers, None) else {
        return Err(err(
            StatusCode::BAD_REQUEST,
            "missing_controller_id",
            "x-knob-id or x-device-id is required.",
        ));
    };
    let event = match event_key(&req.key, &req.gesture) {
        Ok(event) => event,
        // A transport key's plain press is not an error the controller caused: it simply has no
        // shortcut, and its normal action is the controller's own business.
        Err(
            crate::shortcuts::EventRefusal::TransportPress
            | crate::shortcuts::EventRefusal::ReservedKey,
        ) => {
            return Ok(Json(outcome_json(PressOutcome::unmapped_transport())));
        }
        Err(refusal) => {
            return Err(err(
                StatusCode::BAD_REQUEST,
                refusal.code(),
                refusal.message(),
            ))
        }
    };
    if state.knobs.get(&controller_id).await.is_none() {
        return Err(err(
            StatusCode::NOT_FOUND,
            "unknown_controller",
            "This controller is not registered with the bridge yet.",
        ));
    }
    Ok(Json(outcome_json(
        resolve::press(&state, &controller_id, &event).await,
    )))
}

fn outcome_json(outcome: PressOutcome) -> Value {
    serde_json::to_value(outcome).unwrap_or_else(|_| json!({ "ok": false, "code": "refused" }))
}
