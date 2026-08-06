//! Provider authorization state and Spotify OAuth handlers.
//!
//! Credentials are deliberately kept in memory in this first contract slice.
//! The OAuth state is single-use and expires quickly; provider access tokens are
//! owned by the adapter and never returned to HTTP clients.

use crate::adapters::spotify::{SpotifyAdapter, SpotifyToken};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    Json,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

use super::AppState;

const OAUTH_TTL: Duration = Duration::from_secs(600);
const SPOTIFY_AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SPOTIFY_SCOPE: &str = "user-read-playback-state user-modify-playback-state";

#[derive(Clone, Default)]
pub struct ProviderAuthState {
    spotify: Arc<RwLock<Option<Arc<SpotifyAdapter>>>>,
    pending: Arc<RwLock<HashMap<String, PendingOAuth>>>,
}

#[derive(Clone)]
struct PendingOAuth {
    provider: String,
    expires_at: u64,
}

impl ProviderAuthState {
    pub async fn attach_spotify(&self, adapter: Arc<SpotifyAdapter>) {
        *self.spotify.write().await = Some(adapter);
    }

    async fn spotify(&self) -> Option<Arc<SpotifyAdapter>> {
        self.spotify.read().await.clone()
    }
}

#[derive(Debug, Serialize)]
pub struct OAuthStartResponse {
    pub provider: String,
    pub authorization_url: String,
    pub state: String,
    pub expires_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct OAuthCallbackQuery {
    pub code: String,
    pub state: String,
}

#[derive(Debug, Serialize)]
pub struct ProviderAuthResponse {
    pub provider: String,
    pub authorized: bool,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    error: String,
    code: &'static str,
}

pub async fn oauth_start(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<OAuthStartResponse>, (StatusCode, Json<ErrorBody>)> {
    if provider != "spotify" {
        return Err(error(
            StatusCode::NOT_IMPLEMENTED,
            "native MusicKit authorization is performed by the paired Apple companion",
            "companion_required",
        ));
    }
    let client_id = std::env::var("SPOTIFY_CLIENT_ID").map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "SPOTIFY_CLIENT_ID is not configured",
            "oauth_not_configured",
        )
    })?;
    let redirect_uri = std::env::var("SPOTIFY_REDIRECT_URI").unwrap_or_else(|_| {
        "http://localhost:8088/api/providers/spotify/oauth/callback".to_string()
    });
    let state_token = random_token(32);
    let expires_at = now_secs() + OAUTH_TTL.as_secs();
    state.provider_auth.pending.write().await.insert(
        state_token.clone(),
        PendingOAuth {
            provider: provider.clone(),
            expires_at,
        },
    );
    let authorization_url = format!(
        "{SPOTIFY_AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        urlencoding::encode(&client_id),
        urlencoding::encode(&redirect_uri),
        urlencoding::encode(SPOTIFY_SCOPE),
        urlencoding::encode(&state_token)
    );
    Ok(Json(OAuthStartResponse {
        provider,
        authorization_url,
        state: state_token,
        expires_at,
    }))
}

pub async fn oauth_callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Result<Json<ProviderAuthResponse>, (StatusCode, Json<ErrorBody>)> {
    if provider != "spotify" {
        return Err(error(
            StatusCode::NOT_IMPLEMENTED,
            "native MusicKit authorization is performed by the paired Apple companion",
            "companion_required",
        ));
    }
    let pending = state
        .provider_auth
        .pending
        .write()
        .await
        .remove(&query.state)
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "OAuth state is unknown or already used",
                "invalid_state",
            )
        })?;
    if pending.provider != provider || pending.expires_at <= now_secs() {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "OAuth state is expired",
            "expired_state",
        ));
    }
    let client_id = std::env::var("SPOTIFY_CLIENT_ID").map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "SPOTIFY_CLIENT_ID is not configured",
            "oauth_not_configured",
        )
    })?;
    let client_secret = std::env::var("SPOTIFY_CLIENT_SECRET").map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "SPOTIFY_CLIENT_SECRET is not configured",
            "oauth_not_configured",
        )
    })?;
    let redirect_uri = std::env::var("SPOTIFY_REDIRECT_URI").unwrap_or_else(|_| {
        "http://localhost:8088/api/providers/spotify/oauth/callback".to_string()
    });
    let token_url =
        std::env::var("SPOTIFY_TOKEN_URL").unwrap_or_else(|_| SPOTIFY_TOKEN_URL.to_string());
    let response = reqwest::Client::new()
        .post(token_url)
        .basic_auth(client_id, Some(client_secret))
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", query.code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
        ])
        .send()
        .await
        .map_err(|e| {
            error(
                StatusCode::BAD_GATEWAY,
                &format!("Spotify token exchange failed: {e}"),
                "token_exchange_failed",
            )
        })?;
    if !response.status().is_success() {
        return Err(error(
            StatusCode::BAD_GATEWAY,
            "Spotify rejected the authorization code",
            "token_exchange_failed",
        ));
    }
    let token: SpotifyTokenResponse = response.json().await.map_err(|e| {
        error(
            StatusCode::BAD_GATEWAY,
            &format!("Spotify token response was invalid: {e}"),
            "token_exchange_failed",
        )
    })?;
    let expires_at = token.expires_in.map(|seconds| now_secs() + seconds);
    let adapter = state.provider_auth.spotify().await.ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Spotify adapter is not registered",
            "adapter_unavailable",
        )
    })?;
    adapter
        .set_token(SpotifyToken {
            access_token: token.access_token,
            refresh_token: token.refresh_token,
            expires_at,
        })
        .await;
    state.adapter_registry.start("spotify").await.map_err(|e| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            &format!("Spotify adapter failed to start: {e}"),
            "adapter_start_failed",
        )
    })?;
    Ok(Json(ProviderAuthResponse {
        provider,
        authorized: true,
        expires_at,
    }))
}

pub async fn oauth_revoke(
    State(state): State<AppState>,
    Path(provider): Path<String>,
) -> Result<Json<ProviderAuthResponse>, (StatusCode, Json<ErrorBody>)> {
    if provider != "spotify" {
        return Err(error(
            StatusCode::NOT_IMPLEMENTED,
            "native MusicKit authorization is performed by the paired Apple companion",
            "companion_required",
        ));
    }
    let adapter = state.provider_auth.spotify().await.ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Spotify adapter is not registered",
            "adapter_unavailable",
        )
    })?;
    adapter.clear_token().await;
    state.adapter_registry.stop("spotify").await;
    Ok(Json(ProviderAuthResponse {
        provider,
        authorized: false,
        expires_at: None,
    }))
}

#[derive(Debug, Deserialize)]
struct SpotifyTokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

fn random_token(length: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(length)
        .map(char::from)
        .collect()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn error(status: StatusCode, message: &str, code: &'static str) -> (StatusCode, Json<ErrorBody>) {
    (
        status,
        Json(ErrorBody {
            error: message.to_string(),
            code,
        }),
    )
}
