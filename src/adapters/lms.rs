//! LMS (Logitech Media Server) JSON-RPC Client
//!
//! Implements the JSON-RPC protocol over HTTP and CLI event subscription over TCP.
//! Documentation: http://HOST:9000/html/docs/cli-api.html
//!
//! ## Architecture (Issue #165)
//!
//! This module contains two logically separate concerns that share state:
//!
//! 1. **Polling** (HTTP JSON-RPC on port 9000)
//!    - Discovers LMS server and players
//!    - Polls player status at configurable interval
//!    - Primary mechanism - always works
//!
//! 2. **CLI Subscription** (TCP telnet on port 9090)
//!    - Subscribes to real-time events: playlist, mixer, power, client
//!    - Enhancement for faster updates and lower CPU
//!    - Optional - polling continues if CLI unavailable
//!
//! ## Interaction Model
//!
//! The two paths coordinate via a single shared flag: `cli_subscription_active`
//!
//! ```text
//! CLI connects    → flag = true  → Polling slows to 30s interval
//! CLI fails/exits → flag = false → Polling speeds to 2s interval (immediate)
//! CLI reconnects  → flag = true  → Polling slows again
//! ```
//!
//! As of Issue #165, these are split into two independent adapters:
//! - `LmsAdapter`: Polling only
//! - `LmsCliAdapter`: CLI subscription only
//!
//! Each has its own AdapterHandle with independent retry. Use `create_lms_adapters()`
//! factory function to create both with shared state.
//!
//! ## Configuration
//!
//! - `LMS_POLL_INTERVAL`: Base poll interval in seconds (default: 2)
//! - When CLI active, polling runs at 15x base interval (default: 30s)

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock, Semaphore};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::lms_discovery::discover_lms_servers;
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic, LibraryAdapter,
    LibrarySearchResult,
};
use crate::adapters::Startable;
use crate::bus::runtime::{
    CommandEndpoint, CommandGateway, NativeResult, ProjectionEntry, ProjectionIngress,
    ProjectionKind, ProjectionPayload, ProjectionSource, ProjectionUpdate, RuntimeCommand,
};
use crate::bus::{
    BusEvent, Command, PlaybackState, PrefixedZoneId, SharedBus, VolumeControl, Zone,
};
use crate::config::{get_config_file_path, read_config_file};

const LMS_CONFIG_FILE: &str = "lms-config.json";
/// Request ID for LMS JSON-RPC calls (aids debugging in LMS logs)
const LMS_REQUEST_ID: i32 = 217;
/// Name of the per-player LMS pref holding mute state.
///
/// `mixer muting ?` and `players … playerprefs:mute` read the same pref -
/// `mixerQuery` returns `$prefs->client($client)->get("mute")` and
/// `_addPlayersLoop` emits each requested pref by name - so the batched route
/// cannot disagree with the per-player one. Verified against Lyrion 9.1.2.
const LMS_MUTE_PREF: &str = "mute";

/// Strip "lms:" prefix from player IDs.
/// MCP and aggregator use prefixed IDs (e.g., "lms:00:11:22:33:44:55"), but LMS API expects bare IDs.
fn strip_lms_prefix(id: &str) -> &str {
    id.strip_prefix("lms:").unwrap_or(id)
}

/// Require a properly-prefixed LMS zone id and return its bare player id.
///
/// Unlike Music Assistant's ids, LMS player ids are MAC addresses and
/// legitimately contain colons, so (unlike `musicassistant_player_id`) this
/// does not reject a remainder containing `:`.
fn lms_player_id(zone_id: &str, parameter: &str) -> Result<String> {
    zone_id
        .strip_prefix("lms:")
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("{parameter} must be an LMS zone id"))
}

/// [`lms_player_id`] over a list, rejecting duplicates the same way the
/// Music Assistant multiroom contract does.
fn lms_player_ids(zone_ids: &[String], parameter: &str) -> Result<Vec<String>> {
    let mut ids = Vec::with_capacity(zone_ids.len());
    for zone_id in zone_ids {
        let id = lms_player_id(zone_id, parameter)?;
        if ids.contains(&id) {
            return Err(anyhow!("{parameter} must not contain duplicate zones"));
        }
        ids.push(id);
    }
    Ok(ids)
}

/// Read an integer that LMS may emit as either a JSON number or a JSON string.
///
/// LMS is not consistent about this, even within one response and even for the
/// same field across sibling queries. Observed on a single live 9.1.2 server:
/// `track_id` as a number, `mute` as `"0"`, `mixer volume ?` as `"42"`,
/// `mixer muting ?` as `1`, `playlist_cur_index` as `"0"`.
///
/// `Value::as_i64()` alone silently yields `None` on the string form, which is
/// how a parse bug becomes an empty result instead of an error (see #407).
fn lms_i64(value: Option<&Value>) -> Option<i64> {
    let value = value?;
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|f| f as i64))
        .or_else(|| value.as_str().and_then(|s| s.trim().parse::<i64>().ok()))
}

/// Read an LMS 0/1 flag as a bool, tolerating number, string, `null` and absent.
///
/// LMS omits a player pref entirely when it was never written and returns
/// `null` for it from the dedicated query, so "not present" and "not set" both
/// mean false. Anything non-zero means true.
fn lms_flag(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(v) => lms_i64(Some(v)).map(|n| n != 0).unwrap_or(false),
    }
}

/// Read the entity id from a row of one of LMS's `<type>s_loop` arrays.
///
/// LMS keys these `<type>_id` — `album_id` in `albums_loop`, `contributor_id` in
/// `contributors_loop`, `track_id` in `tracks_loop`. `Slim::Control::Queries`
/// derives the loop name and the key from the same variable on adjacent lines
/// (`"${type}s_loop"` / `"${type}_id"`), so a response that contains the loop
/// necessarily uses that key.
///
/// The plain `id` fallback is deliberate belt-and-braces for any LMS version
/// that might differ; this repo declares no minimum LMS version, so the fix must
/// not depend on one. Reading only `id` is the #407 defect that made
/// `search_library()` return an empty vec against every real server.
fn loop_entity_id(row: &Value, entity: &str) -> Option<i64> {
    lms_i64(row.get(format!("{}_id", entity)).or_else(|| row.get("id"))).filter(|id| *id > 0)
}

/// Tags the `albums` query must request for a browsable album row.
///
/// **A `tags:` parameter replaces the default tag set, it does not extend it.**
/// LMS returns exactly the fields the letters name, so anything the caller
/// reads has to appear here. `l` album title, `a` artist, `c` coverid,
/// `J` artwork_track_id (see docs/lyrion.md's tag table).
///
/// Recorded proof of both halves, from live Lyrion 9.1.2:
/// `tests/fixtures/lms/collections_albums_artwork_only.json` (what `tags:cJ`
/// returns -- no `album` key at all) and
/// `collections_albums_with_display_tags.json`.
pub(crate) const ALBUM_DISPLAY_TAGS: &str = "tags:lacJ";

/// Tags the `titles` and `playlists tracks` queries must request.
///
/// Both always return `title`, but the artist subtitle is a tagged field like
/// any other: `tags:cJ` drops it. See [`ALBUM_DISPLAY_TAGS`].
pub(crate) const TRACK_DISPLAY_TAGS: &str = "tags:acJ";

/// This row's artwork handle, for `hifi_collections`' image ref (#549), or
/// `None` when LMS has none for it (an artist row, or a track/album LMS
/// never assigned art to) -- absent honestly, not a placeholder.
///
/// `coverid` first, `artwork_track_id` second: the same preference
/// `get_player_status` already applies for now-playing art (`artwork_id`,
/// this file's docs on #407), here read from `tags:cJ` requested on the
/// `albums`/`titles`/`playlists tracks` queries this helper serves. Both
/// tags resolve to a plain id `AppState::get_image`'s LMS branch already
/// knows how to turn into `/music/<id>/cover.jpg` through
/// `LmsAdapter::get_artwork` -- no different from a coverid read off
/// `status`.
fn row_image_key(row: &Value) -> Option<String> {
    let read = |key: &str| -> Option<String> {
        row.get(key).and_then(|v| {
            v.as_str()
                .map(str::to_string)
                .or_else(|| v.as_i64().map(|n| n.to_string()))
        })
    };
    read("coverid").or_else(|| read("artwork_track_id"))
}

/// The next page's offset for a `hifi_collections` page, or `None` at the
/// end.
///
/// LMS's own `count` field is the total across the whole query (not just
/// this page), so it is the source of truth when present. A response
/// missing it (the mock's catch-all shape, or an unmodeled LMS reply) falls
/// back to "this page was full" as the continue signal -- the same
/// length-based heuristic Music Assistant's `collections_content` uses for
/// its `library_items` paging.
fn next_offset(raw: &Value, offset: usize, limit: usize, page_len: usize) -> Option<u64> {
    match raw.get("count").and_then(Value::as_u64) {
        Some(total) => (total > (offset + limit) as u64).then_some((offset + limit) as u64),
        None => (page_len == limit).then_some((offset + limit) as u64),
    }
}

/// What LMS actually does when a request fails, spelled out once.
///
/// `Slim::Web::JSONRPC::requestMethod` calls `closeHTTPSocket()` on any
/// `$request->isStatusError()`. There is no branch that emits a JSON-RPC `error`
/// object, so the client sees zero bytes and a closed socket - with no HTTP
/// status line at all - for every one of: unknown command (104), bad parameters
/// (102), unknown player id (103), bad server config (105), and Perl exceptions.
/// Verified against live Lyrion 9.1.2; byte counts in
/// `tests/fixtures/lms/PROVENANCE.md`.
///
/// Because the transport cannot tell these apart, a failure here must not be
/// read as "this LMS does not support that command". Use `can <verb> ?` for
/// capability questions, treating `_can: 1` as proof of presence and never
/// `_can: 0` as proof of absence.
const LMS_FAILURE_NOTE: &str = "LMS signals every request error this way - \
unknown command, bad parameters, unknown player id, or bad server config are \
indistinguishable at the transport, and it never returns a JSON-RPC error \
object. A network fault looks the same, so do not read this as 'unsupported'.";

/// Turn a `reqwest` transport failure into a message that says which of LMS's
/// failure modes it actually is.
///
/// Worth distinguishing, because they call for different responses: an
/// unreachable server is a configuration or network problem, whereas a socket
/// closed *after* the request was accepted is LMS rejecting the request, and the
/// operator needs to know that "rejected" carries no detail about why.
fn describe_send_failure(command: &str, error: &reqwest::Error) -> anyhow::Error {
    if error.is_connect() {
        return anyhow!(
            "cannot reach LMS to run `{}`: {}. Check the configured host and port.",
            command,
            error
        );
    }
    if error.is_timeout() {
        return anyhow!(
            "LMS did not answer `{}` before the request timed out: {}. LMS can \
             also hang rather than close on some bad-parameter requests, which \
             looks identical to a slow server.",
            command,
            error
        );
    }
    anyhow!(
        "LMS closed the connection without answering `{}`. {} (transport: {})",
        command,
        LMS_FAILURE_NOTE,
        error
    )
}

/// Render a `slim.request` command array for an error message or log line.
fn describe_command(player_id: Option<&str>, params: &[Value]) -> String {
    let words: Vec<String> = params
        .iter()
        .map(|p| match p {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .collect();
    format!(
        "{} {}",
        player_id.unwrap_or("<server>"),
        truncate_for_log(&words.join(" "))
    )
}

/// Keep diagnostics readable when LMS or a stray responder returns something huge.
fn truncate_for_log(text: &str) -> String {
    const MAX: usize = 200;
    if text.chars().count() <= MAX {
        return text.to_string();
    }
    let head: String = text.chars().take(MAX).collect();
    format!("{}…", head)
}

/// Saved config for persistence
#[derive(Debug, Clone, Serialize, Deserialize)]
struct SavedLmsConfig {
    host: String,
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    username: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    password: Option<String>,
}

fn config_path() -> PathBuf {
    get_config_file_path(LMS_CONFIG_FILE)
}

const DEFAULT_PORT: u16 = 9000;
/// CLI telnet port for event subscription
const CLI_PORT: u16 = 9090;
/// Default poll interval in seconds (when no subscription active)
const DEFAULT_POLL_INTERVAL_SECS: u64 = 2;
/// Multiplier for poll interval when subscription is active (15x base interval)
const SUBSCRIPTION_INTERVAL_MULTIPLIER: u64 = 15;

/// How many results [`LmsAdapter::assert_library_id_exists`] (#396) requests
/// when re-validating a `Library` ref by its mint-time title.
///
/// Sized well above any plausible count of library entries sharing one exact
/// title, so the search-by-title re-validation actually surfaces the target
/// id rather than missing it purely because too many other entries share the
/// same title. See that method's own doc comment for why this number exists
/// at all, and `tests/mcp_refs.rs::lms_a_valid_ref_beyond_the_validation_search_limit_still_resolves`
/// for the test that found the original value (50) was not generous enough.
const LIBRARY_VALIDATION_SEARCH_LIMIT: usize = 500;

/// Get the poll interval from LMS_POLL_INTERVAL env var, or use default
fn get_poll_interval() -> Duration {
    std::env::var("LMS_POLL_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .map(Duration::from_secs)
        .unwrap_or(Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS))
}

/// Get the poll interval when subscription is active (15x base interval)
fn get_poll_interval_with_subscription() -> Duration {
    let base = get_poll_interval();
    Duration::from_secs(base.as_secs() * SUBSCRIPTION_INTERVAL_MULTIPLIER)
}
/// TCP read timeout for CLI subscription (detect unresponsive LMS)
const CLI_READ_TIMEOUT: Duration = Duration::from_secs(120);

// =============================================================================
// CLI Event Parsing
// =============================================================================

/// Now playing update data for bus emission
struct NowPlayingUpdate {
    player_id: String,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    image_key: Option<String>,
}

/// Parsed CLI event from LMS
#[derive(Debug, Clone, PartialEq)]
pub enum CliEvent {
    /// Playlist changed (newsong, play, stop, pause, etc.)
    Playlist {
        player_id: String,
        command: String,
        /// Track name for newsong events
        track_name: Option<String>,
        /// Playlist index for newsong events
        index: Option<u32>,
    },
    /// Mixer changed (volume, muting)
    Mixer {
        player_id: String,
        param: String,
        /// Value is None when parsing fails (avoids silent conversion to 0)
        /// f32 to support fractional steps (LMS uses 2.5% steps)
        value: Option<f32>,
        /// True if value is relative (starts with + or -), false if absolute
        is_relative: bool,
    },
    /// Power state changed
    Power { player_id: String, state: bool },
    /// Client connected/disconnected/new
    Client { player_id: String, action: String },
    /// Unknown/unparsed event (logged but not acted upon)
    Unknown { raw_line: String },
}

/// Parse a raw CLI event line from LMS
///
/// LMS CLI events are URL-encoded, space-separated lines:
/// `<playerid> <command> <args...>`
///
/// Example events:
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz playlist newsong Track%20Name 5`
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz mixer volume 75`
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz power 1`
/// - `00%3A04%3A20%3Axx%3Ayy%3Azz client new`
pub fn parse_cli_event(line: &str) -> CliEvent {
    let line = line.trim();
    if line.is_empty() {
        return CliEvent::Unknown {
            raw_line: line.to_string(),
        };
    }

    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 2 {
        return CliEvent::Unknown {
            raw_line: line.to_string(),
        };
    }

    // Decode player ID (URL-encoded MAC address)
    let player_id = urlencoding::decode(parts[0])
        .map(|s| s.into_owned())
        .unwrap_or_else(|_| parts[0].to_string());

    let command = parts[1];

    match command {
        "playlist" => {
            let subcommand = parts.get(2).copied().unwrap_or("");
            let track_name = parts.get(3).and_then(|s| {
                urlencoding::decode(s)
                    .ok()
                    .map(|decoded| decoded.into_owned())
            });
            let index = parts.get(4).and_then(|s| s.parse().ok());

            CliEvent::Playlist {
                player_id,
                command: subcommand.to_string(),
                track_name,
                index,
            }
        }
        "mixer" => {
            let param = parts.get(2).copied().unwrap_or("volume");
            let raw_value = parts.get(3).copied().unwrap_or("");
            let value: Option<f32> = raw_value.parse().ok();

            // Detect relative values: starts with + OR value is negative
            // Negative values don't make sense as absolute volumes, so they're always relative
            let is_relative = raw_value.starts_with('+') || value.map(|v| v < 0.0).unwrap_or(false);

            CliEvent::Mixer {
                player_id,
                param: param.to_string(),
                value,
                is_relative,
            }
        }
        "power" => {
            let state = parts.get(2).is_some_and(|s| *s == "1");

            CliEvent::Power { player_id, state }
        }
        "client" => {
            let action = parts.get(2).copied().unwrap_or("unknown");

            CliEvent::Client {
                player_id,
                action: action.to_string(),
            }
        }
        _ => CliEvent::Unknown {
            raw_line: line.to_string(),
        },
    }
}

/// Shared JSON-RPC client operations for LMS
/// Extracted to avoid code duplication between LmsAdapter and the polling task
#[derive(Clone)]
struct LmsRpc {
    state: Arc<RwLock<LmsState>>,
    client: Client,
}

impl LmsRpc {
    fn new(state: Arc<RwLock<LmsState>>, client: Client) -> Self {
        Self { state, client }
    }

    async fn base_url(&self) -> Result<String> {
        let state = self.state.read().await;
        let host = state
            .host
            .as_ref()
            .ok_or_else(|| anyhow!("LMS host not configured"))?;
        Ok(format!("http://{}:{}", host, state.port))
    }

    async fn execute(&self, player_id: Option<&str>, params: Vec<Value>) -> Result<Value> {
        let base_url = self.base_url().await?;
        let url = format!("{}/jsonrpc.js", base_url);

        let body = json!({
            "id": LMS_REQUEST_ID,
            "method": "slim.request",
            "params": [player_id.unwrap_or(""), params]
        });

        debug!(
            player_id = player_id.unwrap_or("<server>"),
            params = ?body["params"][1],
            "LMS request"
        );

        let mut request = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .json(&body);

        // Add basic auth if configured
        {
            let state = self.state.read().await;
            if let (Some(username), Some(password)) = (&state.username, &state.password) {
                request = request.basic_auth(username, Some(password));
            }
        }

        // LMS's failure signal is the *absence* of a response, so every branch
        // below has to describe the command that provoked it or the log is
        // useless. See describe_command / LMS_FAILURE_NOTE.
        let command = describe_command(player_id, &params);

        let response = request
            .send()
            .await
            .map_err(|e| describe_send_failure(&command, &e))?;

        if !response.status().is_success() {
            return Err(anyhow!(
                "LMS request failed for `{}`: HTTP {}",
                command,
                response.status()
            ));
        }

        // Read bytes rather than deserialising directly: a reverse proxy can turn
        // LMS's closed socket into a valid empty 200, and `response.json()` would
        // report that as "EOF while parsing a value", which names neither the
        // cause nor the command.
        let body = response
            .bytes()
            .await
            .map_err(|e| describe_send_failure(&command, &e))?;

        if body.is_empty() {
            return Err(anyhow!(
                "LMS returned an empty body for `{}`. {}",
                command,
                LMS_FAILURE_NOTE
            ));
        }

        let data: Value = serde_json::from_slice(&body).map_err(|e| {
            anyhow!(
                "LMS returned a body that is not JSON for `{}`: {} (first 200 \
                 bytes: {:?})",
                command,
                e,
                String::from_utf8_lossy(&body[..body.len().min(200)])
            )
        })?;

        debug!(
            player_id = player_id.unwrap_or("<server>"),
            result = ?data.get("result"),
            "LMS response"
        );

        // Replaces a check for a JSON-RPC `error` member, which was dead code:
        // LMS never emits one (Slim::Web::JSONRPC::requestMethod closes the
        // socket instead). A successful slim.request always carries `result`, so
        // its absence means something that is not a healthy LMS answered - which
        // also catches the hypothetical `error`-emitting responder the old check
        // was aiming at, instead of silently handing callers Value::Null.
        data.get("result").cloned().ok_or_else(|| {
            anyhow!(
                "LMS reply for `{}` has no `result` member: {}",
                command,
                truncate_for_log(&data.to_string())
            )
        })
    }

    async fn get_player_status(&self, player_id: &str) -> Result<LmsPlayer> {
        let base_url = self.base_url().await?;
        let result = self
            .execute(
                Some(player_id),
                vec![json!("status"), json!("-"), json!(1), json!("tags:aAdltKc")],
            )
            .await?;

        let playlist_loop = result
            .get("playlist_loop")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .cloned()
            .unwrap_or(Value::Null);

        let mode = result
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("stop");
        let state = match mode {
            "play" => "playing",
            "pause" => "paused",
            _ => "stopped",
        };

        // Handle artwork URL
        let mut artwork_url = playlist_loop
            .get("artwork_url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());

        if let Some(ref url) = artwork_url {
            if url.starts_with('/') {
                artwork_url = Some(format!("{}{}", base_url, url));
            }
        }

        // NOTE (#407): the artwork_track_id arm never fires, because
        // artwork_track_id is tag `J` and the tags string above does not request
        // it - so artwork falls through to the track's own `id`. Adding `J` is
        // free on the wire but would change image_key values for tracks with no
        // coverid, and image_key is client-visible, so it is left alone here and
        // flagged for #401/#403 rather than changed inside a defect fix.
        let artwork_id = playlist_loop
            .get("coverid")
            .or_else(|| playlist_loop.get("artwork_track_id"))
            .or_else(|| playlist_loop.get("id"))
            .and_then(|v| {
                // Try string first, then try numeric conversion
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| v.as_i64().map(|n| n.to_string()))
            });

        // LMS negates the volume pref while a player is muted, so `mixer volume`
        // arrives as e.g. -42 - and it used to be handed straight to a
        // VolumeControl declared min: 0.0, i.e. clients were shown a negative
        // percentage. Report the magnitude, and treat the sign as a second mute
        // signal.
        //
        // The sign LAGS the mute by roughly 0.8s (mixer muting schedules a fade
        // and only writes the negated value when it completes), so it can produce
        // a false negative but never a false positive. That is what makes it safe
        // to OR with the `mute` pref from `players`, which flips immediately.
        // Timing measurements: tests/fixtures/lms/PROVENANCE.md.
        let raw_volume = result
            .get("mixer volume")
            .and_then(|v| v.as_f64())
            .or_else(|| lms_i64(result.get("mixer volume")).map(|n| n as f64))
            .unwrap_or(0.0);

        Ok(LmsPlayer {
            playerid: player_id.to_string(),
            state: state.to_string(),
            mode: mode.to_string(),
            power: lms_flag(result.get("power")),
            volume: raw_volume.abs().round() as i32,
            muted: raw_volume < 0.0,
            playlist_tracks: result
                .get("playlist_tracks")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u32,
            playlist_cur_index: result
                .get("playlist_cur_index")
                .and_then(|v| v.as_u64())
                .map(|n| n as u32),
            time: result.get("time").and_then(|v| v.as_f64()).unwrap_or(0.0),
            duration: playlist_loop
                .get("duration")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0),
            title: playlist_loop
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artist: playlist_loop
                .get("artist")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            album: playlist_loop
                .get("album")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            artwork_track_id: artwork_id.clone(),
            coverid: artwork_id,
            artwork_url,
            ..Default::default()
        })
    }

    async fn get_players(&self) -> Result<Vec<LmsPlayer>> {
        // `playerprefs:` asks playersQuery to include named per-player prefs in
        // each players_loop entry, so mute state arrives inside the one `players`
        // call this poll already makes: zero extra round-trips, whatever the poll
        // rate or player count. The alternative - `mixer muting ?` per player per
        // poll - would nearly double request volume at the 2s default interval.
        //
        // `status` cannot supply this: it carries no mute key at all. Verified
        // against live Lyrion 9.1.2 (tests/fixtures/lms/PROVENANCE.md).
        let result = self
            .execute(
                None,
                vec![
                    json!("players"),
                    json!(0),
                    json!(100),
                    json!(format!("playerprefs:{}", LMS_MUTE_PREF)),
                ],
            )
            .await?;

        let players_loop = result
            .get("players_loop")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        Ok(players_loop
            .into_iter()
            .map(|p| LmsPlayer {
                playerid: p
                    .get("playerid")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                name: p
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                model: p
                    .get("model")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Unknown")
                    .to_string(),
                connected: lms_flag(p.get("connected")),
                power: lms_flag(p.get("power")),
                ip: p.get("ip").and_then(|v| v.as_str()).map(|s| s.to_string()),
                // Absent means the pref was never written, i.e. not muted. Note
                // that absence is ALSO what an LMS too old for `playerprefs:`
                // would produce - it ignores unknown tagged params silently
                // rather than erroring - so a `false` here is not proof of
                // unmuted. The negative-volume signal in get_player_status()
                // covers that case, needing no tagged parameter.
                muted: lms_flag(p.get(LMS_MUTE_PREF)),
                ..Default::default()
            })
            .collect())
    }
}

/// LMS Player information
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmsPlayer {
    pub playerid: String,
    pub name: String,
    pub model: String,
    pub connected: bool,
    pub power: bool,
    pub ip: Option<String>,
    // Status fields
    pub state: String,
    pub mode: String,
    pub volume: i32,
    pub playlist_tracks: u32,
    pub playlist_cur_index: Option<u32>,
    pub time: f64,
    pub duration: f64,
    pub title: String,
    pub artist: String,
    pub album: String,
    pub artwork_track_id: Option<String>,
    pub coverid: Option<String>,
    pub artwork_url: Option<String>,
    /// Whether the player is muted.
    ///
    /// From the per-player `mute` pref, requested via `playerprefs:mute` on the
    /// `players` query so it costs no extra round-trip, OR-ed with a negative
    /// `mixer volume` in `status` (LMS negates the volume while muted). `status`
    /// itself carries no mute key.
    pub muted: bool,
}

impl Default for LmsPlayer {
    fn default() -> Self {
        Self {
            playerid: String::new(),
            name: String::new(),
            model: "Unknown".to_string(),
            connected: false,
            power: false,
            ip: None,
            state: "stopped".to_string(),
            mode: "stop".to_string(),
            volume: 0,
            playlist_tracks: 0,
            playlist_cur_index: None,
            time: 0.0,
            duration: 0.0,
            title: String::new(),
            artist: String::new(),
            album: String::new(),
            artwork_track_id: None,
            coverid: None,
            artwork_url: None,
            muted: false,
        }
    }
}

/// LMS connection status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmsStatus {
    pub connected: bool,
    pub host: Option<String>,
    pub port: u16,
    pub player_count: usize,
    pub players: Vec<LmsPlayerInfo>,
    /// Whether CLI subscription is active (real-time events vs polling)
    pub cli_subscription_active: bool,
    /// Effective poll interval in seconds (2s base, 30s when CLI active)
    pub poll_interval_secs: u64,
}

/// Summary information about an LMS player for status reporting
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmsPlayerInfo {
    /// Player MAC address identifier
    pub playerid: String,
    /// Display name of the player
    pub name: String,
    /// Current playback state (playing, paused, stopped)
    pub state: String,
    /// Whether the player is connected to LMS
    pub connected: bool,
}

/// Type of search result from LMS
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum LmsSearchResultType {
    Album,
    Artist,
    Track,
}

impl std::fmt::Display for LmsSearchResultType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LmsSearchResultType::Album => write!(f, "album"),
            LmsSearchResultType::Artist => write!(f, "artist"),
            LmsSearchResultType::Track => write!(f, "track"),
        }
    }
}

/// A search result from LMS (library or streaming service)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LmsSearchResult {
    /// Type of result (album, artist, track)
    pub result_type: LmsSearchResultType,
    /// Entity ID in LMS database (for library items, 0 for streaming)
    pub id: i64,
    /// Primary title (album name, artist name, or track title)
    pub title: String,
    /// Artist name (for albums and tracks)
    pub artist: Option<String>,
    /// Album name (for tracks)
    pub album: Option<String>,
    /// Direct playback URL (for streaming service items)
    pub url: Option<String>,
    /// String-based item_id for globalsearch results (used with globalsearch playlist)
    pub item_id: Option<String>,
}

/// Action to take when playing search results
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LmsPlayAction {
    /// Clear queue and start playing immediately
    #[default]
    Play,
    /// Add to end of queue without interrupting playback
    Queue,
    /// Insert next in queue (play next)
    Insert,
}

impl LmsPlayAction {
    /// Parse action from string
    pub fn parse(s: Option<&str>) -> Self {
        match s {
            Some("queue" | "add") => LmsPlayAction::Queue,
            Some("insert" | "next") => LmsPlayAction::Insert,
            _ => LmsPlayAction::Play,
        }
    }

    /// Get the LMS playlistcontrol cmd parameter
    pub fn to_lms_cmd(&self) -> &'static str {
        match self {
            LmsPlayAction::Play => "load",
            LmsPlayAction::Queue => "add",
            LmsPlayAction::Insert => "insert",
        }
    }
}

/// One resolved way to play/queue/insert an LMS result, independent of how it
/// was found.
///
/// Extracted from `search_and_play`'s three-way branch (issue #396) so
/// `hifi_play_ref` can execute a *specific* target — not `results[0]` — via
/// [`LmsAdapter::play_target`], without `LmsRpc::execute` becoming public.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LmsPlayTarget {
    /// `playlistcontrol cmd:<...> <kind>_id:<id>`. Durable: an LMS database
    /// id, surviving both UHC and LMS restarts. Invalidated only by a rescan
    /// that reassigns ids — which is why [`LmsAdapter::play_target`] verifies
    /// it still exists before mutating the queue.
    Library { kind: LmsSearchResultType, id: i64 },
    /// `playlist <load|add|insert> <url>`. Mostly durable: a plugin-resolvable
    /// URL that survives restarts and dies only if the plugin is removed or a
    /// subscription lapses.
    Url { url: String },
    /// `globalsearch playlist <play|add|insert> item_id:<id>`.
    ///
    /// **Not an identifier.** Verified against live Lyrion 9.1.2: this is a
    /// positional breadcrumb of the form `<session>.<index>.<index>` that
    /// changes across identical calls, and a *fabricated* session id resolves
    /// successfully — LMS silently re-walks the position on a cache miss — so
    /// a stale or wrong value here plays the wrong content with **no error**.
    /// #396's hard rule: never mint a ref through this variant. It exists
    /// only so `play_target` can still serve `search_and_play`'s existing,
    /// unref'd first-result path unchanged.
    GlobalSearchItem { item_id: String },
}

impl LmsPlayTarget {
    /// The target `search_and_play` resolves its (only ever first) hit to,
    /// preserving the exact precedence that method always used — `item_id`,
    /// then `url`, then a positive library id — so extracting `play_target`
    /// changes no observable behavior on that path.
    pub fn from_search_hit(result: &LmsSearchResult) -> Option<Self> {
        if let Some(item_id) = &result.item_id {
            Some(Self::GlobalSearchItem {
                item_id: item_id.clone(),
            })
        } else if let Some(url) = &result.url {
            Some(Self::Url { url: url.clone() })
        } else if result.id > 0 {
            Some(Self::Library {
                kind: result.result_type,
                id: result.id,
            })
        } else {
            None
        }
    }

    /// The target #396's ref minter should record for this result, or `None`
    /// if nothing about it is safe to mint a ref for.
    ///
    /// Different precedence than [`Self::from_search_hit`], and deliberately
    /// so: a ref is a promise that outlives this call, so it prefers whichever
    /// handle is durable — `Library` (a persistent database id) over `Url`
    /// (mostly durable) over minting nothing at all. `GlobalSearchItem` is
    /// never mintable, full stop; see that variant's own docs for why.
    pub fn mintable(result: &LmsSearchResult) -> Option<Self> {
        if result.id > 0 {
            Some(Self::Library {
                kind: result.result_type,
                id: result.id,
            })
        } else {
            result.url.clone().map(|url| Self::Url { url })
        }
    }
}

/// Internal state
struct LmsState {
    host: Option<String>,
    port: u16,
    username: Option<String>,
    password: Option<String>,
    connected: bool,
    running: bool,
    players: HashMap<String, LmsPlayer>,
    /// Whether CLI subscription is active (for reduced polling frequency)
    cli_subscription_active: bool,
}

/// The one lossless ingress for LMS facts when the reliable runtime is composed.
///
/// LMS has two observers -- the HTTP poller and the CLI subscription.  They intentionally share
/// this bridge rather than each owning a source cursor: a full player snapshot from either path is
/// an observation of the same server/player state.  Serializing sequence allocation *and* ingress
/// admission prevents a scheduler race from making a newer CLI update look stale behind an older
/// poll result.  Every LMS publication is a Snapshot, so an interrupted observer can reconcile a
/// detected sequence gap with its next complete player readback.
#[derive(Clone)]
pub struct LmsRuntimeBridge {
    ingress: ProjectionIngress,
    commands: CommandGateway,
    publication: Arc<Mutex<LmsPublicationCursor>>,
    publication_gate: Arc<Semaphore>,
}

#[derive(Default)]
struct LmsPublicationCursor {
    epoch: u64,
    sequence: u64,
}

impl LmsRuntimeBridge {
    pub fn new(ingress: ProjectionIngress, commands: CommandGateway) -> Self {
        Self {
            ingress,
            commands,
            publication: Arc::new(Mutex::new(LmsPublicationCursor::default())),
            publication_gate: Arc::new(Semaphore::new(1)),
        }
    }

    fn commands(&self) -> CommandGateway {
        self.commands.clone()
    }

    async fn publish_zone(
        &self,
        zone: Zone,
        caused_by: Option<crate::bus::runtime::CommandId>,
    ) -> Result<()> {
        self.publish(
            ProjectionPayload::Zone(Box::new(zone.clone())),
            format!("zone:{}", zone.zone_id),
            caused_by,
        )
        .await
    }

    async fn publish_removed(&self, zone_id: PrefixedZoneId) -> Result<()> {
        self.publish(
            ProjectionPayload::ZoneRemoved {
                zone_id: zone_id.clone(),
            },
            format!("zone:{zone_id}"),
            None,
        )
        .await
    }

    async fn publish(
        &self,
        payload: ProjectionPayload,
        key: String,
        caused_by: Option<crate::bus::runtime::CommandId>,
    ) -> Result<()> {
        // Keep the single-owner permit through admission. A sequence is only meaningful in the
        // order the actor receives it; allocating under one task and admitting under another
        // permits exactly the dual-ingestion inversion this bridge exists to remove. The data lock
        // itself is released before awaiting ingress, satisfying the no-await-under-lock invariant.
        let _publication_permit = self
            .publication_gate
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| anyhow!("LMS publication lane closed"))?;
        let (epoch, sequence) = {
            let mut cursor = self.publication.lock().await;
            cursor.sequence = cursor.sequence.saturating_add(1);
            (cursor.epoch, cursor.sequence)
        };
        self.ingress
            .submit(ProjectionUpdate {
                source: ProjectionSource {
                    adapter: "lms".to_string(),
                    instance: None,
                    epoch,
                },
                sequence,
                kind: ProjectionKind::Snapshot,
                caused_by,
                entries: vec![ProjectionEntry { key, payload }],
            })
            .await
            .map_err(|_| anyhow!("LMS reliable projection ingress stopped"))?;
        Ok(())
    }
}

impl Default for LmsState {
    fn default() -> Self {
        Self {
            host: None,
            port: DEFAULT_PORT,
            username: None,
            password: None,
            connected: false,
            running: false,
            players: HashMap::new(),
            cli_subscription_active: false,
        }
    }
}

/// LMS Adapter
#[derive(Clone)]
pub struct LmsAdapter {
    state: Arc<RwLock<LmsState>>,
    rpc: LmsRpc,
    bus: SharedBus,
    runtime_bridge: Option<Arc<LmsRuntimeBridge>>,
    /// Wrapped in RwLock to allow creating fresh token on restart
    shutdown: Arc<RwLock<CancellationToken>>,
    /// The polling supervisor.  Stop waits for this task so a readback from an
    /// old LMS server cannot cross a reconfiguration projection retirement.
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl LmsAdapter {
    pub fn new(bus: SharedBus) -> Self {
        Self::new_with_runtime(bus, None)
    }

    fn new_with_runtime(bus: SharedBus, runtime_bridge: Option<Arc<LmsRuntimeBridge>>) -> Self {
        let state = Arc::new(RwLock::new(LmsState::default()));
        #[allow(clippy::expect_used)] // HTTP client creation only fails if TLS setup fails
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("Failed to create HTTP client");
        let rpc = LmsRpc::new(state.clone(), client);
        let adapter = Self {
            state,
            rpc,
            bus,
            runtime_bridge,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            task: Arc::new(Mutex::new(None)),
        };
        // Load saved config synchronously at startup
        adapter.load_config_sync();
        adapter
    }

    /// Load config from disk (sync, for startup)
    /// Issue #76: Uses read_config_file for backwards-compatible fallback
    fn load_config_sync(&self) {
        // read_config_file checks subdir first, falls back to root for legacy files
        if let Some(content) = read_config_file(LMS_CONFIG_FILE) {
            match serde_json::from_str::<SavedLmsConfig>(&content) {
                Ok(saved) => {
                    // Use try_write to avoid async in sync context
                    if let Ok(mut state) = self.state.try_write() {
                        state.host = Some(saved.host.clone());
                        state.port = saved.port;
                        state.username = saved.username;
                        state.password = saved.password;
                        tracing::info!(
                            "Loaded LMS config from disk: {}:{}",
                            saved.host,
                            saved.port
                        );
                    }
                }
                Err(e) => tracing::warn!("Failed to parse LMS config: {}", e),
            }
        }
    }

    /// Save config to disk
    async fn save_config(&self) {
        let state = self.state.read().await;
        if let Some(ref host) = state.host {
            let saved = SavedLmsConfig {
                host: host.clone(),
                port: state.port,
                username: state.username.clone(),
                password: state.password.clone(),
            };
            let path = config_path();
            // Ensure config directory exists
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            match serde_json::to_string_pretty(&saved) {
                Ok(json) => {
                    if let Err(e) = std::fs::write(&path, json) {
                        tracing::error!("Failed to save LMS config: {}", e);
                    } else {
                        tracing::info!("Saved LMS config to disk");
                    }
                }
                Err(e) => tracing::error!("Failed to serialize LMS config: {}", e),
            }
        }
    }

    /// Attempt auto-discovery and configure if exactly one server is found.
    /// Returns Ok(true) if auto-configured, Ok(false) if no single server found, Err on failure.
    ///
    /// Only auto-configures if:
    /// - No existing configuration (host is None)
    /// - Exactly one LMS server responds to discovery
    pub async fn auto_discover_and_configure(&self) -> Result<bool> {
        // Don't auto-configure if already configured
        if self.is_configured().await {
            tracing::debug!("LMS already configured, skipping auto-discovery");
            return Ok(false);
        }

        tracing::info!("Attempting LMS auto-discovery...");

        let servers = discover_lms_servers(None).await?;

        match servers.len() {
            0 => {
                tracing::info!("LMS auto-discovery: no servers found");
                Ok(false)
            }
            1 => {
                let server = &servers[0];
                tracing::info!(
                    "LMS auto-discovery: found single server '{}' at {}:{}",
                    server.name,
                    server.host,
                    server.json_port
                );
                // Auto-configure with discovered settings
                self.configure(server.host.clone(), Some(server.json_port), None, None)
                    .await;
                Ok(true)
            }
            n => {
                tracing::info!(
                    "LMS auto-discovery: found {} servers, not auto-configuring (manual selection required)",
                    n
                );
                for server in &servers {
                    tracing::info!(
                        "  - '{}' at {}:{}",
                        server.name,
                        server.host,
                        server.json_port
                    );
                }
                Ok(false)
            }
        }
    }

    /// Configure the LMS connection
    pub async fn configure(
        &self,
        host: String,
        port: Option<u16>,
        username: Option<String>,
        password: Option<String>,
    ) {
        {
            let mut state = self.state.write().await;
            state.host = Some(host);
            state.port = port.unwrap_or(DEFAULT_PORT);
            state.username = username;
            state.password = password;
            state.connected = false;
        }
        // Persist to disk
        self.save_config().await;
    }

    /// Check if configured
    pub async fn is_configured(&self) -> bool {
        self.state.read().await.host.is_some()
    }

    /// Get connection status
    pub async fn get_status(&self) -> LmsStatus {
        let state = self.state.read().await;
        let base_interval = get_poll_interval();
        let effective_interval = if state.cli_subscription_active {
            get_poll_interval_with_subscription()
        } else {
            base_interval
        };
        LmsStatus {
            connected: state.connected,
            host: state.host.clone(),
            port: state.port,
            player_count: state.players.len(),
            players: state
                .players
                .values()
                .map(|p| LmsPlayerInfo {
                    playerid: p.playerid.clone(),
                    name: p.name.clone(),
                    state: p.state.clone(),
                    connected: p.connected,
                })
                .collect(),
            cli_subscription_active: state.cli_subscription_active,
            poll_interval_secs: effective_interval.as_secs(),
        }
    }

    /// Get list of all players (delegates to shared RPC)
    pub async fn get_players(&self) -> Result<Vec<LmsPlayer>> {
        self.rpc.get_players().await
    }

    /// Get player status (delegates to shared RPC)
    pub async fn get_player_status(&self, player_id: &str) -> Result<LmsPlayer> {
        let player_id = strip_lms_prefix(player_id);
        self.rpc.get_player_status(player_id).await
    }

    // -------------------------------------------------------------------------
    // Sync groups (#510): LMS's native Squeezebox multi-room, wired to the
    // multiroom_status / multiroom_set_members / multiroom_ungroup contract
    // the Music Assistant adapter already implements
    // (`src/adapters/musicassistant.rs`).
    //
    // LMS sync is peer-based -- a group is just a set of players playing the
    // same stream in lockstep, with no server-side concept of a leader. The
    // contract, however, is leader/member shaped. This adapter resolves that
    // mismatch by treating "join the leader's group" literally: every add
    // is sent as `<leader> sync <member>`. Verified live against Lyrion 9.1.2
    // (issue #403's investigation): the **addressed** player in a `sync`
    // command becomes/stays the sync master, and the **argument** player
    // adopts the addressed player's queue, repeat and shuffle -- destroying
    // its own. Addressing the leader therefore keeps the leader's queue
    // intact and makes the member fall in behind it, exactly the semantics
    // `multiroom_set_members`'s `leader_zone_id` implies. Removal is
    // leader-independent: `<member> sync -` unsyncs that one player from
    // whatever group it is currently in.
    //
    // Because there is no server-side leader, `multiroom_status` cannot
    // recover *which* player was originally addressed -- only current group
    // membership via `syncgroups ?`. This adapter picks the first id LMS
    // reports for each group as that group's leader for display purposes.
    // That is a stable-but-arbitrary UHC convention, not an LMS fact.

    /// Read every active LMS sync group from `syncgroups ?` as a raw member
    /// list (no leader/member split yet). Groups of fewer than two players
    /// are not sync groups and are dropped.
    async fn read_sync_groups(&self) -> Result<Vec<Vec<String>>> {
        let result = self
            .rpc
            .execute(None, vec![json!("syncgroups"), json!("?")])
            .await?;
        let groups_loop = result
            .get("syncgroups_loop")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(groups_loop
            .iter()
            .filter_map(|group| group.get("sync_members").and_then(Value::as_str))
            .map(|members| {
                members
                    .split(',')
                    .map(str::trim)
                    .filter(|id| !id.is_empty())
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>()
            })
            .filter(|members| members.len() >= 2)
            .collect())
    }

    /// Report every active LMS sync group in the multiroom contract's shape.
    ///
    /// `can_set_members` is always `true`: unlike Music Assistant's per-leader
    /// `SET_MEMBERS` feature flag, every LMS player accepts `sync`
    /// unconditionally, so there is no per-group capability to check.
    pub async fn multiroom_status(&self) -> Result<Value> {
        let groups = self.read_sync_groups().await?;
        let groups = groups
            .into_iter()
            .filter_map(|members| {
                let (leader, rest) = members.split_first()?;
                Some(json!({
                    "leader_zone_id": PrefixedZoneId::lms(leader).to_string(),
                    "member_zone_ids": rest
                        .iter()
                        .map(|id| PrefixedZoneId::lms(id).to_string())
                        .collect::<Vec<_>>(),
                    "can_set_members": true,
                }))
            })
            .collect::<Vec<_>>();
        Ok(json!({ "groups": groups }))
    }

    /// Sync `add_zone_ids` into `leader_zone_id`'s group and unsync
    /// `remove_zone_ids`, then return the refreshed `multiroom_status`.
    pub async fn set_group_members(
        &self,
        leader_zone_id: &str,
        add_zone_ids: &[String],
        remove_zone_ids: &[String],
    ) -> Result<Value> {
        if add_zone_ids.is_empty() && remove_zone_ids.is_empty() {
            return Err(anyhow!("LMS group membership needs at least one member"));
        }
        let leader_id = lms_player_id(leader_zone_id, "leader_zone_id")?;
        let add_ids = lms_player_ids(add_zone_ids, "member_zone_ids_to_add")?;
        let remove_ids = lms_player_ids(remove_zone_ids, "member_zone_ids_to_remove")?;
        let players = self.rpc.get_players().await?;
        let player_exists = |id: &str| players.iter().any(|player| player.playerid == id);
        if !player_exists(&leader_id) {
            return Err(anyhow!("LMS group leader was not found"));
        }
        for player_id in add_ids.iter().chain(remove_ids.iter()) {
            if player_id == &leader_id {
                return Err(anyhow!("LMS group leader cannot be its own member input"));
            }
            if !player_exists(player_id) {
                return Err(anyhow!("LMS group member was not found"));
            }
        }
        for member_id in &add_ids {
            self.rpc
                .execute(Some(&leader_id), vec![json!("sync"), json!(member_id)])
                .await?;
        }
        for member_id in &remove_ids {
            self.rpc
                .execute(Some(member_id), vec![json!("sync"), json!("-")])
                .await?;
        }
        self.multiroom_status().await
    }

    /// Unsync each of `member_zone_ids` via its own `sync -`, independent of
    /// which group (if any) it currently belongs to, then return the
    /// refreshed `multiroom_status`.
    pub async fn ungroup_members(&self, member_zone_ids: &[String]) -> Result<Value> {
        let member_ids = lms_player_ids(member_zone_ids, "member_zone_ids")?;
        if member_ids.is_empty() {
            return Err(anyhow!("LMS ungroup needs at least one member"));
        }
        let players = self.rpc.get_players().await?;
        for member_id in &member_ids {
            if !players.iter().any(|player| player.playerid == *member_id) {
                return Err(anyhow!("LMS group member was not found"));
            }
        }
        for member_id in &member_ids {
            self.rpc
                .execute(Some(member_id), vec![json!("sync"), json!("-")])
                .await?;
        }
        self.multiroom_status().await
    }

    /// Start polling for player updates (internal - use Startable trait)
    async fn start_internal(&self) -> Result<()> {
        // Check if already running and set running=true atomically to prevent race
        {
            let mut state = self.state.write().await;
            if state.running {
                return Ok(());
            }
            state.running = true;
        }

        // Create fresh cancellation token for this run (previous token may be cancelled)
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        // Create AdapterHandle and spawn run_with_retry
        let adapter = self.clone();
        let bus = self.bus.clone();
        let handle = AdapterHandle::new(adapter, bus, shutdown);

        let task = tokio::spawn(async move {
            let _ = handle.run_with_retry(RetryConfig::default()).await;
        });
        *self.task.lock().await = Some(task);

        Ok(())
    }

    /// Update cached player information (delegates to shared helper)
    pub async fn update_players(&self) -> Result<()> {
        update_players_internal(
            &self.rpc,
            &self.state,
            &self.bus,
            self.runtime_bridge.as_deref(),
        )
        .await
    }

    /// Stop polling (internal - use Startable trait)
    async fn stop_internal(&self) {
        // Cancel background tasks first
        self.shutdown.read().await.cancel();

        // Do not let a request already in flight publish a snapshot from the
        // old server after the paired LMS projection has been retired.
        let task = self.task.lock().await.take();
        if let Some(task) = task {
            if let Err(error) = task.await {
                warn!(%error, "LMS polling supervisor did not stop cleanly");
            }
        }

        let host = {
            let mut state = self.state.write().await;
            state.connected = false;
            state.running = false;
            state.host.clone()
        };

        if let Some(host) = host {
            self.bus.publish(BusEvent::LmsDisconnected { host });
        }
    }

    /// Control player
    pub async fn control(&self, player_id: &str, command: &str, value: Option<i32>) -> Result<()> {
        self.execute_control_native(player_id, command, value)
            .await?;

        // Compatibility-only optimistic cache refresh. Runtime-routed commands instead perform a
        // synchronous coherent readback and publish that full observation with their correlation.
        let player_id = strip_lms_prefix(player_id).to_string();
        let state = self.state.clone();
        let rpc = self.rpc.clone();

        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if let Ok(status) = rpc.get_player_status(&player_id).await {
                let mut state = state.write().await;
                if let Some(player) = state.players.get_mut(&player_id) {
                    player.state = status.state;
                    player.mode = status.mode;
                    player.volume = status.volume;
                    player.time = status.time;
                }
            }
        });

        Ok(())
    }

    /// Execute one LMS control write without claiming that the visible projection changed.
    ///
    /// The reliable endpoint owns the required readback and correlated projection. Keeping this
    /// native-only primitive separate prevents an acknowledgement from becoming an accidental
    /// second state authority.
    async fn execute_control_native(
        &self,
        player_id: &str,
        command: &str,
        value: Option<i32>,
    ) -> Result<()> {
        let player_id = strip_lms_prefix(player_id);
        let params: Vec<Value> = match command {
            // Per real-world testing (issue #68), "play" handles both start and resume.
            // No need to check cached state - just send the command directly.
            "play" => vec![json!("play")],
            // "pause" without args toggles pause state - matches expected UI behavior
            "pause" => vec![json!("pause")],
            "stop" => vec![json!("stop")],
            "play_pause" => vec![json!("pause")], // Toggle
            "next" => vec![json!("playlist"), json!("index"), json!("+1")],
            "previous" | "prev" => vec![json!("playlist"), json!("index"), json!("-1")],
            "volume" | "vol_abs" => {
                let v = value.unwrap_or(50);
                vec![json!("mixer"), json!("volume"), json!(v)]
            }
            "vol_rel" => {
                let v = value.unwrap_or(0);
                let prefix = if v > 0 { "+" } else { "" };
                vec![
                    json!("mixer"),
                    json!("volume"),
                    json!(format!("{}{}", prefix, v)),
                ]
            }
            _ => return Err(anyhow!("Unknown command: {}", command)),
        };

        self.rpc.execute(Some(player_id), params).await?;
        Ok(())
    }

    /// Fetch and cache one complete player observation after a native write.  `players` supplies
    /// stable identity/capability fields (including mute pref); `status` supplies transport and
    /// now-playing state.  The returned Zone is the exact fact the reliable runtime commits.
    async fn coherent_player_readback(&self, player_id: &str) -> Result<Zone> {
        let player_id = strip_lms_prefix(player_id);
        let mut player = self
            .rpc
            .get_players()
            .await?
            .into_iter()
            .find(|candidate| candidate.playerid == player_id)
            .ok_or_else(|| anyhow!("LMS player '{}' is no longer available", player_id))?;
        let status = self.rpc.get_player_status(player_id).await?;
        player.state = status.state;
        player.mode = status.mode;
        player.power = status.power;
        player.volume = status.volume;
        player.muted = player.muted || status.muted;
        player.playlist_tracks = status.playlist_tracks;
        player.playlist_cur_index = status.playlist_cur_index;
        player.time = status.time;
        player.duration = status.duration;
        player.title = status.title;
        player.artist = status.artist;
        player.album = status.album;
        player.artwork_track_id = status.artwork_track_id;
        player.coverid = status.coverid;
        player.artwork_url = status.artwork_url;
        let zone = lms_player_to_zone(&player);
        self.state
            .write()
            .await
            .players
            .insert(player.playerid.clone(), player);
        Ok(zone)
    }

    /// Get artwork URL for a track
    pub async fn get_artwork_url(
        &self,
        coverid: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<String> {
        let base_url = self.rpc.base_url().await?;

        let suffix = match (width, height) {
            (Some(w), Some(h)) => format!("cover_{}x{}.jpg", w, h),
            (Some(w), None) => format!("cover_{}x{}.jpg", w, w),
            _ => "cover".to_string(),
        };

        Ok(format!("{}/music/{}/{}", base_url, coverid, suffix))
    }

    /// Fetch artwork image bytes
    /// If image_key is a URL, fetches directly. Otherwise treats as coverid.
    pub async fn get_artwork(
        &self,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
    ) -> Result<(String, Vec<u8>)> {
        self.get_artwork_with_limit(image_key, width, height, None)
            .await
    }

    pub async fn get_artwork_bounded(
        &self,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
        max_bytes: usize,
    ) -> Result<(String, Vec<u8>)> {
        self.get_artwork_with_limit(image_key, width, height, Some(max_bytes))
            .await
    }

    async fn get_artwork_with_limit(
        &self,
        image_key: &str,
        width: Option<u32>,
        height: Option<u32>,
        max_bytes: Option<usize>,
    ) -> Result<(String, Vec<u8>)> {
        let state = self.state.read().await;
        let username = state.username.clone();
        let password = state.password.clone();
        drop(state);

        // If image_key is a URL, fetch directly. #549: a favorite's `icon`
        // can also be server-relative (a local plugin's own icon path,
        // unlike the coverid/artwork_track_id ids the collections queries
        // otherwise hand back) -- same relative-to-absolute rule
        // docs/lyrion.md already documents for `artwork_url`.
        let url = if image_key.starts_with("http://") || image_key.starts_with("https://") {
            image_key.to_string()
        } else if let Some(path) = image_key.strip_prefix('/') {
            let base_url = self.rpc.base_url().await?;
            format!("{base_url}/{path}")
        } else {
            // Otherwise treat as coverid
            self.get_artwork_url(image_key, width, height).await?
        };

        let mut req = self.rpc.client.get(&url);

        // Add basic auth if configured
        if let (Some(ref user), Some(ref pass)) = (username, password) {
            use base64::Engine;
            let auth =
                base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", user, pass));
            req = req.header("Authorization", format!("Basic {}", auth));
        }

        let mut response = req.send().await?;
        if !response.status().is_success() {
            return Err(anyhow!("Failed to fetch artwork: {}", response.status()));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("image/jpeg")
            .to_string();
        if max_bytes.is_some_and(|limit| {
            response
                .content_length()
                .is_some_and(|length| length > limit as u64)
        }) {
            return Err(anyhow!("Artwork exceeds the source byte limit"));
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if max_bytes.is_some_and(|limit| body.len().saturating_add(chunk.len()) > limit) {
                return Err(anyhow!("Artwork exceeds the source byte limit"));
            }
            body.extend_from_slice(&chunk);
        }
        Ok((content_type, body))
    }

    /// Get cached player
    pub async fn get_cached_player(&self, player_id: &str) -> Option<LmsPlayer> {
        let player_id = strip_lms_prefix(player_id);
        self.state.read().await.players.get(player_id).cloned()
    }

    /// Get all cached players
    pub async fn get_cached_players(&self) -> Vec<LmsPlayer> {
        self.state.read().await.players.values().cloned().collect()
    }

    /// Change volume (f32 for fractional step support)
    pub async fn change_volume(&self, player_id: &str, value: f32, relative: bool) -> Result<()> {
        let player_id = strip_lms_prefix(player_id);
        let command = if relative { "vol_rel" } else { "vol_abs" };
        // LMS uses integer volume 0-100, round at the last moment
        self.control(player_id, command, Some(value.round() as i32))
            .await
    }

    /// Search using LMS globalsearch which includes all providers (library, TIDAL, Qobuz, etc.)
    ///
    /// Uses the LMS JSON-RPC `globalsearch items` command which searches across all
    /// registered search providers including streaming service plugins.
    ///
    /// Globalsearch returns a hierarchy:
    /// 1. Providers (My Music, TIDAL, Qobuz)
    /// 2. Categories (Everything, Playlists, Artists, Albums, Songs)
    /// 3. Actual playable items
    ///
    /// This method drills into streaming providers to find playable tracks.
    ///
    /// NOTE: globalsearch requires a player_id to determine which apps/providers are available.
    /// Fallback chain: passed player_id -> any connected player -> library-only search.
    pub async fn search(
        &self,
        query: &str,
        player_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<LmsSearchResult>> {
        let limit = limit.unwrap_or(20);

        // globalsearch requires a player_id to work - without it, LMS returns empty response
        // Fallback chain: passed player_id -> any connected player -> library-only
        let player_id = match player_id.map(strip_lms_prefix) {
            Some(id) => id.to_string(),
            None => {
                // Try to get any connected player (drop lock before any await)
                let any_player = {
                    let state = self.state.read().await;
                    state.players.keys().next().cloned()
                };
                match any_player {
                    Some(id) => id,
                    None => return self.search_library(query, limit).await,
                }
            }
        };

        // Get top-level providers
        let result = self.globalsearch_items(&player_id, query, None).await?;

        let items = match Self::get_items_from_result(&result) {
            Some(items) => items,
            None => return self.search_library(query, limit).await,
        };

        let mut results = Vec::new();

        // Look for providers and drill into them to find playable items
        for item in items {
            if results.len() >= limit {
                break;
            }

            let item_id = item.get("id").and_then(|v| v.as_str());
            let has_items = item.get("hasitems").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
            let is_audio = item.get("isaudio").and_then(|v| v.as_i64()).unwrap_or(0) == 1
                || item.get("type").and_then(|v| v.as_str()) == Some("audio");

            // If it's a playable item, add it directly
            if is_audio {
                if let Some(result) = self.parse_single_item(item) {
                    results.push(result);
                }
                continue;
            }

            // Drill into any provider that has sub-items (TIDAL, Qobuz, Spotify, Deezer, etc.)
            if let Some(item_id) = item_id {
                if has_items {
                    if let Ok(songs) = self
                        .drill_into_songs(&player_id, query, item_id, limit - results.len())
                        .await
                    {
                        results.extend(songs);
                    }
                }
            }
        }

        // Fallback to library search if no streaming results
        if results.is_empty() {
            return self.search_library(query, limit).await;
        }

        debug!(
            query = query,
            results = results.len(),
            "LMS globalsearch completed"
        );

        Ok(results)
    }

    /// Execute a globalsearch items query
    async fn globalsearch_items(
        &self,
        player_id: &str,
        query: &str,
        item_id: Option<&str>,
    ) -> Result<serde_json::Value> {
        let mut params = vec![
            json!("globalsearch"),
            json!("items"),
            json!(0),
            json!(50),
            json!(format!("search:{}", query)),
        ];

        if let Some(id) = item_id {
            params.push(json!(format!("item_id:{}", id)));
        }

        self.rpc.execute(Some(player_id), params).await
    }

    /// Get items array from globalsearch result
    fn get_items_from_result(result: &serde_json::Value) -> Option<&Vec<serde_json::Value>> {
        result
            .get("loop_loop")
            .or_else(|| result.get("items_loop"))
            .and_then(|v| v.as_array())
    }

    /// Drill into a provider to find the Songs category and return playable items
    async fn drill_into_songs(
        &self,
        player_id: &str,
        query: &str,
        provider_item_id: &str,
        limit: usize,
    ) -> Result<Vec<LmsSearchResult>> {
        // Get the provider's categories (Everything, Playlists, Artists, Albums, Songs)
        let result = self
            .globalsearch_items(player_id, query, Some(provider_item_id))
            .await?;

        let items = match Self::get_items_from_result(&result) {
            Some(items) => items,
            None => return Ok(Vec::new()),
        };

        let mut results = Vec::new();

        // Look for "Songs" or "Everything" category, or playable items directly
        for item in items {
            if results.len() >= limit {
                break;
            }

            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let item_id = item.get("id").and_then(|v| v.as_str());
            let has_items = item.get("hasitems").and_then(|v| v.as_i64()).unwrap_or(0) == 1;
            let is_audio = item.get("isaudio").and_then(|v| v.as_i64()).unwrap_or(0) == 1
                || item.get("type").and_then(|v| v.as_str()) == Some("audio");

            // If it's a playable item, add it directly
            if is_audio {
                if let Some(result) = self.parse_single_item(item) {
                    results.push(result);
                }
                continue;
            }

            // Drill into "Songs" or "Everything" to get actual tracks
            if has_items && item_id.is_some() && (name == "Songs" || name == "Everything") {
                let songs_result = self.globalsearch_items(player_id, query, item_id).await?;

                if let Some(songs) = Self::get_items_from_result(&songs_result) {
                    for song in songs {
                        if results.len() >= limit {
                            break;
                        }
                        if let Some(result) = self.parse_single_item(song) {
                            results.push(result);
                        }
                    }
                }
            }
        }

        Ok(results)
    }

    /// Parse a single globalsearch item into an LmsSearchResult
    ///
    /// Only returns Some if the item has at least one playback handle:
    /// - item_id (string, for globalsearch streaming)
    /// - url (for direct playback)
    /// - numeric id > 0 (for library items)
    fn parse_single_item(&self, item: &serde_json::Value) -> Option<LmsSearchResult> {
        let title = item
            .get("name")
            .or_else(|| item.get("title"))
            .and_then(|v| v.as_str())?;

        let is_audio = item.get("isaudio").and_then(|v| v.as_i64()).unwrap_or(0) == 1
            || item.get("type").and_then(|v| v.as_str()) == Some("audio");

        if !is_audio {
            return None;
        }

        // Get string item_id for streaming playback (globalsearch items have string IDs)
        let string_item_id = item.get("id").and_then(|v| v.as_str()).map(String::from);

        // Get numeric id for library items
        let numeric_id = item
            .get("id")
            .and_then(|v| v.as_i64())
            .or_else(|| item.get("track_id").and_then(|v| v.as_i64()))
            .unwrap_or(0);

        // Get URL for direct playback
        let url = item
            .get("play")
            .or_else(|| item.get("url"))
            .and_then(|v| v.as_str())
            .map(String::from);

        // Must have at least one playback handle
        if string_item_id.is_none() && url.is_none() && numeric_id <= 0 {
            return None;
        }

        Some(LmsSearchResult {
            result_type: LmsSearchResultType::Track,
            id: numeric_id,
            title: title.to_string(),
            artist: item
                .get("artist")
                .and_then(|v| v.as_str())
                .map(String::from),
            album: item.get("album").and_then(|v| v.as_str()).map(String::from),
            url,
            item_id: string_item_id,
        })
    }

    /// Fallback library-only search, used when globalsearch is unavailable or no
    /// player is connected.
    ///
    /// LMS keys each result loop's entity id `<type>_id`, never `id`:
    /// `albums_loop[].album_id`, `contributors_loop[].contributor_id`,
    /// `tracks_loop[].track_id`. Reading `id` made every guard in this function
    /// fail, so it returned an empty vec against every real server (#407).
    /// Loops with no matches are omitted from the response entirely rather than
    /// returned empty, so absent is normal, not an error.
    async fn search_library(&self, query: &str, limit: usize) -> Result<Vec<LmsSearchResult>> {
        let result = self
            .rpc
            .execute(
                None,
                vec![
                    json!("search"),
                    json!(0),
                    json!(limit),
                    json!(format!("term:{}", query)),
                ],
            )
            .await?;

        let mut results = Vec::new();

        // Parse albums — albums_loop[].album_id / .album
        if let Some(albums) = result.get("albums_loop").and_then(|v| v.as_array()) {
            for album in albums.iter().take(limit) {
                if let (Some(id), Some(title)) = (
                    loop_entity_id(album, "album"),
                    album.get("album").and_then(|v| v.as_str()),
                ) {
                    results.push(LmsSearchResult {
                        result_type: LmsSearchResultType::Album,
                        id,
                        title: title.to_string(),
                        artist: album
                            .get("artist")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        album: None,
                        url: None,
                        item_id: None,
                    });
                }
            }
        }

        // Parse artists — contributors_loop[].contributor_id / .contributor
        // (LMS's own type name here is "contributor", not "artist"; the published
        // CLI reference still documents this key as artist_id, which live 9.1.2
        // contradicts.)
        if let Some(artists) = result.get("contributors_loop").and_then(|v| v.as_array()) {
            for artist in artists.iter().take(limit.saturating_sub(results.len())) {
                if let (Some(id), Some(name)) = (
                    loop_entity_id(artist, "contributor"),
                    artist.get("contributor").and_then(|v| v.as_str()),
                ) {
                    results.push(LmsSearchResult {
                        result_type: LmsSearchResultType::Artist,
                        id,
                        title: name.to_string(),
                        artist: None,
                        album: None,
                        url: None,
                        item_id: None,
                    });
                }
            }
        }

        // Parse tracks — tracks_loop[].track_id / .track
        if let Some(tracks) = result.get("tracks_loop").and_then(|v| v.as_array()) {
            for track in tracks.iter().take(limit.saturating_sub(results.len())) {
                if let (Some(id), Some(title)) = (
                    loop_entity_id(track, "track"),
                    track.get("track").and_then(|v| v.as_str()),
                ) {
                    results.push(LmsSearchResult {
                        result_type: LmsSearchResultType::Track,
                        id,
                        title: title.to_string(),
                        artist: track
                            .get("artist")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        album: track
                            .get("album")
                            .and_then(|v| v.as_str())
                            .map(String::from),
                        url: None,
                        item_id: None,
                    });
                }
            }
        }

        debug!(
            query = query,
            results = results.len(),
            "LMS library search completed (fallback)"
        );

        Ok(results)
    }

    // =========================================================================
    // hifi_collections (#531): provider-neutral browse/playlists/favorites
    // =========================================================================

    /// Dispatch one `hifi_collections` operation to the matching taggedlist
    /// query. `params.path` is a plain server-side collection path (never a
    /// client-visible token -- `hifi_collections` mints/resolves the opaque
    /// ref around it); `None`/`"root"` means the collection root.
    ///
    /// The wire shape returned here is LMS-specific, not the provider-neutral
    /// `{title, subtitle, path}` shape `hifi_collections` sends to the client:
    /// a navigable row carries `path` (a collection path for another
    /// `collections_browse` call); a playable leaf carries `kind`+`id` (an
    /// [`LmsPlayTarget::Library`]) or `url` (an [`LmsPlayTarget::Url`]) for
    /// `crate::mcp::tools::collections` to mint a ref over -- mirroring the
    /// split `LmsPlayTarget` already makes for `hifi_search`.
    pub async fn collections_content(&self, operation: &str, params: &Value) -> Result<Value> {
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50) as usize;
        let offset = params.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
        match operation {
            "collections_browse" => {
                let path = params.get("path").and_then(Value::as_str).unwrap_or("root");
                self.browse_collections(path, offset, limit).await
            }
            "collections_playlists" => self.list_playlists(offset, limit).await,
            "collections_favorites" => self.list_favorites(offset, limit).await,
            _ => Err(anyhow!("unsupported LMS collection operation {operation}")),
        }
    }

    /// Walk one level of the LMS collection hierarchy: the synthetic root
    /// (Albums/Artists/Playlists), an album's or artist's contents, or a
    /// playlist's tracks. Every navigable path is a plain string this adapter
    /// invented (`"albums"`, `"album:<id>"`, ...) -- never a raw LMS id handed
    /// back to a client uninterpreted.
    async fn browse_collections(&self, path: &str, offset: usize, limit: usize) -> Result<Value> {
        if path == "root" {
            // The root menu is invented by this adapter, not read from LMS --
            // but a caller with no LMS host configured (or one that is
            // unreachable) must still get that failure here rather than a
            // synthetic success that never touched the server. `serverstatus`
            // with a zero-item request is the cheapest real round trip.
            self.rpc
                .execute(None, vec![json!("serverstatus"), json!(0), json!(0)])
                .await?;
            let categories = [
                ("Albums", "albums"),
                ("Artists", "artists"),
                ("Playlists", "playlists"),
            ];
            let items: Vec<Value> = categories
                .iter()
                .skip(offset)
                .take(limit)
                .map(|(title, path)| json!({"title": title, "path": path}))
                .collect();
            let next_offset =
                (categories.len() > offset + limit).then_some((offset + limit) as u64);
            return Ok(json!({"items": items, "next_offset": next_offset}));
        }
        if path == "albums" {
            return self.query_albums(offset, limit, None).await;
        }
        if path == "artists" {
            return self.query_artists(offset, limit).await;
        }
        if path == "playlists" {
            return self.list_playlists(offset, limit).await;
        }
        if let Some(id) = path
            .strip_prefix("album:")
            .and_then(|s| s.parse::<i64>().ok())
        {
            return self.query_tracks(offset, limit, "album_id", id).await;
        }
        if let Some(id) = path
            .strip_prefix("artist:")
            .and_then(|s| s.parse::<i64>().ok())
        {
            return self.query_albums(offset, limit, Some(id)).await;
        }
        if let Some(id) = path
            .strip_prefix("playlist:")
            .and_then(|s| s.parse::<i64>().ok())
        {
            return self.query_playlist_tracks(offset, limit, id).await;
        }
        Err(anyhow!("unknown LMS collection path {path:?}"))
    }

    /// `albums <start> <count> [artist_id:<id>]` -- the whole album library,
    /// or one artist's albums when `artist_id` is given.
    async fn query_albums(
        &self,
        offset: usize,
        limit: usize,
        artist_id: Option<i64>,
    ) -> Result<Value> {
        // A `tags:` parameter REPLACES the query's default tag set rather than
        // adding to it, so every field this method reads has to be named here.
        // #549 added `tags:cJ` for artwork alone, which dropped `album` from
        // every row -- the title this method requires -- so `filter_map` below
        // discarded the whole page and the Albums level (and every artist's
        // album list) went empty against a real server. `l` album title,
        // `a` artist, `c` coverid, `J` artwork_track_id; see docs/lyrion.md's
        // tag table and `tests/lms_collection_tags.rs`.
        let mut params = vec![
            json!("albums"),
            json!(offset),
            json!(limit),
            json!(ALBUM_DISPLAY_TAGS),
        ];
        if let Some(id) = artist_id {
            params.push(json!(format!("artist_id:{id}")));
        }
        let raw = self.rpc.execute(None, params).await?;
        let items: Vec<Value> = raw
            .get("albums_loop")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let id = loop_entity_id(row, "album")?;
                let title = row.get("album").and_then(Value::as_str)?.to_string();
                let artist = row.get("artist").and_then(Value::as_str).map(String::from);
                Some(json!({
                    "title": title,
                    "subtitle": artist.map(|a| format!("Album by {a}")),
                    "path": format!("album:{id}"),
                    "image_key": row_image_key(row),
                }))
            })
            .collect();
        let next_offset = next_offset(&raw, offset, limit, items.len());
        Ok(json!({"items": items, "next_offset": next_offset}))
    }

    /// `artists <start> <count>` -- the whole artist library.
    async fn query_artists(&self, offset: usize, limit: usize) -> Result<Value> {
        let raw = self
            .rpc
            .execute(None, vec![json!("artists"), json!(offset), json!(limit)])
            .await?;
        let items: Vec<Value> = raw
            .get("artists_loop")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let id = loop_entity_id(row, "artist")?;
                let title = row.get("artist").and_then(Value::as_str)?.to_string();
                Some(json!({"title": title, "path": format!("artist:{id}")}))
            })
            .collect();
        let next_offset = next_offset(&raw, offset, limit, items.len());
        Ok(json!({"items": items, "next_offset": next_offset}))
    }

    /// `titles <start> <count> <filter_key>:<id>` -- tracks under one album
    /// (or, in principle, another filterable field; only album is used today).
    async fn query_tracks(
        &self,
        offset: usize,
        limit: usize,
        filter_key: &str,
        filter_id: i64,
    ) -> Result<Value> {
        let raw = self
            .rpc
            .execute(
                None,
                vec![
                    json!("titles"),
                    json!(offset),
                    json!(limit),
                    json!(format!("{filter_key}:{filter_id}")),
                    // `a` for the artist subtitle alongside the artwork tags --
                    // see `ALBUM_DISPLAY_TAGS` on why the artwork tags alone
                    // silently drop it.
                    json!(TRACK_DISPLAY_TAGS),
                ],
            )
            .await?;
        let items: Vec<Value> = raw
            .get("titles_loop")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let id = loop_entity_id(row, "track")?;
                let title = row.get("title").and_then(Value::as_str)?.to_string();
                let artist = row.get("artist").and_then(Value::as_str).map(String::from);
                Some(json!({
                    "title": title,
                    "subtitle": artist,
                    "kind": "track",
                    "id": id,
                    "image_key": row_image_key(row),
                }))
            })
            .collect();
        let next_offset = next_offset(&raw, offset, limit, items.len());
        Ok(json!({"items": items, "next_offset": next_offset}))
    }

    /// `playlists <start> <count>` -- every saved playlist. Both
    /// `collections_playlists` and browsing into the synthetic `"playlists"`
    /// node use this, since LMS has exactly one playlist listing.
    async fn list_playlists(&self, offset: usize, limit: usize) -> Result<Value> {
        let raw = self
            .rpc
            .execute(None, vec![json!("playlists"), json!(offset), json!(limit)])
            .await?;
        let items: Vec<Value> = raw
            .get("playlists_loop")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let id = loop_entity_id(row, "playlist")?;
                let title = row.get("playlist").and_then(Value::as_str)?.to_string();
                Some(json!({"title": title, "path": format!("playlist:{id}")}))
            })
            .collect();
        let next_offset = next_offset(&raw, offset, limit, items.len());
        Ok(json!({"items": items, "next_offset": next_offset}))
    }

    /// `playlists tracks <start> <count> playlist_id:<id>` -- one playlist's
    /// contents.
    async fn query_playlist_tracks(
        &self,
        offset: usize,
        limit: usize,
        playlist_id: i64,
    ) -> Result<Value> {
        let raw = self
            .rpc
            .execute(
                None,
                vec![
                    json!("playlists"),
                    json!("tracks"),
                    json!(offset),
                    json!(limit),
                    json!(format!("playlist_id:{playlist_id}")),
                    // Same tag rule as `query_tracks`; see `ALBUM_DISPLAY_TAGS`.
                    json!(TRACK_DISPLAY_TAGS),
                ],
            )
            .await?;
        let items: Vec<Value> = raw
            .get("playlisttracks_loop")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let id = loop_entity_id(row, "track")?;
                let title = row
                    .get("title")
                    .or_else(|| row.get("track"))
                    .and_then(Value::as_str)?
                    .to_string();
                let artist = row.get("artist").and_then(Value::as_str).map(String::from);
                Some(json!({
                    "title": title,
                    "subtitle": artist,
                    "kind": "track",
                    "id": id,
                    "image_key": row_image_key(row),
                }))
            })
            .collect();
        let next_offset = next_offset(&raw, offset, limit, items.len());
        Ok(json!({"items": items, "next_offset": next_offset}))
    }

    /// `favorites items <start> <count>` -- the flat favourites list. LMS
    /// favorites carry no durable entity id, only a `url` (verified live,
    /// #403's gap-table note); a folder-shaped favorite (`hasitems`) is
    /// listed but not itself browsable in this slice -- narrower than a full
    /// walk, called out in #531's PR body rather than silently dropped.
    async fn list_favorites(&self, offset: usize, limit: usize) -> Result<Value> {
        let raw = self
            .rpc
            .execute(
                None,
                vec![
                    json!("favorites"),
                    json!("items"),
                    json!(offset),
                    json!(limit),
                ],
            )
            .await?;
        let items: Vec<Value> = raw
            .get("loop_loop")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|row| {
                let title = row
                    .get("name")
                    .or_else(|| row.get("title"))
                    .and_then(Value::as_str)?
                    .to_string();
                let url = row.get("url").and_then(Value::as_str)?.to_string();
                // #549: `favorites items` hands back `icon` (usually an
                // absolute URL) on its own, unlike albums/titles/playlists
                // tracks -- no `tags:` request needed for it.
                let image_key = row.get("icon").and_then(Value::as_str).map(str::to_string);
                Some(json!({"title": title, "url": url, "image_key": image_key}))
            })
            .collect();
        let next_offset = next_offset(&raw, offset, limit, items.len());
        Ok(json!({"items": items, "next_offset": next_offset}))
    }

    /// Search and play the first matching result
    ///
    /// Searches using globalsearch (includes streaming services), finds the first
    /// playable result, and executes the specified action (play, queue, or insert).
    /// Uses URL-based playback for streaming items, playlistcontrol for library items.
    ///
    /// #396: the actual command construction now lives in [`Self::play_target`],
    /// reached through [`LmsPlayTarget::from_search_hit`] so this method's
    /// observable behavior — including which of the three addressing paths a
    /// result with more than one handle takes — is unchanged from before the
    /// extraction. `hifi_play_ref`'s resolution path calls `play_target`
    /// directly, with a target this method never sees: a *specific* result's,
    /// not `results[0]`'s.
    pub async fn search_and_play(
        &self,
        query: &str,
        player_id: &str,
        action: LmsPlayAction,
    ) -> Result<String> {
        let player_id = strip_lms_prefix(player_id);

        // Search for content (uses globalsearch which includes all providers)
        let results = self.search(query, Some(player_id), Some(10)).await?;

        if results.is_empty() {
            return Err(anyhow!("No results found for '{}'", query));
        }

        // Take the first result
        let result = &results[0];
        let target = LmsPlayTarget::from_search_hit(result).ok_or_else(|| {
            anyhow!(
                "No playable result found for '{}' (missing item_id, url, and valid id)",
                query
            )
        })?;

        // No validation: `target` was just produced by the search a few
        // lines up, in this same call -- there is no time gap for the id to
        // have gone stale in, unlike a ref resolved later.
        self.play_target(&target, player_id, action, None).await?;

        // Build response message
        let action_verb = match action {
            LmsPlayAction::Play => "Playing",
            LmsPlayAction::Queue => "Queued",
            LmsPlayAction::Insert => "Playing next",
        };

        let what = match result.result_type {
            LmsSearchResultType::Album => {
                if let Some(ref artist) = result.artist {
                    format!("album \"{}\" by {}", result.title, artist)
                } else {
                    format!("album \"{}\"", result.title)
                }
            }
            LmsSearchResultType::Artist => format!("music by {}", result.title),
            LmsSearchResultType::Track => {
                if let Some(ref artist) = result.artist {
                    format!("\"{}\" by {}", result.title, artist)
                } else {
                    format!("\"{}\"", result.title)
                }
            }
        };

        info!(
            action = action_verb,
            what = what,
            player_id = player_id,
            url = result.url.as_deref().unwrap_or("library"),
            "LMS search_and_play"
        );

        Ok(format!("{} {}", action_verb, what))
    }

    /// Execute one resolved [`LmsPlayTarget`]: `play`, `queue`, or `insert`
    /// (play-next) where the target supports it.
    ///
    /// #396: this is the one path `hifi_play_ref` uses to resolve a ref. It
    /// never re-searches and never takes `results[0]` — the defect the issue
    /// exists to remove from ref resolution. `LmsRpc::execute`
    /// (`src/adapters/lms.rs`) stays private; this is the purpose-specific
    /// `pub` method that replaces handing MCP arbitrary CLI construction.
    ///
    /// # Validate before mutate
    ///
    /// A [`LmsPlayTarget::Library`] id is checked against the live database
    /// *before* the mutating command runs whenever `validate_title` is
    /// `Some` — because `playlistcontrol cmd:load` with an id that no longer
    /// exists clears the queue and *then* fails, and a wipe reported as a
    /// failure is still a wipe. Pass `Some(title)` (the title captured when
    /// the target was found — a ref's mint-time title, in `hifi_play_ref`'s
    /// case) for any caller resolving a target across a time gap; pass
    /// `None` when the target was *just* produced by a search this same call
    /// made (`search_and_play`'s own path), where there is no gap for the id
    /// to have gone stale in and the validation would be pure overhead on
    /// every ordinary play.
    ///
    /// **`Url` targets are not pre-validated**, `validate_title` or not. LMS
    /// exposes no cheap existence check for an arbitrary streaming or plugin
    /// URL, and `playlist load <bad-url>` returns a normal `{}` while
    /// emptying the queue regardless of whether it is checked first. This is
    /// a known, stated gap, not a silently-accepted one — see the issue for
    /// why ref minting still prefers `Url` over
    /// [`LmsPlayTarget::GlobalSearchItem`] in spite of it: an unresolvable
    /// URL is at least a real failure mode, where a positional breadcrumb is
    /// a *silent* one.
    pub async fn play_target(
        &self,
        target: &LmsPlayTarget,
        player_id: &str,
        action: LmsPlayAction,
        validate_title: Option<&str>,
    ) -> Result<()> {
        let player_id = strip_lms_prefix(player_id);

        match target {
            LmsPlayTarget::GlobalSearchItem { item_id } => {
                // Globalsearch streaming item - use globalsearch playlist command
                let method = match action {
                    LmsPlayAction::Play => "play",
                    LmsPlayAction::Queue => "add",
                    LmsPlayAction::Insert => "insert",
                };
                let item_id_param = format!("item_id:{}", item_id);
                self.rpc
                    .execute(
                        Some(player_id),
                        vec![
                            json!("globalsearch"),
                            json!("playlist"),
                            json!(method),
                            json!(item_id_param),
                        ],
                    )
                    .await?;
            }
            LmsPlayTarget::Url { url } => {
                // Direct URL item - use playlist command with URL
                let method = match action {
                    LmsPlayAction::Play => "load",
                    LmsPlayAction::Queue => "add",
                    LmsPlayAction::Insert => "insert",
                };
                self.rpc
                    .execute(
                        Some(player_id),
                        vec![json!("playlist"), json!(method), json!(url)],
                    )
                    .await?;
            }
            LmsPlayTarget::Library { kind, id } => {
                if let Some(title) = validate_title {
                    self.assert_library_id_exists(*kind, *id, title).await?;
                }

                let id_param = match kind {
                    LmsSearchResultType::Album => format!("album_id:{}", id),
                    LmsSearchResultType::Artist => format!("artist_id:{}", id),
                    LmsSearchResultType::Track => format!("track_id:{}", id),
                };
                let cmd_param = format!("cmd:{}", action.to_lms_cmd());

                self.rpc
                    .execute(
                        Some(player_id),
                        vec![json!("playlistcontrol"), json!(cmd_param), json!(id_param)],
                    )
                    .await?;
            }
        }

        Ok(())
    }

    /// Confirm a [`LmsPlayTarget::Library`] id still exists before a mutating
    /// `playlistcontrol` call touches the queue.
    ///
    /// # Why this reuses `search_library` rather than a dedicated query
    ///
    /// An earlier version of this method queried the dedicated taggedlist
    /// command for `kind` (`albums`/`artists`/`titles`), filtered by
    /// `<kind>_id`, and treated `count >= 1` as "exists". That shape is
    /// unverified against a live server, and unverified is not merely
    /// "untested" here: if LMS ignores an unrecognised filter tag rather than
    /// erroring on it — this repo has already observed LMS silently ignoring
    /// unrecognised tagged parameters elsewhere, see the `mute` pref comment
    /// in `get_players` — the query would return the *unfiltered* count of
    /// every album/artist/track on the server, which is essentially always
    /// `>= 1`. That is a **fail-open** bug: validation would always report
    /// "exists" regardless of the actual id, silently defeating the entire
    /// point of validating before a mutating call. A wrong filter name would
    /// have made this method worse than not having it.
    ///
    /// This version instead reuses [`Self::search_library`] — parsing already
    /// verified against a recorded live Lyrion 9.1.2 response (#407,
    /// `tests/fixtures/lms/`) — searching by the title captured when the
    /// target was found, and checking whether any returned entity of the
    /// right kind has the target id. A wrong assumption here fails **closed**
    /// instead: if the title no longer matches anything (or the entity is
    /// gone), no result carries the target id, and this refuses exactly as it
    /// should. The trade a wrong assumption could still make is a false
    /// negative — refusing a still-valid id because the title changed since
    /// mint time — which is the safe direction to be wrong in.
    ///
    /// # A second false-negative source, found and closed during review
    ///
    /// Searching by title only surfaces a *bounded* number of matches
    /// ([`LIBRARY_VALIDATION_SEARCH_LIMIT`]). If more entries than that share
    /// the exact title captured at mint time (a box set re-release, a
    /// self-titled album by different artists, a generic "Live"), a
    /// still-valid id outside that window is wrongly refused —
    /// `tests/mcp_refs.rs::lms_a_valid_ref_beyond_the_validation_search_limit_still_resolves`
    /// reproduces this directly (it failed against the original limit of 50
    /// before this constant was raised). The limit is generous enough that a
    /// real collision is implausible, but "implausible" is not "impossible";
    /// this is a known, bounded, fail-*closed* residual risk, not a claim
    /// that no library can ever trigger it.
    ///
    /// # This is not free, and that cost is unmeasured
    ///
    /// This method runs unconditionally before every `hifi_play_ref`
    /// resolution of a `Library` target (`play_target` passes
    /// `validate_title: Some` for that path) — a full-text `search` over up
    /// to [`LIBRARY_VALIDATION_SEARCH_LIMIT`] rows, on every such call, not
    /// only when a ref is actually stale. That is a real latency cost on top
    /// of the eventual `playlistcontrol` call, and this project ships to
    /// NAS- and SBC-class hardware (see the Synology/QNAP build targets in
    /// CI) where a large-library full-text search is not necessarily cheap.
    /// No benchmark exists for this yet; if `hifi_play_ref` on LMS turns out
    /// noticeably slower than `hifi_play`'s own query path, this method is
    /// the place to look first.
    async fn assert_library_id_exists(
        &self,
        kind: LmsSearchResultType,
        id: i64,
        title: &str,
    ) -> Result<()> {
        let candidates = self
            .search_library(title, LIBRARY_VALIDATION_SEARCH_LIMIT)
            .await?;
        let exists = candidates
            .iter()
            .any(|c| c.result_type == kind && c.id == id);
        if !exists {
            anyhow::bail!(
                "{} id {} (title {:?}) no longer exists in the LMS library (rescanned \
                 since this ref was minted, or renamed?); refusing rather than issuing \
                 playlistcontrol, which would clear the queue and then fail",
                kind,
                id,
                title
            );
        }
        Ok(())
    }
}

/// Convert an LMS player to a unified Zone representation
fn lms_player_to_zone(player: &LmsPlayer) -> Zone {
    let zone_id = PrefixedZoneId::lms(&player.playerid).to_string();
    Zone {
        zone_id: zone_id.clone(),
        zone_name: player.name.clone(),
        state: PlaybackState::from(player.state.as_str()),
        volume_control: Some(VolumeControl {
            value: player.volume as f32,
            min: 0.0,
            max: 100.0,
            // LMS hardcodes $increment = 2.5 in Slim/Player/Client.pm:755
            // This is not queryable via CLI/JSON-RPC, so we use the constant.
            step: 2.5,
            // `status` carries no mute key, but the per-player `mute` pref does -
            // requested via `playerprefs:mute` on the `players` query the poll
            // already makes, so this costs no extra round-trip. See LmsPlayer.muted.
            is_muted: player.muted,
            scale: crate::bus::VolumeScale::Percentage,
            // Use prefixed zone_id as output_id for consistent aggregator matching
            output_id: Some(zone_id),
        }),
        now_playing: if !player.title.is_empty() {
            Some(crate::bus::NowPlaying {
                title: player.title.clone(),
                artist: player.artist.clone(),
                album: player.album.clone(),
                image_key: player.artwork_url.clone().or(player.coverid.clone()),
                seek_position: Some(player.time),
                duration: Some(player.duration),
                metadata: None,
                repeat_mode: None,
                shuffle: None,
            })
        } else {
            None
        },
        source: "lms".to_string(),
        is_controllable: player.power && player.connected,
        is_seekable: true,
        last_updated: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64,
        // LMS always allows playback controls when powered and connected
        is_play_allowed: player.state != "playing",
        is_pause_allowed: player.state == "playing",
        is_next_allowed: true,
        is_previous_allowed: true,
    }
}

/// Shared helper function for updating players from the polling task
/// Uses LmsRpc to avoid code duplication between LmsAdapter and background task
async fn update_players_internal(
    rpc: &LmsRpc,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
    runtime_bridge: Option<&LmsRuntimeBridge>,
) -> Result<()> {
    let players = rpc.get_players().await?;
    // LMS keeps disconnected Squeezelite clients in its `players` inventory.
    // That inventory is useful for server diagnostics, but only a connected
    // client is a controllable UHC zone. Treat the connected subset as this
    // poll's authoritative projection membership so a disconnect retires the
    // old `lms:` zone through the reliable removal path below.
    let observed_ids: std::collections::HashSet<String> = players
        .iter()
        .filter(|player| player.connected)
        .map(|player| player.playerid.clone())
        .collect();

    let previous_ids: std::collections::HashSet<String> =
        { state.read().await.players.keys().cloned().collect() };

    // Collect legacy deltas to emit after releasing the lock. In a reliable-runtime composition
    // they are replaced with full zone snapshots below; the aggregator must not reconstruct LMS
    // state from an independently lossy CLI/poll stream.
    let mut now_playing_updates: Vec<NowPlayingUpdate> = Vec::new();
    // State updates: (player_id, player_name, state)
    let mut state_updates: Vec<(String, String, String)> = Vec::new();
    // VolumeChanged: (player_id, volume, is_muted)
    let mut volume_updates: Vec<(String, i32, bool)> = Vec::new();

    // Helper to convert empty strings to None (metadata cleared)
    let to_option = |s: &str| {
        if s.is_empty() {
            None
        } else {
            Some(s.to_string())
        }
    };

    for mut player in players {
        match rpc.get_player_status(&player.playerid).await {
            Ok(status) => {
                player.state = status.state;
                player.mode = status.mode;
                player.power = status.power;
                player.volume = status.volume;
                // `player.muted` came from the `mute` pref on the `players` call;
                // `status.muted` is the negated-volume signal. OR them: the pref
                // flips immediately but needs `playerprefs:` support, the sign
                // needs nothing but lags a mute by ~0.8s. Either alone can miss;
                // neither produces a false positive.
                player.muted = player.muted || status.muted;
                player.playlist_tracks = status.playlist_tracks;
                player.playlist_cur_index = status.playlist_cur_index;
                player.time = status.time;
                player.duration = status.duration;
                player.title = status.title;
                player.artist = status.artist;
                player.album = status.album;
                player.artwork_track_id = status.artwork_track_id;
                player.coverid = status.coverid;
                player.artwork_url = status.artwork_url;
            }
            Err(e) => {
                tracing::warn!("Failed to get status for player {}: {}", player.playerid, e);
                continue;
            }
        }

        // Check what changed for this player
        let (now_playing_changed, state_changed, volume_changed) = {
            let s = state.read().await;
            if let Some(old_player) = s.players.get(&player.playerid) {
                let np_changed = old_player.title != player.title
                    || old_player.artist != player.artist
                    || old_player.album != player.album
                    || old_player.artwork_url != player.artwork_url
                    || old_player.coverid != player.coverid;
                let state_changed = old_player.state != player.state;
                // Mute counts as a volume change. Zones are served from the
                // aggregator's event-fed cache, so a mute toggled from another
                // controller (iPeng, Squeezer, the LMS web UI) would otherwise
                // never reach /zones unless the volume happened to change too.
                let volume_changed =
                    old_player.volume != player.volume || old_player.muted != player.muted;
                (np_changed, state_changed, volume_changed)
            } else {
                // New player - will be handled by ZoneDiscovered
                (false, false, false)
            }
        };

        if runtime_bridge.is_none() && now_playing_changed {
            // Emit even when metadata clears (all fields empty) so UI can update
            now_playing_updates.push(NowPlayingUpdate {
                player_id: player.playerid.clone(),
                title: to_option(&player.title),
                artist: to_option(&player.artist),
                album: to_option(&player.album),
                image_key: player.artwork_url.clone().or(player.coverid.clone()),
            });
        }

        if runtime_bridge.is_none() && state_changed {
            state_updates.push((
                player.playerid.clone(),
                player.name.clone(),
                player.state.clone(),
            ));
        }

        if runtime_bridge.is_none() && volume_changed {
            volume_updates.push((player.playerid.clone(), player.volume, player.muted));
        }

        let mut s = state.write().await;
        s.players.insert(player.playerid.clone(), player);
    }

    // LMS's connected subset is the authoritative controllable-zone membership. Keep the last
    // observation when a connected player's follow-up status call fails, but retire both missing
    // and disconnected ids. Without this retain, `current_ids` was always a superset of
    // `previous_ids`, making the ZoneRemoved path below unreachable and leaving disconnected
    // players in the aggregator.
    state
        .write()
        .await
        .players
        .retain(|player_id, _| observed_ids.contains(player_id));

    if let Some(bridge) = runtime_bridge {
        // Every HTTP read is a complete player observation. Publishing unchanged snapshots is the
        // intentional reconciliation behaviour after a CLI disconnect or sequence gap.
        let snapshots: Vec<Zone> = state
            .read()
            .await
            .players
            .values()
            .map(lms_player_to_zone)
            .collect();
        for zone in snapshots {
            bridge.publish_zone(zone, None).await?;
        }
    }

    // Emit NowPlayingChanged events for updated players (including metadata clearing) only for
    // legacy compositions without reliable projection ingress.
    for update in now_playing_updates {
        debug!(
            "Polling detected now_playing change for {}: {:?}",
            update.player_id,
            update.title.as_deref().unwrap_or("<cleared>")
        );
        bus.publish(BusEvent::NowPlayingChanged {
            zone_id: PrefixedZoneId::lms(&update.player_id),
            title: update.title,
            artist: update.artist,
            album: update.album,
            image_key: update.image_key,
        });
    }

    // Emit state change events (play/pause/stop)
    for (player_id, player_name, state) in state_updates {
        debug!("Polling detected state change for {}: {}", player_id, state);
        // Publish ZoneUpdated so aggregator updates state (SSE uses zone_id prefix to refresh LMS page)
        bus.publish(BusEvent::ZoneUpdated {
            zone_id: PrefixedZoneId::lms(&player_id),
            display_name: player_name,
            state,
        });
    }

    // Emit VolumeChanged events for volume or mute changes
    for (player_id, volume, is_muted) in volume_updates {
        debug!(
            "Polling detected volume change for {}: {} (muted: {})",
            player_id, volume, is_muted
        );
        bus.publish(BusEvent::VolumeChanged {
            output_id: PrefixedZoneId::lms(&player_id).to_string(),
            value: volume as f32,
            // Previously hard-coded false, which meant every polled volume change
            // clobbered the correct mute state the CLI subscription had already
            // published for the same player.
            is_muted,
        });
    }

    // Emit events for player set changes
    let current_ids: std::collections::HashSet<String> =
        { state.read().await.players.keys().cloned().collect() };

    if previous_ids != current_ids {
        let added: Vec<_> = current_ids.difference(&previous_ids).cloned().collect();
        let removed: Vec<_> = previous_ids.difference(&current_ids).cloned().collect();

        if runtime_bridge.is_none() {
            // Emit zone discovered events for new players
            for player_id in &added {
                if let Some(player) = state.read().await.players.get(player_id) {
                    tracing::debug!("LMS player discovered: {}", player_id);
                    let zone = lms_player_to_zone(player);
                    bus.publish(BusEvent::ZoneDiscovered { zone });
                }
            }
        }

        // Removal is also a canonical projection under the reliable runtime. The aggregator emits
        // its existing compatibility BusEvent only after the removal has committed.
        for player_id in &removed {
            tracing::debug!("LMS player removed: {}", player_id);
            let zone_id = PrefixedZoneId::lms(player_id);
            if let Some(bridge) = runtime_bridge {
                bridge.publish_removed(zone_id).await?;
            } else {
                bus.publish(BusEvent::ZoneRemoved { zone_id });
            }
        }
    }

    Ok(())
}

// =============================================================================
// CLI Subscription (Event-Driven Updates)
// =============================================================================

/// Maximum consecutive poll failures before triggering restart
const MAX_CONSECUTIVE_POLL_FAILURES: u32 = 3;

/// Run the polling loop (extracted helper for AdapterLogic)
/// Returns Err after consecutive failures to trigger adapter restart
async fn run_polling_loop(
    state: Arc<RwLock<LmsState>>,
    bus: SharedBus,
    rpc: LmsRpc,
    shutdown: CancellationToken,
    runtime_bridge: Option<Arc<LmsRuntimeBridge>>,
) -> Result<()> {
    // Start with fast polling; will switch to slow when subscription is active
    let mut current_interval = get_poll_interval();
    let mut poll_timer = interval(current_interval);
    let mut consecutive_failures: u32 = 0;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("LMS polling shutting down");
                break;
            }
            _ = poll_timer.tick() => {
                // Check if we need to adjust polling interval
                let subscription_active = state.read().await.cli_subscription_active;
                let target_interval = if subscription_active {
                    get_poll_interval_with_subscription()
                } else {
                    get_poll_interval()
                };

                if target_interval != current_interval {
                    debug!(
                        "Adjusting poll interval: {:?} -> {:?} (subscription_active={})",
                        current_interval, target_interval, subscription_active
                    );
                    current_interval = target_interval;
                    poll_timer = interval(current_interval);
                }

                match update_players_internal(&rpc, &state, &bus, runtime_bridge.as_deref()).await {
                    Ok(()) => {
                        // Reset failure counter on success
                        if consecutive_failures > 0 {
                            debug!("LMS poll succeeded, resetting failure counter");
                            consecutive_failures = 0;
                        }
                    }
                    Err(e) => {
                        consecutive_failures += 1;
                        if consecutive_failures >= MAX_CONSECUTIVE_POLL_FAILURES {
                            tracing::error!(
                                "LMS poll failed {} consecutive times, triggering restart: {}",
                                consecutive_failures, e
                            );
                            return Err(anyhow!("LMS unreachable after {} consecutive poll failures", consecutive_failures));
                        } else {
                            warn!(
                                "LMS poll failed ({}/{}): {}",
                                consecutive_failures, MAX_CONSECUTIVE_POLL_FAILURES, e
                            );
                        }
                    }
                }
            }
        }
    }

    info!("LMS polling stopped");
    Ok(())
}

/// Run CLI subscription once (calls connect_and_subscribe directly)
async fn run_cli_subscription_once(
    host: &str,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
    rpc: &LmsRpc,
    shutdown: &CancellationToken,
    runtime_bridge: Option<&LmsRuntimeBridge>,
) -> Result<()> {
    info!("[CLI] Connecting to LMS CLI at {}:{}", host, CLI_PORT);
    connect_and_subscribe(host, state, bus, rpc, shutdown, runtime_bridge).await
}

/// Connect to LMS CLI and process events
async fn connect_and_subscribe(
    host: &str,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
    rpc: &LmsRpc,
    shutdown: &CancellationToken,
    runtime_bridge: Option<&LmsRuntimeBridge>,
) -> Result<()> {
    let addr = format!("{}:{}", host, CLI_PORT);
    let stream = TcpStream::connect(&addr).await?;

    info!("[CLI] Connected to LMS CLI at {}", addr);

    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);

    // Send subscription command
    // Subscribe to: playlist, mixer, power, client events
    let subscribe_cmd = "subscribe playlist,mixer,power,client\n";
    writer.write_all(subscribe_cmd.as_bytes()).await?;
    writer.flush().await?;

    info!("[CLI] Subscribed to LMS CLI events");

    // Mark subscription as active
    {
        let mut s = state.write().await;
        s.cli_subscription_active = true;
    }

    // Process events
    let mut line = String::new();

    loop {
        line.clear();

        tokio::select! {
            _ = shutdown.cancelled() => {
                info!("[CLI] Received shutdown signal");
                return Ok(());
            }
            result = tokio::time::timeout(CLI_READ_TIMEOUT, reader.read_line(&mut line)) => {
                match result {
                    Ok(Ok(0)) => {
                        // EOF - connection closed
                        return Err(anyhow!("LMS CLI connection closed"));
                    }
                    Ok(Ok(_)) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            debug!("CLI event: {}", trimmed);
                            handle_cli_event(trimmed, state, bus, rpc, runtime_bridge).await;
                        }
                    }
                    Ok(Err(e)) => {
                        return Err(anyhow!("CLI read error: {}", e));
                    }
                    Err(_) => {
                        // Timeout - LMS may be unresponsive
                        return Err(anyhow!("CLI read timeout after {:?}", CLI_READ_TIMEOUT));
                    }
                }
            }
        }
    }
}

/// Project the cached complete LMS player through the shared observer bridge.
///
/// CLI messages are deltas, but the cache is hydrated by polling and the specific CLI readback
/// before this helper is called.  Sending a reconstructed `Zone` from here means the aggregator
/// never has to merge a playlist event from one source with a volume event from the other.
async fn publish_cached_lms_zone(
    state: &Arc<RwLock<LmsState>>,
    bridge: &LmsRuntimeBridge,
    player_id: &str,
) -> Result<bool> {
    let zone = state
        .read()
        .await
        .players
        .get(player_id)
        .map(lms_player_to_zone);
    match zone {
        Some(zone) => {
            bridge.publish_zone(zone, None).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}

/// Handle a parsed CLI event
async fn handle_cli_event(
    line: &str,
    state: &Arc<RwLock<LmsState>>,
    bus: &SharedBus,
    rpc: &LmsRpc,
    runtime_bridge: Option<&LmsRuntimeBridge>,
) {
    let event = parse_cli_event(line);

    match event {
        CliEvent::Playlist {
            player_id, command, ..
        } => {
            debug!("Playlist event for {}: {}", player_id, command);

            // Refresh player status on playlist changes
            match rpc.get_player_status(&player_id).await {
                Ok(status) => {
                    let zone_id = PrefixedZoneId::lms(&player_id);

                    // Update cached state and get player name for ZoneUpdated
                    let player_name = {
                        let mut s = state.write().await;
                        if let Some(player) = s.players.get_mut(&player_id) {
                            player.state = status.state.clone();
                            player.mode = status.mode.clone();
                            player.volume = status.volume;
                            player.time = status.time;
                            player.duration = status.duration;
                            player.title = status.title.clone();
                            player.artist = status.artist.clone();
                            player.album = status.album.clone();
                            player.artwork_url = status.artwork_url.clone();
                            player.coverid = status.coverid.clone();
                            player.name.clone()
                        } else {
                            player_id.clone() // Fallback to player_id if not in cache
                        }
                    };

                    if let Some(bridge) = runtime_bridge {
                        if let Err(error) = publish_cached_lms_zone(state, bridge, &player_id).await
                        {
                            warn!(%error, player = %player_id, "could not project LMS CLI playlist readback");
                        }
                    } else {
                        // Legacy event-bus embedding: retain the exact prior delta contract.
                        bus.publish(BusEvent::ZoneUpdated {
                            zone_id: zone_id.clone(),
                            display_name: player_name,
                            state: status.state.clone(),
                        });

                        if !status.title.is_empty() {
                            bus.publish(BusEvent::NowPlayingChanged {
                                zone_id,
                                title: Some(status.title),
                                artist: Some(status.artist),
                                album: Some(status.album),
                                image_key: status.artwork_url.or(status.coverid),
                            });
                        }
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to refresh player status after playlist event: {}",
                        e
                    );
                }
            }
        }
        CliEvent::Mixer {
            player_id,
            param,
            value,
            is_relative,
        } => {
            // Only process mixer events when value was successfully parsed
            let Some(value) = value else {
                debug!(
                    "Ignoring mixer event with unparseable value for {}: {}",
                    player_id, param
                );
                return;
            };

            if param == "volume" {
                // Calculate absolute volume:
                // - If relative, apply delta to cached volume (f32 for fractional steps like 2.5)
                // - If absolute, use value directly
                let absolute_volume = if is_relative {
                    let current = {
                        let s = state.read().await;
                        s.players
                            .get(&player_id)
                            .map(|p| p.volume as f32)
                            .unwrap_or(50.0)
                    };
                    // Apply relative change and clamp to 0-100 range
                    (current + value).clamp(0.0, 100.0)
                } else {
                    value.clamp(0.0, 100.0)
                };

                debug!(
                    "Volume change for {}: {} (is_relative={}, result={})",
                    player_id, value, is_relative, absolute_volume
                );

                // Update cached state (rounded to i32 for LMS internal format).
                //
                // Setting the volume clears mute server-side: mixerCommand does
                // `$prefs->client($client)->set('mute', 0)` when the entity is
                // 'volume' and the player is muted (Slim/Control/Commands.pm).
                // So `false` here is LMS's behaviour, not an assumption - but it
                // is now derived from the cache rather than hard-coded, so the
                // model stays consistent with what the next poll will read.
                let is_muted = {
                    let mut s = state.write().await;
                    if let Some(player) = s.players.get_mut(&player_id) {
                        player.volume = absolute_volume.round() as i32;
                        player.muted = false;
                    }
                    false
                };

                if let Some(bridge) = runtime_bridge {
                    if let Err(error) = publish_cached_lms_zone(state, bridge, &player_id).await {
                        warn!(%error, player = %player_id, "could not project LMS CLI volume readback");
                    }
                } else {
                    bus.publish(BusEvent::VolumeChanged {
                        output_id: PrefixedZoneId::lms(&player_id).to_string(),
                        value: absolute_volume,
                        is_muted,
                    });
                }
            } else if param == "muting" {
                let is_muted = value != 0.0;
                debug!("Mute change for {}: {}", player_id, is_muted);

                // Get current volume from cache for the event, and record the mute
                // so a ZoneDiscovered built before the next poll is already right.
                let current_volume = {
                    let mut s = state.write().await;
                    match s.players.get_mut(&player_id) {
                        Some(player) => {
                            player.muted = is_muted;
                            player.volume
                        }
                        None => 0,
                    }
                };

                if let Some(bridge) = runtime_bridge {
                    if let Err(error) = publish_cached_lms_zone(state, bridge, &player_id).await {
                        warn!(%error, player = %player_id, "could not project LMS CLI mute readback");
                    }
                } else {
                    bus.publish(BusEvent::VolumeChanged {
                        output_id: PrefixedZoneId::lms(&player_id).to_string(),
                        value: current_volume as f32,
                        is_muted,
                    });
                }
            }
        }
        CliEvent::Power {
            player_id,
            state: power_state,
        } => {
            debug!("Power change for {}: {}", player_id, power_state);

            // Update cached state and get player name for ZoneUpdated
            let player_name = {
                let mut s = state.write().await;
                if let Some(player) = s.players.get_mut(&player_id) {
                    player.power = power_state;
                    player.name.clone()
                } else {
                    player_id.clone()
                }
            };

            if let Some(bridge) = runtime_bridge {
                if let Err(error) = publish_cached_lms_zone(state, bridge, &player_id).await {
                    warn!(%error, player = %player_id, "could not project LMS CLI power readback");
                }
            // Legacy event-bus projection only learned a state change when power went off.
            // Preserve that compatibility behaviour for non-runtime embeddings.
            } else if !power_state {
                let zone_id = PrefixedZoneId::lms(&player_id);
                bus.publish(BusEvent::ZoneUpdated {
                    zone_id,
                    display_name: player_name,
                    state: "stopped".to_string(),
                });
            }
        }
        CliEvent::Client { player_id, action } => {
            debug!("Client event for {}: {}", player_id, action);

            match action.as_str() {
                "new" | "reconnect" => {
                    // Get status and player name before locking state
                    let Ok(status) = rpc.get_player_status(&player_id).await else {
                        return;
                    };

                    // Try to get player name from existing state first (for reconnect)
                    let existing_name = {
                        let s = state.read().await;
                        s.players
                            .get(&player_id)
                            .map(|p| p.name.clone())
                            .filter(|n| !n.is_empty())
                    };

                    // If no existing name, fetch from player list; fall back to player_id
                    let player_name = match existing_name {
                        Some(name) => name,
                        None => match rpc.get_players().await {
                            Ok(players) => players
                                .iter()
                                .find(|p| p.playerid == player_id)
                                .map(|p| p.name.clone())
                                .filter(|n| !n.is_empty())
                                .unwrap_or_else(|| player_id.clone()),
                            Err(_) => player_id.clone(),
                        },
                    };

                    // Now lock and update state
                    let mut s = state.write().await;
                    let is_new = !s.players.contains_key(&player_id);

                    // Create or update player
                    let player = LmsPlayer {
                        playerid: player_id.clone(),
                        name: player_name,
                        connected: true,
                        power: status.power,
                        state: status.state,
                        mode: status.mode,
                        volume: status.volume,
                        title: status.title,
                        artist: status.artist,
                        album: status.album,
                        ..Default::default()
                    };

                    s.players.insert(player_id.clone(), player.clone());
                    drop(s);

                    if let Some(bridge) = runtime_bridge {
                        if let Err(error) =
                            bridge.publish_zone(lms_player_to_zone(&player), None).await
                        {
                            warn!(%error, player = %player_id, "could not project LMS CLI client readback");
                        }
                    } else if is_new {
                        let zone = lms_player_to_zone(&player);
                        bus.publish(BusEvent::ZoneDiscovered { zone });
                    }
                }
                "disconnect" => {
                    // A CLI disconnect is authoritative membership evidence, not merely a state
                    // delta. Retire immediately rather than republishing a cached stopped zone
                    // and waiting for the deliberately slowed-down poller to catch up.
                    state.write().await.players.remove(&player_id);
                    let zone_id = PrefixedZoneId::lms(&player_id);
                    if let Some(bridge) = runtime_bridge {
                        if let Err(error) = bridge.publish_removed(zone_id).await {
                            warn!(%error, player = %player_id, "could not retire LMS CLI disconnect projection");
                        }
                    } else {
                        bus.publish(BusEvent::ZoneRemoved { zone_id });
                    }
                }
                _ => {}
            }
        }
        CliEvent::Unknown { raw_line } => {
            // Log unknown events at trace level for debugging
            tracing::trace!("Unknown CLI event: {}", raw_line);
        }
    }
}

// =============================================================================
// AdapterLogic Implementation
// =============================================================================

#[async_trait]
impl AdapterLogic for LmsAdapter {
    fn prefix(&self) -> &'static str {
        "lms"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        // Check if configured, if not try auto_discover_and_configure()
        if !self.is_configured().await {
            match self.auto_discover_and_configure().await {
                Ok(true) => {
                    tracing::info!("LMS auto-configured via discovery");
                }
                Ok(false) => {
                    return Err(anyhow!(
                        "LMS not configured and auto-discovery did not find exactly one server. \
                         Configure manually via POST /lms/configure or use GET /lms/discover to see available servers."
                    ));
                }
                Err(e) => {
                    tracing::warn!("LMS auto-discovery failed: {}", e);
                    return Err(anyhow!(
                        "LMS not configured and auto-discovery failed: {}. \
                         Configure manually via POST /lms/configure.",
                        e
                    ));
                }
            }
        }

        // Initial update
        self.update_players().await?;

        // Set state.connected = true and state.running = true
        {
            let mut state = self.state.write().await;
            state.connected = true;
            state.running = true;
        }

        let host = {
            let state = self.state.read().await;
            state.host.clone().unwrap_or_default()
        };

        info!("LMS client connected to {}", host);
        ctx.bus
            .publish(BusEvent::LmsConnected { host: host.clone() });

        // One provider endpoint owns all LMS player commands.  Unlike HQPlayer, LMS's server
        // protocol serializes its own players adequately, so provider scope is intentional; the
        // correlated readback still targets the exact player and commits one complete Zone.
        let command_shutdown = ctx.shutdown.child_token();
        let command_join = self.runtime_bridge.as_ref().and_then(|bridge| {
            match bridge.commands().register_provider("lms", 32) {
                Ok(endpoint) => {
                    let adapter = self.clone();
                    let bridge = bridge.clone();
                    let shutdown = command_shutdown.clone();
                    Some(tokio::spawn(async move {
                        run_lms_command_endpoint(adapter, endpoint, shutdown, bridge).await;
                    }))
                }
                Err(error) => {
                    tracing::error!(?error, "LMS reliable endpoint registration failed");
                    None
                }
            }
        });

        // Run polling loop directly
        // CLI subscription is now handled by separate LmsCliAdapter (Issue #165)
        // Polling reads cli_subscription_active flag to adjust interval:
        // - CLI active (flag=true): slow interval (30s)
        // - CLI inactive (flag=false): fast interval (2s)
        let result = run_polling_loop(
            self.state.clone(),
            ctx.bus.clone(),
            self.rpc.clone(),
            ctx.shutdown.clone(),
            self.runtime_bridge.clone(),
        )
        .await;
        command_shutdown.cancel();
        if let Some(join) = command_join {
            if let Err(error) = join.await {
                tracing::warn!(%error, "LMS reliable command endpoint failed to join");
            }
        }

        // Clean up state on exit
        {
            let mut state = self.state.write().await;
            state.connected = false;
            state.running = false;
        }

        // Publish LmsDisconnected
        ctx.bus.publish(BusEvent::LmsDisconnected { host });

        result
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        // Extract player_id from zone_id (remove "lms:" prefix)
        let player_id = zone_id.strip_prefix("lms:").unwrap_or(zone_id);

        let result = match command {
            AdapterCommand::Play => self.control(player_id, "play", None).await,
            AdapterCommand::Pause => self.control(player_id, "pause", None).await,
            AdapterCommand::PlayPause => self.control(player_id, "play_pause", None).await,
            AdapterCommand::Stop => self.control(player_id, "stop", None).await,
            AdapterCommand::Next => self.control(player_id, "next", None).await,
            AdapterCommand::Previous => self.control(player_id, "previous", None).await,
            AdapterCommand::VolumeAbsolute(v) => self.control(player_id, "vol_abs", Some(v)).await,
            AdapterCommand::VolumeRelative(v) => self.control(player_id, "vol_rel", Some(v)).await,
            AdapterCommand::Mute(_) => {
                // LMS *does* support mute control - `mixer muting <0|1>`, verified
                // against live Lyrion 9.1.2 - and this adapter now reports mute
                // state. Only the control path is unimplemented, and exposing it
                // is player-feature work (#403), out of scope for #407.
                //
                // The previous message said "Mute not supported by LMS adapter",
                // which blamed the provider for a UHC gap. Per #392: report
                // "not yet implemented in UHC" and "not supported by this
                // provider" as distinct states, never collapse them.
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some(
                        "Mute control is not yet implemented in UHC for LMS. LMS \
                         itself supports it via `mixer muting <0|1>`; see #403."
                            .to_string(),
                    ),
                });
            }
            AdapterCommand::SetRepeat(_) | AdapterCommand::SetShuffle(_) => {
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some(
                        "Repeat and shuffle are not implemented by the LMS adapter".to_string(),
                    ),
                });
            }
        };

        match result {
            Ok(()) => Ok(AdapterCommandResponse {
                success: true,
                error: None,
            }),
            Err(e) => Ok(AdapterCommandResponse {
                success: false,
                error: Some(e.to_string()),
            }),
        }
    }
}

// Startable trait implementation via macro
crate::impl_startable!(LmsAdapter, "lms", is_configured);

/// Consume the provider-scoped LMS reliable endpoint.
///
/// A successful JSON-RPC write is only native acceptance.  The endpoint then obtains a fresh
/// `players` + `status` observation for the exact player and submits the complete Zone with the
/// command id.  `ProjectionActor` (and therefore the aggregator) is the only code that can turn
/// that observation into a confirmed command result.
async fn run_lms_command_endpoint(
    adapter: LmsAdapter,
    mut endpoint: CommandEndpoint,
    shutdown: CancellationToken,
    bridge: Arc<LmsRuntimeBridge>,
) {
    loop {
        let work = tokio::select! {
            _ = shutdown.cancelled() => break,
            work = endpoint.recv() => work,
        };
        let Some(work) = work else { break };
        let permit = match work.begin_dispatch() {
            Ok(permit) => permit,
            Err(_) => continue,
        };
        let command_id = permit.id();
        let target = permit.request().target.to_string();
        let player_id = strip_lms_prefix(&target).to_string();
        let native = match permit.request().command.clone() {
            RuntimeCommand::Control(command) => {
                execute_lms_runtime_command(&adapter, &player_id, command).await
            }
            RuntimeCommand::Hqplayer(_) => {
                Err(anyhow!("HQPlayer runtime command routed to LMS endpoint"))
            }
        };
        if let Err(error) = native {
            permit.complete_native(NativeResult::Failed(error.to_string()));
            continue;
        }

        permit.complete_native(NativeResult::Accepted);
        match adapter.coherent_player_readback(&player_id).await {
            Ok(zone) => {
                if let Err(error) = bridge.publish_zone(zone, Some(command_id)).await {
                    tracing::warn!(%error, command_id = command_id.get(), "LMS native write accepted but readback projection could not commit");
                }
            }
            Err(error) => {
                // Native acceptance is retained as an explicit indeterminate operation until its
                // deadline; the command may have applied even though LMS did not serve readback.
                tracing::warn!(%error, command_id = command_id.get(), "LMS native write accepted but coherent player readback failed");
            }
        }
    }
}

/// Content-library surface: the multiroom sync-group operations (#510) and
/// the `hifi_collections` browse operations (#531) are both implemented
/// here. LMS's own search/play surfaces are reached through their
/// dedicated paths (`LmsAdapter::search`, `LmsAdapter::control`, ...)
/// rather than through this trait's `search`/`play_uri` -- LMS's playable
/// targets are typed (`LmsPlayTarget::{Library,Url,GlobalSearchItem}`),
/// which is exactly why `hifi_search`/`hifi_play` already call `LmsAdapter`
/// directly (pre-existing debt, unaffected by this registration). `content`
/// is what lets the provider-neutral MCP registry
/// (`AdapterRegistry::library_content`) dispatch `multiroom_status`,
/// `multiroom_set_members`, `multiroom_ungroup`, `collections_browse`,
/// `collections_playlists` and `collections_favorites` to LMS the same way
/// it already dispatches the multiroom operations to Music Assistant
/// (`src/adapters/musicassistant.rs`).
#[async_trait]
impl LibraryAdapter for LmsAdapter {
    async fn search(&self, _query: &str, _limit: usize) -> Result<Vec<LibrarySearchResult>> {
        Err(anyhow!(
            "LMS search is not reachable through the generic library registry; \
             hifi_search calls LmsAdapter directly because LMS's playable targets are typed, \
             not a flat URI"
        ))
    }

    async fn play_uri(&self, _zone_id: &str, _uri: &str) -> Result<String> {
        Err(anyhow!(
            "LMS playback is not reachable through the generic library registry; \
             hifi_play/hifi_play_ref call LmsAdapter directly for the same reason as search"
        ))
    }

    async fn content(&self, operation: &str, params: &Value) -> Result<Value> {
        match operation {
            "multiroom_status" => self.multiroom_status().await,
            "multiroom_set_members" => {
                let leader = params
                    .get("leader_zone_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("LMS group operation requires leader_zone_id"))?;
                let add = params
                    .get("member_zone_ids_to_add")
                    .or_else(|| params.get("member_zone_ids"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("LMS group operation requires member_zone_ids"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                let remove = params
                    .get("member_zone_ids_to_remove")
                    .and_then(Value::as_array)
                    .map(|members| {
                        members
                            .iter()
                            .filter_map(Value::as_str)
                            .map(ToOwned::to_owned)
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default();
                self.set_group_members(leader, &add, &remove).await
            }
            "multiroom_ungroup" => {
                let members = params
                    .get("member_zone_ids")
                    .and_then(Value::as_array)
                    .ok_or_else(|| anyhow!("LMS ungroup requires member_zone_ids"))?
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect::<Vec<_>>();
                self.ungroup_members(&members).await
            }
            "collections_browse" | "collections_playlists" | "collections_favorites" => {
                self.collections_content(operation, params).await
            }
            _ => Err(anyhow!(
                "LMS content operation `{operation}` is not supported"
            )),
        }
    }
}

async fn execute_lms_runtime_command(
    adapter: &LmsAdapter,
    player_id: &str,
    command: Command,
) -> Result<()> {
    match command {
        Command::Play => {
            adapter
                .execute_control_native(player_id, "play", None)
                .await
        }
        Command::Pause => {
            adapter
                .execute_control_native(player_id, "pause", None)
                .await
        }
        Command::PlayPause => {
            adapter
                .execute_control_native(player_id, "play_pause", None)
                .await
        }
        Command::Stop => {
            adapter
                .execute_control_native(player_id, "stop", None)
                .await
        }
        Command::Next => {
            adapter
                .execute_control_native(player_id, "next", None)
                .await
        }
        Command::Previous => {
            adapter
                .execute_control_native(player_id, "previous", None)
                .await
        }
        Command::VolumeAbsolute { value, output_id } if output_id.is_none() => {
            adapter
                .execute_control_native(player_id, "vol_abs", Some(value.round() as i32))
                .await
        }
        Command::VolumeRelative { delta, output_id } if output_id.is_none() => {
            adapter
                .execute_control_native(player_id, "vol_rel", Some(delta.round() as i32))
                .await
        }
        Command::Mute { .. }
        | Command::MuteToggle { .. }
        | Command::Seek { .. }
        | Command::SeekRelative { .. }
        | Command::Shuffle { .. }
        | Command::Repeat { .. }
        | Command::VolumeAbsolute { .. }
        | Command::VolumeRelative { .. } => Err(anyhow!(
            "LMS command was not resolved to a supported native operation before dispatch"
        )),
    }
}

// =============================================================================
// LMS CLI Adapter - Handles real-time event subscription (Issue #165)
// =============================================================================

/// LMS CLI Adapter - subscribes to real-time events on port 9090
///
/// This adapter runs independently of the main LmsAdapter (polling) with its
/// own AdapterHandle retry logic. The only coordination is via the shared
/// `cli_subscription_active` flag in LmsState.
///
/// When CLI connects: flag = true → polling slows down
/// When CLI fails: flag = false → polling speeds up
#[derive(Clone)]
pub struct LmsCliAdapter {
    /// Shared state with LmsAdapter
    state: Arc<RwLock<LmsState>>,
    /// Shared RPC client for fetching player status after events
    rpc: LmsRpc,
    /// Event bus for publishing updates
    bus: SharedBus,
    /// Same reliable publication bridge as the poller. CLI events are translated into a complete
    /// cached player snapshot before entering it; neither observer gets a private aggregator path.
    runtime_bridge: Option<Arc<LmsRuntimeBridge>>,
    /// Shutdown token (separate from LmsAdapter's)
    shutdown: Arc<RwLock<CancellationToken>>,
    /// Guard against duplicate start() calls
    running: Arc<RwLock<bool>>,
    /// CLI supervisor, joined on stop to fence in-flight CLI readbacks.
    task: Arc<Mutex<Option<tokio::task::JoinHandle<()>>>>,
}

impl LmsCliAdapter {
    /// Create CLI adapter with shared state from LmsAdapter
    /// Use `create_lms_adapters()` factory function instead of calling directly.
    fn new(
        state: Arc<RwLock<LmsState>>,
        rpc: LmsRpc,
        bus: SharedBus,
        runtime_bridge: Option<Arc<LmsRuntimeBridge>>,
    ) -> Self {
        Self {
            state,
            rpc,
            bus,
            runtime_bridge,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            running: Arc::new(RwLock::new(false)),
            task: Arc::new(Mutex::new(None)),
        }
    }

    /// Check if host is configured (CLI needs host to connect)
    pub async fn is_configured(&self) -> bool {
        self.state.read().await.host.is_some()
    }
}

#[async_trait]
impl AdapterLogic for LmsCliAdapter {
    fn prefix(&self) -> &'static str {
        "lms" // Same prefix as main adapter - they share zones
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        let host = {
            let state = self.state.read().await;
            state.host.clone()
        };

        let Some(host) = host else {
            return Err(anyhow!("LMS CLI: No host configured"));
        };

        info!("[CLI] LMS CLI adapter starting for {}", host);

        // Run CLI subscription - this will retry via AdapterHandle on failure
        let result = run_cli_subscription_once(
            &host,
            &self.state,
            &ctx.bus,
            &self.rpc,
            &ctx.shutdown,
            self.runtime_bridge.as_deref(),
        )
        .await;

        // Always reset flag on exit so polling switches to fast interval
        {
            let mut state = self.state.write().await;
            state.cli_subscription_active = false;
        }

        result
    }

    async fn handle_command(
        &self,
        _zone_id: &str,
        _command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        // CLI adapter doesn't handle commands - main LmsAdapter does
        Ok(AdapterCommandResponse {
            success: false,
            error: Some("CLI adapter does not handle commands".to_string()),
        })
    }
}

#[async_trait]
impl Startable for LmsCliAdapter {
    fn name(&self) -> &'static str {
        "lms-cli"
    }

    async fn start(&self) -> Result<()> {
        if !self.is_configured().await {
            return Err(anyhow!("LMS CLI adapter not configured"));
        }

        // Guard against duplicate start() calls
        {
            let mut running = self.running.write().await;
            if *running {
                debug!("[CLI] LMS CLI adapter already running, skipping start");
                return Ok(());
            }
            *running = true;
        }

        // Create fresh cancellation token
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };

        // Create AdapterHandle and spawn with retry
        let adapter = self.clone();
        let bus = self.bus.clone();
        let running_flag = self.running.clone();
        let handle = AdapterHandle::new(adapter, bus, shutdown);

        let task = tokio::spawn(async move {
            let _ = handle.run_with_retry(RetryConfig::default()).await;
            // Reset running flag when task completes
            *running_flag.write().await = false;
        });
        *self.task.lock().await = Some(task);

        Ok(())
    }

    async fn stop(&self) {
        self.shutdown.read().await.cancel();
        let task = self.task.lock().await.take();
        if let Some(task) = task {
            if let Err(error) = task.await {
                warn!(%error, "LMS CLI supervisor did not stop cleanly");
            }
        }
    }

    async fn can_start(&self) -> bool {
        self.is_configured().await
    }
}

/// Factory function to create both LMS adapters with shared state
///
/// Returns (LmsAdapter, LmsCliAdapter) that share state and can be
/// registered independently with the coordinator.
pub fn create_lms_adapters(bus: SharedBus) -> (Arc<LmsAdapter>, Arc<LmsCliAdapter>) {
    create_lms_adapters_with_runtime(bus, None)
}

/// Construct the two LMS observers with one shared reliable command/projection bridge.
///
/// Kept as a separate factory so old test/embedding call sites retain their legacy event-bus
/// behaviour until composition explicitly installs the runtime.
pub fn create_lms_adapters_with_runtime(
    bus: SharedBus,
    runtime_bridge: Option<Arc<LmsRuntimeBridge>>,
) -> (Arc<LmsAdapter>, Arc<LmsCliAdapter>) {
    let lms = Arc::new(LmsAdapter::new_with_runtime(
        bus.clone(),
        runtime_bridge.clone(),
    ));

    // Create CLI adapter with shared state and the same publication source.
    let cli = Arc::new(LmsCliAdapter::new(
        lms.state.clone(),
        lms.rpc.clone(),
        bus,
        runtime_bridge,
    ));

    (lms, cli)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // LMS JSON type tolerance (#407)
    //
    // Every shape asserted here was observed on one live Lyrion 9.1.2 server:
    //   number   `track_id: 1`, `mixer muting ?` -> 1, `connected ?` -> 1
    //   string   `mute: "0"` / `"1"`, `mixer volume ?` -> "42",
    //            `power ?` -> "1", `playlist_cur_index: "0"`
    //   null     `mixer muting ?` -> null before the pref is ever written
    //   absent   `mute` omitted from players_loop when the pref is unset
    //
    // `.as_i64()` alone yields None on the string form. That is how a parse bug
    // becomes an empty result rather than an error - the shape of defect #407.1.
    // -------------------------------------------------------------------------

    #[test]
    fn lms_i64_reads_numbers_and_strings() {
        assert_eq!(lms_i64(Some(&json!(42))), Some(42));
        assert_eq!(lms_i64(Some(&json!("42"))), Some(42));
        assert_eq!(lms_i64(Some(&json!(-42))), Some(-42));
        assert_eq!(lms_i64(Some(&json!("-42"))), Some(-42));
        // `mixer muting ?` returns a float-shaped value on some queries, and
        // `sleep ?` returns e.g. 899.967221021652.
        assert_eq!(lms_i64(Some(&json!(1.0))), Some(1));
        assert_eq!(lms_i64(Some(&json!(" 7 "))), Some(7));
        assert_eq!(lms_i64(Some(&json!(Value::Null))), None);
        assert_eq!(lms_i64(None), None);
        assert_eq!(lms_i64(Some(&json!("not a number"))), None);
    }

    #[test]
    fn lms_flag_treats_absent_and_null_as_false() {
        assert!(!lms_flag(None));
        assert!(!lms_flag(Some(&Value::Null)));
        assert!(!lms_flag(Some(&json!(0))));
        assert!(!lms_flag(Some(&json!("0"))));
        assert!(lms_flag(Some(&json!(1))));
        // The shape live 9.1.2 actually returned for the `mute` pref.
        assert!(lms_flag(Some(&json!("1"))));
        assert!(!lms_flag(Some(&json!("garbage"))));
    }

    #[test]
    fn loop_entity_id_prefers_typed_key_and_falls_back_to_id() {
        // What live LMS emits: `<type>_id`, and no `id` key at all.
        let album = json!({"album_id": 1, "album": "Ember Light"});
        assert_eq!(loop_entity_id(&album, "album"), Some(1));

        let contributor = json!({"contributor_id": 3, "contributor": "Ember Valley Quartet"});
        assert_eq!(loop_entity_id(&contributor, "contributor"), Some(3));

        let track = json!({"track_id": 8, "track": "Ember Coda"});
        assert_eq!(loop_entity_id(&track, "track"), Some(8));

        // The typed key wins when both are present.
        let both = json!({"album_id": 5, "id": 99});
        assert_eq!(loop_entity_id(&both, "album"), Some(5));

        // Version fallback: a hypothetical server emitting only `id`.
        let legacy = json!({"id": 7, "album": "Whatever"});
        assert_eq!(loop_entity_id(&legacy, "album"), Some(7));

        // String form, and non-positive or missing ids are not addressable -
        // playlistcontrol cannot act on them.
        assert_eq!(
            loop_entity_id(&json!({"album_id": "12"}), "album"),
            Some(12)
        );
        assert_eq!(loop_entity_id(&json!({"album_id": 0}), "album"), None);
        assert_eq!(loop_entity_id(&json!({"album_id": -1}), "album"), None);
        assert_eq!(loop_entity_id(&json!({"album": "no id"}), "album"), None);
    }

    // -------------------------------------------------------------------------
    // LmsPlayTarget (#396): ref-minting safety
    //
    // The hard rule this section exists to prove: a `GlobalSearchItem` handle
    // -- LMS's positional breadcrumb, verified against live Lyrion 9.1.2 to
    // change across identical calls and to silently re-resolve a fabricated
    // id by position on a cache miss -- must never be the thing a ref mints
    // through. Break `mintable` to prefer it and this section fails.
    // -------------------------------------------------------------------------

    fn result_with(
        id: i64,
        url: Option<&str>,
        item_id: Option<&str>,
        result_type: LmsSearchResultType,
    ) -> LmsSearchResult {
        LmsSearchResult {
            result_type,
            id,
            title: "Some Title".to_string(),
            artist: Some("Some Artist".to_string()),
            album: None,
            url: url.map(String::from),
            item_id: item_id.map(String::from),
        }
    }

    #[test]
    fn mintable_never_returns_a_global_search_item_target() {
        // The unsafe case: only a positional breadcrumb is available. No ref
        // must be mintable for this -- an absent ref is honest, a ref through
        // this handle can silently play the wrong thing.
        let breadcrumb_only = result_with(0, None, Some("3.7.2"), LmsSearchResultType::Track);
        assert_eq!(LmsPlayTarget::mintable(&breadcrumb_only), None);

        // Even when a breadcrumb is present *alongside* a durable handle,
        // `mintable` must still prefer the durable one rather than merely
        // tolerating the breadcrumb's absence.
        let both = result_with(
            42,
            Some("http://stream.example/track"),
            Some("3.7.2"),
            LmsSearchResultType::Track,
        );
        assert_eq!(
            LmsPlayTarget::mintable(&both),
            Some(LmsPlayTarget::Library {
                kind: LmsSearchResultType::Track,
                id: 42
            })
        );

        // And no `LmsPlayTarget::mintable` result is ever the `GlobalSearchItem`
        // variant, for any input -- the general form of the rule above.
        for result in [
            result_with(0, None, Some("1.0.0"), LmsSearchResultType::Track),
            result_with(0, None, Some("9.9.9"), LmsSearchResultType::Album),
            result_with(7, None, Some("2.1.4"), LmsSearchResultType::Artist),
            result_with(
                0,
                Some("http://x"),
                Some("5.5.5"),
                LmsSearchResultType::Track,
            ),
        ] {
            assert!(
                !matches!(
                    LmsPlayTarget::mintable(&result),
                    Some(LmsPlayTarget::GlobalSearchItem { .. })
                ),
                "mintable() must never yield GlobalSearchItem, got {:?} for {:?}",
                LmsPlayTarget::mintable(&result),
                result
            );
        }
    }

    #[test]
    fn mintable_prefers_library_then_url_then_nothing() {
        let library_only = result_with(5, None, None, LmsSearchResultType::Album);
        assert_eq!(
            LmsPlayTarget::mintable(&library_only),
            Some(LmsPlayTarget::Library {
                kind: LmsSearchResultType::Album,
                id: 5
            })
        );

        let url_only = result_with(
            0,
            Some("http://stream.example/x"),
            None,
            LmsSearchResultType::Track,
        );
        assert_eq!(
            LmsPlayTarget::mintable(&url_only),
            Some(LmsPlayTarget::Url {
                url: "http://stream.example/x".to_string()
            })
        );

        let neither = result_with(0, None, None, LmsSearchResultType::Track);
        assert_eq!(LmsPlayTarget::mintable(&neither), None);
    }

    /// `from_search_hit` is `search_and_play`'s existing precedence
    /// (`item_id`, then `url`, then a positive library id), extracted
    /// unchanged -- this is what makes the extraction behavior-preserving.
    /// It is a *different* precedence than `mintable`'s on purpose: it feeds
    /// only `search_and_play`'s unref'd first-result path, which this issue
    /// leaves untouched.
    #[test]
    fn from_search_hit_preserves_search_and_plays_original_precedence() {
        let all_three = result_with(
            5,
            Some("http://stream.example/x"),
            Some("1.2.3"),
            LmsSearchResultType::Track,
        );
        assert_eq!(
            LmsPlayTarget::from_search_hit(&all_three),
            Some(LmsPlayTarget::GlobalSearchItem {
                item_id: "1.2.3".to_string()
            }),
            "item_id must win when present, matching search_and_play's original order"
        );

        let url_and_id = result_with(
            5,
            Some("http://stream.example/x"),
            None,
            LmsSearchResultType::Track,
        );
        assert_eq!(
            LmsPlayTarget::from_search_hit(&url_and_id),
            Some(LmsPlayTarget::Url {
                url: "http://stream.example/x".to_string()
            }),
            "url must win over a bare library id when there is no item_id"
        );

        let id_only = result_with(5, None, None, LmsSearchResultType::Track);
        assert_eq!(
            LmsPlayTarget::from_search_hit(&id_only),
            Some(LmsPlayTarget::Library {
                kind: LmsSearchResultType::Track,
                id: 5
            })
        );

        let nothing = result_with(0, None, None, LmsSearchResultType::Track);
        assert_eq!(LmsPlayTarget::from_search_hit(&nothing), None);
    }

    #[test]
    fn describe_command_names_the_player_and_the_command() {
        let described = describe_command(
            Some("02:00:00:00:00:11"),
            &[
                json!("players"),
                json!(0),
                json!(100),
                json!("playerprefs:mute"),
            ],
        );
        assert_eq!(
            described,
            "02:00:00:00:00:11 players 0 100 playerprefs:mute"
        );

        // Server-scoped calls pass no player id; the message must still be clear.
        let server = describe_command(None, &[json!("search"), json!(0), json!(10)]);
        assert_eq!(server, "<server> search 0 10");
    }

    #[test]
    fn truncate_for_log_bounds_runaway_bodies() {
        assert_eq!(truncate_for_log("short"), "short");
        let long = "x".repeat(500);
        let truncated = truncate_for_log(&long);
        assert_eq!(truncated.chars().count(), 201, "200 chars plus an ellipsis");
        assert!(truncated.ends_with('…'));
        // Must not split a multi-byte character.
        let unicode = "é".repeat(500);
        assert_eq!(truncate_for_log(&unicode).chars().count(), 201);
    }

    // -------------------------------------------------------------------------
    // CLI Event Parsing Tests (TDD)
    // -------------------------------------------------------------------------

    #[test]
    fn test_parse_cli_event_playlist_newsong() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist newsong Track%20Name 5";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist {
                player_id,
                command,
                track_name,
                index,
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(command, "newsong");
                assert_eq!(track_name, Some("Track Name".to_string()));
                assert_eq!(index, Some(5));
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_playlist_play() {
        let line = "00%3A04%3A20%3Axx%3Ayy%3Azz playlist play";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist {
                player_id,
                command,
                track_name,
                index,
            } => {
                assert_eq!(player_id, "00:04:20:xx:yy:zz");
                assert_eq!(command, "play");
                assert_eq!(track_name, None);
                assert_eq!(index, None);
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_mixer_volume_absolute() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc mixer volume 75";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer {
                player_id,
                param,
                value,
                is_relative,
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(param, "volume");
                assert_eq!(value, Some(75.0));
                assert!(!is_relative, "75 should be absolute (no sign prefix)");
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_mixer_volume_relative_positive() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc mixer volume +2.5";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer {
                value, is_relative, ..
            } => {
                assert_eq!(value, Some(2.5));
                assert!(is_relative, "+2.5 should be relative");
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_mixer_volume_relative_negative() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc mixer volume -3";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer {
                value, is_relative, ..
            } => {
                assert_eq!(value, Some(-3.0));
                assert!(is_relative, "-3 should be relative");
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_power_on() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc power 1";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Power { player_id, state } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert!(state);
            }
            _ => panic!("Expected Power event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_power_off() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc power 0";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Power { player_id, state } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert!(!state);
            }
            _ => panic!("Expected Power event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_client_new() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc client new";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Client { player_id, action } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(action, "new");
            }
            _ => panic!("Expected Client event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_client_disconnect() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc client disconnect";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Client { player_id, action } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(action, "disconnect");
            }
            _ => panic!("Expected Client event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_unknown() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc unknown_command arg1 arg2";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Unknown { raw_line } => {
                assert_eq!(raw_line, line);
            }
            _ => panic!("Expected Unknown event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_empty_line() {
        let event = parse_cli_event("");
        assert!(matches!(event, CliEvent::Unknown { .. }));

        let event = parse_cli_event("   ");
        assert!(matches!(event, CliEvent::Unknown { .. }));
    }

    #[test]
    fn test_parse_cli_event_single_token() {
        let event = parse_cli_event("player_only");
        assert!(matches!(event, CliEvent::Unknown { .. }));
    }

    #[test]
    fn test_parse_cli_event_unencoded_player_id() {
        // Some LMS versions might send unencoded player IDs
        let line = "00:04:20:aa:bb:cc mixer volume 50";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer {
                player_id,
                param,
                value,
                is_relative,
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(param, "volume");
                assert_eq!(value, Some(50.0));
                assert!(!is_relative);
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_special_characters_in_track_name() {
        // Track name with special characters
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist newsong Hello%2C%20World%21 0";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist { track_name, .. } => {
                assert_eq!(track_name, Some("Hello, World!".to_string()));
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_playlist_pause() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist pause 1";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist {
                player_id, command, ..
            } => {
                assert_eq!(player_id, "00:04:20:aa:bb:cc");
                assert_eq!(command, "pause");
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_playlist_stop() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc playlist stop";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Playlist { command, .. } => {
                assert_eq!(command, "stop");
            }
            _ => panic!("Expected Playlist event, got {:?}", event),
        }
    }

    #[test]
    fn test_parse_cli_event_mixer_muting() {
        let line = "00%3A04%3A20%3Aaa%3Abb%3Acc mixer muting 1";
        let event = parse_cli_event(line);

        match event {
            CliEvent::Mixer { param, value, .. } => {
                assert_eq!(param, "muting");
                assert_eq!(value, Some(1.0));
            }
            _ => panic!("Expected Mixer event, got {:?}", event),
        }
    }

    // -------------------------------------------------------------------------
    // Volume Parsing Tests (Issue #299)
    // -------------------------------------------------------------------------

    /// Helper: extract volume from a JSON "mixer volume" value the same way
    /// get_player_status does.
    fn parse_volume(json_value: serde_json::Value) -> i32 {
        json_value.as_f64().unwrap_or(0.0).round() as i32
    }

    #[test]
    fn test_volume_parsing_integer() {
        assert_eq!(parse_volume(json!(75)), 75);
        assert_eq!(parse_volume(json!(0)), 0);
        assert_eq!(parse_volume(json!(100)), 100);
    }

    #[test]
    fn test_volume_parsing_float() {
        // LMS with 2.5-step increments produces these values
        assert_eq!(parse_volume(json!(52.5)), 53);
        assert_eq!(parse_volume(json!(57.5)), 58);
        assert_eq!(parse_volume(json!(50.0)), 50);
        assert_eq!(parse_volume(json!(99.9)), 100);
        assert_eq!(parse_volume(json!(0.4)), 0);
    }

    #[test]
    fn test_volume_parsing_null_defaults_to_zero() {
        let val: Option<serde_json::Value> = None;
        let volume = val.and_then(|v| v.as_f64()).unwrap_or(0.0).round() as i32;
        assert_eq!(volume, 0);
    }
}
