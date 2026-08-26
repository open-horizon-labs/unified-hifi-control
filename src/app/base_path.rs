//! Runtime base-path resolution for reverse-proxy subpath deployments (#581).
//!
//! Home Assistant Ingress serves the UI under a per-session token path
//! (`/api/hassio_ingress/<token>/...`). The Supervisor proxy strips that
//! prefix before forwarding, so the *server* keeps its absolute routes --
//! but every URL the *browser* issues (fetches, the SSE EventSource,
//! artwork `<img src>`, router history pushes) must carry the prefix or it
//! escapes the proxy and dies on Home Assistant's own 401.
//!
//! The server advertises the prefix in ONE place: a
//! `<meta name="uhc-base-path" content="/api/hassio_ingress/<token>">` tag
//! injected into SSR HTML by `crate::api::ingress::IngressRewriteLayer`
//! (from the request's `X-Ingress-Path` header). This module is the ONE
//! client-side consumer: everything that needs an origin-absolute path
//! turned into a browser-issuable URL calls [`href`]. Outside a proxied
//! deployment the meta tag is absent, the base is empty, and [`href`] is the
//! identity function -- direct-mode behavior is unchanged.
//!
//! Why the client prefixes API-returned artwork paths (rather than the
//! server emitting base-relative ones): the `image` fields of
//! `/api/collections` and `/api/search` are forwarded verbatim from the
//! `hifi_collections`/`hifi_search` MCP tool envelopes (see
//! `src/api/browse.rs`), whose consumers include MCP clients that connect to
//! the origin directly and need origin-absolute paths. The wire shape
//! therefore stays origin-absolute, and this module maps it for the one
//! consumer that sits behind a prefix -- through the same resolver the fetch
//! helpers already use, so there is still exactly one prepend point.

/// The meta tag name `crate::api::ingress` injects and this module reads.
pub const BASE_PATH_META: &str = "uhc-base-path";

/// The active base path: `""` in a direct deployment, or a normalized
/// prefix like `/api/hassio_ingress/<token>` (leading `/`, no trailing `/`)
/// behind an ingress proxy. Cached after the first read -- the tag is part
/// of the served document and cannot change within a page's lifetime.
#[cfg(target_arch = "wasm32")]
pub fn base_path() -> String {
    thread_local! {
        static CACHE: std::cell::OnceCell<String> = const { std::cell::OnceCell::new() };
    }
    CACHE.with(|cell| cell.get_or_init(|| read_meta().unwrap_or_default()).clone())
}

/// SSR renders origin-absolute URLs; the ingress layer rewrites the emitted
/// HTML, so the server-side base is always empty.
#[cfg(not(target_arch = "wasm32"))]
pub fn base_path() -> String {
    String::new()
}

#[cfg(target_arch = "wasm32")]
fn read_meta() -> Option<String> {
    let document = web_sys::window()?.document()?;
    let element = document
        .query_selector(&format!("meta[name=\"{BASE_PATH_META}\"]"))
        .ok()??;
    let content = element.get_attribute("content")?;
    normalize(&content)
}

/// Normalize a raw meta value into a usable prefix: must start with `/`,
/// trailing slashes dropped, and a bare `/` (or anything else degenerate)
/// means "no prefix".
#[allow(dead_code)] // wasm-only caller; kept target-neutral so tests cover it
fn normalize(raw: &str) -> Option<String> {
    let trimmed = raw.trim().trim_end_matches('/');
    (trimmed.starts_with('/') && trimmed.len() > 1).then(|| trimmed.to_string())
}

/// Turn an origin-absolute path (`/api/...`, `/events`, artwork URLs) into
/// the URL the browser must actually issue. Identity when there is no base
/// path, for non-path inputs (full `scheme://` URLs, `data:` URLs), and for
/// protocol-relative `//host` URLs.
pub fn href(path: &str) -> String {
    href_with_base(&base_path(), path)
}

fn href_with_base(base: &str, path: &str) -> String {
    if base.is_empty() || !path.starts_with('/') || path.starts_with("//") {
        return path.to_string();
    }
    format!("{base}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn direct_mode_is_the_identity() {
        assert_eq!(href_with_base("", "/api/settings"), "/api/settings");
        assert_eq!(href_with_base("", "/events"), "/events");
    }

    #[test]
    fn ingress_base_prefixes_origin_absolute_paths() {
        let base = "/api/hassio_ingress/abc123";
        assert_eq!(
            href_with_base(base, "/api/collections/image?ref=r1"),
            "/api/hassio_ingress/abc123/api/collections/image?ref=r1"
        );
        assert_eq!(
            href_with_base(base, "/events"),
            "/api/hassio_ingress/abc123/events"
        );
    }

    #[test]
    fn non_path_urls_are_never_touched() {
        let base = "/api/hassio_ingress/abc123";
        assert_eq!(
            href_with_base(base, "https://ma.example/image.jpg"),
            "https://ma.example/image.jpg"
        );
        assert_eq!(
            href_with_base(base, "data:image/png;base64,xyz"),
            "data:image/png;base64,xyz"
        );
        assert_eq!(
            href_with_base(base, "//cdn.example/x.js"),
            "//cdn.example/x.js"
        );
        assert_eq!(href_with_base(base, "relative/path"), "relative/path");
    }

    #[test]
    fn normalize_rejects_degenerate_values_and_trims_trailing_slash() {
        assert_eq!(
            normalize("/api/hassio_ingress/abc/"),
            Some("/api/hassio_ingress/abc".to_string())
        );
        assert_eq!(normalize("/"), None);
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("no-leading-slash"), None);
    }
}
