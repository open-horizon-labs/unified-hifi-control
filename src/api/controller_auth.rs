//! Installation-bound controller authentication for browser and tunnel use.
//!
//! This is deliberately separate from provider credentials and Apple bridge
//! bearers.  A one-time bootstrap secret creates an opaque browser session;
//! state-changing browser requests additionally require an exact same-origin
//! Origin and the session's CSRF token.

use axum::{
    body::Body,
    extract::State,
    http::{header, HeaderMap, HeaderValue, Request, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
    Json,
};
use rand::{distributions::Alphanumeric, Rng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::sync::RwLock;

use super::AppState;
use crate::config::get_config_file_path;

const COOKIE_NAME: &str = "uhc_controller";
const SESSION_TTL: u64 = 30 * 24 * 60 * 60;
const BOOTSTRAP_FILE: &str = "controller-bootstrap.sha256";

#[derive(Clone)]
pub struct ControllerAuthState {
    inner: Arc<RwLock<Inner>>,
    bootstrap_display: Arc<RwLock<Option<String>>>,
}

struct Inner {
    bootstrap_hash: Option<[u8; 32]>,
    sessions: HashMap<String, Session>,
}

#[derive(Clone)]
struct Session {
    csrf: String,
    expires_at: u64,
}

#[derive(Debug, Deserialize)]
pub struct BootstrapRequest {
    pub token: String,
}

#[derive(Debug, Serialize)]
pub struct BootstrapResponse {
    pub authenticated: bool,
    pub csrf_token: String,
    pub expires_at: u64,
}

#[derive(Debug, Serialize)]
pub struct ControllerStatus {
    /// Whether controller auth is enabled at all (UHC_REQUIRE_CONTROLLER_AUTH).
    /// With the gate off (the default), no client should ever prompt.
    pub auth_required: bool,
    pub authenticated: bool,
    pub bootstrap_required: bool,
    pub expires_at: Option<u64>,
}

#[derive(Debug, Serialize)]
struct AuthError {
    error: &'static str,
    code: &'static str,
}

impl ControllerAuthState {
    pub fn new() -> Self {
        let secret = std::env::var("UHC_BOOTSTRAP_TOKEN").ok();
        let path = get_config_file_path(BOOTSTRAP_FILE);
        let mut generated_secret = None;
        let hash = secret
            .as_deref()
            .map(hash_token)
            .or_else(|| {
                std::fs::read(&path)
                    .ok()
                    .and_then(|bytes| parse_hash(&bytes))
            })
            .or_else(|| {
                let generated = random_token(32);
                generated_secret = Some(generated.clone());
                let hash = hash_token(&generated);
                if let Some(parent) = path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = write_private(&path, &hash);
                Some(hash)
            });
        Self {
            inner: Arc::new(RwLock::new(Inner {
                bootstrap_hash: hash,
                sessions: HashMap::new(),
            })),
            bootstrap_display: Arc::new(RwLock::new(generated_secret)),
        }
    }

    #[cfg(test)]
    fn with_bootstrap_token(token: &str) -> Self {
        Self {
            inner: Arc::new(RwLock::new(Inner {
                bootstrap_hash: Some(hash_token(token)),
                sessions: HashMap::new(),
            })),
            bootstrap_display: Arc::new(RwLock::new(None)),
        }
    }

    /// Consume the generated secret for one local-console display. Secrets
    /// supplied through UHC_BOOTSTRAP_TOKEN are never echoed by UHC.
    pub async fn take_bootstrap_secret(&self) -> Option<String> {
        self.bootstrap_display.write().await.take()
    }

    // The unit error is deliberate: bootstrap failure carries no information
    // on purpose (an attacker probing the endpoint learns nothing from the
    // error shape), and the sole caller maps it straight to one HTTP status.
    #[allow(clippy::result_unit_err)]
    pub async fn bootstrap(&self, supplied: &str) -> Result<(String, String, u64), ()> {
        let mut inner = self.inner.write().await;
        let expected = inner.bootstrap_hash.take().ok_or(())?;
        if !constant_time_eq(&expected, &hash_token(supplied)) {
            inner.bootstrap_hash = Some(expected);
            return Err(());
        }
        let session = random_token(48);
        let csrf = random_token(32);
        let expires_at = now_secs() + SESSION_TTL;
        inner.sessions.insert(
            session.clone(),
            Session {
                csrf: csrf.clone(),
                expires_at,
            },
        );
        Ok((session, csrf, expires_at))
    }

    async fn session(&self, headers: &HeaderMap) -> Option<Session> {
        let cookie = headers.get(header::COOKIE)?.to_str().ok()?;
        let token = cookie.split(';').find_map(|part| {
            let (name, value) = part.trim().split_once('=')?;
            (name == COOKIE_NAME && !value.is_empty()).then_some(value)
        })?;
        let mut inner = self.inner.write().await;
        let session = inner.sessions.get(token)?.clone();
        if session.expires_at <= now_secs() {
            inner.sessions.remove(token);
            return None;
        }
        Some(session)
    }

    pub async fn status(&self, headers: &HeaderMap) -> ControllerStatus {
        let session = self.session(headers).await;
        let inner = self.inner.read().await;
        ControllerStatus {
            auth_required: controller_auth_required(),
            authenticated: session.is_some(),
            bootstrap_required: inner.bootstrap_hash.is_some(),
            expires_at: session.map(|s| s.expires_at),
        }
    }
}

impl Default for ControllerAuthState {
    fn default() -> Self {
        Self::new()
    }
}

pub async fn bootstrap(
    State(state): State<AppState>,
    Json(request): Json<BootstrapRequest>,
) -> Response {
    match state.controller_auth.bootstrap(request.token.trim()).await {
        Ok((session, csrf, expires_at)) => {
            let mut response = Json(BootstrapResponse {
                authenticated: true,
                csrf_token: csrf,
                expires_at,
            })
            .into_response();
            let cookie = format!(
                "{COOKIE_NAME}={session}; Path=/; HttpOnly; SameSite=Strict; Max-Age={SESSION_TTL}"
            );
            if let Ok(value) = HeaderValue::from_str(&cookie) {
                response.headers_mut().insert(header::SET_COOKIE, value);
            }
            response
        }
        Err(()) => (
            StatusCode::UNAUTHORIZED,
            Json(AuthError {
                error: "Bootstrap token is invalid or already used",
                code: "bootstrap_invalid",
            }),
        )
            .into_response(),
    }
}

pub async fn status(State(state): State<AppState>, headers: HeaderMap) -> Json<ControllerStatus> {
    Json(state.controller_auth.status(&headers).await)
}

/// Protect the routes that can configure providers, pair management, MCP, or
/// mutate playback. Native Apple companion bearer routes remain independent.
pub async fn middleware(
    State(auth): State<ControllerAuthState>,
    request: Request<Body>,
    next: Next,
) -> Response {
    let path = request.uri().path();
    // Controller auth is OPT-IN, full stop (owner decision, 2026-08-26).
    // An earlier revision force-gated provider/credential routes even with
    // the switch off, on the theory that a fresh LAN install shouldn't let
    // any reachable client replace OAuth credentials -- but that shipped a
    // mandatory bootstrap ceremony to trusting home-LAN installs that never
    // asked for it, contradicting docs/controller-auth.md's own "intentionally
    // opt-in" contract. Installations that want the owner gate (including
    // its always-on protection of provider routes) set
    // UHC_REQUIRE_CONTROLLER_AUTH=1; the add-on and docs surface that choice.
    let owner_operation = requires_controller_auth(path);
    // HA Ingress (#581): a request proxied by the Supervisor is already
    // authenticated by the user's Home Assistant session -- a strictly
    // stronger boundary than the bootstrap cookie. Trust is triple-gated
    // (UHC_INGRESS=1 set only by the add-on's run.sh, the peer must be the
    // Supervisor's proxy network, and X-Ingress-Path must be present and
    // well-formed -- see crate::api::ingress). Same-origin/CSRF cannot apply
    // through the proxy: the browser Origin is the HA frontend's origin and
    // Host is the internal proxy address, so they never match; the proxy
    // itself is what guarantees the request came from an authenticated HA
    // session. Direct-port requests never satisfy the peer gate and keep the
    // full posture below, even on an ingress-enabled install.
    if super::ingress::trusted_ingress_request(&request) {
        return next.run(request).await;
    }
    if !controller_auth_required() {
        return next.run(request).await;
    }
    // The browser shell and hardware/status protocol remain reachable on a
    // LAN during migration. API and MCP surfaces are the protected boundary;
    // the UI obtains its cookie through the bootstrap screen before calling
    // those surfaces.
    if is_public(path)
        || is_native_bridge(path)
        || (!owner_operation && !is_protected(path, request.method()))
    {
        return next.run(request).await;
    }
    let headers = request.headers();
    let Some(session) = auth.session(headers).await else {
        return unauthorized();
    };
    if request.method() != axum::http::Method::GET
        && request.method() != axum::http::Method::HEAD
        && (!same_origin(headers) || !csrf_matches(headers, &session.csrf))
    {
        return (
            StatusCode::FORBIDDEN,
            Json(AuthError {
                error: "Controller request is not authorized",
                code: "csrf_failed",
            }),
        )
            .into_response();
    }
    next.run(request).await
}

/// Routes that establish or revoke authority are protected even when the
/// compatibility mode for ordinary LAN playback is enabled.  The native
/// companion bearer routes remain explicitly excluded by `is_native_bridge`.
fn requires_controller_auth(path: &str) -> bool {
    // Spotify's provider redirect is a cross-site top-level navigation. The
    // browser's SameSite=Strict controller cookie is intentionally absent, so
    // the callback must be authorized by its single-use pending OAuth state
    // and PKCE exchange instead of the browser session. Start/configure,
    // account, and revoke remain controller-owned operations.
    (path.starts_with("/api/providers/") && path != "/api/providers/spotify/oauth/callback")
        || matches!(path, "/api/bridges/applemusic/pair")
}

fn controller_auth_required() -> bool {
    matches!(
        std::env::var("UHC_REQUIRE_CONTROLLER_AUTH").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "on")
    )
}

fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/status" | "/api/controller/bootstrap" | "/api/controller/status"
    ) || path.starts_with("/assets/")
        || matches!(
            path,
            "/favicon.ico"
                | "/tailwind.css"
                | "/dx-components-theme.css"
                | "/apple-touch-icon.png"
                // WebMCP bridge (#579): loaded on every page before the
                // owner may have bootstrapped a controller session, exactly
                // like the other static assets above.
                | "/webmcp-bridge.js"
        )
}

fn is_native_bridge(path: &str) -> bool {
    path == "/api/bridges/applemusic/discover"
        || path == "/api/bridges/applemusic/claim"
        || path == "/api/bridges/applemusic/revoke"
        || path == "/api/bridges/applemusic/state"
        || path == "/api/bridges/applemusic/commands"
        || path.starts_with("/api/bridges/applemusic/commands/")
        || path == "/api/bridges/applemusic/content"
        || path.starts_with("/api/bridges/applemusic/content/")
}

fn is_protected(path: &str, method: &axum::http::Method) -> bool {
    if path.starts_with("/api/providers/") && path != "/api/providers/spotify/oauth/callback"
        || path == "/mcp"
    {
        return true;
    }
    if path == "/api/settings" {
        return method != axum::http::Method::GET;
    }
    if path == "/api/bridges/applemusic/pair"
        || path == "/api/bridges/applemusic/revoke"
        || path == "/api/bridges/applemusic/rename"
        || path == "/api/bridges/applemusic/remove"
    {
        return true;
    }
    if *method == axum::http::Method::GET || *method == axum::http::Method::HEAD {
        return false;
    }
    matches!(
        path,
        "/control"
            | "/knob/control"
            | "/knob/config"
            | "/roon/control"
            | "/roon/volume"
            | "/roon/play"
            | "/roon/play_item"
            | "/roon/browse"
            | "/hqplayer/control"
            | "/hqplayer/volume"
            | "/hqplayer/setting"
            | "/hqplayer/profile"
            | "/hqplayer/configure"
            | "/lms/configure"
            | "/lms/control"
            | "/lms/volume"
            | "/openhome/control"
            | "/upnp/control"
    )
}

fn csrf_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("x-uhc-csrf-token")
        .and_then(|v| v.to_str().ok())
        .map(|v| constant_time_eq(v.as_bytes(), expected.as_bytes()))
        .unwrap_or(false)
}

fn same_origin(headers: &HeaderMap) -> bool {
    let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    let Some(host) = headers.get(header::HOST).and_then(|v| v.to_str().ok()) else {
        return false;
    };
    origin == format!("http://{host}") || origin == format!("https://{host}")
}

fn unauthorized() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(AuthError {
            // Shared with the client-side detector in
            // `crate::app::api::CONTROLLER_UNAUTHORIZED_MESSAGE` so the two
            // sides cannot drift apart (#570): the browser fetch helpers
            // match this exact text to route into the bootstrap prompt
            // instead of rendering the raw HTTP error.
            error: crate::app::api::CONTROLLER_UNAUTHORIZED_MESSAGE,
            code: "controller_unauthorized",
        }),
    )
        .into_response()
}

fn hash_token(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    left.len() == right.len()
        && left
            .iter()
            .zip(right)
            .fold(0_u8, |acc, (a, b)| acc | (a ^ b))
            == 0
}
fn parse_hash(bytes: &[u8]) -> Option<[u8; 32]> {
    (bytes.len() == 32).then(|| bytes.try_into().ok()).flatten()
}
fn write_private(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, bytes)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn bootstrap_is_single_use_and_creates_a_session() {
        let auth = ControllerAuthState::with_bootstrap_token("one-time");
        let (cookie, csrf, expires) = auth.bootstrap("one-time").await.unwrap();
        assert!(!cookie.is_empty());
        assert!(!csrf.is_empty());
        assert!(expires > now_secs());
        assert!(auth.bootstrap("one-time").await.is_err());
        let headers = HeaderMap::from_iter([(
            header::COOKIE,
            HeaderValue::from_str(&format!("{COOKIE_NAME}={cookie}")).unwrap(),
        )]);
        let status = auth.status(&headers).await;
        assert!(status.authenticated);
        assert!(!status.bootstrap_required);
    }

    #[test]
    fn origin_and_csrf_are_required_for_mutations() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("uhc.example.test"));
        assert!(!same_origin(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://evil.example"),
        );
        assert!(!same_origin(&headers));
        headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://uhc.example.test"),
        );
        headers.insert("x-uhc-csrf-token", HeaderValue::from_static("csrf"));
        assert!(same_origin(&headers));
        assert!(csrf_matches(&headers, "csrf"));
        assert!(!csrf_matches(&headers, "other"));
    }
    #[test]
    fn comparison_is_not_plain_string_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
    #[test]
    fn webmcp_bridge_script_is_public_like_other_static_assets() {
        // #579: the bridge script must load on every page, including before
        // the owner has bootstrapped a controller session.
        assert!(is_public("/webmcp-bridge.js"));
        assert!(is_public("/tailwind.css"));
        assert!(!is_public("/webmcp-bridge.js.map"));
    }

    #[test]
    fn bridge_native_routes_are_not_browser_routes() {
        assert!(is_native_bridge("/api/bridges/applemusic/state"));
        assert!(!is_native_bridge("/api/bridges/applemusic/pair"));
    }

    #[test]
    fn protected_surface_keeps_read_only_lan_pages_available() {
        assert!(is_protected(
            "/api/providers/spotify/account",
            &axum::http::Method::GET
        ));
        assert!(!is_protected("/api/settings", &axum::http::Method::GET));
        assert!(is_protected("/api/settings", &axum::http::Method::POST));
        assert!(!is_protected("/roon/zones", &axum::http::Method::GET));
        assert!(is_protected("/roon/control", &axum::http::Method::POST));
    }

    #[test]
    fn authority_transitions_are_protected_in_compatibility_mode() {
        assert!(requires_controller_auth(
            "/api/providers/spotify/oauth/start"
        ));
        assert!(!requires_controller_auth(
            "/api/providers/spotify/oauth/callback"
        ));
        assert!(!is_protected(
            "/api/providers/spotify/oauth/callback",
            &axum::http::Method::GET
        ));
        assert!(requires_controller_auth("/api/providers/spotify/account"));
        assert!(requires_controller_auth("/api/bridges/applemusic/pair"));
        assert!(!requires_controller_auth("/api/bridges/applemusic/status"));
        assert!(!requires_controller_auth("/api/bridges/applemusic/claim"));
        assert!(!requires_controller_auth("/zones"));
    }
}
