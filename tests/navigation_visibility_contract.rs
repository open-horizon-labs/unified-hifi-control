//! Regression contract for server-rendered feature navigation.
//!
//! Dioxus hydrates the server-rendered navigation before client-side fetches
//! complete. Therefore the server and WASM client must bootstrap navigation
//! from the same persisted settings snapshot. Rendering all feature tabs and
//! attempting to hide them later leaks disabled adapters and can change the
//! hydrated DOM topology.

const APP: &str = include_str!("../src/app/mod.rs");
const CONTEXT: &str = include_str!("../src/app/settings_context.rs");
const NAV: &str = include_str!("../src/app/components/nav.rs");

#[test]
fn navigation_visibility_is_bootstrapped_into_ssr_and_wasm() {
    assert!(
        APP.contains("uhc-navigation-visibility"),
        "App must serialize the server-confirmed navigation visibility snapshot into SSR markup"
    );
    assert!(
        CONTEXT.contains("load_app_settings"),
        "the server bootstrap must derive navigation visibility from persisted settings"
    );
    assert!(
        CONTEXT.contains("uhc-navigation-visibility"),
        "the WASM bootstrap must read the same SSR snapshot before Nav hydrates"
    );
}

#[test]
fn every_optional_tab_uses_the_shared_bootstrap_visibility() {
    for tab in ["hide_hqp", "hide_lms", "hide_spotify", "hide_knobs"] {
        assert!(
            NAV.contains(tab),
            "Nav must gate its optional {tab} tab from the shared settings context"
        );
    }
    assert!(
        CONTEXT.contains("NavigationVisibility"),
        "the one server-confirmed visibility model must cover every optional tab"
    );
}
