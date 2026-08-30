use ed25519_dalek::{Signature, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    identity::InstallationIdentity,
    protocol::{InstallationSessionGrantClaims, PROTOCOL_VERSION, RELAY_AUDIENCE},
    transport::RelayEndpoint,
};
use uuid::Uuid;

const SESSION_GRANT_ISSUER: &str = "hiphi-installation-authorization";
const SESSION_JWS_TYPE: &str = "hiphi-session+jwt";
const MAX_SESSION_GRANT_TTL_MS: i64 = 120_000;
const MAX_CLOCK_SKEW_MS: i64 = 60_000;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionProof {
    pub relay_audience: String,
    pub installation_id: String,
    pub nonce: String,
    pub endpoint_path: String,
    pub issued_at: u64,
    pub signature: String,
}

/// Exact proof message accepted after the relay sends a challenge. The same
/// grant that authenticated the HTTP upgrade is carried inside this proof so
/// the relay can bind it to installation-key possession. It is never a URL.
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedInstallationSessionGrant {
    pub installation_id: String,
    pub grant_generation: u64,
    pub grant_jti: Uuid,
}

#[derive(Debug, thiserror::Error, Eq, PartialEq)]
pub enum SessionGrantError {
    #[error("session grant is malformed")]
    Malformed,
    #[error("session grant key is not pinned")]
    UnknownKey,
    #[error("session grant signature is invalid")]
    InvalidSignature,
    #[error("session grant claims do not match this connector")]
    WrongBinding,
    #[error("session grant is expired or outside clock tolerance")]
    Expired,
}

/// Verify relay session authority locally before sending it in the WebSocket
/// Authorization header. The relay remains untrusted: only the pinned
/// authorizer key can produce a grant accepted here.
pub fn verify_installation_session_grant(
    grant: &str,
    expected_key_id: &str,
    issuer_key: &VerifyingKey,
    identity: &InstallationIdentity,
    expected_endpoint: &str,
    now_ms: i64,
) -> Result<VerifiedInstallationSessionGrant, SessionGrantError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Header {
        alg: String,
        kid: String,
        typ: String,
    }

    let mut parts = grant.split('.');
    let (Some(encoded_header), Some(encoded_claims), Some(encoded_signature), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(SessionGrantError::Malformed);
    };
    let header: Header = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_header)
            .map_err(|_| SessionGrantError::Malformed)?,
    )
    .map_err(|_| SessionGrantError::Malformed)?;
    if header.alg != "EdDSA" || header.typ != SESSION_JWS_TYPE {
        return Err(SessionGrantError::Malformed);
    }
    if header.kid != expected_key_id {
        return Err(SessionGrantError::UnknownKey);
    }
    let signature = Signature::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_signature)
            .map_err(|_| SessionGrantError::Malformed)?,
    )
    .map_err(|_| SessionGrantError::Malformed)?;
    issuer_key
        .verify(
            format!("{encoded_header}.{encoded_claims}").as_bytes(),
            &signature,
        )
        .map_err(|_| SessionGrantError::InvalidSignature)?;
    let claims: InstallationSessionGrantClaims = serde_json::from_slice(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_claims)
            .map_err(|_| SessionGrantError::Malformed)?,
    )
    .map_err(|_| SessionGrantError::Malformed)?;
    let expected_endpoint =
        RelayEndpoint::parse(expected_endpoint).map_err(|_| SessionGrantError::WrongBinding)?;
    let granted_endpoint =
        RelayEndpoint::parse(&claims.endpoint).map_err(|_| SessionGrantError::WrongBinding)?;
    let public_key_sha256 = hex::encode(Sha256::digest(identity.verifying_key().as_bytes()));
    if claims.protocol_version != PROTOCOL_VERSION
        || claims.issuer != SESSION_GRANT_ISSUER
        || claims.audience != RELAY_AUDIENCE
        || claims.installation_id != identity.installation_id()
        || granted_endpoint != expected_endpoint
        || claims.public_key_sha256 != public_key_sha256
    {
        return Err(SessionGrantError::WrongBinding);
    }
    if claims.issued_at > now_ms.saturating_add(MAX_CLOCK_SKEW_MS)
        || claims.expires_at <= now_ms
        || claims.expires_at.saturating_sub(claims.issued_at) > MAX_SESSION_GRANT_TTL_MS
    {
        return Err(SessionGrantError::Expired);
    }
    Ok(VerifiedInstallationSessionGrant {
        installation_id: claims.installation_id,
        grant_generation: claims.grant_generation,
        grant_jti: claims.grant_jti,
    })
}

/// Request body for the relay's authenticated session-grant ceremony.
/// The installation signs the exact request fields; the resulting grant is
/// still proof-bound to the WebSocket challenge and later authenticates the
/// WebSocket HTTP upgrade.
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
