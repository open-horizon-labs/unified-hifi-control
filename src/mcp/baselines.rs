//! Session caches are invalidated once until the client fetches a fresh full list.
//! Idle entries expire and capacity eviction bounds clients that disappear without DELETE.

use rust_mcp_sdk::schema::Resource;
use std::collections::HashMap;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::time::Instant;

const IDLE_TTL: Duration = Duration::from_secs(30 * 60);
const CAPACITY: usize = 256;

#[derive(Default)]
pub struct McpResourceBaselines {
    pub(crate) inner: Mutex<Baselines>,
}

#[derive(Default)]
pub(crate) struct Baselines {
    entries: HashMap<String, Baseline>,
    generation: u64,
}
struct Baseline {
    served: String,
    invalidated: bool,
    last_served: Instant,
    generation: u64,
    revision: u64,
}
impl Baselines {
    /// Return a generation only when this session needs a new notifier.
    pub(crate) fn served(&mut self, session: &str, fingerprint: String) -> Option<u64> {
        let now = Instant::now();
        self.entries
            .retain(|_, entry| now.duration_since(entry.last_served) < IDLE_TTL);
        if let Some(entry) = self.entries.get_mut(session) {
            entry.revision += 1;
            entry.served = fingerprint;
            entry.invalidated = false;
            entry.last_served = now;
            return None;
        }
        if self.entries.len() >= CAPACITY {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_served)
                .map(|(k, _)| k.clone())
            {
                self.entries.remove(&oldest);
            }
        }
        self.generation += 1;
        let generation = self.generation;
        self.entries.insert(
            session.into(),
            Baseline {
                served: fingerprint,
                invalidated: false,
                last_served: now,
                generation,
                revision: 0,
            },
        );
        Some(generation)
    }
    pub(crate) fn active(&mut self, session: &str, generation: u64) -> bool {
        if self
            .entries
            .get(session)
            .is_some_and(|e| e.generation == generation && e.last_served.elapsed() < IDLE_TTL)
        {
            return true;
        }
        self.remove(session, generation);
        false
    }
    pub(crate) fn revision(&self, session: &str) -> Option<u64> {
        self.entries
            .get(session)
            .filter(|e| !e.invalidated)
            .map(|e| e.revision)
    }
    pub(crate) fn needs_notification(&self, session: &str, current: &str) -> bool {
        self.entries
            .get(session)
            .is_some_and(|e| !e.invalidated && e.served != current)
    }
    pub(crate) fn claim(&mut self, session: &str) -> u64 {
        let entry = self.entries.get_mut(session).expect("active baseline");
        entry.invalidated = true;
        entry.revision
    }
    pub(crate) fn send_failed(&mut self, session: &str, generation: u64, revision: u64) {
        if let Some(entry) = self.entries.get_mut(session) {
            if entry.generation == generation && entry.revision == revision {
                entry.invalidated = false;
            }
        }
    }
    pub(crate) fn remove(&mut self, session: &str, generation: u64) {
        if self
            .entries
            .get(session)
            .is_some_and(|e| e.generation == generation)
        {
            self.entries.remove(session);
        }
    }
}

pub fn list_fingerprint(resources: &[Resource]) -> String {
    serde_json::to_string(resources).expect("resource advertisements serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sessions_keep_their_own_served_baseline_and_only_one_pending_hint() {
        let mut store = Baselines::default();
        store.served("a", "old".into());
        store.served("b", "new".into());
        assert!(store.needs_notification("a", "new"));
        assert!(!store.needs_notification("b", "new"));
        store.claim("a");
        assert!(!store.needs_notification("a", "new"));
        assert!(!store.needs_notification("a", "third"));
        assert_eq!(store.entries["a"].served, "old");
        store.served("a", "third".into());
        assert!(!store.needs_notification("a", "third"));
        assert!(store.needs_notification("a", "fourth"));
    }
    #[test]
    fn capacity_eviction_and_expiry_rearm_returning_sessions() {
        let mut store = Baselines::default();
        let first = store.served("first", "old".into()).unwrap();
        for i in 0..CAPACITY {
            store.served(&i.to_string(), "x".into());
        }
        assert_eq!(store.entries.len(), CAPACITY);
        assert!(!store.active("first", first));
        let replacement = store.served("first", "full-current-list".into()).unwrap();
        store.remove("first", first);
        assert!(store.active("first", replacement));
        store.entries.get_mut("first").unwrap().last_served = Instant::now() - IDLE_TTL;
        assert!(!store.active("first", replacement));
        assert!(store.served("first", "full-new-list".into()).is_some());
    }
}

#[cfg(test)]
mod race_tests {
    use super::*;
    #[test]
    fn late_send_failure_cannot_rearm_a_newer_served_snapshot() {
        let mut store = Baselines::default();
        let generation = store.served("s", "a".into()).unwrap();
        let revision = store.claim("s");
        store.served("s", "b".into());
        store.claim("s");
        store.send_failed("s", generation, revision);
        assert!(!store.needs_notification("s", "c"));
        assert_eq!(store.entries["s"].served, "b");
    }
}
