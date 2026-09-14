//! HTTP transport for the shared HQPlayer output-routing command service (`api::hqp_outputs`).
//!
//! Thin axum handlers only: every read and write goes through `read_hqp_outputs`,
//! `read_hqp_output_operation` and `submit_hqp_output_command` in `api::hqp_outputs`, and every
//! error status/code/message comes from `HqpOutputServiceError`. Nothing here re-derives the
//! fencing, correlation or targeting rules those own. A malformed request (missing `zone_id`, a
//! body that doesn't deserialize into `HqpOutputCommandRequest`) never falls through to Axum's
//! default plain-text rejection — every handler extracts with `Result<_, _>` and maps the
//! rejection itself into the same structured `{"error", "error_code"}` shape a real service
//! refusal returns, so a client only ever parses one error shape from this surface.

use axum::extract::rejection::{JsonRejection, QueryRejection};
use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Deserialize;
use serde_json::json;

use super::hqp_outputs::{
    read_hqp_output_operation, read_hqp_outputs, submit_hqp_output_command, HqpOutputServiceError,
};
use super::AppState;
use crate::adapters::hqplayer::outputs::HqpOutputCommandRequest;

/// `{"error": ..., "error_code": ..., "projection": {...}?}` per the contract, at the status
/// `HqpOutputServiceError::status()` already defines.
fn error_response(error: HqpOutputServiceError, projection: Option<serde_json::Value>) -> Response {
    let status = StatusCode::from_u16(error.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
    let mut body = json!({
        "error": error.message(),
        "error_code": error.code(),
    });
    if let Some(projection) = projection {
        body["projection"] = projection;
    }
    (status, Json(body)).into_response()
}

/// A request that never reached a real service call — the query string or body didn't parse —
/// mapped to the same two contract-documented 400 codes a real refusal would use, never Axum's
/// bare-text default rejection body.
fn malformed_request_response(code: &'static str, error: String) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error": error, "error_code": code})),
    )
        .into_response()
}

#[derive(Debug, Deserialize)]
pub struct HqpOutputsQuery {
    pub zone_id: String,
}

/// `GET /hqplayer/outputs?zone_id=hqplayer:<instance>`
pub async fn hqp_outputs_handler(
    State(state): State<AppState>,
    query: Result<Query<HqpOutputsQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return malformed_request_response(
                "INVALID_ZONE_ID",
                format!("zone_id query parameter is required: {rejection}"),
            );
        }
    };
    match read_hqp_outputs(&state, &query.zone_id).await {
        Ok(projection) => (StatusCode::OK, Json(projection)).into_response(),
        Err(error) => error_response(error, None),
    }
}

#[derive(Debug, Deserialize)]
pub struct HqpOutputOperationQuery {
    pub zone_id: String,
    pub operation_id: String,
}

/// `GET /hqplayer/outputs/operation?zone_id=hqplayer:<instance>&operation_id=<id>`
pub async fn hqp_output_operation_handler(
    State(state): State<AppState>,
    query: Result<Query<HqpOutputOperationQuery>, QueryRejection>,
) -> Response {
    let Query(query) = match query {
        Ok(query) => query,
        Err(rejection) => {
            return malformed_request_response(
                "INVALID_COMMAND",
                format!("zone_id and operation_id query parameters are both required: {rejection}"),
            );
        }
    };
    match read_hqp_output_operation(&state, &query.zone_id, &query.operation_id).await {
        Ok(operation) => (StatusCode::OK, Json(operation)).into_response(),
        Err(error) => error_response(error, None),
    }
}

/// `POST /hqplayer/outputs/command` (controller-auth protected mutation — see
/// `api::controller_auth::is_protected`). The request body deserializes directly into
/// `HqpOutputCommandRequest`'s flattened action shape; nothing here re-maps it.
pub async fn hqp_output_command_handler(
    State(state): State<AppState>,
    request: Result<Json<HqpOutputCommandRequest>, JsonRejection>,
) -> Response {
    let Json(request) = match request {
        Ok(request) => request,
        Err(rejection) => {
            return malformed_request_response(
                "INVALID_COMMAND",
                format!("request body is not a valid HQPlayer output command: {rejection}"),
            );
        }
    };
    let zone_id = request.zone_id.clone();
    match submit_hqp_output_command(&state, request).await {
        Ok(receipt) => (StatusCode::OK, Json(receipt)).into_response(),
        Err(error) => {
            // Best-effort: attach the committed projection when the zone is at least resolvable
            // (an unknown instance has none to attach), exactly as `read_hqp_outputs` itself
            // reports it — never synthesized.
            let projection = read_hqp_outputs(&state, &zone_id)
                .await
                .ok()
                .and_then(|p| serde_json::to_value(p).ok());
            error_response(error, projection)
        }
    }
}

/// The three `/hqplayer/outputs*` routes as one reusable attachment. `main.rs`'s production router
/// and this crate's public-transport tests both call this function and merge it in before
/// `.with_state(...)`, so a test binding these routes is binding the exact same handlers
/// production does — never a duplicated stand-in.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/hqplayer/outputs", get(hqp_outputs_handler))
        .route(
            "/hqplayer/outputs/command",
            post(hqp_output_command_handler),
        )
        .route(
            "/hqplayer/outputs/operation",
            get(hqp_output_operation_handler),
        )
}
