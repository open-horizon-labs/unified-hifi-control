//! Bounded, provenance-tagged Apple Music feedback for future adaptation.
//!
//! This records explicit signals only. It never infers dislike from absence
//! of a signal and never stores provider credentials or raw Apple IDs. Records
//! carry a bounded event identity and confidence so future observed events can
//! be distinguished from user intent without changing the retention boundary.

use serde::{Deserialize, Serialize};
use rand::RngCore;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

const MAX_RECORDS: usize = 256;
const MAX_REFERENCE_LENGTH: usize = 512;
const MAX_REASON_LENGTH: usize = 512;
static NEXT_EVENT_ID: AtomicU64 = AtomicU64::new(1);
static BOOT_NONCE: OnceLock<u64> = OnceLock::new();

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSignal {
    Favorite,
    Unfavorite,
    Rating,
    Skip,
    MoreLikeThis,
    LessLikeThis,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackSource {
    User,
    Companion,
    Observed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum FeedbackConfidence {
    #[default]
    Explicit,
    Observed,
    Inferred,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeedbackRecord {
    #[serde(default)]
    pub event_id: String,
    pub zone_id: String,
    pub reference: String,
    pub signal: FeedbackSignal,
    pub source: FeedbackSource,
    pub rating: Option<u8>,
    pub reason: Option<String>,
    pub explicit: bool,
    #[serde(default)]
    pub confidence: FeedbackConfidence,
    pub recorded_at: u64,
}

#[derive(Clone, Default)]
pub struct FeedbackStore {
    records: Arc<RwLock<Vec<FeedbackRecord>>>,
    path: Option<PathBuf>,
}

impl FeedbackStore {
    pub fn from_config() -> Self {
        let path = crate::config::get_config_file_path("apple-feedback.json");
        let records = load_records(&path).unwrap_or_else(|error| {
            tracing::warn!("Unable to load Apple Music feedback: {error}");
            Vec::new()
        });
        Self {
            records: Arc::new(RwLock::new(records)),
            path: Some(path),
        }
    }

    pub async fn record(&self, record: FeedbackRecord) -> anyhow::Result<FeedbackRecord> {
        validate(&record)?;
        let mut records = self.records.write().await;
        if let Some(existing) = records.iter().find(|item| item.event_id == record.event_id) {
            if existing == &record {
                return Ok(existing.clone());
            }
            anyhow::bail!(
                "feedback event_id `{}` already exists with different data",
                record.event_id
            );
        }
        let previous = records.clone();
        records.push(record.clone());
        trim(&mut records);
        if let Some(path) = &self.path {
            if let Err(error) = persist(path, &records) {
                *records = previous;
                return Err(error);
            }
        }
        Ok(record)
    }

    pub async fn recent(&self, zone_id: &str, limit: usize) -> Vec<FeedbackRecord> {
        self.records
            .read()
            .await
            .iter()
            .rev()
            .filter(|record| record.zone_id == zone_id)
            .take(limit.min(50))
            .cloned()
            .collect()
    }

    pub async fn clear_zone(&self, zone_id: &str) -> anyhow::Result<()> {
        let mut records = self.records.write().await;
        let previous = records.clone();
        records.retain(|record| record.zone_id != zone_id);
        if let Some(path) = &self.path {
            if let Err(error) = persist(path, &records) {
                *records = previous;
                return Err(error);
            }
        }
        Ok(())
    }
}

pub fn validate(record: &FeedbackRecord) -> anyhow::Result<()> {
    if !record.zone_id.starts_with("applemusic:") {
        anyhow::bail!("feedback requires an applemusic execution-owner zone");
    }
    if record.reference.is_empty() || record.reference.len() > MAX_REFERENCE_LENGTH {
        anyhow::bail!("feedback reference is empty or oversized");
    }
    if record
        .reason
        .as_ref()
        .is_some_and(|value| value.len() > MAX_REASON_LENGTH)
    {
        anyhow::bail!("feedback reason is oversized");
    }
    if record.signal == FeedbackSignal::Rating && !matches!(record.rating, Some(1..=5)) {
        anyhow::bail!("rating feedback must be between 1 and 5");
    }
    if record.signal != FeedbackSignal::Rating && record.rating.is_some() {
        anyhow::bail!("rating is only valid with rating feedback");
    }
    if !record.explicit {
        anyhow::bail!("feedback must identify an explicit user signal");
    }
    if record.confidence != FeedbackConfidence::Explicit {
        anyhow::bail!("explicit feedback must have explicit confidence");
    }
    Ok(())
}

fn trim(records: &mut Vec<FeedbackRecord>) {
    if records.len() > MAX_RECORDS {
        records.drain(..records.len() - MAX_RECORDS);
    }
}

fn load_records(path: &PathBuf) -> anyhow::Result<Vec<FeedbackRecord>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let mut records: Vec<FeedbackRecord> = serde_json::from_slice(&bytes)?;
    records.retain(|record| validate(record).is_ok());
    trim(&mut records);
    Ok(records)
}

fn persist(path: &PathBuf, records: &[FeedbackRecord]) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("feedback path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, serde_json::to_vec_pretty(records)?)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Mint a process-unique event identity while retaining a human-useful time
/// prefix. The counter prevents same-second feedback from collapsing into one
/// logical event; persisted records remain the durable source of truth.
pub fn next_event_id() -> String {
    let sequence = NEXT_EVENT_ID.fetch_add(1, Ordering::Relaxed);
    let boot_nonce = *BOOT_NONCE.get_or_init(|| rand::thread_rng().next_u64());
    format!("feedback-{}-{}-{}", now_secs(), boot_nonce, sequence)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn record(signal: FeedbackSignal) -> FeedbackRecord {
        FeedbackRecord {
            event_id: "test-event".into(),
            zone_id: "applemusic:iphone".into(),
            reference: "ref".into(),
            signal,
            source: FeedbackSource::User,
            rating: None,
            reason: None,
            explicit: true,
            confidence: FeedbackConfidence::Explicit,
            recorded_at: now_secs(),
        }
    }
    #[tokio::test]
    async fn feedback_is_bounded_and_newest_first() {
        let store = FeedbackStore::default();
        for index in 0..260 {
            let mut item = record(FeedbackSignal::Skip);
            item.event_id = format!("test-event-{index}");
            store.record(item).await.unwrap();
        }
        assert_eq!(store.recent("applemusic:iphone", 500).await.len(), 50);
    }
    #[test]
    fn inferred_feedback_and_invalid_ratings_are_refused() {
        let mut value = record(FeedbackSignal::Rating);
        value.rating = Some(6);
        assert!(validate(&value).is_err());
        value.rating = Some(5);
        value.explicit = false;
        assert!(validate(&value).is_err());
    }

    #[test]
    fn event_ids_are_unique_within_the_same_second() {
        assert_ne!(next_event_id(), next_event_id());
    }

    #[tokio::test]
    async fn duplicate_event_ids_are_idempotent_but_conflicts_are_refused() {
        let store = FeedbackStore::default();
        let first = record(FeedbackSignal::Favorite);
        assert_eq!(store.record(first.clone()).await.unwrap(), first);
        assert_eq!(store.record(first.clone()).await.unwrap(), first);
        assert_eq!(store.recent("applemusic:iphone", 50).await.len(), 1);

        let mut conflict = first;
        conflict.signal = FeedbackSignal::Skip;
        assert!(store.record(conflict).await.is_err());
        assert_eq!(store.recent("applemusic:iphone", 50).await.len(), 1);
    }
}
