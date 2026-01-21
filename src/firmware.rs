//! Firmware service - Auto-fetch firmware from GitHub
//!
//! Polls GitHub releases for new knob firmware and downloads automatically.

use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use crate::config::get_config_dir;

const DEFAULT_POLL_INTERVAL_MINUTES: u64 = 60;
const GITHUB_REPO: &str = "muness/roon-knob";
const FIRMWARE_FILENAME: &str = "roon_knob.bin";

/// Firmware version info stored in version.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareVersion {
    pub version: String,
    pub file: String,
    pub fetched_at: String,
    pub release_url: Option<String>,
}

/// GitHub release asset
#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// GitHub release response
#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
}

/// Firmware service state
#[derive(Default)]
struct FirmwareState {
    current_version: Option<String>,
    latest_version: Option<String>,
    checking: bool,
}

/// Firmware service
pub struct FirmwareService {
    client: Client,
    state: Arc<RwLock<FirmwareState>>,
    shutdown: CancellationToken,
}

impl Default for FirmwareService {
    fn default() -> Self {
        Self::new()
    }
}

impl FirmwareService {
    pub fn new() -> Self {
        #[allow(clippy::expect_used)] // HTTP client creation only fails if TLS setup fails
        let client = Client::builder()
            .user_agent("unified-hifi-control")
            .timeout(Duration::from_secs(30))
            .build()
            .expect("Failed to create HTTP client");

        Self {
            client,
            state: Arc::new(RwLock::new(FirmwareState::default())),
            shutdown: CancellationToken::new(),
        }
    }

    /// Stop the firmware polling service
    pub fn stop(&self) {
        self.shutdown.cancel();
        tracing::info!("Firmware service stopped");
    }

    /// Get firmware directory path
    fn firmware_dir() -> PathBuf {
        get_config_dir().join("firmware")
    }

    /// Get current installed version from version.json
    pub fn get_current_version() -> Option<String> {
        let version_path = Self::firmware_dir().join("version.json");
        if version_path.exists() {
            std::fs::read_to_string(&version_path)
                .ok()
                .and_then(|s| serde_json::from_str::<FirmwareVersion>(&s).ok())
                .map(|v| v.version)
        } else {
            None
        }
    }

    /// Fetch latest release info from GitHub
    async fn fetch_latest_release(&self) -> Result<Option<GitHubRelease>> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/latest",
            GITHUB_REPO
        );
        let response = self.client.get(&url).send().await?;

        if response.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }

        if response.status() == reqwest::StatusCode::FORBIDDEN {
            return Err(anyhow!("GitHub API rate limit exceeded"));
        }

        if !response.status().is_success() {
            return Err(anyhow!("GitHub API error: {}", response.status()));
        }

        let release: GitHubRelease = response.json().await?;
        Ok(Some(release))
    }

    /// Download firmware from GitHub release
    async fn download_firmware(
        &self,
        asset: &GitHubAsset,
        version: &str,
        release_url: &str,
    ) -> Result<()> {
        let fw_dir = Self::firmware_dir();
        std::fs::create_dir_all(&fw_dir)?;

        let firmware_path = fw_dir.join(FIRMWARE_FILENAME);
        let temp_path = fw_dir.join(format!("{}.tmp", FIRMWARE_FILENAME));

        tracing::info!(
            "Downloading firmware v{} from {}",
            version,
            asset.browser_download_url
        );

        // Download to temp file
        let response = self.client.get(&asset.browser_download_url).send().await?;
        if !response.status().is_success() {
            return Err(anyhow!(
                "Failed to download firmware: {}",
                response.status()
            ));
        }

        let bytes = response.bytes().await?;
        std::fs::write(&temp_path, &bytes)?;

        // Rename temp to final
        std::fs::rename(&temp_path, &firmware_path)?;

        // Write version.json
        let version_info = FirmwareVersion {
            version: version.to_string(),
            file: FIRMWARE_FILENAME.to_string(),
            fetched_at: chrono::Utc::now().to_rfc3339(),
            release_url: Some(release_url.to_string()),
        };

        let version_path = fw_dir.join("version.json");
        std::fs::write(&version_path, serde_json::to_string_pretty(&version_info)?)?;

        let size = std::fs::metadata(&firmware_path)?.len();
        tracing::info!(
            "Firmware v{} downloaded successfully ({} bytes)",
            version,
            size
        );

        Ok(())
    }

    /// Compare versions (returns true if remote > local)
    fn is_newer_version(remote: &str, local: &str) -> bool {
        let parse = |v: &str| -> Vec<u32> {
            v.trim_start_matches('v')
                .split('-')
                .next()
                .unwrap_or("")
                .split('.')
                .filter_map(|s| s.parse().ok())
                .collect()
        };

        let remote_parts = parse(remote);
        let local_parts = parse(local);

        for i in 0..3 {
            let r = remote_parts.get(i).unwrap_or(&0);
            let l = local_parts.get(i).unwrap_or(&0);
            if r > l {
                return true;
            }
            if r < l {
                return false;
            }
        }
        false
    }

    /// Check for updates and download if available
    pub async fn check_for_updates(&self) -> Result<bool> {
        {
            let mut state = self.state.write().await;
            if state.checking {
                return Ok(false);
            }
            state.checking = true;
        }

        let result = async {
            let release = match self.fetch_latest_release().await? {
                Some(r) => r,
                None => {
                    tracing::debug!("No releases found on GitHub");
                    return Ok(false);
                }
            };

            let latest_version = release.tag_name.trim_start_matches('v').to_string();
            let current_version = Self::get_current_version();

            let needs_update = match &current_version {
                Some(cv) => Self::is_newer_version(&latest_version, cv),
                None => true,
            };

            {
                let mut state = self.state.write().await;
                state.latest_version = Some(latest_version.clone());
                state.current_version = current_version.clone();
            }

            if !needs_update {
                tracing::debug!(
                    "Firmware is up to date (v{})",
                    current_version.unwrap_or_default()
                );
                return Ok(false);
            }

            // Find firmware asset
            let asset = release
                .assets
                .iter()
                .find(|a| a.name == FIRMWARE_FILENAME)
                .ok_or_else(|| anyhow!("Firmware asset not found in release"))?;

            tracing::info!(
                "New firmware available: v{} (current: {})",
                latest_version,
                current_version.as_deref().unwrap_or("none")
            );

            self.download_firmware(asset, &latest_version, &release.html_url)
                .await?;
            Ok(true)
        }
        .await;

        {
            let mut state = self.state.write().await;
            state.checking = false;
        }

        result
    }

    /// Start periodic polling
    pub fn start_polling(self: Arc<Self>, poll_interval_minutes: u64) {
        let interval_mins = if poll_interval_minutes > 0 {
            poll_interval_minutes
        } else {
            DEFAULT_POLL_INTERVAL_MINUTES
        };

        let shutdown = self.shutdown.clone();
        tokio::spawn(async move {
            // Check immediately on startup
            if let Err(e) = self.check_for_updates().await {
                tracing::warn!("Initial firmware check failed: {}", e);
            }

            // Then poll periodically
            let mut ticker = interval(Duration::from_secs(interval_mins * 60));
            ticker.tick().await; // Skip first tick (we already checked)

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::debug!("Firmware polling shutdown requested");
                        break;
                    }
                    _ = ticker.tick() => {
                        if let Err(e) = self.check_for_updates().await {
                            tracing::warn!("Firmware check failed: {}", e);
                        }
                    }
                }
            }
        });
    }
}
