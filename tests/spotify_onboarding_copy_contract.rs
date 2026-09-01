//! Spotify setup copy is part of the safety and support contract.

const SETTINGS: &str = include_str!("../src/app/pages/settings.rs");

#[test]
fn spotify_onboarding_states_prerequisites_and_complete_setup_path() {
    for required in [
        "active Spotify Premium subscription",
        "Select Web API",
        "Users Management",
        "https://app.hiphi.audio/api/spotify/callback",
        "Copy the Client ID",
        "Connect Spotify",
    ] {
        assert!(
            SETTINGS.contains(required),
            "Spotify onboarding must contain {:?}",
            required
        );
    }
}

#[test]
fn spotify_onboarding_explains_the_cloud_credential_boundary_precisely() {
    for required in [
        "does not retain the authorization code",
        "PKCE verifier",
        "access and refresh tokens",
        "encrypted on this UHC server",
    ] {
        assert!(
            SETTINGS.contains(required),
            "Spotify credential-boundary copy must contain {:?}",
            required
        );
    }

    assert!(
        !SETTINGS.contains("HiPhi never receives the authorization code"),
        "HiPhi transiently receives the provider callback code and must not claim otherwise"
    );
}
