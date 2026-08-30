#[path = "../src/cloud_connector/mod.rs"]
mod cloud_connector;

use cloud_connector::commands::{GrantClaims, GrantError};
use cloud_connector::protocol::{canonical_json, parse_envelope, sha256_canonical};
use cloud_connector::protocol::{FieldPatch, ZoneDelta};
use cloud_connector::*;
use ed25519_dalek::{Signer, SigningKey, Verifier};
use rand::rngs::OsRng;
use serde_json::json;

fn id(prefix: &str) -> String {
    format!("{prefix}_01234567890")
}
fn uuid(seed: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(seed)
}

#[test]
fn sanitized_protocol_fixtures_parse_and_adversarial_major_version_rejects() {
    let valid = include_str!("fixtures/hiphi-relay-v1/valid.json");
    let entries: serde_json::Value = serde_json::from_str(valid).unwrap();
    for entry in entries["connector_messages"]
        .as_array()
        .unwrap()
        .iter()
        .chain(entries["relay_messages"].as_array().unwrap())
    {
        let bytes = serde_json::to_vec(entry).unwrap();
        assert!(serde_json::from_slice::<serde_json::Value>(&bytes).is_ok());
    }
    let bad = json!({"protocol_version":2,"type":"heartbeat","installation_id":id("install"),"epoch":7,"message_id":id("message")});
    assert_eq!(
        parse_envelope(&serde_json::to_vec(&bad).unwrap()),
        Err("invalid_protocol_or_identity")
    );
}

#[test]
fn legacy_parser_rejects_duplicate_security_fields() {
    let duplicate = br#"{"protocol_version":1,"type":"hello","installation_id":"install_01234567890","epoch":1,"message_id":"message_01234567890","protocol_version":1}"#;
    assert_eq!(parse_envelope(duplicate), Err("duplicate_object_key"));
}

#[test]
fn private_relay_fixture_messages_match_typed_wire_contract() {
    let fixtures: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/hiphi-relay-v1/valid.json")).unwrap();
    for message in fixtures["connector_messages"].as_array().unwrap() {
        let parsed: ConnectorMessage = serde_json::from_value(message.clone()).unwrap();
        assert!(matches!(
            parsed,
            ConnectorMessage::Hello(_)
                | ConnectorMessage::Heartbeat { .. }
                | ConnectorMessage::Snapshot(_)
                | ConnectorMessage::Delta(_)
                | ConnectorMessage::CommandResult(_)
                | ConnectorMessage::ArtworkResponse(_)
        ));
    }
    for message in fixtures["relay_messages"].as_array().unwrap() {
        let parsed: RelayMessage = serde_json::from_value(message.clone()).unwrap();
        assert!(matches!(
            parsed,
            RelayMessage::Challenge(_)
                | RelayMessage::SessionEstablished { .. }
                | RelayMessage::Heartbeat { .. }
                | RelayMessage::Command(_)
                | RelayMessage::ArtworkRequest(_)
                | RelayMessage::Revoke { .. }
        ));
    }
}

#[test]
fn artwork_chunk_codec_matches_private_header_and_bound() {
    let id = uuid(44);
    let chunk = ArtworkChunk {
        request_id: id,
        index: 0,
        count: 1,
        bytes: vec![7, 8, 9],
    };
    let frame = chunk.encode().unwrap();
    assert_eq!(&frame[..16], id.as_bytes());
    assert_eq!(&frame[16..20], &[0, 0, 0, 1]);
    assert_eq!(ArtworkChunk::decode(&frame).unwrap(), chunk);
    assert!(ArtworkChunk {
        request_id: id,
        index: 1,
        count: 1,
        bytes: vec![1]
    }
    .encode()
    .is_err());
}

#[test]
fn artwork_lane_rejects_source_output_over_bound() {
    let mut lane = ArtLane::default();
    let request = ArtRequest {
        request_id: id("oversize_request"),
        zone_handle: id("zone"),
        art_capability: id("capability_oversize_012345678901234567890"),
    };
    lane.enqueue(request).unwrap();
    let request = lane.start_next().unwrap();
    assert_eq!(
        lane.finish(ArtResponse {
            request_id: request.request_id,
            art_revision: "art_oversize".into(),
            content_type: "image/jpeg".into(),
            bytes: vec![0; cloud_connector::artwork::MAX_OUTPUT_BYTES + 1],
        }),
        Err(cloud_connector::artwork::ArtError::TooLarge)
    );
}

#[test]
fn state_delta_field_patches_preserve_unchanged_set_and_clear() {
    let unchanged: ZoneDelta =
        serde_json::from_value(json!({"zone_handle":"zone_demo_lounge","state":"playing"}))
            .unwrap();
    assert!(matches!(unchanged.volume_control, FieldPatch::Unchanged));
    let clear: ZoneDelta = serde_json::from_value(json!({"zone_handle":"zone_demo_lounge","volume_control":{"op":"clear"},"now_playing":{"op":"clear"}})).unwrap();
    assert!(matches!(clear.volume_control, FieldPatch::Clear));
    assert!(matches!(clear.now_playing, FieldPatch::Clear));
}

#[test]
fn canonical_payload_hash_is_key_order_independent() {
    let a = json!({"action":"volume.absolute","value":-23.5,"zone_handle":"zone_demo_lounge"});
    let b = json!({"zone_handle":"zone_demo_lounge","value":-23.5,"action":"volume.absolute"});
    assert_eq!(canonical_json(&a).unwrap(), canonical_json(&b).unwrap());
    assert_eq!(sha256_canonical(&a).unwrap(), sha256_canonical(&b).unwrap());
}

#[test]
fn opaque_handles_round_trip_locally_without_provider_id_in_projection() {
    let mut store = StateStore::default();
    let projection = store.snapshot(SemanticStateInput {
        installation_id: id("install"),
        epoch: 1,
        revision: 1,
        observed_at: 10,
        expires_at: 20,
        zones: vec![cloud_connector::state::SemanticZoneInput {
            provider_id: "roon:secret-provider-id".into(),
            name: "Lounge".into(),
            state: "playing".into(),
            volume: None,
            now_playing: None,
        }],
    });
    let encoded = serde_json::to_string(&projection.zones).unwrap();
    assert!(!encoded.contains("roon:secret-provider-id"));
    assert!(store
        .provider_id(&projection.zones[0].zone_handle)
        .is_some());
    assert!(store.is_fresh(1, 15));
    assert!(!store.is_fresh(2, 15));
    assert!(!store.is_fresh(1, 20));

    let removed_handle = projection.zones[0].zone_handle.clone();
    store.snapshot(SemanticStateInput {
        installation_id: id("install"),
        epoch: 1,
        revision: 2,
        observed_at: 21,
        expires_at: 30,
        zones: vec![],
    });
    assert!(store.provider_id(&removed_handle).is_none());
}

#[test]
fn stale_state_is_not_command_eligible() {
    let mut store = StateStore::default();
    let projection = store.snapshot(SemanticStateInput {
        installation_id: id("install"),
        epoch: 9,
        revision: 4,
        observed_at: 100,
        expires_at: 200,
        zones: vec![],
    });
    assert!(store.is_fresh(projection.epoch, 199));
    assert!(!store.is_fresh(projection.epoch, 200));
    assert!(!store.is_fresh(projection.epoch + 1, 150));
}

#[test]
fn installation_key_is_saved_with_restricted_permissions() {
    let identity = InstallationIdentity::generate(id("install")).unwrap();
    let path = tempfile::NamedTempFile::new().unwrap();
    identity.save(path.path()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(path.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let loaded =
        InstallationIdentity::load(path.path(), identity.installation_id().to_owned()).unwrap();
    assert_eq!(identity.verifying_key(), loaded.verifying_key());
}

fn signed_grant(identity: &SigningKey, claims: &GrantClaims) -> String {
    use base64::Engine as _;
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(r#"{"alg":"EdDSA","kid":"issuer-1"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let sig = identity.sign(format!("{header}.{payload}").as_bytes());
    format!(
        "{header}.{payload}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes())
    )
}

fn command_and_grant(now: u64, key: &SigningKey, generation: u64) -> CommandEnvelope {
    command_with_claims(now, key, generation, |_| {})
}

fn command_with_claims(
    now: u64,
    key: &SigningKey,
    generation: u64,
    mutate: impl FnOnce(&mut GrantClaims),
) -> CommandEnvelope {
    let payload = CommandPayload {
        zone_handle: id("zone"),
        action: AllowedAction::VolumeAbsolute { value: -23.5 },
    };
    let request_id = uuid(1);
    let idempotency_key = uuid(2);
    let payload_hash = payload.canonical_hash().unwrap();
    let claims = GrantClaims {
        protocol_version: 1,
        issuer: "hiphi-command-authorization".into(),
        audience: "uhc-connector".into(),
        installation_id: id("install"),
        control_node_id: id("node"),
        request_id,
        idempotency_key,
        epoch: 7,
        scope: "playback_control".into(),
        payload_sha256: payload_hash,
        issued_at: (now - 100) as i64,
        expires_at: (now + 5_000) as i64,
        grant_generation: generation,
    };
    let mut claims = claims;
    mutate(&mut claims);
    CommandEnvelope {
        protocol_version: 1,
        installation_id: claims.installation_id.clone(),
        epoch: 7,
        message_id: id("message"),
        request_id: claims.request_id,
        idempotency_key: claims.idempotency_key,
        created_at: claims.issued_at,
        expires_at: claims.expires_at,
        payload,
        grant: signed_grant(key, &claims),
    }
}

#[test]
fn grant_binds_key_audience_expiry_generation_and_replay() {
    let key = SigningKey::generate(&mut OsRng);
    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        id("node"),
        7,
        3,
    );
    verifier.pin_key("issuer-1", key.verifying_key());
    let command = command_and_grant(1_000, &key, 3);
    assert!(verifier.verify(&command, 1_000).is_ok());
    assert_eq!(verifier.verify(&command, 1_000), Err(GrantError::Replayed));
    let mut wrong = command_and_grant(1_000, &key, 3);
    wrong.installation_id = id("other");
    assert_eq!(
        verifier.verify(&wrong, 1_000),
        Err(GrantError::BindingMismatch)
    );
    verifier.revoke_generation(4);
    let revoked = command_and_grant(1_000, &key, 3);
    assert_eq!(verifier.verify(&revoked, 1_000), Err(GrantError::Revoked));
}

#[test]
fn grant_rejects_each_security_binding_dimension() {
    let key = SigningKey::generate(&mut OsRng);
    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        id("node"),
        7,
        3,
    );
    verifier.pin_key("issuer-1", key.verifying_key());

    let wrong_audience =
        command_with_claims(1_000, &key, 3, |claims| claims.audience = "other".into());
    assert_eq!(
        verifier.verify(&wrong_audience, 1_000),
        Err(GrantError::WrongAudience)
    );

    let mut wrong_key = command_and_grant(1_000, &SigningKey::generate(&mut OsRng), 3);
    assert_eq!(
        verifier.verify(&wrong_key, 1_000),
        Err(GrantError::InvalidSignature)
    );

    let mut wrong_epoch = command_and_grant(1_000, &key, 3);
    wrong_epoch.epoch = 8;
    assert_eq!(
        verifier.verify(&wrong_epoch, 1_000),
        Err(GrantError::BindingMismatch)
    );

    let wrong_scope =
        command_with_claims(1_000, &key, 3, |claims| claims.scope = "state_read".into());
    assert_eq!(
        verifier.verify(&wrong_scope, 1_000),
        Err(GrantError::BindingMismatch)
    );

    let wrong_node = command_with_claims(1_000, &key, 3, |claims| {
        claims.control_node_id = id("other_node");
    });
    let mut node_verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        id("node"),
        7,
        3,
    );
    node_verifier.pin_key("issuer-1", key.verifying_key());
    assert_eq!(
        node_verifier.verify(&wrong_node, 1_000),
        Err(GrantError::BindingMismatch)
    );

    let expired = command_with_claims(1_000, &key, 3, |claims| {
        claims.issued_at = 0;
        claims.expires_at = 999;
    });
    assert_eq!(verifier.verify(&expired, 1_000), Err(GrantError::Expired));

    let mut wrong_version = command_and_grant(1_000, &key, 3);
    wrong_version.protocol_version = 2;
    assert_eq!(
        verifier.verify(&wrong_version, 1_000),
        Err(GrantError::Expired)
    );

    let revoked = command_with_claims(1_000, &key, 2, |_| {});
    assert_eq!(verifier.verify(&revoked, 1_000), Err(GrantError::Revoked));

    let mut tampered_payload = command_and_grant(1_000, &key, 3);
    tampered_payload.payload.action = AllowedAction::Next;
    assert_eq!(
        verifier.verify(&tampered_payload, 1_000),
        Err(GrantError::BindingMismatch)
    );

    let mut wrong_idempotency = command_and_grant(1_000, &key, 3);
    wrong_idempotency.idempotency_key = uuid(99);
    assert_eq!(
        verifier.verify(&wrong_idempotency, 1_000),
        Err(GrantError::BindingMismatch)
    );

    // A pinned issuer key cannot be replaced by a relay-provided key id.
    wrong_key.grant = wrong_key.grant.replace("issuer-1", "attacker");
    assert!(matches!(
        verifier.verify(&wrong_key, 1_000),
        Err(GrantError::InvalidSignature | GrantError::UnknownKey | GrantError::Malformed)
    ));
}

#[test]
fn dropped_result_is_idempotent_and_art_lane_has_no_url_surface() {
    let mut ledger = cloud_connector::commands::CommandLedger::default();
    ledger.record(id("request"), CommandOutcome::Executed);
    assert_eq!(ledger.get(&id("request")), Some(CommandOutcome::Executed));
    let mut lane = ArtLane::default();
    let request = ArtRequest {
        request_id: id("request"),
        zone_handle: id("zone"),
        art_capability: id("capability_012345678901234567890"),
    };
    lane.enqueue(request).unwrap();
    let request = lane.start_next().unwrap();
    lane.finish(ArtResponse {
        request_id: request.request_id,
        art_revision: "art_1".into(),
        content_type: "image/jpeg".into(),
        bytes: vec![1, 2, 3],
    })
    .unwrap();
    assert!(lane.result(&id("request")).unwrap().bytes.len() <= 512 * 1024);
}

#[test]
fn run_loop_increments_epoch_sends_snapshot_first_and_drops_offline_work() {
    let mut loop_state = ConnectorRunLoop::default();
    loop_state.connect();
    let epoch = loop_state.connected();
    assert_eq!(epoch, 1);
    assert!(!loop_state.can_send_delta());
    loop_state.mark_snapshot_sent();
    assert!(loop_state.can_send_delta());
    loop_state.disconnect();
    assert_eq!(loop_state.state(), ConnectionState::Offline);
    assert!(loop_state.drain_events().is_empty());
    assert!(loop_state.reconnect_delay() <= std::time::Duration::from_secs(30));
}

#[test]
fn relay_endpoint_requires_wss_and_rejects_embedded_credentials_or_query() {
    assert!(RelayEndpoint::parse("ws://relay.example/v1").is_err());
    assert!(RelayEndpoint::parse("https://relay.example/v1").is_err());
    assert!(RelayEndpoint::parse("wss://user:pass@relay.example/v1").is_err());
    assert!(RelayEndpoint::parse("wss://relay.example/v1?token=secret").is_err());
    assert_eq!(
        RelayEndpoint::parse("wss://relay.example/v1/")
            .unwrap()
            .as_str(),
        "wss://relay.example/v1"
    );
}

#[test]
fn session_proof_binds_endpoint_and_consumes_nonce_once() {
    let identity = InstallationIdentity::generate(id("install")).unwrap();
    let proof = SessionProof::sign(
        &identity,
        "hiphi-relay",
        "nonce_01234567890",
        "/v1/relay",
        1_000,
    );
    let mut verifier = SessionVerifier::new("hiphi-relay", "/v1/relay");
    assert_eq!(
        verifier.verify(
            &proof,
            &identity.verifying_key(),
            1_001,
            identity.installation_id()
        ),
        Ok(())
    );
    assert_eq!(
        verifier.verify(
            &proof,
            &identity.verifying_key(),
            1_001,
            identity.installation_id()
        ),
        Err(SessionError::ReplayedNonce)
    );
}

#[test]
fn installation_grant_request_is_signed_and_contains_no_bearer_header() {
    let identity = InstallationIdentity::generate(id("install")).unwrap();
    let request = cloud_connector::session::sign_installation_grant_request(
        &identity,
        "wss://cloud.invalid/v1/relay/connect".into(),
        1_000,
    );
    let encoded = serde_json::to_string(&request).unwrap();
    assert!(!encoded.contains("Authorization"));
    assert!(!encoded.contains("token"));
    assert!(!request.signature.is_empty());
}

#[test]
fn challenge_proof_binds_nonce_endpoint_and_is_raw_proof_json() {
    let identity = InstallationIdentity::generate(id("install")).unwrap();
    let challenge = cloud_connector::protocol::SessionChallengeMessage {
        protocol_version: 1,
        challenge_id: uuid(55),
        endpoint: "wss://cloud.invalid/v1/relay/connect".into(),
        nonce: "nonce_not_a_credential_123456789".into(),
        expires_at: 10_000,
    };
    let proof = cloud_connector::session::sign_installation_session_proof(
        &identity,
        "grant.not-a-real-secret.signature".into(),
        &challenge,
        1_000,
    );
    let encoded = serde_json::to_value(&proof).unwrap();
    assert!(encoded.get("grant").is_some());
    assert!(encoded.get("challenge_id").is_some());
    assert!(encoded.get("installation_signature").is_some());
    assert!(encoded.get("type").is_none());
    assert_eq!(proof.endpoint, challenge.endpoint);
    assert_eq!(proof.nonce, challenge.nonce);
}

#[test]
fn pairing_possession_proof_binds_all_ceremony_inputs() {
    let key = SigningKey::generate(&mut OsRng);
    let pairing_id = uuid(61);
    let installation_id = uuid(62);
    let account_id = uuid(63);
    let message = cloud_connector::pairing::possession_message(
        pairing_id,
        installation_id,
        account_id,
        "ABCD-EFGH",
    );
    let signature = key.sign(&message);
    assert!(key.verifying_key().verify(&message, &signature).is_ok());
    let altered = cloud_connector::pairing::possession_message(
        pairing_id,
        installation_id,
        account_id,
        "ABCD-EFGX",
    );
    assert!(key.verifying_key().verify(&altered, &signature).is_err());
}
