use std::{env, path::PathBuf};

use super::transport::{EndpointError, RelayEndpoint};
use base64::Engine as _;

#[derive(Clone, Debug)]
pub struct CloudConnectorConfig {
    pub endpoint: RelayEndpoint,
    pub installation_id: String,
    pub key_path: PathBuf,
    pub epoch_path: PathBuf,
    pub session_issuer_key_id: String,
    pub session_issuer_public_key: Vec<u8>,
    pub command_issuer_key_id: String,
    pub command_issuer_public_key: Vec<u8>,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("missing required HiPhi Cloud setting {0}")]
    Missing(&'static str),
    #[error("invalid HiPhi Cloud setting {0}")]
    Invalid(&'static str),
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
        let session_issuer_key_id = env::var("UHC_HIPHI_SESSION_ISSUER_KEY_ID")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_SESSION_ISSUER_KEY_ID"))?;
        let encoded_session_key = env::var("UHC_HIPHI_SESSION_ISSUER_PUBLIC_KEY")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_SESSION_ISSUER_PUBLIC_KEY"))?;
        let command_issuer_key_id = env::var("UHC_HIPHI_COMMAND_ISSUER_KEY_ID")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_COMMAND_ISSUER_KEY_ID"))?;
        let encoded_command_key = env::var("UHC_HIPHI_COMMAND_ISSUER_PUBLIC_KEY")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_COMMAND_ISSUER_PUBLIC_KEY"))?;
        Ok(Some(Self::from_values(
            config_dir.into(),
            &endpoint,
            &installation_id,
            &session_issuer_key_id,
            &encoded_session_key,
            &command_issuer_key_id,
            &encoded_command_key,
        )?))
    }

    fn from_values(
        config_dir: PathBuf,
        endpoint: &str,
        installation_id: &str,
        session_issuer_key_id: &str,
        encoded_session_key: &str,
        command_issuer_key_id: &str,
        encoded_command_key: &str,
    ) -> Result<Self, ConfigError> {
        let session_issuer_public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_session_key)
            .map_err(|_| ConfigError::Invalid("UHC_HIPHI_SESSION_ISSUER_PUBLIC_KEY"))?;
        let command_issuer_public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_command_key)
            .map_err(|_| ConfigError::Invalid("UHC_HIPHI_COMMAND_ISSUER_PUBLIC_KEY"))?;
        if uuid::Uuid::parse_str(installation_id).is_err()
            || session_issuer_public_key.len() != 32
            || command_issuer_public_key.len() != 32
            || session_issuer_key_id.trim().is_empty()
            || command_issuer_key_id.trim().is_empty()
        {
            return Err(ConfigError::Invalid("installation/key id"));
        }
        if session_issuer_key_id == command_issuer_key_id
            || session_issuer_public_key == command_issuer_public_key
        {
            return Err(ConfigError::Invalid("issuer authority separation"));
        }
        Ok(Self {
            endpoint: RelayEndpoint::parse(endpoint)?,
            installation_id: installation_id.to_owned(),
            key_path: config_dir.join("hiphi-installation.key"),
            epoch_path: config_dir.join("hiphi-relay-epoch"),
            session_issuer_key_id: session_issuer_key_id.to_owned(),
            session_issuer_public_key,
            command_issuer_key_id: command_issuer_key_id.to_owned(),
            command_issuer_public_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_accept_distinct_session_and_command_authorities() {
        let session_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8; 32]);
        let command_key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([8_u8; 32]);
        assert!(matches!(
            CloudConnectorConfig::from_values(
                PathBuf::from("/tmp/uhc-test"),
                "wss://cloud.example/v1/relay",
                "11111111-1111-4111-8111-111111111111",
                "session-issuer-1",
                &session_key,
                "command-issuer-1",
                &command_key,
            ),
            Ok(config)
                if config.installation_id == "11111111-1111-4111-8111-111111111111"
                    && config.session_issuer_key_id == "session-issuer-1"
                    && config.command_issuer_key_id == "command-issuer-1"
        ));
    }

    #[test]
    fn values_reject_reused_session_and_command_authority() {
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8; 32]);
        assert!(matches!(
            CloudConnectorConfig::from_values(
                PathBuf::from("/tmp/uhc-test"),
                "wss://cloud.example/v1/relay",
                "11111111-1111-4111-8111-111111111111",
                "shared-issuer",
                &key,
                "shared-issuer",
                &key,
            ),
            Err(ConfigError::Invalid("issuer authority separation"))
        ));
    }
}
