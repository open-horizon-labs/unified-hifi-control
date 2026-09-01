//! #557 regression guard: exactly one `EventSource` per app session.
//!
//! Bug: the live install's console showed repeated "SSE: Creating
//! EventSource connection to /events" lines alongside the render/fetch
//! loop, suggesting a per-component (or per-remount) `EventSource` rather
//! than one shared connection. Investigation confirmed the connection
//! itself was already centralized correctly (`use_sse_provider()` opens
//! the browser's `EventSource` once, at the app root, above the router;
//! every page-level `use_sse()` call only reads the shared context, it
//! never opens a new connection) -- the actual loop was in `pages::library`'s
//! effects (see `src/app/pages/library.rs`'s `loop_regression_tests`
//! module), not in SSE connection management.
//!
//! This is a lint test, not a runtime one: driving an actual `EventSource`
//! open/reconnect count needs a browser (or a wasm test runner with a DOM),
//! which is not available in this sandbox. It instead scans the source for
//! the two invariants a second `EventSource` call site would violate, so a
//! future page that opens its own connection (defeating the single shared
//! one) fails CI immediately rather than only showing up as console churn
//! in a live install.

use std::fs;

/// `EventSource::new` must be constructed in exactly one place:
/// `use_sse_provider()`, called once at the app root (`src/app/mod.rs`).
/// A second call site -- e.g. a page reaching for its own connection
/// instead of `use_sse()` -- reproduces the reconnect churn #557 reports.
#[test]
fn lint_eventsource_constructed_in_exactly_one_place() {
    let src = fs::read_to_string("src/app/sse.rs").expect("failed to read src/app/sse.rs");
    let count = src.matches("EventSource::new(").count();
    assert_eq!(
        count, 1,
        "REGRESSION: EventSource::new(...) must appear exactly once, inside \
         use_sse_provider(). Found {count} call sites in src/app/sse.rs -- a \
         second one means some page opens its own SSE connection instead of \
         sharing the app-root one, which is how #557's reconnect churn happened."
    );
}

/// `use_sse_provider()` -- which owns the one `EventSource` -- must be
/// called exactly once, from the app root, not from any individual page.
/// Every page instead calls the read-only `use_sse()`.
#[test]
fn lint_use_sse_provider_called_once_from_app_root() {
    let mod_rs = fs::read_to_string("src/app/mod.rs").expect("failed to read src/app/mod.rs");
    assert!(
        mod_rs.contains("use_sse_provider();"),
        "REGRESSION: src/app/mod.rs must call use_sse_provider() at the app root \
         so every page shares one EventSource."
    );

    for (path, page_should_not_call_provider) in [
        ("src/app/pages/library.rs", true),
        ("src/app/pages/zones.rs", true),
        ("src/app/pages/knobs.rs", true),
        ("src/app/pages/settings.rs", true),
        ("src/app/pages/spotify.rs", true),
        ("src/app/pages/hqplayer.rs", true),
        ("src/app/pages/lms.rs", true),
    ] {
        let src = fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {path}: {e}"));
        if page_should_not_call_provider {
            assert!(
                !src.contains("use_sse_provider("),
                "REGRESSION: {path} calls use_sse_provider(), which would open a \
                 second EventSource. Pages must call use_sse() (read-only) instead."
            );
        }
    }
}
