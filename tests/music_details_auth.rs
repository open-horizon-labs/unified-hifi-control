//! A separate one-test executable isolates the opt-in environment gate from library tests.
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use unified_hifi_control::api::controller_auth::{middleware, ControllerAuthState};
#[tokio::test]
async fn music_details_get_uses_real_controller_auth_boundary() {
    use axum::{routing::get, Router};
    use tower::ServiceExt;
    let previous = std::env::var_os("UHC_REQUIRE_CONTROLLER_AUTH");
    let previous_bootstrap = std::env::var_os("UHC_BOOTSTRAP_TOKEN");
    std::env::set_var("UHC_REQUIRE_CONTROLLER_AUTH", "1");
    std::env::set_var("UHC_BOOTSTRAP_TOKEN", "music-test");
    let auth = ControllerAuthState::new();
    let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_calls = calls.clone();
    let app = Router::new()
        .route(
            "/zones/{zone_id}/music-details",
            get(move || {
                let calls = handler_calls.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    "context"
                }
            }),
        )
        .layer(axum::middleware::from_fn_with_state(
            auth.clone(),
            middleware,
        ));
    let unauthorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/zones/roon:test/music-details?language=en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let unauthorized_calls = calls.load(std::sync::atomic::Ordering::SeqCst);
    let (cookie, _, _) = auth.bootstrap("music-test").await.unwrap();
    let authorized = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/zones/roon:test/music-details?language=en")
                .header(header::COOKIE, format!("uhc_controller={cookie}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    std::env::remove_var("UHC_REQUIRE_CONTROLLER_AUTH");
    let compatibility = app
        .oneshot(
            Request::builder()
                .uri("/zones/roon:test/music-details?language=en")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    match previous {
        Some(v) => std::env::set_var("UHC_REQUIRE_CONTROLLER_AUTH", v),
        None => std::env::remove_var("UHC_REQUIRE_CONTROLLER_AUTH"),
    };
    match previous_bootstrap {
        Some(v) => std::env::set_var("UHC_BOOTSTRAP_TOKEN", v),
        None => std::env::remove_var("UHC_BOOTSTRAP_TOKEN"),
    };
    assert_eq!(
        unauthorized_calls, 0,
        "Unauthenticated reads must not invoke enrichment"
    );
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        unauthorized.headers()[header::CACHE_CONTROL],
        "private, no-store"
    );
    assert_eq!(authorized.status(), StatusCode::OK);
    assert_eq!(
        compatibility.status(),
        StatusCode::OK,
        "Existing opt-in compatibility remains unchanged"
    );
    let bytes = axum::body::to_bytes(unauthorized.into_body(), 4096)
        .await
        .unwrap();
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["code"], "controller_unauthorized");
    assert_eq!(
        body["error"],
        unified_hifi_control::app::api::CONTROLLER_UNAUTHORIZED_MESSAGE
    );
}
