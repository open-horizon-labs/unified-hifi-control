//! Native Apple Music adapter boundary.
//!
//! Apple Music playback is controlled by a native MusicKit companion.
//! The companion owns MusicKit authorization and its platform playback
//! session; this module owns the UHC adapter lifecycle and translates the
//! companion's state into the shared zone/event model.  The initial platform
//! target is iPhone `SystemMusicPlayer`; the legacy macOS package remains a
//! separate, unvalidated execution owner until #486 is proven on hardware.
//!
//! The Rust server deliberately does not import MusicKit or automate the
//! Music.app process.  [`MusicKitCompanion`] is the narrow boundary used by an
//! in-process macOS companion today and by a paired companion transport later.
//! See `companion/apple_music/README.md` for the line-oriented wire contract.

use anyhow::{bail, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use crate::adapters::traits::{
    AdapterCommand, AdapterCommandResponse, AdapterContext, AdapterLogic, LibraryAdapter,
    LibrarySearchResult,
};
use crate::bus::{
    BusEvent, NowPlaying, PlaybackState, PrefixedZoneId, VolumeControl, VolumeScale, Zone,
};

/// UHC adapter prefix for Apple Music zones.
pub const APPLE_MUSIC_PREFIX: &str = "applemusic";

/// The ApplicationMusicPlayer session used by the native companion.
pub const APPLICATION_PLAYER_ID: &str = "application";

/// The platform that owns an Apple Music playback session.
///
/// This is deliberately not part of [`MusicKitSnapshot`]'s wire shape yet:
/// adding fields to the paired bridge payload requires the API contract work
/// tracked by #463.  Keeping the model here lets the adapter and tests make
/// the ownership distinction without claiming an unvalidated SDK surface.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionPlatform {
    IPhone,
    Mac,
}

/// Identity of the process/device that owns playback for an `applemusic:` zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecutionOwner {
    pub companion_id: String,
    pub platform: CompanionPlatform,
}

impl ExecutionOwner {
    pub fn new(companion_id: impl Into<String>, platform: CompanionPlatform) -> Result<Self> {
        let companion_id = companion_id.into();
        if companion_id.is_empty() || companion_id.contains(':') {
            bail!("Apple Music companion id must be non-empty and may not contain ':'");
        }
        Ok(Self {
            companion_id,
            platform,
        })
    }

    /// The only controllable zone identity owned by this companion.
    pub fn zone_id(&self) -> PrefixedZoneId {
        PrefixedZoneId::applemusic(&self.companion_id)
    }
}

/// Output route selected by the execution owner.
///
/// A route is observation about where audio is expected to emerge; it is not
/// a UHC zone and therefore cannot receive commands or create a second zone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybackRoute {
    Unknown,
    LocalOutput {
        display_name: String,
    },
    AirPlay {
        route_id: String,
        display_name: String,
    },
}

impl PlaybackRoute {
    pub fn is_destination_only(&self) -> bool {
        matches!(self, Self::AirPlay { .. })
    }
}

/// Playback states emitted by the MusicKit companion.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MusicKitPlaybackState {
    Playing,
    Paused,
    Stopped,
    Interrupted,
    /// The companion has not obtained a MusicKit playback state yet.
    Unknown,
}

impl From<MusicKitPlaybackState> for PlaybackState {
    fn from(state: MusicKitPlaybackState) -> Self {
        match state {
            MusicKitPlaybackState::Playing => Self::Playing,
            MusicKitPlaybackState::Paused => Self::Paused,
            MusicKitPlaybackState::Stopped => Self::Stopped,
            MusicKitPlaybackState::Interrupted => Self::Buffering,
            MusicKitPlaybackState::Unknown => Self::Unknown,
        }
    }
}

/// Track metadata returned by the native MusicKit companion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MusicKitTrack {
    pub title: String,
    pub artist: String,
    pub album: String,
    #[serde(default)]
    pub artwork_url: Option<String>,
    #[serde(default)]
    pub position_seconds: Option<f64>,
    #[serde(default)]
    pub duration_seconds: Option<f64>,
}

/// Snapshot of the ApplicationMusicPlayer session.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MusicKitSnapshot {
    /// Stable companion-local player identifier. `application` is the
    /// canonical ID for ApplicationMusicPlayer on macOS.
    pub player_id: String,
    pub display_name: String,
    pub state: MusicKitPlaybackState,
    #[serde(default)]
    pub track: Option<MusicKitTrack>,
    /// Linear volume in the inclusive range 0.0..=1.0, when available.
    #[serde(default)]
    pub volume: Option<f32>,
    #[serde(default)]
    pub is_muted: bool,
}

/// Commands understood by the native companion.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "command")]
pub enum MusicKitCommand {
    Play,
    Pause,
    Toggle,
    Stop,
    Next,
    Previous,
    SetVolume { value: f32 },
    AdjustVolume { delta: f32 },
    SetMute { muted: bool },
}

/// Request sent over a future in-process/paired companion transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum MusicKitRequest {
    Snapshot,
    Command { command: MusicKitCommand },
}

/// Response envelope for the companion transport.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum MusicKitResponse {
    Snapshot { snapshot: MusicKitSnapshot },
    Ack,
    Error { message: String },
}

/// Narrow Rust/native boundary for MusicKit.
///
/// Implementations may call Swift directly in an in-process macOS build or
/// exchange [`MusicKitRequest`] / [`MusicKitResponse`] over a paired transport.
/// No provider token crosses this boundary: the companion owns authorization.
#[async_trait]
pub trait MusicKitCompanion: Send + Sync {
    async fn snapshot(&self) -> Result<MusicKitSnapshot>;
    async fn execute(&self, command: MusicKitCommand) -> Result<()>;

    /// Return every live execution-owner snapshot known to this companion
    /// transport. Older single-owner companions keep working through the
    /// default implementation.
    async fn snapshots(&self) -> Result<Vec<MusicKitSnapshot>> {
        Ok(vec![self.snapshot().await?])
    }

    /// Execute against the named player/execution owner. The default keeps
    /// the historical single-owner behavior; paired transports override it
    /// to prevent a command crossing companion boundaries.
    async fn execute_for_player(&self, _player_id: &str, command: MusicKitCommand) -> Result<()> {
        self.execute(command).await
    }

    /// Execute an authenticated Apple Music content operation on the native
    /// companion. The companion owns the MusicKit token; UHC receives only the
    /// provider-neutral JSON result. Keeping this optional preserves playback
    /// compatibility with older companions while the content surface rolls
    /// out in a later companion version.
    async fn content(
        &self,
        _operation: &str,
        _params: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        bail!("Apple Music content operations are not implemented by this companion")
    }
}

/// Apple Music adapter backed by a native MusicKit companion.
#[derive(Clone)]
pub struct AppleMusicAdapter {
    companion: Arc<dyn MusicKitCompanion>,
    poll_interval: Duration,
    bus: crate::bus::SharedBus,
    shutdown: Arc<RwLock<CancellationToken>>,
    running: Arc<AtomicBool>,
}

impl AppleMusicAdapter {
    /// Build an adapter with a companion implementation.
    pub fn with_companion(
        bus: crate::bus::SharedBus,
        companion: Arc<dyn MusicKitCompanion>,
        poll_interval: Duration,
    ) -> Self {
        Self {
            companion,
            poll_interval: poll_interval.max(Duration::from_millis(1)),
            bus,
            shutdown: Arc::new(RwLock::new(CancellationToken::new())),
            running: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Convert a companion snapshot into the shared zone representation.
    pub fn zone_from_snapshot(snapshot: &MusicKitSnapshot) -> Result<Zone> {
        if snapshot.player_id.is_empty() || snapshot.player_id.contains(':') {
            bail!("MusicKit companion returned an invalid player id");
        }
        if snapshot.display_name.is_empty() {
            bail!("MusicKit companion returned an empty display name");
        }
        if snapshot
            .volume
            .is_some_and(|value| !(0.0..=1.0).contains(&value))
        {
            bail!("MusicKit companion returned volume outside 0.0..=1.0");
        }

        let zone_id = PrefixedZoneId::applemusic(&snapshot.player_id).to_string();
        let now_playing = snapshot.track.as_ref().map(|track| NowPlaying {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            image_key: track.artwork_url.clone(),
            seek_position: track.position_seconds,
            duration: track.duration_seconds,
            metadata: None,
            repeat_mode: None,
            shuffle: None,
        });
        let volume_control = snapshot.volume.map(|value| VolumeControl {
            value: value * 100.0,
            min: 0.0,
            max: 100.0,
            step: 1.0,
            is_muted: snapshot.is_muted,
            scale: VolumeScale::Percentage,
            output_id: Some(zone_id.clone()),
        });
        let state: PlaybackState = snapshot.state.into();

        Ok(Zone {
            zone_id,
            zone_name: snapshot.display_name.clone(),
            state,
            volume_control,
            now_playing,
            source: APPLE_MUSIC_PREFIX.to_string(),
            is_controllable: true,
            is_seekable: snapshot
                .track
                .as_ref()
                .and_then(|track| track.duration_seconds)
                .is_some(),
            last_updated: now_millis(),
            is_play_allowed: !matches!(state, PlaybackState::Playing),
            is_pause_allowed: matches!(state, PlaybackState::Playing),
            is_next_allowed: true,
            is_previous_allowed: true,
        })
    }

    async fn publish_snapshot(
        &self,
        bus: &crate::bus::SharedBus,
        snapshot: MusicKitSnapshot,
        discovered: &mut std::collections::HashSet<String>,
    ) -> Result<PrefixedZoneId> {
        let zone = Self::zone_from_snapshot(&snapshot)?;
        let zone_id = PrefixedZoneId::applemusic(&snapshot.player_id);

        if discovered.insert(zone_id.to_string()) {
            bus.publish(BusEvent::ZoneDiscovered { zone });
        } else {
            bus.publish(BusEvent::ZoneUpdated {
                zone_id: zone_id.clone(),
                display_name: snapshot.display_name.clone(),
                state: zone.state.to_string(),
            });
            if let Some(track) = snapshot.track {
                bus.publish(BusEvent::NowPlayingChanged {
                    zone_id: zone_id.clone(),
                    title: Some(track.title),
                    artist: Some(track.artist),
                    album: Some(track.album),
                    image_key: track.artwork_url,
                });
                if let Some(position) = track.position_seconds {
                    bus.publish(BusEvent::SeekPositionChanged {
                        zone_id: zone_id.clone(),
                        position: position as i64,
                    });
                }
            }
            if let Some(volume) = snapshot.volume {
                bus.publish(BusEvent::VolumeChanged {
                    output_id: PrefixedZoneId::applemusic(&snapshot.player_id).to_string(),
                    value: volume * 100.0,
                    is_muted: snapshot.is_muted,
                });
            }
        }
        Ok(zone_id)
    }

    fn command_for(command: AdapterCommand) -> Result<MusicKitCommand> {
        match command {
            AdapterCommand::Play => Ok(MusicKitCommand::Play),
            AdapterCommand::Pause => Ok(MusicKitCommand::Pause),
            AdapterCommand::PlayPause => Ok(MusicKitCommand::Toggle),
            AdapterCommand::Stop => Ok(MusicKitCommand::Stop),
            AdapterCommand::Next => Ok(MusicKitCommand::Next),
            AdapterCommand::Previous => Ok(MusicKitCommand::Previous),
            AdapterCommand::VolumeAbsolute(value) => {
                if !(0..=100).contains(&value) {
                    bail!("Apple Music volume must be between 0 and 100");
                }
                Ok(MusicKitCommand::SetVolume {
                    value: value as f32 / 100.0,
                })
            }
            AdapterCommand::VolumeRelative(value) => {
                if !(-100..=100).contains(&value) {
                    bail!("Apple Music relative volume must be between -100 and 100");
                }
                Ok(MusicKitCommand::AdjustVolume {
                    delta: value as f32 / 100.0,
                })
            }
            AdapterCommand::Mute(muted) => Ok(MusicKitCommand::SetMute { muted }),
            AdapterCommand::SetRepeat(_) | AdapterCommand::SetShuffle(_) => {
                bail!("Apple Music repeat and shuffle are not implemented by the adapter")
            }
        }
    }

    async fn start_internal(&self) -> Result<()> {
        if self.running.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let shutdown = {
            let mut token = self.shutdown.write().await;
            *token = CancellationToken::new();
            token.clone()
        };
        let handle =
            crate::adapters::handle::AdapterHandle::new(self.clone(), self.bus.clone(), shutdown);
        tokio::spawn(async move {
            if let Err(error) = handle
                .run_with_retry(crate::adapters::handle::RetryConfig::default())
                .await
            {
                tracing::warn!("Apple Music adapter stopped: {}", error);
            }
        });
        Ok(())
    }

    async fn stop_internal(&self) {
        self.shutdown.read().await.cancel();
        self.running.store(false, Ordering::SeqCst);
        self.bus.publish(BusEvent::AdapterStopping {
            adapter: APPLE_MUSIC_PREFIX.to_string(),
            reason: Some("requested".to_string()),
        });
    }
}

#[async_trait]
impl AdapterLogic for AppleMusicAdapter {
    fn prefix(&self) -> &'static str {
        APPLE_MUSIC_PREFIX
    }

    async fn run(&self, ctx: AdapterContext) -> Result<()> {
        let mut ticker = interval(self.poll_interval);
        let mut discovered = std::collections::HashSet::new();
        loop {
            tokio::select! {
                _ = ctx.shutdown.cancelled() => return Ok(()),
                _ = ticker.tick() => {
                    match self.companion.snapshots().await {
                        Ok(snapshots) => {
                            let mut seen = std::collections::HashSet::new();
                            for snapshot in snapshots {
                                match self.publish_snapshot(&ctx.bus, snapshot, &mut seen).await {
                                    Ok(_) => {}
                                    Err(error) => tracing::debug!("invalid Apple Music companion snapshot: {error}"),
                                }
                            }
                            for zone_id in discovered.difference(&seen) {
                                ctx.bus.publish(BusEvent::ZoneRemoved {
                                    zone_id: PrefixedZoneId::parse(zone_id)
                                        .expect("discovered Apple Music zone is prefixed"),
                                });
                            }
                            discovered = seen;
                        }
                        Err(error) => {
                            for zone_id in &discovered {
                                ctx.bus.publish(BusEvent::ZoneRemoved {
                                    zone_id: PrefixedZoneId::parse(zone_id)
                                        .expect("discovered Apple Music zone is prefixed"),
                                });
                            }
                            discovered.clear();
                            tracing::debug!("Apple Music companion unavailable: {error}");
                        }
                    }
                },
            }
        }
    }

    async fn handle_command(
        &self,
        zone_id: &str,
        command: AdapterCommand,
    ) -> Result<AdapterCommandResponse> {
        if !crate::bus::is_applemusic_zone_id(zone_id) {
            return Ok(AdapterCommandResponse {
                success: false,
                error: Some(format!(
                    "zone `{zone_id}` is not owned by the applemusic adapter"
                )),
            });
        }

        let player_id = zone_id
            .strip_prefix("applemusic:")
            .ok_or_else(|| anyhow::anyhow!("invalid Apple Music zone id `{zone_id}`"))?;
        if matches!(
            &command,
            AdapterCommand::VolumeAbsolute(_)
                | AdapterCommand::VolumeRelative(_)
                | AdapterCommand::Mute(_)
        ) {
            let has_volume = self
                .companion
                .snapshots()
                .await?
                .into_iter()
                .find(|snapshot| snapshot.player_id == player_id)
                .and_then(|snapshot| snapshot.volume)
                .is_some();
            if !has_volume {
                return Ok(AdapterCommandResponse {
                    success: false,
                    error: Some(
                        "Apple Music volume/mute is unavailable until the companion publishes a validated volume control"
                            .to_string(),
                    ),
                });
            }
        }

        let command = Self::command_for(command)?;
        self.companion
            .execute_for_player(player_id, command)
            .await?;
        Ok(AdapterCommandResponse {
            success: true,
            error: None,
        })
    }
}

#[async_trait]
impl LibraryAdapter for AppleMusicAdapter {
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<LibrarySearchResult>> {
        let value = self
            .companion
            .content(
                "search",
                &serde_json::json!({"query": query, "limit": limit.clamp(1, 50)}),
            )
            .await?;
        serde_json::from_value(value).map_err(Into::into)
    }

    async fn play_uri(&self, zone_id: &str, uri: &str) -> Result<String> {
        self.validate_content_zone(zone_id)?;
        let value = self
            .companion
            .content("play_uri", &serde_json::json!({"uri": uri}))
            .await?;
        content_message(value, "Apple Music item started")
    }

    async fn queue_uri(&self, zone_id: &str, uri: &str) -> Result<()> {
        self.validate_content_zone(zone_id)?;
        self.companion
            .content("queue_uri", &serde_json::json!({"uri": uri}))
            .await
            .map(|_| ())
    }

    async fn read_queue(&self, zone_id: &str) -> Result<serde_json::Value> {
        self.validate_content_zone(zone_id)?;
        self.companion
            .content("queue_read", &serde_json::json!({}))
            .await
    }

    async fn content(
        &self,
        operation: &str,
        params: &serde_json::Value,
    ) -> Result<serde_json::Value> {
        validate_content_request(operation, params)?;
        self.companion.content(operation, params).await
    }
}

impl AppleMusicAdapter {
    fn validate_content_zone(&self, zone_id: &str) -> Result<()> {
        if !crate::bus::is_applemusic_zone_id(zone_id) {
            bail!("zone `{zone_id}` is not owned by the applemusic adapter");
        }
        Ok(())
    }
}

fn validate_content_request(operation: &str, params: &serde_json::Value) -> Result<()> {
    // Catalog search is provider-global. Every account, queue, playlist, and
    // playback operation must name the execution owner so a future multi-
    // companion bridge cannot silently route content to the wrong player.
    if matches!(operation, "search" | "catalog_search") {
        return Ok(());
    }
    let zone_id = params
        .get("zone_id")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("Apple Music content operation requires zone_id"))?;
    if !crate::bus::is_applemusic_zone_id(zone_id) {
        bail!("zone `{zone_id}` is not an Apple Music execution-owner zone");
    }
    Ok(())
}

fn content_message(value: serde_json::Value, default: &str) -> Result<String> {
    match value {
        serde_json::Value::String(message) => Ok(message),
        serde_json::Value::Object(map) => Ok(map
            .get("message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(default)
            .to_string()),
        _ => Ok(default.to_string()),
    }
}

crate::impl_startable!(AppleMusicAdapter, "applemusic");

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_and_response_are_stable_json() {
        let request = MusicKitRequest::Command {
            command: MusicKitCommand::SetVolume { value: 0.5 },
        };
        assert_eq!(
            serde_json::to_string(&request).expect("request serializes"),
            r#"{"type":"command","command":{"command":"set_volume","value":0.5}}"#
        );

        let response = MusicKitResponse::Ack;
        assert_eq!(
            serde_json::to_string(&response).expect("response serializes"),
            r#"{"type":"ack"}"#
        );
    }

    #[test]
    fn volume_is_rejected_when_companion_reports_an_invalid_range() {
        let mut snapshot = MusicKitSnapshot {
            player_id: APPLICATION_PLAYER_ID.to_string(),
            display_name: "Apple Music".to_string(),
            state: MusicKitPlaybackState::Paused,
            track: None,
            volume: Some(1.5),
            is_muted: false,
        };
        assert!(AppleMusicAdapter::zone_from_snapshot(&snapshot).is_err());
        snapshot.volume = Some(0.5);
        assert!(AppleMusicAdapter::zone_from_snapshot(&snapshot).is_ok());
    }

    #[test]
    fn player_id_cannot_smuggle_a_second_zone_prefix() {
        let snapshot = MusicKitSnapshot {
            player_id: "application:other".to_string(),
            display_name: "Apple Music".to_string(),
            state: MusicKitPlaybackState::Paused,
            track: None,
            volume: None,
            is_muted: false,
        };
        assert!(AppleMusicAdapter::zone_from_snapshot(&snapshot).is_err());
    }

    #[test]
    fn content_operations_require_an_apple_execution_owner() {
        assert!(validate_content_request("catalog_search", &serde_json::json!({})).is_ok());
        assert!(validate_content_request("search", &serde_json::json!({})).is_ok());
        assert!(validate_content_request("playlist_add", &serde_json::json!({})).is_err());
        assert!(validate_content_request(
            "playlist_add",
            &serde_json::json!({"zone_id": "spotify:device"})
        )
        .is_err());
        assert!(validate_content_request(
            "playlist_add",
            &serde_json::json!({"zone_id": "applemusic:iphone"})
        )
        .is_ok());
    }
}
