use rand::{rngs::OsRng, RngCore};
use std::{collections::HashMap, fs, io, path::Path};

use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

use super::protocol::validate_id;

#[derive(Debug, thiserror::Error)]
pub enum IdentityError {
    #[error("identity file I/O: {0}")]
    Io(#[from] io::Error),
    #[error("identity file must contain exactly 32 bytes")]
    InvalidKey,
    #[error("installation id is not an opaque protocol id")]
    InvalidInstallationId,
    #[error("identity file must be a regular owner-only file")]
    InsecurePermissions,
}

pub struct InstallationIdentity {
    installation_id: String,
    signing_key: SigningKey,
}

impl InstallationIdentity {
    pub fn generate(installation_id: String) -> Result<Self, IdentityError> {
        if !validate_id(&installation_id) {
            return Err(IdentityError::InvalidInstallationId);
        }
        Ok(Self {
            installation_id,
            signing_key: SigningKey::generate(&mut OsRng),
        })
    }

    pub fn load(path: impl AsRef<Path>, installation_id: String) -> Result<Self, IdentityError> {
        if !validate_id(&installation_id) {
            return Err(IdentityError::InvalidInstallationId);
        }
        let path = path.as_ref();
        let metadata = fs::symlink_metadata(path)?;
        if !metadata.file_type().is_file() {
            return Err(IdentityError::InsecurePermissions);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(IdentityError::InsecurePermissions);
            }
        }
        let bytes = fs::read(path)?;
        let raw: [u8; 32] = bytes.try_into().map_err(|_| IdentityError::InvalidKey)?;
        Ok(Self {
            installation_id,
            signing_key: SigningKey::from_bytes(&raw),
        })
    }

    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), IdentityError> {
        use std::os::unix::fs::OpenOptionsExt;
        let path = path.as_ref();
        let mut options = fs::OpenOptions::new();
        options.create(true).truncate(true).write(true).mode(0o600);
        let mut file = options.open(path)?;
        use std::io::Write;
        file.write_all(&self.signing_key.to_bytes())?;
        file.sync_all()?;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
        Ok(())
    }

    pub fn installation_id(&self) -> &str {
        &self.installation_id
    }
    pub fn verifying_key(&self) -> VerifyingKey {
        self.signing_key.verifying_key()
    }
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(self.verifying_key().as_bytes());
        digest[..16]
            .chunks(2)
            .map(hex::encode_upper)
            .collect::<Vec<_>>()
            .join("-")
    }
    pub fn sign(&self, bytes: &[u8]) -> Signature {
        self.signing_key.sign(bytes)
    }
}

/// Provider IDs are accepted only at this local boundary. They are never
/// serializable and never recoverable from a handle outside this process.
#[derive(Default)]
pub struct ZoneHandleMap {
    by_handle: HashMap<String, String>,
    by_provider: HashMap<String, String>,
}

impl ZoneHandleMap {
    pub fn handle_for(&mut self, provider_id: &str) -> String {
        if let Some(handle) = self.by_provider.get(provider_id) {
            return handle.clone();
        }
        let mut bytes = [0u8; 18];
        loop {
            OsRng.fill_bytes(&mut bytes);
            let handle = format!(
                "zh_{}",
                base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
            );
            if !self.by_handle.contains_key(&handle) {
                self.by_handle
                    .insert(handle.clone(), provider_id.to_owned());
                self.by_provider
                    .insert(provider_id.to_owned(), handle.clone());
                return handle;
            }
        }
    }

    pub fn provider_id(&self, handle: &str) -> Option<&str> {
        self.by_handle.get(handle).map(String::as_str)
    }
    pub fn contains(&self, handle: &str) -> bool {
        self.by_handle.contains_key(handle)
    }
}

use base64::Engine as _;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
