//! Firmware service - Auto-fetch firmware from GitHub
//!
//! Polls GitHub releases for new knob firmware and downloads automatically.

pub use crate::firmware_catalog as catalog;

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
        let firmware_path =
            publish_firmware_with_checkpoint(&fw_dir, &bytes, version, release_url, || {})?;
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

// Polling and manual fetch create separate service instances. Serialize publication,
// not networking, so their metadata switches cannot interleave or downgrade the cache.
static PUBLICATION_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A unique owned temporary file cleans up on every error path. create_new prevents
/// sharing a staging filename between concurrent fetches; UUIDs do not name public files.
struct TemporaryFirmwareFile {
    path: PathBuf,
}

impl TemporaryFirmwareFile {
    fn create(directory: &std::path::Path, bytes: &[u8]) -> Result<Self> {
        use std::io::Write;
        let path = directory.join(format!(".firmware-{}.tmp", uuid::Uuid::new_v4()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        let temporary = Self { path };
        let result = file.write_all(bytes).and_then(|_| file.sync_all());
        drop(file);
        result?;
        Ok(temporary)
    }
}

impl Drop for TemporaryFirmwareFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn publish_firmware_with_checkpoint(
    directory: &std::path::Path,
    bytes: &[u8],
    version: &str,
    release_url: &str,
    before_metadata: impl FnOnce(),
) -> Result<PathBuf> {
    let parts: Vec<_> = version.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()))
    {
        return Err(anyhow!("Invalid stable firmware version"));
    }
    if bytes.is_empty() {
        return Err(anyhow!("Empty firmware image"));
    }
    let _publication = PUBLICATION_LOCK
        .lock()
        .map_err(|_| anyhow!("Firmware publication lock poisoned"))?;
    std::fs::create_dir_all(directory)?;
    if !std::fs::symlink_metadata(directory)?.is_dir() {
        return Err(anyhow!("Firmware directory is not a directory"));
    }
    let metadata_path = directory.join("version.json");
    if std::fs::symlink_metadata(&metadata_path).is_ok_and(|metadata| metadata.is_file()) {
        if let Ok(previous) = std::fs::read(&metadata_path).and_then(|bytes| {
            serde_json::from_slice::<FirmwareVersion>(&bytes).map_err(std::io::Error::other)
        }) {
            if FirmwareService::is_newer_version(&previous.version, version) {
                return Err(anyhow!("Refusing to replace newer firmware metadata"));
            }
        }
    }
    let filename = format!("roon_knob_v{version}.bin");
    let path = directory.join(&filename);
    let temporary = TemporaryFirmwareFile::create(directory, bytes)?;
    // hard_link publishes a complete file without replacing an existing version.
    // A repeated download is idempotent only when the published bytes match exactly.
    match std::fs::hard_link(&temporary.path, &path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            if !std::fs::symlink_metadata(&path)?.is_file() || std::fs::read(&path)? != bytes {
                return Err(anyhow!("Published firmware version has different bytes"));
            }
        }
        Err(error) => return Err(error.into()),
    }
    before_metadata();
    let info = FirmwareVersion {
        version: version.to_string(),
        file: filename,
        fetched_at: chrono::Utc::now().to_rfc3339(),
        release_url: Some(release_url.to_string()),
    };
    let metadata = TemporaryFirmwareFile::create(directory, &serde_json::to_vec_pretty(&info)?)?;
    std::fs::rename(&metadata.path, metadata_path)?;
    Ok(path)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod publication_tests {
    use super::*;

    fn current(directory: &std::path::Path) -> FirmwareVersion {
        serde_json::from_slice(&std::fs::read(directory.join("version.json")).unwrap()).unwrap()
    }

    #[test]
    fn reader_during_publication_sees_old_consistent_bundle_until_metadata_switches() {
        let directory = tempfile::tempdir().unwrap();
        publish_firmware_with_checkpoint(directory.path(), b"old bytes", "2.5.2", "release", || {})
            .unwrap();
        publish_firmware_with_checkpoint(
            directory.path(),
            b"new bytes",
            "2.5.3",
            "release",
            || {
                let metadata = current(directory.path());
                assert_eq!(metadata.version, "2.5.2");
                assert_eq!(
                    std::fs::read(directory.path().join(metadata.file)).unwrap(),
                    b"old bytes"
                );
            },
        )
        .unwrap();
        let metadata = current(directory.path());
        assert_eq!(metadata.version, "2.5.3");
        assert_eq!(
            std::fs::read(directory.path().join(metadata.file)).unwrap(),
            b"new bytes"
        );
    }

    #[test]
    fn invalid_version_cannot_name_an_artifact_or_replace_previous_metadata() {
        let directory = tempfile::tempdir().unwrap();
        publish_firmware_with_checkpoint(directory.path(), b"old", "2.5.2", "release", || {})
            .unwrap();
        for invalid in ["../../escape", "", "alpha", "2.5.3/evil", "2.5.3-alpha.1"] {
            assert!(publish_firmware_with_checkpoint(
                directory.path(),
                b"bad",
                invalid,
                "release",
                || {}
            )
            .is_err());
            assert_eq!(current(directory.path()).version, "2.5.2");
        }
    }

    #[test]
    fn same_version_cannot_overwrite_published_bytes() {
        let directory = tempfile::tempdir().unwrap();
        publish_firmware_with_checkpoint(directory.path(), b"original", "2.5.2", "release", || {})
            .unwrap();
        assert!(publish_firmware_with_checkpoint(
            directory.path(),
            b"replacement",
            "2.5.2",
            "release",
            || {}
        )
        .is_err());
        let metadata = current(directory.path());
        assert_eq!(
            std::fs::read(directory.path().join(metadata.file)).unwrap(),
            b"original"
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod publication_isolation_tests {
    use super::*;
    #[test]
    fn temporary_files_are_unique_and_cleaned_up() {
        let directory = tempfile::tempdir().unwrap();
        let first = TemporaryFirmwareFile::create(directory.path(), b"one").unwrap();
        let second = TemporaryFirmwareFile::create(directory.path(), b"two").unwrap();
        assert_ne!(first.path, second.path);
        assert_eq!(std::fs::read(&first.path).unwrap(), b"one");
        assert_eq!(std::fs::read(&second.path).unwrap(), b"two");
        let paths = [first.path.clone(), second.path.clone()];
        drop(first);
        drop(second);
        assert!(paths.iter().all(|path| !path.exists()));
    }

    #[test]
    fn older_concurrent_fetch_cannot_downgrade_newer_publication() {
        let directory = tempfile::tempdir().unwrap();
        publish_firmware_with_checkpoint(directory.path(), b"new", "2.5.3", "release", || {})
            .unwrap();
        assert!(publish_firmware_with_checkpoint(
            directory.path(),
            b"old",
            "2.5.2",
            "release",
            || {}
        )
        .is_err());
        let metadata: FirmwareVersion =
            serde_json::from_slice(&std::fs::read(directory.path().join("version.json")).unwrap())
                .unwrap();
        assert_eq!(metadata.version, "2.5.3");
        assert_eq!(
            std::fs::read(directory.path().join(metadata.file)).unwrap(),
            b"new"
        );
    }

    #[test]
    fn identical_version_retry_is_safe_but_empty_image_is_rejected() {
        let directory = tempfile::tempdir().unwrap();
        let first =
            publish_firmware_with_checkpoint(directory.path(), b"valid", "2.5.2", "release", || {})
                .unwrap();
        let second =
            publish_firmware_with_checkpoint(directory.path(), b"valid", "2.5.2", "release", || {})
                .unwrap();
        assert_eq!(first, second);
        assert!(
            publish_firmware_with_checkpoint(directory.path(), b"", "2.5.3", "release", || {})
                .is_err()
        );
        assert!(!directory.path().join("roon_knob_v2.5.3.bin").exists());
    }
}
