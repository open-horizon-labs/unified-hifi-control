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

use crate::config::{get_config_dir, get_config_file_path};
use anyhow::{Context, Result};
use std::io::Write;
use std::path::{Path, PathBuf};

const KNOBS_FILE: &str = "knobs.json";

/// Power mode configuration (timeout-based state transition)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PowerModeConfig {
    pub enabled: bool,
    pub timeout_sec: u32,
}

/// Knob configuration (synced to device via config_sha)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
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

    /// Volume step override (None = use adapter default)
    /// When set, substituted in /now_playing response instead of adapter's step
    pub volume_step_override: Option<f64>,

    /// User-requested zone reassignment (None = device chooses its own zone).
    /// Distinct from `KnobStatus::zone_id`, which reports the zone the knob is
    /// *currently* controlling; this is the desired override synced down to
    /// the device via `config_sha`, e.g. from a Home Assistant `select`
    /// entity (#523).
    pub assigned_zone_id: Option<String>,

    /// Zones this controller does not list, on top of the global hide list in app settings.
    ///
    /// A list filter and nothing else, like the global one (see `zone_list`): a zone hidden here
    /// still plays when addressed by ID, so an `assigned_zone_id` or a shortcut that targets it
    /// keeps working. Omitted from the serialised config while empty so a controller that never
    /// uses this keeps its `config_sha`, and is not resynced by an upgrade.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hidden_zone_ids: Vec<String>,
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
            volume_step_override: None,
            assigned_zone_id: None,
            hidden_zone_ids: Vec::new(),
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

fn legacy_device_type() -> String {
    "knob".into()
}

/// Registered knob device
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Knob {
    #[serde(default = "legacy_device_type")]
    pub device_type: String,
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
    path: Arc<PathBuf>,
    load_failed: bool,
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
        // Resolve once: a running store must never switch storage directories.
        let path = get_config_file_path(KNOBS_FILE);
        let loaded = Self::load_from_disk(&path, &get_config_dir().join(KNOBS_FILE));
        let load_failed = loaded.is_err();
        let knobs = loaded.unwrap_or_else(|error| {
            tracing::error!(path = %path.display(), %error,
                "Controller settings could not be loaded; refusing writes and default registration. Restore the file/access and restart the bridge");
            HashMap::new()
        });
        tracing::info!(path = %path.display(), controllers = knobs.len(), load_failed,
            "Controller settings storage initialized");
        Self {
            knobs: Arc::new(RwLock::new(knobs)),
            path: Arc::new(path),
            load_failed,
        }
    }

    fn load_from_disk(path: &Path, legacy: &Path) -> Result<HashMap<String, Knob>> {
        let bytes = match fs::read(path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => match fs::read(legacy) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    return Ok(HashMap::new());
                }
                Err(error) => return Err(error).context("read legacy controller settings"),
            },
            Err(error) => return Err(error).context("read controller settings"),
        };
        serde_json::from_slice(&bytes).map_err(|error| {
            // Never include configuration contents in diagnostics.
            anyhow::anyhow!(
                "invalid controller settings JSON at line {}, column {}",
                error.line(),
                error.column()
            )
        })
    }

    fn ensure_loaded(&self) -> Result<()> {
        anyhow::ensure!(!self.load_failed,
            "controller settings were not loaded; restore storage access/file and restart the bridge");
        Ok(())
    }

    /// Called under the store's exclusive lock, so snapshots cannot overtake
    /// each other. A same-directory temporary file keeps the previous record
    /// intact until a complete, synced replacement can be atomically renamed.
    fn save_to_disk(
        &self,
        current: &mut HashMap<String, Knob>,
        candidate: HashMap<String, Knob>,
    ) -> Result<()> {
        self.save_with_sync(current, candidate, |_parent| {
            #[cfg(unix)]
            fs::File::open(_parent)?.sync_all()?;
            Ok(())
        })
    }

    fn save_with_sync(
        &self,
        current: &mut HashMap<String, Knob>,
        candidate: HashMap<String, Knob>,
        sync_directory: impl Fn(&Path) -> Result<()>,
    ) -> Result<()> {
        let result = (|| -> Result<()> {
            self.ensure_loaded()?;
            let parent = self
                .path
                .parent()
                .context("controller settings path has no parent")?;
            fs::create_dir_all(parent).context("create controller settings directory")?;
            // Repeat ancestor syncs on retries too: an earlier attempt may have
            // created a directory but failed to commit its entry. Settings writes
            // are infrequent; avoid a second state machine for pending syncs.
            #[cfg(unix)]
            {
                let mut ancestor = parent.parent();
                while let Some(directory) = ancestor {
                    let directory = if directory.as_os_str().is_empty() {
                        Path::new(".")
                    } else {
                        directory
                    };
                    fs::File::open(directory)?
                        .sync_all()
                        .context("sync controller settings ancestor directory")?;
                    ancestor = if directory == Path::new(".") {
                        None
                    } else {
                        directory.parent()
                    };
                }
            }
            let json =
                serde_json::to_vec_pretty(&candidate).context("encode controller settings")?;
            let mut temporary = tempfile::NamedTempFile::new_in(parent)
                .context("create controller settings replacement")?;
            temporary
                .write_all(&json)
                .context("write controller settings replacement")?;
            temporary
                .as_file()
                .sync_all()
                .context("sync controller settings replacement")?;
            temporary
                .persist(self.path.as_ref())
                .map_err(|error| error.error)
                .context("replace controller settings")?;
            // Rename installed this snapshot even if the following durability check
            // fails. Keep memory aligned; report the uncertain durability to callers.
            *current = candidate;
            sync_directory(parent).context("sync controller settings directory")?;
            Ok(())
        })();
        if let Err(error) = &result {
            tracing::error!(path = %self.path.display(), error = %format!("{error:#}"),
                "Controller settings were not durably saved; check directory, permissions and disk space");
        }
        result
    }

    /// Get knob by ID
    pub async fn get(&self, knob_id: &str) -> Option<Knob> {
        let knobs = self.knobs.read().await;
        knobs.get(knob_id).cloned()
    }

    /// Legacy requests keep the device identity previously observed from explicit headers.
    pub async fn get_or_create(&self, knob_id: &str, version: Option<&str>) -> Result<Knob> {
        self.get_or_create_with_device_type(knob_id, version, None)
            .await
    }

    /// Register a validated canonical identity, retaining identity when a header is absent or invalid.
    pub async fn get_or_create_with_device_type(
        &self,
        knob_id: &str,
        version: Option<&str>,
        device_type: Option<&str>,
    ) -> Result<Knob> {
        self.ensure_loaded()?;
        let identity = device_type.and_then(crate::firmware_catalog::FirmwareTarget::parse);
        let mut knobs = self.knobs.write().await;
        if let Some(knob) = knobs.get_mut(knob_id) {
            let old_version = knob.version.clone();
            let old_identity = knob.device_type.clone();
            knob.last_seen = Utc::now();
            if let Some(v) = version {
                knob.version = Some(v.to_string());
            }
            if let Some(target) = identity {
                knob.device_type = target.slug().to_string();
            }
            let result = knob.clone();
            let identity_changed = old_version != knob.version || old_identity != knob.device_type;
            // Routine polls remain memory-only. Persist newly observed identity,
            // without preventing service of a known config on storage failure.
            if identity_changed {
                let candidate = knobs.clone();
                let _ = self.save_to_disk(&mut knobs, candidate);
            }
            return Ok(result);
        }
        // Identity must not silently rewrite a user's power/display configuration.
        let config = KnobConfig::default();
        let name = String::new();
        let config_sha = compute_sha(&config, &name);
        let knob = Knob {
            device_type: identity
                .map_or_else(legacy_device_type, |target| target.slug().to_string()),
            name,
            last_seen: Utc::now(),
            version: version.map(|s| s.to_string()),
            config,
            config_sha,
            status: KnobStatus::default(),
        };
        let mut candidate = knobs.clone();
        candidate.insert(knob_id.to_string(), knob.clone());
        self.save_to_disk(&mut knobs, candidate)?;
        tracing::info!("Created new knob: {}", knob_id);
        Ok(knob)
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

        // Runtime telemetry is intentionally volatile, not a durable settings change.
    }

    /// Update knob configuration
    pub async fn update_config(
        &self,
        knob_id: &str,
        updates: KnobConfigUpdate,
    ) -> Result<Option<Knob>> {
        self.ensure_loaded()?;
        let mut knobs = self.knobs.write().await;
        let mut candidate = knobs.clone();
        let Some(knob) = candidate.get_mut(knob_id) else {
            return Ok(None);
        };

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
        if let Some(v) = updates.volume_step_override {
            knob.config.volume_step_override = if v > 0.0 { Some(v) } else { None };
        }
        if let Some(v) = updates.assigned_zone_id {
            knob.config.assigned_zone_id = if v.is_empty() { None } else { Some(v) };
        }
        if let Some(v) = updates.hidden_zone_ids {
            knob.config.hidden_zone_ids = normalize_zone_ids(v);
        }

        // Recompute config hash
        knob.config_sha = compute_sha(&knob.config, &knob.name);
        knob.last_seen = Utc::now();

        let result = knob.clone();
        self.save_to_disk(&mut knobs, candidate)?;

        tracing::info!(
            "Updated knob config: {} (sha: {})",
            knob_id,
            result.config_sha
        );
        Ok(Some(result))
    }

    /// List all registered knobs
    pub async fn list(&self) -> Vec<KnobSummary> {
        let knobs = self.knobs.read().await;
        knobs
            .iter()
            .map(|(id, knob)| KnobSummary {
                knob_id: id.clone(),
                device_type: knob.device_type.clone(),
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

    /// List every registered knob with its full record (config + status),
    /// keyed by knob id. Used by the MQTT/Home Assistant publisher (#523),
    /// which needs `KnobConfig` fields `list()`'s `KnobSummary` omits.
    pub async fn list_full(&self) -> Vec<(String, Knob)> {
        let knobs = self.knobs.read().await;
        knobs
            .iter()
            .map(|(id, knob)| (id.clone(), knob.clone()))
            .collect()
    }

    /// Remove a registered knob entirely. Returns `true` if a knob with this
    /// id existed. Callers (e.g. the MQTT publisher) are responsible for
    /// retracting any Home Assistant discovery configs they published for it.
    pub async fn remove(&self, knob_id: &str) -> Result<bool> {
        self.ensure_loaded()?;
        let mut knobs = self.knobs.write().await;
        let mut candidate = knobs.clone();
        if candidate.remove(knob_id).is_none() {
            return Ok(false);
        }
        self.save_to_disk(&mut knobs, candidate)?;
        tracing::info!("Removed knob: {}", knob_id);
        Ok(true)
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
    /// Volume step override (0 or negative = clear override)
    pub volume_step_override: Option<f64>,
    /// Zone reassignment override (empty string = clear override, device
    /// chooses its own zone again)
    pub assigned_zone_id: Option<String>,
    /// Replaces the controller's own hide list (empty list = show every zone again).
    pub hidden_zone_ids: Option<Vec<String>>,
}

/// Drop blanks and duplicates, keeping first-seen order, so the stored list and `config_sha` do
/// not depend on how a client happened to send it.
fn normalize_zone_ids(ids: Vec<String>) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    ids.into_iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty() && seen.insert(id.clone()))
        .collect()
}

/// Summary for listing knobs
#[derive(Debug, Serialize)]
pub struct KnobSummary {
    pub device_type: String,
    pub knob_id: String,
    pub name: String,
    pub last_seen: DateTime<Utc>,
    pub version: Option<String>,
    pub status: KnobStatus,
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[test]
    fn relative_storage_path_supports_new_directory_entries() {
        let directory = tempfile::tempdir_in(".").unwrap();
        let relative = directory
            .path()
            .strip_prefix(std::env::current_dir().unwrap())
            .unwrap();
        assert!(relative.is_relative());
        let store = KnobStore {
            knobs: Arc::new(RwLock::new(HashMap::new())),
            path: Arc::new(relative.join("new/nested/knobs.json")),
            load_failed: false,
        };
        store
            .save_to_disk(&mut HashMap::new(), HashMap::new())
            .unwrap();
        assert_eq!(fs::read(store.path.as_ref()).unwrap(), b"{}");
    }

    #[test]
    fn failed_directory_sync_keeps_memory_aligned_with_installed_file() {
        let directory = tempfile::tempdir().unwrap();
        let store = KnobStore {
            knobs: Arc::new(RwLock::new(HashMap::new())),
            path: Arc::new(directory.path().join("knobs.json")),
            load_failed: false,
        };
        let mut current = HashMap::new();
        let candidate = HashMap::from([(
            "dial".into(),
            Knob {
                device_type: "knob".into(),
                name: "Installed".into(),
                last_seen: Utc::now(),
                version: None,
                config: KnobConfig::default(),
                config_sha: "test".into(),
                status: KnobStatus::default(),
            },
        )]);
        let result = store.save_with_sync(&mut current, candidate, |_| {
            anyhow::bail!("injected post-rename directory sync failure")
        });
        assert!(result.is_err());
        let disk: HashMap<String, Knob> =
            serde_json::from_slice(&fs::read(store.path.as_ref()).unwrap()).unwrap();
        assert_eq!(current["dial"].name, disk["dial"].name);
        assert_eq!(current["dial"].name, "Installed");
    }
}

#[cfg(test)]
mod hidden_zone_tests {
    use super::*;

    #[test]
    fn normalize_drops_blanks_and_duplicates_keeping_first_seen_order() {
        let ids = vec![
            " roon:a ".to_string(),
            "".to_string(),
            "lms:b".to_string(),
            "roon:a".to_string(),
        ];
        assert_eq!(normalize_zone_ids(ids), ["roon:a", "lms:b"]);
    }

    #[test]
    fn empty_hide_list_leaves_config_sha_unchanged() {
        // A controller that never uses per-controller hiding must not be resynced by an upgrade:
        // the empty list is skipped when serialising, so the hash input is what it was before.
        let config = KnobConfig::default();
        let json = serde_json::to_value(&config).unwrap();
        assert!(json.get("hidden_zone_ids").is_none());

        let mut hiding = config.clone();
        hiding.hidden_zone_ids = vec!["roon:a".to_string()];
        assert_ne!(compute_sha(&config, "k"), compute_sha(&hiding, "k"));
    }

    #[test]
    fn config_without_the_field_deserialises_to_an_empty_list() {
        let config: KnobConfig = serde_json::from_str("{}").unwrap();
        assert!(config.hidden_zone_ids.is_empty());
    }
}
