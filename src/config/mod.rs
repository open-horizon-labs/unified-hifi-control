//! Configuration management

use anyhow::Result;
use serde::Deserialize;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,

    #[serde(default)]
    pub roon: RoonConfig,

    #[serde(default)]
    pub hqplayer: Option<HqpConfig>,

    #[serde(default)]
    pub lms: Option<LmsConfig>,

    #[serde(default)]
    pub mqtt: Option<MqttConfig>,
}

fn default_port() -> u16 {
    3000
}

#[derive(Debug, Default, Deserialize)]
pub struct RoonConfig {
    pub extension_id: Option<String>,
    pub display_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct HqpConfig {
    pub host: String,
    #[serde(default = "default_hqp_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

fn default_hqp_port() -> u16 {
    8088
}

#[derive(Debug, Deserialize)]
pub struct LmsConfig {
    pub host: String,
    #[serde(default = "default_lms_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

fn default_lms_port() -> u16 {
    9000
}

#[derive(Debug, Deserialize)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    pub topic_prefix: Option<String>,
}

fn default_mqtt_port() -> u16 {
    1883
}

/// Get config directory (XDG_CONFIG_HOME or platform default)
pub fn get_config_dir() -> std::path::PathBuf {
    // Check environment variable first
    if let Ok(dir) = std::env::var("UHC_CONFIG_DIR") {
        return std::path::PathBuf::from(dir);
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home)
                .join("Library/Application Support/unified-hifi-control");
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
            return std::path::PathBuf::from(xdg).join("unified-hifi-control");
        }
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(".config/unified-hifi-control");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            return std::path::PathBuf::from(appdata).join("unified-hifi-control");
        }
    }

    // Fallback to current directory
    std::path::PathBuf::from(".")
}

/// Get data directory (XDG_DATA_HOME or platform default)
pub fn get_data_dir() -> std::path::PathBuf {
    // Check environment variable first
    if let Ok(dir) = std::env::var("UHC_DATA_DIR") {
        return std::path::PathBuf::from(dir);
    }

    #[cfg(target_os = "macos")]
    {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home)
                .join("Library/Application Support/unified-hifi-control");
        }
    }

    #[cfg(target_os = "linux")]
    {
        if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
            return std::path::PathBuf::from(xdg).join("unified-hifi-control");
        }
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(".local/share/unified-hifi-control");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Ok(appdata) = std::env::var("LOCALAPPDATA") {
            return std::path::PathBuf::from(appdata).join("unified-hifi-control");
        }
    }

    // Fallback to ./data
    std::path::PathBuf::from("./data")
}

pub fn load_config() -> Result<Config> {
    let config_dir = get_config_dir();

    let config = ::config::Config::builder()
        // Start with defaults
        .set_default("port", 3000)?
        // Load from config file if it exists
        .add_source(
            ::config::File::with_name(&config_dir.join("config").to_string_lossy())
                .required(false),
        )
        // Override with environment variables (UHC_PORT, UHC_ROON__EXTENSION_ID, etc.)
        .add_source(
            ::config::Environment::with_prefix("UHC")
                .separator("__")
                .try_parsing(true),
        )
        .build()?;

    Ok(config.try_deserialize()?)
}
