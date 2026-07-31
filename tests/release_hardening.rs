use std::fs;

const MUSL_TARGETS: [&str; 3] = [
    "x86_64-unknown-linux-musl",
    "aarch64-unknown-linux-musl",
    "armv7-unknown-linux-musleabihf",
];

fn read(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|error| panic!("failed to read {path}: {error}"))
}

#[test]
fn musl_release_targets_enable_pie_and_full_relro() {
    let config = read(".cargo/config.toml");

    for target in MUSL_TARGETS {
        let heading = format!("[target.{target}]");
        let section = config
            .split(&heading)
            .nth(1)
            .unwrap_or_else(|| panic!("missing Cargo configuration for {target}"))
            .split("\n[")
            .next()
            .unwrap();

        assert!(
            section.contains("relocation-model=pic"),
            "{target} must generate position-independent code"
        );
        assert!(
            section.contains("relro-level=full"),
            "{target} must enable full RELRO"
        );
    }
}

#[test]
fn release_workflow_verifies_every_linux_binary() {
    let workflow = read(".github/workflows/build.yml");

    assert!(
        workflow.contains("scripts/check-elf-hardening.sh dist/bin/unified-hifi-linux-x64"),
        "x86_64 release artifacts must be checked before upload"
    );
    assert!(
        workflow.contains("scripts/check-elf-hardening.sh dist/bin/${{ matrix.output }}"),
        "the ARM matrix must check both aarch64 and armv7 artifacts before upload"
    );
}

#[test]
fn elf_hardening_check_follows_rust_verification_guidance() {
    let script = read("scripts/check-elf-hardening.sh");

    assert!(script.contains("readelf -h"));
    assert!(script.contains("DYN"), "PIE must be verified as ELF ET_DYN");
    assert!(script.contains("readelf -l"));
    assert!(script.contains("GNU_RELRO"));
    assert!(script.contains("readelf -d"));
    assert!(script.contains("BIND_NOW"));
}
