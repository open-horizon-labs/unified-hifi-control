//! Contract tests for the published Arch Linux package.

const PKGBUILD: &str = include_str!("../build/arch/PKGBUILD");
const SERVICE: &str = include_str!("../build/arch/unified-hifi-control.service");
const INSTALL: &str = include_str!("../build/arch/unified-hifi-control.install");
const ARCH_DOCS: &str = include_str!("../docs/arch-linux.md");
const RELEASE_WORKFLOW: &str = include_str!("../.github/workflows/build.yml");

#[test]
fn sources_are_immutable_and_verified() {
    assert!(
        !PKGBUILD.contains("'SKIP'"),
        "Arch package sources must have fixed integrity checks"
    );
    assert!(
        PKGBUILD.contains("v${pkgver}/LICENSE"),
        "the upstream license must be pinned to the packaged release"
    );
    assert!(
        PKGBUILD.contains("license=('PolyForm-Noncommercial-1.0.0')"),
        "the license field must use the SPDX identifier"
    );
}

#[test]
fn package_metadata_matches_packaged_files() {
    assert!(
        !PKGBUILD.contains("backup=('etc/unified-hifi-control/config.json')"),
        "backup entries must refer to files shipped by the package"
    );
    assert!(
        !PKGBUILD.lines().any(|line| {
            line.trim_start().starts_with('"')
                && line.contains("${_pkgname}.install")
        }),
        ".install files are handled by makepkg and must not be sources"
    );
    assert!(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/arch/LICENSE")
            .is_file(),
        "AUR packaging sources must carry a package-source license"
    );
    assert!(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/arch/REUSE.toml")
            .is_file(),
        "AUR packaging sources must carry REUSE metadata"
    );
    assert!(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("build/arch/LICENSES/0BSD.txt")
            .is_file(),
        "REUSE metadata must include the 0BSD license text"
    );
    assert!(
        PKGBUILD.contains("options=('!debug' '!strip')"),
        "pre-stripped release binaries must not produce empty debug packages"
    );
}

#[test]
fn service_uses_systemd_managed_writable_state() {
    assert!(SERVICE.contains("StateDirectory=unified-hifi-control"));
    assert!(
        SERVICE.contains("Environment=CONFIG_DIR=/var/lib/unified-hifi-control")
    );
    assert!(!SERVICE.contains("Environment=DATA_DIR="));
    assert!(!SERVICE.contains("Environment=CONFIG_DIR=/etc/"));
    assert!(!SERVICE.contains("ConfigurationDirectory="));
}

#[test]
fn package_hooks_do_not_restart_services_during_transactions() {
    assert!(
        !INSTALL.contains("systemctl restart"),
        "package upgrades must not restart a running service automatically"
    );
    assert!(INSTALL.contains("Restart the service when convenient"));
    assert!(ARCH_DOCS.contains("Upgrading from packages that used `/etc`"));
}

#[test]
fn release_workflow_publishes_complete_verified_sources() {
    for required in [
        "sha256sums_x86_64",
        "sha256sums_aarch64",
        "sha256sums_armv7h",
        "build/arch/LICENSE",
        "build/arch/LICENSES",
        "build/arch/REUSE.toml",
    ] {
        assert!(
            RELEASE_WORKFLOW.contains(required),
            "release workflow is missing {required}"
        );
    }
}

#[test]
fn documentation_describes_the_embedded_asset_package() {
    assert!(!ARCH_DOCS.contains("/usr/share/unified-hifi-control/public/"));
    assert!(!ARCH_DOCS.contains("sudo pacman -S nodejs npm"));
    assert!(!ARCH_DOCS.contains("cargo build --release"));
    assert!(ARCH_DOCS.contains("target/dx/unified-hifi-control/release/web/server"));
}
