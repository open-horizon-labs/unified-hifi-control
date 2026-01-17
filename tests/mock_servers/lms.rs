//! Mock LMS (Logitech Media Server) for testing
//!
//! Simulates the JSON-RPC interface at /jsonrpc.js

use axum::{extract::State, http::StatusCode, routing::post, Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

/// Mock player state
#[derive(Debug, Clone)]
pub struct MockPlayer {
    pub playerid: String,
    pub name: String,
    pub model: String,
    pub connected: bool,
    pub power: bool,
    pub mode: String, // "play", "pause", "stop"
    pub volume: i32,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub duration: f64,
    pub time: f64,
}

impl MockPlayer {
    pub fn new(playerid: &str, name: &str) -> Self {
        Self {
            playerid: playerid.to_string(),
            name: name.to_string(),
            model: "MockPlayer".to_string(),
            connected: true,
            power: true,
            mode: "stop".to_string(),
            volume: 50,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            duration: 0.0,
            time: 0.0,
        }
    }
}

/// Mock LMS server state
struct MockLmsState {
    players: HashMap<String, MockPlayer>,
}

/// Mock LMS Server
pub struct MockLmsServer {
    addr: SocketAddr,
    state: Arc<RwLock<MockLmsState>>,
    handle: JoinHandle<()>,
}

impl MockLmsServer {
    /// Start a mock LMS server on a random port
    pub async fn start() -> Self {
        let state = Arc::new(RwLock::new(MockLmsState {
            players: HashMap::new(),
        }));

        let app = Router::new()
            .route("/jsonrpc.js", post(handle_jsonrpc))
            .with_state(state.clone());

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        Self {
            addr,
            state,
            handle,
        }
    }

    /// Get the server address
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Add a player to the mock server
    pub async fn add_player(&self, playerid: &str, name: &str) {
        let mut state = self.state.write().await;
        state
            .players
            .insert(playerid.to_string(), MockPlayer::new(playerid, name));
    }

    /// Set player state (play/pause/stop)
    pub async fn set_mode(&self, playerid: &str, mode: &str) {
        let mut state = self.state.write().await;
        if let Some(player) = state.players.get_mut(playerid) {
            player.mode = mode.to_string();
        }
    }

    /// Set player volume (0-100)
    pub async fn set_volume(&self, playerid: &str, volume: i32) {
        let mut state = self.state.write().await;
        if let Some(player) = state.players.get_mut(playerid) {
            player.volume = volume.clamp(0, 100);
        }
    }

    /// Set now playing info
    pub async fn set_now_playing(&self, playerid: &str, title: &str, artist: &str, album: &str) {
        let mut state = self.state.write().await;
        if let Some(player) = state.players.get_mut(playerid) {
            player.title = title.to_string();
            player.artist = artist.to_string();
            player.album = album.to_string();
        }
    }

    /// Stop the mock server
    pub async fn stop(self) {
        self.handle.abort();
    }
}

/// JSON-RPC request format
#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    id: Value,
    method: String,
    params: Vec<Value>,
}

/// JSON-RPC response format
#[derive(Debug, Serialize)]
struct JsonRpcResponse {
    id: Value,
    result: Value,
}

/// Handle JSON-RPC requests
async fn handle_jsonrpc(
    State(state): State<Arc<RwLock<MockLmsState>>>,
    Json(request): Json<JsonRpcRequest>,
) -> Result<Json<JsonRpcResponse>, StatusCode> {
    if request.method != "slim.request" {
        return Err(StatusCode::BAD_REQUEST);
    }

    let params = &request.params;
    if params.len() < 2 {
        return Err(StatusCode::BAD_REQUEST);
    }

    let player_id = params[0].as_str().unwrap_or("");
    let commands = params[1].as_array().ok_or(StatusCode::BAD_REQUEST)?;

    let command = commands
        .first()
        .and_then(|v| v.as_str())
        .ok_or(StatusCode::BAD_REQUEST)?;

    // Handle commands that modify state
    match command {
        "play" => {
            // LMS "play" command starts playback from stopped/beginning
            // NOTE: This does NOT resume from pause - use "pause 0" for that
            let mut state = state.write().await;
            if let Some(player) = state.players.get_mut(player_id) {
                // Only start playing if stopped (not if paused!)
                // This matches real LMS behavior where "play" starts playlist
                // but doesn't resume from pause
                if player.mode == "stop" {
                    player.mode = "play".to_string();
                }
                // If paused, "play" does nothing - need "pause 0" to resume
            }
            return Ok(Json(JsonRpcResponse {
                id: request.id,
                result: json!({}),
            }));
        }
        "pause" => {
            let mut state = state.write().await;
            if let Some(player) = state.players.get_mut(player_id) {
                // Get optional parameter: 0=unpause, 1=pause, none=toggle
                let pause_arg = commands.get(1).and_then(|v| v.as_i64());
                match pause_arg {
                    Some(0) => {
                        // pause 0 = unpause/resume
                        if player.mode == "pause" {
                            player.mode = "play".to_string();
                        }
                    }
                    Some(1) => {
                        // pause 1 = force pause
                        if player.mode == "play" {
                            player.mode = "pause".to_string();
                        }
                    }
                    None => {
                        // No arg = toggle
                        player.mode = match player.mode.as_str() {
                            "play" => "pause".to_string(),
                            "pause" => "play".to_string(),
                            _ => player.mode.clone(),
                        };
                    }
                    _ => {}
                }
            }
            return Ok(Json(JsonRpcResponse {
                id: request.id,
                result: json!({}),
            }));
        }
        "stop" => {
            let mut state = state.write().await;
            if let Some(player) = state.players.get_mut(player_id) {
                player.mode = "stop".to_string();
            }
            return Ok(Json(JsonRpcResponse {
                id: request.id,
                result: json!({}),
            }));
        }
        _ => {}
    }

    // Handle read-only commands
    let state = state.read().await;
    let result = match command {
        "players" => {
            // Return list of players
            let players_loop: Vec<Value> = state
                .players
                .values()
                .map(|p| {
                    json!({
                        "playerid": p.playerid,
                        "name": p.name,
                        "model": p.model,
                        "connected": if p.connected { 1 } else { 0 },
                        "power": if p.power { 1 } else { 0 },
                    })
                })
                .collect();

            json!({
                "count": players_loop.len(),
                "players_loop": players_loop
            })
        }
        "status" => {
            // Return player status
            if let Some(player) = state.players.get(player_id) {
                let playlist_loop = if !player.title.is_empty() {
                    vec![json!({
                        "title": player.title,
                        "artist": player.artist,
                        "album": player.album,
                        "duration": player.duration,
                    })]
                } else {
                    vec![]
                };

                json!({
                    "mode": player.mode,
                    "power": if player.power { 1 } else { 0 },
                    "mixer volume": player.volume,
                    "time": player.time,
                    "duration": player.duration,
                    "playlist_tracks": playlist_loop.len(),
                    "playlist_cur_index": if playlist_loop.is_empty() { Value::Null } else { json!(0) },
                    "playlist_loop": playlist_loop,
                })
            } else {
                json!({})
            }
        }
        "mixer" => {
            // Volume control - return empty success
            json!({})
        }
        "playlist" => {
            // Playlist control (next/prev) - return empty success
            json!({})
        }
        _ => {
            json!({})
        }
    };

    Ok(Json(JsonRpcResponse {
        id: request.id,
        result,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_lms_starts_and_stops() {
        let server = MockLmsServer::start().await;
        let addr = server.addr();
        assert!(addr.port() > 0);
        server.stop().await;
    }

    #[tokio::test]
    async fn mock_lms_returns_players() {
        let server = MockLmsServer::start().await;
        server.add_player("aa:bb:cc:dd:ee:ff", "Test Player").await;

        let client = reqwest::Client::new();
        let response = client
            .post(format!("http://{}/jsonrpc.js", server.addr()))
            .json(&json!({
                "id": 1,
                "method": "slim.request",
                "params": ["", ["players", 0, 100]]
            }))
            .send()
            .await
            .unwrap();

        let body: Value = response.json().await.unwrap();
        let players = body["result"]["players_loop"].as_array().unwrap();
        assert_eq!(players.len(), 1);
        assert_eq!(players[0]["name"], "Test Player");

        server.stop().await;
    }

    /// Helper to get player status mode
    async fn get_mode(client: &reqwest::Client, addr: &SocketAddr, player_id: &str) -> String {
        let response = client
            .post(format!("http://{}/jsonrpc.js", addr))
            .json(&json!({
                "id": 1,
                "method": "slim.request",
                "params": [player_id, ["status", "-", 1, "tags:"]]
            }))
            .send()
            .await
            .unwrap();
        let body: Value = response.json().await.unwrap();
        body["result"]["mode"]
            .as_str()
            .unwrap_or("unknown")
            .to_string()
    }

    /// Helper to send a command
    async fn send_command(
        client: &reqwest::Client,
        addr: &SocketAddr,
        player_id: &str,
        cmd: Vec<Value>,
    ) {
        let request = json!({
            "id": 1,
            "method": "slim.request",
            "params": [player_id, cmd]
        });
        client
            .post(format!("http://{}/jsonrpc.js", addr))
            .json(&request)
            .send()
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn mock_lms_pause_0_resumes_playback() {
        // This test verifies the correct LMS behavior:
        // - "pause 0" (unpause) resumes playback from pause
        let server = MockLmsServer::start().await;
        let player_id = "aa:bb:cc:dd:ee:ff";
        server.add_player(player_id, "Test Player").await;
        server.set_mode(player_id, "pause").await;

        let client = reqwest::Client::new();
        let addr = server.addr();

        // Verify initial state is paused
        assert_eq!(get_mode(&client, &addr, player_id).await, "pause");

        // Send "pause 0" (unpause) - this should resume
        send_command(&client, &addr, player_id, vec![json!("pause"), json!(0)]).await;

        // Verify player is now playing
        assert_eq!(get_mode(&client, &addr, player_id).await, "play");

        server.stop().await;
    }

    #[tokio::test]
    async fn mock_lms_play_does_not_resume_from_pause() {
        // This test documents the LMS quirk that the current adapter doesn't handle:
        // - The "play" command does NOT resume from pause
        // - You need "pause 0" to resume
        let server = MockLmsServer::start().await;
        let player_id = "aa:bb:cc:dd:ee:ff";
        server.add_player(player_id, "Test Player").await;
        server.set_mode(player_id, "pause").await;

        let client = reqwest::Client::new();
        let addr = server.addr();

        // Verify initial state is paused
        assert_eq!(get_mode(&client, &addr, player_id).await, "pause");

        // Send "play" - in real LMS, this does NOT resume from pause!
        send_command(&client, &addr, player_id, vec![json!("play")]).await;

        // Player should still be paused (play doesn't resume!)
        assert_eq!(get_mode(&client, &addr, player_id).await, "pause");

        server.stop().await;
    }

    #[tokio::test]
    async fn mock_lms_pause_toggle() {
        // Test that "pause" with no args toggles
        let server = MockLmsServer::start().await;
        let player_id = "aa:bb:cc:dd:ee:ff";
        server.add_player(player_id, "Test Player").await;
        server.set_mode(player_id, "play").await;

        let client = reqwest::Client::new();
        let addr = server.addr();

        // Toggle: play -> pause
        send_command(&client, &addr, player_id, vec![json!("pause")]).await;
        assert_eq!(get_mode(&client, &addr, player_id).await, "pause");

        // Toggle: pause -> play
        send_command(&client, &addr, player_id, vec![json!("pause")]).await;
        assert_eq!(get_mode(&client, &addr, player_id).await, "play");

        server.stop().await;
    }
}
