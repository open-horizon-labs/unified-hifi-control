//! Knob device store - manages registered S3 Knob devices
//!
//! Each knob has:
//! - Unique ID (from ESP32 chip ID)
//! - Name (user-assigned)
//! - Configuration (power saving, display rotation, etc.)
//! - Status (battery level, current zone, last seen)

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::config::{get_config_file_path, read_config_file};

const KNOBS_FILE: &str = "knobs.json";

/// Power mode configuration (timeout-based state transition)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PowerModeConfig {
    pub enabled: bool,
    pub timeout_sec: u32,
}

/// Knob configuration (synced to device via config_sha)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KnobConfig {
    /// Display rotation when charging (0, 90, 180, 270)
    pub rotation_charging: u16,
    /// Display rotation when on battery
    pub rotation_not_charging: u16,

    // Power modes when charging
    pub art_mode_charging: PowerModeConfig,
    pub dim_charging: PowerModeConfig,
    pub sleep_charging: PowerModeConfig,
    pub deep_sleep_charging: PowerModeConfig,

    // Power modes when on battery
    pub art_mode_battery: PowerModeConfig,
    pub dim_battery: PowerModeConfig,
    pub sleep_battery: PowerModeConfig,
    pub deep_sleep_battery: PowerModeConfig,

    /// WiFi power save mode
    pub wifi_power_save_enabled: bool,
    /// CPU frequency scaling
    pub cpu_freq_scaling_enabled: bool,
    /// Poll interval when playback stopped
    pub sleep_poll_stopped_sec: u32,
}

impl Default for KnobConfig {
    fn default() -> Self {
        Self {
            rotation_charging: 180,
            rotation_not_charging: 0,
            art_mode_charging: PowerModeConfig {
                enabled: true,
                timeout_sec: 60,
            },
            dim_charging: PowerModeConfig {
                enabled: true,
                timeout_sec: 120,
            },
            sleep_charging: PowerModeConfig {
                enabled: false,
                timeout_sec: 0,
            },
            deep_sleep_charging: PowerModeConfig {
                enabled: false,
                timeout_sec: 0,
            },
            art_mode_battery: PowerModeConfig {
                enabled: true,
                timeout_sec: 30,
            },
            dim_battery: PowerModeConfig {
                enabled: true,
                timeout_sec: 30,
            },
            sleep_battery: PowerModeConfig {
                enabled: true,
                timeout_sec: 60,
            },
            deep_sleep_battery: PowerModeConfig {
                enabled: true,
                timeout_sec: 1200,
            },
            wifi_power_save_enabled: false,
            cpu_freq_scaling_enabled: false,
            sleep_poll_stopped_sec: 60,
        }
    }
}

/// Knob runtime status (updated on each request)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct KnobStatus {
    pub battery_level: Option<u8>,
    pub battery_charging: Option<bool>,
    pub zone_id: Option<String>,
    pub ip: Option<String>,
}

/// Registered knob device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Knob {
    pub name: String,
    pub last_seen: DateTime<Utc>,
    pub version: Option<String>,
    pub config: KnobConfig,
    pub config_sha: String,
    pub status: KnobStatus,
}

/// Compute SHA256 hash of config (first 8 chars)
fn compute_sha(config: &KnobConfig, name: &str) -> String {
    let mut hasher = Sha256::new();
    // Include name in hash so renaming triggers sync
    let json = serde_json::json!({
        "config": config,
        "name": name,
    });
    hasher.update(json.to_string().as_bytes());
    let result = hasher.finalize();
    hex::encode(&result[..4]) // First 8 hex chars
}

/// Knob device store
#[derive(Clone)]
pub struct KnobStore {
    knobs: Arc<RwLock<HashMap<String, Knob>>>,
}

impl Default for KnobStore {
    fn default() -> Self {
        Self::new()
    }
}

impl KnobStore {
    /// Create new store, loading existing knobs from disk
    /// Issue #76: Uses config subdirectory for knobs.json
    pub fn new() -> Self {
        let knobs = Self::load_from_disk();
        Self {
            knobs: Arc::new(RwLock::new(knobs)),
        }
    }

    /// Load knobs from disk with backwards-compatible fallback
    /// Issue #76: Uses read_config_file to check subdir first, fall back to root
    fn load_from_disk() -> HashMap<String, Knob> {
        // read_config_file checks subdir first, falls back to root for legacy files
        if let Some(content) = read_config_file(KNOBS_FILE) {
            if let Ok(knobs) = serde_json::from_str(&content) {
                return knobs;
            }
        }
        HashMap::new()
    }

    /// Save knobs to disk in the config subdirectory
    /// Issue #76: Always writes to unified-hifi/ subdirectory
    async fn save_to_disk(&self) {
        let knobs = self.knobs.read().await;
        let path = get_config_file_path(KNOBS_FILE);

        // Ensure directory exists
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }

        if let Ok(json) = serde_json::to_string_pretty(&*knobs) {
            let _ = fs::write(path, json);
        }
    }

    /// Get knob by ID
    pub async fn get(&self, knob_id: &str) -> Option<Knob> {
        let knobs = self.knobs.read().await;
        knobs.get(knob_id).cloned()
    }

    /// Get or create knob, updating last_seen and version
    pub async fn get_or_create(&self, knob_id: &str, version: Option<&str>) -> Knob {
        let mut knobs = self.knobs.write().await;

        if let Some(knob) = knobs.get_mut(knob_id) {
            knob.last_seen = Utc::now();
            if let Some(v) = version {
                knob.version = Some(v.to_string());
            }
            let result = knob.clone();
            drop(knobs);
            self.save_to_disk().await;
            return result;
        }

        // Create new knob
        let config = KnobConfig::default();
        let name = String::new();
        let config_sha = compute_sha(&config, &name);

        let knob = Knob {
            name,
            last_seen: Utc::now(),
            version: version.map(|s| s.to_string()),
            config,
            config_sha,
            status: KnobStatus::default(),
        };

        knobs.insert(knob_id.to_string(), knob.clone());
        drop(knobs);
        self.save_to_disk().await;

        tracing::info!("Created new knob: {}", knob_id);
        knob
    }

    /// Update knob status (battery, zone, IP)
    pub async fn update_status(&self, knob_id: &str, updates: KnobStatusUpdate) {
        let mut knobs = self.knobs.write().await;

        if let Some(knob) = knobs.get_mut(knob_id) {
            if let Some(level) = updates.battery_level {
                knob.status.battery_level = Some(level);
            }
            if let Some(charging) = updates.battery_charging {
                knob.status.battery_charging = Some(charging);
            }
            if let Some(zone_id) = updates.zone_id {
                knob.status.zone_id = Some(zone_id);
            }
            if let Some(ip) = updates.ip {
                knob.status.ip = Some(ip);
            }
            knob.last_seen = Utc::now();
        }

        drop(knobs);
        self.save_to_disk().await;
    }

    /// Update knob configuration
    pub async fn update_config(&self, knob_id: &str, updates: KnobConfigUpdate) -> Option<Knob> {
        let mut knobs = self.knobs.write().await;

        let knob = knobs.get_mut(knob_id)?;

        if let Some(name) = updates.name {
            knob.name = name;
        }
        if let Some(v) = updates.rotation_charging {
            knob.config.rotation_charging = v;
        }
        if let Some(v) = updates.rotation_not_charging {
            knob.config.rotation_not_charging = v;
        }
        // Power mode updates
        if let Some(v) = updates.art_mode_charging {
            knob.config.art_mode_charging = v;
        }
        if let Some(v) = updates.art_mode_battery {
            knob.config.art_mode_battery = v;
        }
        if let Some(v) = updates.dim_charging {
            knob.config.dim_charging = v;
        }
        if let Some(v) = updates.dim_battery {
            knob.config.dim_battery = v;
        }
        if let Some(v) = updates.sleep_charging {
            knob.config.sleep_charging = v;
        }
        if let Some(v) = updates.sleep_battery {
            knob.config.sleep_battery = v;
        }
        if let Some(v) = updates.deep_sleep_charging {
            knob.config.deep_sleep_charging = v;
        }
        if let Some(v) = updates.deep_sleep_battery {
            knob.config.deep_sleep_battery = v;
        }
        if let Some(v) = updates.wifi_power_save_enabled {
            knob.config.wifi_power_save_enabled = v;
        }
        if let Some(v) = updates.cpu_freq_scaling_enabled {
            knob.config.cpu_freq_scaling_enabled = v;
        }
        if let Some(v) = updates.sleep_poll_stopped_sec {
            knob.config.sleep_poll_stopped_sec = v;
        }

        // Recompute config hash
        knob.config_sha = compute_sha(&knob.config, &knob.name);
        knob.last_seen = Utc::now();

        let result = knob.clone();
        drop(knobs);
        self.save_to_disk().await;

        tracing::info!(
            "Updated knob config: {} (sha: {})",
            knob_id,
            result.config_sha
        );
        Some(result)
    }

    /// List all registered knobs
    pub async fn list(&self) -> Vec<KnobSummary> {
        let knobs = self.knobs.read().await;
        knobs
            .iter()
            .map(|(id, knob)| KnobSummary {
                knob_id: id.clone(),
                name: knob.name.clone(),
                last_seen: knob.last_seen,
                version: knob.version.clone(),
                status: knob.status.clone(),
            })
            .collect()
    }

    /// Get config SHA for a knob (for change detection)
    pub async fn get_config_sha(&self, knob_id: &str) -> Option<String> {
        let knobs = self.knobs.read().await;
        knobs.get(knob_id).map(|k| k.config_sha.clone())
    }
}

/// Partial status update
#[derive(Debug, Default)]
pub struct KnobStatusUpdate {
    pub battery_level: Option<u8>,
    pub battery_charging: Option<bool>,
    pub zone_id: Option<String>,
    pub ip: Option<String>,
}

/// Partial config update
#[derive(Debug, Default, Deserialize)]
pub struct KnobConfigUpdate {
    pub name: Option<String>,
    pub rotation_charging: Option<u16>,
    pub rotation_not_charging: Option<u16>,
    pub art_mode_charging: Option<PowerModeConfig>,
    pub art_mode_battery: Option<PowerModeConfig>,
    pub dim_charging: Option<PowerModeConfig>,
    pub dim_battery: Option<PowerModeConfig>,
    pub sleep_charging: Option<PowerModeConfig>,
    pub sleep_battery: Option<PowerModeConfig>,
    pub deep_sleep_charging: Option<PowerModeConfig>,
    pub deep_sleep_battery: Option<PowerModeConfig>,
    pub wifi_power_save_enabled: Option<bool>,
    pub cpu_freq_scaling_enabled: Option<bool>,
    pub sleep_poll_stopped_sec: Option<u32>,
}

/// Summary for listing knobs
#[derive(Debug, Serialize)]
pub struct KnobSummary {
    pub knob_id: String,
    pub name: String,
    pub last_seen: DateTime<Utc>,
    pub version: Option<String>,
    pub status: KnobStatus,
}
