use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::identity::InstallationIdentity;
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionProof {
    pub relay_audience: String,
    pub installation_id: String,
    pub nonce: String,
    pub endpoint_path: String,
    pub issued_at: u64,
    pub signature: String,
}

/// Exact proof message accepted after the relay sends a challenge. The grant
/// is carried inside this JSON proof, never as a bearer header or URL value.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InstallationSessionProof {
    pub grant: String,
    pub challenge_id: Uuid,
    pub endpoint: String,
    pub nonce: String,
    pub proof_jti: Uuid,
    pub issued_at: i64,
    pub installation_signature: String,
}

pub type SessionChallengeMessage = super::protocol::SessionChallengeMessage;

/// Request body for the relay's authenticated session-grant ceremony.
/// The installation signs the exact request fields; the resulting grant is
/// still proof-bound to the WebSocket challenge and is never an HTTP bearer.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstallationGrantRequest {
    pub installation_id: String,
    pub request_id: Uuid,
    pub endpoint: String,
    pub issued_at: i64,
    pub signature: String,
}

/// Sign the installation-key request accepted by `POST /v1/relay/session-grant`.
/// The relay authenticates this proof before issuing the short-lived grant.
pub fn sign_installation_grant_request(
    identity: &InstallationIdentity,
    endpoint: String,
    issued_at: i64,
) -> InstallationGrantRequest {
    let mut request = InstallationGrantRequest {
        installation_id: identity.installation_id().to_owned(),
        request_id: Uuid::new_v4(),
        endpoint,
        issued_at,
        signature: String::new(),
    };
    let message = serde_json::json!({
        "audience": "hiphi-relay",
        "endpoint": request.endpoint,
        "installation_id": request.installation_id,
        "issued_at": request.issued_at,
        "request_id": request.request_id,
    });
    let bytes = super::protocol::canonical_json(&message).unwrap_or_default();
    request.signature =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(identity.sign(&bytes).to_bytes());
    request
}

pub fn sign_installation_session_proof(
    identity: &InstallationIdentity,
    grant: String,
    challenge: &SessionChallengeMessage,
    now_ms: i64,
) -> InstallationSessionProof {
    let mut proof = InstallationSessionProof {
        grant,
        challenge_id: challenge.challenge_id,
        endpoint: challenge.endpoint.clone(),
        nonce: challenge.nonce.clone(),
        proof_jti: Uuid::new_v4(),
        issued_at: now_ms,
        installation_signature: String::new(),
    };
    let endpoint_path = url::Url::parse(&proof.endpoint)
        .map(|url| url.path().to_owned())
        .unwrap_or_default();
    let binding = serde_json::json!({"audience":"hiphi-relay","installation_id":identity.installation_id(),"nonce":proof.nonce,"endpoint_path":endpoint_path,"issued_at":proof.issued_at});
    let bytes = super::protocol::canonical_json(&binding).unwrap_or_default();
    proof.installation_signature =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(identity.sign(&bytes).to_bytes());
    proof
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SessionError {
    #[error("session proof has invalid audience, endpoint, identity, or time")]
    InvalidClaims,
    #[error("session proof signature is invalid")]
    InvalidSignature,
    #[error("session nonce was already consumed")]
    ReplayedNonce,
}

impl SessionProof {
    pub fn sign(
        identity: &InstallationIdentity,
        audience: &str,
        nonce: &str,
        endpoint_path: &str,
        issued_at: u64,
    ) -> Self {
        let claims = canonical_claims(
            audience,
            identity.installation_id(),
            nonce,
            endpoint_path,
            issued_at,
        );
        let signature = identity.sign(claims.as_bytes());
        Self {
            relay_audience: audience.to_owned(),
            installation_id: identity.installation_id().to_owned(),
            nonce: nonce.to_owned(),
            endpoint_path: endpoint_path.to_owned(),
            issued_at,
            signature: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(signature.to_bytes()),
        }
    }
}

pub struct SessionVerifier {
    relay_audience: String,
    endpoint_path: String,
    seen_nonces: std::collections::HashSet<String>,
    nonce_validity_ms: u64,
}

impl SessionVerifier {
    pub fn new(relay_audience: impl Into<String>, endpoint_path: impl Into<String>) -> Self {
        Self {
            relay_audience: relay_audience.into(),
            endpoint_path: endpoint_path.into(),
            seen_nonces: Default::default(),
            nonce_validity_ms: 60_000,
        }
    }
    pub fn verify(
        &mut self,
        proof: &SessionProof,
        key: &VerifyingKey,
        now_ms: u64,
        expected_installation: &str,
    ) -> Result<(), SessionError> {
        if proof.relay_audience != self.relay_audience
            || proof.endpoint_path != self.endpoint_path
            || proof.installation_id != expected_installation
            || now_ms.abs_diff(proof.issued_at) > self.nonce_validity_ms
            || !self.seen_nonces.insert(proof.nonce.clone())
        {
            return if self.seen_nonces.contains(&proof.nonce) {
                Err(SessionError::ReplayedNonce)
            } else {
                Err(SessionError::InvalidClaims)
            };
        }
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&proof.signature)
            .map_err(|_| SessionError::InvalidSignature)?;
        let signature =
            Signature::from_slice(&bytes).map_err(|_| SessionError::InvalidSignature)?;
        key.verify(
            canonical_claims(
                &proof.relay_audience,
                &proof.installation_id,
                &proof.nonce,
                &proof.endpoint_path,
                proof.issued_at,
            )
            .as_bytes(),
            &signature,
        )
        .map_err(|_| SessionError::InvalidSignature)
    }
}

fn canonical_claims(
    audience: &str,
    installation: &str,
    nonce: &str,
    endpoint: &str,
    issued_at: u64,
) -> String {
    format!("{audience}\n{installation}\n{nonce}\n{endpoint}\n{issued_at}")
}

use base64::Engine as _;
#[allow(dead_code)]
fn _claims_digest(claims: &str) -> [u8; 32] {
    Sha256::digest(claims.as_bytes()).into()
}
