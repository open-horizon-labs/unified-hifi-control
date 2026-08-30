//! Spotify Web API controller adapter.
//!
//! This adapter controls existing Spotify Connect devices. It deliberately
//! does not implement a Spotify Connect receiver: each available Connect
//! device is exposed as its own `spotify:<device-id>` UHC zone.
//!
//! Authorization is intentionally supplied by the surrounding application.
//! The adapter accepts an access token, refreshes it through the authorization
//! layer when needed, and refuses commands when credentials are absent. OAuth
//! and browser onboarding belong to the shared provider authorization layer.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic, LibraryAdapter,
    LibrarySearchResult,
};
use crate::bus::{
    BusEvent, NowPlaying, PlaybackState, PrefixedZoneId, ProviderAccount, RepeatMode, SharedBus,
    VolumeControl, VolumeScale, Zone,
};

const SPOTIFY_API_URL: &str = "https://api.spotify.com/v1";
const SPOTIFY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// OAuth credentials needed to call the Spotify Web API.
///
/// `expires_at` is a Unix timestamp in seconds. The token itself is not
/// refreshed here: refresh-token exchange needs the client credentials and
/// redirect policy owned by the shared authorization layer (#463).
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpotifyToken {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
}

impl fmt::Debug for SpotifyToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SpotifyToken")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .finish()
    }
}

/// Refreshes an expired Spotify access token. The authorization layer owns the
/// provider client credentials and durable storage; the adapter only invokes
/// this narrow callback immediately before a Web API request.
#[async_trait]
pub trait SpotifyTokenRefresher: Send + Sync {
    async fn refresh(&self, current: &SpotifyToken) -> Result<SpotifyToken>;
}

impl SpotifyToken {
    /// Return true when the token is expired at `now` (Unix seconds).
    /// A 30-second safety window avoids sending a command with a token that is
    /// about to expire while the provider request is in flight.
    pub fn is_expired(&self, now: u64) -> bool {
        self.expires_at
            .map(|expires_at| expires_at <= now.saturating_add(30))
            .unwrap_or(false)
    }
}

/// A Spotify Connect playback device returned by `/me/player/devices`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotifyDevice {
    pub id: String,
    pub name: String,
    #[serde(rename = "type", default)]
    pub device_type: String,
    #[serde(default)]
    pub is_active: bool,
    #[serde(default)]
    pub is_restricted: bool,
    #[serde(default)]
    pub supports_volume: bool,
    #[serde(default)]
    pub volume_percent: Option<u8>,
}

impl SpotifyDevice {
    /// Convert a Connect device into a UHC zone.
    ///
    /// Playback metadata is only associated with Spotify's currently active
    /// device; Spotify does not expose independent now-playing state for every
    /// available device in one response.
    pub fn to_zone(&self, playback: Option<&SpotifyPlayback>) -> Zone {
        let zone_id = PrefixedZoneId::spotify(&self.id).to_string();
        let controllable = !self.is_restricted;
        let now_playing = playback.and_then(SpotifyPlayback::to_now_playing);
        let state = playback
            .map(|p| {
                if p.is_playing {
                    PlaybackState::Playing
                } else {
                    PlaybackState::Paused
                }
            })
            .unwrap_or(PlaybackState::Unknown);

        Zone {
            zone_id: zone_id.clone(),
            zone_name: self.name.clone(),
            state,
            volume_control: self
                .supports_volume
                .then(|| {
                    self.volume_percent.map(|value| VolumeControl {
                        value: f32::from(value),
                        min: 0.0,
                        max: 100.0,
                        step: 1.0,
                        is_muted: false,
                        scale: VolumeScale::Percentage,
                        output_id: Some(zone_id),
                    })
                })
                .flatten(),
            now_playing,
            source: "spotify".to_string(),
            is_controllable: controllable,
            is_seekable: false,
            last_updated: now_millis(),
            is_play_allowed: controllable,
            is_pause_allowed: controllable,
            is_next_allowed: controllable,
            is_previous_allowed: controllable,
        }
    }
}

/// The currently playing Spotify item and playback position.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpotifyPlayback {
    #[serde(default)]
    pub is_playing: bool,
    #[serde(default)]
    pub progress_ms: Option<u64>,
    #[serde(default)]
    pub device: Option<SpotifyPlaybackDevice>,
    #[serde(default)]
    pub item: Option<SpotifyTrack>,
    #[serde(default)]
    pub repeat_state: Option<String>,
    #[serde(default)]
    pub shuffle_state: Option<bool>,
}

impl SpotifyPlayback {
    pub fn to_now_playing(&self) -> Option<NowPlaying> {
        let track = self.item.as_ref()?;
        Some(NowPlaying {
            title: track.name.clone(),
            artist: track
                .artists
                .first()
                .map(|artist| artist.name.clone())
                .unwrap_or_default(),
            album: track.album.name.clone(),
            image_key: track.album.images.first().map(|image| image.url.clone()),
            seek_position: self.progress_ms.map(|ms| ms as f64 / 1000.0),
            duration: track.duration_ms.map(|ms| ms as f64 / 1000.0),
            metadata: None,
            repeat_mode: self.repeat_mode(),
            shuffle: self.shuffle_state,
        })
    }

    fn repeat_mode(&self) -> Option<RepeatMode> {
        match self.repeat_state.as_deref()? {
            "off" => Some(RepeatMode::Off),
            "context" => Some(RepeatMode::All),
            "track" => Some(RepeatMode::One),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpotifyPlaybackDevice {
    pub id: Option<String>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpotifyTrack {
    pub name: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub artists: Vec<SpotifyArtist>,
    pub album: SpotifyAlbum,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// A queue entry returned by Spotify's Web API.
///
/// Tracks and episodes share `name`, `uri`, and `duration_ms`, while only
/// tracks carry `artists`/`album` and only episodes carry `show`. Keeping those
/// fields optional lets the adapter represent both item kinds without making
/// the MCP layer parse provider-specific JSON.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyQueueItem {
    #[serde(default)]
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<SpotifyArtist>,
    #[serde(default)]
    pub album: Option<SpotifyAlbum>,
    #[serde(default)]
    pub show: Option<SpotifyShow>,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

/// The current Spotify playback queue.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotifyQueue {
    #[serde(default)]
    pub currently_playing: Option<SpotifyQueueItem>,
    #[serde(default)]
    pub queue: Vec<SpotifyQueueItem>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotifyShow {
    pub name: String,
}

/// A bounded Spotify Web API page. The adapter always clamps `limit` before
/// sending a request; `next`/`previous` remain provider links for callers that
/// want to continue walking a catalog surface.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotifyPage<T> {
    #[serde(default)]
    pub href: String,
    #[serde(default)]
    pub limit: usize,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub previous: Option<String>,
    #[serde(default)]
    pub total: usize,
    #[serde(default)]
    pub items: Vec<T>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyCategory {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub icons: Vec<SpotifyImage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyPlaylist {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub public: Option<bool>,
    #[serde(default)]
    pub collaborative: bool,
    #[serde(default)]
    pub images: Vec<SpotifyImage>,
    #[serde(default)]
    pub items: Option<SpotifyPlaylistItemSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SpotifyPlaylistItemSummary {
    #[serde(default)]
    pub total: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyPlaylistItem {
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub item: Option<SpotifyQueueItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifySavedTrack {
    #[serde(default)]
    pub added_at: Option<String>,
    pub track: SpotifyQueueItem,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyAlbumSummary {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub artists: Vec<SpotifyArtist>,
    #[serde(default)]
    pub images: Vec<SpotifyImage>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct SpotifySnapshot {
    #[serde(default)]
    snapshot_id: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyArtist {
    pub name: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyAlbum {
    pub name: String,
    #[serde(default)]
    pub images: Vec<SpotifyImage>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct SpotifyImage {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DevicesResponse {
    #[serde(default)]
    devices: Vec<SpotifyDeviceResponse>,
}

/// Spotify documents device IDs as nullable. UHC zones require a stable ID, so
/// the wire shape stays nullable until the inventory boundary and anonymous
/// devices are skipped without discarding the rest of the poll.
#[derive(Debug, Clone, Deserialize)]
struct SpotifyDeviceResponse {
    id: Option<String>,
    name: String,
    #[serde(rename = "type", default)]
    device_type: String,
    #[serde(default)]
    is_active: bool,
    #[serde(default)]
    is_restricted: bool,
    #[serde(default)]
    supports_volume: bool,
    #[serde(default)]
    volume_percent: Option<u8>,
}

impl SpotifyDeviceResponse {
    fn into_device(self) -> Option<SpotifyDevice> {
        let id = self.id.filter(|id| !id.trim().is_empty())?;
        Some(SpotifyDevice {
            id,
            name: self.name,
            device_type: self.device_type,
            is_active: self.is_active,
            is_restricted: self.is_restricted,
            supports_volume: self.supports_volume,
            volume_percent: self.volume_percent,
        })
    }
}

#[derive(Debug, Clone, Deserialize)]
struct SpotifySearchResponse {
    #[serde(default)]
    tracks: SpotifySearchPage<SpotifySearchTrack>,
    #[serde(default)]
    albums: SpotifySearchPage<SpotifySearchAlbum>,
    #[serde(default)]
    artists: SpotifySearchPage<SpotifySearchArtist>,
}

#[derive(Debug, Clone, Deserialize)]
struct SpotifySearchPage<T> {
    #[serde(default)]
    items: Vec<T>,
}

impl<T> Default for SpotifySearchPage<T> {
    fn default() -> Self {
        Self { items: Vec::new() }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SpotifySearchTrack {
    name: String,
    uri: String,
    #[serde(default)]
    artists: Vec<SpotifyArtist>,
    album: SpotifyAlbum,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SpotifySearchAlbum {
    name: String,
    uri: String,
    #[serde(default)]
    artists: Vec<SpotifyArtist>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct SpotifySearchArtist {
    name: String,
    uri: String,
}

#[derive(Debug, Clone, Deserialize)]
struct SpotifyUserProfile {
    id: String,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    email: Option<String>,
}

#[derive(Default)]
struct SpotifyState {
    token: Option<SpotifyToken>,
    devices: HashMap<String, SpotifyDevice>,
    playback: Option<SpotifyPlayback>,
    account: Option<ProviderAccount>,
    account_loaded: bool,
    running: bool,
}

/// Direct Spotify Connect controller adapter.
#[derive(Clone)]
pub struct SpotifyAdapter {
    state: Arc<RwLock<SpotifyState>>,
    client: Client,
    api_base_url: String,
    bus: SharedBus,
    shutdown: Arc<RwLock<CancellationToken>>,
    refresher: Arc<RwLock<Option<Arc<dyn SpotifyTokenRefresher>>>>,
}

impl SpotifyAdapter {
    /// Create an adapter using Spotify's production Web API.
    pub fn new(bus: SharedBus) -> Self {
        Self::with_base_url(bus, SPOTIFY_API_URL.to_string())
    }

    /// Create an adapter pointed at a custom API URL (used by protocol tests).
    pub fn with_base_url(bus: SharedBus, api_base_url: String) -> Self {
        #[allow(clippy::expect_used)]
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .expect("failed to create Spotify HTTP client");
        Self {
            state: Arc::new(RwLock::new(SpotifyState::default())),
            client,
            api_base_url: api_base_url.trim_end_matches('/').to_string(),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            refresher: Arc::new(RwLock::new(None)),
        }
    }

    /// Install or replace the token supplied by the authorization layer.
    pub async fn set_token(&self, token: SpotifyToken) {
        let mut state = self.state.write().await;
        state.token = Some(token);
        state.account = None;
        state.account_loaded = false;
    }

    /// Install the authorization-layer refresh callback.
    pub async fn set_token_refresher(&self, refresher: Arc<dyn SpotifyTokenRefresher>) {
        *self.refresher.write().await = Some(refresher);
    }

    /// Return the in-memory token metadata without exposing it through HTTP.
    pub async fn token_metadata(&self) -> Option<(bool, Option<u64>)> {
        self.state
            .read()
            .await
            .token
            .as_ref()
            .map(|token| (!token.is_expired(now_secs()), token.expires_at))
    }

    /// Remove credentials and cached provider state.
    pub async fn clear_token(&self) {
        let mut state = self.state.write().await;
        state.token = None;
        state.devices.clear();
        state.playback = None;
        state.account = None;
        state.account_loaded = false;
    }

    /// Return whether an unexpired access token is available.
    pub async fn is_configured(&self) -> bool {
        let token = self.state.read().await.token.clone();
        let Some(token) = token else { return false };
        if token.access_token.is_empty() {
            return false;
        }
        if !token.is_expired(now_secs()) {
            return true;
        }
        token.refresh_token.is_some() && self.refresher.read().await.is_some()
    }

    /// Return the last device inventory fetched from Spotify.
    pub async fn get_devices(&self) -> Vec<SpotifyDevice> {
        self.state.read().await.devices.values().cloned().collect()
    }

    /// Return the last non-secret account identity fetched from Spotify.
    pub async fn get_account(&self) -> Option<ProviderAccount> {
        self.state.read().await.account.clone()
    }

    /// Search Spotify's catalog and return provider URIs suitable for an
    /// exact play-by-reference call. Search is intentionally independent of
    /// the current playback device; the zone is selected when the URI plays.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<LibrarySearchResult>> {
        let query = query.trim();
        if query.is_empty() {
            return Err(anyhow!("Spotify search query cannot be empty"));
        }
        let limit = spotify_search_limit(limit);
        let path = format!(
            "/search?q={}&type=track%2Calbum%2Cartist&limit={limit}",
            urlencoding::encode(query)
        );
        let response: SpotifySearchResponse = self.request_json(Method::GET, &path).await?;
        let mut results = Vec::with_capacity(
            response.tracks.items.len()
                + response.albums.items.len()
                + response.artists.items.len(),
        );
        results.extend(
            response
                .tracks
                .items
                .into_iter()
                .map(|track| LibrarySearchResult {
                    title: track.name,
                    subtitle: match (track.artists.first(), track.album.name.is_empty()) {
                        (Some(artist), false) => {
                            Some(format!("{} - {}", artist.name, track.album.name))
                        }
                        (Some(artist), true) => Some(artist.name.clone()),
                        (None, false) => Some(track.album.name),
                        (None, true) => None,
                    },
                    uri: track.uri,
                }),
        );
        results.extend(response.albums.items.into_iter().map(|album| {
            LibrarySearchResult {
                title: album.name,
                subtitle: album
                    .artists
                    .first()
                    .map(|artist| format!("Album by {}", artist.name)),
                uri: album.uri,
            }
        }));
        results.extend(
            response
                .artists
                .items
                .into_iter()
                .map(|artist| LibrarySearchResult {
                    title: artist.name,
                    subtitle: Some("Artist".to_string()),
                    uri: artist.uri,
                }),
        );
        Ok(results)
    }

    /// Spotify removed catalog categories from the Web API surface available
    /// to new Development Mode applications in February 2026.
    pub async fn browse_categories(
        &self,
        _limit: usize,
        _offset: usize,
        _country: Option<&str>,
        _locale: Option<&str>,
    ) -> Result<SpotifyPage<SpotifyCategory>> {
        Err(development_mode_browse_unavailable("catalog categories"))
    }

    /// Spotify removed category playlists from the Web API surface available
    /// to new Development Mode applications in February 2026.
    pub async fn browse_category_playlists(
        &self,
        _category_id: &str,
        _limit: usize,
        _offset: usize,
        _country: Option<&str>,
    ) -> Result<SpotifyPage<SpotifyPlaylist>> {
        Err(development_mode_browse_unavailable("category playlists"))
    }

    /// Spotify removed featured playlists from the Web API surface available
    /// to new Development Mode applications in February 2026.
    pub async fn browse_featured_playlists(
        &self,
        _limit: usize,
        _offset: usize,
        _country: Option<&str>,
        _locale: Option<&str>,
        _timestamp: Option<&str>,
    ) -> Result<SpotifyPage<SpotifyPlaylist>> {
        Err(development_mode_browse_unavailable("featured playlists"))
    }

    /// Spotify removed new releases from the Web API surface available to new
    /// Development Mode applications in February 2026.
    pub async fn browse_new_releases(
        &self,
        _limit: usize,
        _offset: usize,
        _country: Option<&str>,
    ) -> Result<SpotifyPage<SpotifyAlbumSummary>> {
        Err(development_mode_browse_unavailable("new releases"))
    }

    /// List playlists visible to the current Spotify user.
    pub async fn get_playlists(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<SpotifyPage<SpotifyPlaylist>> {
        let path = format!(
            "/me/playlists?limit={}&offset={offset}",
            spotify_page_limit(limit)
        );
        self.request_json(Method::GET, &path).await
    }

    /// Read one playlist's items. Spotify calls this endpoint `/items`; the
    /// adapter keeps the item URI and metadata intact for reference minting.
    pub async fn get_playlist_items(
        &self,
        playlist_id: &str,
        limit: usize,
        offset: usize,
    ) -> Result<SpotifyPage<SpotifyPlaylistItem>> {
        let playlist_id = playlist_id.trim();
        if playlist_id.is_empty() {
            return Err(anyhow!("Spotify playlist id cannot be empty"));
        }
        let path = format!(
            "/playlists/{}/items?limit={}&offset={offset}",
            urlencoding::encode(playlist_id),
            spotify_page_limit(limit)
        );
        self.request_json(Method::GET, &path).await
    }

    /// List the user's saved (liked) tracks.
    pub async fn get_saved_tracks(
        &self,
        limit: usize,
        offset: usize,
    ) -> Result<SpotifyPage<SpotifySavedTrack>> {
        let path = format!(
            "/me/tracks?limit={}&offset={offset}",
            spotify_page_limit(limit)
        );
        self.request_json(Method::GET, &path).await
    }

    /// Check which Spotify track IDs are in the user's saved library.
    pub async fn check_saved_tracks(&self, ids: &[String]) -> Result<Vec<bool>> {
        let uris = spotify_track_uris(ids)?;
        let path = format!("/me/library/contains?uris={uris}");
        self.request_json(Method::GET, &path).await
    }

    /// Save tracks to the user's Spotify library.
    pub async fn save_tracks(&self, ids: &[String]) -> Result<()> {
        let uris = spotify_track_uris(ids)?;
        self.send_library_command(Method::PUT, &format!("/me/library?uris={uris}"))
            .await
    }

    /// Remove tracks from the user's Spotify library.
    pub async fn remove_saved_tracks(&self, ids: &[String]) -> Result<()> {
        let uris = spotify_track_uris(ids)?;
        self.send_library_command(Method::DELETE, &format!("/me/library?uris={uris}"))
            .await
    }

    /// Create a playlist owned by the authenticated Spotify user.
    pub async fn create_playlist(
        &self,
        name: &str,
        public: bool,
        collaborative: bool,
        description: Option<&str>,
    ) -> Result<SpotifyPlaylist> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("Spotify playlist name cannot be empty"));
        }
        let body = serde_json::json!({
            "name": name,
            "public": public,
            "collaborative": collaborative,
            "description": description.unwrap_or_default(),
        });
        self.request_json_body(Method::POST, "/me/playlists", &body)
            .await
    }

    /// Update playlist metadata without changing its item list.
    pub async fn update_playlist(
        &self,
        playlist_id: &str,
        name: Option<&str>,
        public: Option<bool>,
        collaborative: Option<bool>,
        description: Option<&str>,
    ) -> Result<()> {
        let playlist_id = playlist_id.trim();
        if playlist_id.is_empty() {
            return Err(anyhow!("Spotify playlist id cannot be empty"));
        }
        let mut body = serde_json::Map::new();
        if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
            body.insert("name".to_string(), serde_json::json!(name));
        }
        if let Some(public) = public {
            body.insert("public".to_string(), serde_json::json!(public));
        }
        if let Some(collaborative) = collaborative {
            body.insert(
                "collaborative".to_string(),
                serde_json::json!(collaborative),
            );
        }
        if let Some(description) = description {
            body.insert("description".to_string(), serde_json::json!(description));
        }
        if body.is_empty() {
            return Err(anyhow!("Spotify playlist update has no fields"));
        }
        self.send_json_command(
            Method::PUT,
            &format!("/playlists/{}", urlencoding::encode(playlist_id)),
            &serde_json::Value::Object(body),
        )
        .await
    }

    /// Add track or episode URIs to a playlist, returning Spotify's snapshot.
    pub async fn add_playlist_items(
        &self,
        playlist_id: &str,
        uris: &[String],
        position: Option<usize>,
    ) -> Result<Option<String>> {
        let playlist_id = spotify_playlist_id(playlist_id)?;
        let uris = spotify_content_uris(uris)?;
        let mut body = serde_json::json!({"uris": uris});
        if let Some(position) = position {
            body["position"] = serde_json::json!(position);
        }
        let response: SpotifySnapshot = self
            .request_json_body(
                Method::POST,
                &format!("/playlists/{playlist_id}/items"),
                &body,
            )
            .await?;
        Ok(response.snapshot_id)
    }

    /// Replace a playlist's item list with track or episode URIs.
    pub async fn replace_playlist_items(
        &self,
        playlist_id: &str,
        uris: &[String],
    ) -> Result<Option<String>> {
        let playlist_id = spotify_playlist_id(playlist_id)?;
        let uris = spotify_content_uris(uris)?;
        let response: SpotifySnapshot = self
            .request_json_body(
                Method::PUT,
                &format!("/playlists/{playlist_id}/items"),
                &serde_json::json!({"uris": uris}),
            )
            .await?;
        Ok(response.snapshot_id)
    }

    /// Remove playlist items by URI, optionally guarded by a snapshot ID.
    pub async fn remove_playlist_items(
        &self,
        playlist_id: &str,
        uris: &[String],
        snapshot_id: Option<&str>,
    ) -> Result<Option<String>> {
        let playlist_id = spotify_playlist_id(playlist_id)?;
        let uris = spotify_content_uris(uris)?;
        let tracks: Vec<Value> = uris
            .iter()
            .map(|uri| serde_json::json!({"uri": uri}))
            .collect();
        let mut body = serde_json::json!({"items": tracks});
        if let Some(snapshot_id) = snapshot_id.filter(|value| !value.is_empty()) {
            body["snapshot_id"] = serde_json::json!(snapshot_id);
        }
        let response: SpotifySnapshot = self
            .request_json_body(
                Method::DELETE,
                &format!("/playlists/{playlist_id}/items"),
                &body,
            )
            .await?;
        Ok(response.snapshot_id)
    }

    /// Play a Spotify track, album, artist, or other context URI on a UHC
    /// Spotify zone. Track URIs use uris; context URIs use context_uri.
    pub async fn play_uri(&self, zone_id: &str, uri: &str) -> Result<String> {
        let uri = uri.trim();
        if uri.is_empty() || !uri.starts_with("spotify:") {
            return Err(anyhow!("Spotify play target must be a spotify: URI"));
        }
        if !self.is_configured().await {
            return Err(anyhow!(
                "Spotify access token is not configured or has expired"
            ));
        }
        let device_id = zone_id.strip_prefix("spotify:").unwrap_or(zone_id);
        let device = self
            .state
            .read()
            .await
            .devices
            .get(device_id)
            .cloned()
            .ok_or_else(|| anyhow!("Spotify device is not currently available"))?;
        if device.is_restricted {
            return Err(anyhow!("Spotify device is restricted"));
        }
        let query = format!("?device_id={}", urlencoding::encode(device_id));
        let body = if uri.starts_with("spotify:track:") {
            serde_json::json!({ "uris": [uri] })
        } else {
            serde_json::json!({ "context_uri": uri })
        };
        self.send_json_command(Method::PUT, &format!("/me/player/play{query}"), &body)
            .await?;
        if let Err(error) = self.update().await {
            debug!("Spotify URI play succeeded but refresh failed: {}", error);
        }
        Ok(format!("Playing {uri} on {}", device.name))
    }

    /// Read Spotify's account-level playback queue for a known Connect device.
    ///
    /// The Web API queue endpoint has no device selector, but requiring a known
    /// zone keeps MCP scope honest and prevents a typo from returning another
    /// device's queue. An empty `currently_playing` value is valid when Spotify
    /// has no active playback.
    pub async fn get_queue(&self, zone_id: &str) -> Result<SpotifyQueue> {
        self.require_device(zone_id).await?;
        self.request_json(Method::GET, "/me/player/queue").await
    }

    /// Add a track or episode URI to Spotify's playback queue for a device.
    pub async fn add_to_queue(&self, zone_id: &str, uri: &str) -> Result<()> {
        let device_id = self.require_device(zone_id).await?;
        if !(uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:")) {
            return Err(anyhow!(
                "Spotify queue URI must be a spotify:track: or spotify:episode: URI"
            ));
        }
        if uri.ends_with(':') {
            return Err(anyhow!("Spotify queue URI is missing its item id"));
        }
        let query = format!(
            "?uri={}&device_id={}",
            urlencoding::encode(uri),
            urlencoding::encode(&device_id)
        );
        self.send_command(Method::POST, &format!("/me/player/queue{query}"))
            .await
    }

    async fn require_device(&self, zone_id: &str) -> Result<String> {
        let device_id = zone_id.strip_prefix("spotify:").unwrap_or(zone_id);
        let state = self.state.read().await;
        let device = state
            .devices
            .get(device_id)
            .ok_or_else(|| anyhow!("Spotify device is not currently available"))?;
        if device.is_restricted {
            return Err(anyhow!("Spotify device is restricted"));
        }
        Ok(device_id.to_string())
    }

    /// Poll device inventory and current playback, publishing zones.
    pub async fn update(&self) -> Result<()> {
        let (previous_devices, previous_playback) = {
            let state = self.state.read().await;
            (state.devices.clone(), state.playback.clone())
        };
        let devices = self.fetch_devices().await?;
        let playback = self.fetch_playback().await?;
        let should_fetch_account = !self.state.read().await.account_loaded;
        let account = if should_fetch_account {
            match self.fetch_account().await {
                Ok(account) => Some(account),
                Err(error) => {
                    // Profile scopes are optional for playback. Keep the
                    // controller usable if an older grant lacks them.
                    debug!("Spotify account profile unavailable: {}", error);
                    None
                }
            }
        } else {
            None
        };
        {
            let mut state = self.state.write().await;
            state.devices = devices
                .iter()
                .cloned()
                .map(|device| (device.id.clone(), device))
                .collect();
            state.playback = playback.clone();
            if should_fetch_account {
                state.account_loaded = true;
                state.account = account.clone();
            }
        }

        if should_fetch_account {
            self.bus.publish(BusEvent::ProviderAccountUpdated {
                provider: "spotify".to_string(),
                account,
            });
        }

        for device in &devices {
            let zone_id = PrefixedZoneId::spotify(&device.id);
            let device_playback = playback_for_device(playback.as_ref(), &device.id);
            if previous_devices.contains_key(&device.id) {
                let zone = device.to_zone(device_playback);
                self.bus.publish(BusEvent::ZoneUpdated {
                    zone_id: zone_id.clone(),
                    display_name: device.name.clone(),
                    state: zone.state.to_string(),
                });

                let previous_device_playback =
                    playback_for_device(previous_playback.as_ref(), &device.id);
                if track_identity(previous_device_playback) != track_identity(device_playback) {
                    let now_playing = device_playback.and_then(SpotifyPlayback::to_now_playing);
                    self.bus.publish(BusEvent::NowPlayingChanged {
                        zone_id: zone_id.clone(),
                        title: now_playing.as_ref().map(|track| track.title.clone()),
                        artist: now_playing.as_ref().map(|track| track.artist.clone()),
                        album: now_playing.as_ref().map(|track| track.album.clone()),
                        image_key: now_playing.and_then(|track| track.image_key),
                    });
                }
                let previous_modes = previous_device_playback
                    .map(|state| (state.repeat_mode(), state.shuffle_state));
                let current_modes =
                    device_playback.map(|state| (state.repeat_mode(), state.shuffle_state));
                if previous_modes != current_modes {
                    self.bus.publish(BusEvent::PlaybackModesChanged {
                        zone_id: zone_id.clone(),
                        repeat_mode: current_modes.and_then(|modes| modes.0),
                        shuffle: current_modes.and_then(|modes| modes.1),
                    });
                }
                if previous_device_playback.and_then(|state| state.progress_ms)
                    != device_playback.and_then(|state| state.progress_ms)
                {
                    if let Some(position) = device_playback.and_then(|state| state.progress_ms) {
                        self.bus.publish(BusEvent::SeekPositionChanged {
                            zone_id: zone_id.clone(),
                            position: (position / 1000) as i64,
                        });
                    }
                }
                if previous_devices
                    .get(&device.id)
                    .and_then(|previous| previous.volume_percent)
                    != device.volume_percent
                {
                    if let Some(value) = device.volume_percent {
                        self.bus.publish(BusEvent::VolumeChanged {
                            output_id: zone_id.to_string(),
                            value: f32::from(value),
                            is_muted: false,
                        });
                    }
                }
            } else {
                self.bus.publish(BusEvent::ZoneDiscovered {
                    zone: device.to_zone(device_playback),
                });
            }
        }

        for device_id in previous_devices.keys() {
            if !devices.iter().any(|device| &device.id == device_id) {
                self.bus.publish(BusEvent::ZoneRemoved {
                    zone_id: PrefixedZoneId::spotify(device_id),
                });
            }
        }
        Ok(())
    }

    async fn fetch_devices(&self) -> Result<Vec<SpotifyDevice>> {
        let response: DevicesResponse =
            self.request_json(Method::GET, "/me/player/devices").await?;
        Ok(response
            .devices
            .into_iter()
            .filter_map(SpotifyDeviceResponse::into_device)
            .collect())
    }

    async fn fetch_playback(&self) -> Result<Option<SpotifyPlayback>> {
        let response = self.request(Method::GET, "/me/player").await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        self.decode_response(response).await.map(Some)
    }

    async fn fetch_account(&self) -> Result<ProviderAccount> {
        let profile: SpotifyUserProfile = self.request_json(Method::GET, "/me").await?;
        Ok(ProviderAccount {
            id: profile.id,
            display_name: profile.display_name,
            email: profile.email,
        })
    }

    async fn request(&self, method: Method, path: &str) -> Result<reqwest::Response> {
        let access_token = self.ensure_access_token().await?;
        let has_command_body = method == Method::POST || method == Method::PUT;
        let request = self
            .client
            .request(method, format!("{}{}", self.api_base_url, path))
            .bearer_auth(access_token);
        let request = if has_command_body {
            // Spotify's command endpoints require an explicit zero-length
            // body. Without it some proxies return HTTP 411 Length Required.
            request
                .header(reqwest::header::CONTENT_LENGTH, "0")
                .body(String::new())
        } else {
            request
        };
        request
            .send()
            .await
            .map_err(|e| anyhow!("Spotify request {} failed: {}", path, e))
    }

    async fn ensure_access_token(&self) -> Result<String> {
        let token = self
            .state
            .read()
            .await
            .token
            .clone()
            .ok_or_else(|| anyhow!("Spotify access token is not configured"))?;
        if token.access_token.is_empty() {
            return Err(anyhow!("Spotify access token is empty"));
        }
        if !token.is_expired(now_secs()) {
            return Ok(token.access_token);
        }
        let refresher =
            self.refresher.read().await.clone().ok_or_else(|| {
                anyhow!("Spotify access token is expired; authorize Spotify again")
            })?;
        let refreshed = match refresher.refresh(&token).await {
            Ok(token) => token,
            Err(error) => {
                // A failed refresh (especially invalid_grant) must not leave an
                // expired token in memory for every subsequent poll/command.
                self.state.write().await.token = None;
                return Err(error);
            }
        };
        if refreshed.access_token.is_empty() {
            return Err(anyhow!(
                "Spotify token refresh returned an empty access token"
            ));
        }
        let access_token = refreshed.access_token.clone();
        self.state.write().await.token = Some(refreshed);
        Ok(access_token)
    }

    async fn request_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
    ) -> Result<T> {
        let response = self.request(method, path).await?;
        self.decode_response(response).await
    }

    async fn request_json_body<T: Serialize, R: DeserializeOwned>(
        &self,
        method: Method,
        path: &str,
        body: &T,
    ) -> Result<R> {
        let access_token = self.ensure_access_token().await?;
        let response = self
            .client
            .request(method, format!("{}{}", self.api_base_url, path))
            .bearer_auth(access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("Spotify request {} failed: {}", path, e))?;
        self.decode_response(response).await
    }

    async fn decode_response<T: for<'de> Deserialize<'de>>(
        &self,
        response: reqwest::Response,
    ) -> Result<T> {
        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| anyhow!("failed reading Spotify response: {}", e))?;
        if !status.is_success() {
            return Err(spotify_api_error(status, &body, false));
        }
        serde_json::from_str(&body).map_err(|e| anyhow!("invalid Spotify response JSON: {}", e))
    }

    async fn send_command(&self, method: Method, path: &str) -> Result<()> {
        self.send_command_with_context(method, path, true).await
    }

    async fn send_library_command(&self, method: Method, path: &str) -> Result<()> {
        self.send_command_with_context(method, path, false).await
    }

    async fn send_command_with_context(
        &self,
        method: Method,
        path: &str,
        playback_control: bool,
    ) -> Result<()> {
        let response = self.request(method, path).await?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NO_CONTENT {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(spotify_api_error(status, &body, playback_control))
    }

    async fn send_json_command<T: Serialize>(
        &self,
        method: Method,
        path: &str,
        body: &T,
    ) -> Result<()> {
        let access_token = self.ensure_access_token().await?;
        let response = self
            .client
            .request(method, format!("{}{}", self.api_base_url, path))
            .bearer_auth(access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("Spotify request {} failed: {}", path, e))?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NO_CONTENT {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        Err(spotify_api_error(status, &body, false))
    }

    async fn request_json_with_body<T: Serialize, R: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
        body: &T,
    ) -> Result<R> {
        let access_token = self.ensure_access_token().await?;
        let response = self
            .client
            .request(method, format!("{}{}", self.api_base_url, path))
            .bearer_auth(access_token)
            .json(body)
            .send()
            .await
            .map_err(|e| anyhow!("Spotify request {} failed: {}", path, e))?;
        self.decode_response(response).await
    }

    async fn start_internal(&self) -> Result<()> {
        let mut running = self.state.write().await;
        if running.running {
            return Ok(());
        }
        running.running = true;
        drop(running);
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };
        let handle = AdapterHandle::new(self.clone(), self.bus.clone(), shutdown);
        tokio::spawn(async move {
            if let Err(error) = handle.run_with_retry(RetryConfig::default()).await {
                warn!("Spotify adapter stopped: {}", error);
            }
        });
        Ok(())
    }

    fn publish_error(&self, error: &anyhow::Error) {
        self.bus.publish(BusEvent::AdapterError {
            adapter: "spotify".to_string(),
            error: error.to_string(),
        });
    }

    async fn stop_internal(&self) {
        self.shutdown.read().await.cancel();
        self.state.write().await.running = false;
        self.bus.publish(BusEvent::AdapterStopping {
            adapter: "spotify".to_string(),
            reason: Some("requested".to_string()),
        });
    }
}

#[async_trait]
impl AdapterLogic for SpotifyAdapter {
    fn prefix(&self) -> &'static str {
        "spotify"
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        if !self.is_configured().await {
            return Err(anyhow!(
                "Spotify access token is not configured or has expired"
            ));
        }
        if let Err(error) = self.update().await {
            self.publish_error(&error);
            return Err(error);
        }
        self.bus.publish(BusEvent::AdapterConnected {
            adapter: "spotify".to_string(),
            details: Some("Spotify Web API".to_string()),
        });
        let mut ticker = interval(SPOTIFY_POLL_INTERVAL);
        loop {
            tokio::select! {
                _ = ctx.shutdown.cancelled() => break,
                _ = ticker.tick() => {
                    if let Err(error) = self.update().await {
                        self.publish_error(&error);
                        debug!("Spotify polling failed: {}", error);
                    }
                }
            }
        }
        self.bus.publish(BusEvent::AdapterDisconnected {
            adapter: "spotify".to_string(),
            reason: Some("adapter stopped".to_string()),
        });
        Ok(())
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        if !self.is_configured().await {
            return Ok(failure(
                "Spotify access token is not configured or has expired",
            ));
        }
        let device_id = zone_id.strip_prefix("spotify:").unwrap_or(zone_id);
        let device = match self.state.read().await.devices.get(device_id).cloned() {
            Some(device) => device,
            None => return Ok(failure("Spotify device is not currently available")),
        };
        if device.is_restricted {
            return Ok(failure("Spotify device is restricted"));
        }
        let query = format!("?device_id={}", urlencoding::encode(device_id));
        let result = match command {
            AdapterCommand::Play => {
                self.send_command(Method::PUT, &format!("/me/player/play{}", query))
                    .await
            }
            AdapterCommand::Pause => {
                self.send_command(Method::PUT, &format!("/me/player/pause{}", query))
                    .await
            }
            AdapterCommand::PlayPause => {
                let is_playing = {
                    let state = self.state.read().await;
                    state
                        .playback
                        .as_ref()
                        .filter(|playback| {
                            playback
                                .device
                                .as_ref()
                                .and_then(|current| current.id.as_deref())
                                == Some(device_id)
                        })
                        .map(|playback| playback.is_playing)
                        .unwrap_or(false)
                };
                // A toggle is resolved against cached state when available;
                // absent state defaults to play, matching Spotify's command
                // semantics and avoiding an extra request per button press.
                let playing = is_playing;
                let action = if playing { "pause" } else { "play" };
                self.send_command(Method::PUT, &format!("/me/player/{}{}", action, query))
                    .await
            }
            AdapterCommand::Next => {
                self.send_command(Method::POST, &format!("/me/player/next{}", query))
                    .await
            }
            AdapterCommand::Previous => {
                self.send_command(Method::POST, &format!("/me/player/previous{}", query))
                    .await
            }
            AdapterCommand::VolumeAbsolute(value) => {
                if !device.supports_volume {
                    return Ok(failure("Spotify device does not support volume control"));
                }
                let value = value.clamp(0, 100);
                self.send_command(
                    Method::PUT,
                    &format!(
                        "/me/player/volume?volume_percent={value}&device_id={}",
                        urlencoding::encode(device_id)
                    ),
                )
                .await
            }
            AdapterCommand::VolumeRelative(delta) => {
                if !device.supports_volume {
                    return Ok(failure("Spotify device does not support volume control"));
                }
                let current = device
                    .volume_percent
                    .ok_or_else(|| anyhow!("Spotify device volume is unavailable"));
                match current {
                    Ok(current) => {
                        let value = (i32::from(current) + delta).clamp(0, 100);
                        self.send_command(
                            Method::PUT,
                            &format!(
                                "/me/player/volume?volume_percent={value}&device_id={}",
                                urlencoding::encode(device_id)
                            ),
                        )
                        .await
                    }
                    Err(error) => Err(error),
                }
            }
            AdapterCommand::SetRepeat(mode) => {
                let state = match mode {
                    RepeatMode::Off => "off",
                    RepeatMode::One => "track",
                    RepeatMode::All => "context",
                };
                self.send_command(
                    Method::PUT,
                    &format!(
                        "/me/player/repeat?state={state}&device_id={}",
                        urlencoding::encode(device_id)
                    ),
                )
                .await
            }
            AdapterCommand::SetShuffle(enabled) => {
                self.send_command(
                    Method::PUT,
                    &format!(
                        "/me/player/shuffle?state={enabled}&device_id={}",
                        urlencoding::encode(device_id)
                    ),
                )
                .await
            }
            AdapterCommand::Mute(_) => {
                return Ok(failure("Spotify Connect does not expose a mute command"));
            }
            AdapterCommand::Stop => {
                return Ok(failure(
                    "Spotify Connect does not expose a stop command; use pause",
                ));
            }
        };
        match result {
            Ok(()) => {
                // Spotify's command endpoints return before the next normal
                // five-second poll. Refresh once immediately so the bus and
                // all clients see the new track/state without waiting for the
                // polling cadence.
                if let Err(error) = self.update().await {
                    debug!("Spotify command succeeded but refresh failed: {}", error);
                }
                Ok(AdapterCommandResponse {
                    success: true,
                    error: None,
                })
            }
            Err(error) => Ok(failure(&error.to_string())),
        }
    }
}

#[async_trait]
impl LibraryAdapter for SpotifyAdapter {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<LibrarySearchResult>> {
        SpotifyAdapter::search(self, query, limit).await
    }

    async fn play_uri(&self, zone_id: &str, uri: &str) -> Result<String> {
        SpotifyAdapter::play_uri(self, zone_id, uri).await
    }

    async fn queue_uri(&self, zone_id: &str, uri: &str) -> Result<()> {
        self.add_to_queue(zone_id, uri).await
    }

    async fn read_queue(&self, zone_id: &str) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(self.get_queue(zone_id).await?)?)
    }

    async fn content(
        &self,
        operation: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(20)
            .clamp(1, 50);
        let result = match operation {
            "browse_categories" => {
                return Err(development_mode_browse_unavailable("catalog categories"))
            }
            "browse_category_playlists" => {
                return Err(development_mode_browse_unavailable("category playlists"))
            }
            "browse_featured_playlists" => {
                return Err(development_mode_browse_unavailable("featured playlists"))
            }
            "browse_new_releases" => {
                return Err(development_mode_browse_unavailable("new releases"))
            }
            "playlists" => {
                self.request_json(Method::GET, &format!("/me/playlists?limit={limit}"))
                    .await?
            }
            "saved_tracks" => {
                self.request_json(Method::GET, &format!("/me/tracks?limit={limit}"))
                    .await?
            }
            "playlist_tracks" => {
                let id = params
                    .get("playlist_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("playlist_id is required"))?;
                if !valid_spotify_id(id) {
                    return Err(anyhow!("playlist_id is invalid"));
                }
                serde_json::to_value(self.get_playlist_items(id, limit as usize, 0).await?)?
            }
            "create_playlist" => {
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("name is required"))?;
                let body = serde_json::json!({
                    "name": name,
                    "public": params.get("public").and_then(Value::as_bool).unwrap_or(false),
                    "collaborative": false,
                    "description": params.get("description").and_then(Value::as_str).unwrap_or("")
                });
                self.request_json_with_body(Method::POST, "/me/playlists", &body)
                    .await?
            }
            "update_playlist" => {
                let id = params
                    .get("playlist_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("playlist_id is required"))?;
                if !valid_spotify_id(id) {
                    return Err(anyhow!("playlist_id is invalid"));
                }
                let mut body = serde_json::Map::new();
                for key in ["name", "description", "public", "collaborative"] {
                    if let Some(value) = params.get(key) {
                        body.insert(key.to_string(), value.clone());
                    }
                }
                self.send_json_command(
                    Method::PUT,
                    &format!("/playlists/{id}"),
                    &Value::Object(body),
                )
                .await?;
                serde_json::json!({"updated": true, "playlist_id": id})
            }
            "playlist_add" => {
                let id = params
                    .get("playlist_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("playlist_id is required"))?;
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("uri is required"))?;
                if !valid_spotify_id(id) || !uri.starts_with("spotify:track:") {
                    return Err(anyhow!(
                        "playlist_add requires a valid playlist id and spotify:track URI"
                    ));
                }
                serde_json::to_value(
                    self.add_playlist_items(id, &[uri.to_string()], None)
                        .await?,
                )?
            }
            "playlist_replace" => {
                let id = params
                    .get("playlist_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("playlist_id is required"))?;
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("uri is required"))?;
                serde_json::to_value(self.replace_playlist_items(id, &[uri.to_string()]).await?)?
            }
            "playlist_remove" => {
                let id = params
                    .get("playlist_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("playlist_id is required"))?;
                let uri = params
                    .get("uri")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("uri is required"))?;
                serde_json::to_value(
                    self.remove_playlist_items(id, &[uri.to_string()], None)
                        .await?,
                )?
            }
            "saved_tracks_check" => {
                let id = params
                    .get("track_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("track_id is required"))?;
                serde_json::to_value(self.check_saved_tracks(&[id.to_string()]).await?)?
            }
            "saved_tracks_add" => {
                let id = params
                    .get("track_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("track_id is required"))?;
                self.save_tracks(&[id.to_string()]).await?;
                serde_json::json!({"saved": true})
            }
            "saved_tracks_remove" => {
                let id = params
                    .get("track_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("track_id is required"))?;
                self.remove_saved_tracks(&[id.to_string()]).await?;
                serde_json::json!({"saved": false})
            }
            _ => return Err(anyhow!("unknown Spotify content operation: {operation}")),
        };
        Ok(result)
    }
}

fn valid_spotify_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 128 && value.bytes().all(|b| b.is_ascii_alphanumeric())
}

fn failure(message: &str) -> AdapterCommandResponse {
    AdapterCommandResponse {
        success: false,
        error: Some(message.to_string()),
    }
}

crate::impl_startable!(SpotifyAdapter, "spotify", is_configured);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn spotify_page_limit(limit: usize) -> usize {
    limit.clamp(1, 50)
}

fn spotify_search_limit(limit: usize) -> usize {
    limit.clamp(1, 10)
}

fn development_mode_browse_unavailable(surface: &str) -> anyhow::Error {
    anyhow!(
        "Spotify {surface} are unavailable to new Development Mode applications as of the February 2026 Web API changes; use search or the authenticated user's library instead"
    )
}

fn spotify_api_error(status: StatusCode, body: &str, playback_control: bool) -> anyhow::Error {
    if status == StatusCode::TOO_MANY_REQUESTS {
        let reason = serde_json::from_str::<Value>(body)
            .ok()
            .and_then(|json| json["error"]["reason"].as_str().map(str::to_owned));
        if reason.as_deref() == Some("QUOTA_EXCEEDED") {
            return anyhow!(
                "Spotify API quota exhausted (QUOTA_EXCEEDED); retry after the developer quota resets"
            );
        }
        return anyhow!("Spotify API rate limited the request; retry after Retry-After");
    }

    let detail = serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|json| json["error"]["message"].as_str().map(str::to_string))
        .unwrap_or_else(|| body.chars().take(240).collect());
    if playback_control && status == StatusCode::FORBIDDEN {
        return anyhow!("Spotify playback control requires a Premium account: {detail}");
    }
    anyhow!("Spotify API returned HTTP {}: {}", status, detail)
}

fn spotify_ids(ids: &[String]) -> Result<String> {
    if ids.is_empty() {
        return Err(anyhow!("Spotify track id list cannot be empty"));
    }
    if ids.len() > 50 {
        return Err(anyhow!("Spotify accepts at most 50 track ids per request"));
    }
    if ids
        .iter()
        .any(|id| id.trim().is_empty() || id.contains(','))
    {
        return Err(anyhow!(
            "Spotify track ids must be non-empty and cannot contain commas"
        ));
    }
    Ok(urlencoding::encode(&ids.join(",")).into_owned())
}

fn spotify_track_uris(ids: &[String]) -> Result<String> {
    let _ = spotify_ids(ids)?;
    let uris = ids
        .iter()
        .map(|id| format!("spotify:track:{}", id.trim()))
        .collect::<Vec<_>>()
        .join(",");
    Ok(urlencoding::encode(&uris).into_owned())
}

fn spotify_playlist_id(playlist_id: &str) -> Result<String> {
    let playlist_id = playlist_id.trim();
    if playlist_id.is_empty() {
        return Err(anyhow!("Spotify playlist id cannot be empty"));
    }
    Ok(urlencoding::encode(playlist_id).into_owned())
}

fn spotify_content_uris(uris: &[String]) -> Result<Vec<String>> {
    if uris.is_empty() {
        return Err(anyhow!("Spotify playlist URI list cannot be empty"));
    }
    if uris.len() > 100 {
        return Err(anyhow!(
            "Spotify accepts at most 100 playlist items per request"
        ));
    }
    if uris.iter().any(|uri| {
        !(uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:"))
            || uri.ends_with(':')
    }) {
        return Err(anyhow!(
            "Spotify playlist items must be spotify:track: or spotify:episode: URIs"
        ));
    }
    Ok(uris.to_vec())
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

fn playback_for_device<'a>(
    playback: Option<&'a SpotifyPlayback>,
    device_id: &str,
) -> Option<&'a SpotifyPlayback> {
    playback.filter(|current| {
        current
            .device
            .as_ref()
            .and_then(|device| device.id.as_deref())
            == Some(device_id)
    })
}

fn track_identity(
    playback: Option<&SpotifyPlayback>,
) -> Option<(String, String, String, Option<String>)> {
    let track = playback?.item.as_ref()?;
    Some((
        track.name.clone(),
        track
            .artists
            .first()
            .map(|artist| artist.name.clone())
            .unwrap_or_default(),
        track.album.name.clone(),
        track.album.images.first().map(|image| image.url.clone()),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_maps_only_non_secret_identity_fields() {
        let profile: SpotifyUserProfile = serde_json::from_value(serde_json::json!({
            "id": "acct-123",
            "display_name": "Ada Lovelace",
            "email": "ada@example.test",
            "country": "US",
            "product": "premium"
        }))
        .expect("Spotify profile should decode");

        assert_eq!(
            ProviderAccount {
                id: profile.id,
                display_name: profile.display_name,
                email: profile.email,
            },
            ProviderAccount {
                id: "acct-123".to_string(),
                display_name: Some("Ada Lovelace".to_string()),
                email: Some("ada@example.test".to_string()),
            }
        );
    }
}
