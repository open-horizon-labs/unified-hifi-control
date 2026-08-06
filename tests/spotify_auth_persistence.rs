use tempfile::tempdir;
use unified_hifi_control::adapters::spotify::SpotifyToken;
use unified_hifi_control::api::credentials::EncryptedCredentialStore;

#[test]
fn encrypted_credentials_survive_restart_without_plaintext_leakage() {
    let directory = tempdir().expect("temporary credential directory");
    let credential_path = directory.path().join("spotify.enc");
    let key = [7_u8; 32];
    let token = SpotifyToken {
        access_token: "access-secret-value".to_string(),
        refresh_token: Some("refresh-secret-value".to_string()),
        expires_at: Some(1234),
    };
    let debug = format!("{token:?}");
    assert!(!debug.contains("access-secret-value"));
    assert!(!debug.contains("refresh-secret-value"));

    let store = EncryptedCredentialStore::new(credential_path.clone(), key);
    store.save(&token).expect("save credentials");
    let ciphertext = std::fs::read_to_string(&credential_path).expect("read ciphertext");
    assert!(!ciphertext.contains("access-secret-value"));
    assert!(!ciphertext.contains("refresh-secret-value"));

    let restarted = EncryptedCredentialStore::new(credential_path, key);
    assert_eq!(restarted.load().expect("load credentials"), Some(token));
}

#[test]
fn revoke_clears_durable_credentials() {
    let directory = tempdir().expect("temporary credential directory");
    let credential_path = directory.path().join("spotify.enc");
    let store = EncryptedCredentialStore::new(credential_path.clone(), [9_u8; 32]);
    store
        .save(&SpotifyToken {
            access_token: "access".to_string(),
            refresh_token: Some("refresh".to_string()),
            expires_at: Some(1234),
        })
        .expect("save credentials");

    store.clear().expect("clear credentials");
    assert_eq!(store.load().expect("load after revoke"), None);
    assert!(!credential_path.exists());
}

#[test]
fn wrong_key_and_tampered_ciphertext_fail_closed() {
    let directory = tempdir().expect("temporary credential directory");
    let credential_path = directory.path().join("spotify.enc");
    let store = EncryptedCredentialStore::new(credential_path.clone(), [1_u8; 32]);
    store
        .save(&SpotifyToken {
            access_token: "access-secret".to_string(),
            refresh_token: Some("refresh-secret".to_string()),
            expires_at: Some(1234),
        })
        .expect("save credentials");

    let wrong_key = EncryptedCredentialStore::new(credential_path.clone(), [2_u8; 32]);
    assert!(wrong_key.load().is_err());
    let mut bytes = std::fs::read(&credential_path).expect("read envelope");
    let last = bytes.len() - 2;
    bytes[last] ^= 0x40;
    std::fs::write(&credential_path, bytes).expect("tamper envelope");
    assert!(store.load().is_err());
}

#[cfg(unix)]
#[test]
fn credential_file_is_owner_only() {
    use std::os::unix::fs::PermissionsExt;

    let directory = tempdir().expect("temporary credential directory");
    let credential_path = directory.path().join("spotify.enc");
    let store = EncryptedCredentialStore::new(credential_path.clone(), [5_u8; 32]);
    store
        .save(&SpotifyToken {
            access_token: "access".to_string(),
            refresh_token: None,
            expires_at: None,
        })
        .expect("save credentials");
    assert_eq!(
        std::fs::metadata(credential_path)
            .expect("credential metadata")
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}
