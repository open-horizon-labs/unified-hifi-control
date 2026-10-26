//! Controller shortcuts: a named pointer to a Library item, and per-controller maps from an input
//! event (a key and a gesture) to one of those pointers.
//!
//! # Why two layers
//!
//! A *shortcut* says what to play. A controller's *map* says which button plays it. Keeping them
//! apart means one pin in the Library can serve a BLE remote on a Frame, a Dial and a HALO, and a
//! re-flashed controller loses nothing because the map lives here, not in the device's NVS.
//!
//! # What a shortcut stores
//!
//! Never a ref token and never a provider session. The MCP ref table is deliberately short-lived
//! (see `mcp::refs`), and a Roon `item_key` is only meaningful inside the browse session that
//! minted it. A shortcut keeps the part that outlives both:
//!
//! - **Roon:** the title trail from the browse root to the item, re-walked in a fresh session at
//!   press time (`RoonAdapter::resolve_trail`, the recovery #616 added).
//! - **LMS:** the database id or URL, which `LmsAdapter::play_target` re-validates before it
//!   touches the queue. A `GlobalSearchItem` is a positional breadcrumb that can play the wrong
//!   thing silently, so it can never be pinned.
//! - **Spotify, Music Assistant:** the provider URI.
//!
//! Anything without a durable handle is refused at pin time, with a reason, rather than pinned and
//! left to fail on a button nobody can see.
//!
//! # What a press never does
//!
//! It never falls back to another zone. The zone is the controller's own (its assigned zone, else
//! the one it reports); if that zone is gone, the press fails with a reason the controller can
//! show, because music starting in the wrong room is worse than none. A zone hidden from lists
//! still plays, exactly as everywhere else (`zone_list`).

pub mod resolve;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::adapters::lms::{LmsPlayTarget, LmsSearchResultType};
use crate::config::get_config_file_path;
use crate::mcp::refs::RefTarget;

const SHORTCUTS_FILE: &str = "shortcuts.json";

/// The stored form of an LMS target. A mirror of [`LmsPlayTarget`] without the variant that must
/// never be persisted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LmsShortcutTarget {
    Library { item: LmsSearchResultType, id: i64 },
    SavedPlaylist { id: i64 },
    Url { url: String },
}

impl LmsShortcutTarget {
    pub fn to_play_target(&self) -> LmsPlayTarget {
        match self {
            Self::Library { item, id } => LmsPlayTarget::Library {
                kind: *item,
                id: *id,
            },
            Self::SavedPlaylist { id } => LmsPlayTarget::SavedPlaylist { id: *id },
            Self::Url { url } => LmsPlayTarget::Url { url: url.clone() },
        }
    }
}

/// What a shortcut points at, in a form that survives restarts and expired sessions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "provider", rename_all = "snake_case")]
pub enum ShortcutTarget {
    Roon { trail: Vec<String> },
    Lms { target: LmsShortcutTarget },
    Spotify { uri: String },
    MusicAssistant { uri: String },
}

impl ShortcutTarget {
    /// Provider label as the API reports it.
    pub fn provider_label(&self) -> &'static str {
        match self {
            Self::Roon { .. } => "roon",
            Self::Lms { .. } => "lms",
            Self::Spotify { .. } => "spotify",
            Self::MusicAssistant { .. } => "musicassistant",
        }
    }

    pub fn provider(&self) -> crate::mcp::envelope::Provider {
        use crate::mcp::envelope::Provider;
        match self {
            Self::Roon { .. } => Provider::Roon,
            Self::Lms { .. } => Provider::Lms,
            Self::Spotify { .. } => Provider::Spotify,
            Self::MusicAssistant { .. } => Provider::MusicAssistant,
        }
    }

    /// Convert a freshly minted ref into a durable target, or say why it cannot be one.
    pub fn from_ref(target: &RefTarget) -> Result<Self, PinRefusal> {
        match target {
            RefTarget::Roon { target, .. } => {
                // A trail is re-walked from the browse root, so it has to be a path. A search hit
                // carries only its own title (`library.rs`, #616), which names no root-level row
                // and could never be found again. Playable items sit at least two levels down
                // (a section, then the item), so anything shorter is not a path.
                if target.trail.len() < 2 {
                    Err(PinRefusal::NoDurableHandle(
                        "Roon gave no path to this item, so it could not be found again once \
                         this browse session ends. Open it from Browse and pin it there."
                            .into(),
                    ))
                } else {
                    Ok(Self::Roon {
                        trail: target.trail.clone(),
                    })
                }
            }
            RefTarget::Lms { target, .. } => match target {
                LmsPlayTarget::Library { kind, id } => Ok(Self::Lms {
                    target: LmsShortcutTarget::Library {
                        item: *kind,
                        id: *id,
                    },
                }),
                LmsPlayTarget::SavedPlaylist { id } => Ok(Self::Lms {
                    target: LmsShortcutTarget::SavedPlaylist { id: *id },
                }),
                LmsPlayTarget::Url { url } => Ok(Self::Lms {
                    target: LmsShortcutTarget::Url { url: url.clone() },
                }),
                LmsPlayTarget::GlobalSearchItem { .. } => Err(PinRefusal::NoDurableHandle(
                    "This search result has no stable identifier in LMS and could play the wrong \
                     item later. Find it under Browse instead and pin it there."
                        .into(),
                )),
            },
            RefTarget::Spotify { uri, .. } => Ok(Self::Spotify { uri: uri.clone() }),
            RefTarget::MusicAssistant { uri, .. } => Ok(Self::MusicAssistant { uri: uri.clone() }),
            RefTarget::MusicAssistantBrowse { .. }
            | RefTarget::LmsBrowse { .. }
            | RefTarget::RoonBrowse { .. } => Err(PinRefusal::NotPlayable(
                "This is a folder, not something that plays. Open it and pin an album, playlist \
                 or station inside."
                    .into(),
            )),
            RefTarget::AppleMusic { .. } => Err(PinRefusal::Unsupported(
                "Apple Music items cannot be pinned yet: the companion's content handles are not \
                 verified to stay valid between sessions."
                    .into(),
            )),
        }
    }
}

/// Why an item could not be pinned. Each carries the sentence the person should read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinRefusal {
    /// The ref expired or was never valid.
    UnknownRef(String),
    NoDurableHandle(String),
    NotPlayable(String),
    Unsupported(String),
}

impl PinRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::UnknownRef(_) => "unknown_ref",
            Self::NoDurableHandle(_) => "no_durable_handle",
            Self::NotPlayable(_) => "not_playable",
            Self::Unsupported(_) => "unsupported",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::UnknownRef(m)
            | Self::NoDurableHandle(m)
            | Self::NotPlayable(m)
            | Self::Unsupported(m) => m,
        }
    }
}

/// A pinned Library item.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Shortcut {
    pub id: String,
    /// What the curator calls it; defaults to the item's title.
    pub name: String,
    /// The item's title when it was pinned. Playback re-validates against the live library using
    /// this, so it is kept even after a rename.
    pub title: String,
    pub target: ShortcutTarget,
    pub created_at: DateTime<Utc>,
}

/// A gesture on a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Gesture {
    Press,
    Long,
    Double,
}

impl Gesture {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Press => "press",
            Self::Long => "long",
            Self::Double => "double",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "press" => Some(Self::Press),
            "long" => Some(Self::Long),
            "double" => Some(Self::Double),
            _ => None,
        }
    }
}

/// Keys whose plain press already means something and keeps meaning it: the controller runs the
/// key's normal action on the press, before any gesture is recognised. Only a long press can carry
/// a shortcut, because a press or a double would have already done the normal thing (a remote
/// whose play/pause starts a playlist cannot pause).
const TRANSPORT_KEYS: &[&str] = &["play_pause", "play", "pause", "next", "previous"];

/// Keys that never carry a shortcut under any gesture: volume must always mean volume, and a
/// firmware that recognised a gesture on them would be acting on it after already changing it.
const RESERVED_KEYS: &[&str] = &["volume_up", "volume_down", "mute"];

/// Why an input event cannot carry a shortcut.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventRefusal {
    BadKey,
    BadGesture,
    /// A press or double on a transport key.
    TransportPress,
    /// Any gesture on a volume or mute key.
    ReservedKey,
}

impl EventRefusal {
    pub fn code(&self) -> &'static str {
        match self {
            Self::BadKey => "bad_key",
            Self::BadGesture => "bad_gesture",
            Self::TransportPress => "transport_press",
            Self::ReservedKey => "reserved_key",
        }
    }

    pub fn message(&self) -> &'static str {
        match self {
            Self::BadKey => "Key names are lowercase letters, digits and underscores, up to 24.",
            Self::BadGesture => "Gesture must be press, long or double.",
            Self::TransportPress => {
                "Play/pause, next and previous keep their normal job on a press. Only a long \
                 press can start a shortcut, or use a key with no transport role."
            }
            Self::ReservedKey => {
                "Volume and mute always mean volume and mute, so they cannot start a shortcut."
            }
        }
    }
}

/// Validate and normalise an event to its map key, `"<key>:<gesture>"`.
pub fn event_key(key: &str, gesture: &str) -> Result<String, EventRefusal> {
    let valid_key = !key.is_empty()
        && key.len() <= 24
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !valid_key {
        return Err(EventRefusal::BadKey);
    }
    let gesture = Gesture::parse(gesture).ok_or(EventRefusal::BadGesture)?;
    if RESERVED_KEYS.contains(&key) {
        return Err(EventRefusal::ReservedKey);
    }
    if gesture != Gesture::Long && TRANSPORT_KEYS.contains(&key) {
        return Err(EventRefusal::TransportPress);
    }
    Ok(format!("{key}:{}", gesture.as_str()))
}

/// Split a stored event key back into its parts, for display.
pub fn split_event_key(event: &str) -> Option<(&str, &str)> {
    event.split_once(':')
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
struct Data {
    #[serde(default)]
    shortcuts: BTreeMap<String, Shortcut>,
    /// controller id -> event key -> shortcut id
    #[serde(default)]
    bindings: HashMap<String, BTreeMap<String, String>>,
}

/// What happened on a shortcut's last press, in memory only.
#[derive(Debug, Clone, Serialize)]
pub struct LastPress {
    pub at: DateTime<Utc>,
    /// Stable code (see [`resolve::PressCode`]).
    pub code: &'static str,
    pub ok: bool,
    pub message: String,
}

/// A button event a controller reported, kept in memory so the editor can show which keys a remote
/// actually sends (and let a person map one by pressing it).
#[derive(Debug, Clone, Serialize)]
pub struct RecentEvent {
    pub event: String,
    pub at: DateTime<Utc>,
    pub code: &'static str,
}

/// How many distinct recent events are kept per controller.
const RECENT_EVENTS: usize = 8;

/// Persistent shortcuts and controller maps.
#[derive(Clone)]
pub struct ShortcutStore {
    data: Arc<RwLock<Data>>,
    path: Arc<PathBuf>,
    /// Set when the file existed but could not be read. A store in this state refuses writes, so
    /// the next save cannot replace a file the person may still be able to repair.
    load_failed: bool,
    last_press: Arc<RwLock<HashMap<String, LastPress>>>,
    recent: Arc<RwLock<HashMap<String, Vec<RecentEvent>>>>,
}

impl ShortcutStore {
    /// The store at the configured location.
    pub fn open_default() -> Self {
        Self::open(get_config_file_path(SHORTCUTS_FILE))
    }

    pub fn open(path: PathBuf) -> Self {
        let (data, load_failed) = match Self::load(&path) {
            Ok(data) => (data, false),
            Err(error) => {
                tracing::error!(path = %path.display(), %error,
                    "Shortcuts could not be loaded; refusing writes. Restore the file and restart the bridge");
                (Data::default(), true)
            }
        };
        tracing::info!(path = %path.display(), shortcuts = data.shortcuts.len(), load_failed,
            "Shortcut storage initialized");
        Self {
            data: Arc::new(RwLock::new(data)),
            path: Arc::new(path),
            load_failed,
            last_press: Arc::new(RwLock::new(HashMap::new())),
            recent: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    fn load(path: &Path) -> Result<Data> {
        match fs::read(path) {
            Ok(bytes) => serde_json::from_slice(&bytes).map_err(|error| {
                // Never include the file's contents in diagnostics.
                anyhow::anyhow!(
                    "invalid shortcuts JSON at line {}, column {}",
                    error.line(),
                    error.column()
                )
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Data::default()),
            Err(error) => Err(error).context("read shortcuts"),
        }
    }

    fn ensure_loaded(&self) -> Result<()> {
        anyhow::ensure!(
            !self.load_failed,
            "shortcuts were not loaded; restore the file and restart the bridge"
        );
        Ok(())
    }

    /// Write `candidate` through a same-directory temporary file so the previous file stays intact
    /// until a complete, synced replacement is renamed over it.
    fn save(&self, current: &mut Data, candidate: Data) -> Result<()> {
        self.ensure_loaded()?;
        let parent = self.path.parent().context("shortcuts path has no parent")?;
        fs::create_dir_all(parent).context("create shortcuts directory")?;
        let json = serde_json::to_vec_pretty(&candidate).context("encode shortcuts")?;
        let mut temporary =
            tempfile::NamedTempFile::new_in(parent).context("create shortcuts replacement")?;
        temporary
            .write_all(&json)
            .context("write shortcuts replacement")?;
        temporary
            .as_file()
            .sync_all()
            .context("sync shortcuts replacement")?;
        temporary
            .persist(self.path.as_ref())
            .map_err(|error| error.error)
            .context("replace shortcuts")?;
        *current = candidate;
        #[cfg(unix)]
        fs::File::open(parent)
            .and_then(|dir| dir.sync_all())
            .context("sync shortcuts directory")?;
        Ok(())
    }

    pub async fn list(&self) -> Vec<Shortcut> {
        let mut all: Vec<Shortcut> = self.data.read().await.shortcuts.values().cloned().collect();
        all.sort_by(|a, b| {
            a.name
                .to_lowercase()
                .cmp(&b.name.to_lowercase())
                .then_with(|| a.id.cmp(&b.id))
        });
        all
    }

    pub async fn get(&self, id: &str) -> Option<Shortcut> {
        self.data.read().await.shortcuts.get(id).cloned()
    }

    /// Pin `target`. Pinning the same item twice returns the existing shortcut rather than a
    /// duplicate; the flag says which happened.
    pub async fn pin(
        &self,
        target: ShortcutTarget,
        title: String,
        name: Option<String>,
    ) -> Result<(Shortcut, bool)> {
        self.ensure_loaded()?;
        let mut data = self.data.write().await;
        if let Some(existing) = data.shortcuts.values().find(|s| s.target == target) {
            return Ok((existing.clone(), true));
        }
        let name = name
            .map(|n| n.trim().to_string())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| title.clone());
        let shortcut = Shortcut {
            id: new_id(),
            name,
            title,
            target,
            created_at: Utc::now(),
        };
        let mut candidate = data.clone();
        candidate
            .shortcuts
            .insert(shortcut.id.clone(), shortcut.clone());
        self.save(&mut data, candidate)?;
        Ok((shortcut, false))
    }

    pub async fn rename(&self, id: &str, name: &str) -> Result<Option<Shortcut>> {
        self.ensure_loaded()?;
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "a shortcut needs a name");
        let mut data = self.data.write().await;
        let mut candidate = data.clone();
        let Some(shortcut) = candidate.shortcuts.get_mut(id) else {
            return Ok(None);
        };
        shortcut.name = name.to_string();
        let updated = shortcut.clone();
        self.save(&mut data, candidate)?;
        Ok(Some(updated))
    }

    /// Delete a shortcut and every binding that pointed at it, so no button is left pointing at
    /// nothing.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        self.ensure_loaded()?;
        let mut data = self.data.write().await;
        let mut candidate = data.clone();
        if candidate.shortcuts.remove(id).is_none() {
            return Ok(false);
        }
        for map in candidate.bindings.values_mut() {
            map.retain(|_, shortcut_id| shortcut_id != id);
        }
        candidate.bindings.retain(|_, map| !map.is_empty());
        self.save(&mut data, candidate)?;
        self.last_press.write().await.remove(id);
        Ok(true)
    }

    pub async fn bindings_for(&self, controller_id: &str) -> BTreeMap<String, String> {
        self.data
            .read()
            .await
            .bindings
            .get(controller_id)
            .cloned()
            .unwrap_or_default()
    }

    /// Replace one controller's whole map. Every event key is validated and every shortcut id must
    /// exist; nothing is written if any entry is wrong.
    pub async fn set_bindings(
        &self,
        controller_id: &str,
        bindings: BTreeMap<String, String>,
    ) -> Result<(), BindingError> {
        for (event, shortcut_id) in &bindings {
            let (key, gesture) = split_event_key(event).ok_or_else(|| BindingError::Event {
                event: event.clone(),
                refusal: EventRefusal::BadKey,
            })?;
            event_key(key, gesture).map_err(|refusal| BindingError::Event {
                event: event.clone(),
                refusal,
            })?;
            if self.get(shortcut_id).await.is_none() {
                return Err(BindingError::UnknownShortcut(shortcut_id.clone()));
            }
        }
        self.ensure_loaded().map_err(BindingError::Storage)?;
        let mut data = self.data.write().await;
        let mut candidate = data.clone();
        if bindings.is_empty() {
            candidate.bindings.remove(controller_id);
        } else {
            candidate
                .bindings
                .insert(controller_id.to_string(), bindings);
        }
        self.save(&mut data, candidate)
            .map_err(BindingError::Storage)
    }

    pub async fn record_press(&self, shortcut_id: &str, press: LastPress) {
        self.last_press
            .write()
            .await
            .insert(shortcut_id.to_string(), press);
    }

    /// Remember that `controller_id` reported `event`. A repeat moves it to the front rather than
    /// filling the list, so ten presses of one key still leave room for the others.
    pub async fn record_event(&self, controller_id: &str, event: &str, code: &'static str) {
        let mut recent = self.recent.write().await;
        let list = recent.entry(controller_id.to_string()).or_default();
        list.retain(|seen| seen.event != event);
        list.insert(
            0,
            RecentEvent {
                event: event.to_string(),
                at: Utc::now(),
                code,
            },
        );
        list.truncate(RECENT_EVENTS);
    }

    pub async fn recent_events(&self, controller_id: &str) -> Vec<RecentEvent> {
        self.recent
            .read()
            .await
            .get(controller_id)
            .cloned()
            .unwrap_or_default()
    }

    pub async fn last_press(&self, shortcut_id: &str) -> Option<LastPress> {
        self.last_press.read().await.get(shortcut_id).cloned()
    }
}

/// Why a map was refused.
#[derive(Debug)]
pub enum BindingError {
    Event {
        event: String,
        refusal: EventRefusal,
    },
    UnknownShortcut(String),
    Storage(anyhow::Error),
}

fn new_id() -> String {
    use rand::Rng;
    let mut bytes = [0u8; 6];
    rand::rng().fill_bytes(&mut bytes);
    let mut id = String::from("sc_");
    for b in bytes {
        id.push_str(&format!("{b:02x}"));
    }
    id
}

#[cfg(test)]
mod tests;
