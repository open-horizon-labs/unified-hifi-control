//! Bounded, provenance-tagged Apple Music feedback for future adaptation.
//!
//! This records explicit signals only. It never infers dislike from absence
//! of a signal and never stores provider credentials or raw Apple IDs. Records
//! carry a bounded event identity and confidence so future observed events can
//! be distinguished from user intent without changing the retention boundary.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

const MAX_RECORDS: usize = 256;
const MAX_REFERENCE_LENGTH: usize = 512;
const MAX_REASON_LENGTH: usize = 512;

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
        records.retain(|record| record.zone_id != zone_id);
        if let Some(path) = &self.path {
            persist(path, &records)?;
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
        for _ in 0..260 {
            store.record(record(FeedbackSignal::Skip)).await.unwrap();
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
}
