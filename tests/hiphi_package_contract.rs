//! Package-neutral HiPhi enrollment contract.
//!
//! Settings invokes `uhc-hiphi-pair` beside the running server. Every package
//! must therefore ship both executables and keep `UHC_CONFIG_DIR` on durable
//! storage; otherwise the browser ceremony appears to work only on QNAP.

const WORKFLOW: &str = include_str!("../.github/workflows/build.yml");
const RELEASE_DOCKERFILE: &str = include_str!("../Dockerfile.release");
const CI_DOCKERFILE: &str = include_str!("../Dockerfile.ci");
const COMPOSE: &str = include_str!("../docker-compose.yml");
const SYNOLOGY_BUILD: &str = include_str!("../build/synology/build-spk.sh");
const SYNOLOGY_SERVICE: &str = include_str!("../build/synology/scripts/start-stop-status");
const LMS_HELPER: &str = include_str!("../lms-plugin/Helper.pm");
const LINUX_SERVICE: &str = include_str!("../build/linux/unified-hifi-control.service");
const PAIRING_API: &str = include_str!("../src/api/hiphi_pairing.rs");

#[test]
fn docker_and_home_assistant_ship_pairing_with_durable_identity() {
    for dockerfile in [RELEASE_DOCKERFILE, CI_DOCKERFILE] {
        assert!(dockerfile.contains("/app/uhc-hiphi-pair"));
        assert!(dockerfile.contains("chmod +x /app/unified-hifi-control /app/uhc-hiphi-pair"));
        assert!(dockerfile.contains("CONFIG_DIR=/data"));
    }
    assert!(COMPOSE.contains("- ./data:/data"));
    assert!(WORKFLOW.contains("dist/uhc-hiphi-pair"));
}

#[test]
fn synology_ships_pairing_beside_server_and_uses_package_state() {
    assert!(SYNOLOGY_BUILD.contains("<pairing-helper>"));
    assert!(SYNOLOGY_BUILD.contains("package/uhc-hiphi-pair"));
    assert!(SYNOLOGY_SERVICE.contains("export UHC_CONFIG_DIR=\"$VAR_DIR\""));
    assert!(WORKFLOW.contains("pairing_helper: uhc-hiphi-pair-x64"));
    assert!(WORKFLOW.contains("pairing_helper: uhc-hiphi-pair-arm64"));
}

#[test]
fn lms_ships_pairing_for_every_bundled_platform_and_uses_cache_state() {
    for directory in [
        "Bin/x86_64-linux/uhc-hiphi-pair",
        "Bin/aarch64-linux/uhc-hiphi-pair",
        "Bin/arm-linux/uhc-hiphi-pair",
        "Bin/darwin/uhc-hiphi-pair",
        "Bin/MSWin32-x64-multi-thread/uhc-hiphi-pair.exe",
    ] {
        assert!(
            WORKFLOW.contains(directory),
            "missing LMS helper {directory}"
        );
    }
    assert!(LMS_HELPER.contains("local $ENV{UHC_CONFIG_DIR} = $configDir"));
    assert!(LMS_HELPER.contains("my $binaryDir = dirname($binary)"));
    assert!(LMS_HELPER.contains("system('xattr', '-cr', $binaryDir)"));
}

#[test]
fn native_packages_and_direct_artifacts_ship_the_matching_helper() {
    for helper in [
        "uhc-hiphi-pair-x64=/usr/bin/uhc-hiphi-pair",
        "uhc-hiphi-pair-arm64=/usr/bin/uhc-hiphi-pair",
        "uhc-hiphi-pair-armv7=/usr/bin/uhc-hiphi-pair",
        "uhc-hiphi-pair-macos-universal",
        "uhc-hiphi-pair-win64.exe",
    ] {
        assert!(WORKFLOW.contains(helper), "missing native helper {helper}");
    }
    assert!(LINUX_SERVICE.contains("Environment=CONFIG_DIR=/etc/unified-hifi-control"));
    assert!(LINUX_SERVICE.contains("ConfigurationDirectory=unified-hifi-control"));
    assert!(PAIRING_API.contains("helper_candidates"));
}
