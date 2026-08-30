use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::Deserialize;
use std::collections::HashMap;

/// A connector process is intentionally bounded even when the relay is
/// hostile.  The terminal ledger and replay cache use the same envelope
/// budget, but remain separate so a command cannot bypass signature replay
/// protection by evicting only a ledger row.
pub const MAX_REPLAY_REQUESTS: usize = 256;

use super::protocol::{
    payload_hash, CommandAction, CommandEnvelope, CommandGrantClaims, CommandPayload,
};

impl CommandPayload {
    pub fn canonical_hash(&self) -> Result<String, serde_json::Error> {
        payload_hash(self)
    }

    pub fn is_well_formed(&self) -> bool {
        if !(16..=128).contains(&self.zone_handle.len())
            || !self
                .zone_handle
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        {
            return false;
        }
        match self.action {
            CommandAction::VolumeAbsolute { value } => value.is_finite(),
            _ => true,
        }
    }
}

pub type GrantClaims = CommandGrantClaims;

#[derive(Clone, Debug, PartialEq)]
pub struct VerifiedCommand {
    pub claims: GrantClaims,
    pub payload: CommandPayload,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum GrantError {
    #[error("malformed grant")]
    Malformed,
    #[error("grant key is not pinned")]
    UnknownKey,
    #[error("grant signature is invalid")]
    InvalidSignature,
    #[error("grant claims do not bind this command")]
    BindingMismatch,
    #[error("grant audience is not this connector")]
    WrongAudience,
    #[error("grant generation has been revoked")]
    Revoked,
    #[error("grant is expired or outside clock tolerance")]
    Expired,
    #[error("request has already been seen")]
    Replayed,
    #[error("command replay cache is full")]
    AtCapacity,
}

pub struct CommandGrantVerifier {
    issuer: String,
    audience: String,
    installation_id: String,
    epoch: u64,
    generation: u64,
    keys: HashMap<String, VerifyingKey>,
    seen_requests: HashMap<String, i64>,
}

impl CommandGrantVerifier {
    pub fn new(
        issuer: impl Into<String>,
        audience: impl Into<String>,
        installation_id: impl Into<String>,
        epoch: u64,
        generation: u64,
    ) -> Self {
        Self {
            issuer: issuer.into(),
            audience: audience.into(),
            installation_id: installation_id.into(),
            epoch,
            generation,
            keys: HashMap::new(),
            seen_requests: Default::default(),
        }
    }
    pub fn pin_key(&mut self, key_id: impl Into<String>, key: VerifyingKey) {
        self.keys.insert(key_id.into(), key);
    }
    pub fn revoke_generation(&mut self, generation: u64) {
        self.generation = generation;
        self.seen_requests.clear();
    }

    pub fn seen_requests_len(&self) -> usize {
        self.seen_requests.len()
    }
    pub fn verify(
        &mut self,
        envelope: &CommandEnvelope,
        now_ms: i64,
    ) -> Result<VerifiedCommand, GrantError> {
        if envelope.protocol_version != 1 || envelope.expires_at <= now_ms {
            return Err(GrantError::Expired);
        }
        let (header, claims, signature) = decode_jws(&envelope.grant)?;
        let key = self
            .keys
            .get(&header.key_id)
            .ok_or(GrantError::UnknownKey)?;
        key.verify(
            format!("{}.{}", header.encoded, claims.encoded).as_bytes(),
            &signature,
        )
        .map_err(|_| GrantError::InvalidSignature)?;
        let claims: GrantClaims =
            serde_json::from_slice(&claims.bytes).map_err(|_| GrantError::Malformed)?;
        if claims.expires_at <= now_ms
            || claims.issued_at > now_ms.saturating_add(60_000)
            || claims.expires_at.saturating_sub(claims.issued_at) > 15_000
        {
            return Err(GrantError::Expired);
        }
        if claims.grant_generation < self.generation {
            return Err(GrantError::Revoked);
        }
        if claims.audience != self.audience {
            return Err(GrantError::WrongAudience);
        }
        if !envelope.payload.is_well_formed() {
            return Err(GrantError::BindingMismatch);
        }
        let expected_hash = envelope
            .payload
            .canonical_hash()
            .map_err(|_| GrantError::Malformed)?;
        if claims.protocol_version != 1
            || envelope.installation_id != self.installation_id
            || envelope.epoch != self.epoch
            || claims.issuer != self.issuer
            || claims.installation_id != self.installation_id
            || claims.control_node_id != envelope.control_node_id
            || claims.epoch != self.epoch
            || claims.grant_generation != self.generation
            || claims.request_id != envelope.request_id
            || claims.idempotency_key != envelope.idempotency_key
            || claims.scope != envelope.payload.action.scope()
            || claims.payload_sha256 != expected_hash
            || claims.expires_at != envelope.expires_at
        {
            return Err(GrantError::BindingMismatch);
        }
        self.seen_requests
            .retain(|_, seen_at| now_ms.saturating_sub(*seen_at) <= 60_000);
        if self
            .seen_requests
            .contains_key(&envelope.request_id.to_string())
        {
            return Err(GrantError::Replayed);
        }
        if self.seen_requests.len() >= MAX_REPLAY_REQUESTS {
            return Err(GrantError::AtCapacity);
        }
        self.seen_requests
            .insert(envelope.request_id.to_string(), now_ms);
        Ok(VerifiedCommand {
            claims,
            payload: envelope.payload.clone(),
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CommandOutcome {
    Executed,
    Rejected,
    Expired,
    Busy,
    InstallationOffline,
    Forbidden,
    UnknownOutcome,
    StaleState,
}

#[derive(Default)]
pub struct CommandLedger {
    terminal: HashMap<String, LedgerEntry>,
}

#[derive(Clone, Debug)]
struct LedgerEntry {
    outcome: CommandOutcome,
    recorded_at: u64,
    payload_hash: Option<String>,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum LedgerError {
    #[error("command result ledger is full")]
    AtCapacity,
    #[error("idempotency key was reused for a different command payload")]
    Conflict,
}
impl CommandLedger {
    pub fn record(&mut self, request_id: impl Into<String>, outcome: CommandOutcome) {
        self.record_at(request_id, outcome, 0);
    }
    pub fn record_at(
        &mut self,
        request_id: impl Into<String>,
        outcome: CommandOutcome,
        now_ms: u64,
    ) {
        self.expire(now_ms);
        let request_id = request_id.into();
        if self.terminal.len() < 256 || self.terminal.contains_key(&request_id) {
            self.terminal.insert(
                request_id,
                LedgerEntry {
                    outcome,
                    recorded_at: now_ms,
                    payload_hash: None,
                },
            );
        }
    }
    fn get(&self, request_id: &str) -> Option<CommandOutcome> {
        self.terminal.get(request_id).map(|entry| entry.outcome)
    }
    pub fn get_at(&mut self, request_id: &str, now_ms: u64) -> Option<CommandOutcome> {
        self.expire(now_ms);
        self.get(request_id)
    }

    pub fn len(&self) -> usize {
        self.terminal.len()
    }

    pub fn is_empty(&self) -> bool {
        self.terminal.is_empty()
    }
    pub fn is_full(&self) -> bool {
        self.terminal.len() >= 256
    }
    pub fn lookup_command_at(
        &mut self,
        idempotency_key: &str,
        payload_hash: &str,
        now_ms: u64,
    ) -> Result<Option<CommandOutcome>, LedgerError> {
        self.expire(now_ms);
        match self.terminal.get(idempotency_key) {
            Some(entry) if entry.payload_hash.as_deref() == Some(payload_hash) => {
                Ok(Some(entry.outcome))
            }
            Some(_) => Err(LedgerError::Conflict),
            None => Ok(None),
        }
    }
    pub fn record_command_at(
        &mut self,
        idempotency_key: impl Into<String>,
        payload_hash: &str,
        outcome: CommandOutcome,
        now_ms: u64,
    ) -> Result<(), LedgerError> {
        self.expire(now_ms);
        let idempotency_key = idempotency_key.into();
        if let Some(entry) = self.terminal.get(&idempotency_key) {
            return if entry.payload_hash.as_deref() == Some(payload_hash) {
                Ok(())
            } else {
                Err(LedgerError::Conflict)
            };
        }
        if self.is_full() {
            return Err(LedgerError::AtCapacity);
        }
        self.terminal.insert(
            idempotency_key,
            LedgerEntry {
                outcome,
                recorded_at: now_ms,
                payload_hash: Some(payload_hash.to_owned()),
            },
        );
        Ok(())
    }
    fn expire(&mut self, now_ms: u64) {
        self.terminal.retain(|_, entry| {
            entry.recorded_at == 0 || now_ms.saturating_sub(entry.recorded_at) <= 60_000
        });
    }
}

#[derive(Deserialize)]
struct JwsHeader {
    alg: String,
    #[allow(dead_code)]
    typ: Option<String>,
    #[serde(rename = "kid")]
    key_id: String,
    #[serde(skip)]
    encoded: String,
}
struct EncodedPart {
    bytes: Vec<u8>,
    encoded: String,
}
fn decode_jws(input: &str) -> Result<(JwsHeader, EncodedPart, Signature), GrantError> {
    use base64::Engine as _;
    let mut parts = input.split('.');
    let (h, p, s) = (parts.next(), parts.next(), parts.next());
    if parts.next().is_some() || h.is_none() || p.is_none() || s.is_none() {
        return Err(GrantError::Malformed);
    }
    let (Some(h), Some(p), Some(s)) = (h, p, s) else {
        return Err(GrantError::Malformed);
    };
    let header_bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(h)
        .map_err(|_| GrantError::Malformed)?;
    let mut header: JwsHeader =
        serde_json::from_slice(&header_bytes).map_err(|_| GrantError::Malformed)?;
    if header.alg != "EdDSA" || header.key_id.is_empty() {
        return Err(GrantError::Malformed);
    }
    header.encoded = h.to_owned();
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(p)
        .map_err(|_| GrantError::Malformed)?;
    let signature = Signature::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(s)
            .map_err(|_| GrantError::Malformed)?,
    )
    .map_err(|_| GrantError::Malformed)?;
    Ok((
        header,
        EncodedPart {
            bytes: payload,
            encoded: p.to_owned(),
        },
        signature,
    ))
}
