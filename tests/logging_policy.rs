//! Cross-platform contract for production logging and bounded file retention.

const CARGO: &str = include_str!("../Cargo.toml");
const MAIN: &str = include_str!("../src/main.rs");
const LOGGING: &str = include_str!("../src/logging.rs");
const QNAP: &str = include_str!("../build/qnap/shared/unified-hifi-control.sh");
const SYNOLOGY: &str = include_str!("../build/synology/scripts/start-stop-status");
const MACOS: &str = include_str!("../build/macos/com.cloudatlas.unified-hifi-control.plist");
const LINUX: &str = include_str!("../build/linux/unified-hifi-control.service");
const DOCKER: &str = include_str!("../Dockerfile");

#[test]
fn core_owns_one_daily_bounded_file_logging_policy() {
    assert!(CARGO.contains("tracing-appender"));
    assert!(MAIN.contains("logging::initialize"));
    assert!(LOGGING.contains("Rotation::DAILY"));
    assert!(LOGGING.contains("UHC_LOG_RETENTION_DAYS"));
    assert!(LOGGING.contains("DEFAULT_RETENTION_DAYS: usize = 7"));
    assert!(LOGGING.contains("max_log_files"));
    assert!(LOGGING.contains("unified_hifi_control=info"));
    // A bare `info` default keeps unlisted targets (anything outside the
    // explicit unified_hifi_control/tower_http/roon_api directives) at info
    // instead of silently falling back to EnvFilter's implicit error level,
    // matching the RUST_LOG=info policy set for Docker/systemd.
    assert!(LOGGING.contains(r#"DEFAULT_FILTER: &str = "info,unified_hifi_control=info"#));
}

#[test]
fn file_based_packages_select_the_core_destination_without_own_rotators() {
    for package in [QNAP, SYNOLOGY, MACOS] {
        assert!(package.contains("UHC_LOG_DIR"));
        assert!(!package.contains("UHC_LOG_CHECK_SECONDS"));
        assert!(!package.contains("LOG_ROTATOR_PIDF"));
    }
    assert!(!QNAP.contains("unified-hifi-control\" >> \"$LOGF\""));
    assert!(!SYNOLOGY.contains("nohup \"$BINARY\" >> \"$LOG_FILE\""));
}

#[test]
fn supervisor_managed_platforms_keep_their_native_log_sink() {
    assert!(LINUX.contains("StandardOutput=journal"));
    assert!(LINUX.contains("Environment=RUST_LOG=info"));
    assert!(DOCKER.contains("ENV RUST_LOG=info"));
    assert!(!DOCKER.contains("UHC_LOG_DIR"));
}
