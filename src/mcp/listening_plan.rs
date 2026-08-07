//! UHC-owned listening plans for model- or user-directed curation.
//!
//! Apple Music's system queue is only partially observable. This store keeps
//! the sequence UHC requested separate from the provider's observed current
//! item, so MCP never presents a plan as a complete native queue.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListeningPlanItem {
    pub reference: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListeningPlan {
    pub zone_id: String,
    pub items: Vec<ListeningPlanItem>,
    pub current_index: Option<usize>,
    pub generation: u64,
    pub updated_at: u64,
}

#[derive(Clone, Default)]
pub struct ListeningPlanStore {
    plans: Arc<RwLock<HashMap<String, ListeningPlan>>>,
    path: Option<PathBuf>,
}

impl ListeningPlanStore {
    /// Production store backed by UHC's persistent data directory.
    pub fn from_config() -> Self {
        let path = crate::config::get_config_file_path("apple-listening-plans.json");
        let plans = load_plans(&path).unwrap_or_else(|error| {
            tracing::warn!("Unable to load Apple Music listening plans: {error}");
            HashMap::new()
        });
        Self {
            plans: Arc::new(RwLock::new(plans)),
            path: Some(path),
        }
    }

    pub async fn replace(
        &self,
        zone_id: &str,
        items: Vec<ListeningPlanItem>,
    ) -> anyhow::Result<ListeningPlan> {
        if zone_id.is_empty() || !zone_id.starts_with("applemusic:") {
            anyhow::bail!("listening plan zone must be an applemusic execution-owner zone");
        }
        if items.len() > MAX_ITEMS {
            anyhow::bail!("listening plan is limited to {MAX_ITEMS} items");
        }
        if items.iter().any(|item| {
            item.reference.is_empty()
                || item.reference.len() > MAX_REFERENCE_LENGTH
                || item.title.len() > MAX_TITLE_LENGTH
        }) {
            anyhow::bail!("listening plan contains an invalid or oversized item");
        }
        let mut plans = self.plans.write().await;
        let previous = plans.get(zone_id).cloned();
        let generation = plans
            .get(zone_id)
            .map(|plan| plan.generation.saturating_add(1))
            .unwrap_or(1);
        let plan = ListeningPlan {
            zone_id: zone_id.to_string(),
            items,
            current_index: None,
            generation,
            updated_at: now_secs(),
        };
        plans.insert(zone_id.to_string(), plan.clone());
        trim_plans(&mut plans);
        if let Some(path) = &self.path {
            if let Err(error) = persist_plans(path, &plans) {
                if let Some(previous) = previous {
                    plans.insert(zone_id.to_string(), previous);
                } else {
                    plans.remove(zone_id);
                }
                return Err(error);
            }
        }
        Ok(plan)
    }

    pub async fn get(&self, zone_id: &str) -> Option<ListeningPlan> {
        self.plans.read().await.get(zone_id).cloned()
    }
}

const MAX_PLANS: usize = 32;
const MAX_ITEMS: usize = 200;
const MAX_REFERENCE_LENGTH: usize = 512;
const MAX_TITLE_LENGTH: usize = 512;

fn trim_plans(plans: &mut HashMap<String, ListeningPlan>) {
    while plans.len() > MAX_PLANS {
        let Some(oldest) = plans
            .iter()
            .min_by_key(|(_, plan)| plan.updated_at)
            .map(|(zone_id, _)| zone_id.clone())
        else {
            break;
        };
        plans.remove(&oldest);
    }
}

fn load_plans(path: &PathBuf) -> anyhow::Result<HashMap<String, ListeningPlan>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(error) => return Err(error.into()),
    };
    let persisted: Vec<ListeningPlan> = serde_json::from_slice(&bytes)?;
    let mut plans = persisted
        .into_iter()
        .filter(|plan| {
            plan.zone_id.starts_with("applemusic:")
                && plan.items.len() <= MAX_ITEMS
                && plan.items.iter().all(|item| {
                    !item.reference.is_empty()
                        && item.reference.len() <= MAX_REFERENCE_LENGTH
                        && item.title.len() <= MAX_TITLE_LENGTH
                })
        })
        .map(|plan| (plan.zone_id.clone(), plan))
        .collect();
    trim_plans(&mut plans);
    Ok(plans)
}

fn persist_plans(path: &PathBuf, plans: &HashMap<String, ListeningPlan>) -> anyhow::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("listening plan path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let mut values: Vec<_> = plans.values().cloned().collect();
    values.sort_by(|left, right| left.zone_id.cmp(&right.zone_id));
    let bytes = serde_json::to_vec_pretty(&values)?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, bytes)?;
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replacing_a_plan_increments_generation_and_keeps_observation_separate() {
        let store = ListeningPlanStore::default();
        let first = store
            .replace(
                "applemusic:iphone",
                vec![ListeningPlanItem {
                    reference: "ref_one".to_string(),
                    title: "One".to_string(),
                }],
            )
            .await
            .unwrap();
        let second = store
            .replace(
                "applemusic:iphone",
                vec![ListeningPlanItem {
                    reference: "ref_two".to_string(),
                    title: "Two".to_string(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
        assert_eq!(second.current_index, None);
        assert_eq!(store.get("applemusic:iphone").await, Some(second));
    }

    #[tokio::test]
    async fn a_persisted_plan_survives_store_reconstruction() {
        let path = std::env::temp_dir().join(format!(
            "uhc-listening-plan-{}-{}.json",
            std::process::id(),
            now_secs()
        ));
        let store = ListeningPlanStore {
            plans: Arc::new(RwLock::new(HashMap::new())),
            path: Some(path.clone()),
        };
        let expected = store
            .replace(
                "applemusic:iphone",
                vec![ListeningPlanItem {
                    reference: "ref_one".to_string(),
                    title: "One".to_string(),
                }],
            )
            .await
            .unwrap();
        let restarted = ListeningPlanStore {
            plans: Arc::new(RwLock::new(load_plans(&path).unwrap())),
            path: Some(path.clone()),
        };
        assert_eq!(restarted.get("applemusic:iphone").await, Some(expected));
        let _ = std::fs::remove_file(path);
    }
}
