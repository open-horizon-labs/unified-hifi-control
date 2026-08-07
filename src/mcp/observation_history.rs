//! Bounded, metadata-only playback observations owned by the aggregator.
//!
//! Apple Music's current bridge snapshot does not carry a trusted opaque
//! catalog reference. This store therefore records what the aggregator saw,
//! but never mints an identity from title/artist/album text. It is deliberately
//! separate from [`crate::mcp::listening_plan::ListeningPlan`], which records
//! UHC intent rather than provider observations.

use crate::bus::{NowPlaying, PlaybackState, Zone};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

const MAX_ZONES: usize = 32;
const MAX_RECORDS_PER_ZONE: usize = 64;
const MAX_TEXT_LENGTH: usize = 512;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PlaybackObservation {
    pub zone_id: String,
    pub state: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub image_key: Option<String>,
    /// None until a trusted provider reference arrives through the approved
    /// Apple content bridge. Text is never hashed or promoted to an identity.
    pub reference: Option<String>,
    pub source: String,
    pub confidence: String,
    pub observed_at: u64,
}

#[derive(Clone, Default)]
pub struct PlaybackObservationHistory {
    records: Arc<RwLock<HashMap<String, Vec<PlaybackObservation>>>>,
}

impl PlaybackObservationHistory {
    pub fn from_config() -> Self {
        // Raw listening metadata is intentionally process-scoped until an
        // explicit retention/clear contract exists. Restarting UHC drops it.
        Self::default()
    }

    pub fn new_for_test() -> Self {
        Self::default()
    }

    pub async fn record_zone(&self, zone: &Zone, observed_at: u64) {
        if !zone.zone_id.starts_with("applemusic:") {
            return;
        }
        self.record(
            &zone.zone_id,
            zone.state,
            zone.now_playing.as_ref(),
            observed_at,
        )
        .await;
    }

    pub async fn record_state(&self, zone_id: &str, state: PlaybackState, observed_at: u64) {
        if !zone_id.starts_with("applemusic:") {
            return;
        }
        self.record(zone_id, state, None, observed_at).await;
    }

    pub async fn record_now_playing(
        &self,
        zone_id: &str,
        state: PlaybackState,
        now_playing: Option<&NowPlaying>,
        observed_at: u64,
    ) {
        if !zone_id.starts_with("applemusic:") {
            return;
        }
        self.record(zone_id, state, now_playing, observed_at).await;
    }

    pub async fn recent(&self, zone_id: &str, limit: usize) -> Vec<PlaybackObservation> {
        self.records
            .read()
            .await
            .get(zone_id)
            .into_iter()
            .flat_map(|records| records.iter().rev())
            .take(limit.min(MAX_RECORDS_PER_ZONE))
            .cloned()
            .collect()
    }

    async fn record(
        &self,
        zone_id: &str,
        state: PlaybackState,
        now_playing: Option<&NowPlaying>,
        observed_at: u64,
    ) {
        let observation = PlaybackObservation {
            zone_id: zone_id.to_string(),
            state: state.to_string(),
            title: now_playing.and_then(|item| bounded(item.title.as_str())),
            artist: now_playing.and_then(|item| bounded(item.artist.as_str())),
            album: now_playing.and_then(|item| bounded(item.album.as_str())),
            image_key: now_playing.and_then(|item| item.image_key.as_deref().and_then(bounded)),
            reference: None,
            source: "aggregator".to_string(),
            confidence: "observed_unresolved".to_string(),
            observed_at,
        };
        let mut records = self.records.write().await;
        let per_zone = records.entry(zone_id.to_string()).or_default();
        if per_zone
            .last()
            .is_some_and(|previous| same_snapshot(previous, &observation))
        {
            return;
        }
        per_zone.push(observation);
        if per_zone.len() > MAX_RECORDS_PER_ZONE {
            per_zone.drain(0..per_zone.len() - MAX_RECORDS_PER_ZONE);
        }
        while records.len() > MAX_ZONES {
            let Some(oldest) = records
                .iter()
                .min_by_key(|(_, values)| values.last().map(|item| item.observed_at).unwrap_or(0))
                .map(|(zone_id, _)| zone_id.clone())
            else {
                break;
            };
            records.remove(&oldest);
        }
    }
}

fn same_snapshot(left: &PlaybackObservation, right: &PlaybackObservation) -> bool {
    left.state == right.state
        && left.title == right.title
        && left.artist == right.artist
        && left.album == right.album
        && left.image_key == right.image_key
        && left.reference == right.reference
}

fn bounded(value: &str) -> Option<String> {
    (!value.is_empty() && value.len() <= MAX_TEXT_LENGTH).then(|| value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{PlaybackState, Zone};

    fn zone(id: &str, title: &str) -> Zone {
        Zone {
            zone_id: id.to_string(),
            zone_name: "Test".to_string(),
            state: PlaybackState::Playing,
            now_playing: Some(NowPlaying {
                title: title.to_string(),
                artist: "Artist".to_string(),
                album: "Album".to_string(),
                image_key: Some("art".to_string()),
                seek_position: None,
                duration: None,
                metadata: None,
                repeat_mode: None,
                shuffle: None,
            }),
            volume_control: None,
            source: "applemusic".to_string(),
            is_controllable: true,
            is_seekable: false,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }

    #[tokio::test]
    async fn observations_are_metadata_only_and_consecutive_duplicates_are_deduped() {
        let store = PlaybackObservationHistory::new_for_test();
        store
            .record_zone(&zone("applemusic:iphone", "One"), 1)
            .await;
        store
            .record_zone(&zone("applemusic:iphone", "One"), 2)
            .await;
        store
            .record_zone(&zone("applemusic:iphone", "Two"), 3)
            .await;
        let recent = store.recent("applemusic:iphone", 10).await;
        assert_eq!(recent.len(), 2);
        assert_eq!(recent[0].title.as_deref(), Some("Two"));
        assert_eq!(recent[0].reference, None);
        assert_eq!(recent[0].confidence, "observed_unresolved");
    }

    #[tokio::test]
    async fn observations_are_isolated_to_apple_execution_owners() {
        let store = PlaybackObservationHistory::new_for_test();
        store
            .record_zone(&zone("spotify:device", "Ignored"), 1)
            .await;
        store
            .record_state("applemusic:iphone", PlaybackState::Paused, 2)
            .await;
        assert!(store.recent("spotify:device", 10).await.is_empty());
        assert_eq!(store.recent("applemusic:iphone", 10).await.len(), 1);
    }

    #[tokio::test]
    async fn observation_history_is_bounded_per_owner() {
        let store = PlaybackObservationHistory::new_for_test();
        for index in 0..(MAX_RECORDS_PER_ZONE + 5) {
            store
                .record_zone(
                    &zone("applemusic:iphone", &format!("Track {index}")),
                    index as u64,
                )
                .await;
        }
        let recent = store.recent("applemusic:iphone", usize::MAX).await;
        assert_eq!(recent.len(), MAX_RECORDS_PER_ZONE);
        assert_eq!(recent.first().unwrap().title.as_deref(), Some("Track 68"));
    }
}
