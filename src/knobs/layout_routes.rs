//! Axum API handlers for the layout + device stack system.
//!
//! Endpoints:
//! - GET/POST        /api/layouts           — list / create layouts
//! - GET/PUT/DELETE  /api/layouts/{id}       — read / update / delete a layout
//! - GET/PUT         /api/devices/{device_id}/stack          — get / replace stack
//! - POST            /api/devices/{device_id}/stack/entries   — add entry
//! - DELETE           /api/devices/{device_id}/stack/entries/{index} — remove entry
//! - POST            /api/devices/{device_id}/stack/reorder   — reorder stack

use axum::{
    extract::{Path, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::api::AppState;
use crate::knobs::layout_store::{Condition, DeviceStack, Layout, StackEntry};
use crate::knobs::manifest::{Nav, Screen};

// ── Request types ───────────────────────────────────────────────────────────

/// Body for POST /api/layouts
#[derive(Debug, Deserialize)]
pub struct CreateLayoutRequest {
    pub name: String,
    pub screens: Vec<Screen>,
    pub nav: Nav,
    #[serde(default)]
    pub interactions: Option<HashMap<String, String>>,
}

/// Body for PUT /api/layouts/{id}
#[derive(Debug, Deserialize)]
pub struct UpdateLayoutRequest {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub screens: Option<Vec<Screen>>,
    #[serde(default)]
    pub nav: Option<Nav>,
    #[serde(default)]
    pub interactions: Option<HashMap<String, String>>,
}

/// Body for PUT /api/devices/{device_id}/stack
#[derive(Debug, Deserialize)]
pub struct SetStackRequest {
    pub entries: Vec<StackEntry>,
}

/// Body for POST /api/devices/{device_id}/stack/entries
#[derive(Debug, Deserialize)]
pub struct AddStackEntryRequest {
    pub layout_id: String,
    #[serde(default)]
    pub conditions: Vec<Condition>,
    #[serde(default = "crate::knobs::layout_store::default_true")]
    pub enabled: bool,
}

/// Body for POST /api/devices/{device_id}/stack/reorder
#[derive(Debug, Deserialize)]
pub struct ReorderStackRequest {
    pub from: usize,
    pub to: usize,
}

// ── Response types ──────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct LayoutListResponse {
    pub layouts: Vec<Layout>,
}

#[derive(Debug, Serialize)]
pub struct ErrorResponse {
    pub error: String,
}

// ── Helper ──────────────────────────────────────────────────────────────────

fn error_json(msg: impl Into<String>) -> Json<ErrorResponse> {
    Json(ErrorResponse { error: msg.into() })
}

// ── Layout handlers ─────────────────────────────────────────────────────────

/// GET /api/layouts — List all layouts
pub async fn list_layouts(State(state): State<AppState>) -> Json<LayoutListResponse> {
    let layouts = state.layouts.list_layouts().await;
    Json(LayoutListResponse { layouts })
}

/// POST /api/layouts — Create a layout. Returns the created layout directly.
pub async fn create_layout(
    State(state): State<AppState>,
    Json(body): Json<CreateLayoutRequest>,
) -> Result<(StatusCode, Json<Layout>), (StatusCode, Json<ErrorResponse>)> {
    if body.name.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            error_json("name must not be empty"),
        ));
    }

    let layout = state
        .layouts
        .create_layout(body.name, body.screens, body.nav, body.interactions)
        .await;

    Ok((StatusCode::CREATED, Json(layout)))
}

/// GET /api/layouts/{id} — Get a layout by ID
pub async fn get_layout(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Layout>, (StatusCode, Json<ErrorResponse>)> {
    state
        .layouts
        .get_layout(&id)
        .await
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                error_json(format!("layout '{}' not found", id)),
            )
        })
}

/// DELETE /api/layouts/{id} — Delete a layout
pub async fn delete_layout(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    if state.layouts.delete_layout(&id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((
            StatusCode::NOT_FOUND,
            error_json(format!("layout '{}' not found", id)),
        ))
    }
}

/// PUT /api/layouts/{id} — Update a layout
pub async fn update_layout(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<UpdateLayoutRequest>,
) -> Result<Json<Layout>, (StatusCode, Json<ErrorResponse>)> {
    state
        .layouts
        .update_layout(&id, body.name, body.screens, body.nav, body.interactions)
        .await
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                error_json(format!("layout '{}' not found", id)),
            )
        })
}

// ── Device stack handlers ───────────────────────────────────────────────────

/// GET /api/devices/{device_id}/stack — Get a device's condition stack.
/// Returns 200 with empty entries if no stack exists yet.
pub async fn get_device_stack(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
) -> Json<crate::knobs::layout_store::DeviceStack> {
    let stack = state
        .layouts
        .get_stack(&device_id)
        .await
        .unwrap_or_else(|| crate::knobs::layout_store::DeviceStack {
            device_id: device_id.clone(),
            entries: vec![],
        });
    Json(stack)
}

/// PUT /api/devices/{device_id}/stack — Set (replace) a device's entire stack
pub async fn set_device_stack(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<SetStackRequest>,
) -> Json<DeviceStack> {
    state.layouts.set_stack(&device_id, body.entries).await;
    let stack = state
        .layouts
        .get_stack(&device_id)
        .await
        .unwrap_or_else(|| DeviceStack {
            device_id: device_id.clone(),
            entries: vec![],
        });
    Json(stack)
}

/// POST /api/devices/{device_id}/stack/entries — Add an entry to the stack
pub async fn add_stack_entry(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<AddStackEntryRequest>,
) -> StatusCode {
    let entry = StackEntry {
        layout_id: body.layout_id,
        conditions: body.conditions,
        enabled: body.enabled,
    };
    state.layouts.push_stack_entry(&device_id, entry).await;
    StatusCode::CREATED
}

/// DELETE /api/devices/{device_id}/stack/entries/{index} — Remove stack entry by index
pub async fn remove_stack_entry(
    State(state): State<AppState>,
    Path((device_id, index)): Path<(String, usize)>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    if state.layouts.remove_stack_entry(&device_id, index).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err((
            StatusCode::NOT_FOUND,
            error_json(format!(
                "stack entry index {} not found for device '{}'",
                index, device_id
            )),
        ))
    }
}

/// POST /api/devices/{device_id}/stack/reorder — Reorder stack
pub async fn reorder_stack(
    State(state): State<AppState>,
    Path(device_id): Path<String>,
    Json(body): Json<ReorderStackRequest>,
) -> Result<StatusCode, (StatusCode, Json<ErrorResponse>)> {
    if state
        .layouts
        .reorder_stack_entry(&device_id, body.from, body.to)
        .await
    {
        Ok(StatusCode::OK)
    } else {
        Err((
            StatusCode::BAD_REQUEST,
            error_json(format!(
                "invalid reorder indices (from: {}, to: {}) for device '{}'",
                body.from, body.to, device_id
            )),
        ))
    }
}
