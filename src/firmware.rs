//! Firmware service - Auto-fetch firmware from GitHub
//!
//! Polls GitHub releases for new knob firmware and downloads automatically.

pub use crate::firmware_catalog as catalog;
use catalog::{compare_firmware_versions, FirmwareChannel, FirmwareTarget};

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

/// Firmware version info stored in version.json
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirmwareVersion {
    pub version: String,
    pub file: String,
    pub fetched_at: String,
    pub release_url: Option<String>,
}

/// GitHub release asset
#[derive(Debug, Clone, Deserialize)]
struct GitHubAsset {
    name: String,
    browser_download_url: String,
}

/// GitHub release response
#[derive(Debug, Clone, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    html_url: String,
    assets: Vec<GitHubAsset>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Firmware service state
#[derive(Default)]
struct FirmwareState {
    checking: bool,
}

/// Firmware service
pub struct FirmwareService {
    client: Client,
    state: Arc<RwLock<FirmwareState>>,
    shutdown: CancellationToken,
    api_root: String,
    cache_root: PathBuf,
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
            api_root: format!("https://api.github.com/repos/{GITHUB_REPO}"),
            cache_root: Self::firmware_dir(),
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

    /// Legacy stable version remains the default admin projection.
    pub fn get_current_version() -> Option<String> {
        cached_version(&Self::firmware_dir(), FirmwareTarget::LegacyKnob)
    }

    async fn fetch_releases(&self) -> Result<Vec<GitHubRelease>> {
        let mut releases = Vec::new();
        let mut page = 1usize;
        loop {
            let response = self
                .client
                .get(format!(
                    "{}/releases?per_page=100&page={page}",
                    self.api_root
                ))
                .send()
                .await?;
            if response.status() == reqwest::StatusCode::NOT_FOUND {
                return Ok(releases);
            }
            if !response.status().is_success() {
                return Err(anyhow!("GitHub releases API error: {}", response.status()));
            }
            let batch: Vec<GitHubRelease> = response.json().await?;
            let more = batch.len() == 100;
            releases.extend(batch);
            if !more {
                return Ok(releases);
            }
            page += 1;
        }
    }

    /// Download firmware from GitHub release
    async fn download_firmware(
        &self,
        asset: &GitHubAsset,
        version: &str,
        release_url: &str,
        target: FirmwareTarget,
        channel: FirmwareChannel,
    ) -> Result<()> {
        if !channel.matches_version(version) {
            return Err(anyhow!("Release channel/version mismatch"));
        }
        let fw_dir = target.cache_directory(&self.cache_root, channel);
        ensure_cache_directory(&self.cache_root, target, channel)?;
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
            publish_target_with_checkpoint(&fw_dir, &bytes, version, release_url, target, || {})?;
        let size = std::fs::metadata(&firmware_path)?.len();
        tracing::info!(
            "Firmware v{} downloaded successfully ({} bytes)",
            version,
            size
        );

        Ok(())
    }

    fn is_newer_version(remote: &str, local: &str) -> bool {
        compare_firmware_versions(remote, local).is_some_and(|order| order.is_gt())
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
            let releases = self.fetch_releases().await?;
            let mut updated = false;
            let mut failures = Vec::new();
            for channel in FirmwareChannel::ALL {
                for target in FirmwareTarget::ALL {
                    let Some((release, asset)) = select_release(&releases, *target, *channel)
                    else {
                        continue;
                    };
                    let version = release.tag_name.trim_start_matches('v');
                    let directory = target.cache_directory(&self.cache_root, *channel);
                    if let Err(error) = ensure_cache_directory(&self.cache_root, *target, *channel)
                    {
                        failures.push(format!("{}/{}: {error}", channel.slug(), target.slug()));
                        continue;
                    }
                    let current = cached_version(&directory, *target)
                        .filter(|version| channel.matches_version(version));
                    if current
                        .as_deref()
                        .is_some_and(|current| !Self::is_newer_version(version, current))
                    {
                        continue;
                    }
                    match self
                        .download_firmware(asset, version, &release.html_url, *target, *channel)
                        .await
                    {
                        Ok(()) => updated = true,
                        Err(error) => {
                            failures.push(format!("{}/{}: {error}", channel.slug(), target.slug()))
                        }
                    }
                }
            }
            if !failures.is_empty() {
                return Err(anyhow!(
                    "Firmware cache refresh partially failed: {}",
                    failures.join("; ")
                ));
            }
            Ok(updated)
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

#[cfg(test)]
fn publish_firmware_with_checkpoint(
    directory: &std::path::Path,
    bytes: &[u8],
    version: &str,
    release_url: &str,
    before_metadata: impl FnOnce(),
) -> Result<PathBuf> {
    if !FirmwareChannel::Stable.matches_version(version) {
        return Err(anyhow!("Invalid stable firmware version"));
    }
    publish_target_with_checkpoint(
        directory,
        bytes,
        version,
        release_url,
        FirmwareTarget::LegacyKnob,
        before_metadata,
    )
}

fn publish_target_with_checkpoint(
    directory: &std::path::Path,
    bytes: &[u8],
    version: &str,
    release_url: &str,
    target: FirmwareTarget,
    before_metadata: impl FnOnce(),
) -> Result<PathBuf> {
    if compare_firmware_versions(version, version).is_none() {
        return Err(anyhow!("Invalid firmware version"));
    }
    let filename = format!(
        "{}_v{version}.bin",
        target.application_file().trim_end_matches(".bin")
    );
    if !target.valid_application_filename(&filename) {
        return Err(anyhow!("Invalid firmware version"));
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
    if let Some(previous) = cached_version(directory, target) {
        if FirmwareChannel::ALL
            .iter()
            .any(|channel| channel.matches_version(&previous) && channel.matches_version(version))
            && FirmwareService::is_newer_version(&previous, version)
        {
            return Err(anyhow!("Refusing to replace newer firmware metadata"));
        }
    }
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

fn select_release(
    releases: &[GitHubRelease],
    target: FirmwareTarget,
    channel: FirmwareChannel,
) -> Option<(&GitHubRelease, &GitHubAsset)> {
    let filename = target.application_file();
    releases
        .iter()
        .filter(|release| {
            !release.draft
                && release.prerelease == (channel != FirmwareChannel::Stable)
                && channel.matches_version(release.tag_name.trim_start_matches('v'))
        })
        .filter_map(|release| {
            release
                .assets
                .iter()
                .find(|asset| asset.name == filename)
                .map(|asset| (release, asset))
        })
        .max_by(|(left, _), (right, _)| {
            compare_firmware_versions(&left.tag_name, &right.tag_name)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

fn cached_version(directory: &std::path::Path, target: FirmwareTarget) -> Option<String> {
    if !std::fs::symlink_metadata(directory).ok()?.is_dir() {
        return None;
    }
    let metadata_path = directory.join("version.json");
    if !std::fs::symlink_metadata(&metadata_path).ok()?.is_file() {
        return None;
    }
    let info: FirmwareVersion = serde_json::from_slice(&std::fs::read(metadata_path).ok()?).ok()?;
    if !target.valid_application_filename(&info.file) {
        return None;
    }
    if target
        .filename_version(&info.file)
        .is_some_and(|named| named != info.version)
    {
        return None;
    }
    let image = std::fs::symlink_metadata(directory.join(info.file)).ok()?;
    if !image.is_file() || image.len() == 0 {
        return None;
    }
    Some(info.version)
}

fn ensure_cache_directory(
    root: &std::path::Path,
    target: FirmwareTarget,
    channel: FirmwareChannel,
) -> Result<()> {
    let mut paths = vec![root.to_path_buf()];
    if target != FirmwareTarget::LegacyKnob || channel != FirmwareChannel::Stable {
        paths.push(root.join(channel.slug()));
        paths.push(target.cache_directory(root, channel));
    }
    for path in paths {
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return Err(anyhow!("Firmware cache directory is not a real directory")),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir_all(&path)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod multi_device_upstream_tests {
    use super::*;
    use axum::{extract::Path, routing::get, Json, Router};

    fn release(
        tag: &str,
        prerelease: bool,
        draft: bool,
        assets: &[(&str, &str)],
        root: &str,
    ) -> serde_json::Value {
        serde_json::json!({"tag_name":tag,"html_url":"release","prerelease":prerelease,"draft":draft,"assets":assets.iter().map(|(name,path)| serde_json::json!({"name":name,"browser_download_url":format!("{root}/assets/{path}")})).collect::<Vec<_>>()})
    }

    #[tokio::test]
    async fn manual_and_automatic_refresh_select_exact_assets_best_versions_and_isolated_channels()
    {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let root = format!("http://{}", listener.local_addr().unwrap());
        let releases = vec![
            release(
                "v9.0.0",
                false,
                true,
                &[("hiphi_frame.bin", "draft")],
                &root,
            ),
            release(
                "v3.0.0-alpha.2",
                true,
                false,
                &[("hiphi_frame.bin", "alpha2")],
                &root,
            ),
            release(
                "v2.5.2",
                false,
                false,
                &[("roon_knob.bin", "legacy")],
                &root,
            ),
            release(
                "v3.0.0-beta.2",
                true,
                false,
                &[("hiphi_tough.bin", "beta2")],
                &root,
            ),
            release(
                "v3.0.0-alpha.10",
                true,
                false,
                &[("hiphi_frame.bin", "alpha10")],
                &root,
            ),
            release(
                "v4.0.0",
                false,
                false,
                &[("hiphi_frame_merged.bin", "merged-only")],
                &root,
            ),
            release(
                "v3.0.0",
                false,
                false,
                &[("hiphi_frame.bin", "stable")],
                &root,
            ),
            release(
                "v3.0.0-nightly.99",
                true,
                false,
                &[("hiphi_frame.bin", "nightly")],
                &root,
            ),
        ];
        let requests = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let observed = requests.clone();
        let published = Arc::new(std::sync::Mutex::new(releases));
        let upstream = published.clone();
        let app = Router::new()
            .route(
                "/releases",
                get(move || {
                    let releases = upstream.lock().unwrap().clone();
                    async move { Json(releases) }
                }),
            )
            .route(
                "/assets/{file}",
                get(move |Path(file): Path<String>| {
                    let observed = observed.clone();
                    async move {
                        observed.lock().unwrap().push(file.clone());
                        (
                            if file == "broken" {
                                axum::http::StatusCode::NOT_FOUND
                            } else {
                                axum::http::StatusCode::OK
                            },
                            file,
                        )
                    }
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let mut manual = FirmwareService::new();
        manual.api_root = root.clone();
        manual.cache_root = directory.path().join("firmware");
        assert!(manual.check_for_updates().await.unwrap());
        let image = |target: FirmwareTarget, channel: FirmwareChannel| {
            let path = target.cache_directory(&manual.cache_root, channel);
            let metadata: FirmwareVersion =
                serde_json::from_slice(&std::fs::read(path.join("version.json")).unwrap()).unwrap();
            std::fs::read(path.join(metadata.file)).unwrap()
        };
        assert_eq!(
            image(FirmwareTarget::LegacyKnob, FirmwareChannel::Stable),
            b"legacy"
        );
        assert_eq!(
            image(FirmwareTarget::Frame, FirmwareChannel::Stable),
            b"stable"
        );
        assert_eq!(
            image(FirmwareTarget::Frame, FirmwareChannel::Alpha),
            b"alpha10"
        );
        assert_eq!(
            image(FirmwareTarget::Tough, FirmwareChannel::Beta),
            b"beta2"
        );
        assert!(!FirmwareTarget::Frame
            .cache_directory(&manual.cache_root, FirmwareChannel::Beta)
            .exists());
        assert!(
            !manual.check_for_updates().await.unwrap(),
            "an unchanged release should not redownload"
        );
        assert_eq!(requests.lock().unwrap().len(), 4);
        assert!(!requests.lock().unwrap().iter().any(|file| [
            "draft",
            "nightly",
            "merged-only",
            "alpha2"
        ]
        .contains(&file.as_str())));

        let original = published.lock().unwrap().clone();
        published.lock().unwrap().push(release(
            "v3.1.0",
            false,
            false,
            &[
                ("hiphi_frame.bin", "broken"),
                ("hiphi_tough.bin", "stable-tough"),
            ],
            &root,
        ));
        assert!(
            manual.check_for_updates().await.is_err(),
            "one failed family must be reported"
        );
        assert_eq!(
            image(FirmwareTarget::Frame, FirmwareChannel::Stable),
            b"stable",
            "failed download retains previous bytes"
        );
        assert_eq!(
            image(FirmwareTarget::Tough, FirmwareChannel::Stable),
            b"stable-tough",
            "other families must still refresh"
        );
        *published.lock().unwrap() = original;

        let automatic_directory = tempfile::tempdir().unwrap();
        let mut automatic = FirmwareService::new();
        automatic.api_root = root;
        automatic.cache_root = automatic_directory.path().join("firmware");
        let final_cache = FirmwareTarget::Frame
            .cache_directory(&automatic.cache_root, FirmwareChannel::Alpha)
            .join("version.json");
        let automatic = Arc::new(automatic);
        automatic.clone().start_polling(60);
        tokio::time::timeout(Duration::from_secs(5), async {
            while !final_cache.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        automatic.stop();
        let metadata: FirmwareVersion =
            serde_json::from_slice(&std::fs::read(&final_cache).unwrap()).unwrap();
        assert_eq!(metadata.version, "3.0.0-alpha.10");
        assert_eq!(
            std::fs::read(final_cache.parent().unwrap().join(metadata.file)).unwrap(),
            b"alpha10"
        );
        server.abort();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod prerelease_publication_tests {
    use super::*;
    #[test]
    fn older_alpha_counter_cannot_replace_newer_target_image() {
        let directory = tempfile::tempdir().unwrap();
        publish_target_with_checkpoint(
            directory.path(),
            b"new",
            "3.0.0-alpha.10",
            "release",
            FirmwareTarget::Frame,
            || {},
        )
        .unwrap();
        assert!(publish_target_with_checkpoint(
            directory.path(),
            b"old",
            "3.0.0-alpha.9",
            "release",
            FirmwareTarget::Frame,
            || {}
        )
        .is_err());
        let info: FirmwareVersion =
            serde_json::from_slice(&std::fs::read(directory.path().join("version.json")).unwrap())
                .unwrap();
        assert_eq!(info.version, "3.0.0-alpha.10");
        assert_eq!(
            std::fs::read(directory.path().join(info.file)).unwrap(),
            b"new"
        );
    }
}
