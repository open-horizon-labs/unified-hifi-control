//! Encrypted provider credential storage for server deployments.
//!
//! The store is intentionally small and synchronous: credentials are tiny and
//! writes happen only on OAuth/revoke, never in the playback polling loop.
//! Runtime code should construct it with [`EncryptedCredentialStore::from_env`].
//!
//! Key strategy:
//! - `UHC_CREDENTIAL_KEY` may contain a 32-byte base64url/hex key supplied by a
//!   secret manager or container secret on QNAP.
//! - Otherwise `UHC_CREDENTIAL_KEY_FILE` (or `credential.key` beside UHC's
//!   config) is created with random bytes and mode 0600 on Unix.
//!
//! The encrypted file contains a versioned ChaCha20-Poly1305 envelope. The key
//! is deliberately not stored in that envelope; operators can rotate it by
//! replacing the external key and deleting/re-authorizing credentials.

use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chacha20poly1305::{aead::Aead, ChaCha20Poly1305, Key, KeyInit, Nonce};
use rand::{rngs::OsRng, RngCore};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::adapters::spotify::SpotifyToken;
use crate::config::get_config_file_path;

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const CREDENTIAL_FILE_ENV: &str = "UHC_SPOTIFY_CREDENTIAL_FILE";
const KEY_FILE_ENV: &str = "UHC_CREDENTIAL_KEY_FILE";
const KEY_ENV: &str = "UHC_CREDENTIAL_KEY";
const APPLE_BRIDGE_FILE_ENV: &str = "UHC_APPLE_BRIDGE_CREDENTIAL_FILE";
const MUSIC_ASSISTANT_CREDENTIAL_FILE_ENV: &str = "UHC_MUSIC_ASSISTANT_CREDENTIAL_FILE";

#[derive(Debug, Serialize, Deserialize)]
struct EncryptedEnvelope {
    version: u8,
    cipher: String,
    nonce: String,
    ciphertext: String,
}

/// Durable encrypted Spotify credential file.
#[derive(Clone)]
pub struct EncryptedCredentialStore {
    credential_path: PathBuf,
    key: [u8; KEY_BYTES],
}

/// Spotify provider configuration and token kept in one encrypted record.
/// `client_secret` is optional for a public PKCE client.
#[derive(Clone, Serialize, Deserialize)]
pub struct SpotifyCredentialRecord {
    #[serde(default)]
    pub token: Option<SpotifyToken>,
    pub client_id: String,
    #[serde(default)]
    pub client_secret: Option<String>,
    pub redirect_uri: String,
}

impl EncryptedCredentialStore {
    /// Construct a store with an explicit key. Primarily useful for tests and
    /// for callers that already integrate with a secret manager.
    pub fn new(credential_path: PathBuf, key: [u8; KEY_BYTES]) -> Self {
        Self {
            credential_path,
            key,
        }
    }

    /// Build the production store from environment/config paths.
    pub fn from_env() -> Result<Self> {
        let credential_path = std::env::var_os(CREDENTIAL_FILE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| get_config_file_path("spotify-credentials.enc"));
        let key = if let Ok(value) = std::env::var(KEY_ENV) {
            parse_key(&value).context("UHC_CREDENTIAL_KEY must be 32-byte hex or base64url")?
        } else {
            let key_path = std::env::var_os(KEY_FILE_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| get_config_file_path("credential.key"));
            load_or_create_key(&key_path)?
        };
        Ok(Self::new(credential_path, key))
    }

    /// Load and decrypt credentials. Missing files mean “not connected”.
    pub fn load(&self) -> Result<Option<SpotifyToken>> {
        Ok(self.load_record()?.and_then(|record| record.token))
    }

    /// Load the complete encrypted Spotify record.
    pub fn load_record(&self) -> Result<Option<SpotifyCredentialRecord>> {
        let bytes = match std::fs::read(&self.credential_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let envelope: EncryptedEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("Spotify credential envelope is invalid: {error}"))?;
        if envelope.version != 1 || envelope.cipher != "chacha20poly1305" {
            return Err(anyhow!("unsupported Spotify credential envelope"));
        }
        let nonce_bytes = URL_SAFE_NO_PAD
            .decode(envelope.nonce)
            .context("Spotify credential nonce is invalid")?;
        if nonce_bytes.len() != NONCE_BYTES {
            return Err(anyhow!("Spotify credential nonce has an invalid length"));
        }
        let ciphertext = URL_SAFE_NO_PAD
            .decode(envelope.ciphertext)
            .context("Spotify credential ciphertext is invalid")?;
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.key));
        let plaintext = cipher
            .decrypt(Nonce::from_slice(&nonce_bytes), ciphertext.as_ref())
            .map_err(|_| anyhow!("Spotify credential decryption failed"))?;
        serde_json::from_slice(&plaintext)
            .map_err(|error| anyhow!("Spotify credential payload is invalid: {error}"))
    }

    /// Encrypt and atomically replace credentials with owner-only permissions.
    pub fn save(&self, token: &SpotifyToken) -> Result<()> {
        let record = self
            .load_record()?
            .unwrap_or_else(|| SpotifyCredentialRecord {
                token: None,
                client_id: String::new(),
                client_secret: None,
                redirect_uri: String::new(),
            });
        self.save_record(&SpotifyCredentialRecord {
            token: Some(token.clone()),
            ..record
        })
    }

    /// Encrypt and atomically replace the complete credential record.
    pub fn save_record(&self, record: &SpotifyCredentialRecord) -> Result<()> {
        let parent = self
            .credential_path
            .parent()
            .ok_or_else(|| anyhow!("Spotify credential path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let plaintext = serde_json::to_vec(record).context("serialize Spotify credentials")?;
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce_bytes);
        let cipher = ChaCha20Poly1305::new(Key::from_slice(&self.key));
        let ciphertext = cipher
            .encrypt(Nonce::from_slice(&nonce_bytes), plaintext.as_ref())
            .map_err(|_| anyhow!("encrypt Spotify credentials"))?;
        let envelope = EncryptedEnvelope {
            version: 1,
            cipher: "chacha20poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce_bytes),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        let encoded = serde_json::to_vec(&envelope).context("serialize credential envelope")?;
        let temporary = self.credential_path.with_extension("enc.tmp");
        write_private_file(&temporary, &encoded)?;
        std::fs::rename(&temporary, &self.credential_path)
            .context("replace Spotify credential file")?;
        Ok(())
    }

    /// Clear only the persisted Spotify token while retaining the provider
    /// configuration needed to authorize again.
    ///
    /// A rejected refresh token means the user must complete OAuth again, but
    /// it does not invalidate the client id, client secret, or redirect URI
    /// configured for that OAuth client.  Keep the encrypted record in place
    /// so the next authorization attempt can reuse that configuration.
    pub fn clear_token(&self) -> Result<()> {
        let Some(mut record) = self.load_record()? else {
            return Ok(());
        };
        record.token = None;
        self.save_record(&record)
    }

    /// Delete durable credentials. Missing files are already revoked.
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.credential_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Path used for diagnostics; never includes credential contents.
    pub fn path(&self) -> &Path {
        &self.credential_path
    }
}

/// Connection identity and bearer token for one Music Assistant server.
///
/// The token is intentionally only available to server-side callers.  The
/// Settings/UI contract reports configuration presence, never this record.
#[derive(Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MusicAssistantCredentialRecord {
    pub host: String,
    pub port: u16,
    pub token: String,
    pub tls: bool,
    pub allow_insecure_http: bool,
}

impl std::fmt::Debug for MusicAssistantCredentialRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MusicAssistantCredentialRecord")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("token", &"[REDACTED]")
            .field("tls", &self.tls)
            .field("allow_insecure_http", &self.allow_insecure_http)
            .finish()
    }
}

/// Encrypted credential storage for Music Assistant's outbound bearer token.
#[derive(Clone)]
pub struct MusicAssistantCredentialStore {
    credential_path: PathBuf,
    key: [u8; KEY_BYTES],
}

impl MusicAssistantCredentialStore {
    pub fn new(credential_path: PathBuf, key: [u8; KEY_BYTES]) -> Self {
        Self {
            credential_path,
            key,
        }
    }

    pub fn from_env() -> Result<Self> {
        let credential_path = std::env::var_os(MUSIC_ASSISTANT_CREDENTIAL_FILE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| get_config_file_path("musicassistant-credentials.enc"));
        let key = if let Ok(value) = std::env::var(KEY_ENV) {
            parse_key(&value).context("UHC_CREDENTIAL_KEY must be 32-byte hex or base64url")?
        } else {
            let key_path = std::env::var_os(KEY_FILE_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| get_config_file_path("credential.key"));
            load_or_create_key(&key_path)?
        };
        Ok(Self::new(credential_path, key))
    }

    pub fn load(&self) -> Result<Option<MusicAssistantCredentialRecord>> {
        let bytes = match std::fs::read(&self.credential_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let envelope: EncryptedEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("Music Assistant credential envelope is invalid: {error}"))?;
        if envelope.version != 1 || envelope.cipher != "chacha20poly1305" {
            return Err(anyhow!("unsupported Music Assistant credential envelope"));
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(envelope.nonce)
            .context("Music Assistant credential nonce is invalid")?;
        if nonce.len() != NONCE_BYTES {
            return Err(anyhow!(
                "Music Assistant credential nonce has an invalid length"
            ));
        }
        let ciphertext = URL_SAFE_NO_PAD
            .decode(envelope.ciphertext)
            .context("Music Assistant credential ciphertext is invalid")?;
        let plaintext = ChaCha20Poly1305::new(Key::from_slice(&self.key))
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| anyhow!("Music Assistant credential decryption failed"))?;
        serde_json::from_slice(&plaintext)
            .map_err(|error| anyhow!("Music Assistant credential payload is invalid: {error}"))
    }

    pub fn save(&self, record: &MusicAssistantCredentialRecord) -> Result<()> {
        let parent = self
            .credential_path
            .parent()
            .ok_or_else(|| anyhow!("Music Assistant credential path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let plaintext =
            serde_json::to_vec(record).context("serialize Music Assistant credentials")?;
        let mut nonce = [0_u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = ChaCha20Poly1305::new(Key::from_slice(&self.key))
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
            .map_err(|_| anyhow!("encrypt Music Assistant credentials"))?;
        let encoded = serde_json::to_vec(&EncryptedEnvelope {
            version: 1,
            cipher: "chacha20poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        })
        .context("serialize Music Assistant credential envelope")?;
        let temporary = self.credential_path.with_extension("enc.tmp");
        write_private_file(&temporary, &encoded)?;
        std::fs::rename(&temporary, &self.credential_path)
            .context("replace Music Assistant credential file")?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.credential_path
    }

    /// Remove the encrypted record when rolling a failed first-time setup
    /// back to its prior absent state.
    pub fn clear(&self) -> Result<()> {
        match std::fs::remove_file(&self.credential_path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

/// Durable identity and bearer records for native Apple Music companions.
///
/// This intentionally stores no MusicKit authorization or transient bridge
/// queues.  The bearer is encrypted with the same externally-held key as the
/// provider credential store, and the file is replaced atomically.
#[derive(Clone, Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct AppleBridgeCredentialRecord {
    pub bridge_id: String,
    pub access_token: String,
    #[serde(default)]
    pub bound_player_id: Option<String>,
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Clone)]
pub struct AppleBridgeCredentialStore {
    credential_path: PathBuf,
    key: [u8; KEY_BYTES],
}

impl AppleBridgeCredentialStore {
    pub fn new(credential_path: PathBuf, key: [u8; KEY_BYTES]) -> Self {
        Self {
            credential_path,
            key,
        }
    }

    pub fn from_env() -> Result<Self> {
        let credential_path = std::env::var_os(APPLE_BRIDGE_FILE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| get_config_file_path("applemusic-bridges.enc"));
        let key = if let Ok(value) = std::env::var(KEY_ENV) {
            parse_key(&value).context("UHC_CREDENTIAL_KEY must be 32-byte hex or base64url")?
        } else {
            let key_path = std::env::var_os(KEY_FILE_ENV)
                .map(PathBuf::from)
                .unwrap_or_else(|| get_config_file_path("credential.key"));
            load_or_create_key(&key_path)?
        };
        Ok(Self::new(credential_path, key))
    }

    pub fn load(&self) -> Result<Vec<AppleBridgeCredentialRecord>> {
        let bytes = match std::fs::read(&self.credential_path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.into()),
        };
        let envelope: EncryptedEnvelope = serde_json::from_slice(&bytes)
            .map_err(|error| anyhow!("Apple bridge credential envelope is invalid: {error}"))?;
        if envelope.version != 1 || envelope.cipher != "chacha20poly1305" {
            return Err(anyhow!("unsupported Apple bridge credential envelope"));
        }
        let nonce = URL_SAFE_NO_PAD
            .decode(envelope.nonce)
            .context("Apple bridge nonce is invalid")?;
        let ciphertext = URL_SAFE_NO_PAD
            .decode(envelope.ciphertext)
            .context("Apple bridge ciphertext is invalid")?;
        if nonce.len() != NONCE_BYTES {
            return Err(anyhow!("Apple bridge nonce has an invalid length"));
        }
        let plaintext = ChaCha20Poly1305::new(Key::from_slice(&self.key))
            .decrypt(Nonce::from_slice(&nonce), ciphertext.as_ref())
            .map_err(|_| anyhow!("Apple bridge credential decryption failed"))?;
        serde_json::from_slice(&plaintext)
            .map_err(|error| anyhow!("Apple bridge credential payload is invalid: {error}"))
    }

    pub fn save(&self, records: &[AppleBridgeCredentialRecord]) -> Result<()> {
        let parent = self
            .credential_path
            .parent()
            .ok_or_else(|| anyhow!("Apple bridge credential path has no parent"))?;
        std::fs::create_dir_all(parent)?;
        let plaintext =
            serde_json::to_vec(records).context("serialize Apple bridge credentials")?;
        let mut nonce = [0_u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce);
        let ciphertext = ChaCha20Poly1305::new(Key::from_slice(&self.key))
            .encrypt(Nonce::from_slice(&nonce), plaintext.as_ref())
            .map_err(|_| anyhow!("encrypt Apple bridge credentials"))?;
        let envelope = EncryptedEnvelope {
            version: 1,
            cipher: "chacha20poly1305".to_string(),
            nonce: URL_SAFE_NO_PAD.encode(nonce),
            ciphertext: URL_SAFE_NO_PAD.encode(ciphertext),
        };
        let encoded = serde_json::to_vec(&envelope).context("serialize Apple bridge envelope")?;
        let temporary = self.credential_path.with_extension("enc.tmp");
        write_private_file(&temporary, &encoded)?;
        std::fs::rename(temporary, &self.credential_path)
            .context("replace Apple bridge credentials")?;
        Ok(())
    }
}

fn parse_key(value: &str) -> Result<[u8; KEY_BYTES]> {
    let decoded = if value.len() == KEY_BYTES * 2
        && value.chars().all(|character| character.is_ascii_hexdigit())
    {
        hex::decode(value)?
    } else {
        URL_SAFE_NO_PAD.decode(value.trim())?
    };
    decoded
        .try_into()
        .map_err(|_| anyhow!("credential key must contain exactly 32 bytes"))
}

fn load_or_create_key(path: &Path) -> Result<[u8; KEY_BYTES]> {
    match std::fs::read(path) {
        Ok(bytes) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
            }
            bytes
                .try_into()
                .map_err(|_| anyhow!("credential key file must contain exactly 32 bytes"))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .ok_or_else(|| anyhow!("credential key path has no parent"))?;
            std::fs::create_dir_all(parent)?;
            let mut key = [0_u8; KEY_BYTES];
            OsRng.fill_bytes(&mut key);
            write_private_file(path, &key)?;
            Ok(key)
        }
        Err(error) => Err(error.into()),
    }
}

fn write_private_file(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = path.with_extension("tmp");
    let mut options = std::fs::OpenOptions::new();
    options.create(true).truncate(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    }
    std::io::Write::write_all(&mut file, bytes)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(temporary, path)?;
    Ok(())
}
