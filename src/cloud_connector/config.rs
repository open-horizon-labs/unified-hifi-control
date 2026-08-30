use std::{env, path::PathBuf};

use super::transport::{EndpointError, RelayEndpoint};
use base64::Engine as _;

#[derive(Clone, Debug)]
pub struct CloudConnectorConfig {
    pub endpoint: RelayEndpoint,
    pub installation_id: String,
    pub key_path: PathBuf,
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
        let issuer_public_key = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(encoded_key)
            .map_err(|_| ConfigError::Invalid("UHC_HIPHI_ISSUER_PUBLIC_KEY"))?;
        if !super::protocol::validate_id(&installation_id)
            || issuer_public_key.len() != 32
            || issuer_key_id.is_empty()
        {
            return Err(ConfigError::Invalid("installation/key id"));
        }
        Ok(Some(Self {
            endpoint: RelayEndpoint::parse(&endpoint)?,
            installation_id,
            key_path: config_dir.into().join("hiphi-installation.key"),
            issuer_key_id,
            issuer_public_key,
        }))
    }
}
