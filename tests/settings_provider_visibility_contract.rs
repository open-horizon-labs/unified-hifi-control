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
        provider_area.contains("hidden: !(spotify_enabled() || applemusic_enabled())"),
        "hide Streaming providers when neither Spotify nor Apple Music is enabled"
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
