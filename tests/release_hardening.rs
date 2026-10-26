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
        assert!(
            section.contains("link-arg=-pie"),
            "{target} must explicitly select PIE with the Zig linker"
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

#[test]
fn all_distributable_builds_use_the_canonical_release_profile() {
    let workflow = read(".github/workflows/build.yml");
    assert!(
        !workflow.contains("CARGO_PROFILE_RELEASE_LTO")
            && !workflow.contains("CARGO_PROFILE_RELEASE_CODEGEN_UNITS"),
        "PR, manual, branch and tagged installers must use Cargo.toml's same shipping profile"
    );
    let cargo = read("Cargo.toml");
    let release = cargo
        .split("[profile.release]")
        .nth(1)
        .unwrap()
        .split("\n[")
        .next()
        .unwrap();
    for setting in [
        "lto = true",
        "codegen-units = 1",
        "strip = true",
        "debug = 0",
    ] {
        assert!(
            release.contains(setting),
            "missing canonical release setting: {setting}"
        );
    }
}

#[test]
fn browser_debug_is_rejected_before_assets_are_embedded_or_cached() {
    let workflow = read(".github/workflows/build.yml");
    let client_build = workflow.find("dx build --fullstack=false").unwrap();
    let finalization = workflow
        .find("python3 scripts/check-wasm-release.py")
        .expect("final WASM audit missing");
    let upload = workflow[client_build..]
        .find("uses: actions/upload-artifact")
        .unwrap()
        + client_build;
    assert!(
        client_build < finalization && finalization < upload,
        "final WASM debug audit must run after wasm-bindgen and before uploading browser assets"
    );
    assert!(
        workflow.contains("scripts/check-wasm-release.py',"),
        "the WASM cache key must include its final artifact audit"
    );
    let dioxus = read("Dioxus.toml");
    assert!(
        dioxus.contains("[web.wasm_opt]")
            && dioxus.contains("debug = false")
            && dioxus.contains("\"--strip-debug\"")
            && dioxus.contains("\"--strip-dwarf\""),
        "Dioxus must remove browser debug data before hashing final assets"
    );
}

#[test]
fn source_container_uses_the_same_audited_release_assets() {
    let docker = read("Dockerfile");
    assert!(
        docker.contains("COPY crates/ ./crates/"),
        "container must include the deployment backend"
    );
    assert!(docker.contains("COPY public/ ./public/"));
    assert!(docker.contains(
        "python3 scripts/check-wasm-release.py target/dx/unified-hifi-control/release/web/public"
    ));
    assert!(
        !docker.contains("COPY input.css tailwind.config.js"),
        "legacy Tailwind inputs no longer exist"
    );
}
