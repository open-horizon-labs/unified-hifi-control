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
}

fn default_port() -> u16 {
    8088
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

/// Subdirectory name for unified-hifi config files
/// Issue #76: Organize config files into a subdirectory to avoid clutter
const CONFIG_SUBDIR_NAME: &str = "unified-hifi";

/// Config files that should be migrated to the subdirectory
const MIGRATABLE_CONFIG_FILES: &[&str] = &[
    "app-settings.json",
    "lms-config.json",
    "hqp-config.json",
    "hqp-zone-links.json",
    "roon_state.json",
    "knobs.json",
];

/// Get config directory (XDG_CONFIG_HOME or platform default)
pub fn get_config_dir() -> std::path::PathBuf {
    // Check UHC-specific env var first
    if let Ok(dir) = std::env::var("UHC_CONFIG_DIR") {
        return std::path::PathBuf::from(dir);
    }
    // Support Node.js CONFIG_DIR for seamless migration
    if let Ok(dir) = std::env::var("CONFIG_DIR") {
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

/// Get config subdirectory for unified-hifi config files
/// Issue #76: Organize config files into unified-hifi/ subdirectory
pub fn get_config_subdir() -> std::path::PathBuf {
    get_config_dir().join(CONFIG_SUBDIR_NAME)
}

/// Get the path for a config file (always in subdirectory for new writes)
/// Issue #76: New files are written to the subdirectory
pub fn get_config_file_path(filename: &str) -> std::path::PathBuf {
    get_config_subdir().join(filename)
}

/// Read a config file with backwards compatibility fallback
/// Issue #76: Check subdirectory first, fall back to root for legacy files
pub fn read_config_file(filename: &str) -> Option<String> {
    let subdir_path = get_config_subdir().join(filename);
    let root_path = get_config_dir().join(filename);

    // Try subdirectory first (new location)
    if subdir_path.exists() {
        return std::fs::read_to_string(&subdir_path).ok();
    }

    // Fall back to root (legacy location)
    if root_path.exists() {
        return std::fs::read_to_string(&root_path).ok();
    }

    None
}

/// Migrate config files from root directory to subdirectory
/// Issue #76: On startup, move config files to unified-hifi/ subdirectory
pub fn migrate_config_to_subdir() {
    let config_dir = get_config_dir();
    let data_dir = get_data_dir();
    let subdir = config_dir.join(CONFIG_SUBDIR_NAME);

    // Ensure subdirectory exists
    if let Err(e) = std::fs::create_dir_all(&subdir) {
        tracing::warn!("Failed to create config subdirectory: {}", e);
        return;
    }

    // Migrate each config file from config dir root
    for filename in MIGRATABLE_CONFIG_FILES {
        migrate_single_file(&config_dir, &subdir, filename);
    }

    // Also check data directory for roon_state.json (may differ from config dir on Linux)
    // This handles the case where roon_state.json was previously in XDG_DATA_HOME
    if data_dir != config_dir {
        migrate_single_file(&data_dir, &subdir, "roon_state.json");
    }
}

/// Migrate a single file from source directory to subdirectory
fn migrate_single_file(source_dir: &std::path::Path, subdir: &std::path::Path, filename: &str) {
    let source_path = source_dir.join(filename);
    let subdir_path = subdir.join(filename);

    // Skip if file doesn't exist at source
    if !source_path.exists() {
        return;
    }

    // Don't overwrite existing files in subdirectory
    if subdir_path.exists() {
        tracing::debug!(
            "Skipping migration of {} (already exists in subdirectory)",
            filename
        );
        return;
    }

    // Move file from source to subdirectory
    match std::fs::rename(&source_path, &subdir_path) {
        Ok(()) => {
            tracing::info!(
                "Migrated config file: {} -> unified-hifi/{}",
                filename,
                filename
            );
        }
        Err(e) => {
            // If rename fails (e.g., cross-device), try copy + delete
            match std::fs::read(&source_path) {
                Ok(content) => {
                    if let Err(e) = std::fs::write(&subdir_path, &content) {
                        tracing::warn!("Failed to write migrated config {}: {}", filename, e);
                        return;
                    }
                    if let Err(e) = std::fs::remove_file(&source_path) {
                        tracing::warn!(
                            "Migrated {} but failed to remove original: {}",
                            filename,
                            e
                        );
                    } else {
                        tracing::info!(
                            "Migrated config file (copy): {} -> unified-hifi/{}",
                            filename,
                            filename
                        );
                    }
                }
                Err(_) => {
                    tracing::warn!("Failed to migrate config {}: {}", filename, e);
                }
            }
        }
    }
}

/// Get data directory (XDG_DATA_HOME or platform default)
pub fn get_data_dir() -> std::path::PathBuf {
    // Check UHC-specific env var first
    if let Ok(dir) = std::env::var("UHC_DATA_DIR") {
        return std::path::PathBuf::from(dir);
    }
    // Support Node.js CONFIG_DIR for seamless migration (Node.js uses same dir for config and data)
    if let Ok(dir) = std::env::var("CONFIG_DIR") {
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

/// Check if started from LMS UnifiedHiFi plugin (explicit signal)
/// The LMS plugin sets LMS_UNIFIEDHIFI_STARTED=true when launching the bridge
pub fn is_lms_plugin_started() -> bool {
    std::env::var("LMS_UNIFIEDHIFI_STARTED")
        .map(|v| v == "true" || v == "1")
        .unwrap_or(false)
}

pub fn load_config() -> Result<Config> {
    let config_dir = get_config_dir();

    let mut builder = ::config::Config::builder()
        // Start with defaults
        .set_default("port", 8088)?
        // Load from config file if it exists
        .add_source(
            ::config::File::with_name(&config_dir.join("config").to_string_lossy()).required(false),
        )
        // Override with environment variables (UHC_PORT, UHC_ROON__EXTENSION_ID, etc.)
        .add_source(
            ::config::Environment::with_prefix("UHC")
                .separator("__")
                .try_parsing(true),
        );

    // Support PORT env vars with explicit precedence: UHC_PORT > PORT > config > default
    // Handle manually to ensure consistent behavior across all environments
    if let Ok(port) = std::env::var("UHC_PORT") {
        if let Ok(port_num) = port.parse::<u16>() {
            builder = builder.set_override("port", port_num as i64)?;
        }
    } else if let Ok(port) = std::env::var("PORT") {
        // Legacy PORT fallback (used by LMS plugin Helper.pm, Docker, etc.)
        if let Ok(port_num) = port.parse::<u16>() {
            builder = builder.set_override("port", port_num as i64)?;
        }
    }

    // Support legacy LMS_HOST/LMS_PORT env vars (used by LMS plugin Helper.pm)
    if let Ok(host) = std::env::var("LMS_HOST") {
        builder = builder.set_override("lms.host", host)?;
    }
    if let Ok(port) = std::env::var("LMS_PORT") {
        if let Ok(port_num) = port.parse::<u16>() {
            builder = builder.set_override("lms.port", port_num as i64)?;
        }
    }

    let config = builder.build()?;

    Ok(config.try_deserialize()?)
}

/// Migrate Node.js config files to Rust format on startup
///
/// This function runs once at startup to seamlessly import Node.js configs:
/// - roon-config.json → roon_state.json (Roon pairing state)
/// - hqp-config.json (adjust port → web_port mapping)
/// - app-settings.json (handled by serde aliases in AppSettings)
/// - knobs.json (compatible format)
pub fn migrate_nodejs_configs() {
    let data_dir = get_data_dir();

    // Ensure data directory exists
    if let Err(e) = std::fs::create_dir_all(&data_dir) {
        tracing::warn!("Failed to create data directory: {}", e);
        return;
    }

    // Migrate Roon config (roon-config.json → roon_state.json)
    migrate_roon_config(&data_dir);

    // Migrate HQPlayer config (adjust port mapping)
    migrate_hqp_config(&data_dir);

    tracing::debug!("Node.js config migration check complete");
}

/// Migrate Roon config from Node.js format
fn migrate_roon_config(data_dir: &std::path::Path) {
    let nodejs_path = data_dir.join("roon-config.json");
    let rust_path = data_dir.join("roon_state.json");

    // Only migrate if Node.js config exists and Rust config doesn't
    if nodejs_path.exists() && !rust_path.exists() {
        match std::fs::read_to_string(&nodejs_path) {
            Ok(content) => {
                // The format is compatible - both use the same Roon API state structure
                match std::fs::write(&rust_path, &content) {
                    Ok(()) => {
                        tracing::info!(
                            "Migrated Roon config from Node.js: {} → {}",
                            nodejs_path.display(),
                            rust_path.display()
                        );
                    }
                    Err(e) => tracing::warn!("Failed to write Roon state file: {}", e),
                }
            }
            Err(e) => tracing::warn!("Failed to read Node.js Roon config: {}", e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::env;

    #[test]
    #[serial]
    fn test_lms_host_env_enables_lms_config() {
        // Issue #62: When LMS_HOST is set, config.lms should be Some
        // This simulates the LMS plugin starting the bridge with LMS_HOST=127.0.0.1

        // Set env var (will be cleaned up at end)
        env::set_var("LMS_HOST", "127.0.0.1");
        // Ensure no config file interferes (use temp dir)
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent");

        let config = load_config().expect("config should load");

        // Clean up
        env::remove_var("LMS_HOST");
        env::remove_var("UHC_CONFIG_DIR");

        // The key assertion: LMS should be configured when LMS_HOST is set
        assert!(
            config.lms.is_some(),
            "config.lms should be Some when LMS_HOST env var is set"
        );

        let lms = config.lms.unwrap();
        assert_eq!(lms.host, "127.0.0.1");
        assert_eq!(lms.port, 9000); // default port
    }

    #[test]
    #[serial]
    fn test_lms_host_and_port_env() {
        env::set_var("LMS_HOST", "192.168.1.100");
        env::set_var("LMS_PORT", "9001");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent");

        let config = load_config().expect("config should load");

        env::remove_var("LMS_HOST");
        env::remove_var("LMS_PORT");
        env::remove_var("UHC_CONFIG_DIR");

        assert!(config.lms.is_some());
        let lms = config.lms.unwrap();
        assert_eq!(lms.host, "192.168.1.100");
        assert_eq!(lms.port, 9001);
    }

    #[test]
    #[serial]
    fn test_lms_plugin_started_detection() {
        // Test the helper function that checks if started from LMS plugin
        env::set_var("LMS_UNIFIEDHIFI_STARTED", "true");
        assert!(is_lms_plugin_started());
        env::set_var("LMS_UNIFIEDHIFI_STARTED", "1");
        assert!(is_lms_plugin_started());
        env::set_var("LMS_UNIFIEDHIFI_STARTED", "false");
        assert!(!is_lms_plugin_started());
        env::remove_var("LMS_UNIFIEDHIFI_STARTED");
        assert!(!is_lms_plugin_started());
    }

    #[test]
    #[serial]
    fn test_port_env_fallback() {
        // Issue #75: PORT env var should work as fallback when UHC_PORT is not set
        // Clean slate - remove any existing port env vars
        env::remove_var("UHC_PORT");
        env::remove_var("PORT");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent");

        // Set only PORT (legacy)
        env::set_var("PORT", "3000");

        let config = load_config().expect("config should load");

        // Clean up
        env::remove_var("PORT");
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(config.port, 3000, "PORT env var should set config.port");
    }

    #[test]
    #[serial]
    fn test_uhc_port_takes_precedence_over_port() {
        // Issue #75: UHC_PORT should take precedence over legacy PORT
        env::remove_var("UHC_PORT");
        env::remove_var("PORT");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent");

        // Set both - UHC_PORT should win
        env::set_var("UHC_PORT", "5000");
        env::set_var("PORT", "3000");

        let config = load_config().expect("config should load");

        // Clean up
        env::remove_var("UHC_PORT");
        env::remove_var("PORT");
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(
            config.port, 5000,
            "UHC_PORT should take precedence over PORT"
        );
    }

    #[test]
    #[serial]
    fn test_invalid_port_uses_default() {
        // Invalid PORT value should fall back to default (8088)
        env::remove_var("UHC_PORT");
        env::remove_var("PORT");
        env::set_var("UHC_CONFIG_DIR", "/tmp/uhc-test-nonexistent");

        // Set invalid PORT
        env::set_var("PORT", "not-a-number");

        let config = load_config().expect("config should load");

        // Clean up
        env::remove_var("PORT");
        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(
            config.port, 8088,
            "Invalid PORT should fall back to default"
        );
    }

    // =========================================================================
    // Issue #76: Config subdirectory organization tests
    // =========================================================================

    #[test]
    #[serial]
    fn test_get_config_subdir_returns_unified_hifi_subdir() {
        // Issue #76: get_config_subdir() should return unified-hifi/ subdirectory
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        env::set_var("UHC_CONFIG_DIR", temp_dir.path());

        let subdir = get_config_subdir();

        env::remove_var("UHC_CONFIG_DIR");

        assert!(
            subdir.ends_with("unified-hifi"),
            "subdir should end with 'unified-hifi', got: {:?}",
            subdir
        );
        assert_eq!(
            subdir.parent().unwrap(),
            temp_dir.path(),
            "parent should be config dir"
        );
    }

    #[test]
    #[serial]
    fn test_migrate_config_files_to_subdir() {
        // Issue #76: migrate_config_to_subdir() should move files from root to subdirectory
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config_dir = temp_dir.path();

        // Create test config files at root level
        let files = ["app-settings.json", "lms-config.json", "hqp-config.json"];
        for file in &files {
            std::fs::write(config_dir.join(file), r#"{"test": true}"#).expect("write file");
        }

        env::set_var("UHC_CONFIG_DIR", config_dir);

        // Run migration
        migrate_config_to_subdir();

        env::remove_var("UHC_CONFIG_DIR");

        // Verify files moved to subdirectory
        let subdir = config_dir.join("unified-hifi");
        assert!(subdir.exists(), "subdirectory should be created");

        for file in &files {
            assert!(
                subdir.join(file).exists(),
                "file {} should exist in subdirectory",
                file
            );
            assert!(
                !config_dir.join(file).exists(),
                "file {} should not exist at root",
                file
            );
        }
    }

    #[test]
    #[serial]
    fn test_migration_skips_if_subdir_exists() {
        // Issue #76: If subdir already has files, don't overwrite them
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config_dir = temp_dir.path();
        let subdir = config_dir.join("unified-hifi");

        // Create subdirectory with existing config
        std::fs::create_dir_all(&subdir).expect("create subdir");
        std::fs::write(subdir.join("app-settings.json"), r#"{"existing": true}"#)
            .expect("write existing");

        // Create file at root (should not overwrite subdir file)
        std::fs::write(config_dir.join("app-settings.json"), r#"{"root": true}"#)
            .expect("write root");

        env::set_var("UHC_CONFIG_DIR", config_dir);

        migrate_config_to_subdir();

        env::remove_var("UHC_CONFIG_DIR");

        // Verify existing subdir file was not overwritten
        let content =
            std::fs::read_to_string(subdir.join("app-settings.json")).expect("read subdir file");
        assert!(
            content.contains("existing"),
            "subdir file should not be overwritten"
        );
    }

    #[test]
    #[serial]
    fn test_get_config_file_path_prefers_subdir() {
        // Issue #76: get_config_file_path() should check subdir first, fall back to root
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config_dir = temp_dir.path();
        let subdir = config_dir.join("unified-hifi");

        // Create file only in subdir
        std::fs::create_dir_all(&subdir).expect("create subdir");
        std::fs::write(subdir.join("lms-config.json"), r#"{"subdir": true}"#)
            .expect("write subdir");

        env::set_var("UHC_CONFIG_DIR", config_dir);

        let path = get_config_file_path("lms-config.json");

        env::remove_var("UHC_CONFIG_DIR");

        assert_eq!(path, subdir.join("lms-config.json"));
    }

    #[test]
    #[serial]
    fn test_get_config_file_path_always_returns_subdir() {
        // Issue #76: get_config_file_path() always returns subdir path for writes
        // Note: read_config_file() handles fallback to root for legacy files
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config_dir = temp_dir.path();

        // Create file only at root (legacy location)
        std::fs::write(config_dir.join("lms-config.json"), r#"{"root": true}"#)
            .expect("write root");

        env::set_var("UHC_CONFIG_DIR", config_dir);

        let path = get_config_file_path("lms-config.json");

        env::remove_var("UHC_CONFIG_DIR");

        // Should return subdir path (for new writes), even though file exists at root
        // The file reading logic handles fallback
        assert!(
            path.to_string_lossy().contains("unified-hifi"),
            "path should be in unified-hifi subdir for new writes"
        );
    }

    #[test]
    #[serial]
    fn test_read_config_file_with_fallback() {
        // Issue #76: Reading config should check subdir first, fall back to root
        let temp_dir = tempfile::tempdir().expect("create temp dir");
        let config_dir = temp_dir.path();

        // Create file only at root (legacy location)
        std::fs::write(config_dir.join("lms-config.json"), r#"{"legacy": true}"#)
            .expect("write root");

        env::set_var("UHC_CONFIG_DIR", config_dir);

        let content = read_config_file("lms-config.json");

        env::remove_var("UHC_CONFIG_DIR");

        assert!(content.is_some(), "should find legacy file at root");
        assert!(
            content.unwrap().contains("legacy"),
            "should read legacy content"
        );
    }
}

/// Migrate HQPlayer config from Node.js format
fn migrate_hqp_config(data_dir: &std::path::Path) {
    let hqp_path = data_dir.join("hqp-config.json");

    if !hqp_path.exists() {
        return;
    }

    // Read the existing config
    let content = match std::fs::read_to_string(&hqp_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    // Check if it's Node.js format (single object without web_port field)
    // Node.js format: {"host":"...", "port":8088, "username":"...", "password":"..."}
    // Rust format: {"host":"...", "port":4321, "web_port":8088, ...} or array format
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(&content) {
        // Skip if already migrated (has web_port or is array format)
        if value.is_array() {
            return;
        }
        if value.get("web_port").is_some() {
            return;
        }

        // It's Node.js single-object format - convert it
        if let Some(obj) = value.as_object() {
            let host = obj.get("host").and_then(|v| v.as_str()).unwrap_or("");
            let nodejs_port = obj.get("port").and_then(|v| v.as_u64()).unwrap_or(8088) as u16;
            let username = obj.get("username").and_then(|v| v.as_str());
            let password = obj.get("password").and_then(|v| v.as_str());

            // In Node.js, "port" is the web UI port (8088)
            // In Rust, "port" is the native protocol port (4321), "web_port" is web UI
            let rust_config = serde_json::json!([{
                "name": "default",
                "host": host,
                "port": 4321,  // Native protocol port
                "web_port": nodejs_port,  // Node.js port becomes web_port
                "username": username,
                "password": password
            }]);

            if let Ok(json) = serde_json::to_string_pretty(&rust_config) {
                match std::fs::write(&hqp_path, &json) {
                    Ok(()) => {
                        tracing::info!(
                            "Migrated HQPlayer config from Node.js format (port {} → web_port {})",
                            nodejs_port,
                            nodejs_port
                        );
                    }
                    Err(e) => tracing::warn!("Failed to write migrated HQP config: {}", e),
                }
            }
        }
    }
}
