//! UHC-owned listening plans for model- or user-directed curation.
//!
//! Apple Music's system queue is only partially observable. This store keeps
//! the sequence UHC requested separate from the provider's observed current
//! item, so MCP never presents a plan as a complete native queue.

use std::collections::HashMap;
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
}

impl ListeningPlanStore {
    pub async fn replace(&self, zone_id: &str, items: Vec<ListeningPlanItem>) -> ListeningPlan {
        let mut plans = self.plans.write().await;
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
        plan
    }

    pub async fn get(&self, zone_id: &str) -> Option<ListeningPlan> {
        self.plans.read().await.get(zone_id).cloned()
    }
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
            .await;
        let second = store
            .replace(
                "applemusic:iphone",
                vec![ListeningPlanItem {
                    reference: "ref_two".to_string(),
                    title: "Two".to_string(),
                }],
            )
            .await;
        assert_eq!(first.generation, 1);
        assert_eq!(second.generation, 2);
        assert_eq!(second.current_index, None);
        assert_eq!(store.get("applemusic:iphone").await, Some(second));
    }
}
