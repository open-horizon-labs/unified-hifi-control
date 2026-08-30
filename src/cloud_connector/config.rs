use std::{env, path::PathBuf};

use super::transport::{EndpointError, RelayEndpoint};
use base64::Engine as _;

#[derive(Clone, Debug)]
pub struct CloudConnectorConfig {
    pub endpoint: RelayEndpoint,
    pub installation_id: String,
    pub key_path: PathBuf,
    pub epoch_path: PathBuf,
    pub issuer_key_id: String,
    pub issuer_public_key: Vec<u8>,
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
        let issuer_key_id = env::var("UHC_HIPHI_ISSUER_KEY_ID")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_ISSUER_KEY_ID"))?;
        let encoded_key = env::var("UHC_HIPHI_ISSUER_PUBLIC_KEY")
            .map_err(|_| ConfigError::Missing("UHC_HIPHI_ISSUER_PUBLIC_KEY"))?;
        Ok(Some(Self::from_values(
            config_dir.into(),
            &endpoint,
            &installation_id,
            &issuer_key_id,
            &encoded_key,
        )?))
    }

    fn from_values(
        config_dir: PathBuf,
        endpoint: &str,
        installation_id: &str,
        issuer_key_id: &str,
        encoded_key: &str,
    ) -> Result<Self, ConfigError> {
        let issuer_public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_key)
            .map_err(|_| ConfigError::Invalid("UHC_HIPHI_ISSUER_PUBLIC_KEY"))?;
        if uuid::Uuid::parse_str(installation_id).is_err()
            || issuer_public_key.len() != 32
            || issuer_key_id.is_empty()
        {
            return Err(ConfigError::Invalid("installation/key id"));
        }
        Ok(Self {
            endpoint: RelayEndpoint::parse(endpoint)?,
            installation_id: installation_id.to_owned(),
            key_path: config_dir.join("hiphi-installation.key"),
            epoch_path: config_dir.join("hiphi-relay-epoch"),
            issuer_key_id: issuer_key_id.to_owned(),
            issuer_public_key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn values_accept_an_installation_without_a_single_control_node() {
        let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode([7_u8; 32]);
        assert!(matches!(
            CloudConnectorConfig::from_values(
                PathBuf::from("/tmp/uhc-test"),
                "wss://cloud.example/v1/relay",
                "11111111-1111-4111-8111-111111111111",
                "issuer-1",
                &key,
            ),
            Ok(config) if config.installation_id == "11111111-1111-4111-8111-111111111111"
        ));
    }
}
