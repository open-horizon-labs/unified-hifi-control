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
    /// Bounded UHC-owned intent history. These revisions describe what UHC
    /// requested, not what Music.app confirmed; observed playback remains a
    /// separate aggregator fact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<ListeningPlanRevision>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ListeningPlanRevision {
    pub generation: u64,
    pub operation: String,
    /// Opaque refs only; titles stay on the current plan so context history
    /// remains a small, provider-neutral summary.
    pub references: Vec<String>,
    /// Compatibility shim for plans written by 43e4d0a, which retained full
    /// item objects in history. It is migrated to `references` on load and is
    /// never emitted in the current format.
    #[serde(default, skip_serializing)]
    legacy_items: Vec<ListeningPlanItem>,
    pub source: String,
    pub confidence: String,
    pub recorded_at: u64,
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
        if !crate::bus::is_applemusic_zone_id(zone_id) {
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
            history: previous
                .as_ref()
                .map(|plan| plan.history.clone())
                .unwrap_or_default(),
        };
        let mut plan = plan;
        record_revision(&mut plan, "replace");
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

    /// Append intent to an existing plan without pretending the provider queue
    /// has accepted it. The generation changes so callers can correlate a
    /// later acknowledgement or detect a stale plan response.
    pub async fn append(
        &self,
        zone_id: &str,
        items: Vec<ListeningPlanItem>,
    ) -> anyhow::Result<ListeningPlan> {
        validate_items(zone_id, &items)?;
        let mut plans = self.plans.write().await;
        let previous = plans.get(zone_id).cloned();
        let mut plan = previous.clone().unwrap_or(ListeningPlan {
            zone_id: zone_id.to_string(),
            items: Vec::new(),
            current_index: None,
            generation: 0,
            updated_at: 0,
            history: Vec::new(),
        });
        if plan.items.len().saturating_add(items.len()) > MAX_ITEMS {
            anyhow::bail!("listening plan is limited to {MAX_ITEMS} items");
        }
        plan.items.extend(items);
        plan.generation = plan.generation.saturating_add(1).max(1);
        plan.updated_at = now_secs();
        record_revision(&mut plan, "append");
        plans.insert(zone_id.to_string(), plan.clone());
        trim_plans(&mut plans);
        if let Some(path) = &self.path {
            if let Err(error) = persist_plans(path, &plans) {
                restore_plan(&mut plans, zone_id, previous);
                return Err(error);
            }
        }
        Ok(plan)
    }

    /// Add one item immediately after the plan's observed current item. If no
    /// current item is known, it is inserted at the front and remains merely
    /// planned intent until the companion confirms playback.
    pub async fn play_next(
        &self,
        zone_id: &str,
        item: ListeningPlanItem,
    ) -> anyhow::Result<ListeningPlan> {
        validate_items(zone_id, std::slice::from_ref(&item))?;
        let mut plans = self.plans.write().await;
        let previous = plans.get(zone_id).cloned();
        let mut plan = previous.clone().unwrap_or(ListeningPlan {
            zone_id: zone_id.to_string(),
            items: Vec::new(),
            current_index: None,
            generation: 0,
            updated_at: 0,
            history: Vec::new(),
        });
        if plan.items.len() >= MAX_ITEMS {
            anyhow::bail!("listening plan is limited to {MAX_ITEMS} items");
        }
        let index = plan
            .current_index
            .map(|current| current.saturating_add(1).min(plan.items.len()))
            .unwrap_or(0);
        plan.items.insert(index, item);
        plan.generation = plan.generation.saturating_add(1).max(1);
        plan.updated_at = now_secs();
        record_revision(&mut plan, "play_next");
        plans.insert(zone_id.to_string(), plan.clone());
        trim_plans(&mut plans);
        if let Some(path) = &self.path {
            if let Err(error) = persist_plans(path, &plans) {
                restore_plan(&mut plans, zone_id, previous);
                return Err(error);
            }
        }
        Ok(plan)
    }
}

const MAX_PLANS: usize = 32;
const MAX_ITEMS: usize = 200;
const MAX_HISTORY: usize = 16;
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

fn record_revision(plan: &mut ListeningPlan, operation: &str) {
    plan.history.push(ListeningPlanRevision {
        generation: plan.generation,
        operation: operation.to_string(),
        references: plan
            .items
            .iter()
            .map(|item| item.reference.clone())
            .collect(),
        legacy_items: Vec::new(),
        source: "uhc_mcp".to_string(),
        confidence: "planned".to_string(),
        recorded_at: plan.updated_at,
    });
    trim_history(plan);
}

fn trim_history(plan: &mut ListeningPlan) {
    for revision in &mut plan.history {
        if revision.references.is_empty() && !revision.legacy_items.is_empty() {
            revision.references = revision
                .legacy_items
                .iter()
                .map(|item| item.reference.clone())
                .collect();
        }
        revision.legacy_items.clear();
    }
    plan.history.retain(|revision| {
        revision.references.len() <= MAX_ITEMS
            && revision
                .references
                .iter()
                .all(|reference| !reference.is_empty() && reference.len() <= MAX_REFERENCE_LENGTH)
            && revision.operation.len() <= 64
            && revision.source.len() <= 64
            && revision.confidence.len() <= 64
    });
    if plan.history.len() > MAX_HISTORY {
        let excess = plan.history.len() - MAX_HISTORY;
        plan.history.drain(0..excess);
    }
}

fn validate_items(zone_id: &str, items: &[ListeningPlanItem]) -> anyhow::Result<()> {
    if !crate::bus::is_applemusic_zone_id(zone_id) {
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
    Ok(())
}

fn restore_plan(
    plans: &mut HashMap<String, ListeningPlan>,
    zone_id: &str,
    previous: Option<ListeningPlan>,
) {
    if let Some(previous) = previous {
        plans.insert(zone_id.to_string(), previous);
    } else {
        plans.remove(zone_id);
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
            crate::bus::is_applemusic_zone_id(&plan.zone_id)
                && plan.items.len() <= MAX_ITEMS
                && plan.items.iter().all(|item| {
                    !item.reference.is_empty()
                        && item.reference.len() <= MAX_REFERENCE_LENGTH
                        && item.title.len() <= MAX_TITLE_LENGTH
                })
        })
        .map(|mut plan| {
            trim_history(&mut plan);
            (plan.zone_id.clone(), plan)
        })
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
        assert_eq!(second.history.len(), 2);
        assert_eq!(second.history[0].operation, "replace");
        assert_eq!(second.history[1].confidence, "planned");
        assert_eq!(store.get("applemusic:iphone").await, Some(second));
    }

    #[tokio::test]
    async fn nested_or_empty_apple_owner_ids_cannot_create_plans() {
        let store = ListeningPlanStore::default();
        let item = ListeningPlanItem {
            reference: "ref".into(),
            title: "Track".into(),
        };
        assert!(store
            .replace("applemusic:", vec![item.clone()])
            .await
            .is_err());
        assert!(store
            .replace("applemusic:owner:child", vec![item])
            .await
            .is_err());
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

    #[tokio::test]
    async fn append_and_play_next_preserve_truthful_plan_generation() {
        let store = ListeningPlanStore::default();
        let first = store
            .replace(
                "applemusic:iphone",
                vec![ListeningPlanItem {
                    reference: "one".into(),
                    title: "One".into(),
                }],
            )
            .await
            .unwrap();
        let appended = store
            .append(
                "applemusic:iphone",
                vec![ListeningPlanItem {
                    reference: "two".into(),
                    title: "Two".into(),
                }],
            )
            .await
            .unwrap();
        assert_eq!(appended.generation, first.generation + 1);
        assert_eq!(
            appended
                .items
                .iter()
                .map(|i| i.reference.as_str())
                .collect::<Vec<_>>(),
            ["one", "two"]
        );
        let next = store
            .play_next(
                "applemusic:iphone",
                ListeningPlanItem {
                    reference: "next".into(),
                    title: "Next".into(),
                },
            )
            .await
            .unwrap();
        assert_eq!(next.items[0].reference, "next");
        assert_eq!(next.generation, appended.generation + 1);
        assert_eq!(
            next.history
                .iter()
                .map(|entry| entry.operation.as_str())
                .collect::<Vec<_>>(),
            ["replace", "append", "play_next",]
        );
    }

    #[tokio::test]
    async fn plan_history_is_bounded_and_keeps_newest_intent() {
        let store = ListeningPlanStore::default();
        for index in 0..(MAX_HISTORY + 4) {
            store
                .replace(
                    "applemusic:iphone",
                    vec![ListeningPlanItem {
                        reference: format!("ref-{index}"),
                        title: format!("Track {index}"),
                    }],
                )
                .await
                .unwrap();
        }
        let plan = store.get("applemusic:iphone").await.unwrap();
        assert_eq!(plan.history.len(), MAX_HISTORY);
        assert_eq!(plan.history.first().unwrap().generation, 5);
        assert_eq!(
            plan.history.last().unwrap().generation,
            MAX_HISTORY as u64 + 4
        );
        assert_eq!(plan.history.last().unwrap().source, "uhc_mcp");
        assert_eq!(plan.history.last().unwrap().confidence, "planned");
    }
}
