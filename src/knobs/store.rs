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

    // Device-specific settings. Each is omitted from the wire until the owner sets it, and only
    // controllers whose firmware applies it accept it (see `crate::device_settings`), so a
    // controller without these serializes -- and hashes -- exactly as it always did.
    /// Frame: print the controller's IP address on the display.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub show_ip: Option<bool>,
    /// Dial and HALO: backlight percentage.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<u8>,
    /// Kizz: body movement.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub body_enabled: Option<bool>,
    /// Kizz: its own sounds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound_enabled: Option<bool>,
    /// Kizz: loudness of its own sounds, 0 Low to 2 High.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sound_volume: Option<u8>,

    /// A pending install request, present only until the controller reports a new version or the
    /// request expires. It is part of the hash, so the controller refetches its config to see it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firmware_update: Option<FirmwareUpdateRequest>,
}

/// A request, made from the settings page, for a controller to install the available firmware.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FirmwareUpdateRequest {
    /// Always `"install"`; the controller ignores anything else.
    pub action: String,
    /// Version the controller was running when asked. A controller already on another version
    /// treats the request as stale instead of installing again.
    pub from_version: Option<String>,
    pub requested_at: DateTime<Utc>,
}

/// How long an install request stays pending without the controller changing version. Past this
/// the bridge stops offering it so a failed install is not retried forever on every reboot.
const FIRMWARE_REQUEST_TTL_MINUTES: i64 = 30;

impl FirmwareUpdateRequest {
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        now - self.requested_at > chrono::Duration::minutes(FIRMWARE_REQUEST_TTL_MINUTES)
    }
}

/// A requested setting the controller does not have, or a value outside its range.
#[derive(Debug)]
pub struct SettingRejected(pub String);

impl std::fmt::Display for SettingRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for SettingRejected {}

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
            show_ip: None,
            brightness: None,
            body_enabled: None,
            sound_enabled: None,
            sound_volume: None,
            firmware_update: None,
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
            // An install request ends when the controller reports another version (it took the
            // update) or when it has gone unanswered for too long. Either way the hash moves so
            // the controller refetches a config without it.
            let request_done = knob.config.firmware_update.as_ref().is_some_and(|request| {
                request.is_expired(Utc::now())
                    || request
                        .from_version
                        .as_ref()
                        .is_some_and(|from| Some(from) != knob.version.as_ref())
            });
            if request_done {
                knob.config.firmware_update = None;
                knob.config_sha = compute_sha(&knob.config, &knob.name);
            }
            let result = knob.clone();
            let identity_changed =
                request_done || old_version != knob.version || old_identity != knob.device_type;
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

        // Device-specific settings. Every value is checked against this controller's own
        // schema before anything is stored; a bad field rejects the whole update.
        let target = crate::firmware_catalog::FirmwareTarget::parse(&knob.device_type);
        let reject = |message: String| anyhow::Error::new(SettingRejected(message));
        let target_for = |key: &str| {
            target.ok_or_else(|| reject(format!("unknown controller type for \"{key}\"")))
        };
        if let Some(v) = updates.show_ip {
            crate::device_settings::check_toggle(target_for("show_ip")?, "show_ip")
                .map_err(reject)?;
            knob.config.show_ip = Some(v);
        }
        if let Some(v) = updates.body_enabled {
            crate::device_settings::check_toggle(target_for("body_enabled")?, "body_enabled")
                .map_err(reject)?;
            knob.config.body_enabled = Some(v);
        }
        if let Some(v) = updates.sound_enabled {
            crate::device_settings::check_toggle(target_for("sound_enabled")?, "sound_enabled")
                .map_err(reject)?;
            knob.config.sound_enabled = Some(v);
        }
        if let Some(v) = updates.brightness {
            crate::device_settings::check_number(target_for("brightness")?, "brightness", v)
                .map_err(reject)?;
            knob.config.brightness = u8::try_from(v).ok();
        }
        if let Some(v) = updates.sound_volume {
            crate::device_settings::check_number(target_for("sound_volume")?, "sound_volume", v)
                .map_err(reject)?;
            knob.config.sound_volume = u8::try_from(v).ok();
        }
        match updates.firmware_update.as_deref() {
            None => {}
            Some("install") => {
                let Some(version) = knob.version.clone() else {
                    return Err(reject(
                        "the controller has not reported its firmware version yet".into(),
                    ));
                };
                knob.config.firmware_update = Some(FirmwareUpdateRequest {
                    action: "install".into(),
                    from_version: Some(version),
                    requested_at: Utc::now(),
                });
            }
            Some("cancel") => knob.config.firmware_update = None,
            Some(other) => {
                return Err(reject(format!("unknown firmware_update action \"{other}\"")))
            }
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
    /// Device-specific settings; see `crate::device_settings` for who has which. Numbers are
    /// signed here so a negative request is rejected instead of wrapping.
    pub show_ip: Option<bool>,
    pub brightness: Option<i64>,
    pub body_enabled: Option<bool>,
    pub sound_enabled: Option<bool>,
    pub sound_volume: Option<i64>,
    /// `"install"` asks the controller to install the available firmware; `"cancel"` withdraws
    /// the request.
    pub firmware_update: Option<String>,
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
mod device_setting_tests {
    use super::*;
    use crate::device_settings::ALL_KEYS;

    fn store() -> (tempfile::TempDir, KnobStore) {
        let directory = tempfile::tempdir().unwrap();
        let store = KnobStore {
            knobs: Arc::new(RwLock::new(HashMap::new())),
            path: Arc::new(directory.path().join("knobs.json")),
            load_failed: false,
        };
        (directory, store)
    }

    async fn register(store: &KnobStore, id: &str, device: &str, version: &str) {
        store
            .get_or_create_with_device_type(id, Some(version), Some(device))
            .await
            .unwrap();
    }

    fn update(f: impl FnOnce(&mut KnobConfigUpdate)) -> KnobConfigUpdate {
        let mut update = KnobConfigUpdate::default();
        f(&mut update);
        update
    }

    #[test]
    fn a_default_config_puts_nothing_new_on_the_wire() {
        // Installed controllers and Home Assistant parse this payload, and the config hash covers
        // it: a controller that has none of the new settings must serialize exactly as before.
        let json = serde_json::to_value(KnobConfig::default()).unwrap();
        for key in ALL_KEYS.iter().chain(&["firmware_update"]) {
            assert!(json.get(key).is_none(), "{key} leaked into the default config");
        }
    }

    #[tokio::test]
    async fn a_setting_is_stored_and_changes_the_hash_so_the_device_refetches() {
        let (_dir, store) = store();
        register(&store, "d1", "dial", "2.8.0-alpha.5").await;
        let before = store.get("d1").await.unwrap().config_sha;
        let knob = store
            .update_config("d1", update(|u| u.brightness = Some(40)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(knob.config.brightness, Some(40));
        assert_ne!(knob.config_sha, before);
        let json = serde_json::to_value(&knob.config).unwrap();
        assert_eq!(json["brightness"], 40);
    }

    #[tokio::test]
    async fn a_controller_cannot_be_given_a_setting_it_does_not_have() {
        let (_dir, store) = store();
        register(&store, "f1", "frame", "2.8.0-alpha.5").await;
        let error = store
            .update_config("f1", update(|u| u.brightness = Some(40)))
            .await
            .unwrap_err();
        assert!(error.downcast_ref::<SettingRejected>().is_some(), "{error}");
        assert_eq!(store.get("f1").await.unwrap().config.brightness, None);
        let accepted = store
            .update_config("f1", update(|u| u.show_ip = Some(true)))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(accepted.config.show_ip, Some(true));
    }

    #[tokio::test]
    async fn out_of_range_values_never_reach_the_stored_config() {
        let (_dir, store) = store();
        register(&store, "d1", "dial", "2.8.0-alpha.5").await;
        for bad in [0, 4, 101, -1, 100_000] {
            let error = store
                .update_config("d1", update(|u| u.brightness = Some(bad)))
                .await
                .unwrap_err();
            assert!(error.downcast_ref::<SettingRejected>().is_some(), "{bad}");
        }
        register(&store, "s1", "stackchan", "2.8.0-alpha.5").await;
        assert!(store
            .update_config("s1", update(|u| u.sound_volume = Some(3)))
            .await
            .is_err());
        assert_eq!(store.get("d1").await.unwrap().config.brightness, None);
    }

    #[tokio::test]
    async fn one_bad_field_rejects_the_whole_update() {
        let (_dir, store) = store();
        register(&store, "d1", "dial", "2.8.0-alpha.5").await;
        let result = store
            .update_config(
                "d1",
                update(|u| {
                    u.name = Some("Kitchen".into());
                    u.brightness = Some(500);
                }),
            )
            .await;
        assert!(result.is_err());
        assert_eq!(store.get("d1").await.unwrap().name, "");
    }

    #[tokio::test]
    async fn an_install_request_remembers_the_version_it_was_made_against() {
        let (_dir, store) = store();
        register(&store, "f1", "frame", "2.8.0-alpha.4").await;
        let before = store.get("f1").await.unwrap().config_sha;
        let knob = store
            .update_config("f1", update(|u| u.firmware_update = Some("install".into())))
            .await
            .unwrap()
            .unwrap();
        let request = knob.config.firmware_update.clone().unwrap();
        assert_eq!(request.action, "install");
        assert_eq!(request.from_version.as_deref(), Some("2.8.0-alpha.4"));
        assert_ne!(knob.config_sha, before, "the device must see a changed hash");
        let json = serde_json::to_value(&knob.config).unwrap();
        assert_eq!(json["firmware_update"]["action"], "install");
        assert_eq!(json["firmware_update"]["from_version"], "2.8.0-alpha.4");
    }

    #[tokio::test]
    async fn an_install_request_needs_a_known_version_and_a_known_action() {
        let (_dir, store) = store();
        store
            .get_or_create_with_device_type("f1", None, Some("frame"))
            .await
            .unwrap();
        assert!(store
            .update_config("f1", update(|u| u.firmware_update = Some("install".into())))
            .await
            .is_err());
        register(&store, "f2", "frame", "2.8.0-alpha.4").await;
        assert!(store
            .update_config("f2", update(|u| u.firmware_update = Some("reboot".into())))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn cancelling_removes_the_request() {
        let (_dir, store) = store();
        register(&store, "f1", "frame", "2.8.0-alpha.4").await;
        store
            .update_config("f1", update(|u| u.firmware_update = Some("install".into())))
            .await
            .unwrap();
        let knob = store
            .update_config("f1", update(|u| u.firmware_update = Some("cancel".into())))
            .await
            .unwrap()
            .unwrap();
        assert!(knob.config.firmware_update.is_none());
    }

    #[tokio::test]
    async fn the_request_ends_when_the_device_reports_a_different_version() {
        let (_dir, store) = store();
        register(&store, "f1", "frame", "2.8.0-alpha.4").await;
        store
            .update_config("f1", update(|u| u.firmware_update = Some("install".into())))
            .await
            .unwrap();
        let pending = store.get("f1").await.unwrap();
        // Same version polling again keeps the request (and the hash) steady.
        register(&store, "f1", "frame", "2.8.0-alpha.4").await;
        let still = store.get("f1").await.unwrap();
        assert!(still.config.firmware_update.is_some());
        assert_eq!(still.config_sha, pending.config_sha);
        // The device came back on the new image: the request is done and the hash moves so the
        // device refetches a config without it.
        register(&store, "f1", "frame", "2.8.0-alpha.5").await;
        let done = store.get("f1").await.unwrap();
        assert!(done.config.firmware_update.is_none());
        assert_ne!(done.config_sha, pending.config_sha);
    }

    #[test]
    fn an_unanswered_request_expires() {
        let now = Utc::now();
        let request = |age_minutes| FirmwareUpdateRequest {
            action: "install".into(),
            from_version: Some("2.8.0-alpha.4".into()),
            requested_at: now - chrono::Duration::minutes(age_minutes),
        };
        assert!(!request(5).is_expired(now));
        assert!(request(31).is_expired(now));
    }
}
