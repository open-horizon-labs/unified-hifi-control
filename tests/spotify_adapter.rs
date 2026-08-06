use std::sync::{Arc, Mutex};

use axum::{
    extract::State,
    http::{Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use tokio::net::TcpListener;
use unified_hifi_control::adapters::spotify::{SpotifyAdapter, SpotifyDevice, SpotifyToken};
use unified_hifi_control::adapters::{AdapterCommand, AdapterLogic, Startable};
use unified_hifi_control::bus::create_bus;

#[derive(Clone, Default)]
struct MockSpotifyState {
    requests: Arc<Mutex<Vec<String>>>,
}

async fn spotify_mock(State(state): State<MockSpotifyState>, method: Method, uri: Uri) -> Response {
    state
        .requests
        .lock()
        .expect("mock lock")
        .push(format!("{} {}", method, uri));

    match (method, uri.path()) {
        (Method::GET, "/me/player/devices") => Json(serde_json::json!({
            "devices": [{
                "id": "device-1",
                "name": "Kitchen",
                "type": "Speaker",
                "is_active": true,
                "is_restricted": false,
                "volume_percent": 42
            }]
        }))
        .into_response(),
        (Method::GET, "/me/player") => Json(serde_json::json!({
            "is_playing": true,
            "progress_ms": 1200,
            "device": {"id": "device-1"},
            "item": {
                "name": "Song",
                "artists": [{"name": "Artist"}],
                "album": {"name": "Album", "images": []},
                "duration_ms": 180000
            }
        }))
        .into_response(),
        _ => StatusCode::NO_CONTENT.into_response(),
    }
}

async fn mock_server() -> (String, MockSpotifyState, tokio::task::JoinHandle<()>) {
    let state = MockSpotifyState::default();
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("mock bind");
    let address = listener.local_addr().expect("mock address");
    let router = Router::new()
        .route("/me/player/devices", any(spotify_mock))
        .route("/me/player", any(spotify_mock))
        .route("/me/player/{*path}", any(spotify_mock))
        .with_state(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(listener, router).await.expect("mock serve");
    });
    (format!("http://{}", address), state, handle)
}

#[test]
fn spotify_device_maps_to_prefixed_zone() {
    let device = SpotifyDevice {
        id: "kitchen-device".to_string(),
        name: "Kitchen".to_string(),
        device_type: "Speaker".to_string(),
        is_active: true,
        is_restricted: false,
        volume_percent: Some(42),
    };

    let zone = device.to_zone(None);
    assert_eq!(zone.zone_id, "spotify:kitchen-device");
    assert_eq!(zone.source, "spotify");
    assert_eq!(zone.zone_name, "Kitchen");
    assert_eq!(zone.volume_control.as_ref().map(|v| v.value), Some(42.0));
    assert!(zone.is_controllable);
}

#[test]
fn restricted_spotify_device_is_not_controllable() {
    let device = SpotifyDevice {
        id: "restricted".to_string(),
        name: "Restricted".to_string(),
        device_type: "Speaker".to_string(),
        is_active: false,
        is_restricted: true,
        volume_percent: None,
    };
    let zone = device.to_zone(None);
    assert!(!zone.is_controllable);
    assert!(!zone.is_play_allowed);
    assert!(zone.volume_control.is_none());
}

#[tokio::test]
async fn adapter_without_token_cannot_start() {
    let adapter = SpotifyAdapter::new(create_bus());
    assert!(!adapter.can_start().await);
    let result = adapter
        .handle_command("spotify:device", AdapterCommand::Play)
        .await
        .expect("unsupported adapter command should be a response");
    assert!(!result.success);
    assert!(result.error.expect("error").contains("token"));
}

#[tokio::test]
async fn update_discovers_devices_and_commands_target_device() {
    let (base_url, mock, server) = mock_server().await;
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter
        .set_token(SpotifyToken {
            access_token: "access".to_string(),
            refresh_token: None,
            expires_at: Some(u64::MAX),
        })
        .await;

    adapter.update().await.expect("Spotify poll");
    let devices = adapter.get_devices().await;
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0].id, "device-1");

    let response = adapter
        .handle_command("spotify:device-1", AdapterCommand::Play)
        .await
        .expect("Spotify command response");
    assert!(
        response.success,
        "Spotify command failed: {:?}",
        response.error
    );

    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(requests
        .iter()
        .any(|request| { request == "PUT /me/player/play?device_id=device-1" }));
    let volume = adapter
        .handle_command("spotify:device-1", AdapterCommand::VolumeRelative(8))
        .await
        .expect("Spotify volume response");
    assert!(volume.success, "Spotify volume failed: {:?}", volume.error);
    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(requests.iter().any(|request| {
        request == "PUT /me/player/volume?volume_percent=50&device_id=device-1"
    }));
    server.abort();
}

#[test]
fn token_expiry_is_detected() {
    let token = SpotifyToken {
        access_token: "access".to_string(),
        refresh_token: Some("refresh".to_string()),
        expires_at: Some(0),
    };
    assert!(token.is_expired(1));
}
