//! Regression guard for Settings feature switches.
//!
//! A Dioxus fullstack page hydrates server-rendered HTML in the browser. Do
//! not optimistically toggle adapter signals or dynamically hide provider
//! cards after a click: a partial/stale hydration can leave the browser showing
//! a state that UHC never persisted. Instead, submit the complete setting to
//! `/api/settings`, let the server run its adapter lifecycle transaction, then
//! reload Settings from that confirmed state.

#[test]
fn feature_switches_are_server_confirmed_not_optimistic() {
    let source = include_str!("../src/app/pages/settings.rs");

    let persist = source
        .find("fn persist_settings_then_reload")
        .expect("Settings feature writes need a dedicated server-confirmed path");
    let persist_body = &source[persist
        ..source[persist..]
            .find("#[component]")
            .expect("persistence helper must end before the first component")
            + persist];
    let post = persist_body
        .find("\"/api/settings\"")
        .expect("feature writes must use the Settings API");
    let reload = persist_body
        .find("reload_settings_page()")
        .expect("feature writes must reload the server-confirmed Settings state");
    assert!(
        post < reload,
        "post the setting before reloading; never claim a feature changed locally first"
    );

    for optimistic_mutation in [
        "roon_enabled.toggle()",
        "openhome_enabled.toggle()",
        "upnp_enabled.toggle()",
        "lms_enabled.toggle()",
        "hqplayer_enabled.toggle()",
        "spotify_enabled.toggle()",
        "applemusic_enabled.toggle()",
        "hide_knobs.toggle()",
    ] {
        assert!(
            !source.contains(optimistic_mutation),
            "{optimistic_mutation} reintroduces optimistic Settings UI. Submit through \
             persist_settings_then_reload so the next render reflects UHC's confirmed state."
        );
    }
}

#[test]
fn every_feature_switch_hydrates_from_the_server_snapshot() {
    let source = include_str!("../src/app/pages/settings.rs");

    for adapter in [
        "roon",
        "lms",
        "openhome",
        "upnp",
        "hqplayer",
        "spotify",
        "applemusic",
    ] {
        assert!(
            source.contains(&format!("initial_settings.adapters.{adapter}")),
            "{adapter} must hydrate from the app-root settings snapshot, not a browser default that can disagree with the rendered feature state"
        );
    }

    assert!(
        source.contains("initial_settings.hide_knobs_page"),
        "Knobs visibility must hydrate from the same confirmed app-root snapshot as adapters"
    );
}

#[test]
fn settings_bootstrap_is_available_at_app_root_before_route_hydration() {
    let app = include_str!("../src/app/mod.rs");
    let settings = include_str!("../src/app/pages/settings.rs");
    let context = include_str!("../src/app/settings_context.rs");

    assert!(
        app.contains("uhc-settings-bootstrap"),
        "the complete settings snapshot must be emitted at the app root before route hydration"
    );
    assert!(
        settings.contains("initial_app_settings") && context.contains("uhc-settings-bootstrap"),
        "Settings must read the app-root bootstrap rather than a route-local marker"
    );
    assert!(
        !settings.contains("settings-adapter-hydration"),
        "a route-local settings marker can be absent when the route first hydrates"
    );
}

#[test]
fn spotify_redirect_uri_has_one_hydration_safe_initial_value() {
    let source = include_str!("../src/app/pages/settings.rs");

    assert!(
        source.contains("const DEFAULT_SPOTIFY_REDIRECT_URI"),
        "SSR and WASM must share one initial redirect URI while Settings hydrates"
    );
    assert!(
        !source.contains("window().location().origin"),
        "reading the browser origin during the first render can disagree with SSR"
    );
}
