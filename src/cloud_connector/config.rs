use std::{
    collections::{BTreeMap, HashSet},
    env,
    path::PathBuf,
};

use super::transport::{EndpointError, RelayEndpoint};
use base64::Engine as _;
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;

pub const MAX_ISSUER_KEYS: usize = 8;
const MAX_ISSUER_RING_JSON_BYTES: usize = 4 * 1024;
const MAX_PERSISTED_CONFIG_BYTES: u64 = 16 * 1024;

#[derive(Clone, Debug)]
pub struct IssuerVerifyingKeyRing {
    keys: Vec<(String, VerifyingKey)>,
}

impl IssuerVerifyingKeyRing {
    pub fn from_entries(
        entries: impl IntoIterator<Item = (String, VerifyingKey)>,
    ) -> Result<Self, ConfigError> {
        Self::validated(entries.into_iter().collect(), "issuer key ring")
    }

    fn validated(
        keys: Vec<(String, VerifyingKey)>,
        setting: &'static str,
    ) -> Result<Self, ConfigError> {
        if !(1..=MAX_ISSUER_KEYS).contains(&keys.len()) {
            return Err(ConfigError::Invalid(setting));
        }
        let mut key_ids = HashSet::with_capacity(keys.len());
        let mut public_keys = HashSet::with_capacity(keys.len());
        for (key_id, key) in &keys {
            if !valid_key_id(key_id)
                || !key_ids.insert(key_id.clone())
                || !public_keys.insert(key.to_bytes())
                || key.is_weak()
            {
                return Err(ConfigError::Invalid(setting));
            }
        }
        Ok(Self { keys })
    }

    pub fn get(&self, key_id: &str) -> Option<&VerifyingKey> {
        self.keys
            .iter()
            .find_map(|(candidate, key)| (candidate == key_id).then_some(key))
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &VerifyingKey)> {
        self.keys.iter().map(|(key_id, key)| (key_id.as_str(), key))
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuerKeyRingManifest {
    version: u8,
    keys: Vec<IssuerKeyManifestEntry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct IssuerKeyManifestEntry {
    kid: String,
    key: String,
}

#[derive(Clone, Debug)]
pub struct CloudConnectorConfig {
    pub endpoint: RelayEndpoint,
    pub installation_id: String,
    pub key_path: PathBuf,
    pub epoch_path: PathBuf,
    pub session_issuer_keys: IssuerVerifyingKeyRing,
    pub command_issuer_keys: IssuerVerifyingKeyRing,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required HiPhi Cloud setting {0}")]
    Missing(&'static str),
    #[error("invalid HiPhi Cloud setting {0}")]
    Invalid(&'static str),
    #[error("HiPhi Cloud persisted configuration is unsafe or invalid")]
    InvalidPersisted,
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Endpoint(#[from] EndpointError),
}

impl CloudConnectorConfig {
    /// Environment is deliberately opt-in. An installation never attempts a
    /// cloud connection merely because the public binary contains this code.
    pub fn from_env(config_dir: impl Into<PathBuf>) -> Result<Option<Self>, ConfigError> {
        let Some(endpoint) = env::var("UHC_HIPHI_RELAY_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
        else {
            return Ok(None);
        };
        let installation_id = env::var("UHC_HIPHI_INSTALLATION_ID")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_INSTALLATION_ID"))?;
        let session_issuer_keys = env::var("UHC_HIPHI_SESSION_ISSUER_KEYS")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_SESSION_ISSUER_KEYS"))?;
        let command_issuer_keys = env::var("UHC_HIPHI_COMMAND_ISSUER_KEYS")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_COMMAND_ISSUER_KEYS"))?;
        Ok(Some(Self::from_values(
            config_dir.into(),
            &endpoint,
            &installation_id,
            &session_issuer_keys,
            &command_issuer_keys,
        )?))
    }

    /// Load connector configuration without depending on a packaging-specific
    /// launcher. Explicit process environment wins; otherwise UHC reads the
    /// owner-only file written by the pairing ceremony itself.
    pub fn from_runtime(config_dir: impl Into<PathBuf>) -> Result<Option<Self>, ConfigError> {
        let config_dir = config_dir.into();
        match Self::from_env(config_dir.clone())? {
            Some(config) => Ok(Some(config)),
            None => Self::from_persisted(config_dir),
        }
    }

    pub fn from_persisted(config_dir: impl Into<PathBuf>) -> Result<Option<Self>, ConfigError> {
        let config_dir = config_dir.into();
        let path = config_dir.join("hiphi.env");
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !metadata.file_type().is_file() || metadata.len() > MAX_PERSISTED_CONFIG_BYTES {
            return Err(ConfigError::InvalidPersisted);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            if metadata.permissions().mode() & 0o077 != 0 {
                return Err(ConfigError::InvalidPersisted);
            }
        }
        let contents = std::fs::read_to_string(&path)?;
        if contents.trim().is_empty() {
            return Ok(None);
        }
        let mut values = parse_persisted_config(&contents)?;
        let endpoint = take_persisted(&mut values, "UHC_HIPHI_RELAY_URL")?;
        let installation_id = take_persisted(&mut values, "UHC_HIPHI_INSTALLATION_ID")?;
        let session_keys = take_persisted(&mut values, "UHC_HIPHI_SESSION_ISSUER_KEYS")?;
        let command_keys = take_persisted(&mut values, "UHC_HIPHI_COMMAND_ISSUER_KEYS")?;
        if !values.is_empty() {
            return Err(ConfigError::InvalidPersisted);
        }
        Ok(Some(Self::from_values(
            config_dir,
            &endpoint,
            &installation_id,
            &session_keys,
            &command_keys,
        )?))
    }

    /// Validate an explicit connector binding before it is persisted by a
    /// local enrollment tool. This applies the same rules as service startup.
    pub fn from_values(
        config_dir: PathBuf,
        endpoint: &str,
        installation_id: &str,
        encoded_session_keys: &str,
        encoded_command_keys: &str,
    ) -> Result<Self, ConfigError> {
        if uuid::Uuid::parse_str(installation_id).is_err() {
            return Err(ConfigError::Invalid("installation/key id"));
        }
        let session_issuer_keys =
            parse_key_ring(encoded_session_keys, "UHC_HIPHI_SESSION_ISSUER_KEYS")?;
        let command_issuer_keys =
            parse_key_ring(encoded_command_keys, "UHC_HIPHI_COMMAND_ISSUER_KEYS")?;
        validate_distinct_authorities(&session_issuer_keys, &command_issuer_keys)?;
        Ok(Self {
            endpoint: RelayEndpoint::parse(endpoint)?,
            installation_id: installation_id.to_owned(),
            key_path: config_dir.join("hiphi-installation.key"),
            epoch_path: config_dir.join("hiphi-relay-epoch"),
            session_issuer_keys,
            command_issuer_keys,
        })
    }
}

fn parse_persisted_config(contents: &str) -> Result<BTreeMap<String, String>, ConfigError> {
    let mut values = BTreeMap::new();
    for line in contents.lines() {
        let (name, value) = line.split_once('=').ok_or(ConfigError::InvalidPersisted)?;
        if name.is_empty()
            || value.is_empty()
            || values.insert(name.to_owned(), value.to_owned()).is_some()
        {
            return Err(ConfigError::InvalidPersisted);
        }
    }
    Ok(values)
}

fn take_persisted(
    values: &mut BTreeMap<String, String>,
    name: &'static str,
) -> Result<String, ConfigError> {
    values.remove(name).ok_or(ConfigError::InvalidPersisted)
}

fn parse_key_ring(
    encoded: &str,
    setting: &'static str,
) -> Result<IssuerVerifyingKeyRing, ConfigError> {
    if encoded.len() > MAX_ISSUER_RING_JSON_BYTES {
        return Err(ConfigError::Invalid(setting));
    }
    let manifest: IssuerKeyRingManifest =
        serde_json::from_str(encoded).map_err(|_| ConfigError::Invalid(setting))?;
    if manifest.version != 1 {
        return Err(ConfigError::Invalid(setting));
    }
    let mut keys = Vec::with_capacity(manifest.keys.len().min(MAX_ISSUER_KEYS));
    for entry in manifest.keys {
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&entry.key)
            .map_err(|_| ConfigError::Invalid(setting))?;
        if base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(&bytes) != entry.key {
            return Err(ConfigError::Invalid(setting));
        }
        let key = VerifyingKey::from_bytes(
            &bytes
                .try_into()
                .map_err(|_| ConfigError::Invalid(setting))?,
        )
        .map_err(|_| ConfigError::Invalid(setting))?;
        keys.push((entry.kid, key));
    }
    IssuerVerifyingKeyRing::validated(keys, setting)
}

fn validate_distinct_authorities(
    session: &IssuerVerifyingKeyRing,
    command: &IssuerVerifyingKeyRing,
) -> Result<(), ConfigError> {
    if session.iter().any(|(session_id, session_key)| {
        command
            .iter()
            .any(|(command_id, command_key)| session_id == command_id || session_key == command_key)
    }) {
        return Err(ConfigError::Invalid("issuer authority separation"));
    }
    Ok(())
}

fn valid_key_id(value: &str) -> bool {
    (1..=64).contains(&value.len())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    fn key_ring(entries: &[(&str, u8)]) -> String {
        key_ring_owned(
            entries
                .iter()
                .map(|(kid, byte)| ((*kid).to_owned(), *byte))
                .collect(),
        )
    }

    fn key_ring_owned(entries: Vec<(String, u8)>) -> String {
        serde_json::json!({
            "version": 1,
            "keys": entries
                .iter()
                .map(|(kid, byte)| serde_json::json!({
                    "kid": kid,
                    "key": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(
                        SigningKey::from_bytes(&[*byte; 32]).verifying_key().to_bytes()
                    )
                }))
                .collect::<Vec<_>>()
        })
        .to_string()
    }

    #[test]
    fn values_accept_distinct_session_and_command_authorities() {
        let session_keys = key_ring(&[("session-issuer-1", 7), ("session-issuer-2", 8)]);
        let command_keys = key_ring(&[("command-issuer-1", 9), ("command-issuer-2", 10)]);
        assert!(matches!(
            CloudConnectorConfig::from_values(
                PathBuf::from("/tmp/uhc-test"),
                "wss://cloud.example/v1/relay",
                "11111111-1111-4111-8111-111111111111",
                &session_keys,
                &command_keys,
            ),
            Ok(config)
                if config.installation_id == "11111111-1111-4111-8111-111111111111"
                    && config.session_issuer_keys.len() == 2
                    && config.command_issuer_keys.len() == 2
        ));
    }

    #[test]
    fn values_reject_malformed_duplicate_oversized_and_cross_authority_rings() {
        let valid_session = key_ring(&[("session-issuer-1", 7)]);
        let valid_command = key_ring(&[("command-issuer-1", 8)]);
        let mut weak_key = [0_u8; 32];
        weak_key[0] = 1;
        for malformed in [
            "not-json".to_owned(),
            r#"{"version":2,"keys":[{"kid":"session-issuer-1","key":"bad"}]}"#.to_owned(),
            r#"{"version":1,"keys":[]}"#.to_owned(),
            r#"{"version":1,"keys":[{"kid":"invalid-key","key":"not-base64url"}]}"#.to_owned(),
            serde_json::json!({
                "version": 1,
                "keys": [{
                    "kid": "weak-key",
                    "key": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(weak_key)
                }]
            })
            .to_string(),
            key_ring(&[("duplicate", 7), ("duplicate", 8)]),
            key_ring(&[("same-key-1", 7), ("same-key-2", 7)]),
            key_ring_owned(
                (0..=MAX_ISSUER_KEYS)
                    .map(|index| (format!("key-{index}"), index as u8 + 1))
                    .collect(),
            ),
            format!(
                "{{\"version\":1,\"keys\":[],\"padding\":\"{}\"}}",
                "x".repeat(MAX_ISSUER_RING_JSON_BYTES)
            ),
        ] {
            assert!(CloudConnectorConfig::from_values(
                PathBuf::from("/tmp/uhc-test"),
                "wss://cloud.example/v1/relay",
                "11111111-1111-4111-8111-111111111111",
                &malformed,
                &valid_command,
            )
            .is_err());
        }

        let reused_id = key_ring(&[("shared-issuer", 7)]);
        assert!(matches!(
            CloudConnectorConfig::from_values(
                PathBuf::from("/tmp/uhc-test"),
                "wss://cloud.example/v1/relay",
                "11111111-1111-4111-8111-111111111111",
                &reused_id,
                &key_ring(&[("shared-issuer", 8)]),
            ),
            Err(ConfigError::Invalid("issuer authority separation"))
        ));
        assert!(matches!(
            CloudConnectorConfig::from_values(
                PathBuf::from("/tmp/uhc-test"),
                "wss://cloud.example/v1/relay",
                "11111111-1111-4111-8111-111111111111",
                &valid_session,
                &key_ring(&[("command-issuer-1", 7)]),
            ),
            Err(ConfigError::Invalid("issuer authority separation"))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_runtime_config_is_owner_only_exact_and_complete() {
        use std::os::unix::fs::PermissionsExt as _;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("hiphi.env");
        let session_keys = key_ring(&[("session-issuer-1", 7)]);
        let command_keys = key_ring(&[("command-issuer-1", 8)]);
        std::fs::write(
            &path,
            format!(
                "UHC_HIPHI_RELAY_URL=wss://cloud.example/v1/relay\n\
                 UHC_HIPHI_INSTALLATION_ID=11111111-1111-4111-8111-111111111111\n\
                 UHC_HIPHI_SESSION_ISSUER_KEYS={session_keys}\n\
                 UHC_HIPHI_COMMAND_ISSUER_KEYS={command_keys}\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let config = CloudConnectorConfig::from_persisted(directory.path())
            .unwrap()
            .expect("complete config should load");
        assert_eq!(
            config.installation_id,
            "11111111-1111-4111-8111-111111111111"
        );

        for invalid in [
            format!(
                "UHC_HIPHI_RELAY_URL=wss://cloud.example/v1/relay\n\
                 UHC_HIPHI_INSTALLATION_ID=11111111-1111-4111-8111-111111111111\n\
                 UHC_HIPHI_SESSION_ISSUER_KEYS={session_keys}\n"
            ),
            format!(
                "UHC_HIPHI_RELAY_URL=wss://cloud.example/v1/relay\n\
                 UHC_HIPHI_RELAY_URL=wss://cloud.example/v1/relay\n\
                 UHC_HIPHI_INSTALLATION_ID=11111111-1111-4111-8111-111111111111\n\
                 UHC_HIPHI_SESSION_ISSUER_KEYS={session_keys}\n\
                 UHC_HIPHI_COMMAND_ISSUER_KEYS={command_keys}\n"
            ),
            format!(
                "UHC_HIPHI_RELAY_URL=wss://cloud.example/v1/relay\n\
                 UHC_HIPHI_INSTALLATION_ID=11111111-1111-4111-8111-111111111111\n\
                 UHC_HIPHI_SESSION_ISSUER_KEYS={session_keys}\n\
                 UHC_HIPHI_COMMAND_ISSUER_KEYS={command_keys}\n\
                 SHELL_COMMAND=touch /tmp/nope\n"
            ),
        ] {
            std::fs::write(&path, invalid).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
            assert!(CloudConnectorConfig::from_persisted(directory.path()).is_err());
        }

        std::fs::write(&path, "").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert!(CloudConnectorConfig::from_persisted(directory.path())
            .unwrap()
            .is_none());

        std::fs::write(&path, "UHC_HIPHI_RELAY_URL=wss://cloud.example/v1/relay\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(CloudConnectorConfig::from_persisted(directory.path()).is_err());
    }
}
