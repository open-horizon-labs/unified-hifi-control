use serde::{Deserialize, Deserializer, Serialize};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

pub const PROTOCOL_VERSION: u16 = 1;
pub const PROTOCOL_NAME: &str = "hiphi.relay.v1";
pub const RELAY_AUDIENCE: &str = "hiphi-relay";
pub const UHC_AUDIENCE: &str = "uhc-connector";
pub const MAX_MESSAGE_BYTES: usize = 64 * 1024;
pub const MAX_COMMAND_BYTES: usize = 8 * 1024;
pub const MAX_ZONES: usize = 128;
pub const MAX_STRING_BYTES: usize = 512;
pub const MAX_ARTWORK_SOURCE_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ARTWORK_OUTPUT_BYTES: usize = 512 * 1024;
pub const MAX_ARTWORK_CHUNK_BYTES: usize = 48 * 1024;
pub type Epoch = u64;
pub type TimestampMs = i64;
pub type InstallationId = String;
pub type ControlNodeId = Uuid;

// Historical fixture parser only. Runtime traffic uses the typed messages below.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WireType {
    Hello,
    Heartbeat,
    StateSnapshot,
    StateDelta,
    Command,
    Result,
    ArtRequest,
    ArtResponse,
    Revoked,
    Error,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WireEnvelope {
    pub protocol_version: u16,
    #[serde(rename = "type")]
    pub kind: WireType,
    pub installation_id: String,
    pub epoch: u64,
    pub message_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<CommandStatus>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConnectorHello {
    pub protocol_version: u16,
    pub message_id: String,
    pub connector_version: String,
    pub installation_id: InstallationId,
    pub epoch: Epoch,
    pub capabilities: Vec<String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SessionChallengeMessage {
    pub protocol_version: u16,
    pub challenge_id: Uuid,
    pub endpoint: String,
    pub nonce: String,
    pub expires_at: TimestampMs,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct InstallationSessionGrantClaims {
    pub protocol_version: u16,
    pub connector_version: String,
    pub issuer: String,
    pub audience: String,
    pub installation_id: InstallationId,
    pub endpoint: String,
    pub public_key_sha256: String,
    pub grant_jti: Uuid,
    pub issued_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub grant_generation: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "action")]
pub enum CommandAction {
    #[serde(rename = "transport.play_pause")]
    PlayPause,
    #[serde(rename = "transport.next")]
    Next,
    #[serde(rename = "transport.previous")]
    Previous,
    #[serde(rename = "volume.up")]
    VolumeUp,
    #[serde(rename = "volume.down")]
    VolumeDown,
    #[serde(rename = "volume.absolute")]
    VolumeAbsolute { value: f64 },
}
impl CommandAction {
    pub fn scope(&self) -> &'static str {
        "playback_control"
    }
}
pub type AllowedAction = CommandAction;
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct CommandPayload {
    pub zone_handle: String,
    #[serde(flatten)]
    pub action: CommandAction,
}

#[derive(Deserialize)]
#[serde(tag = "action", deny_unknown_fields)]
enum CommandPayloadWire {
    #[serde(rename = "transport.play_pause")]
    PlayPause { zone_handle: String },
    #[serde(rename = "transport.next")]
    Next { zone_handle: String },
    #[serde(rename = "transport.previous")]
    Previous { zone_handle: String },
    #[serde(rename = "volume.up")]
    VolumeUp { zone_handle: String },
    #[serde(rename = "volume.down")]
    VolumeDown { zone_handle: String },
    #[serde(rename = "volume.absolute")]
    VolumeAbsolute { zone_handle: String, value: f64 },
}

impl<'de> Deserialize<'de> for CommandPayload {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = CommandPayloadWire::deserialize(deserializer)?;
        Ok(match wire {
            CommandPayloadWire::PlayPause { zone_handle } => Self {
                zone_handle,
                action: CommandAction::PlayPause,
            },
            CommandPayloadWire::Next { zone_handle } => Self {
                zone_handle,
                action: CommandAction::Next,
            },
            CommandPayloadWire::Previous { zone_handle } => Self {
                zone_handle,
                action: CommandAction::Previous,
            },
            CommandPayloadWire::VolumeUp { zone_handle } => Self {
                zone_handle,
                action: CommandAction::VolumeUp,
            },
            CommandPayloadWire::VolumeDown { zone_handle } => Self {
                zone_handle,
                action: CommandAction::VolumeDown,
            },
            CommandPayloadWire::VolumeAbsolute { zone_handle, value } => Self {
                zone_handle,
                action: CommandAction::VolumeAbsolute { value },
            },
        })
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommandGrantClaims {
    pub protocol_version: u16,
    pub issuer: String,
    pub audience: String,
    pub installation_id: InstallationId,
    pub control_node_id: ControlNodeId,
    pub request_id: Uuid,
    pub idempotency_key: Uuid,
    pub scope: String,
    pub payload_sha256: String,
    pub epoch: Epoch,
    pub issued_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub grant_generation: u64,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
/// A command is addressed to one control node, but the installation can serve
/// many nodes over its authenticated relay session. The node binding is
/// repeated in the signed grant claims and this envelope so UHC can verify it
/// without a process-wide node allowlist.
pub struct CommandEnvelope {
    pub protocol_version: u16,
    pub installation_id: InstallationId,
    pub control_node_id: ControlNodeId,
    pub epoch: Epoch,
    pub message_id: String,
    pub request_id: Uuid,
    pub idempotency_key: Uuid,
    pub created_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub payload: CommandPayload,
    pub grant: String,
}
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandStatus {
    Executed,
    Rejected,
    Expired,
    Busy,
    InstallationOffline,
    Forbidden,
    UnknownOutcome,
    StaleState,
}
pub type CommandResultCode = CommandStatus;
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CommandResult {
    pub protocol_version: u16,
    pub request_id: Uuid,
    pub idempotency_key: Uuid,
    pub status: CommandStatus,
    pub reason_code: Option<String>,
    pub epoch: Epoch,
    pub completed_at: TimestampMs,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct VolumeControl {
    pub value: f64,
    pub min: f64,
    pub max: f64,
    pub step: f64,
    pub is_muted: bool,
    #[serde(default)]
    pub scale: Option<String>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ZoneProjection {
    pub zone_handle: String,
    pub zone_name: String,
    pub state: String,
    #[serde(default)]
    pub volume_control: Option<VolumeControl>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NowPlayingProjection {
    pub zone_handle: String,
    pub title: String,
    pub artist: String,
    pub image_revision: Option<String>,
    pub is_playing: bool,
    pub volume: Option<f64>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StateSnapshot {
    pub protocol_version: u16,
    pub message_id: String,
    pub installation_id: InstallationId,
    pub epoch: Epoch,
    pub revision: u64,
    pub observed_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub zones: Vec<ZoneProjection>,
    pub now_playing: Vec<NowPlayingProjection>,
}
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", content = "value", rename_all = "snake_case")]
pub enum FieldPatch<T> {
    #[default]
    Unchanged,
    Set(T),
    Clear,
}
impl<T> FieldPatch<T> {
    pub fn is_unchanged(&self) -> bool {
        matches!(self, Self::Unchanged)
    }
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ZoneDelta {
    pub zone_handle: String,
    #[serde(default)]
    pub zone_name: Option<String>,
    #[serde(default)]
    pub state: Option<String>,
    #[serde(default, skip_serializing_if = "FieldPatch::is_unchanged")]
    pub volume_control: FieldPatch<VolumeControl>,
    #[serde(default, skip_serializing_if = "FieldPatch::is_unchanged")]
    pub now_playing: FieldPatch<NowPlayingProjection>,
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StateDelta {
    pub protocol_version: u16,
    pub message_id: String,
    pub installation_id: InstallationId,
    pub epoch: Epoch,
    pub revision: u64,
    pub observed_at: TimestampMs,
    pub expires_at: TimestampMs,
    pub zones: Vec<ZoneDelta>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ArtworkRelayRequest {
    pub protocol_version: u16,
    pub message_id: String,
    pub installation_id: InstallationId,
    pub epoch: Epoch,
    pub request_id: Uuid,
    pub zone_handle: String,
    pub artwork_revision: String,
    pub max_source_bytes: usize,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArtworkRelayResponse {
    pub protocol_version: u16,
    pub message_id: String,
    pub installation_id: InstallationId,
    pub epoch: Epoch,
    pub request_id: Uuid,
    pub artwork_revision: String,
    pub content_type: String,
    pub total_bytes: usize,
    pub chunk_count: u16,
    pub sha256: String,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtworkChunk {
    pub request_id: Uuid,
    pub index: u16,
    pub count: u16,
    pub bytes: Vec<u8>,
}
impl ArtworkChunk {
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        if self.count == 0
            || self.index >= self.count
            || self.bytes.is_empty()
            || self.bytes.len() > MAX_ARTWORK_CHUNK_BYTES
        {
            return Err("invalid artwork chunk");
        }
        let mut out = Vec::with_capacity(20 + self.bytes.len());
        out.extend_from_slice(self.request_id.as_bytes());
        out.extend_from_slice(&self.index.to_be_bytes());
        out.extend_from_slice(&self.count.to_be_bytes());
        out.extend_from_slice(&self.bytes);
        Ok(out)
    }
    pub fn decode(frame: &[u8]) -> Result<Self, &'static str> {
        if frame.len() <= 20 || frame.len() > 20 + MAX_ARTWORK_CHUNK_BYTES {
            return Err("invalid artwork chunk length");
        }
        let chunk = Self {
            request_id: Uuid::from_slice(&frame[..16]).map_err(|_| "invalid artwork request id")?,
            index: u16::from_be_bytes([frame[16], frame[17]]),
            count: u16::from_be_bytes([frame[18], frame[19]]),
            bytes: frame[20..].to_vec(),
        };
        if chunk.count == 0 || chunk.index >= chunk.count {
            return Err("invalid artwork chunk");
        }
        Ok(chunk)
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum ConnectorMessage {
    Hello(ConnectorHello),
    Heartbeat { epoch: Epoch, sent_at: TimestampMs },
    Snapshot(StateSnapshot),
    Delta(StateDelta),
    CommandResult(CommandResult),
    ArtworkResponse(ArtworkRelayResponse),
}
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "type", content = "body", rename_all = "snake_case")]
pub enum RelayMessage {
    Challenge(SessionChallengeMessage),
    SessionEstablished { epoch: Epoch },
    Heartbeat { epoch: Epoch, sent_at: TimestampMs },
    Command(CommandEnvelope),
    ArtworkRequest(ArtworkRelayRequest),
    Revoke { reason_code: String },
}

/// Parse relay traffic with duplicate-key rejection before serde can collapse
/// a repeated security field to its last value.
pub fn parse_relay_message(bytes: &[u8]) -> Result<RelayMessage, &'static str> {
    validate_message_bytes(bytes)?;
    if contains_duplicate_object_keys(bytes) {
        return Err("duplicate_object_key");
    }
    serde_json::from_slice(bytes).map_err(|_| "invalid_relay_message")
}

pub fn validate_id(id: &str) -> bool {
    (16..=128).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub fn validate_message_bytes(bytes: &[u8]) -> Result<(), &'static str> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        Err("message_too_large")
    } else {
        Ok(())
    }
}
pub fn parse_envelope(bytes: &[u8]) -> Result<WireEnvelope, &'static str> {
    validate_message_bytes(bytes)?;
    if contains_duplicate_object_keys(bytes) {
        return Err("duplicate_object_key");
    }
    let envelope: WireEnvelope = serde_json::from_slice(bytes).map_err(|_| "invalid_envelope")?;
    if envelope.protocol_version != PROTOCOL_VERSION
        || !validate_id(&envelope.installation_id)
        || !validate_id(&envelope.message_id)
        || envelope.epoch == 0
    {
        return Err("invalid_protocol_or_identity");
    }
    if let Some(payload) = &envelope.payload {
        if !within_value_limits(payload)
            || matches!(
                envelope.kind,
                WireType::StateSnapshot | WireType::StateDelta
            ) && payload
                .get("zones")
                .and_then(serde_json::Value::as_array)
                .is_some_and(|z| z.len() > MAX_ZONES)
        {
            return Err("message_limits_exceeded");
        }
    }
    Ok(envelope)
}
pub fn payload_hash(payload: &CommandPayload) -> Result<String, serde_json::Error> {
    Ok(hex::encode(Sha256::digest(canonical_json(
        &serde_json::to_value(payload)?,
    )?)))
}
pub fn canonical_json(value: &serde_json::Value) -> Result<Vec<u8>, serde_json::Error> {
    fn normalize(value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().cloned().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for key in keys {
                    out.insert(key.clone(), normalize(&map[&key]));
                }
                serde_json::Value::Object(out)
            }
            serde_json::Value::Array(values) => {
                serde_json::Value::Array(values.iter().map(normalize).collect())
            }
            serde_json::Value::Number(number) if number.is_f64() => {
                if let Some(value) = number.as_f64() {
                    const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;
                    if value.fract() == 0.0 && value.abs() <= MAX_SAFE_INTEGER {
                        serde_json::Value::Number(serde_json::Number::from(value as i64))
                    } else {
                        value.into()
                    }
                } else {
                    serde_json::Value::Number(number.clone())
                }
            }
            other => other.clone(),
        }
    }
    serde_json::to_vec(&normalize(value))
}
pub fn sha256_canonical(value: &serde_json::Value) -> Result<String, serde_json::Error> {
    Ok(format!(
        "sha256:{}",
        hex::encode(Sha256::digest(canonical_json(value)?))
    ))
}
fn within_value_limits(value: &serde_json::Value) -> bool {
    match value {
        serde_json::Value::Object(m) => {
            m.len() <= 24
                && m.keys().all(|k| k.len() <= MAX_STRING_BYTES)
                && m.values().all(within_value_limits)
        }
        serde_json::Value::Array(v) => v.iter().all(within_value_limits),
        serde_json::Value::String(s) => s.len() <= MAX_STRING_BYTES,
        _ => true,
    }
}
fn contains_duplicate_object_keys(bytes: &[u8]) -> bool {
    fn ws(b: &[u8], mut i: usize) -> usize {
        while b.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    }
    fn string_end(b: &[u8], mut i: usize) -> Option<usize> {
        if b.get(i) != Some(&b'"') {
            return None;
        }
        i += 1;
        while let Some(c) = b.get(i) {
            if *c == b'\\' {
                i = i.saturating_add(2);
            } else if *c == b'"' {
                return Some(i + 1);
            } else {
                i += 1;
            }
        }
        None
    }
    fn value(b: &[u8], mut i: usize) -> Option<usize> {
        i = ws(b, i);
        match b.get(i)? {
            b'{' => object(b, i),
            b'[' => {
                i += 1;
                loop {
                    i = ws(b, i);
                    if b.get(i) == Some(&b']') {
                        return Some(i + 1);
                    }
                    i = value(b, i)?;
                    i = ws(b, i);
                    match b.get(i)? {
                        b',' => i += 1,
                        b']' => return Some(i + 1),
                        _ => return None,
                    }
                }
            }
            b'"' => string_end(b, i),
            _ => {
                while let Some(c) = b.get(i) {
                    if matches!(c, b',' | b']' | b'}' | b' ' | b'\n' | b'\r' | b'\t') {
                        break;
                    }
                    i += 1;
                }
                Some(i)
            }
        }
    }
    fn object(b: &[u8], mut i: usize) -> Option<usize> {
        use std::collections::HashSet;
        let mut keys = HashSet::new();
        i += 1;
        loop {
            i = ws(b, i);
            if b.get(i) == Some(&b'}') {
                return Some(i + 1);
            }
            let start = i + 1;
            let end = string_end(b, i)?;
            if !keys.insert(b.get(start..end - 1)?.to_vec()) {
                return None;
            }
            i = ws(b, end);
            if b.get(i) != Some(&b':') {
                return None;
            }
            i = value(b, i + 1)?;
            i = ws(b, i);
            match b.get(i)? {
                b',' => i += 1,
                b'}' => return Some(i + 1),
                _ => return None,
            }
        }
    }
    value(bytes, 0).is_none()
}
