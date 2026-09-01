//! The loopback-only HTTP surface published by the temporary Spotify tunnel.
//!
//! This is deliberately a separate listener, not middleware on UHC's main
//! router.  An SSH reverse forward publishes its target socket wholesale, so
//! filtering after a request enters the main listener would leave every future
//! route one wiring mistake away from public exposure (#641).

use axum::{http::StatusCode, routing::get, Router};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

use super::{provider_auth, AppState};

pub const CALLBACK_PATH: &str = "/api/providers/spotify/oauth/callback";
pub const LIVENESS_PATH: &str = "/healthz";

/// The whole public surface of a built-in Spotify tunnel.  The callback must
/// stay exact (rather than using UHC's generic `{provider}` route), and every
/// other path is absent.  Axum supplies 405 for the wrong method on either
/// allowed path; the fallback supplies 404 for everything else.
pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route(CALLBACK_PATH, get(provider_auth::spotify_tunnel_callback))
        .route(LIVENESS_PATH, get(liveness))
        .fallback(denied)
        .with_state(state)
}

/// A bounded, non-sensitive round-trip check for a newly allocated tunnel.
async fn liveness() -> StatusCode {
    StatusCode::NO_CONTENT
}

/// Do not disclose whether a main-UHC path happens to exist.
async fn denied() -> StatusCode {
    StatusCode::NOT_FOUND
}

/// Start the callback listener on loopback with an OS-chosen port.  Its port,
/// rather than the main UHC listener's port or an inbound Host header, is the
/// only target ever given to `ssh -R`.
pub async fn spawn(state: AppState, shutdown: CancellationToken) -> anyhow::Result<u16> {
    let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await?;
    let port = listener.local_addr()?.port();
    let app = router(state);
    tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
        {
            tracing::error!(%error, "Spotify callback-only listener stopped unexpectedly");
        }
    });
    Ok(port)
}
