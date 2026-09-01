//! Provider authorization state and Spotify OAuth handlers.
//!
//! OAuth state is single-use and expires quickly; provider access tokens are
//! owned by the adapter and never returned to HTTP clients.  The Spotify token
//! is persisted in UHC's config subdirectory so a normal restart does not
//! require another login.  Deployments should protect that directory as a
//! secret-backed volume; Unix installs additionally enforce mode 0600.

use crate::adapters::musicassistant::{
    MusicAssistantAdapter, MusicAssistantConfig, ReconfigurableMusicAssistant,
};
use crate::adapters::spotify::{SpotifyAdapter, SpotifyToken, SpotifyTokenRefresher};
use crate::api::credentials::{
    EncryptedCredentialStore, MusicAssistantCredentialRecord, MusicAssistantCredentialStore,
    SpotifyCredentialRecord,
};
use crate::api::spotify_tunnel::{SpotifyTunnelManager, TunnelStatusResponse};
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Redirect, Response},
    Json,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::{distributions::Alphanumeric, Rng, RngCore};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::AppState;

const OAUTH_TTL: Duration = Duration::from_secs(600);
const MAX_PENDING_OAUTH: usize = 64;
const SPOTIFY_TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
const HIPHI_SPOTIFY_CONNECT_URL: &str = "https://app.hiphi.audio/spotify/connect";
const HIPHI_SPOTIFY_CALLBACK_URI: &str = "https://app.hiphi.audio/api/spotify/callback";

#[derive(Clone)]
pub struct ProviderAuthState {
    spotify: Arc<RwLock<Option<Arc<SpotifyAdapter>>>>,
    pending: Arc<RwLock<HashMap<String, PendingOAuth>>>,
    credentials: Option<Arc<EncryptedCredentialStore>>,
    oauth: Arc<RwLock<Option<Arc<SpotifyOAuthConfig>>>>,
    musicassistant: Arc<RwLock<Option<Arc<ReconfigurableMusicAssistant>>>>,
    musicassistant_credentials: Option<Arc<MusicAssistantCredentialStore>>,
    /// Temporary HTTPS tunnel for the Spotify OAuth callback (#538).
    pub spotify_tunnel: Arc<SpotifyTunnelManager>,
    /// The separate, loopback-only listener published by the built-in SSH
    /// tunnel.  This is intentionally never the main UHC listener's port.
    callback_port: Arc<std::sync::OnceLock<u16>>,
}

#[derive(Clone)]
struct PendingOAuth {
    provider: String,
    expires_at: u64,
    code_verifier: Option<String>,
    cloud_request_id: Option<uuid::Uuid>,
    client_id: Option<String>,
    state_digest: Option<String>,
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
        let musicassistant_credentials =
            MusicAssistantCredentialStore::from_env().ok().map(Arc::new);
        Self {
            spotify: Arc::new(RwLock::new(None)),
            pending: Arc::new(RwLock::new(HashMap::new())),
            credentials,
            oauth: Arc::new(RwLock::new(oauth)),
            musicassistant: Arc::new(RwLock::new(None)),
            musicassistant_credentials,
            spotify_tunnel: Arc::new(SpotifyTunnelManager::default()),
            callback_port: Arc::new(std::sync::OnceLock::new()),
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
            musicassistant: Arc::new(RwLock::new(None)),
            musicassistant_credentials: MusicAssistantCredentialStore::from_env()
                .ok()
                .map(Arc::new),
            spotify_tunnel: Arc::new(SpotifyTunnelManager::default()),
            callback_port: Arc::new(std::sync::OnceLock::new()),
        }
    }

    /// Wires the server's graceful-shutdown token into the Spotify tunnel
    /// manager so a leftover tunnel process is killed on shutdown too, not
    /// just on the user-facing stop paths. Called once from `AppState::new`.
    pub fn bind_shutdown(&self, token: CancellationToken) {
        self.spotify_tunnel.bind_shutdown(token);
    }

    /// Records the dedicated callback listener's ephemeral loopback port at
    /// boot.  There is deliberately no Host, environment, or main-listener
    /// fallback: losing this binding must fail closed rather than publishing
    /// UHC's complete LAN router.
    pub fn bind_callback_port(&self, port: u16) {
        let _ = self.callback_port.set(port);
    }

    /// The only port the built-in tunnel may forward to.
    pub fn callback_port(&self) -> Option<u16> {
        self.callback_port.get().copied()
    }

    /// Construct provider auth with an explicit Music Assistant credential
    /// store. This keeps outbound-provider integration tests isolated from an
    /// operator's real encrypted configuration.
    pub fn with_musicassistant_credential_store(store: MusicAssistantCredentialStore) -> Self {
        Self {
            musicassistant_credentials: Some(Arc::new(store)),
            ..Self::default()
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

    pub async fn attach_musicassistant(&self, adapter: Arc<ReconfigurableMusicAssistant>) {
        *self.musicassistant.write().await = Some(adapter);
    }

    async fn spotify(&self) -> Option<Arc<SpotifyAdapter>> {
        self.spotify.read().await.clone()
    }

    /// Whether a valid Spotify client configuration is available. This only
    /// reports configuration presence; it never exposes client credentials.
    pub async fn spotify_configured(&self) -> bool {
        self.oauth.read().await.is_some()
    }

    async fn musicassistant(&self) -> Option<Arc<ReconfigurableMusicAssistant>> {
        self.musicassistant.read().await.clone()
    }

    /// Load the encrypted boot configuration without exposing its bearer token
    /// to a caller outside the server process.
    pub fn musicassistant_bootstrap_config(&self) -> anyhow::Result<Option<MusicAssistantConfig>> {
        match self.musicassistant_credentials.as_ref() {
            Some(store) => store.load().map(|record| {
                record.map(|record| MusicAssistantConfig {
                    host: record.host,
                    port: record.port,
                    token: record.token,
                    tls: record.tls,
                    allow_insecure_http: record.allow_insecure_http,
                })
            }),
            None => Ok(None),
        }
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

#[derive(Debug, Deserialize)]
pub struct MusicAssistantConfigureRequest {
    pub host: String,
    #[serde(default = "default_musicassistant_port")]
    pub port: u16,
    /// Omit or submit blank to retain the stored bearer token.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default = "default_musicassistant_tls")]
    pub tls: bool,
    #[serde(default)]
    pub allow_insecure_http: bool,
}

fn default_musicassistant_port() -> u16 {
    8095
}
fn default_musicassistant_tls() -> bool {
    true
}

#[derive(Debug, Serialize)]
pub struct MusicAssistantEndpoint {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    pub allow_insecure_http: bool,
}

#[derive(Debug, Serialize)]
pub struct MusicAssistantConfigureResponse {
    pub provider: String,
    pub configured: bool,
    pub endpoint: MusicAssistantEndpoint,
    pub has_token: bool,
}

#[derive(Debug, Serialize)]
pub struct MusicAssistantStatusResponse {
    pub provider: String,
    pub configured: bool,
    pub enabled: bool,
    pub running: bool,
    pub endpoint: Option<MusicAssistantEndpoint>,
    pub has_token: bool,
    pub error: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ErrorBody {
    error: String,
    code: &'static str,
}

/// Persist provider configuration without returning secrets or tokens. The
/// route is shared for compatibility; each provider retains its own request
/// contract after path dispatch.
pub async fn configure_provider(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Json(request): Json<Value>,
) -> Result<Response, (StatusCode, Json<ErrorBody>)> {
    match provider.as_str() {
        "spotify" => {
            let request = serde_json::from_value(request).map_err(|_| {
                error(
                    StatusCode::BAD_REQUEST,
                    "Spotify client configuration is invalid",
                    "invalid_client_configuration",
                )
            })?;
            configure_spotify_request(state, provider, request)
                .await
                .map(|value| value.into_response())
        }
        "musicassistant" => {
            let request = serde_json::from_value(request).map_err(|_| {
                error(
                    StatusCode::BAD_REQUEST,
                    "Music Assistant connection configuration is invalid",
                    "invalid_connection_configuration",
                )
            })?;
            configure_musicassistant_request(state, request)
                .await
                .map(|value| value.into_response())
        }
        _ => Err(error(
            StatusCode::NOT_IMPLEMENTED,
            "this provider does not support browser configuration",
            "provider_not_supported",
        )),
    }
}

async fn configure_spotify_request(
    state: AppState,
    provider: String,
    request: SpotifyConfigureRequest,
) -> Result<Json<SpotifyConfigureResponse>, (StatusCode, Json<ErrorBody>)> {
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
        .unwrap_or_else(|| HIPHI_SPOTIFY_CALLBACK_URI.to_string());
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

async fn configure_musicassistant_request(
    state: AppState,
    request: MusicAssistantConfigureRequest,
) -> Result<Json<MusicAssistantConfigureResponse>, (StatusCode, Json<ErrorBody>)> {
    let store = state.provider_auth.musicassistant_credentials.clone().ok_or_else(|| error(
        StatusCode::SERVICE_UNAVAILABLE,
        "Music Assistant credential storage is unavailable; configure UHC_CREDENTIAL_KEY or a writable key file",
        "credential_storage_unavailable",
    ))?;
    let existing = store.load().map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Music Assistant credentials could not be loaded",
            "credential_storage_failed",
        )
    })?;
    let token = request
        .token
        .filter(|token| !token.trim().is_empty())
        .or_else(|| existing.as_ref().map(|record| record.token.clone()))
        .ok_or_else(|| {
            error(
                StatusCode::BAD_REQUEST,
                "Enter a Music Assistant access token before saving setup",
                "missing_access_token",
            )
        })?;
    let record = MusicAssistantCredentialRecord {
        host: request.host.trim().to_string(),
        port: request.port,
        token,
        tls: request.tls,
        allow_insecure_http: request.allow_insecure_http,
    };
    let config = MusicAssistantConfig {
        host: record.host.clone(),
        port: record.port,
        token: record.token.clone(),
        tls: record.tls,
        allow_insecure_http: record.allow_insecure_http,
    };
    let candidate = Arc::new(
        MusicAssistantAdapter::new(state.bus.clone(), config.clone()).map_err(|_| {
            error(
                StatusCode::BAD_REQUEST,
                "Music Assistant connection settings are not safe or complete",
                "invalid_connection_configuration",
            )
        })?,
    );
    candidate.probe().await.map_err(|_| {
        error(
            StatusCode::BAD_GATEWAY,
            "Music Assistant could not be reached with those settings",
            "connection_probe_failed",
        )
    })?;
    let runtime = state.provider_auth.musicassistant().await.ok_or_else(|| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Music Assistant runtime is unavailable",
            "adapter_unavailable",
        )
    })?;
    let previous_runtime_config = runtime.configuration().await;
    // Probe before either durable or runtime mutation. Once that boundary has
    // passed, installing this already validated in-memory adapter cannot make
    // an outbound request, so a failed peer leaves the current setup intact.
    if store.save(&record).is_err() {
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Music Assistant credentials could not be saved",
            "credential_storage_failed",
        ));
    }
    if runtime.install(candidate, config).await.is_err() {
        restore_musicassistant_record(&store, existing.as_ref());
        return Err(error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Music Assistant runtime could not be updated",
            "adapter_start_failed",
        ));
    }
    let runtime_for_rollback = runtime.clone();
    if state
        .adapter_registry
        .start_registered_if_enabled(&state.coordinator, "musicassistant")
        .await
        .is_err()
    {
        restore_musicassistant_record(&store, existing.as_ref());
        restore_musicassistant_runtime(&state.bus, &runtime_for_rollback, previous_runtime_config)
            .await;
        return Err(error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Music Assistant could not be started",
            "adapter_start_failed",
        ));
    }
    Ok(Json(MusicAssistantConfigureResponse {
        provider: "musicassistant".to_string(),
        configured: true,
        endpoint: endpoint_from_record(&record),
        has_token: true,
    }))
}

fn restore_musicassistant_record(
    store: &MusicAssistantCredentialStore,
    previous: Option<&MusicAssistantCredentialRecord>,
) {
    let result = match previous {
        Some(record) => store.save(record),
        None => store.clear(),
    };
    if let Err(error) = result {
        tracing::error!("Music Assistant credential rollback failed: {error}");
    }
}

async fn restore_musicassistant_runtime(
    bus: &crate::bus::SharedBus,
    runtime: &ReconfigurableMusicAssistant,
    previous: Option<MusicAssistantConfig>,
) {
    match previous {
        Some(config) => match MusicAssistantAdapter::new(bus.clone(), config.clone()) {
            Ok(adapter) => {
                if let Err(error) = runtime.install(Arc::new(adapter), config).await {
                    tracing::error!("Music Assistant runtime rollback failed: {error}");
                }
            }
            Err(error) => tracing::error!("Music Assistant rollback config was invalid: {error}"),
        },
        None => runtime.clear().await,
    }
}

pub async fn musicassistant_status(
    State(state): State<AppState>,
) -> Json<MusicAssistantStatusResponse> {
    let record = state
        .provider_auth
        .musicassistant_credentials
        .as_ref()
        .and_then(|store| store.load().ok().flatten());
    let runtime = state.provider_auth.musicassistant().await;
    let runtime_config = match runtime.as_ref() {
        Some(runtime) => runtime.configuration().await,
        None => None,
    };
    let running = match runtime.as_ref() {
        Some(runtime) => runtime.is_running().await,
        None => false,
    };
    Json(MusicAssistantStatusResponse {
        provider: "musicassistant".to_string(),
        configured: record.is_some() || runtime_config.is_some(),
        enabled: state.coordinator.is_enabled("musicassistant").await,
        running,
        endpoint: record
            .as_ref()
            .map(endpoint_from_record)
            .or_else(|| runtime_config.as_ref().map(endpoint_from_config)),
        has_token: record.is_some_and(|record| !record.token.trim().is_empty())
            || runtime_config.is_some_and(|config| !config.token.trim().is_empty()),
        error: state.aggregator.get_adapter_error("musicassistant").await,
    })
}

fn endpoint_from_record(record: &MusicAssistantCredentialRecord) -> MusicAssistantEndpoint {
    MusicAssistantEndpoint {
        host: record.host.clone(),
        port: record.port,
        tls: record.tls,
        allow_insecure_http: record.allow_insecure_http,
    }
}

fn endpoint_from_config(config: &MusicAssistantConfig) -> MusicAssistantEndpoint {
    MusicAssistantEndpoint {
        host: config.host.clone(),
        port: config.port,
        tls: config.tls,
        allow_insecure_http: config.allow_insecure_http,
    }
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
    let cloud =
        crate::cloud_connector::CloudConnectorConfig::from_runtime(crate::config::get_config_dir())
            .map_err(|_| {
                error(
            StatusCode::SERVICE_UNAVAILABLE,
            "HiPhi Cloud pairing is invalid; repair the installation before connecting Spotify",
            "hiphi_cloud_unavailable",
        )
            })?;
    let cloud = cloud.ok_or_else(|| {
        error(
            StatusCode::PRECONDITION_REQUIRED,
            "Pair this UHC installation with HiPhi Cloud before connecting Spotify",
            "hiphi_cloud_required",
        )
    })?;
    let identity = crate::cloud_connector::InstallationIdentity::load(
        &cloud.key_path,
        cloud.installation_id.clone(),
    )
    .map_err(|_| {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "HiPhi Cloud installation identity is unavailable",
            "hiphi_cloud_unavailable",
        )
    })?;
    let code_verifier = generate_pkce_verifier();
    let code_challenge = pkce_challenge(&code_verifier);
    let state_token = random_token(32);
    let state_digest = URL_SAFE_NO_PAD.encode(Sha256::digest(state_token.as_bytes()));
    let request_id = uuid::Uuid::new_v4();
    let issued_at = now_millis();
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
        cloud_pending_key(request_id),
        PendingOAuth {
            provider: provider.clone(),
            expires_at,
            code_verifier: Some(code_verifier),
            cloud_request_id: Some(request_id),
            client_id: Some(config.client_id.clone()),
            state_digest: Some(state_digest.clone()),
        },
    );
    let authorization_url = hiphi_spotify_authorization_url(
        &identity,
        &config.client_id,
        &code_challenge,
        &state_digest,
        request_id,
        issued_at,
    )
    .map_err(|_| {
        error(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Spotify authorization could not be prepared",
            "oauth_start_failed",
        )
    })?;
    Ok(Json(OAuthStartResponse {
        provider,
        authorization_url,
        state: request_id.to_string(),
        expires_at,
    }))
}

pub async fn oauth_callback(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Response {
    let machine = query.format.as_deref() == Some("json");
    let is_spotify = provider == "spotify";
    let result = oauth_callback_json(State(state.clone()), Path(provider), Query(query)).await;
    // The tunnel exists only to carry this one callback across the public
    // internet; once a real in-flight attempt has concluded (whether Spotify
    // accepted or rejected it), there is nothing left for it to do. Two
    // hard-won rules from #592 apply, though:
    //
    // 1. Never stop the tunnel inline before responding -- this response
    //    (and the settings page it redirects the browser to) travels back
    //    through the tunnel, so an inline stop resets the very connection
    //    carrying the "success" redirect. Teardown is deferred by a grace
    //    period instead.
    // 2. Only tear down for a request that matched a real pending attempt.
    //    A request with an unknown `state` (an internet scanner probing the
    //    public URL, a stale link, a user double-checking the address) must
    //    not be able to kill the tunnel while the user is still working
    //    through Spotify's dashboard.
    if is_spotify {
        stop_tunnel_after_concluded_callback(&state, &result);
    }
    match result {
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

/// The handler mounted only on the callback-only loopback listener used by
/// the built-in SSH tunnel.  It deliberately does not redirect to `/settings`:
/// that page belongs to the main LAN listener and must remain unreachable from
/// the internet-facing tunnel.  The page is bounded and contains no OAuth
/// values, credential data, provider response, or request parameters.
pub(crate) async fn spotify_tunnel_callback(
    State(state): State<AppState>,
    Query(query): Query<OAuthCallbackQuery>,
) -> Response {
    let result = oauth_callback_json(
        State(state.clone()),
        Path("spotify".to_string()),
        Query(query),
    )
    .await;
    stop_tunnel_after_concluded_callback(&state, &result);
    match result {
        Ok(_) => (
            StatusCode::OK,
            "Spotify authorization completed. Return to UHC.",
        )
            .into_response(),
        Err((status, _)) => (
            status,
            "Spotify authorization could not be completed. Return to UHC and try again.",
        )
            .into_response(),
    }
}

fn stop_tunnel_after_concluded_callback(
    state: &AppState,
    result: &Result<Json<ProviderAuthResponse>, (StatusCode, Json<ErrorBody>)>,
) {
    let concluded = match result {
        Ok(_) => true,
        Err((_, Json(body))) => body.code != "invalid_state",
    };
    if concluded {
        state
            .provider_auth
            .spotify_tunnel
            .stop_after(crate::api::spotify_tunnel::TUNNEL_CALLBACK_GRACE);
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
    complete_spotify_authorization(&state, provider, config, code, pending.code_verifier, true)
        .await
}

pub async fn accept_cloud_spotify_callback(
    state: &AppState,
    callback: crate::cloud_connector::SpotifyCallbackMessage,
) -> anyhow::Result<()> {
    callback
        .validate(now_millis())
        .map_err(|reason| anyhow::anyhow!(reason))?;
    let pending = state
        .provider_auth
        .pending
        .write()
        .await
        .remove(&cloud_pending_key(callback.request_id))
        .ok_or_else(|| anyhow::anyhow!("Spotify callback request is unknown or already used"))?;
    if pending.provider != "spotify"
        || pending.expires_at <= now_secs()
        || pending.cloud_request_id != Some(callback.request_id)
        || pending.client_id.as_deref() != Some(callback.client_id.as_str())
        || pending.state_digest.as_deref() != Some(callback.state_digest.as_str())
    {
        anyhow::bail!("Spotify callback binding did not match the pending authorization");
    }
    if let Some(provider_error) = callback.error {
        anyhow::bail!("Spotify authorization ended: {provider_error}");
    }
    let code = callback
        .code
        .ok_or_else(|| anyhow::anyhow!("Spotify callback omitted its authorization code"))?;
    let verifier = pending
        .code_verifier
        .ok_or_else(|| anyhow::anyhow!("Spotify callback lost its local PKCE verifier"))?;
    let config = state
        .provider_auth
        .oauth
        .read()
        .await
        .clone()
        .ok_or_else(|| anyhow::anyhow!("Spotify client configuration is not configured"))?;
    if config.client_id != callback.client_id || callback.redirect_uri != HIPHI_SPOTIFY_CALLBACK_URI
    {
        anyhow::bail!("Spotify callback client or redirect binding changed");
    }
    complete_spotify_authorization(
        state,
        "spotify".to_string(),
        config,
        code,
        Some(verifier),
        false,
    )
    .await
    .map(|_| ())
    .map_err(|(status, Json(body))| anyhow::anyhow!("{} {}", status, body.code))
}

async fn complete_spotify_authorization(
    state: &AppState,
    provider: String,
    config: Arc<SpotifyOAuthConfig>,
    code: String,
    code_verifier: Option<String>,
    allow_client_secret: bool,
) -> Result<Json<ProviderAuthResponse>, (StatusCode, Json<ErrorBody>)> {
    let mut form = vec![
        ("grant_type", "authorization_code".to_string()),
        ("code", code),
        (
            "redirect_uri",
            if allow_client_secret {
                config.redirect_uri.clone()
            } else {
                HIPHI_SPOTIFY_CALLBACK_URI.to_string()
            },
        ),
    ];
    if let Some(verifier) = code_verifier {
        form.push(("client_id", config.client_id.clone()));
        form.push(("code_verifier", verifier));
    }
    let mut request = reqwest::Client::new()
        .post(&config.token_url)
        .timeout(Duration::from_secs(15))
        .form(&form);
    if allow_client_secret {
        if let Some(secret) = &config.client_secret {
            request = request.basic_auth(&config.client_id, Some(secret));
        }
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
            redirect_uri: if allow_client_secret {
                config.redirect_uri.clone()
            } else {
                HIPHI_SPOTIFY_CALLBACK_URI.to_string()
            },
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
        // Re-authorization is a lifecycle transition too.  Route it through
        // the coordinator so Spotify follows the same enable/can_start policy
        // as startup and settings changes.
        let startable: Arc<dyn crate::adapters::Startable> = adapter.clone();
        state
            .coordinator
            .start_enabled(&startable)
            .await
            .map_err(|start_error| {
                error(
                    StatusCode::SERVICE_UNAVAILABLE,
                    &format!("Spotify adapter failed to start: {start_error}"),
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
        // Disconnect the account without deleting the configured OAuth client.
        // The user should be able to reconnect immediately, and the durable
        // configuration must remain the authority reported by Settings.
        store.clear_token().map_err(|e| {
            error(
                StatusCode::INTERNAL_SERVER_ERROR,
                &format!("Spotify token could not be removed from storage: {e}"),
                "token_storage_failed",
            )
        })?;
    }
    adapter.clear_token().await;
    let startable: Arc<dyn crate::adapters::Startable> = adapter.clone();
    state.coordinator.stop_one(&startable).await;
    Ok(Json(ProviderAuthResponse {
        provider,
        authorized: false,
        expires_at: None,
    }))
}

/// POST /api/providers/spotify/tunnel/start - Start (or report the existing)
/// temporary HTTPS tunnel to the separate callback-only listener.
pub async fn spotify_tunnel_start(State(state): State<AppState>) -> Json<TunnelStatusResponse> {
    let status = match state.provider_auth.callback_port() {
        Some(port) => state.provider_auth.spotify_tunnel.start(port).await,
        None => state
            .provider_auth
            .spotify_tunnel
            .fail_closed(
                "Spotify callback listener is unavailable; restart UHC before opening a tunnel.",
            )
            .await,
    };
    Json(status.into())
}

/// GET /api/providers/spotify/tunnel/status - Current tunnel phase, without
/// starting or stopping anything.
pub async fn spotify_tunnel_status(State(state): State<AppState>) -> Json<TunnelStatusResponse> {
    let status = state.provider_auth.spotify_tunnel.status().await;
    Json(status.into())
}

/// POST /api/providers/spotify/tunnel/stop - Stop the tunnel, if any, and
/// return to idle. Also called automatically when the OAuth callback
/// completes; exposed here for a manual "Stop tunnel" action and so a user
/// who abandons the flow does not have to wait for the lifetime cap.
pub async fn spotify_tunnel_stop(State(state): State<AppState>) -> Json<TunnelStatusResponse> {
    state.provider_auth.spotify_tunnel.stop().await;
    let status = state.provider_auth.spotify_tunnel.status().await;
    Json(status.into())
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
    let value = value.trim();
    (16..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
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

fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(i64::MAX)
}

fn cloud_pending_key(request_id: uuid::Uuid) -> String {
    format!("hiphi-cloud:{request_id}")
}

fn hiphi_spotify_authorization_url(
    identity: &crate::cloud_connector::InstallationIdentity,
    client_id: &str,
    code_challenge: &str,
    state_digest: &str,
    request_id: uuid::Uuid,
    issued_at: i64,
) -> anyhow::Result<String> {
    let payload = serde_json::json!({
        "audience": "hiphi-spotify-broker",
        "callback_uri": HIPHI_SPOTIFY_CALLBACK_URI,
        "client_id": client_id,
        "code_challenge": code_challenge,
        "installation_id": identity.installation_id(),
        "issued_at": issued_at,
        "protocol_version": crate::cloud_connector::protocol::PROTOCOL_VERSION,
        "request_id": request_id,
        "state_digest": state_digest,
    });
    let signature = identity.sign(&crate::cloud_connector::protocol::canonical_json(&payload)?);
    let mut url = url::Url::parse(HIPHI_SPOTIFY_CONNECT_URL)?;
    url.query_pairs_mut()
        .append_pair("protocol_version", "1")
        .append_pair("installation_id", identity.installation_id())
        .append_pair("request_id", &request_id.to_string())
        .append_pair("client_id", client_id)
        .append_pair("code_challenge", code_challenge)
        .append_pair("state_digest", state_digest)
        .append_pair("issued_at", &issued_at.to_string())
        .append_pair("signature", &URL_SAFE_NO_PAD.encode(signature.to_bytes()));
    Ok(url.into())
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
    use crate::api::spotify_tunnel::TunnelStatus;
    use axum::{
        body::{to_bytes, Body},
        extract::State,
        http::{Request, StatusCode},
        response::{IntoResponse, Response},
        routing::any,
        Router,
    };
    use ed25519_dalek::{Signature, Verifier as _};
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

    #[test]
    fn hiphi_authorization_url_is_installation_signed_and_contains_no_local_secret() {
        let identity = crate::cloud_connector::InstallationIdentity::generate(
            "11111111-1111-4111-8111-111111111111".to_string(),
        )
        .unwrap();
        let request_id = uuid::Uuid::from_u128(0x22222222222242228222222222222222);
        let client_id = "0123456789abcdef0123456789abcdef";
        let challenge = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let state_digest = "BBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBBB";
        let issued_at = 1_788_060_000_000_i64;
        let authorization_url = hiphi_spotify_authorization_url(
            &identity,
            client_id,
            challenge,
            state_digest,
            request_id,
            issued_at,
        )
        .unwrap();
        let parsed = url::Url::parse(&authorization_url).unwrap();
        assert_eq!(
            parsed.origin().ascii_serialization(),
            "https://app.hiphi.audio"
        );
        assert_eq!(parsed.path(), "/spotify/connect");
        let query: HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(query.get("client_id").map(String::as_str), Some(client_id));
        assert_eq!(
            query.get("code_challenge").map(String::as_str),
            Some(challenge)
        );
        assert!(!authorization_url.contains("verifier"));
        assert!(!authorization_url.contains("secret"));
        assert!(!authorization_url.contains("127.0.0.1"));
        assert_ne!(cloud_pending_key(request_id), request_id.to_string());

        let payload = serde_json::json!({
            "audience": "hiphi-spotify-broker",
            "callback_uri": HIPHI_SPOTIFY_CALLBACK_URI,
            "client_id": client_id,
            "code_challenge": challenge,
            "installation_id": identity.installation_id(),
            "issued_at": issued_at,
            "protocol_version": crate::cloud_connector::protocol::PROTOCOL_VERSION,
            "request_id": request_id,
            "state_digest": state_digest,
        });
        let signature = Signature::from_slice(
            &URL_SAFE_NO_PAD
                .decode(query.get("signature").unwrap())
                .unwrap(),
        )
        .unwrap();
        identity
            .verifying_key()
            .verify(
                &crate::cloud_connector::protocol::canonical_json(&payload).unwrap(),
                &signature,
            )
            .unwrap();
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
        assert!(!valid_spotify_client_id("0123456789abcdef/unsafe"));
        assert!(!valid_spotify_client_id(&"a".repeat(65)));
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

    // ----- #592: tunnel/OAuth interplay ---------------------------------

    use crate::api::spotify_tunnel::{
        TunnelLaunchError, TunnelLauncher, TunnelProcess, TunnelProcessEvent, TunnelProviderKind,
    };
    use axum::http::Method;
    use tower::ServiceExt;

    /// A tunnel process that "prints" one URL line and then idles until
    /// killed, like a healthy long-lived `ssh -R` child.
    struct StaticUrlProcess {
        line: Option<String>,
        idle: Arc<tokio::sync::Notify>,
    }

    #[async_trait::async_trait]
    impl TunnelProcess for StaticUrlProcess {
        async fn next_event(&mut self) -> TunnelProcessEvent {
            if let Some(line) = self.line.take() {
                return TunnelProcessEvent::Line(line);
            }
            self.idle.notified().await;
            TunnelProcessEvent::Exited {
                stderr_tail: String::new(),
            }
        }

        fn kill(&mut self) {
            self.idle.notify_waiters();
        }
    }

    struct StaticUrlLauncher(String);

    #[async_trait::async_trait]
    impl TunnelLauncher for StaticUrlLauncher {
        async fn launch(
            &self,
            _provider: TunnelProviderKind,
            _port: u16,
        ) -> Result<Box<dyn TunnelProcess>, TunnelLaunchError> {
            Ok(Box::new(StaticUrlProcess {
                line: Some(self.0.clone()),
                idle: Arc::new(tokio::sync::Notify::new()),
            }))
        }
    }

    /// Captures the actual loopback target passed to `ssh -R`; a correct
    /// callback router is not enough if startup can still point SSH at UHC's
    /// broad main listener.
    struct RecordingTunnelLauncher {
        url: String,
        ports: Arc<Mutex<Vec<u16>>>,
    }

    #[async_trait::async_trait]
    impl TunnelLauncher for RecordingTunnelLauncher {
        async fn launch(
            &self,
            _provider: TunnelProviderKind,
            port: u16,
        ) -> Result<Box<dyn TunnelProcess>, TunnelLaunchError> {
            self.ports.lock().unwrap().push(port);
            Ok(Box::new(StaticUrlProcess {
                line: Some(self.url.clone()),
                idle: Arc::new(tokio::sync::Notify::new()),
            }))
        }
    }

    /// AppState whose Spotify OAuth config is saved with `saved_redirect`
    /// and whose tunnel manager launches a scripted tunnel that reports
    /// `tunnel_url`. The returned tempdir keeps the credential store alive.
    async fn tunnel_test_state(
        saved_redirect: &str,
        tunnel_url: &str,
    ) -> (AppState, tempfile::TempDir) {
        let dir = tempdir().expect("tempdir");
        let store =
            EncryptedCredentialStore::new(dir.path().join("spotify-credentials.enc"), [7_u8; 32]);
        store
            .save_record(&SpotifyCredentialRecord {
                token: None,
                client_id: "0123456789abcdef0123456789abcdef".to_string(),
                client_secret: Some("shhh".to_string()),
                redirect_uri: saved_redirect.to_string(),
            })
            .expect("save spotify record");
        let mut provider_auth = ProviderAuthState::with_credential_store(store);
        provider_auth.spotify_tunnel = Arc::new(SpotifyTunnelManager::with_launcher(Arc::new(
            StaticUrlLauncher(tunnel_url.to_string()),
        )));

        let bus = crate::bus::create_bus();
        let roon = Arc::new(crate::adapters::roon::RoonAdapter::new_disconnected(
            bus.clone(),
        ));
        let hqp_instances = Arc::new(crate::adapters::hqplayer::HqpInstanceManager::new(
            bus.clone(),
        ));
        let hqplayer = hqp_instances.get_default().await;
        let hqp_zone_links = Arc::new(crate::adapters::hqplayer::HqpZoneLinkService::new(
            hqp_instances.clone(),
        ));
        let lms = Arc::new(crate::adapters::lms::LmsAdapter::new(bus.clone()));
        let openhome = Arc::new(crate::adapters::openhome::OpenHomeAdapter::new(bus.clone()));
        let upnp = Arc::new(crate::adapters::upnp::UPnPAdapter::new(bus.clone()));
        let aggregator = Arc::new(crate::aggregator::ZoneAggregator::new(bus.clone()));
        let coordinator = Arc::new(crate::coordinator::AdapterCoordinator::new(bus.clone()));
        let mut state = AppState::new(
            roon,
            hqplayer,
            hqp_instances,
            hqp_zone_links,
            lms,
            openhome,
            upnp,
            crate::knobs::KnobStore::new(),
            bus,
            aggregator,
            coordinator,
            Vec::new(),
            std::time::Instant::now(),
            CancellationToken::new(),
        );
        state.provider_auth = Arc::new(provider_auth);
        (state, dir)
    }

    async fn start_tunnel_and_wait_active(state: &AppState) {
        state.provider_auth.spotify_tunnel.start(8088).await;
        for _ in 0..200 {
            if matches!(
                state.provider_auth.spotify_tunnel.status().await,
                TunnelStatus::Active { .. }
            ) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!(
            "tunnel did not become active; last status: {:?}",
            state.provider_auth.spotify_tunnel.status().await
        );
    }

    const TUNNEL_A: &str = "https://aaaaa-1-2-3-4.run.pinggy-free.link";

    fn callback_of(tunnel: &str) -> String {
        format!("{tunnel}/api/providers/spotify/oauth/callback")
    }

    /// #641: the public SSH reverse tunnel must terminate at a different,
    /// default-deny listener.  This list intentionally spans reads, writes,
    /// controller/bootstrap routes, provider authority, MCP, and legacy
    /// routes: controller auth may be disabled on a LAN install, so it cannot
    /// be the thing preventing these from becoming internet reachable.
    #[tokio::test]
    async fn callback_only_listener_rejects_every_non_callback_uhc_route() {
        let (state, _dir) = tunnel_test_state(&callback_of(TUNNEL_A), TUNNEL_A).await;
        let app = crate::api::spotify_callback_listener::router(state);

        let liveness = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/healthz")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(liveness.status(), StatusCode::NO_CONTENT);

        let callback = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri("/api/providers/spotify/oauth/callback?state=unknown")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(callback.status(), StatusCode::BAD_REQUEST);

        for (method, path) in [
            (Method::GET, "/zones"),
            (Method::GET, "/now_playing"),
            (Method::POST, "/control"),
            (Method::POST, "/mcp"),
            (Method::POST, "/api/controller/bootstrap"),
            (Method::GET, "/api/controller/status"),
            (Method::POST, "/api/providers/spotify/configure"),
            (Method::POST, "/api/providers/spotify/oauth/revoke"),
            (Method::GET, "/api/providers/spotify/tunnel/status"),
            (Method::GET, "/api/bridges/applemusic/status"),
            (Method::GET, "/roon/status"),
            (Method::GET, "/knob/now_playing"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{path} must be absent from the callback listener"
            );
        }

        let wrong_method = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/providers/spotify/oauth/callback")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_method.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    /// Exercise the socket which `ssh -R` actually targets, rather than only
    /// the in-process router.  The listener is bound to an ephemeral IPv4
    /// loopback port and must expose exactly the same default-deny surface.
    #[tokio::test]
    async fn callback_only_listener_denies_main_routes_over_its_bound_socket() {
        let (state, _dir) = tunnel_test_state(&callback_of(TUNNEL_A), TUNNEL_A).await;
        let shutdown = CancellationToken::new();
        let port = crate::api::spotify_callback_listener::spawn(state, shutdown.clone())
            .await
            .expect("callback listener starts");
        let client = reqwest::Client::new();
        let origin = format!("http://127.0.0.1:{port}");

        let liveness = client
            .get(format!("{origin}/healthz"))
            .send()
            .await
            .expect("loopback liveness response");
        assert_eq!(liveness.status(), StatusCode::NO_CONTENT);

        for path in [
            "/status",
            "/zones",
            "/api/providers/spotify/tunnel/status",
            "/api/bridges/applemusic/status",
            "/mcp",
        ] {
            let response = client
                .get(format!("{origin}{path}"))
                .send()
                .await
                .expect("loopback denial response");
            assert_eq!(
                response.status(),
                StatusCode::NOT_FOUND,
                "{path} must not be served on the tunnel socket"
            );
        }
        shutdown.cancel();
    }

    #[tokio::test]
    async fn tunnel_start_uses_only_the_callback_listener_port() {
        let (mut state, _dir) = tunnel_test_state(&callback_of(TUNNEL_A), TUNNEL_A).await;
        let ports = Arc::new(Mutex::new(Vec::new()));
        // Preserve the configured state from the fixture and replace only its
        // process launcher, avoiding every real-network path.
        let mut auth = (*state.provider_auth).clone();
        auth.spotify_tunnel = Arc::new(SpotifyTunnelManager::with_launcher(Arc::new(
            RecordingTunnelLauncher {
                url: TUNNEL_A.to_string(),
                ports: ports.clone(),
            },
        )));
        auth.bind_callback_port(19_417);
        state.provider_auth = Arc::new(auth);

        let _ = spotify_tunnel_start(State(state.clone())).await;
        start_tunnel_and_wait_active(&state).await;
        assert_eq!(*ports.lock().unwrap(), vec![19_417]);
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn oauth_start_requires_a_paired_hiphi_installation_even_if_a_legacy_tunnel_is_live() {
        let (state, _dir) = tunnel_test_state(&callback_of(TUNNEL_A), TUNNEL_A).await;
        start_tunnel_and_wait_active(&state).await;
        let error = oauth_start(State(state), Path("spotify".to_string()))
            .await
            .expect_err("a temporary callback tunnel must not bypass cloud pairing");
        assert_eq!(error.0, StatusCode::PRECONDITION_REQUIRED);
        assert_eq!(error.1 .0.code, "hiphi_cloud_required");
    }

    /// #592: a random probe of the public callback URL (internet scanner,
    /// stale link, user double-checking the address) must not tear the
    /// tunnel down while the user is still working in Spotify's dashboard.
    #[tokio::test]
    async fn callback_probe_with_unknown_state_leaves_the_tunnel_running() {
        let (state, _dir) = tunnel_test_state(&callback_of(TUNNEL_A), TUNNEL_A).await;
        start_tunnel_and_wait_active(&state).await;
        let response = oauth_callback(
            State(state.clone()),
            Path("spotify".to_string()),
            Query(OAuthCallbackQuery {
                code: Some("whatever".to_string()),
                state: "not-a-real-state-token".to_string(),
                error: None,
                error_description: None,
                format: Some("json".to_string()),
            }),
        )
        .await;
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(
            matches!(
                state.provider_auth.spotify_tunnel.status().await,
                TunnelStatus::Active { .. }
            ),
            "an unknown-state probe must not kill the tunnel"
        );
    }
}
