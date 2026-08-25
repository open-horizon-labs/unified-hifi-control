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

/// A command as it arrived on the wire: the target player id and the raw
/// command array LMS was asked to run.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCommand {
    pub player_id: String,
    pub command: Vec<Value>,
}

impl RecordedCommand {
    /// The command array rendered as strings, e.g. `["mixer", "volume", "-5"]`.
    /// Convenient for asserting on the exact backend command an MCP action
    /// produced without matching on `Value` variants.
    pub fn parts(&self) -> Vec<String> {
        self.command
            .iter()
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect()
    }
}

/// One library entry for the `search` (library fallback) and
/// `albums`/`artists`/`titles` (existence-check) handlers below.
///
/// #396: purpose-built for that issue's ref tests, not a general #417 fix —
/// see `tests/mock_servers/README.md` and this module's own docs on the
/// difference. Real LMS keys each type's own entity id `<type>_id`
/// (`album_id`, `contributor_id`, `track_id`); this mock mirrors that, one
/// `HashMap` per kind, rather than modeling LMS's actual database.
#[derive(Debug, Clone)]
pub struct MockLibraryItem {
    pub id: i64,
    pub title: String,
    pub artist: String,
}

/// One track row for `titles` (an album's tracks) and `playlists tracks` (a
/// playlist's tracks) -- #531's `hifi_collections` drill-down.
#[derive(Debug, Clone)]
pub struct MockTrack {
    pub id: i64,
    pub title: String,
    pub artist: Option<String>,
}

/// One row of LMS's `favorites items` -- #531. Real LMS favorites carry no
/// durable entity id, only a `url`; this mock mirrors that.
#[derive(Debug, Clone)]
pub struct MockFavorite {
    pub name: String,
    pub url: String,
}

/// Mock LMS server state
struct MockLmsState {
    players: HashMap<String, MockPlayer>,
    /// Every command received, in arrival order. Lets a test assert which
    /// backend command an MCP action actually produced (issue #394) rather than
    /// only observing the resulting state.
    commands: Vec<RecordedCommand>,
    /// Library items for `search` (the `search_library` fallback path) and for
    /// the `albums`/`artists`/`titles` existence checks
    /// `LmsAdapter::assert_library_id_exists` (#396) issues before a mutating
    /// `playlistcontrol` call, and for #531's `hifi_collections` listings.
    /// Keyed by [`LmsSearchResultType`]-shaped kind: `"album"`, `"artist"`,
    /// `"track"`, plus `"playlist"` for #531.
    library: HashMap<&'static str, Vec<MockLibraryItem>>,
    /// Active sync groups (#510). Each inner `Vec` is one group's full
    /// membership, first-added player first -- mirroring what real LMS's
    /// `syncgroups ?` reports (a flat member list with no leader marker).
    /// This is a simplified peer model, not a full replica of LMS's
    /// undocumented internal master-election-on-leave behavior: `sync -`
    /// dissolves the whole group rather than promoting a new master, which is
    /// enough to exercise the adapter's join/leave/status calls without
    /// claiming to model LMS's internals exactly.
    sync_groups: Vec<Vec<String>>,
    /// `album_id` -> that album's tracks, for `titles <start> <count>
    /// album_id:<id>` (#531).
    album_tracks: HashMap<i64, Vec<MockTrack>>,
    /// `playlist_id` -> that playlist's tracks, for `playlists tracks <start>
    /// <count> playlist_id:<id>` (#531).
    playlist_tracks: HashMap<i64, Vec<MockTrack>>,
    /// The flat favourites list for `favorites items <start> <count>` (#531).
    favorites: Vec<MockFavorite>,
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
            commands: Vec::new(),
            library: HashMap::new(),
            sync_groups: Vec::new(),
            album_tracks: HashMap::new(),
            playlist_tracks: HashMap::new(),
            favorites: Vec::new(),
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

    /// Remove a player from the authoritative `players` inventory.
    pub async fn remove_player(&self, playerid: &str) {
        self.state.write().await.players.remove(playerid);
    }

    /// Keep a player in LMS's inventory while changing whether its client is
    /// actually connected. Real LMS retains disconnected players this way.
    pub async fn set_connected(&self, playerid: &str, connected: bool) {
        let mut state = self.state.write().await;
        if let Some(player) = state.players.get_mut(playerid) {
            player.connected = connected;
        }
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

    /// Drop the recorded command log. Call before the action under test so
    /// polling traffic (`players`, `status`) does not have to be filtered out.
    pub async fn clear_commands(&self) {
        self.state.write().await.commands.clear();
    }

    /// Seed the library albums the `search` (term-based) and `albums`
    /// (id-existence) handlers answer from. Replaces whatever was set before
    /// -- call again with a shorter list to simulate a rescan that dropped an
    /// id, which is exactly what
    /// `LmsAdapter::assert_library_id_exists` (#396) has to notice before a
    /// mutating `playlistcontrol` call.
    pub async fn set_library_albums(&self, albums: Vec<(i64, &str, &str)>) {
        let items = albums
            .into_iter()
            .map(|(id, title, artist)| MockLibraryItem {
                id,
                title: title.to_string(),
                artist: artist.to_string(),
            })
            .collect();
        self.state.write().await.library.insert("album", items);
    }

    /// Seed the artist library `artists <start> <count>` answers from (#531).
    pub async fn set_library_artists(&self, artists: Vec<(i64, &str)>) {
        let items = artists
            .into_iter()
            .map(|(id, title)| MockLibraryItem {
                id,
                title: title.to_string(),
                artist: String::new(),
            })
            .collect();
        self.state.write().await.library.insert("artist", items);
    }

    /// Seed the playlist library `playlists <start> <count>` answers from
    /// (#531).
    pub async fn set_library_playlists(&self, playlists: Vec<(i64, &str)>) {
        let items = playlists
            .into_iter()
            .map(|(id, title)| MockLibraryItem {
                id,
                title: title.to_string(),
                artist: String::new(),
            })
            .collect();
        self.state.write().await.library.insert("playlist", items);
    }

    /// Seed one album's tracks, for `titles <start> <count> album_id:<id>`
    /// (#531).
    pub async fn set_album_tracks(&self, album_id: i64, tracks: Vec<(i64, &str, Option<&str>)>) {
        let items = tracks
            .into_iter()
            .map(|(id, title, artist)| MockTrack {
                id,
                title: title.to_string(),
                artist: artist.map(str::to_string),
            })
            .collect();
        self.state
            .write()
            .await
            .album_tracks
            .insert(album_id, items);
    }

    /// Seed one playlist's tracks, for `playlists tracks <start> <count>
    /// playlist_id:<id>` (#531).
    pub async fn set_playlist_tracks(
        &self,
        playlist_id: i64,
        tracks: Vec<(i64, &str, Option<&str>)>,
    ) {
        let items = tracks
            .into_iter()
            .map(|(id, title, artist)| MockTrack {
                id,
                title: title.to_string(),
                artist: artist.map(str::to_string),
            })
            .collect();
        self.state
            .write()
            .await
            .playlist_tracks
            .insert(playlist_id, items);
    }

    /// Seed the flat favourites list `favorites items <start> <count>`
    /// answers from (#531). Real LMS favorites have no durable id, only a
    /// `url` -- mirrored here rather than inventing one.
    pub async fn set_favorites(&self, favorites: Vec<(&str, &str)>) {
        let items = favorites
            .into_iter()
            .map(|(name, url)| MockFavorite {
                name: name.to_string(),
                url: url.to_string(),
            })
            .collect();
        self.state.write().await.favorites = items;
    }

    /// Commands received for `player_id`, excluding the read-only polling
    /// commands the adapter issues on its own schedule.
    pub async fn write_commands(&self, player_id: &str) -> Vec<Vec<String>> {
        const POLLING: &[&str] = &["players", "status", "serverstatus", "syncgroups"];
        self.state
            .read()
            .await
            .commands
            .iter()
            .filter(|c| c.player_id == player_id)
            .filter(|c| {
                c.command
                    .first()
                    .and_then(Value::as_str)
                    .is_none_or(|first| !POLLING.contains(&first))
            })
            .map(RecordedCommand::parts)
            .collect()
    }

    /// Current sync groups, each as its full member id list (#510). Lets a
    /// test assert on server-side membership directly, independent of how
    /// the adapter reports it.
    pub async fn sync_groups(&self) -> Vec<Vec<String>> {
        self.state.read().await.sync_groups.clone()
    }

    /// Stop the mock server
    pub async fn stop(self) {
        self.handle.abort();
    }
}

/// Join `member` into `master`'s sync group (#510).
///
/// Mirrors the live-verified behavior (issue #403): the *addressed* player
/// (`master`) becomes/stays the sync master, and `member` joins its group.
/// `member` is first removed from any group it was previously in.
fn mock_sync_join(state: &mut MockLmsState, master: &str, member: &str) {
    mock_sync_remove(state, member);
    if let Some(group) = state
        .sync_groups
        .iter_mut()
        .find(|group| group.first().map(String::as_str) == Some(master))
    {
        if !group.iter().any(|id| id == member) {
            group.push(member.to_string());
        }
        return;
    }
    // `master` was not already a group's first (leader) entry. If it was a
    // member of some other group, leaving that group first keeps this model
    // consistent with "the addressed player becomes master".
    mock_sync_remove(state, master);
    state
        .sync_groups
        .push(vec![master.to_string(), member.to_string()]);
}

/// Remove `player` from whatever sync group it currently belongs to, if any.
/// A group left with fewer than two members is no longer a sync group and is
/// dropped entirely.
fn mock_sync_remove(state: &mut MockLmsState, player: &str) {
    for group in state.sync_groups.iter_mut() {
        group.retain(|id| id != player);
    }
    state.sync_groups.retain(|group| group.len() >= 2);
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

/// Read a tagged `"<tag>:<value>"` parameter out of a command array, the way
/// LMS's own CLI encodes filters (`term:query`, `album_id:5`, ...).
fn filter_value<'a>(commands: &'a [Value], tag: &str) -> Option<&'a str> {
    let prefix = format!("{tag}:");
    commands
        .iter()
        .filter_map(Value::as_str)
        .find_map(|s| s.strip_prefix(prefix.as_str()))
}

/// Read the `<start> <count>` pair LMS's own taggedlist queries carry at a
/// fixed position (#531): `["albums", <start>, <count>, ...]` has them at
/// `1, 2`; `["playlists", "tracks", <start>, <count>, ...]` has them one
/// further out, at `2, 3`. `start_index` names where `<start>` sits.
fn paging(commands: &[Value], start_index: usize) -> (usize, usize) {
    let offset = commands
        .get(start_index)
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let count = commands
        .get(start_index + 1)
        .and_then(Value::as_u64)
        .map(|c| c as usize)
        .unwrap_or(usize::MAX);
    (offset, count)
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

    // Record every command before dispatching, so tests can assert which
    // backend command an MCP action produced (issue #394).
    state.write().await.commands.push(RecordedCommand {
        player_id: player_id.to_string(),
        command: commands.clone(),
    });

    // Handle commands that modify state
    match command {
        "play" => {
            // LMS "play" command starts playback from stopped OR resumes from pause
            // Per real-world testing (issue #68), a single "play" command handles both
            let mut state = state.write().await;
            if let Some(player) = state.players.get_mut(player_id) {
                // "play" works from both stopped and paused states
                if player.mode == "stop" || player.mode == "pause" {
                    player.mode = "play".to_string();
                }
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
        "mixer"
            if commands.get(1).and_then(Value::as_str) == Some("volume")
                && commands.get(2).is_some() =>
        {
            let requested = commands.get(2).expect("guarded volume value");
            let rendered = requested
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| requested.to_string());
            let relative = rendered.starts_with('+') || rendered.starts_with('-');
            let value = rendered.parse::<i32>().unwrap_or_default();
            let mut state = state.write().await;
            if let Some(player) = state.players.get_mut(player_id) {
                player.volume = if relative {
                    player.volume.saturating_add(value)
                } else {
                    value
                }
                .clamp(0, 100);
            }
            return Ok(Json(JsonRpcResponse {
                id: request.id,
                result: json!({}),
            }));
        }
        // `<playerid> sync <otherplayerid>` joins a group; `<playerid> sync -`
        // leaves whatever group it is in (#510).
        "sync" if commands.get(1).is_some() => {
            let target = commands.get(1).and_then(Value::as_str).unwrap_or("");
            let mut state = state.write().await;
            if target == "-" {
                mock_sync_remove(&mut state, player_id);
            } else {
                mock_sync_join(&mut state, player_id, target);
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
        // Server-scoped `syncgroups ?` (#510): one entry per active group,
        // with the full membership as a comma-joined id list, matching the
        // `sync_members`/`sync_member_names` shape verified live against
        // Lyrion 9.1.2 (issue #403's investigation).
        "syncgroups" => {
            let syncgroups_loop: Vec<Value> = state
                .sync_groups
                .iter()
                .map(|group| {
                    let names: Vec<String> = group
                        .iter()
                        .map(|id| {
                            state
                                .players
                                .get(id)
                                .map(|p| p.name.clone())
                                .unwrap_or_default()
                        })
                        .collect();
                    json!({
                        "sync_members": group.join(","),
                        "sync_member_names": names.join(","),
                    })
                })
                .collect();
            json!({
                "count": syncgroups_loop.len(),
                "syncgroups_loop": syncgroups_loop
            })
        }
        "playlist" => {
            // Playlist control (next/prev) - return empty success
            json!({})
        }
        // The `search_library` fallback path (`LmsAdapter::search_library`):
        // `["search", 0, limit, "term:<query>"]`, answered from `library`
        // rather than the catch-all `{}` every other command still gets.
        // #396: this is what lets an MCP-level test mint a durable `Library`
        // ref without needing a full #417 globalsearch fix.
        "search" => {
            // `["search", offset, count, "term:<query>"]` -- offset/count are
            // real LMS pagination params, honored here (not merely accepted
            // and ignored) so a test can prove what happens when more than
            // `count` items match a term, the same as a real server would
            // truncate. See #396's dissent: `assert_library_id_exists`
            // re-searches by title with a fixed count, so whether matches
            // beyond that count are reachable is exactly what this models.
            let offset = commands.get(1).and_then(Value::as_u64).unwrap_or(0) as usize;
            let count = commands
                .get(2)
                .and_then(Value::as_u64)
                .map(|c| c as usize)
                .unwrap_or(usize::MAX);
            let query = filter_value(commands, "term")
                .unwrap_or_default()
                .to_lowercase();
            let matches: Vec<&MockLibraryItem> = state
                .library
                .get("album")
                .into_iter()
                .flatten()
                .filter(|item| {
                    item.title.to_lowercase().contains(&query)
                        || item.artist.to_lowercase().contains(&query)
                })
                .collect();
            let page: Vec<Value> = matches
                .into_iter()
                .skip(offset)
                .take(count)
                .map(|item| {
                    json!({
                        "album_id": item.id,
                        "album": item.title,
                        "artist": item.artist,
                    })
                })
                .collect();
            // #531: `LmsAdapter::assert_library_id_exists` re-searches by
            // title before honoring a track ref minted by
            // `hifi_collections` (same existence check #396 added for
            // albums), so a track from an album's or a playlist's tracks
            // must be findable here too -- by title, exactly like albums
            // above, deduped by id since the same track can appear in both.
            let mut seen_track_ids = std::collections::HashSet::new();
            let track_page: Vec<Value> = state
                .album_tracks
                .values()
                .chain(state.playlist_tracks.values())
                .flatten()
                .filter(|t| seen_track_ids.insert(t.id))
                .filter(|t| {
                    t.title.to_lowercase().contains(&query)
                        || t.artist
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&query)
                })
                .skip(offset)
                .take(count)
                .map(|t| json!({"track_id": t.id, "track": t.title, "artist": t.artist}))
                .collect();
            let mut result = serde_json::Map::new();
            if !page.is_empty() {
                result.insert("albums_loop".to_string(), json!(page));
            }
            if !track_page.is_empty() {
                result.insert("tracks_loop".to_string(), json!(track_page));
            }
            Value::Object(result)
        }
        // The existence check `LmsAdapter::assert_library_id_exists` (#396)
        // issues before a mutating `playlistcontrol`:
        // `["albums", 0, 1, "album_id:<id>"]` (and the `artists`/`titles`
        // analogues, unmodeled here since #396's tests only exercise the
        // album path -- they fall to the catch-all `{"count": 0}` shape via
        // `_`, which is the safe direction: an unmodeled kind reads as "not
        // found" rather than "found").
        "albums" if filter_value(commands, "album_id").is_some() => {
            let requested_id =
                filter_value(commands, "album_id").and_then(|v| v.parse::<i64>().ok());
            let count = match requested_id {
                Some(id) => state
                    .library
                    .get("album")
                    .into_iter()
                    .flatten()
                    .filter(|item| item.id == id)
                    .count(),
                None => 0,
            };
            json!({ "count": count })
        }
        // #531's `hifi_collections browse`: `["albums", <start>, <count>]`,
        // the whole album library (no `album_id` filter, handled above).
        "albums" => {
            let (offset, count) = paging(commands, 1);
            let all: Vec<&MockLibraryItem> =
                state.library.get("album").into_iter().flatten().collect();
            let page: Vec<Value> = all
                .iter()
                .skip(offset)
                .take(count)
                .map(
                    |item| json!({"album_id": item.id, "album": item.title, "artist": item.artist}),
                )
                .collect();
            json!({ "count": all.len(), "albums_loop": page })
        }
        // #531: `["artists", <start>, <count>]`.
        "artists" => {
            let (offset, count) = paging(commands, 1);
            let all: Vec<&MockLibraryItem> =
                state.library.get("artist").into_iter().flatten().collect();
            let page: Vec<Value> = all
                .iter()
                .skip(offset)
                .take(count)
                .map(|item| json!({"artist_id": item.id, "artist": item.title}))
                .collect();
            json!({ "count": all.len(), "artists_loop": page })
        }
        // #531: `["titles", <start>, <count>, "album_id:<id>"]` -- one
        // album's tracks.
        "titles" => {
            let (offset, count) = paging(commands, 1);
            let album_id = filter_value(commands, "album_id").and_then(|v| v.parse::<i64>().ok());
            let tracks = album_id
                .and_then(|id| state.album_tracks.get(&id))
                .cloned()
                .unwrap_or_default();
            let page: Vec<Value> = tracks
                .iter()
                .skip(offset)
                .take(count)
                .map(|t| json!({"track_id": t.id, "title": t.title, "artist": t.artist}))
                .collect();
            json!({ "count": tracks.len(), "titles_loop": page })
        }
        // #531: `["playlists", <start>, <count>]` (the playlist library) or
        // `["playlists", "tracks", <start>, <count>, "playlist_id:<id>"]`
        // (one playlist's tracks).
        "playlists" if commands.get(1).and_then(Value::as_str) == Some("tracks") => {
            let (offset, count) = paging(commands, 2);
            let playlist_id =
                filter_value(commands, "playlist_id").and_then(|v| v.parse::<i64>().ok());
            let tracks = playlist_id
                .and_then(|id| state.playlist_tracks.get(&id))
                .cloned()
                .unwrap_or_default();
            let page: Vec<Value> = tracks
                .iter()
                .skip(offset)
                .take(count)
                .map(|t| json!({"id": t.id, "title": t.title, "artist": t.artist}))
                .collect();
            json!({ "count": tracks.len(), "playlisttracks_loop": page })
        }
        "playlists" => {
            let (offset, count) = paging(commands, 1);
            let all: Vec<&MockLibraryItem> = state
                .library
                .get("playlist")
                .into_iter()
                .flatten()
                .collect();
            let page: Vec<Value> = all
                .iter()
                .skip(offset)
                .take(count)
                .map(|item| json!({"id": item.id, "playlist": item.title}))
                .collect();
            json!({ "count": all.len(), "playlists_loop": page })
        }
        // #531: `["favorites", "items", <start>, <count>]`.
        "favorites" if commands.get(1).and_then(Value::as_str) == Some("items") => {
            let (offset, count) = paging(commands, 2);
            let page: Vec<Value> = state
                .favorites
                .iter()
                .skip(offset)
                .take(count)
                .map(|f| json!({"name": f.name, "url": f.url}))
                .collect();
            json!({ "count": state.favorites.len(), "loop_loop": page })
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
    async fn mock_lms_play_resumes_from_pause() {
        // Per real-world testing (issue #68), "play" command resumes from pause
        // This matches actual LMS behavior - a single command handles both start and resume
        let server = MockLmsServer::start().await;
        let player_id = "aa:bb:cc:dd:ee:ff";
        server.add_player(player_id, "Test Player").await;
        server.set_mode(player_id, "pause").await;

        let client = reqwest::Client::new();
        let addr = server.addr();

        // Verify initial state is paused
        assert_eq!(get_mode(&client, &addr, player_id).await, "pause");

        // Send "play" - this resumes from pause (confirmed by user testing)
        send_command(&client, &addr, player_id, vec![json!("play")]).await;

        // Player should now be playing
        assert_eq!(get_mode(&client, &addr, player_id).await, "play");

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
