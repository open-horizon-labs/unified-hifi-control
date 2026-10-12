//! Persistence for zone→device links and per-device settings (#779).
//!
//! # What this deliberately does not copy from the HQPlayer link store
//!
//! `HqpZoneLinkService` keys links by a user-editable instance name, re-points orphans to "the only
//! remaining instance", and on a parse error logs and continues with an empty map — which the next
//! save then writes over the unreadable file. For a link that routes volume to an amplifier, each
//! of those is a routing failure:
//!
//! - links key on hardware identity ([`DeviceId`]);
//! - a link whose zone or device is absent stays, dormant, until a person removes it;
//! - an unreadable or newer-format file puts the store in a refusing state: reads return what was
//!   last loaded (nothing), writes are refused, and the file on disk is never touched.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use super::{DeviceId, DeviceLink};

/// File name under the config directory.
pub const DEVICE_LINKS_FILE: &str = "device-links.json";

/// Current on-disk format version. Bump with a migration, never silently.
pub const DEVICE_LINKS_VERSION: u32 = 1;

/// Per-device user settings that are not observations.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceSettings {
    /// The user's volume ceiling in dB. Only ever lowers the published maximum.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_ceiling: Option<f32>,
}

/// The whole persisted document.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceLinksDocument {
    pub version: u32,
    #[serde(default)]
    pub links: Vec<DeviceLink>,
    #[serde(default)]
    pub devices: BTreeMap<DeviceId, DeviceSettings>,
    /// Hosts to probe directly when multicast discovery cannot reach a device (another subnet).
    #[serde(default)]
    pub manual_hosts: Vec<String>,
    /// Devices with no discovery mechanism (Anthem, Trinnov) or on another subnet, per family.
    #[serde(default)]
    pub manual_devices: Vec<ManualDevice>,
}

/// A device configured by address rather than discovered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualDevice {
    /// Device family (adapter) name, e.g. `anthem`, `trinnov`.
    pub family: String,
    pub host: String,
    /// Control port, when not the family default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Hardware identity when the protocol cannot report one (and for Wake-on-LAN).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mac: Option<String>,
}

impl DeviceLinksDocument {
    fn empty() -> Self {
        Self {
            version: DEVICE_LINKS_VERSION,
            ..Self::default()
        }
    }
}

/// Why the store refuses writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StoreError {
    /// The file exists but could not be read as a document this build understands. The file is
    /// left untouched so a person (or a newer build) can recover it.
    Unreadable { path: PathBuf, detail: String },
    /// Writing the file failed.
    Io(String),
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreadable { path, detail } => write!(
                f,
                "device link file {} is unreadable ({detail}); refusing to overwrite it",
                path.display()
            ),
            Self::Io(detail) => write!(f, "could not save device links: {detail}"),
        }
    }
}

impl std::error::Error for StoreError {}

/// Loaded state: a document, or the reason it could not be loaded.
#[derive(Debug, Clone)]
enum Loaded {
    Ready(DeviceLinksDocument),
    Refusing(StoreError),
}

/// Owns the device-links file. All writes go through one async mutex, and every write persists
/// before it returns, so callers publish to the aggregator only what is already durable.
pub struct DeviceLinkStore {
    path: PathBuf,
    loaded: Mutex<Loaded>,
}

impl DeviceLinkStore {
    /// Open the store at the standard config location.
    pub fn open_default() -> Self {
        Self::open(crate::config::get_config_file_path(DEVICE_LINKS_FILE))
    }

    /// Open the store at `path`. A missing file is an empty store; anything else unreadable
    /// leaves the store refusing writes.
    pub fn open(path: PathBuf) -> Self {
        let loaded = match std::fs::read_to_string(&path) {
            Ok(content) => match parse(&content) {
                Ok(document) => {
                    tracing::info!(
                        links = document.links.len(),
                        "Loaded device links from {}",
                        path.display()
                    );
                    Loaded::Ready(document)
                }
                Err(detail) => {
                    let error = StoreError::Unreadable {
                        path: path.clone(),
                        detail,
                    };
                    tracing::error!("{error}");
                    Loaded::Refusing(error)
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Loaded::Ready(DeviceLinksDocument::empty())
            }
            Err(error) => {
                let error = StoreError::Unreadable {
                    path: path.clone(),
                    detail: error.to_string(),
                };
                tracing::error!("{error}");
                Loaded::Refusing(error)
            }
        };
        Self {
            path,
            loaded: Mutex::new(loaded),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current document, or the reason the store is refusing.
    pub async fn document(&self) -> Result<DeviceLinksDocument, StoreError> {
        match &*self.loaded.lock().await {
            Loaded::Ready(document) => Ok(document.clone()),
            Loaded::Refusing(error) => Err(error.clone()),
        }
    }

    /// Apply `change` to the document and persist it. On any failure the in-memory document is
    /// unchanged, so memory never runs ahead of disk.
    pub async fn update<F>(&self, change: F) -> Result<DeviceLinksDocument, StoreError>
    where
        F: FnOnce(&mut DeviceLinksDocument),
    {
        let mut loaded = self.loaded.lock().await;
        let current = match &*loaded {
            Loaded::Ready(document) => document.clone(),
            Loaded::Refusing(error) => return Err(error.clone()),
        };
        let mut next = current;
        change(&mut next);
        next.version = DEVICE_LINKS_VERSION;
        write_atomically(&self.path, &next).map_err(StoreError::Io)?;
        *loaded = Loaded::Ready(next.clone());
        Ok(next)
    }
}

fn parse(content: &str) -> Result<DeviceLinksDocument, String> {
    let value: serde_json::Value = serde_json::from_str(content).map_err(|e| e.to_string())?;
    let version = value
        .get("version")
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| "missing version".to_string())?;
    if version != u64::from(DEVICE_LINKS_VERSION) {
        return Err(format!(
            "format version {version} is not {DEVICE_LINKS_VERSION}; written by a different build"
        ));
    }
    serde_json::from_value(value).map_err(|e| e.to_string())
}

fn write_atomically(path: &Path, document: &DeviceLinksDocument) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let json = serde_json::to_string_pretty(document).map_err(|e| e.to_string())?;
    let temporary = path.with_extension("json.tmp");
    std::fs::write(&temporary, json).map_err(|e| e.to_string())?;
    std::fs::rename(&temporary, path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn link(zone: &str, device: &str) -> DeviceLink {
        DeviceLink {
            zone_id: zone.to_string(),
            device_id: DeviceId::parse(device).unwrap(),
            source_id: Some("MPLAY".to_string()),
        }
    }

    #[tokio::test]
    async fn missing_file_is_an_empty_store_and_writes_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVICE_LINKS_FILE);
        let store = DeviceLinkStore::open(path.clone());
        assert!(store.document().await.unwrap().links.is_empty());

        store
            .update(|doc| doc.links.push(link("roon:living", "marantz:0006781E0AB8")))
            .await
            .unwrap();

        let reopened = DeviceLinkStore::open(path);
        let document = reopened.document().await.unwrap();
        assert_eq!(document.version, DEVICE_LINKS_VERSION);
        assert_eq!(
            document.links,
            vec![link("roon:living", "marantz:0006781E0AB8")]
        );
    }

    /// CRITICAL (#779): an unreadable link file must never be replaced by an empty one. The HQP
    /// store's behaviour — warn, load nothing, then save the empty map over the file on the next
    /// write — silently destroys every link, and with it the record of which amplifier each zone
    /// drives.
    #[tokio::test]
    async fn critical_unreadable_file_is_preserved_and_writes_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVICE_LINKS_FILE);
        let corrupt = "{\"version\": 1, \"links\": [ this is not json";
        std::fs::write(&path, corrupt).unwrap();

        let store = DeviceLinkStore::open(path.clone());
        assert!(matches!(
            store.document().await,
            Err(StoreError::Unreadable { .. })
        ));
        let refused = store
            .update(|doc| doc.links.push(link("roon:living", "marantz:0006781E0AB8")))
            .await;
        assert!(matches!(refused, Err(StoreError::Unreadable { .. })));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), corrupt);
    }

    #[tokio::test]
    async fn a_newer_format_version_is_refused_not_downgraded() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVICE_LINKS_FILE);
        let newer = "{\"version\": 2, \"links\": [], \"something_new\": true}";
        std::fs::write(&path, newer).unwrap();
        let store = DeviceLinkStore::open(path.clone());
        assert!(store.update(|_| {}).await.is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), newer);
    }

    #[tokio::test]
    async fn dormant_links_survive_a_reload_untouched() {
        // A link to a device that is offline and a zone that is not currently published is still a
        // link: nothing in the store prunes or re-points it.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DEVICE_LINKS_FILE);
        std::fs::write(
            &path,
            r#"{"version":1,"links":[{"zone_id":"lms:gone","device_id":"marantz:0006781E2AB9"}],"devices":{"marantz:0006781E2AB9":{"volume_ceiling":-30.0}}}"#,
        )
        .unwrap();
        let store = DeviceLinkStore::open(path);
        let document = store.document().await.unwrap();
        assert_eq!(document.links.len(), 1);
        assert_eq!(document.links[0].source_id, None);
        assert_eq!(
            document.devices[&DeviceId::parse("marantz:0006781E2AB9").unwrap()].volume_ceiling,
            Some(-30.0)
        );
    }
}
