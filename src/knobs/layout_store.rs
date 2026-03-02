//! Layout store — named manifest templates with condition-based activation.
//!
//! Layouts are standalone manifest templates (screens + nav + interactions)
//! created via the configurator's LLM chat. Each device has an ordered
//! condition stack; the bridge evaluates conditions against zone runtime
//! state and serves the first matching layout.
//!
//! Data model:
//! - Layout: named manifest template (screens + nav)
//! - Condition: field/op/value triple matching zone state
//! - DeviceStack: ordered list of (conditions → layout) entries per device
//! - StackEntry: one entry in the stack (layout_id + conditions + enabled flag)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::bus::{NowPlaying, Zone};
use crate::config;
use crate::knobs::manifest::{Nav, Screen};

const LAYOUTS_FILE: &str = "layouts.json";

/// Generate a random ID (16 hex chars).
fn generate_id() -> String {
    hex::encode(rand::random::<[u8; 8]>())
}

// ── Core types ──────────────────────────────────────────────────────────────

/// A named manifest template.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layout {
    pub id: String,
    pub name: String,
    pub screens: Vec<Screen>,
    pub nav: Nav,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interactions: Option<HashMap<String, String>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A condition that matches against zone runtime state.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Condition {
    /// Field to match: "zone_id", "zone_name", "source", "genre", "artist",
    /// "album", "format", "duration", "default"
    pub field: String,
    /// Comparison operator
    pub op: ConditionOp,
    /// Value to compare against (None for Always/Exists)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

/// Comparison operators for conditions.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ConditionOp {
    /// Exact string match
    Equals,
    /// Substring match (case-insensitive)
    Contains,
    /// Numeric greater-than (for duration, sample_rate, etc.)
    GreaterThan,
    /// Field exists and is non-empty
    Exists,
    /// Always matches (used for "default" fallback)
    Always,
}

/// A device's ordered condition stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStack {
    pub device_id: String,
    pub entries: Vec<StackEntry>,
}

/// One entry in a device's condition stack.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StackEntry {
    pub layout_id: String,
    /// ALL conditions must match (AND logic). Empty = always matches.
    pub conditions: Vec<Condition>,
    /// Can be toggled without removing from stack.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

pub fn default_true() -> bool {
    true
}

// ── Condition evaluation ────────────────────────────────────────────────────

impl Condition {
    /// Test whether this condition matches the given zone state.
    pub fn matches(&self, zone: &Zone) -> bool {
        let field_value = self.extract_field(zone);
        match self.op {
            ConditionOp::Always => true,
            ConditionOp::Exists => field_value.map(|v| !v.is_empty()).unwrap_or(false),
            ConditionOp::Equals => {
                let target = self.value.as_deref().unwrap_or("");
                field_value
                    .map(|v| v.eq_ignore_ascii_case(target))
                    .unwrap_or(false)
            }
            ConditionOp::Contains => {
                let target = self.value.as_deref().unwrap_or("").to_lowercase();
                field_value
                    .map(|v| v.to_lowercase().contains(&target))
                    .unwrap_or(false)
            }
            ConditionOp::GreaterThan => {
                let target: f64 = match self.value.as_deref().and_then(|v| v.parse().ok()) {
                    Some(t) => t,
                    None => {
                        tracing::warn!(
                            field = %self.field,
                            value = ?self.value,
                            "GreaterThan condition has missing or unparseable value"
                        );
                        return false;
                    }
                };
                field_value
                    .and_then(|v| v.parse::<f64>().ok())
                    .map(|v| v > target)
                    .unwrap_or(false)
            }
        }
    }

    /// Extract the named field from zone state as a string.
    fn extract_field(&self, zone: &Zone) -> Option<String> {
        match self.field.as_str() {
            "default" => Some("true".to_string()),
            "zone_id" => Some(zone.zone_id.clone()),
            "zone_name" => Some(zone.zone_name.clone()),
            "source" => Some(zone.source.clone()),
            "state" => Some(zone.state.to_string()),
            // NowPlaying fields
            "title" => np_field(zone, |np| np.title.clone()),
            "artist" => np_field(zone, |np| np.artist.clone()),
            "album" => np_field(zone, |np| np.album.clone()),
            "duration" => np_field(zone, |np| {
                np.duration.map(|d| d.to_string()).unwrap_or_default()
            }),
            // TrackMetadata fields
            "genre" => meta_field(zone, |m| m.genre.clone()),
            "format" => meta_field(zone, |m| m.format.clone()),
            "sample_rate" => meta_field(zone, |m| m.sample_rate.map(|sr| sr.to_string())),
            "bit_depth" => meta_field(zone, |m| m.bit_depth.map(|bd| bd.to_string())),
            "composer" => meta_field(zone, |m| m.composer.clone()),
            other => {
                tracing::warn!(field = %other, "Unknown condition field — condition will not match");
                None
            }
        }
    }
}

/// Helper: extract a field from NowPlaying.
fn np_field(zone: &Zone, f: impl Fn(&NowPlaying) -> String) -> Option<String> {
    zone.now_playing.as_ref().map(f)
}

/// Helper: extract an optional field from TrackMetadata.
fn meta_field(
    zone: &Zone,
    f: impl Fn(&crate::bus::TrackMetadata) -> Option<String>,
) -> Option<String> {
    zone.now_playing
        .as_ref()
        .and_then(|np| np.metadata.as_ref())
        .and_then(f)
}

// ── Store ───────────────────────────────────────────────────────────────────

/// Persistent storage for persisted layouts on disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct PersistedData {
    #[serde(default)]
    layouts: HashMap<String, Layout>,
    #[serde(default)]
    stacks: HashMap<String, DeviceStack>,
}

/// Thread-safe layout store with disk persistence.
#[derive(Debug, Clone)]
pub struct LayoutStore {
    data: Arc<RwLock<PersistedData>>,
}

impl Default for LayoutStore {
    fn default() -> Self {
        Self::new()
    }
}

impl LayoutStore {
    pub fn new() -> Self {
        let data = Self::load_from_disk();
        Self {
            data: Arc::new(RwLock::new(data)),
        }
    }

    // ── Layout CRUD ─────────────────────────────────────────────────────

    /// Create a new layout and return it.
    pub async fn create_layout(
        &self,
        name: String,
        screens: Vec<Screen>,
        nav: Nav,
        interactions: Option<HashMap<String, String>>,
    ) -> Layout {
        let id = generate_id();
        let now = Utc::now();
        let layout = Layout {
            id: id.clone(),
            name,
            screens,
            nav,
            interactions,
            created_at: now,
            updated_at: now,
        };
        self.data
            .write()
            .await
            .layouts
            .insert(id.clone(), layout.clone());
        self.save_to_disk().await;
        tracing::info!(layout_id = %id, "Layout created");
        layout
    }

    /// Get a layout by ID.
    pub async fn get_layout(&self, id: &str) -> Option<Layout> {
        self.data.read().await.layouts.get(id).cloned()
    }

    /// List all layouts.
    pub async fn list_layouts(&self) -> Vec<Layout> {
        self.data.read().await.layouts.values().cloned().collect()
    }

    /// Update a layout's name, screens, nav, or interactions. Returns the updated layout.
    pub async fn update_layout(
        &self,
        id: &str,
        name: Option<String>,
        screens: Option<Vec<Screen>>,
        nav: Option<Nav>,
        interactions: Option<HashMap<String, String>>,
    ) -> Option<Layout> {
        let mut data = self.data.write().await;
        if let Some(layout) = data.layouts.get_mut(id) {
            if let Some(n) = name {
                layout.name = n;
            }
            if let Some(s) = screens {
                layout.screens = s;
            }
            if let Some(n) = nav {
                layout.nav = n;
            }
            if let Some(i) = interactions {
                layout.interactions = Some(i);
            }
            layout.updated_at = Utc::now();
            let result = layout.clone();
            drop(data);
            self.save_to_disk().await;
            Some(result)
        } else {
            None
        }
    }

    /// Delete a layout. Also removes it from any device stacks.
    pub async fn delete_layout(&self, id: &str) -> bool {
        let mut data = self.data.write().await;
        let existed = data.layouts.remove(id).is_some();
        if existed {
            // Remove from all stacks
            for stack in data.stacks.values_mut() {
                stack.entries.retain(|e| e.layout_id != id);
            }
            drop(data);
            self.save_to_disk().await;
            tracing::info!(layout_id = %id, "Layout deleted");
        }
        existed
    }

    // ── Device Stack CRUD ───────────────────────────────────────────────

    /// Get a device's condition stack.
    pub async fn get_stack(&self, device_id: &str) -> Option<DeviceStack> {
        self.data.read().await.stacks.get(device_id).cloned()
    }

    /// Set (replace) a device's entire condition stack.
    pub async fn set_stack(&self, device_id: &str, entries: Vec<StackEntry>) {
        let stack = DeviceStack {
            device_id: device_id.to_string(),
            entries,
        };
        self.data
            .write()
            .await
            .stacks
            .insert(device_id.to_string(), stack);
        self.save_to_disk().await;
        tracing::info!(device_id = %device_id, "Device stack updated");
    }

    /// Add an entry to the end of a device's stack.
    pub async fn push_stack_entry(&self, device_id: &str, entry: StackEntry) {
        let mut data = self.data.write().await;
        let stack = data
            .stacks
            .entry(device_id.to_string())
            .or_insert_with(|| DeviceStack {
                device_id: device_id.to_string(),
                entries: Vec::new(),
            });
        stack.entries.push(entry);
        drop(data);
        self.save_to_disk().await;
    }

    /// Remove a stack entry by index.
    pub async fn remove_stack_entry(&self, device_id: &str, index: usize) -> bool {
        let mut data = self.data.write().await;
        if let Some(stack) = data.stacks.get_mut(device_id) {
            if index < stack.entries.len() {
                stack.entries.remove(index);
                drop(data);
                self.save_to_disk().await;
                return true;
            }
        }
        false
    }

    /// Reorder a device's stack (move entry from `from` to `to` index).
    pub async fn reorder_stack_entry(&self, device_id: &str, from: usize, to: usize) -> bool {
        let mut data = self.data.write().await;
        if let Some(stack) = data.stacks.get_mut(device_id) {
            let len = stack.entries.len();
            if from < len && to < len {
                let entry = stack.entries.remove(from);
                stack.entries.insert(to, entry);
                drop(data);
                self.save_to_disk().await;
                return true;
            }
        }
        false
    }

    // ── Condition resolution ────────────────────────────────────────────

    /// Resolve which layout should be active for a device given the current zone state.
    /// Walks the stack top-to-bottom, returns the first matching layout.
    pub async fn resolve_layout(&self, device_id: &str, zone: &Zone) -> Option<Layout> {
        let data = self.data.read().await;
        let stack = data.stacks.get(device_id)?;
        for entry in &stack.entries {
            if !entry.enabled {
                continue;
            }
            // All conditions must match (AND). Empty conditions = always matches.
            let all_match =
                entry.conditions.is_empty() || entry.conditions.iter().all(|c| c.matches(zone));
            if all_match {
                return data.layouts.get(&entry.layout_id).cloned();
            }
        }
        None
    }

    // ── Persistence ─────────────────────────────────────────────────────

    fn load_from_disk() -> PersistedData {
        let content = match config::read_config_file(LAYOUTS_FILE) {
            Some(c) if !c.is_empty() => c,
            _ => return PersistedData::default(),
        };
        match serde_json::from_str::<PersistedData>(&content) {
            Ok(data) => {
                tracing::info!(
                    layouts = data.layouts.len(),
                    stacks = data.stacks.len(),
                    "Loaded layouts from disk"
                );
                data
            }
            Err(e) => {
                tracing::error!("Failed to parse {}: {}", LAYOUTS_FILE, e);
                PersistedData::default()
            }
        }
    }

    async fn save_to_disk(&self) {
        // Serialize under lock, then drop lock before blocking I/O
        let json = {
            let data = self.data.read().await;
            match serde_json::to_string_pretty(&*data) {
                Ok(json) => json,
                Err(e) => {
                    tracing::error!("Failed to serialize layouts: {}", e);
                    return;
                }
            }
        };
        let path = config::get_config_file_path(LAYOUTS_FILE);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&path, json) {
            tracing::error!("Failed to write {}: {}", LAYOUTS_FILE, e);
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{NowPlaying, PlaybackState, TrackMetadata};

    fn test_zone() -> Zone {
        Zone {
            zone_id: "roon:abc123".to_string(),
            zone_name: "Living Room".to_string(),
            state: PlaybackState::Playing,
            volume_control: None,
            now_playing: Some(NowPlaying {
                title: "Episode 42".to_string(),
                artist: "NPR".to_string(),
                album: "All Things Considered".to_string(),
                image_key: None,
                seek_position: Some(300.0),
                duration: Some(3600.0),
                metadata: Some(TrackMetadata {
                    format: Some("FLAC".to_string()),
                    sample_rate: Some(44100),
                    bit_depth: Some(16),
                    bitrate: None,
                    genre: Some("Podcast".to_string()),
                    composer: None,
                    track_number: None,
                    disc_number: None,
                }),
            }),
            source: "roon".to_string(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 0,
            is_play_allowed: false,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: false,
        }
    }

    #[test]
    fn test_condition_always() {
        let c = Condition {
            field: "default".to_string(),
            op: ConditionOp::Always,
            value: None,
        };
        assert!(c.matches(&test_zone()));
    }

    #[test]
    fn test_condition_equals() {
        let c = Condition {
            field: "source".to_string(),
            op: ConditionOp::Equals,
            value: Some("roon".to_string()),
        };
        assert!(c.matches(&test_zone()));

        let c2 = Condition {
            field: "source".to_string(),
            op: ConditionOp::Equals,
            value: Some("lms".to_string()),
        };
        assert!(!c2.matches(&test_zone()));
    }

    #[test]
    fn test_condition_contains() {
        let c = Condition {
            field: "genre".to_string(),
            op: ConditionOp::Contains,
            value: Some("podcast".to_string()),
        };
        assert!(c.matches(&test_zone()));

        let c2 = Condition {
            field: "artist".to_string(),
            op: ConditionOp::Contains,
            value: Some("npr".to_string()),
        };
        assert!(c2.matches(&test_zone()));
    }

    #[test]
    fn test_condition_greater_than() {
        let c = Condition {
            field: "duration".to_string(),
            op: ConditionOp::GreaterThan,
            value: Some("600".to_string()),
        };
        assert!(c.matches(&test_zone())); // 3600 > 600

        let c2 = Condition {
            field: "duration".to_string(),
            op: ConditionOp::GreaterThan,
            value: Some("7200".to_string()),
        };
        assert!(!c2.matches(&test_zone())); // 3600 > 7200 = false
    }

    #[test]
    fn test_condition_exists() {
        let c = Condition {
            field: "genre".to_string(),
            op: ConditionOp::Exists,
            value: None,
        };
        assert!(c.matches(&test_zone()));
    }

    #[test]
    fn test_condition_missing_field() {
        let c = Condition {
            field: "nonexistent".to_string(),
            op: ConditionOp::Equals,
            value: Some("anything".to_string()),
        };
        assert!(!c.matches(&test_zone()));
    }
}
