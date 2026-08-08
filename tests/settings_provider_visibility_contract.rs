//! Regression contract for provider visibility in Settings.
//!
//! A streaming-provider card is useful only when its adapter is enabled.
//! Keep the provider area and each card in the hydrated DOM with `hidden`
//! gates, rather than optimistically changing switch state or conditionally
//! inserting siblings after hydration. The next Settings page render must be
//! derived from the server-confirmed adapter settings.

const SETTINGS: &str = include_str!("../src/app/pages/settings.rs");

#[test]
fn provider_area_is_hidden_until_a_streaming_adapter_is_enabled() {
    let provider_area = &SETTINGS[SETTINGS
        .find("id: \"streaming-providers-anchor\"")
        .expect("Settings needs a stable streaming-provider hydration anchor")..];

    assert!(
        provider_area.contains(
            "hidden: !(spotify_enabled() || applemusic_enabled() || musicassistant_enabled())",
        ),
        "hide Streaming providers when no streaming provider is enabled"
    );
}

#[test]
fn each_provider_card_is_gated_by_its_own_feature_state() {
    let spotify_heading = SETTINGS
        .find("aria_labelledby: \"spotify-heading\"")
        .expect("Settings must contain the Spotify provider card");
    let apple_heading = SETTINGS
        .find("aria_labelledby: \"apple-music-heading\"")
        .expect("Settings must contain the Apple Music provider card");
    let spotify_card_start = spotify_heading.saturating_sub(200);
    let apple_card_start = apple_heading.saturating_sub(200);

    assert!(
        SETTINGS[spotify_card_start..spotify_heading].contains("hidden: !spotify_enabled()"),
        "Spotify configuration must disappear when Spotify is disabled"
    );
    assert!(
        SETTINGS[apple_card_start..apple_heading].contains("hidden: !applemusic_enabled()"),
        "Apple Music pairing must disappear when Apple Music is disabled"
    );
}

#[test]
fn musicassistant_setup_explains_the_first_success_and_keeps_advanced_http_secondary() {
    let card = &SETTINGS[SETTINGS
        .find("aria_labelledby: \"musicassistant-heading\"")
        .expect("Settings must contain the Music Assistant provider card")..];

    assert!(
        card.contains("Create a long-lived access token in Music Assistant, then paste it here."),
        "first-run setup must tell listeners where the required token comes from"
    );
    assert!(
        card.contains("Advanced connection options"),
        "the exceptional plaintext HTTP option must not compete with the default setup path"
    );
    assert!(
        card.contains("View discovered zones"),
        "a successful connection must lead to UHC's first-value destination"
    );
    assert!(
        card.contains("sm:col-span-2"),
        "the token field must span the complete desktop form instead of occupying a stray grid cell"
    );
}
