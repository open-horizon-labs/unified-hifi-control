use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    body::Bytes,
    extract::State,
    http::{header::CONTENT_TYPE, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
    Json, Router,
};
use tokio::net::TcpListener;
use unified_hifi_control::adapters::spotify::{
    SpotifyAdapter, SpotifyDevice, SpotifyToken, SpotifyTokenRefresher,
};
use unified_hifi_control::adapters::{AdapterCommand, AdapterLogic, LibraryAdapter, Startable};
use unified_hifi_control::bus::RepeatMode;
use unified_hifi_control::bus::{create_bus, BusEvent};
use unified_hifi_control::mcp::capabilities::{support, Capability, Support};
use unified_hifi_control::mcp::routing::ZoneTarget;

const SPOTIFY_2026_CONTRACT: &str = include_str!("fixtures/spotify/web_api_2026_contract.json");
const SPOTIFY_2026_DEVICES: &str = include_str!("fixtures/spotify/devices_2026.json");
const SPOTIFY_2026_PLAYLIST: &str = include_str!("fixtures/spotify/playlist_2026.json");
const SPOTIFY_2026_SEARCH: &str = include_str!("fixtures/spotify/search_2026.json");
const SPOTIFY_2026_QUOTA: &str = include_str!("fixtures/spotify/quota_exceeded_2026.json");
const SPOTIFY_2026_RATE_LIMIT: &str = include_str!("fixtures/spotify/rate_limited_2026.json");

fn spotify_token() -> SpotifyToken {
    SpotifyToken {
        access_token: "access".to_string(),
        refresh_token: None,
        expires_at: Some(u64::MAX),
    }
}

struct RefreshingToken;

#[async_trait]
impl SpotifyTokenRefresher for RefreshingToken {
    async fn refresh(&self, _current: &SpotifyToken) -> anyhow::Result<SpotifyToken> {
        Ok(SpotifyToken {
            access_token: "fresh".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(u64::MAX),
        })
    }
}

#[derive(Clone, Default)]
struct MockSpotifyState {
    requests: Arc<Mutex<Vec<String>>>,
    bodies: Arc<Mutex<Vec<String>>>,
    devices_response: Arc<Mutex<Option<(StatusCode, String)>>>,
    search_response: Arc<Mutex<Option<(StatusCode, String)>>>,
}

fn json_response(status: StatusCode, body: String) -> Response {
    (status, [(CONTENT_TYPE, "application/json")], body).into_response()
}

async fn spotify_mock(
    State(state): State<MockSpotifyState>,
    method: Method,
    uri: Uri,
    body: Bytes,
) -> Response {
    state
        .requests
        .lock()
        .expect("mock lock")
        .push(format!("{} {}", method, uri));
    if !body.is_empty() {
        state
            .bodies
            .lock()
            .expect("mock body lock")
            .push(String::from_utf8_lossy(&body).into_owned());
    }

    if method == Method::GET && uri.path() == "/me/player/devices" {
        if let Some((status, body)) = state
            .devices_response
            .lock()
            .expect("devices response lock")
            .clone()
        {
            return json_response(status, body);
        }
    }
    if method == Method::GET && uri.path() == "/search" {
        if let Some((status, body)) = state
            .search_response
            .lock()
            .expect("search response lock")
            .clone()
        {
            return json_response(status, body);
        }
    }

    match (method, uri.path()) {
        (Method::GET, "/me/player/devices") => Json(serde_json::json!({
            "devices": [{
                "id": "device-1",
                "name": "Kitchen",
                "type": "Speaker",
                "is_active": true,
                "is_restricted": false,
                "supports_volume": true,
                "volume_percent": 42
            }]
        }))
        .into_response(),
        (Method::GET, "/me/player") => Json(serde_json::json!({
            "is_playing": true,
            "repeat_state": "context",
            "shuffle_state": false,
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
        (Method::GET, "/me/player/queue") => Json(serde_json::json!({
            "currently_playing": {
                "uri": "spotify:track:current",
                "name": "Song",
                "artists": [{"name": "Artist"}],
                "album": {"name": "Album", "images": []},
                "duration_ms": 180000
            },
            "queue": [{
                "uri": "spotify:track:queued",
                "name": "Queued Song",
                "artists": [{"name": "Queued Artist"}],
                "album": {"name": "Queued Album", "images": []},
                "duration_ms": 200000
            }, {
                "uri": "spotify:episode:queued-episode",
                "name": "Queued Episode",
                "show": {"name": "Queued Show"},
                "duration_ms": 240000
            }]
        }))
        .into_response(),
        (Method::GET, "/search") => json_response(StatusCode::OK, SPOTIFY_2026_SEARCH.to_string()),
        (Method::GET, "/me") => Json(serde_json::json!({
            "id": "account-1",
            "display_name": "Muness Castle",
            "email": "muness@example.test"
        }))
        .into_response(),
        (Method::GET, "/me/playlists") => Json(serde_json::json!({
            "href": "https://api.spotify.test/me/playlists",
            "limit": 2,
            "next": null,
            "offset": 0,
            "previous": null,
            "total": 1,
            "items": [{
                "id": "playlist-1",
                "name": "Workout",
                "uri": "spotify:playlist:playlist-1",
                "description": "Keep moving",
                "public": true,
                "collaborative": false,
                "images": []
            }]
        }))
        .into_response(),
        (Method::GET, "/playlists/playlist-1/items") => Json(serde_json::json!({
            "href": "https://api.spotify.test/playlists/playlist-1/items",
            "limit": 2,
            "next": null,
            "offset": 0,
            "previous": null,
            "total": 1,
            "items": [{
                "added_at": "2026-08-06T00:00:00Z",
                "item": {
                    "uri": "spotify:track:track-1",
                    "name": "Song",
                    "artists": [{"name": "Artist"}],
                    "album": {"name": "Album", "images": []},
                    "duration_ms": 180000
                }
            }]
        }))
        .into_response(),
        (Method::GET, "/me/tracks") => Json(serde_json::json!({
            "href": "https://api.spotify.test/me/tracks",
            "limit": 2,
            "next": null,
            "offset": 0,
            "previous": null,
            "total": 1,
            "items": [{
                "added_at": "2026-08-06T00:00:00Z",
                "track": {
                    "uri": "spotify:track:track-1",
                    "name": "Song",
                    "artists": [{"name": "Artist"}],
                    "album": {"name": "Album", "images": []},
                    "duration_ms": 180000
                }
            }]
        }))
        .into_response(),
        (Method::GET, "/me/library/contains") => {
            Json(serde_json::json!([true, false])).into_response()
        }
        (Method::POST, "/me/playlists") => {
            json_response(StatusCode::OK, SPOTIFY_2026_PLAYLIST.to_string())
        }
        (Method::PUT, "/playlists/playlist-1") => StatusCode::NO_CONTENT.into_response(),
        (Method::POST, "/playlists/playlist-1/items") => Json(serde_json::json!({
            "snapshot_id": "snapshot-add"
        }))
        .into_response(),
        (Method::PUT, "/playlists/playlist-1/items") => Json(serde_json::json!({
            "snapshot_id": "snapshot-edit"
        }))
        .into_response(),
        (Method::DELETE, "/playlists/playlist-1/items") => Json(serde_json::json!({
            "snapshot_id": "snapshot-remove"
        }))
        .into_response(),
        (Method::PUT, "/me/library") => StatusCode::NO_CONTENT.into_response(),
        (Method::DELETE, "/me/library") => StatusCode::NO_CONTENT.into_response(),
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
        .route("/search", any(spotify_mock))
        .route("/browse/{*path}", any(spotify_mock))
        .route("/me", any(spotify_mock))
        .route("/me/{*path}", any(spotify_mock))
        .route("/playlists/{*path}", any(spotify_mock))
        .route("/users/{*path}", any(spotify_mock))
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
        supports_volume: true,
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
        supports_volume: false,
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
async fn expired_persisted_token_can_start_when_refresh_is_available() {
    let adapter = SpotifyAdapter::new(create_bus());
    adapter
        .set_token(SpotifyToken {
            access_token: "expired".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(0),
        })
        .await;
    adapter.set_token_refresher(Arc::new(RefreshingToken)).await;
    assert!(adapter.can_start().await);
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
fn spotify_playback_maps_repeat_and_shuffle_state() {
    let playback: unified_hifi_control::adapters::spotify::SpotifyPlayback =
        serde_json::from_value(serde_json::json!({
            "is_playing": true,
            "repeat_state": "track",
            "shuffle_state": true,
            "item": {
                "name": "Song",
                "artists": [{"name": "Artist"}],
                "album": {"name": "Album", "images": []}
            }
        }))
        .expect("Spotify playback payload");
    let now_playing = playback.to_now_playing().expect("track metadata");
    assert_eq!(now_playing.repeat_mode, Some(RepeatMode::One));
    assert_eq!(now_playing.shuffle, Some(true));
}

#[tokio::test]
async fn repeat_and_shuffle_commands_target_spotify_device() {
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

    for (command, expected) in [
        (
            AdapterCommand::SetRepeat(RepeatMode::Off),
            "PUT /me/player/repeat?state=off&device_id=device-1",
        ),
        (
            AdapterCommand::SetRepeat(RepeatMode::All),
            "PUT /me/player/repeat?state=context&device_id=device-1",
        ),
        (
            AdapterCommand::SetRepeat(RepeatMode::One),
            "PUT /me/player/repeat?state=track&device_id=device-1",
        ),
        (
            AdapterCommand::SetShuffle(true),
            "PUT /me/player/shuffle?state=true&device_id=device-1",
        ),
        (
            AdapterCommand::SetShuffle(false),
            "PUT /me/player/shuffle?state=false&device_id=device-1",
        ),
    ] {
        let result = adapter
            .handle_command("spotify:device-1", command)
            .await
            .expect("Spotify mode command response");
        assert!(result.success, "mode command failed: {:?}", result.error);
        assert!(
            mock.requests
                .lock()
                .expect("mock lock")
                .iter()
                .any(|request| request == expected),
            "expected request {expected:?}"
        );
    }
    server.abort();
}

#[tokio::test]
async fn queue_methods_read_queue_and_add_uri_for_target_device() {
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

    let queue = adapter
        .get_queue("spotify:device-1")
        .await
        .expect("Spotify queue read");
    assert_eq!(
        queue
            .currently_playing
            .as_ref()
            .map(|item| item.name.as_str()),
        Some("Song")
    );
    assert_eq!(queue.queue.len(), 2);
    assert_eq!(queue.queue[0].uri, "spotify:track:queued");
    assert_eq!(
        queue.queue[1].show.as_ref().map(|show| show.name.as_str()),
        Some("Queued Show")
    );

    adapter
        .add_to_queue("spotify:device-1", "spotify:track:queued")
        .await
        .expect("Spotify queue add");

    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(requests
        .iter()
        .any(|request| request == "GET /me/player/queue"));
    assert!(requests.iter().any(|request| {
        request == "POST /me/player/queue?uri=spotify%3Atrack%3Aqueued&device_id=device-1"
    }));
    server.abort();
}

#[tokio::test]
async fn queue_methods_reject_unknown_devices_before_provider_call() {
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
    let result = adapter.get_queue("spotify:missing").await;
    assert!(result
        .expect_err("unknown device must fail")
        .to_string()
        .contains("not currently available"));
    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(!requests
        .iter()
        .any(|request| request == "GET /me/player/queue"));
    server.abort();
}

#[tokio::test]
async fn add_to_queue_rejects_non_track_or_episode_uri() {
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
    let result = adapter
        .add_to_queue("spotify:device-1", "spotify:album:album-1")
        .await;
    let error = result.expect_err("album URI must not be queued directly");
    assert!(error
        .to_string()
        .contains("spotify:track: or spotify:episode:"));
    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(!requests
        .iter()
        .any(|request| request.starts_with("POST /me/player/queue")));
    server.abort();
}

#[tokio::test]
async fn search_returns_track_album_and_artist_uri_targets() {
    let (base_url, mock, server) = mock_server().await;
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter
        .set_token(SpotifyToken {
            access_token: "access".to_string(),
            refresh_token: None,
            expires_at: Some(u64::MAX),
        })
        .await;

    let results = adapter.search("Song", 10).await.expect("Spotify search");
    assert_eq!(results.len(), 3);
    assert_eq!(results[0].title, "Song");
    assert_eq!(results[0].uri, "spotify:track:track-1");
    assert_eq!(results[1].uri, "spotify:album:album-1");
    assert_eq!(results[2].uri, "spotify:artist:artist-1");
    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(requests.iter().any(|request| {
        request.starts_with("GET /search?")
            && request.contains("q=Song")
            && request.contains("type=track%2Calbum%2Cartist")
    }));
    server.abort();
}

#[tokio::test]
async fn official_2026_device_shape_skips_null_ids_and_honors_supports_volume() {
    let (base_url, mock, server) = mock_server().await;
    *mock.devices_response.lock().expect("devices response lock") =
        Some((StatusCode::OK, SPOTIFY_2026_DEVICES.to_string()));
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter.set_token(spotify_token()).await;

    adapter
        .update()
        .await
        .expect("the current Spotify device response must be accepted");
    let devices = adapter.get_devices().await;
    assert_eq!(devices.len(), 2);
    let fixed = devices
        .iter()
        .find(|device| device.id == "fixed-device")
        .expect("fixed-volume device");
    let controllable = devices
        .iter()
        .find(|device| device.id == "controllable-device")
        .expect("volume-capable device");
    assert!(fixed.to_zone(None).volume_control.is_none());
    assert_eq!(
        controllable
            .to_zone(None)
            .volume_control
            .as_ref()
            .map(|volume| volume.value),
        Some(42.0)
    );
    let response = adapter
        .handle_command("spotify:fixed-device", AdapterCommand::VolumeAbsolute(20))
        .await
        .expect("fixed device refusal");
    assert!(!response.success);
    assert!(response.error.expect("error").contains("does not support"));
    assert!(!mock
        .requests
        .lock()
        .expect("mock lock")
        .iter()
        .any(|request| request.contains("device_id=fixed-device")));
    server.abort();
}

#[tokio::test]
async fn search_clamps_every_request_to_the_official_2026_maximum() {
    let contract: serde_json::Value =
        serde_json::from_str(SPOTIFY_2026_CONTRACT).expect("Spotify contract fixture");
    let maximum = contract["search_limit_max"]
        .as_u64()
        .expect("search maximum");
    let (base_url, mock, server) = mock_server().await;
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter.set_token(spotify_token()).await;

    adapter
        .search("Song", 50)
        .await
        .expect("Spotify search response");
    let requests = mock.requests.lock().expect("mock lock").clone();
    let search_request = requests
        .iter()
        .find(|request| request.starts_with("GET /search?"))
        .expect("search request");
    assert!(search_request.contains(&format!("limit={maximum}")));
    assert!(!search_request.contains("limit=50"));
    server.abort();
}

#[tokio::test]
async fn current_playlist_shape_and_content_creation_use_me_endpoint() {
    let contract: serde_json::Value =
        serde_json::from_str(SPOTIFY_2026_CONTRACT).expect("Spotify contract fixture");
    let create_path = contract["playlist_create_path"]
        .as_str()
        .expect("playlist create path");
    let items_field = contract["playlist_items_field"]
        .as_str()
        .expect("playlist items field");
    let (base_url, mock, server) = mock_server().await;
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter.set_token(spotify_token()).await;

    let created = LibraryAdapter::content(
        &adapter,
        "create_playlist",
        &serde_json::json!({"name": "Official Current"}),
    )
    .await
    .expect("content playlist creation");
    assert_eq!(created[items_field]["total"], 0);
    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(requests
        .iter()
        .any(|request| request == &format!("POST {create_path}")));
    assert!(!requests.iter().any(|request| request.contains("/users/")));
    server.abort();
}

#[tokio::test]
async fn removed_development_mode_browse_actions_refuse_without_provider_calls() {
    let contract: serde_json::Value =
        serde_json::from_str(SPOTIFY_2026_CONTRACT).expect("Spotify contract fixture");
    let removed = contract["removed_development_mode_browse_actions"]
        .as_array()
        .expect("removed browse actions");
    let (base_url, mock, server) = mock_server().await;
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter.set_token(spotify_token()).await;

    for action in removed {
        let action = action.as_str().expect("action name");
        let error = LibraryAdapter::content(
            &adapter,
            action,
            &serde_json::json!({"category_id": "workout"}),
        )
        .await
        .expect_err("removed Development Mode browse action must be refused");
        let message = error.to_string();
        assert!(message.contains("Development Mode"), "{action}: {message}");
    }
    assert!(adapter
        .browse_categories(10, 0, None, None)
        .await
        .expect_err("removed category browse must be refused")
        .to_string()
        .contains("Development Mode"));
    assert!(adapter
        .browse_category_playlists("workout", 10, 0, None)
        .await
        .expect_err("removed category-playlist browse must be refused")
        .to_string()
        .contains("Development Mode"));
    assert!(adapter
        .browse_featured_playlists(10, 0, None, None, None)
        .await
        .expect_err("removed featured browse must be refused")
        .to_string()
        .contains("Development Mode"));
    assert!(adapter
        .browse_new_releases(10, 0, None)
        .await
        .expect_err("removed new-releases browse must be refused")
        .to_string()
        .contains("Development Mode"));
    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(!requests.iter().any(|request| request.contains("/browse/")));
    server.abort();
}

#[tokio::test]
async fn quota_exceeded_is_classified_without_exposing_the_provider_body() {
    let (base_url, mock, server) = mock_server().await;
    *mock.search_response.lock().expect("search response lock") = Some((
        StatusCode::TOO_MANY_REQUESTS,
        SPOTIFY_2026_QUOTA.to_string(),
    ));
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter.set_token(spotify_token()).await;

    let message = adapter
        .search("Song", 10)
        .await
        .expect_err("quota exhaustion must fail")
        .to_string();
    assert!(message.contains("QUOTA_EXCEEDED"), "{message}");
    assert!(message.to_ascii_lowercase().contains("quota"), "{message}");
    assert!(
        !message.contains("provider-secret-diagnostic-marker"),
        "{message}"
    );
    server.abort();
}

#[tokio::test]
async fn ordinary_rate_limit_is_distinct_and_does_not_expose_the_provider_body() {
    let (base_url, mock, server) = mock_server().await;
    *mock.search_response.lock().expect("search response lock") = Some((
        StatusCode::TOO_MANY_REQUESTS,
        SPOTIFY_2026_RATE_LIMIT.to_string(),
    ));
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter.set_token(spotify_token()).await;

    let message = adapter
        .search("Song", 10)
        .await
        .expect_err("rate limiting must fail")
        .to_string();
    assert!(
        message.to_ascii_lowercase().contains("rate limit"),
        "{message}"
    );
    assert!(!message.contains("QUOTA_EXCEEDED"), "{message}");
    assert!(
        !message.contains("provider-rate-limit-diagnostic"),
        "{message}"
    );
    server.abort();
}

#[test]
fn spotify_capability_truth_distinguishes_removed_and_unsupported_operations() {
    assert!(matches!(
        support(ZoneTarget::Spotify, Capability::Browse),
        Support::NotImplemented { .. }
    ));
    for capability in [
        Capability::QueueJump,
        Capability::QueueReorder,
        Capability::QueueRemove,
        Capability::QueueClear,
        Capability::QueueTransfer,
        Capability::MultiroomSync,
    ] {
        assert!(
            matches!(
                support(ZoneTarget::Spotify, capability),
                Support::Unsupported { .. }
            ),
            "{} must be a provider limitation, not an implementation promise",
            capability.name()
        );
    }
}

#[test]
fn spotify_tool_source_does_not_advertise_removed_development_mode_actions() {
    let contract: serde_json::Value =
        serde_json::from_str(SPOTIFY_2026_CONTRACT).expect("Spotify contract fixture");
    let source = include_str!("../src/mcp/tools/spotify.rs");
    for action in contract["removed_development_mode_browse_actions"]
        .as_array()
        .expect("removed browse actions")
    {
        let action = action.as_str().expect("action name");
        assert!(!source.contains(&format!("\"{action}\"")), "{action}");
    }
    assert!(source.contains("Development Mode"));
}

#[tokio::test]
async fn playlists_saved_tracks_and_playlist_edits_use_current_spotify_endpoints() {
    let (base_url, mock, server) = mock_server().await;
    let adapter = SpotifyAdapter::with_base_url(create_bus(), base_url);
    adapter
        .set_token(SpotifyToken {
            access_token: "access".to_string(),
            refresh_token: None,
            expires_at: Some(u64::MAX),
        })
        .await;

    let playlists = adapter
        .get_playlists(2, 0)
        .await
        .expect("Spotify playlists");
    assert_eq!(playlists.items[0].uri, "spotify:playlist:playlist-1");
    let items = adapter
        .get_playlist_items("playlist-1", 2, 0)
        .await
        .expect("Spotify playlist items");
    assert_eq!(
        items.items[0].item.as_ref().expect("track").uri,
        "spotify:track:track-1"
    );
    let saved = adapter
        .get_saved_tracks(2, 0)
        .await
        .expect("Spotify saved tracks");
    assert_eq!(saved.items[0].track.uri, "spotify:track:track-1");
    assert_eq!(
        adapter
            .check_saved_tracks(&["track-1".to_string(), "track-2".to_string()])
            .await
            .expect("Spotify saved track check"),
        vec![true, false]
    );

    let created = adapter
        .create_playlist("New Playlist", false, false, Some("Made by UHC"))
        .await
        .expect("Spotify create playlist");
    assert_eq!(created.id, "playlist-2");
    assert_eq!(created.items.as_ref().map(|items| items.total), Some(0));
    adapter
        .update_playlist("playlist-1", Some("Renamed"), Some(false), None, None)
        .await
        .expect("Spotify update playlist");
    let added = adapter
        .add_playlist_items("playlist-1", &["spotify:track:track-1".to_string()], None)
        .await
        .expect("Spotify add playlist items");
    assert_eq!(added, Some("snapshot-add".to_string()));
    let replaced = adapter
        .replace_playlist_items("playlist-1", &["spotify:track:track-1".to_string()])
        .await
        .expect("Spotify replace playlist items");
    assert_eq!(replaced, Some("snapshot-edit".to_string()));
    let removed = adapter
        .remove_playlist_items("playlist-1", &["spotify:track:track-1".to_string()], None)
        .await
        .expect("Spotify remove playlist items");
    assert_eq!(removed, Some("snapshot-remove".to_string()));
    adapter
        .save_tracks(&["track-1".to_string()])
        .await
        .expect("Spotify save track");
    adapter
        .remove_saved_tracks(&["track-1".to_string()])
        .await
        .expect("Spotify remove saved track");

    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(!requests.iter().any(|request| request.contains("/browse/")));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("GET /me/playlists?")));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("GET /playlists/playlist-1/items?")));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("GET /me/tracks?")));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("GET /me/library/contains?")));
    assert!(requests
        .iter()
        .any(|request| request == "POST /me/playlists"));
    assert!(requests
        .iter()
        .any(|request| request == "PUT /playlists/playlist-1"));
    assert!(requests
        .iter()
        .any(|request| request == "POST /playlists/playlist-1/items"));
    assert!(requests
        .iter()
        .any(|request| request == "PUT /playlists/playlist-1/items"));
    assert!(requests
        .iter()
        .any(|request| request == "DELETE /playlists/playlist-1/items"));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("PUT /me/library?uris=")));
    assert!(requests
        .iter()
        .any(|request| request.starts_with("DELETE /me/library?uris=")));
    let bodies = mock.bodies.lock().expect("mock body lock").clone();
    assert!(bodies.iter().any(|body| body.contains("New Playlist")));
    assert!(bodies.iter().any(|body| body.contains("track-1")));
    assert!(bodies
        .iter()
        .any(|body| body.contains("\"items\"") && !body.contains("\"tracks\"")));
    server.abort();
}

#[tokio::test]
async fn play_uri_targets_device_and_sends_track_uri_body() {
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

    let message = adapter
        .play_uri("spotify:device-1", "spotify:track:track-1")
        .await
        .expect("Spotify URI playback");
    assert!(message.contains("spotify:track:track-1"));
    let requests = mock.requests.lock().expect("mock lock").clone();
    assert!(requests
        .iter()
        .any(|request| request == "PUT /me/player/play?device_id=device-1"));
    let bodies = mock.bodies.lock().expect("mock body lock").clone();
    assert!(bodies.iter().any(|body| {
        serde_json::from_str::<serde_json::Value>(body)
            .map(|json| json == serde_json::json!({"uris": ["spotify:track:track-1"]}))
            .unwrap_or(false)
    }));

    adapter
        .play_uri("spotify:device-1", "spotify:album:album-1")
        .await
        .expect("Spotify context playback");
    let bodies = mock.bodies.lock().expect("mock body lock").clone();
    assert!(bodies.iter().any(|body| {
        serde_json::from_str::<serde_json::Value>(body)
            .map(|json| json == serde_json::json!({"context_uri": "spotify:album:album-1"}))
            .unwrap_or(false)
    }));
    server.abort();
}

#[tokio::test]
async fn polling_emits_discovery_once_then_incremental_updates() {
    let (base_url, _mock, server) = mock_server().await;
    let bus = create_bus();
    let mut events = bus.subscribe();
    let adapter = SpotifyAdapter::with_base_url(bus, base_url);
    adapter
        .set_token(SpotifyToken {
            access_token: "access".to_string(),
            refresh_token: None,
            expires_at: Some(u64::MAX),
        })
        .await;

    adapter.update().await.expect("initial Spotify poll");
    let first = events.recv().await.expect("account event");
    let second = events.recv().await.expect("discovery event");
    assert!(matches!(first, BusEvent::ProviderAccountUpdated { .. }));
    assert!(matches!(second, BusEvent::ZoneDiscovered { .. }));

    adapter.update().await.expect("subsequent Spotify poll");
    let next = events.recv().await.expect("incremental update event");
    assert!(matches!(next, BusEvent::ZoneUpdated { .. }));
    assert!(
        tokio::time::timeout(Duration::from_millis(25), events.recv())
            .await
            .is_err()
    );

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

#[tokio::test]
async fn stopping_adapter_flushes_spotify_zones() {
    let bus = create_bus();
    let mut events = bus.subscribe();
    let adapter = SpotifyAdapter::new(bus);

    adapter.stop().await;

    let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
        .await
        .expect("stop must publish a lifecycle event")
        .expect("bus must remain open");
    assert!(matches!(
        event,
        BusEvent::AdapterStopping { adapter, reason }
            if adapter == "spotify" && reason.as_deref() == Some("requested")
    ));
}
