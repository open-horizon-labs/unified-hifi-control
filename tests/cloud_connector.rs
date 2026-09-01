use unified_hifi_control::cloud_connector;

use cloud_connector::commands::{GrantClaims, GrantError, MAX_REPLAY_REQUESTS};
use cloud_connector::protocol::{
    canonical_json, parse_envelope, parse_relay_message, sha256_canonical,
};
use cloud_connector::protocol::{FieldPatch, ZoneDelta};
use cloud_connector::*;
use ed25519_dalek::{Signer, SigningKey, Verifier};
use rand::rngs::OsRng;
use serde_json::json;
use std::time::Duration;

fn id(prefix: &str) -> String {
    format!("{prefix}_01234567890")
}
fn uuid(seed: u128) -> uuid::Uuid {
    uuid::Uuid::from_u128(seed)
}

fn signed_session_grant(
    key: &SigningKey,
    claims: &cloud_connector::protocol::InstallationSessionGrantClaims,
) -> String {
    signed_session_grant_with_kid("issuer-1", key, claims)
}

fn signed_session_grant_with_kid(
    key_id: &str,
    key: &SigningKey,
    claims: &cloud_connector::protocol::InstallationSessionGrantClaims,
) -> String {
    use base64::Engine as _;
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(format!(
        r#"{{"alg":"EdDSA","kid":"{key_id}","typ":"hiphi-session+jwt"}}"#
    ));
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let signature = key.sign(format!("{header}.{payload}").as_bytes());
    format!(
        "{header}.{payload}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(signature.to_bytes())
    )
}

#[test]
fn session_issuer_rotation_overlaps_then_retires_the_old_key() {
    use sha2::{Digest as _, Sha256};

    let old = SigningKey::generate(&mut OsRng);
    let new = SigningKey::generate(&mut OsRng);
    let identity =
        InstallationIdentity::generate("11111111-1111-4111-8111-111111111111".to_owned()).unwrap();
    let now = 1_800_000_000_000_i64;
    let claims = cloud_connector::protocol::InstallationSessionGrantClaims {
        protocol_version: 1,
        connector_version: env!("UHC_VERSION").into(),
        issuer: "hiphi-installation-authorization".into(),
        audience: "hiphi-relay".into(),
        installation_id: identity.installation_id().to_owned(),
        endpoint: "wss://cloud.example/v1/relay/connect".into(),
        public_key_sha256: hex::encode(Sha256::digest(identity.verifying_key().as_bytes())),
        grant_jti: uuid(102),
        issued_at: now - 1_000,
        expires_at: now + 60_000,
        grant_generation: 7,
    };
    let overlapping = IssuerVerifyingKeyRing::from_entries([
        ("session-old".to_owned(), old.verifying_key()),
        ("session-new".to_owned(), new.verifying_key()),
    ])
    .unwrap();
    for (kid, key) in [("session-old", &old), ("session-new", &new)] {
        assert!(verify_installation_session_grant(
            &signed_session_grant_with_kid(kid, key, &claims),
            &overlapping,
            &identity,
            "wss://cloud.example/v1/relay/connect",
            now,
        )
        .is_ok());
    }
    let retired =
        IssuerVerifyingKeyRing::from_entries([("session-new".to_owned(), new.verifying_key())])
            .unwrap();
    assert_eq!(
        verify_installation_session_grant(
            &signed_session_grant_with_kid("session-old", &old, &claims),
            &retired,
            &identity,
            "wss://cloud.example/v1/relay/connect",
            now,
        ),
        Err(SessionGrantError::UnknownKey)
    );
}

#[test]
fn session_grant_is_verified_before_it_can_authenticate_the_websocket_upgrade() {
    use sha2::{Digest as _, Sha256};

    let issuer = SigningKey::generate(&mut OsRng);
    let installation_id = "11111111-1111-4111-8111-111111111111".to_owned();
    let identity = InstallationIdentity::generate(installation_id.clone()).unwrap();
    let now = 1_800_000_000_000_i64;
    let claims = cloud_connector::protocol::InstallationSessionGrantClaims {
        protocol_version: 1,
        connector_version: env!("UHC_VERSION").into(),
        issuer: "hiphi-installation-authorization".into(),
        audience: "hiphi-relay".into(),
        installation_id,
        endpoint: "wss://cloud.example/v1/relay/connect".into(),
        public_key_sha256: hex::encode(Sha256::digest(identity.verifying_key().as_bytes())),
        grant_jti: uuid(101),
        issued_at: now - 1_000,
        expires_at: now + 60_000,
        grant_generation: 7,
    };
    let grant = signed_session_grant(&issuer, &claims);
    let issuer_keys =
        IssuerVerifyingKeyRing::from_entries([("issuer-1".to_owned(), issuer.verifying_key())])
            .unwrap();
    let verified = verify_installation_session_grant(
        &grant,
        &issuer_keys,
        &identity,
        "wss://cloud.example/v1/relay/connect",
        now,
    )
    .unwrap();
    assert_eq!(verified.grant_generation, 7);

    let mut wrong_version_claims = claims.clone();
    wrong_version_claims.connector_version = "0.9.9".into();
    let wrong_version_grant = signed_session_grant(&issuer, &wrong_version_claims);
    assert_eq!(
        verify_installation_session_grant(
            &wrong_version_grant,
            &issuer_keys,
            &identity,
            "wss://cloud.example/v1/relay/connect",
            now,
        ),
        Err(SessionGrantError::WrongBinding)
    );

    let mut forged_claims = claims;
    forged_claims.grant_generation = 8;
    let forged_payload = signed_session_grant(&SigningKey::generate(&mut OsRng), &forged_claims);
    assert_eq!(
        verify_installation_session_grant(
            &forged_payload,
            &issuer_keys,
            &identity,
            "wss://cloud.example/v1/relay/connect",
            now,
        ),
        Err(SessionGrantError::InvalidSignature)
    );
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
fn relay_parser_rejects_unknown_security_fields_and_duplicate_or_nonfinite_values() {
    let base = json!({
        "type": "command",
        "body": {
            "protocol_version": 1,
            "installation_id": "11111111-1111-4111-8111-111111111111",
            "control_node_id": "66666666-6666-4666-8666-666666666666",
            "epoch": 7,
            "message_id": "message_demo_command",
            "request_id": "22222222-2222-4222-8222-222222222222",
            "idempotency_key": "33333333-3333-4333-8333-333333333333",
            "created_at": 1788060000000_i64,
            "expires_at": 1788060015000_i64,
            "payload": {"zone_handle": "zone_demo_lounge", "action": "transport.next"},
            "grant": "fake-v1-grant-not-a-credential-000000000000000000000000"
        }
    });
    let mut arbitrary_http = base.clone();
    arbitrary_http["body"]["payload"]["url"] = json!("http://127.0.0.1/admin");
    assert_eq!(
        parse_relay_message(&serde_json::to_vec(&arbitrary_http).unwrap()),
        Err("invalid_relay_message")
    );

    let mut missing_grant = base.clone();
    missing_grant["body"]
        .as_object_mut()
        .unwrap()
        .remove("grant");
    assert_eq!(
        parse_relay_message(&serde_json::to_vec(&missing_grant).unwrap()),
        Err("invalid_relay_message")
    );

    let nonfinite = br#"{"type":"command","body":{"payload":{"zone_handle":"zone_demo_lounge","action":"volume.absolute","value":1e999}}}"#;
    assert_eq!(parse_relay_message(nonfinite), Err("invalid_relay_message"));
    let duplicate = br#"{"type":"command","body":{"payload":{"zone_handle":"zone_demo_lounge","action":"transport.next","action":"transport.previous"}}}"#;
    assert_eq!(parse_relay_message(duplicate), Err("duplicate_object_key"));
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
fn artwork_lane_rejects_work_beyond_pending_bound() {
    let mut lane = ArtLane::default();
    for index in 0..cloud_connector::artwork::MAX_PENDING {
        lane.enqueue(ArtRequest {
            request_id: format!("request_{index:016}"),
            zone_handle: id("zone"),
            art_capability: format!("capability_{index:032}"),
        })
        .unwrap();
    }
    assert_eq!(
        lane.enqueue(ArtRequest {
            request_id: id("request_overflow"),
            zone_handle: id("zone"),
            art_capability: id("capability_overflow_012345678901234567890"),
        }),
        Err(cloud_connector::artwork::ArtError::Busy)
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
    let projection = store
        .snapshot(SemanticStateInput {
            installation_id: "11111111-1111-4111-8111-111111111111".into(),
            epoch: 1,
            revision: 1,
            observed_at: 10,
            expires_at: 20,
            zones: vec![cloud_connector::state::SemanticZoneInput {
                provider_id: "roon:secret-provider-id".into(),
                name: "Lounge".into(),
                state: "playing".into(),
                control: cloud_connector::state::ControlEligibility::all(),
                volume: None,
                now_playing: None,
            }],
        })
        .unwrap();
    let encoded = serde_json::to_string(&projection.zones).unwrap();
    assert!(!encoded.contains("roon:secret-provider-id"));
    assert!(store
        .provider_id(&projection.zones[0].zone_handle)
        .is_some());
    assert!(store.is_fresh(1, 15));
    assert!(!store.is_fresh(2, 15));
    assert!(!store.is_fresh(1, 20));

    let removed_handle = projection.zones[0].zone_handle.clone();
    store
        .snapshot(SemanticStateInput {
            installation_id: "11111111-1111-4111-8111-111111111111".into(),
            epoch: 1,
            revision: 2,
            observed_at: 21,
            expires_at: 30,
            zones: vec![],
        })
        .unwrap();
    assert!(store.provider_id(&removed_handle).is_none());
}

#[test]
fn runtime_cloud_projection_uses_the_canonical_visible_zone_policy() {
    let state_source = include_str!("../src/cloud_connector/state.rs");

    assert!(
        state_source.contains("crate::zone_list::visible_zones(state).await"),
        "cloud projection must apply the same hidden-zone, adapter, rename, and ordering policy as every other zone list"
    );
    assert!(
        !state_source.contains("aggregator.get_zones().await"),
        "reading the aggregator directly leaks hidden zones and restores nondeterministic ordering"
    );
}

#[test]
fn spotify_projection_keeps_identity_credentials_urls_and_raw_artwork_local() {
    use cloud_connector::state::{
        ControlEligibility, NowPlayingInput, SemanticZoneInput, VolumeInput,
    };

    let provider_id = "spotify:device_7c4d3b2a";
    let account_token = "Bearer spotify-owner-token-must-stay-local";
    let raw_artwork_key = "https://i.scdn.co/image/provider-image-key-7c4d3b2a";
    let mut store = StateStore::default();
    let projection = store
        .snapshot(SemanticStateInput {
            installation_id: "11111111-1111-4111-8111-111111111111".into(),
            epoch: 7,
            revision: 1,
            observed_at: 10,
            expires_at: 20,
            zones: vec![SemanticZoneInput {
                provider_id: provider_id.into(),
                name: "Kitchen".into(),
                state: "playing".into(),
                control: ControlEligibility::all(),
                volume: Some(VolumeInput {
                    value: 42.0,
                    min: 0.0,
                    max: 100.0,
                    step: 1.0,
                    scale: "percent".into(),
                }),
                now_playing: Some(NowPlayingInput {
                    title: "Public song title".into(),
                    artist: "Public artist".into(),
                    art_revision: Some("art_0123456789abcdef".into()),
                    image_key: Some(raw_artwork_key.into()),
                }),
            }],
        })
        .unwrap();

    let encoded = serde_json::to_string(&projection).unwrap();
    for forbidden in [
        provider_id,
        "device_7c4d3b2a",
        account_token,
        raw_artwork_key,
        "i.scdn.co",
        "spotify-owner-token",
    ] {
        assert!(
            !encoded.contains(forbidden),
            "cloud projection leaked provider-native value `{forbidden}`"
        );
    }
    assert_eq!(
        store.provider_id(&projection.zones[0].zone_handle),
        Some(provider_id),
        "the reverse mapping remains connector-local"
    );
    assert_eq!(
        store.artwork_key(
            &projection.zones[0].zone_handle,
            projection.now_playing[0].image_revision.as_deref().unwrap(),
        ),
        Some(raw_artwork_key),
        "the raw image key remains connector-local for bounded artwork fetches"
    );
}

#[test]
fn stale_state_is_not_command_eligible() {
    let mut store = StateStore::default();
    let projection = store
        .snapshot(SemanticStateInput {
            installation_id: "11111111-1111-4111-8111-111111111111".into(),
            epoch: 9,
            revision: 4,
            observed_at: 100,
            expires_at: 200,
            zones: vec![],
        })
        .unwrap();
    assert!(store.is_fresh(projection.epoch, 199));
    assert!(!store.is_fresh(projection.epoch, 200));
    assert!(!store.is_fresh(projection.epoch + 1, 150));
}

#[test]
fn outbound_projection_rejects_oversized_duplicate_and_nonfinite_state() {
    use cloud_connector::state::{
        ControlEligibility, NowPlayingInput, SemanticZoneInput, VolumeInput,
    };

    let zone = |provider_id: &str| SemanticZoneInput {
        provider_id: provider_id.into(),
        name: "Lounge".into(),
        state: "playing".into(),
        control: ControlEligibility::all(),
        volume: None,
        now_playing: None,
    };
    let input = |zones| SemanticStateInput {
        installation_id: "11111111-1111-4111-8111-111111111111".into(),
        epoch: 1,
        revision: 1,
        observed_at: 10,
        expires_at: 20,
        zones,
    };

    let mut store = StateStore::default();
    assert!(store
        .snapshot(input(vec![zone("roon:one"), zone("roon:one")]))
        .is_err());

    let oversized = (0..129)
        .map(|index| zone(&format!("roon:{index}")))
        .collect();
    assert!(store.snapshot(input(oversized)).is_err());

    let mut bad_volume = zone("roon:volume");
    bad_volume.volume = Some(VolumeInput {
        value: f64::NAN,
        min: 0.0,
        max: 100.0,
        step: 1.0,
        scale: "percent".into(),
    });
    assert!(store.snapshot(input(vec![bad_volume])).is_err());

    let mut bad_art = zone("roon:art");
    bad_art.now_playing = Some(NowPlayingInput {
        title: "Track".into(),
        artist: "Artist".into(),
        art_revision: Some("not valid because it has spaces".into()),
        image_key: Some("provider-secret".into()),
    });
    assert!(store.snapshot(input(vec![bad_art])).is_err());
}

#[test]
fn absolute_volume_is_bounded_by_the_last_truthful_zone_projection() {
    use cloud_connector::state::{ControlEligibility, SemanticZoneInput, VolumeInput};

    let mut store = StateStore::default();
    let projection = store
        .snapshot(SemanticStateInput {
            installation_id: "11111111-1111-4111-8111-111111111111".into(),
            epoch: 1,
            revision: 1,
            observed_at: 10,
            expires_at: 20,
            zones: vec![SemanticZoneInput {
                provider_id: "roon:bounded".into(),
                name: "Bounded".into(),
                state: "playing".into(),
                control: ControlEligibility::all(),
                volume: Some(VolumeInput {
                    value: -23.5,
                    min: -80.0,
                    max: 0.0,
                    step: 0.5,
                    scale: "db".into(),
                }),
                now_playing: None,
            }],
        })
        .unwrap();
    let handle = &projection.zones[0].zone_handle;
    assert!(store.accepts_absolute_volume(handle, -23.5));
    assert!(store.accepts_absolute_volume(handle, -80.0));
    assert!(store.accepts_absolute_volume(handle, 0.0));
    assert!(!store.accepts_absolute_volume(handle, -80.1));
    assert!(!store.accepts_absolute_volume(handle, 0.1));
    assert!(!store.accepts_absolute_volume("zone_unknown_opaque", -23.5));
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

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path.path(), std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            InstallationIdentity::load(path.path(), identity.installation_id().to_owned()),
            Err(cloud_connector::identity::IdentityError::InsecurePermissions)
        ));
    }
}

#[test]
fn session_epoch_high_water_mark_survives_reconnect_and_process_restart() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("epoch");
    let mut guard = SessionEpochGuard::load(&path).unwrap();
    guard
        .accept_at(1_800_000_000_001, 1_800_000_000_001)
        .unwrap();
    assert_eq!(guard.last(), 1_800_000_000_001);
    assert!(matches!(
        guard.accept_at(1_800_000_000_001, 1_800_000_000_001),
        Err(EpochGuardError::StaleEpoch)
    ));

    let mut restarted = SessionEpochGuard::load(&path).unwrap();
    assert_eq!(restarted.last(), 1_800_000_000_001);
    assert!(matches!(
        restarted.accept_at(1_800_000_000_000, 1_800_000_000_002),
        Err(EpochGuardError::StaleEpoch)
    ));
    assert!(matches!(
        restarted.accept_at(u64::MAX, 1_800_000_000_002),
        Err(EpochGuardError::StaleEpoch)
    ));
    restarted
        .accept_at(1_800_000_000_002, 1_800_000_000_002)
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

fn signed_grant_with_kid(key_id: &str, identity: &SigningKey, claims: &GrantClaims) -> String {
    use base64::Engine as _;
    let header = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(format!(r#"{{"alg":"EdDSA","kid":"{key_id}"}}"#));
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(claims).unwrap());
    let sig = identity.sign(format!("{header}.{payload}").as_bytes());
    format!(
        "{header}.{payload}.{}",
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sig.to_bytes())
    )
}

#[test]
fn command_issuer_rotation_overlaps_then_retires_the_old_key() {
    let old = SigningKey::generate(&mut OsRng);
    let new = SigningKey::generate(&mut OsRng);
    let overlapping = || {
        let mut verifier = CommandGrantVerifier::new(
            "hiphi-command-authorization",
            "uhc-connector",
            id("install"),
            7,
            3,
        );
        verifier.pin_key("command-old", old.verifying_key());
        verifier.pin_key("command-new", new.verifying_key());
        verifier
    };
    let old_command = command_with_claims_and_kid(1_000, "command-old", &old, 3, |_| {});
    let new_command = command_with_claims_and_kid(1_000, "command-new", &new, 3, |_| {});
    assert!(overlapping().verify(&old_command, 1_000).is_ok());
    assert!(overlapping().verify(&new_command, 1_000).is_ok());

    let mut retired = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        7,
        3,
    );
    retired.pin_key("command-new", new.verifying_key());
    assert_eq!(
        retired.verify(&old_command, 1_000),
        Err(GrantError::UnknownKey)
    );
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
    command_with_claims_and_kid(now, "issuer-1", key, generation, mutate)
}

fn command_with_claims_and_kid(
    now: u64,
    key_id: &str,
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
        control_node_id: uuid(3),
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
        control_node_id: uuid(3),
        epoch: 7,
        message_id: id("message"),
        request_id: claims.request_id,
        idempotency_key: claims.idempotency_key,
        created_at: claims.issued_at,
        expires_at: claims.expires_at,
        payload,
        grant: signed_grant_with_kid(key_id, key, &claims),
    }
}

#[test]
fn grant_binds_key_audience_expiry_generation_and_replay() {
    let key = SigningKey::generate(&mut OsRng);
    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
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
        claims.control_node_id = uuid(4);
    });
    let mut node_verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
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
fn command_ledger_expires_terminal_results_before_reuse() {
    let mut ledger = cloud_connector::commands::CommandLedger::default();
    ledger.record_at("request", CommandOutcome::Executed, 1_000);
    assert_eq!(ledger.get_at("request", 61_001), None);
    assert!(ledger.is_empty());
}

#[test]
fn command_ledger_refuses_new_entry_at_capacity_without_evicting_live_results() {
    let mut ledger = cloud_connector::commands::CommandLedger::default();
    for index in 0..257 {
        ledger.record_at(format!("request_{index}"), CommandOutcome::Executed, index);
    }
    assert_eq!(ledger.len(), 256);
    assert_eq!(
        ledger.get_at("request_0", 257),
        Some(CommandOutcome::Executed)
    );
    assert_eq!(ledger.get_at("request_256", 257), None);
}

#[test]
fn valid_new_request_retry_uses_the_signed_idempotency_result_without_reexecution() {
    let key = SigningKey::generate(&mut OsRng);
    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        7,
        3,
    );
    verifier.pin_key("issuer-1", key.verifying_key());
    let first = command_and_grant(1_000, &key, 3);
    let first_verified = verifier.verify(&first, 1_000).unwrap();
    let hash = first_verified.payload.canonical_hash().unwrap();
    let mut ledger = cloud_connector::commands::CommandLedger::default();
    ledger
        .record_command_at(
            first.idempotency_key.to_string(),
            &hash,
            CommandOutcome::Executed,
            1_000,
        )
        .unwrap();

    let retry = command_with_claims(1_001, &key, 3, |claims| {
        claims.request_id = uuid(88);
    });
    let retry_verified = verifier.verify(&retry, 1_001).unwrap();
    assert_eq!(retry.idempotency_key, first.idempotency_key);
    assert_eq!(retry_verified.payload.canonical_hash().unwrap(), hash);
    assert_eq!(
        ledger
            .lookup_command_at(&retry.idempotency_key.to_string(), &hash, 1_001)
            .unwrap(),
        Some(CommandOutcome::Executed)
    );
}

#[test]
fn invalid_grant_cannot_poison_the_idempotency_ledger() {
    let key = SigningKey::generate(&mut OsRng);
    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        7,
        3,
    );
    verifier.pin_key("issuer-1", key.verifying_key());
    let invalid = command_with_claims(1_000, &key, 3, |claims| {
        claims.audience = "attacker".into();
    });
    let ledger = cloud_connector::commands::CommandLedger::default();
    assert_eq!(
        verifier.verify(&invalid, 1_000),
        Err(GrantError::WrongAudience)
    );
    assert!(ledger.is_empty());
}

#[test]
fn command_ledger_binds_idempotency_to_payload_hash() {
    let mut ledger = cloud_connector::commands::CommandLedger::default();
    ledger
        .record_command_at("idempotency", "hash_a", CommandOutcome::Executed, 1_000)
        .unwrap();
    assert_eq!(
        ledger
            .lookup_command_at("idempotency", "hash_a", 1_001)
            .unwrap(),
        Some(CommandOutcome::Executed)
    );
    assert_eq!(
        ledger.lookup_command_at("idempotency", "hash_b", 1_001),
        Err(cloud_connector::commands::LedgerError::Conflict)
    );
    assert_eq!(ledger.len(), 1);
}

#[test]
fn command_replay_cache_remains_bounded_under_unique_grants() {
    let key = SigningKey::generate(&mut OsRng);
    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        7,
        3,
    );
    verifier.pin_key("issuer-1", key.verifying_key());
    for request in 0..MAX_REPLAY_REQUESTS {
        let command = command_with_claims(1_000, &key, 3, |claims| {
            claims.request_id = uuid((request + 100) as u128);
        });
        assert!(verifier.verify(&command, 1_000).is_ok());
    }
    assert_eq!(verifier.seen_requests_len(), MAX_REPLAY_REQUESTS);
    let full = command_with_claims(1_000, &key, 3, |claims| {
        claims.request_id = uuid(10_000);
    });
    assert_eq!(verifier.verify(&full, 1_000), Err(GrantError::AtCapacity));
}

#[test]
fn command_verifier_binds_the_signed_node_to_each_envelope() {
    let key = SigningKey::generate(&mut OsRng);
    let mut verifier = CommandGrantVerifier::new(
        "hiphi-command-authorization",
        "uhc-connector",
        id("install"),
        7,
        3,
    );
    verifier.pin_key("issuer-1", key.verifying_key());
    let wrong_node = command_with_claims(1_000, &key, 3, |claims| {
        claims.control_node_id = uuid(4);
    });
    assert_eq!(
        verifier.verify(&wrong_node, 1_000),
        Err(GrantError::BindingMismatch)
    );

    let mut node_a = command_with_claims(1_000, &key, 3, |claims| {
        claims.request_id = uuid(10);
        claims.control_node_id = uuid(10);
    });
    node_a.control_node_id = uuid(10);
    assert!(verifier.verify(&node_a, 1_000).is_ok());

    let mut node_b = command_with_claims(1_000, &key, 3, |claims| {
        claims.request_id = uuid(11);
        claims.control_node_id = uuid(11);
    });
    node_b.control_node_id = uuid(11);
    assert!(verifier.verify(&node_b, 1_000).is_ok());
}

#[test]
fn dropped_result_is_idempotent_and_art_lane_has_no_url_surface() {
    let mut ledger = cloud_connector::commands::CommandLedger::default();
    ledger.record(id("request"), CommandOutcome::Executed);
    assert_eq!(
        ledger.get_at(&id("request"), 1_000),
        Some(CommandOutcome::Executed)
    );
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
fn revocation_is_terminal_for_the_run_loop() {
    let mut loop_state = ConnectorRunLoop::default();
    loop_state.connect();
    loop_state.revoke();
    loop_state.connect();
    assert_eq!(loop_state.state(), ConnectionState::Revoked);
    assert!(loop_state.drain_events().is_empty());
}

#[test]
fn backoff_progresses_until_authenticated_session_then_resets() {
    let mut backoff = cloud_connector::transport::Backoff::default();
    assert_eq!(backoff.next_delay(), Duration::from_millis(250));
    assert_eq!(backoff.next_delay(), Duration::from_millis(500));
    assert_eq!(backoff.next_delay(), Duration::from_millis(1_000));
    // The runtime calls reset only after challenge/proof/session establishment.
    backoff.reset();
    assert_eq!(backoff.next_delay(), Duration::from_millis(250));
}

#[test]
fn peer_watchdog_expires_only_after_bounded_silence() {
    let mut watchdog = cloud_connector::transport::PeerWatchdog::new(
        10_000,
        cloud_connector::transport::PEER_HEARTBEAT_TIMEOUT,
    );
    assert!(!watchdog.expired(10_000 + 89_999));
    watchdog.observe(100_000);
    assert!(!watchdog.expired(100_000 + 89_999));
    assert!(watchdog.expired(100_000 + 90_000));
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
    assert_eq!(request.connector_version, env!("UHC_VERSION"));
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

#[test]
fn enrollment_possession_proof_binds_one_use_capability_audience_and_local_key() {
    use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};

    let fixture: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/hiphi-enrollment-v1.json")).unwrap();
    let valid = &fixture["valid"];
    let public_key: [u8; 32] = URL_SAFE_NO_PAD
        .decode(valid["installation_public_key"].as_str().unwrap())
        .unwrap()
        .try_into()
        .unwrap();
    let key = ed25519_dalek::VerifyingKey::from_bytes(&public_key).unwrap();
    let signature = ed25519_dalek::Signature::from_slice(
        &URL_SAFE_NO_PAD
            .decode(valid["possession_signature"].as_str().unwrap())
            .unwrap(),
    )
    .unwrap();
    let message = cloud_connector::pairing::enrollment_possession_message(
        valid["enrollment_capability"].as_str().unwrap(),
        valid["installation_audience"].as_str().unwrap(),
        valid["public_key_fingerprint"].as_str().unwrap(),
    );
    assert_eq!(
        URL_SAFE_NO_PAD.encode(&message),
        valid["possession_message"].as_str().unwrap()
    );
    assert!(key.verify(&message, &signature).is_ok());

    for adversarial in fixture["adversarial"].as_array().unwrap() {
        let altered = cloud_connector::pairing::enrollment_possession_message(
            adversarial["enrollment_capability"].as_str().unwrap(),
            adversarial["installation_audience"].as_str().unwrap(),
            adversarial["public_key_fingerprint"].as_str().unwrap(),
        );
        assert!(
            key.verify(&altered, &signature).is_err(),
            "accepted {}",
            adversarial["name"].as_str().unwrap()
        );
    }
}
