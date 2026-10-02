//! Optional source-backed metadata reads using the existing paired identity.
//! No playback calls, listening history, public LAN API or relay wire changes.
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use super::{protocol::canonical_json, CloudConnectorConfig, InstallationIdentity};

const MAX_RESPONSE: usize = 512 * 1024;
pub const PURPOSE: &str = "hiphi.music-details.v1";

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct MusicIdentity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub artist_mbid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recording_mbid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub release_mbid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wikidata_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MusicDetails {
    pub version: u8,
    pub item_token: String,
    pub status: String,
    #[serde(flatten)]
    pub sections: serde_json::Map<String, Value>,
}

impl MusicDetails {
    /// Late A cannot replace B; the caller changes this opaque token only for an item/zone/epoch change.
    pub fn applies_to(&self, current_item_token: &str) -> bool {
        self.version == 1 && self.item_token == current_item_token
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MusicError {
    #[error("HiPhi Cloud is not paired")]
    NotPaired,
    #[error("invalid music details request")]
    InvalidRequest,
    #[error("music details unavailable")]
    Unavailable,
    #[error("music details authorization refused ({0})")]
    Refused(u16),
}

/// Sign a dedicated read proof; relay session and command grants are never reused.
pub fn signed_request(
    identity: &InstallationIdentity,
    endpoint: &str,
    music: &MusicIdentity,
    item_token: &str,
    language: &str,
    now_ms: u64,
) -> Result<Value, MusicError> {
    let valid_token = !item_token.is_empty()
        && item_token.len() <= 128
        && item_token
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
    let valid_language = (2..=12).contains(&language.len())
        && language
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c == b'-');
    if !valid_token || !valid_language || music.duration_ms.is_some_and(|n| n > 86_400_000) {
        return Err(MusicError::InvalidRequest);
    }
    for text in [&music.artist, &music.title, &music.album]
        .into_iter()
        .flatten()
    {
        if text.trim().is_empty() || text.len() > 512 || text.chars().any(char::is_control) {
            return Err(MusicError::InvalidRequest);
        }
    }
    let mut request = serde_json::json!({
        "purpose": PURPOSE, "version": 1, "installation_id": identity.installation_id(),
        "request_id": Uuid::new_v4(), "issued_at": now_ms, "endpoint": endpoint,
        "item_token": item_token, "identity": music, "language": language,
    });
    let bytes = canonical_json(&request).map_err(|_| MusicError::InvalidRequest)?;
    if bytes.len() > 7 * 1024 {
        return Err(MusicError::InvalidRequest);
    }
    request["signature"] = Value::String(URL_SAFE_NO_PAD.encode(identity.sign(&bytes).to_bytes()));
    Ok(request)
}

pub struct MusicDetailsClient {
    identity: InstallationIdentity,
    endpoint: url::Url,
    client: reqwest::Client,
}

impl MusicDetailsClient {
    /// Uses owner-only pairing configuration; never takes a cloud URL from track metadata.
    pub fn from_runtime(config_dir: impl AsRef<Path>) -> Result<Self, MusicError> {
        let config = CloudConnectorConfig::from_runtime(config_dir.as_ref().to_owned())
            .map_err(|_| MusicError::Unavailable)?
            .ok_or(MusicError::NotPaired)?;
        let identity = InstallationIdentity::load(&config.key_path, config.installation_id)
            .map_err(|_| MusicError::Unavailable)?;
        let mut endpoint =
            url::Url::parse(config.endpoint.as_str()).map_err(|_| MusicError::Unavailable)?;
        // This capability is advertised by the production HiPhi authority only.
        // A custom relay does not implicitly become an enrichment data recipient.
        if endpoint.host_str() != Some("relay.hiphi.audio")
            || endpoint.port().is_some()
            || endpoint.path() != "/v1/relay/connect"
        {
            return Err(MusicError::Unavailable);
        }
        endpoint
            .set_scheme("https")
            .map_err(|_| MusicError::Unavailable)?;
        endpoint.set_path("/v1/music/details");
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| MusicError::Unavailable)?;
        Ok(Self {
            identity,
            endpoint,
            client,
        })
    }

    /// On-demand only. Seek, volume and heartbeat updates must not invoke this method.
    pub async fn read(
        &self,
        music: &MusicIdentity,
        item_token: &str,
        language: &str,
    ) -> Result<MusicDetails, MusicError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| MusicError::Unavailable)?
            .as_millis()
            .try_into()
            .map_err(|_| MusicError::Unavailable)?;
        let body = signed_request(
            &self.identity,
            self.endpoint.as_str(),
            music,
            item_token,
            language,
            now,
        )?;
        let mut response = self
            .client
            .post(self.endpoint.clone())
            .json(&body)
            .send()
            .await
            .map_err(|_| MusicError::Unavailable)?;
        if !response.status().is_success() {
            return Err(MusicError::Refused(response.status().as_u16()));
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(MusicError::Unavailable);
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| MusicError::Unavailable)?
        {
            if bytes.len().saturating_add(chunk.len()) > MAX_RESPONSE {
                return Err(MusicError::Unavailable);
            }
            bytes.extend_from_slice(&chunk);
        }
        let details: MusicDetails =
            serde_json::from_slice(&bytes).map_err(|_| MusicError::Unavailable)?;
        if !details.applies_to(item_token)
            || !["complete", "partial", "ambiguous", "unavailable"]
                .contains(&details.status.as_str())
        {
            return Err(MusicError::Unavailable);
        }
        Ok(details)
    }

    /// Read from the aggregator and re-check after the optional cloud work.
    /// This deliberately ignores seek/volume changes when identifying the item.
    pub async fn read_now_playing(
        &self,
        aggregator: &crate::aggregator::ZoneAggregator,
        zone_id: &str,
        language: &str,
    ) -> Result<MusicDetails, MusicError> {
        let before = aggregator
            .get_now_playing(zone_id)
            .await
            .ok_or(MusicError::InvalidRequest)?;
        let music = MusicIdentity::from_now_playing(&before);
        let token = Uuid::new_v4().simple().to_string();
        let details = self.read(&music, &token, language).await?;
        let after = aggregator
            .get_now_playing(zone_id)
            .await
            .ok_or(MusicError::Unavailable)?;
        if music != MusicIdentity::from_now_playing(&after) {
            return Err(MusicError::Unavailable);
        }
        Ok(details)
    }
}

impl MusicIdentity {
    pub fn from_now_playing(item: &crate::bus::NowPlaying) -> Self {
        let nonempty = |s: &str| (!s.trim().is_empty()).then(|| s.to_owned());
        Self {
            artist: nonempty(&item.artist),
            title: nonempty(&item.title),
            album: nonempty(&item.album),
            duration_ms: item
                .duration
                .filter(|v| v.is_finite() && *v >= 0.0 && *v <= 86400.0)
                .map(|v| (v * 1000.0).round() as u64),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use ed25519_dalek::{Signature, Verifier};

    #[test]
    fn proof_binds_identity_track_and_dedicated_audience() {
        let identity =
            InstallationIdentity::generate("12345678-1234-4234-8234-123456789abc".into()).unwrap();
        let music = MusicIdentity {
            artist: Some("Sting".into()),
            ..Default::default()
        };
        let mut body = signed_request(
            &identity,
            "https://relay.hiphi.audio/v1/music/details",
            &music,
            "track_A",
            "en",
            1_000,
        )
        .unwrap();
        let signature = body.as_object_mut().unwrap().remove("signature").unwrap();
        let raw = URL_SAFE_NO_PAD.decode(signature.as_str().unwrap()).unwrap();
        let signature = Signature::from_slice(&raw).unwrap();
        assert!(identity
            .verifying_key()
            .verify(&canonical_json(&body).unwrap(), &signature)
            .is_ok());
        body["item_token"] = "track_B".into();
        assert!(identity
            .verifying_key()
            .verify(&canonical_json(&body).unwrap(), &signature)
            .is_err());
        assert_eq!(body["purpose"], PURPOSE);
    }

    #[test]
    fn late_response_cannot_apply_after_track_or_epoch_change() {
        let packet = MusicDetails {
            version: 1,
            item_token: "A".into(),
            status: "partial".into(),
            sections: Default::default(),
        };
        assert!(packet.applies_to("A"));
        assert!(!packet.applies_to("B"));
    }

    #[test]
    fn rejects_unbounded_or_control_metadata() {
        let identity =
            InstallationIdentity::generate("12345678-1234-4234-8234-123456789abc".into()).unwrap();
        let music = MusicIdentity {
            artist: Some("é".repeat(300)),
            ..Default::default()
        };
        assert!(signed_request(
            &identity,
            "https://relay.hiphi.audio/v1/music/details",
            &music,
            "A",
            "en",
            0
        )
        .is_err());
    }

    #[test]
    fn playback_progress_does_not_change_music_identity_but_album_does() {
        let mut item: crate::bus::NowPlaying = serde_json::from_value(serde_json::json!({
            "title":"Fragile","artist":"Sting","album":"Nothing Like the Sun", "duration":234.0,
            "seek_position":1.0
        }))
        .unwrap();
        let initial = MusicIdentity::from_now_playing(&item);
        item.seek_position = Some(120.0);
        assert_eq!(initial, MusicIdentity::from_now_playing(&item));
        item.album = "Live edition".into();
        assert_ne!(initial, MusicIdentity::from_now_playing(&item));
    }
}
