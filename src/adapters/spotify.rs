//! Spotify Web API controller adapter.
//!
//! This adapter controls existing Spotify Connect devices. It deliberately
//! does not implement a Spotify Connect receiver: each available Connect
//! device is exposed as its own `spotify:<device-id>` UHC zone.
//!
//! Authorization is intentionally supplied by the surrounding application.
//! The adapter accepts an access token and refuses commands when the token is
//! absent or expired. OAuth/browser pairing belongs to the shared provider
//! authorization layer (issue #463), not to this provider transport.

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use reqwest::{Client, Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};

use crate::adapters::handle::{AdapterHandle, RetryConfig};
use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic,
};
use crate::bus::{
    BusEvent, NowPlaying, PlaybackState, SharedBus, VolumeControl, VolumeScale, Zone,
};

const SPOTIFY_API_URL: &str = "https://api.spotify.com/v1";
const SPOTIFY_POLL_INTERVAL: Duration = Duration::from_secs(5);

/// OAuth credentials needed to call the Spotify Web API.
///
/// `expires_at` is a Unix timestamp in seconds. The token itself is not
/// refreshed here: refresh-token exchange needs the client credentials and
/// redirect policy owned by the shared authorization layer (#463).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SpotifyToken {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: Option<String>,
    #[serde(default)]
    pub expires_at: Option<u64>,
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
    pub volume_percent: Option<u8>,
}

impl SpotifyDevice {
    /// Convert a Connect device into a UHC zone.
    ///
    /// Playback metadata is only associated with Spotify's currently active
    /// device; Spotify does not expose independent now-playing state for every
    /// available device in one response.
    pub fn to_zone(&self, playback: Option<&SpotifyPlayback>) -> Zone {
        // Keep the prefix construction local to this adapter. The central
        // `PrefixedZoneId` vocabulary is extended by the shared routing work;
        // this module remains independently testable until that lands.
        let zone_id = format!("spotify:{}", self.id);
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
            volume_control: self.volume_percent.map(|value| VolumeControl {
                value: f32::from(value),
                min: 0.0,
                max: 100.0,
                step: 1.0,
                is_muted: false,
                scale: VolumeScale::Percentage,
                output_id: Some(zone_id),
            }),
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
}

impl SpotifyPlayback {
    fn to_now_playing(&self) -> Option<NowPlaying> {
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
        })
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
    pub artists: Vec<SpotifyArtist>,
    pub album: SpotifyAlbum,
    #[serde(default)]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpotifyArtist {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpotifyAlbum {
    pub name: String,
    #[serde(default)]
    pub images: Vec<SpotifyImage>,
}

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct SpotifyImage {
    pub url: String,
}

#[derive(Debug, Clone, Deserialize)]
struct DevicesResponse {
    #[serde(default)]
    devices: Vec<SpotifyDevice>,
}

#[derive(Debug, Default)]
struct SpotifyState {
    token: Option<SpotifyToken>,
    devices: HashMap<String, SpotifyDevice>,
    playback: Option<SpotifyPlayback>,
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
        }
    }

    /// Install or replace the token supplied by the authorization layer.
    pub async fn set_token(&self, token: SpotifyToken) {
        self.state.write().await.token = Some(token);
    }

    /// Remove credentials and cached provider state.
    pub async fn clear_token(&self) {
        let mut state = self.state.write().await;
        state.token = None;
        state.devices.clear();
        state.playback = None;
    }

    /// Return whether an unexpired access token is available.
    pub async fn is_configured(&self) -> bool {
        let state = self.state.read().await;
        state
            .token
            .as_ref()
            .map(|token| !token.access_token.is_empty() && !token.is_expired(now_secs()))
            .unwrap_or(false)
    }

    /// Return the last device inventory fetched from Spotify.
    pub async fn get_devices(&self) -> Vec<SpotifyDevice> {
        self.state.read().await.devices.values().cloned().collect()
    }

    /// Poll device inventory and current playback, publishing zones.
    pub async fn update(&self) -> Result<()> {
        let devices = self.fetch_devices().await?;
        let playback = self.fetch_playback().await?;
        {
            let mut state = self.state.write().await;
            state.devices = devices
                .iter()
                .cloned()
                .map(|device| (device.id.clone(), device))
                .collect();
            state.playback = playback.clone();
        }

        for device in devices {
            let device_playback = playback.as_ref().and_then(|current| {
                current
                    .device
                    .as_ref()
                    .and_then(|current_device| current_device.id.as_ref())
                    .filter(|id| *id == &device.id)
                    .map(|_| current)
            });
            self.bus.publish(BusEvent::ZoneDiscovered {
                zone: device.to_zone(device_playback),
            });
        }
        Ok(())
    }

    async fn fetch_devices(&self) -> Result<Vec<SpotifyDevice>> {
        let response: DevicesResponse =
            self.request_json(Method::GET, "/me/player/devices").await?;
        Ok(response.devices)
    }

    async fn fetch_playback(&self) -> Result<Option<SpotifyPlayback>> {
        let response = self.request(Method::GET, "/me/player").await?;
        if response.status() == StatusCode::NO_CONTENT {
            return Ok(None);
        }
        self.decode_response(response).await.map(Some)
    }

    async fn request(&self, method: Method, path: &str) -> Result<reqwest::Response> {
        let token = {
            let state = self.state.read().await;
            state.token.clone()
        }
        .ok_or_else(|| anyhow!("Spotify access token is not configured"))?;
        if token.access_token.is_empty() {
            return Err(anyhow!("Spotify access token is empty"));
        }
        if token.is_expired(now_secs()) {
            return Err(anyhow!(
                "Spotify access token is expired; authorize Spotify again"
            ));
        }
        self.client
            .request(method, format!("{}{}", self.api_base_url, path))
            .bearer_auth(token.access_token)
            .send()
            .await
            .map_err(|e| anyhow!("Spotify request {} failed: {}", path, e))
    }

    async fn request_json<T: for<'de> Deserialize<'de>>(
        &self,
        method: Method,
        path: &str,
    ) -> Result<T> {
        let response = self.request(method, path).await?;
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
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|json| json["error"]["message"].as_str().map(str::to_string))
                .unwrap_or_else(|| body.chars().take(240).collect());
            return Err(anyhow!("Spotify API returned HTTP {}: {}", status, detail));
        }
        serde_json::from_str(&body).map_err(|e| anyhow!("invalid Spotify response JSON: {}", e))
    }

    async fn send_command(&self, method: Method, path: &str) -> Result<()> {
        let response = self.request(method, path).await?;
        let status = response.status();
        if status.is_success() || status == StatusCode::NO_CONTENT {
            return Ok(());
        }
        let body = response.text().await.unwrap_or_default();
        let detail = serde_json::from_str::<Value>(&body)
            .ok()
            .and_then(|json| json["error"]["message"].as_str().map(str::to_string))
            .unwrap_or_else(|| body.chars().take(240).collect());
        Err(anyhow!("Spotify API returned HTTP {}: {}", status, detail))
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

    async fn stop_internal(&self) {
        self.shutdown.read().await.cancel();
        self.state.write().await.running = false;
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
        self.update().await?;
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
            AdapterCommand::Mute(_) => {
                return Ok(failure("Spotify Connect does not expose a mute command"));
            }
            AdapterCommand::Stop => {
                return Ok(failure(
                    "Spotify Connect does not expose a stop command; use pause",
                ));
            }
        };
        Ok(match result {
            Ok(()) => AdapterCommandResponse {
                success: true,
                error: None,
            },
            Err(error) => failure(&error.to_string()),
        })
    }
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

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}
