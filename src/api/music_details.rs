//! Explicit, optional context reads; playback never invokes this adapter.
use axum::{
    extract::{
        rejection::{PathRejection, QueryRejection},
        Path, Query, State,
    },
    http::{header::CACHE_CONTROL, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use serde::Deserialize;

use super::AppState;
use crate::{
    aggregator::ZoneAggregator,
    music_context::{MusicContext, MusicContextError, MusicContextService},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MusicDetailsQuery {
    pub language: String,
}

pub async fn handler(
    State(state): State<AppState>,
    zone_id: Result<Path<String>, PathRejection>,
    query: Result<Query<MusicDetailsQuery>, QueryRejection>,
) -> Response {
    let Ok(Path(zone_id)) = zone_id else {
        return render(Err(MusicContextError::InvalidRequest));
    };
    response(
        crate::music_context::shared_service(),
        &state.aggregator,
        &crate::config::get_config_dir(),
        &zone_id,
        query,
    )
    .await
}

/// Both real HTTP requests and injectable consumer fixtures use this boundary.
pub async fn response(
    service: &MusicContextService,
    aggregator: &ZoneAggregator,
    config_dir: &std::path::Path,
    zone_id: &str,
    query: Result<Query<MusicDetailsQuery>, QueryRejection>,
) -> Response {
    let result = match query {
        Ok(Query(query)) => {
            service
                .read_context(aggregator, config_dir, zone_id, &query.language)
                .await
        }
        Err(_) => Err(MusicContextError::InvalidRequest),
    };
    render(result)
}

fn render(result: Result<MusicContext, MusicContextError>) -> Response {
    let mut response = match result {
        Ok(context) => Json(context).into_response(),
        Err(error) => {
            let status = match error {
                MusicContextError::InvalidRequest => StatusCode::BAD_REQUEST,
                MusicContextError::ZoneNotFound => StatusCode::NOT_FOUND,
                MusicContextError::NoMusic | MusicContextError::MusicChanged => {
                    StatusCode::CONFLICT
                }
                MusicContextError::CloudNotPaired | MusicContextError::Unavailable => {
                    StatusCode::SERVICE_UNAVAILABLE
                }
            };
            (
                status,
                Json(serde_json::json!({"error":error.message(), "code":error.code()})),
            )
                .into_response()
        }
    };
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
}
