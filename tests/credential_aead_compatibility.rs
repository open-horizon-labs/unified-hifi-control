//! Fixed, synthetic envelopes generated independently with Python cryptography's
//! ChaCha20Poly1305 (key [7; 32], nonce [9; 12], no AAD). Unlike a same-library
//! round trip, these catch incompatible changes to the on-disk v1 cipher format.
use serde_json::Value;
use std::path::PathBuf;
use unified_hifi_control::api::credentials::{
    AppleBridgeCredentialStore, EncryptedCredentialStore, MqttCredentialStore,
    MusicAssistantCredentialStore,
};

fn load(name: &str, path: PathBuf, key: [u8; 32]) -> anyhow::Result<Value> {
    Ok(match name {
        "spotify" => serde_json::to_value(EncryptedCredentialStore::new(path, key).load_record()?)?,
        "musicassistant" => {
            serde_json::to_value(MusicAssistantCredentialStore::new(path, key).load()?)?
        }
        "mqtt" => serde_json::to_value(MqttCredentialStore::new(path, key).load()?)?,
        "applebridge" => serde_json::to_value(AppleBridgeCredentialStore::new(path, key).load()?)?,
        _ => unreachable!("fixture names are exhaustive"),
    })
}

#[test]
fn all_credential_stores_read_independently_encrypted_v1_records() {
    let fixtures: Value =
        serde_json::from_str(include_str!("fixtures/credential_aead_compatibility.json")).unwrap();
    let directory = tempfile::tempdir().unwrap();
    for (name, fixture) in fixtures.as_object().unwrap() {
        let path = directory.path().join(name);
        std::fs::write(&path, serde_json::to_vec(&fixture["envelope"]).unwrap()).unwrap();
        assert_eq!(
            load(name, path.clone(), [7; 32]).unwrap(),
            fixture["plaintext"],
            "{name}"
        );
        assert!(
            load(name, path.clone(), [8; 32]).is_err(),
            "{name}: wrong key"
        );
        for nonce in ["", "AA", "AAAAAAAAAAAAAAAAAA"] {
            let mut envelope = fixture["envelope"].clone();
            envelope["nonce"] = nonce.into();
            std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
            assert!(
                load(name, path.clone(), [7; 32]).is_err(),
                "{name}: invalid nonce"
            );
        }
    }
}
