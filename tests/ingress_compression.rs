//! HA Ingress vs. response compression (#604 follow-up).
//!
//! `IngressRewriteLayer` is the OUTERMOST layer, so it observes responses
//! *after* the inner `CompressionLayer` has encoded them. The first cut of
//! the wasm-path rewrite (#604) therefore ran `String::from_utf8_lossy` over
//! gzip bytes: the needle was never found, the body was mangled into U+FFFD
//! replacement characters, and the embedded panel silently failed to load its
//! wasm bundle -- the exact symptom #604 set out to fix.
//!
//! These tests pin the layer ordering that production uses (compression
//! inside, rewrite outside) so the interaction is covered, not just the pure
//! string function.

#![cfg(all(feature = "server", not(target_arch = "wasm32")))]

use axum::{
    body::Body,
    http::{header, Request, StatusCode},
    routing::get,
    Router,
};
use http_body_util::BodyExt;
use tower::ServiceExt;
use tower_http::compression::CompressionLayer;
use unified_hifi_control::api::ingress::IngressRewriteLayer;

const PREFIX: &str = "/api/hassio_ingress/tok3n";

/// A stand-in for the dx-generated client bundle: big enough that the
/// compression layer bothers to encode it, and carrying the rooted wasm
/// literal the rewrite has to find.
fn client_bundle() -> String {
    format!(
        "{}\nht({{module_or_path:\"/./assets/app_bg-dxh98.wasm\"}}).then(s=>{{}});\n",
        "// filler to clear the compression size threshold\n".repeat(200)
    )
}

fn app() -> Router {
    Router::new()
        .route(
            "/assets/{*path}",
            get(|| async { ([(header::CONTENT_TYPE, "text/javascript")], client_bundle()) }),
        )
        // Mirrors src/main.rs: compression is applied to the inner router,
        // and the ingress rewrite wraps the whole thing.
        .layer(CompressionLayer::new())
        .layer(IngressRewriteLayer)
}

fn ingress_request(accept_encoding: Option<&str>) -> Request<Body> {
    let mut builder = Request::builder()
        .uri("/assets/app.js")
        .header("x-ingress-path", PREFIX);
    if let Some(encoding) = accept_encoding {
        builder = builder.header(header::ACCEPT_ENCODING, encoding);
    }
    builder
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            40000,
        ))))
        .body(Body::empty())
        .unwrap()
}

/// The regression: a browser offers gzip, so without intervention the body
/// reaching the rewrite is compressed and unrewritable.
#[tokio::test]
async fn js_is_rewritten_even_when_the_client_offers_gzip() {
    // Gate 1 + gate 2 (loopback stands in for the Supervisor's proxy net).
    std::env::set_var("UHC_INGRESS", "1");
    std::env::set_var("UHC_INGRESS_TRUSTED_PROXIES", "127.0.0.1");

    let response = app()
        .oneshot(ingress_request(Some("gzip, br")))
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        !response.headers().contains_key(header::CONTENT_ENCODING),
        "ingress responses must not be content-encoded, or the rewrite cannot read them"
    );

    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).expect("body must still be valid UTF-8, not mangled");
    assert!(
        text.contains(&format!("\"{PREFIX}/./assets/app_bg-dxh98.wasm\"")),
        "wasm loader path was not prefixed with the ingress base"
    );
    assert!(
        !text.contains('\u{FFFD}'),
        "body contains replacement characters: it was decoded from compressed bytes"
    );
}

/// The same request without an `Accept-Encoding` header must behave
/// identically -- this is the path that always worked, kept as a control.
#[tokio::test]
async fn js_is_rewritten_when_no_encoding_is_offered() {
    std::env::set_var("UHC_INGRESS", "1");
    std::env::set_var("UHC_INGRESS_TRUSTED_PROXIES", "127.0.0.1");

    let response = app().oneshot(ingress_request(None)).await.unwrap();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let text = std::str::from_utf8(&body).unwrap();

    assert!(text.contains(&format!("\"{PREFIX}/./assets/app_bg-dxh98.wasm\"")));
}

/// Direct-port requests keep compression: dropping it for everyone would be
/// a real bandwidth regression for the normal (non-add-on) deployment.
#[tokio::test]
async fn direct_mode_still_compresses_and_is_untouched() {
    std::env::set_var("UHC_INGRESS", "1");
    std::env::set_var("UHC_INGRESS_TRUSTED_PROXIES", "127.0.0.1");

    // No X-Ingress-Path header -> not an ingress request (gate 3 fails).
    let request = Request::builder()
        .uri("/assets/app.js")
        .header(header::ACCEPT_ENCODING, "gzip")
        .extension(axum::extract::ConnectInfo(std::net::SocketAddr::from((
            [127, 0, 0, 1],
            40000,
        ))))
        .body(Body::empty())
        .unwrap();

    let response = app().oneshot(request).await.unwrap();
    assert_eq!(
        response
            .headers()
            .get(header::CONTENT_ENCODING)
            .and_then(|v| v.to_str().ok()),
        Some("gzip"),
        "direct-mode responses must still be compressed"
    );
}
