//! Home Assistant Ingress support (#581).
//!
//! HA Ingress proxies the UI under `/api/hassio_ingress/<token>/...` and
//! strips that prefix before forwarding, adding an `X-Ingress-Path` header
//! carrying it. Server-side routing is therefore untouched; what breaks are
//! the origin-absolute URLs in the HTML we emit and the URLs the client
//! constructs. This module owns the server half of the fix:
//!
//! - [`IngressRewriteLayer`]: for trusted ingress requests, rewrites HTML
//!   responses so `href="/..."`/`src="/..."` attributes carry the prefix and
//!   injects a `<meta name="uhc-base-path">` tag -- the single value
//!   `crate::app::base_path` reads to prefix every client-issued URL.
//! - [`trusted_ingress_request`]: the trust rule controller-auth consults.
//!
//! # Trust rule (deliberately triple-gated)
//!
//! A request is "ingress" only when ALL of:
//! 1. `UHC_INGRESS=1` -- set exclusively by the HA add-on's run.sh. Any
//!    other deployment never enters these code paths, so the non-ingress
//!    posture is byte-identical.
//! 2. The TCP peer is the Supervisor's ingress proxy network
//!    (`172.30.32.0/23` by default; `UHC_INGRESS_TRUSTED_PROXIES` overrides
//!    for testing). `X-Ingress-Path` is just a header -- a LAN client hitting
//!    the fallback direct port can type it, so the peer check is what makes
//!    it evidence of Supervisor proxying rather than a client claim.
//! 3. The `X-Ingress-Path` header is present and shaped like a safe path
//!    (charset-restricted, so it can be interpolated into HTML without any
//!    injection surface).
//!
//! Controller-auth treats a trusted ingress request as authenticated: Home
//! Assistant has already authenticated the user session before the
//! Supervisor proxies anything, which is a strictly stronger boundary than
//! the one-time bootstrap cookie. Same-origin/CSRF checks are skipped for
//! those requests too -- the browser Origin is the HA frontend's, never
//! UHC's own host, and cross-site requests cannot reach the proxy without an
//! authenticated HA session. Direct-port requests (even on an
//! ingress-enabled install) never satisfy gate 2 and keep the full posture.

use axum::{
    body::Body,
    extract::ConnectInfo,
    http::{header, Request},
    response::Response,
};
use futures::future::BoxFuture;
use http_body_util::BodyExt;
use std::net::{IpAddr, SocketAddr};
use std::task::{Context, Poll};
use tower::{Layer, Service};

/// Header the Supervisor's ingress proxy adds with the browser-visible
/// prefix (e.g. `/api/hassio_ingress/<token>`).
pub const INGRESS_PATH_HEADER: &str = "x-ingress-path";

/// Meta tag name the rewrite layer injects; mirrored by
/// `crate::app::base_path::BASE_PATH_META`.
const BASE_PATH_META: &str = "uhc-base-path";

/// Gate 1: ingress handling is opt-in via the add-on's run.sh.
pub fn ingress_enabled() -> bool {
    matches!(
        std::env::var("UHC_INGRESS").ok().as_deref(),
        Some("1" | "true" | "TRUE" | "yes" | "on")
    )
}

/// Gate 3: the header value must be a plain absolute path over a charset
/// that cannot break out of an HTML attribute or smuggle traversal.
fn valid_ingress_path(value: &str) -> bool {
    value.starts_with('/')
        && value.len() > 1
        && value.len() <= 256
        && !value.contains("..")
        && !value.ends_with('/')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '/' | '_' | '-' | '.'))
}

/// Gate 2: the TCP peer must be the Supervisor's proxy network.
/// `UHC_INGRESS_TRUSTED_PROXIES` (comma-separated `ip` or `ip/prefix_len`
/// IPv4 entries) overrides the default for local verification rigs.
fn trusted_peer(ip: &IpAddr) -> bool {
    match std::env::var("UHC_INGRESS_TRUSTED_PROXIES") {
        Ok(raw) => raw
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .any(|entry| ip_matches(ip, entry)),
        // Default: the HA Supervisor internal network the ingress proxy
        // lives on (hassio's docker network, 172.30.32.0/23).
        Err(_) => ip_matches(ip, "172.30.32.0/23"),
    }
}

/// Match `ip` against `entry`: an exact IP (v4 or v6) or an IPv4 CIDR.
fn ip_matches(ip: &IpAddr, entry: &str) -> bool {
    if let Some((net, len)) = entry.split_once('/') {
        let (Ok(net), Ok(len)) = (net.parse::<std::net::Ipv4Addr>(), len.parse::<u32>()) else {
            return false;
        };
        let IpAddr::V4(ip) = ip else { return false };
        if len > 32 {
            return false;
        }
        let mask = if len == 0 { 0 } else { u32::MAX << (32 - len) };
        (u32::from(*ip) & mask) == (u32::from(net) & mask)
    } else {
        entry.parse::<IpAddr>().map(|e| e == *ip).unwrap_or(false)
    }
}

/// The validated ingress base path for this request, when it satisfies all
/// three trust gates; `None` otherwise (including every direct-mode request).
pub fn request_ingress_base<B>(request: &Request<B>) -> Option<String> {
    if !ingress_enabled() {
        return None;
    }
    let peer = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ConnectInfo(addr)| addr.ip())?;
    if !trusted_peer(&peer) {
        return None;
    }
    let value = request
        .headers()
        .get(INGRESS_PATH_HEADER)?
        .to_str()
        .ok()?
        .trim()
        .trim_end_matches('/')
        .to_string();
    valid_ingress_path(&value).then_some(value)
}

/// Whether this request arrived through the authenticated Supervisor proxy.
/// Consulted by `crate::api::controller_auth::middleware`.
pub fn trusted_ingress_request<B>(request: &Request<B>) -> bool {
    request_ingress_base(request).is_some()
}

/// Prefix `attr="/..."` occurrences with `base`, leaving protocol-relative
/// (`attr="//..."`) and non-rooted values alone.
fn prefix_attr(html: &str, attr: &str, base: &str) -> String {
    let needle = format!("{attr}=\"/");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    while let Some(idx) = rest.find(&needle) {
        let after = idx + needle.len();
        out.push_str(&rest[..after]);
        // Protocol-relative URL: `attr="//host/..."` -- not a rooted path.
        if !rest[after..].starts_with('/') {
            // Rewrite `attr="/x` to `attr="{base}/x`: back up over the `/`
            // we already copied and splice the base in front of it.
            out.truncate(out.len() - 1);
            out.push_str(base);
            out.push('/');
        }
        rest = &rest[after..];
    }
    out.push_str(rest);
    out
}

/// Rewrite one HTML document for serving under `base`: prefix rooted
/// `href`/`src` attributes and inject the base-path meta tag.
fn rewrite_html(html: &str, base: &str) -> String {
    let mut out = prefix_attr(html, "href", base);
    out = prefix_attr(&out, "src", base);
    let meta = format!("<meta name=\"{BASE_PATH_META}\" content=\"{base}\">");
    if out.contains(&meta) {
        return out; // idempotent across repeated middleware passes
    }
    if let Some(idx) = out.find("<head>") {
        out.insert_str(idx + "<head>".len(), &meta);
    } else {
        out.insert_str(0, &meta);
    }
    out
}

/// Outermost tower layer: rewrites HTML responses for trusted ingress
/// requests. Inert (a pure passthrough) for every other request, so direct
/// mode never observes it.
#[derive(Clone, Default)]
pub struct IngressRewriteLayer;

impl<S> Layer<S> for IngressRewriteLayer {
    type Service = IngressRewriteService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        IngressRewriteService { inner }
    }
}

#[derive(Clone)]
pub struct IngressRewriteService<S> {
    inner: S,
}

impl<S> Service<Request<Body>> for IngressRewriteService<S>
where
    S: Service<Request<Body>, Response = Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    S::Error: Send + 'static,
{
    type Response = Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<Response<Body>, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: Request<Body>) -> Self::Future {
        let mut inner = self.inner.clone();
        let base = request_ingress_base(&req);

        Box::pin(async move {
            let res = inner.call(req).await?;
            let Some(base) = base else { return Ok(res) };

            let is_html = res
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|ct| ct.starts_with("text/html"))
                .unwrap_or(false);
            if !is_html {
                return Ok(res);
            }

            let (parts, body) = res.into_parts();
            let body_bytes = match body.collect().await {
                Ok(collected) => collected.to_bytes(),
                Err(_) => return Ok(Response::from_parts(parts, Body::empty())),
            };
            let html = String::from_utf8_lossy(&body_bytes).to_string();
            let rewritten = rewrite_html(&html, &base);

            let mut new_res = Response::from_parts(parts, Body::from(rewritten));
            new_res.headers_mut().remove(header::CONTENT_LENGTH);
            Ok(new_res)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = "/api/hassio_ingress/tok3n";

    #[test]
    fn rooted_href_and_src_attributes_are_prefixed() {
        let html = r#"<head></head><body><a href="/ui/zones">z</a><script src="/./assets/app-dxh123.js"></script><img src="/api/collections/image?ref=r1"></body>"#;
        let out = rewrite_html(html, BASE);
        assert!(out.contains(r#"href="/api/hassio_ingress/tok3n/ui/zones""#));
        assert!(out.contains(r#"src="/api/hassio_ingress/tok3n/./assets/app-dxh123.js""#));
        assert!(out.contains(r#"src="/api/hassio_ingress/tok3n/api/collections/image?ref=r1""#));
    }

    #[test]
    fn absolute_data_and_protocol_relative_urls_are_untouched() {
        let html = r#"<a href="https://example.com/x">a</a><img src="data:image/png;base64,x"><script src="//cdn.example/x.js"></script>"#;
        let out = rewrite_html(html, BASE);
        assert!(out.contains(r#"href="https://example.com/x""#));
        assert!(out.contains(r#"src="data:image/png;base64,x""#));
        assert!(out.contains(r#"src="//cdn.example/x.js""#));
    }

    #[test]
    fn base_path_meta_is_injected_into_head_exactly_once() {
        let html = "<html><head><title>t</title></head><body></body></html>";
        let once = rewrite_html(html, BASE);
        assert!(once
            .contains(r#"<head><meta name="uhc-base-path" content="/api/hassio_ingress/tok3n">"#));
        let twice = rewrite_html(&once, BASE);
        assert_eq!(once, twice);
    }

    #[test]
    fn ingress_path_validation_refuses_injection_and_traversal() {
        assert!(valid_ingress_path("/api/hassio_ingress/AbC-12_3"));
        assert!(!valid_ingress_path("/"));
        assert!(!valid_ingress_path(""));
        assert!(!valid_ingress_path("no-slash"));
        assert!(!valid_ingress_path("/a/../b"));
        assert!(!valid_ingress_path("/a\"><script>"));
        assert!(!valid_ingress_path("/a b"));
        assert!(!valid_ingress_path(&format!("/{}", "a".repeat(300))));
    }

    #[test]
    fn supervisor_network_is_trusted_and_lan_is_not() {
        // Default rule (no override env var set in the test process; if a
        // developer machine sets it, these become the override's semantics,
        // so pin via ip_matches directly).
        assert!(ip_matches(
            &"172.30.32.2".parse().unwrap(),
            "172.30.32.0/23"
        ));
        assert!(ip_matches(
            &"172.30.33.7".parse().unwrap(),
            "172.30.32.0/23"
        ));
        assert!(!ip_matches(
            &"172.30.34.1".parse().unwrap(),
            "172.30.32.0/23"
        ));
        assert!(!ip_matches(
            &"192.168.1.50".parse().unwrap(),
            "172.30.32.0/23"
        ));
        assert!(ip_matches(&"127.0.0.1".parse().unwrap(), "127.0.0.0/8"));
        assert!(ip_matches(&"127.0.0.1".parse().unwrap(), "127.0.0.1"));
        assert!(!ip_matches(&"::1".parse().unwrap(), "172.30.32.0/23"));
        assert!(ip_matches(&"::1".parse().unwrap(), "::1"));
        assert!(!ip_matches(&"10.0.0.1".parse().unwrap(), "not-an-ip"));
        assert!(!ip_matches(&"10.0.0.1".parse().unwrap(), "10.0.0.0/33"));
    }

    #[test]
    fn request_gating_requires_env_peer_and_header() {
        // Without UHC_INGRESS the whole feature is inert regardless of
        // headers -- the direct-mode guarantee. (Env is process-global, so
        // this test only asserts the disabled path; the enabled path is
        // covered by the pure functions above and the live probe.)
        if !ingress_enabled() {
            let req = Request::builder()
                .header(INGRESS_PATH_HEADER, "/api/hassio_ingress/tok")
                .body(())
                .unwrap();
            assert!(request_ingress_base(&req).is_none());
        }
    }
}
