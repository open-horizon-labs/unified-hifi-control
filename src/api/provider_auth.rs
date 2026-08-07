//! Provider authorization state and Spotify OAuth handlers.
//!
//! OAuth state is single-use and expires quickly; provider access tokens are
//! owned by the adapter and never returned to HTTP clients.  The Spotify token
//! is persisted in UHC's config subdirectory so a normal restart does not
//! require another login.  Deployments should protect that directory as a
//! secret-backed volume; Unix installs additionally enforce mode 0600.

use crate::adapters::spotify::{SpotifyAdapter, SpotifyToken, SpotifyTokenRefresher};
use crate::api::credentials::{EncryptedCredentialStore, SpotifyCredentialRecord};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

use super::AppState;

const OAUTH_TTL: Duration = Duration::from_secs(600);
const MAX_PENDING_OAUTH: usize = 64;
const SPOTIFY_AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const SPOTIFY_SCOPE: &str = "user-read-playback-state user-modify-playback-state user-read-private user-read-email playlist-read-private playlist-read-collaborative playlist-modify-public playlist-modify-private user-library-read user-library-modify";

#[derive(Clone)]
pub struct ProviderAuthState {
    spotify: Arc<RwLock<Option<Arc<SpotifyAdapter>>>>,
    pending: Arc<RwLock<HashMap<String, PendingOAuth>>>,
    credentials: Option<Arc<EncryptedCredentialStore>>,
    oauth: Arc<RwLock<Option<Arc<SpotifyOAuthConfig>>>>,
}

#[derive(Clone)]
struct PendingOAuth {
    provider: String,
    expires_at: u64,
    code_verifier: Option<String>,
}

#[derive(Clone)]
struct SpotifyOAuthConfig {
    client_id: String,
    client_secret: Option<String>,
    token_url: String,
    redirect_uri: String,
}

impl Default for ProviderAuthState {
    fn default() -> Self {
        let credentials = EncryptedCredentialStore::from_env().ok().map(Arc::new);
        let oauth = spotify_oauth_config(credentials.as_ref())
            .ok()
            .map(Arc::new);
        Self {
            spotify: Arc::new(RwLock::new(None)),
            pending: Arc::new(RwLock::new(HashMap::new())),
            credentials,
            oauth: Arc::new(RwLock::new(oauth)),
        }
    }
}

impl ProviderAuthState {
    /// Construct state with an explicit credential store (tests and operators
    /// that inject a QNAP secret-backed path).
    pub fn with_credential_store(store: EncryptedCredentialStore) -> Self {
        let credentials = Some(Arc::new(store));
        let oauth = spotify_oauth_config(credentials.as_ref())
            .ok()
            .map(Arc::new);
        Self {
            spotify: Arc::new(RwLock::new(None)),
            pending: Arc::new(RwLock::new(HashMap::new())),
            credentials,
            oauth: Arc::new(RwLock::new(oauth)),
        }
    }

    pub async fn attach_spotify(&self, adapter: Arc<SpotifyAdapter>) {
        if let Some(store) = &self.credentials {
            match store.load() {
                Ok(Some(token)) => adapter.set_token(token).await,
                Ok(None) => {}
                Err(error) => tracing::warn!(
                    path = %store.path().display(),
                    "Ignoring unreadable Spotify credentials: {}",
                    error
                ),
            }
        }
        if let Some(config) = self.oauth.read().await.clone() {
            let refresher = SpotifyOAuthRefresher {
                config: config.clone(),
                credentials: self.credentials.clone(),
            };
            adapter.set_token_refresher(Arc::new(refresher)).await;
        }
        *self.spotify.write().await = Some(adapter);
    }

    async fn spotify(&self) -> Option<Arc<SpotifyAdapter>> {
        self.spotify.read().await.clone()
    }

    /// Whether a valid Spotify client configuration is available. This only
    /// reports configuration presence; it never exposes client credentials.
    pub async fn spotify_configured(&self) -> bool {
        self.oauth.read().await.is_some()
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
    pub code: Option<String>,
    pub state: String,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub error_description: Option<String>,
    /// `json` keeps a machine-client callback response; browser callbacks
    /// redirect to the settings page by default.
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ProviderAuthResponse {
    pub provider: String,
    pub authorized: bool,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Deserialize)]
pub struct SpotifyConfigureRequest {
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    #[serde(default)]
    pub redirect_uri: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct SpotifyConfigureResponse {
    pub provider: String,
    pub configured: bool,
    pub client_id: Option<String>,
    pub redirect_uri: Option<String>,
    pub has_client_secret: bool,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    error: String,
    code: &'static str,
}

/// Persist Spotify client configuration without returning secrets or tokens.
pub async fn configure_spotify(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(request): Json<SpotifyConfigureRequest>,
) -> Result<Json<SpotifyConfigureResponse>, (StatusCode, Json<ErrorBody>)> {
    if provider != "spotify" {
        return Err(error(
            StatusCode::NOT_IMPLEMENTED,
            "only Spotify client configuration is supported here",
            "provider_not_supported",
        ));
    }
    let client_id = request.client_id.trim().to_string();
    if !valid_spotify_client_id(&client_id) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Enter a valid Spotify client ID before saving setup",
            "invalid_client_configuration",
        ));
    }
    let redirect_uri = request
        .redirect_uri
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| {
            "http://127.0.0.1:8088/api/providers/spotify/oauth/callback".to_string()
        });
    if !valid_redirect_uri(&redirect_uri) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "redirect_uri must use HTTPS or an explicit 127.0.0.1/::1 loopback HTTP URL",
            "invalid_redirect_uri",
        ));
    }
    let store = state.provider_auth.credentials.clone().ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Spotify credential storage is unavailable; configure UHC_CREDENTIAL_KEY or a writable key file",
            "credential_storage_unavailable",
        )
    })?;
    let existing = store.load_record().map_err(|e| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            &format!("Spotify credentials could not be loaded: {e}"),
            "credential_storage_failed",
        )
    })?;
    // The browser deliberately never receives the existing secret.  Treat an
    // omitted/blank secret as "keep the current one" so editing the client ID
    // or redirect URI cannot silently downgrade a configured confidential
    // client to PKCE.  Replacing the secret still works by submitting a new
    // non-empty value.
    let client_secret = select_client_secret(
        request.client_secret,
        existing
            .as_ref()
            .and_then(|record| record.client_secret.clone()),
    );
    store
        .save_record(&SpotifyCredentialRecord {
            token: existing.as_ref().and_then(|record| record.token.clone()),
            client_id: client_id.clone(),
            client_secret: client_secret.clone(),
            redirect_uri: redirect_uri.clone(),
        })
        .map_err(|e| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Spotify credentials could not be saved: {e}"),
                "credential_storage_failed",
            )
        })?;
    let config = Arc::new(SpotifyOAuthConfig {
        client_id: client_id.clone(),
        client_secret: client_secret.clone(),
        token_url: std::env::var("SPOTIFY_TOKEN_URL")
            .unwrap_or_else(|_| SPOTIFY_TOKEN_URL.to_string()),
        redirect_uri: redirect_uri.clone(),
    });
    *state.provider_auth.oauth.write().await = Some(config.clone());
    if let Some(adapter) = state.provider_auth.spotify().await {
        adapter
            .set_token_refresher(Arc::new(SpotifyOAuthRefresher {
                config,
                credentials: Some(store),
            }))
            .await;
    }
    Ok(Json(SpotifyConfigureResponse {
        provider,
        configured: true,
        client_id: Some(client_id),
        redirect_uri: Some(redirect_uri),
        has_client_secret: client_secret.is_some(),
    }))
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
    let config = state
        .provider_auth
        .oauth
        .read()
        .await
        .clone()
        .ok_or_else(|| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Spotify client configuration is not configured",
                "oauth_not_configured",
            )
        })?;
    if !valid_spotify_client_id(&config.client_id) {
        return Err(error(
            StatusCode::BAD_REQUEST,
            "Spotify client setup is incomplete; enter and save the client ID first",
            "invalid_client_configuration",
        ));
    }
    let code_verifier = config.client_secret.is_none().then(generate_pkce_verifier);
    let code_challenge = code_verifier.as_deref().map(pkce_challenge);
    let state_token = random_token(32);
    let expires_at = now_secs() + OAUTH_TTL.as_secs();
    let mut pending = state.provider_auth.pending.write().await;
    let now = now_secs();
    pending.retain(|_, item| item.expires_at > now);
    if pending.len() >= MAX_PENDING_OAUTH {
        return Err(error(
            StatusCode::TOO_MANY_REQUESTS,
            "Too many pending Spotify authorizations; finish or wait for one to expire",
            "oauth_capacity_exceeded",
        ));
    }
    pending.insert(
        state_token.clone(),
        PendingOAuth {
            provider: provider.clone(),
            expires_at,
            code_verifier,
        },
    );
    let authorization_url = format!(
        "{SPOTIFY_AUTHORIZE_URL}?response_type=code&client_id={}&redirect_uri={}&scope={}&state={}",
        urlencoding::encode(&config.client_id),
        urlencoding::encode(&config.redirect_uri),
        urlencoding::encode(SPOTIFY_SCOPE),
        urlencoding::encode(&state_token)
    );
    let authorization_url = if let Some(challenge) = code_challenge {
        format!(
            "{authorization_url}&code_challenge_method=S256&code_challenge={}",
            urlencoding::encode(&challenge)
        )
    } else {
        authorization_url
    };
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
) -> Response {
    let machine = query.format.as_deref() == Some("json");
    match oauth_callback_json(State(state), Path(provider), Query(query)).await {
        Ok(Json(response)) if machine => Json(response).into_response(),
        Ok(Json(_)) => Redirect::to("/settings?spotify=connected").into_response(),
        Err((status, Json(error))) if machine => (status, Json(error)).into_response(),
        Err((_status, Json(error))) => Redirect::to(&format!(
            "/settings?spotify=error&reason={}",
            urlencoding::encode(error.code)
        ))
        .into_response(),
    }
}

async fn oauth_callback_json(
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
    if let Some(provider_error) = query.error {
        let description = query
            .error_description
            .unwrap_or_else(|| "Spotify authorization was denied".to_string());
        return Err(error(
            StatusCode::BAD_REQUEST,
            &description,
            if provider_error == "access_denied" {
                "provider_denied"
            } else {
                "provider_oauth_error"
            },
        ));
    }
    let code = query.code.ok_or_else(|| {
        error(
            StatusCode::BAD_REQUEST,
            "Spotify callback did not include an authorization code",
            "missing_authorization_code",
        )
    })?;
    let config = state
        .provider_auth
        .oauth
        .read()
        .await
        .clone()
        .ok_or_else(|| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                "Spotify client configuration is not configured",
                "oauth_not_configured",
            )
        })?;
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        ("redirect_uri", config.redirect_uri.clone()),
    ];
    if let Some(verifier) = pending.code_verifier {
        form.push(("client_id", config.client_id.clone()));
        form.push(("code_verifier", verifier));
    }
    let mut request = reqwest::Client::new().post(&config.token_url).form(&form);
    if let Some(secret) = &config.client_secret {
        request = request.basic_auth(&config.client_id, Some(secret));
    }
    let response = request.send().await.map_err(|e| {
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
    let spotify_token = SpotifyToken {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at,
    };
    let adapter = state.provider_auth.spotify().await.ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Spotify adapter is not registered",
            "adapter_unavailable",
        )
    })?;
    if let Some(store) = &state.provider_auth.credentials {
        let record = SpotifyCredentialRecord {
            token: Some(spotify_token.clone()),
            client_id: config.client_id.clone(),
            client_secret: config.client_secret.clone(),
            redirect_uri: config.redirect_uri.clone(),
        };
        store.save_record(&record).map_err(|e| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Spotify authorization succeeded but token storage failed: {e}"),
                "token_storage_failed",
            )
        })?;
    }
    adapter.set_token(spotify_token).await;
    if state.coordinator.is_enabled("spotify").await {
        state.adapter_registry.start("spotify").await.map_err(|e| {
            error(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("Spotify adapter failed to start: {e}"),
                "adapter_start_failed",
            )
        })?;
    } else {
        tracing::info!("Spotify authorization completed while adapter is disabled");
    }
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
    if let Some(store) = &state.provider_auth.credentials {
        store.clear().map_err(|e| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Spotify credentials could not be removed from storage: {e}"),
                "token_storage_failed",
            )
        })?;
    }
    adapter.clear_token().await;
    state.adapter_registry.stop("spotify").await;
    Ok(Json(ProviderAuthResponse {
        provider,
        authorized: false,
        expires_at: None,
    }))
}

fn spotify_oauth_config(
    credentials: Option<&Arc<EncryptedCredentialStore>>,
) -> anyhow::Result<SpotifyOAuthConfig> {
    let record = credentials.and_then(|store| store.load_record().ok().flatten());
    let client_id = std::env::var("SPOTIFY_CLIENT_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            record
                .as_ref()
                .map(|record| record.client_id.clone())
                .filter(|value| !value.is_empty())
        })
        .ok_or_else(|| anyhow::anyhow!("SPOTIFY_CLIENT_ID is not configured"))?;
    if !valid_spotify_client_id(&client_id) {
        return Err(anyhow::anyhow!(
            "Spotify client ID is invalid or still a placeholder"
        ));
    }
    let client_secret = std::env::var("SPOTIFY_CLIENT_SECRET")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            record
                .as_ref()
                .and_then(|record| record.client_secret.clone())
        });
    let redirect_uri = std::env::var("SPOTIFY_REDIRECT_URI")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            record
                .as_ref()
                .map(|record| record.redirect_uri.clone())
                .filter(|value| !value.is_empty())
        })
        .unwrap_or_else(|| {
            "http://127.0.0.1:8088/api/providers/spotify/oauth/callback".to_string()
        });
    if !valid_redirect_uri(&redirect_uri) {
        return Err(anyhow::anyhow!(
            "Spotify redirect URI is not HTTPS or loopback HTTP"
        ));
    }
    let token_url =
        std::env::var("SPOTIFY_TOKEN_URL").unwrap_or_else(|_| SPOTIFY_TOKEN_URL.to_string());
    Ok(SpotifyOAuthConfig {
        client_id,
        client_secret,
        token_url,
        redirect_uri,
    })
}

fn valid_redirect_uri(value: &str) -> bool {
    let Ok(url) = url::Url::parse(value) else {
        return false;
    };
    if url.scheme() == "https" {
        return url
            .host_str()
            .is_some_and(|host| host != "example.test" && !host.ends_with(".test"));
    }
    if url.scheme() != "http" {
        return false;
    }
    matches!(
        url.host_str(),
        Some("127.0.0.1") | Some("[::1]") | Some("::1")
    )
}

fn valid_spotify_client_id(value: &str) -> bool {
    let normalized = value.trim().to_ascii_lowercase();
    !normalized.is_empty()
        && !matches!(
            normalized.as_str(),
            "test" | "local-test" | "example" | "example-client" | "your-client-id"
        )
        && !normalized.contains("example.test")
}

fn select_client_secret(requested: Option<String>, existing: Option<String>) -> Option<String> {
    requested
        .filter(|value| !value.trim().is_empty())
        .or(existing)
}

struct SpotifyOAuthRefresher {
    config: Arc<SpotifyOAuthConfig>,
    credentials: Option<Arc<EncryptedCredentialStore>>,
}

#[async_trait::async_trait]
impl SpotifyTokenRefresher for SpotifyOAuthRefresher {
    async fn refresh(&self, current: &SpotifyToken) -> anyhow::Result<SpotifyToken> {
        let refresh_token = current
            .refresh_token
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("Spotify refresh token is unavailable"))?;
        let mut form = vec![
            ("grant_type", "refresh_token".to_string()),
            ("refresh_token", refresh_token.to_string()),
        ];
        let mut request = reqwest::Client::new()
            .post(&self.config.token_url)
            .form(&form);
        if let Some(secret) = &self.config.client_secret {
            request = request.basic_auth(&self.config.client_id, Some(secret));
        } else {
            form.push(("client_id", self.config.client_id.clone()));
            request = reqwest::Client::new()
                .post(&self.config.token_url)
                .form(&form);
        }
        let response = request.send().await?;
        if !response.status().is_success() {
            if response.status() == StatusCode::BAD_REQUEST {
                if let Ok(body) = response.json::<serde_json::Value>().await {
                    if body["error"].as_str() == Some("invalid_grant") {
                        if let Some(store) = &self.credentials {
                            store.clear_token()?;
                        }
                        return Err(anyhow::anyhow!(
                            "Spotify refresh token was rejected; authorize Spotify again"
                        ));
                    }
                }
            }
            return Err(anyhow::anyhow!("Spotify token refresh failed"));
        }
        let response: SpotifyTokenResponse = response.json().await?;
        let refreshed = SpotifyToken {
            access_token: response.access_token,
            refresh_token: response
                .refresh_token
                .or_else(|| current.refresh_token.clone()),
            expires_at: response.expires_in.map(|seconds| now_secs() + seconds),
        };
        if let Some(store) = &self.credentials {
            let mut record = store.load_record()?.unwrap_or(SpotifyCredentialRecord {
                token: None,
                client_id: self.config.client_id.clone(),
                client_secret: self.config.client_secret.clone(),
                redirect_uri: self.config.redirect_uri.clone(),
            });
            record.token = Some(refreshed.clone());
            store.save_record(&record)?;
        }
        Ok(refreshed)
    }
}

fn generate_pkce_verifier() -> String {
    let mut bytes = [0_u8; 48];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn pkce_challenge(verifier: &str) -> String {
    let digest = Sha256::digest(verifier.as_bytes());
    URL_SAFE_NO_PAD.encode(digest)
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

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        body::{to_bytes, Body},
        extract::State,
        http::{Request, StatusCode},
        response::{IntoResponse, Response},
        routing::any,
        Router,
    };
    use std::sync::{Arc, Mutex};
    use tempfile::tempdir;
    use tokio::net::TcpListener;

    #[derive(Clone)]
    struct RefreshMock {
        invalid_grant: bool,
        requests: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    async fn refresh_handler(State(mock): State<RefreshMock>, request: Request<Body>) -> Response {
        let body = to_bytes(request.into_body(), 16 * 1024)
            .await
            .unwrap_or_default();
        if let Ok(mut requests) = mock.requests.lock() {
            requests.push(body.to_vec());
        }
        if mock.invalid_grant {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({"error": "invalid_grant"})),
            )
                .into_response();
        }
        (
            StatusCode::OK,
            Json(serde_json::json!({
                "access_token": "rotated-access",
                "refresh_token": "rotated-refresh",
                "expires_in": 3600
            })),
        )
            .into_response()
    }

    async fn refresh_server(mock: RefreshMock) -> (String, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = Router::new()
            .fallback(any(refresh_handler))
            .with_state(mock);
        let handle = tokio::spawn(async move {
            axum::serve(listener, router).await.unwrap();
        });
        (format!("http://{address}/token"), handle)
    }

    #[tokio::test]
    async fn refresh_rotates_token_and_persists_new_refresh_token() {
        let directory = tempdir().unwrap();
        let store = Arc::new(EncryptedCredentialStore::new(
            directory.path().join("spotify.enc"),
            [3_u8; 32],
        ));
        store
            .save_record(&SpotifyCredentialRecord {
                token: Some(SpotifyToken {
                    access_token: "old-access".to_string(),
                    refresh_token: Some("old-refresh".to_string()),
                    expires_at: Some(1),
                }),
                client_id: "client".to_string(),
                client_secret: None,
                redirect_uri: "http://127.0.0.1/callback".to_string(),
            })
            .unwrap();
        let mock = RefreshMock {
            invalid_grant: false,
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let (token_url, server) = refresh_server(mock.clone()).await;
        let refresher = SpotifyOAuthRefresher {
            config: Arc::new(SpotifyOAuthConfig {
                client_id: "client".to_string(),
                client_secret: None,
                token_url,
                redirect_uri: "http://127.0.0.1/callback".to_string(),
            }),
            credentials: Some(store.clone()),
        };
        let refreshed = refresher
            .refresh(&SpotifyToken {
                access_token: "old-access".to_string(),
                refresh_token: Some("old-refresh".to_string()),
                expires_at: Some(1),
            })
            .await
            .unwrap();
        assert_eq!(refreshed.access_token, "rotated-access");
        assert_eq!(refreshed.refresh_token.as_deref(), Some("rotated-refresh"));
        assert_eq!(
            store.load().unwrap().unwrap().access_token,
            "rotated-access"
        );
        assert!(String::from_utf8(mock.requests.lock().unwrap()[0].clone())
            .unwrap()
            .contains("refresh_token=old-refresh"));
        server.abort();
    }

    #[tokio::test]
    async fn invalid_grant_clears_token_preserving_configuration_without_echoing_token() {
        let directory = tempdir().unwrap();
        let store = Arc::new(EncryptedCredentialStore::new(
            directory.path().join("spotify.enc"),
            [4_u8; 32],
        ));
        store
            .save_record(&SpotifyCredentialRecord {
                token: Some(SpotifyToken {
                    access_token: "private-access".to_string(),
                    refresh_token: Some("private-refresh".to_string()),
                    expires_at: Some(1),
                }),
                client_id: "client-id-to-preserve".to_string(),
                client_secret: Some("client-secret-to-preserve".to_string()),
                redirect_uri: "https://uhc.example/callback-to-preserve".to_string(),
            })
            .unwrap();
        let mock = RefreshMock {
            invalid_grant: true,
            requests: Arc::new(Mutex::new(Vec::new())),
        };
        let (token_url, server) = refresh_server(mock).await;
        let refresher = SpotifyOAuthRefresher {
            config: Arc::new(SpotifyOAuthConfig {
                client_id: "client".to_string(),
                client_secret: Some("secret".to_string()),
                token_url,
                redirect_uri: "https://uhc.example/callback".to_string(),
            }),
            credentials: Some(store.clone()),
        };
        let error = refresher
            .refresh(&SpotifyToken {
                access_token: "private-access".to_string(),
                refresh_token: Some("private-refresh".to_string()),
                expires_at: Some(1),
            })
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("private-access"));
        assert!(!error.to_string().contains("private-refresh"));
        let record = store.load_record().unwrap().unwrap();
        assert_eq!(record.token, None);
        assert_eq!(record.client_id, "client-id-to-preserve");
        assert_eq!(
            record.client_secret.as_deref(),
            Some("client-secret-to-preserve")
        );
        assert_eq!(
            record.redirect_uri,
            "https://uhc.example/callback-to-preserve"
        );
        server.abort();
    }

    #[test]
    fn redirect_validation_rejects_unsafe_http_and_localhost() {
        assert!(valid_redirect_uri("https://uhc.example/callback"));
        assert!(valid_redirect_uri("http://127.0.0.1:8088/callback"));
        assert!(valid_redirect_uri("http://[::1]:8088/callback"));
        assert!(!valid_redirect_uri("https://example.test/callback"));
        assert!(!valid_redirect_uri("http://localhost:8088/callback"));
        assert!(!valid_redirect_uri("http://192.168.1.9:8088/callback"));
    }

    #[test]
    fn client_id_validation_rejects_placeholders() {
        assert!(valid_spotify_client_id("32characterSpotifyClientId"));
        assert!(!valid_spotify_client_id(""));
        assert!(!valid_spotify_client_id("local-test"));
        assert!(!valid_spotify_client_id("your-client-id"));
    }

    #[test]
    fn blank_client_secret_preserves_existing_secret() {
        assert_eq!(
            select_client_secret(Some("   ".to_string()), Some("stored-secret".to_string())),
            Some("stored-secret".to_string())
        );
        assert_eq!(
            select_client_secret(None, Some("stored-secret".to_string())),
            Some("stored-secret".to_string())
        );
        assert_eq!(
            select_client_secret(
                Some("replacement".to_string()),
                Some("stored-secret".to_string())
            ),
            Some("replacement".to_string())
        );
    }
}
