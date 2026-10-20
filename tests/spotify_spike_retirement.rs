//! Owner-approved retirement of the pre-HiPhi Spotify SSH relay spike (#651).
use std::path::Path;

#[test]
fn ssh_relay_spike_is_absent_not_merely_hidden() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for file in [
        "src/api/spotify_tunnel.rs",
        "src/api/spotify_callback_listener.rs",
    ] {
        assert!(
            !root.join(file).exists(),
            "remove obsolete relay implementation: {file}"
        );
    }
    for file in [
        "src/main.rs",
        "src/api/provider_auth.rs",
        "src/app/pages/settings.rs",
        "src/app/api.rs",
        "tests/fixtures/api_routes.txt",
    ] {
        let source = std::fs::read_to_string(root.join(file)).unwrap();
        for obsolete in [
            "spotify_tunnel",
            "SpotifyTunnelStatus",
            "spotify_callback_listener",
            "/api/providers/spotify/tunnel/",
            "pinggy",
            "localhost.run",
        ] {
            assert!(
                !source.contains(obsolete),
                "{file} still contains {obsolete}"
            );
        }
    }
    let docker = std::fs::read_to_string(root.join("Dockerfile.release")).unwrap();
    assert!(
        !docker.contains("openssh-client"),
        "do not ship SSH just for the retired spike"
    );
}

#[test]
fn hiphi_spotify_authorization_remains_wired() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let auth = std::fs::read_to_string(root.join("src/api/provider_auth.rs")).unwrap();
    assert!(auth.contains("hiphi_spotify_authorization_url("));
    assert!(auth.contains("pub async fn accept_cloud_spotify_callback("));
    let connector = std::fs::read_to_string(root.join("src/cloud_connector/runtime.rs")).unwrap();
    assert!(connector.contains("accept_cloud_spotify_callback"));
    let settings = std::fs::read_to_string(root.join("src/app/pages/settings.rs")).unwrap();
    assert!(settings.contains("HiPhi Cloud routes the callback response"));
}
