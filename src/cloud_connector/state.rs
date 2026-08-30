use std::collections::{HashMap, HashSet};

use super::{
    identity::ZoneHandleMap,
    protocol::{NowPlayingProjection, VolumeControl, ZoneProjection, MAX_STRING_BYTES, MAX_ZONES},
};

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum StateError {
    #[error("state projection exceeds a protocol bound: {0}")]
    Invalid(&'static str),
}

/// Build the cloud projection from the aggregator's authoritative zones. The
/// provider image key is reduced to a revision digest; the key itself never
/// enters the semantic snapshot.
#[cfg(not(test))]
pub async fn snapshot_from_aggregator(
    aggregator: &crate::aggregator::ZoneAggregator,
    store: &mut StateStore,
    installation_id: String,
    epoch: u64,
    revision: u64,
    now_ms: u64,
) -> Result<StateProjection, StateError> {
    let zones = aggregator.get_zones().await;
    let input = SemanticStateInput {
        installation_id,
        epoch,
        revision,
        observed_at: now_ms,
        expires_at: now_ms.saturating_add(30_000),
        zones: zones
            .into_iter()
            .map(|zone| SemanticZoneInput {
                provider_id: zone.zone_id,
                name: zone.zone_name,
                state: zone.state.to_string(),
                volume: zone.volume_control.map(|volume| VolumeInput {
                    value: volume.value.into(),
                    min: volume.min.into(),
                    max: volume.max.into(),
                    step: volume.step.into(),
                    scale: match volume.scale {
                        crate::bus::VolumeScale::Decibel => "db",
                        crate::bus::VolumeScale::Percentage => "percent",
                        crate::bus::VolumeScale::Linear => "linear",
                        crate::bus::VolumeScale::Unknown => "unknown",
                    }
                    .to_owned(),
                }),
                now_playing: zone.now_playing.map(|playing| {
                    let image_key = playing.image_key;
                    NowPlayingInput {
                        title: playing.title,
                        artist: playing.artist,
                        art_revision: image_key.clone().map(|key| {
                            use sha2::{Digest, Sha256};
                            format!("art_{}", hex::encode(Sha256::digest(key.as_bytes())))
                        }),
                        image_key,
                    }
                }),
            })
            .collect(),
    };
    store.snapshot(input)
}

#[derive(Clone, Debug)]
pub struct SemanticZoneInput {
    pub provider_id: String,
    pub name: String,
    pub state: String,
    pub volume: Option<VolumeInput>,
    pub now_playing: Option<NowPlayingInput>,
}
#[derive(Clone, Debug)]
pub struct VolumeInput {
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub scale: String,
}
#[derive(Clone, Debug)]
pub struct NowPlayingInput {
    pub title: String,
    pub artist: String,
    pub art_revision: Option<String>,
    pub image_key: Option<String>,
}

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct StateProjection {
    pub installation_id: String,
    pub epoch: u64,
    pub revision: u64,
    pub observed_at: u64,
    pub expires_at: u64,
    pub zones: Vec<ZoneProjection>,
    pub now_playing: Vec<NowPlayingProjection>,
}

pub struct SemanticStateInput {
    pub installation_id: String,
    pub epoch: u64,
    pub revision: u64,
    pub observed_at: u64,
    pub expires_at: u64,
    pub zones: Vec<SemanticZoneInput>,
}

#[derive(Default)]
pub struct StateStore {
    handles: ZoneHandleMap,
    artwork_keys: HashMap<(String, String), String>,
    latest: Option<StateProjection>,
    delta_revision: Option<u64>,
}

impl StateStore {
    pub fn snapshot(&mut self, input: SemanticStateInput) -> Result<StateProjection, StateError> {
        validate_input(&input)?;
        self.artwork_keys.clear();
        let mut zones = Vec::new();
        let mut now_playing = Vec::new();
        let mut handles = HashSet::new();
        for zone in input.zones {
            let handle = self.handles.handle_for(&zone.provider_id);
            if !handles.insert(handle.clone()) {
                return Err(StateError::Invalid("duplicate zone handle"));
            }
            if let Some(now_playing) = zone.now_playing.as_ref() {
                if let (Some(revision), Some(key)) =
                    (&now_playing.art_revision, &now_playing.image_key)
                {
                    self.artwork_keys
                        .insert((handle.clone(), revision.clone()), key.clone());
                }
            }
            if let Some(playing) = zone.now_playing.as_ref() {
                now_playing.push(NowPlayingProjection {
                    zone_handle: handle.clone(),
                    title: playing.title.clone(),
                    artist: playing.artist.clone(),
                    image_revision: playing.art_revision.clone(),
                    is_playing: zone.state == "playing",
                    volume: zone.volume.as_ref().map(|v| v.value),
                });
            }
            zones.push(ZoneProjection {
                zone_handle: handle,
                zone_name: zone.name,
                state: zone.state,
                volume_control: zone.volume.map(|v| VolumeControl {
                    value: v.value,
                    min: v.min,
                    max: v.max,
                    step: v.step,
                    is_muted: false,
                    scale: Some(v.scale),
                }),
            });
        }
        let projection = StateProjection {
            installation_id: input.installation_id,
            epoch: input.epoch,
            revision: input.revision,
            observed_at: input.observed_at,
            expires_at: input.expires_at,
            zones,
            now_playing,
        };
        self.delta_revision = Some(projection.revision);
        self.latest = Some(projection.clone());
        Ok(projection)
    }
    pub fn accepts_delta(&self, revision: u64) -> bool {
        self.delta_revision
            .map(|current| revision == current + 1)
            .unwrap_or(false)
    }
    pub fn provider_id(&self, handle: &str) -> Option<&str> {
        self.latest
            .as_ref()?
            .zones
            .iter()
            .any(|zone| zone.zone_handle == handle)
            .then(|| self.handles.provider_id(handle))
            .flatten()
    }
    pub fn is_fresh(&self, epoch: u64, now_ms: u64) -> bool {
        self.latest.as_ref().is_some_and(|projection| {
            projection.epoch == epoch
                && projection.observed_at <= now_ms
                && projection.expires_at > now_ms
        })
    }
    pub fn state(&self, handle: &str) -> Option<&str> {
        self.latest
            .as_ref()?
            .zones
            .iter()
            .find(|zone| zone.zone_handle == handle)
            .map(|zone| zone.state.as_str())
    }
    pub fn artwork_key(&self, handle: &str, revision: &str) -> Option<&str> {
        self.artwork_keys
            .get(&(handle.to_owned(), revision.to_owned()))
            .map(String::as_str)
    }
    pub fn latest(&self) -> Option<&StateProjection> {
        self.latest.as_ref()
    }
}

/// Explicitly typed to prevent provider identifiers from finding their way
/// into a cloud-facing state object.
pub fn public_zone_handles(projection: &StateProjection) -> HashMap<String, ()> {
    projection
        .zones
        .iter()
        .map(|z| (z.zone_handle.clone(), ()))
        .collect()
}

fn validate_input(input: &SemanticStateInput) -> Result<(), StateError> {
    if input.epoch == 0 || input.expires_at <= input.observed_at {
        return Err(StateError::Invalid("epoch or expiry"));
    }
    if input.installation_id.len() > MAX_STRING_BYTES
        || input.zones.len() > MAX_ZONES
        || uuid::Uuid::parse_str(&input.installation_id).is_err()
    {
        return Err(StateError::Invalid("installation, zone count, or id"));
    }
    let mut providers = HashSet::new();
    for zone in &input.zones {
        if zone.provider_id.is_empty()
            || zone.provider_id.len() > MAX_STRING_BYTES
            || !providers.insert(&zone.provider_id)
            || zone.name.len() > MAX_STRING_BYTES
            || zone.state.len() > MAX_STRING_BYTES
        {
            return Err(StateError::Invalid("zone identity or string bound"));
        }
        if let Some(volume) = &zone.volume {
            if !volume.value.is_finite()
                || !volume.min.is_finite()
                || !volume.max.is_finite()
                || !volume.step.is_finite()
                || volume.min > volume.max
                || volume.value < volume.min
                || volume.value > volume.max
                || volume.step <= 0.0
                || volume.scale.is_empty()
                || volume.scale.len() > MAX_STRING_BYTES
            {
                return Err(StateError::Invalid("volume values"));
            }
        }
        if let Some(playing) = &zone.now_playing {
            if playing.title.len() > MAX_STRING_BYTES
                || playing.artist.len() > MAX_STRING_BYTES
                || playing
                    .art_revision
                    .as_ref()
                    .is_some_and(|revision| !super::protocol::validate_id(revision))
                || playing
                    .image_key
                    .as_ref()
                    .is_some_and(|key| key.is_empty() || key.len() > MAX_STRING_BYTES)
            {
                return Err(StateError::Invalid("now-playing string bound"));
            }
        }
    }
    Ok(())
}
