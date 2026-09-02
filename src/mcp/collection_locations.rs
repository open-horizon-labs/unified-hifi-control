//! Durable, provider-private collection locations used by canonical Library URLs.

use anyhow::{Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

const LOCATION_FILE: &str = "collection-locations.jsonl";

/// One human-visible step in a collection walk. `provider_path` never leaves
/// the server; Roon uses `subtitle` + `position` to safely find the equivalent
/// row in a fresh Core browse session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionStep {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subtitle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<u32>,
    #[serde(default = "visible_breadcrumb")]
    pub breadcrumb: bool,
}

const fn visible_breadcrumb() -> bool {
    true
}

impl CollectionStep {
    pub fn new(title: impl Into<String>, provider_path: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            provider_path: Some(provider_path.into()),
            position: None,
            breadcrumb: true,
        }
    }

    pub fn roon(title: impl Into<String>, subtitle: Option<String>, position: Option<u32>) -> Self {
        Self {
            title: title.into(),
            subtitle,
            provider_path: None,
            position,
            breadcrumb: true,
        }
    }

    pub fn hidden_roon(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            subtitle: None,
            provider_path: None,
            position: None,
            breadcrumb: false,
        }
    }
}

/// Provider-specific resolution material behind one provider-neutral URL
/// token. The enum is persisted, but never serialized into a client response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum CollectionLocation {
    Roon {
        origin: RoonLocationOrigin,
        steps: Vec<CollectionStep>,
    },
    Lms {
        steps: Vec<CollectionStep>,
    },
    #[serde(rename = "musicassistant")]
    MusicAssistant {
        steps: Vec<CollectionStep>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RoonLocationOrigin {
    BrowseRoot,
    Search {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
    },
}

impl CollectionLocation {
    pub fn provider(&self) -> &'static str {
        match self {
            Self::Roon { .. } => "roon",
            Self::Lms { .. } => "lms",
            Self::MusicAssistant { .. } => "musicassistant",
        }
    }

    pub fn steps(&self) -> &[CollectionStep] {
        match self {
            Self::Roon { steps, .. } | Self::Lms { steps } | Self::MusicAssistant { steps } => {
                steps
            }
        }
    }

    pub fn with_steps(&self, steps: Vec<CollectionStep>) -> Self {
        match self {
            Self::Roon { origin, .. } => Self::Roon {
                origin: origin.clone(),
                steps,
            },
            Self::Lms { .. } => Self::Lms { steps },
            Self::MusicAssistant { .. } => Self::MusicAssistant { steps },
        }
    }

    pub fn appended(&self, step: CollectionStep) -> Self {
        let mut steps = self.steps().to_vec();
        steps.push(step);
        self.with_steps(steps)
    }

    pub fn last_provider_path(&self) -> Option<&str> {
        self.steps().last()?.provider_path.as_deref()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CollectionBreadcrumb {
    pub title: String,
    pub location: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct StoredLocation {
    token: String,
    target: CollectionLocation,
}

#[derive(Debug)]
struct StoreInner {
    path: PathBuf,
    locations: RwLock<HashMap<String, CollectionLocation>>,
}

/// Append-only durable map from short URL tokens to provider-private browse
/// identity. Clone is cheap because every request shares one in-process map.
#[derive(Clone, Debug)]
pub struct CollectionLocationStore {
    inner: Arc<StoreInner>,
}

impl Default for CollectionLocationStore {
    fn default() -> Self {
        Self::at(crate::config::get_config_file_path(LOCATION_FILE))
    }
}

impl CollectionLocationStore {
    pub fn at(path: impl Into<PathBuf>) -> Self {
        let path = path.into();
        let mut locations = HashMap::new();
        if let Ok(contents) = std::fs::read_to_string(&path) {
            for (index, line) in contents.lines().enumerate() {
                if line.trim().is_empty() {
                    continue;
                }
                match serde_json::from_str::<StoredLocation>(line) {
                    Ok(entry) => {
                        locations.entry(entry.token).or_insert(entry.target);
                    }
                    Err(error) => tracing::warn!(
                        path = %path.display(),
                        line = index + 1,
                        %error,
                        "ignoring invalid collection-location journal entry"
                    ),
                }
            }
        }
        Self {
            inner: Arc::new(StoreInner {
                path,
                locations: RwLock::new(locations),
            }),
        }
    }

    pub fn mint(&self, target: CollectionLocation) -> Result<String> {
        let token = location_token(&target)?;
        let mut locations = self
            .inner
            .locations
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(existing) = locations.get(&token) {
            anyhow::ensure!(
                existing == &target,
                "collection location token collision for {token}"
            );
            return Ok(token);
        }

        append_location(
            &self.inner.path,
            &StoredLocation {
                token: token.clone(),
                target: target.clone(),
            },
        )?;
        locations.insert(token.clone(), target);
        Ok(token)
    }

    pub fn resolve(&self, token: &str) -> Option<CollectionLocation> {
        self.inner
            .locations
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(token)
            .cloned()
    }

    pub fn breadcrumbs(&self, target: &CollectionLocation) -> Result<Vec<CollectionBreadcrumb>> {
        let mut breadcrumbs = Vec::with_capacity(target.steps().len());
        for end in 1..=target.steps().len() {
            if !target.steps()[end - 1].breadcrumb {
                continue;
            }
            let prefix = target.with_steps(target.steps()[..end].to_vec());
            breadcrumbs.push(CollectionBreadcrumb {
                title: target.steps()[end - 1].title.clone(),
                location: self.mint(prefix)?,
            });
        }
        Ok(breadcrumbs)
    }
}

fn location_token(target: &CollectionLocation) -> Result<String> {
    let bytes = serde_json::to_vec(target).context("serialize collection location")?;
    let digest = Sha256::digest(bytes);
    let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&digest[..16]);
    Ok(format!("loc_{encoded}"))
}

fn append_location(path: &Path, entry: &StoredLocation) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!("create collection location directory {}", parent.display())
        })?;
    }

    let mut options = OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("open collection location journal {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .with_context(|| {
                format!(
                    "restrict collection location journal permissions {}",
                    path.display()
                )
            })?;
    }
    serde_json::to_writer(&mut file, entry)
        .context("serialize collection location journal entry")?;
    file.write_all(b"\n")
        .context("append collection location journal entry")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{CollectionLocation, CollectionLocationStore, CollectionStep};

    fn lms_album() -> CollectionLocation {
        CollectionLocation::Lms {
            steps: vec![
                CollectionStep::new("Albums", "albums"),
                CollectionStep::new("Ambient 1", "album:42"),
            ],
        }
    }

    #[test]
    fn location_tokens_are_short_deterministic_and_provider_private() {
        let directory = tempfile::tempdir().unwrap();
        let store = CollectionLocationStore::at(directory.path().join("locations.jsonl"));

        let first = store.mint(lms_album()).unwrap();
        let second = store.mint(lms_album()).unwrap();

        assert_eq!(first, second);
        assert!(first.starts_with("loc_"));
        assert!(first.len() <= 27, "canonical URL token grew: {first}");
        assert!(!first.contains("album"));
        assert!(!first.contains("Ambient"));
    }

    #[test]
    fn locations_survive_a_new_store_instance() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("locations.jsonl");
        let token = CollectionLocationStore::at(path.clone())
            .mint(lms_album())
            .unwrap();

        let restarted = CollectionLocationStore::at(path);
        assert_eq!(restarted.resolve(&token), Some(lms_album()));
    }

    #[test]
    fn breadcrumb_locations_are_minted_for_every_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let store = CollectionLocationStore::at(directory.path().join("locations.jsonl"));

        let breadcrumbs = store.breadcrumbs(&lms_album()).unwrap();

        assert_eq!(breadcrumbs.len(), 2);
        assert_eq!(breadcrumbs[0].title, "Albums");
        assert_eq!(breadcrumbs[1].title, "Ambient 1");
        assert_ne!(breadcrumbs[0].location, breadcrumbs[1].location);
    }

    #[cfg(unix)]
    #[test]
    fn location_journal_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("locations.jsonl");
        CollectionLocationStore::at(path.clone())
            .mint(lms_album())
            .unwrap();

        let mode = std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
