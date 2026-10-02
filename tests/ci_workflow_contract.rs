use std::{fs, path::PathBuf};

const DEVELOPMENT_BRANCH: &str = "v4";

fn workflow(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

fn job(source: &str, name: &str) -> String {
    source
        .split(&format!("\n  {name}:"))
        .nth(1)
        .unwrap_or_else(|| panic!("build.yml has no {name} job"))
        .lines()
        .take_while(|line| line.trim().is_empty() || line.starts_with("    "))
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn development_pull_requests_run_rust_and_api_contract_checks() {
    for name in ["build.yml", "api-guard.yml"] {
        let source = workflow(name);
        let pull_request = source
            .split("pull_request:")
            .nth(1)
            .unwrap_or_else(|| panic!("{name} has no pull_request trigger"));
        let trigger = pull_request.split("jobs:").next().unwrap_or(pull_request);
        assert!(
            trigger.contains(DEVELOPMENT_BRANCH),
            "{name} must run for pull requests targeting {DEVELOPMENT_BRANCH}"
        );
    }
}

#[test]
fn development_home_assistant_changes_run_the_ha_workflow() {
    let source = workflow("ha-integration.yml");
    let development_branch_triggers = source
        .lines()
        .filter(|line| {
            line.trim_start().starts_with("branches:") && line.contains(DEVELOPMENT_BRANCH)
        })
        .count();
    assert_eq!(
        development_branch_triggers, 2,
        "HA integration must cover v4 push and pull-request triggers"
    );
}

#[test]
fn development_does_not_enable_edge_image_publication() {
    let source = workflow("docker.yml");
    let development_branch_triggers = source.lines().any(|line| {
        line.trim_start().starts_with("branches:") && line.contains(DEVELOPMENT_BRANCH)
    });
    assert!(
        !development_branch_triggers,
        "v4 work must not enter the Docker edge publication workflow"
    );
}

// The contract is the capability, not the machine. Pinning `nuc14` here is what
// let the workflow name a host that no longer exists: the test agreed with the
// workflow and both were wrong together. A capability label is placeable by the
// broker on whatever machine currently serves it.
#[test]
fn trusted_expensive_linux_jobs_ask_for_a_capability_with_a_hosted_fork_fallback() {
    let source = workflow("build.yml");
    let selector = r#"vars.LOCAL_LINUX_CI_ENABLED == 'true' && (github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository) && fromJSON('["self-hosted","linux","x64","linux-general"]') || 'ubuntu-latest'"#;

    for name in ["lint", "test", "build-wasm", "build-linux-x64"] {
        let body = job(&source, name);
        assert!(
            body.contains(selector),
            "{name} must ask for the linux-general capability for trusted work and ubuntu-latest for fork PRs"
        );
        assert!(
            !body.contains("nuc14"),
            "{name} still names a host rather than a capability"
        );
    }
}

#[test]
fn jobs_that_need_playwright_or_docker_stay_on_hosted_ubuntu() {
    let source = workflow("build.yml");

    // synology-package-test joined this list after a fleet migration moved it and
    // its lifecycle script died on `docker: command not found`. The job runs
    // inside a container with no docker CLI and no host socket; granting one
    // would hand every job the container engine, so the job stays hosted.
    for name in ["smoke-test", "build-qnap-x64", "synology-package-test"] {
        assert!(
            job(&source, name).contains("runs-on: ubuntu-latest"),
            "{name} requires tooling absent from the ephemeral fleet runner image"
        );
    }
}

#[test]
fn linux_x64_tool_install_is_safe_on_a_persistent_runner() {
    let source = workflow("build.yml");
    let linux_x64 = job(&source, "build-linux-x64");

    assert!(linux_x64.contains("RUNNER_TOOL_CACHE"));
    assert!(linux_x64.contains("Using runner-provided Zig"));
    assert!(!linux_x64.contains("sudo mv zig-linux"));
    assert!(linux_x64.contains(r#"test -x "$STAGED_ROOT/zig""#));
    assert!(linux_x64.contains(r#"rm -rf "$ZIG_ROOT""#));
}

#[test]
fn zigbuild_tool_cache_is_versioned_and_validated() {
    let source = workflow("build.yml");

    for name in ["build-linux-x64", "build-linux-arm"] {
        let body = job(&source, name);
        assert!(
            body.contains("cargo-zigbuild-${{ runner.os }}-${{ runner.arch }}-0.23.4"),
            "{name} must key the cargo-zigbuild cache by platform and pinned version"
        );
        assert!(
            body.contains("cargo-zigbuild --version | grep -q 'cargo-zigbuild 0.23.4'"),
            "{name} must validate a restored cargo-zigbuild binary before using it"
        );
        assert!(
            body.contains("cargo install cargo-zigbuild --version 0.23.4 --locked"),
            "{name} must install the same version named by its cache key"
        );
        assert!(
            body.contains("path: ${{ runner.tool_cache }}/zig/0.13.0/"),
            "{name} must restore Zig from the runner tool cache"
        );
        assert!(
            body.find("name: Cache Zig") < body.find("name: Install zig"),
            "{name} must restore Zig before checking whether an install is needed"
        );
    }
}

#[test]
fn server_artifacts_include_the_naa_proxy() {
    let source = workflow("build.yml");

    for name in [
        "build-linux-x64",
        "build-linux-arm",
        "build-macos-x64",
        "build-macos-arm64",
        "build-windows",
    ] {
        let body = job(&source, name);
        assert!(
            body.contains("--features naa-proxy"),
            "{name} must compile the installed server artifact with the NAA relay"
        );
    }
}

#[test]
fn dioxus_cli_cache_matches_the_isolated_cargo_home() {
    let source = workflow("build.yml");
    let wasm = job(&source, "build-wasm");

    assert!(
        wasm.contains("path: ${{ runner.tool_cache }}/uhc/${{ runner.name }}/cargo/bin/dx"),
        "Dioxus CLI cache must use the isolated runner Cargo bin path"
    );
    assert!(
        wasm.contains("key: dx-cli-${{ runner.os }}-${{ runner.arch }}-0.7.10"),
        "Dioxus CLI cache must be scoped by runner platform and pinned version"
    );
    assert!(
        wasm.find("name: Cache Dioxus CLI") < wasm.find("name: Install Dioxus CLI"),
        "Dioxus CLI cache must restore before installation"
    );
}

#[test]
fn parallel_fleet_workers_do_not_share_mutable_rust_toolchains() {
    let source = workflow("build.yml");

    for name in [
        "lint",
        "test",
        "build-wasm",
        "build-linux-x64",
        "build-linux-arm",
    ] {
        let body = job(&source, name);
        assert!(body.contains(
            r#"echo "CARGO_HOME=${RUNNER_TOOL_CACHE}/uhc/${RUNNER_NAME}/cargo" >> "$GITHUB_ENV""#
        ));
        assert!(body.contains(
            r#"echo "RUSTUP_HOME=${RUNNER_TOOL_CACHE}/uhc/${RUNNER_NAME}/rustup" >> "$GITHUB_ENV""#
        ));
    }
}

#[test]
fn blocking_rust_setups_share_one_reviewed_baseline() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let pin =
        fs::read_to_string(root.join("rust-toolchain.toml")).expect("reviewed baseline is missing");
    let channel = pin
        .lines()
        .find(|line| line.starts_with("channel = "))
        .expect("pin needs a channel");
    let release = channel.split('"').nth(1).unwrap();
    let parts: Vec<_> = release.split('.').collect();
    assert!(
        parts.len() == 3
            && parts
                .iter()
                .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit())),
        "blocking baseline must name an exact release"
    );
    let action = fs::read_to_string(root.join(".github/actions/setup-rust/action.yml")).unwrap();
    assert!(action.contains("rust-toolchain.toml"));
    assert!(action.contains("RUSTUP_TOOLCHAIN="));
    for entry in fs::read_dir(root.join(".github/workflows")).unwrap() {
        let path = entry.unwrap().path();
        let source = fs::read_to_string(&path).unwrap();
        let baseline_source = if source.contains("\n  lint-latest-stable:") {
            source.replace(&job(&source, "lint-latest-stable"), "")
        } else {
            source.clone()
        };
        assert!(
            !baseline_source.contains("dtolnay/rust-toolchain@"),
            "{} bypasses the reviewed setup action",
            path.display()
        );
        assert!(
            !source.contains("dtolnay/rust-toolchain@stable"),
            "{} floats a blocking Rust setup",
            path.display()
        );
    }
}

#[test]
fn every_lint_path_builds_css_before_compilation() {
    let directory = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        let name = path.display();
        let source = fs::read_to_string(&path).unwrap();
        for id in source
            .lines()
            .filter(|line| {
                line.starts_with("  ") && !line.starts_with("    ") && line.ends_with(':')
            })
            .map(|line| line.trim().trim_end_matches(':'))
        {
            let body = job(&source, id);
            if !body.contains("cargo clippy") && !body.contains("cargo +stable clippy") {
                continue;
            }
            let compile = body
                .find("cargo clippy")
                .or_else(|| body.find("cargo +stable clippy"))
                .unwrap();
            assert!(
                body[..compile].contains("make css"),
                "{name} lint path lacks its embedded CSS prerequisite"
            );
            assert!(
                body.contains("-- -D warnings"),
                "{name} weakened lint enforcement"
            );
        }
    }
}

#[test]
fn latest_stable_lints_are_visible_and_advisory() {
    let source = workflow("build.yml");
    let advisory = job(&source, "lint-latest-stable");
    assert!(advisory.contains("continue-on-error: true"));
    assert!(advisory.contains("cargo +stable clippy -- -D warnings"));
    assert!(advisory.contains("if: always()"));
    assert!(advisory.contains("$GITHUB_STEP_SUMMARY"));
    assert!(job(&source, "lint").contains("$GITHUB_STEP_SUMMARY"));
    assert!(!job(&source, "lint").contains("continue-on-error: true"));
}

#[test]
fn compiler_changes_invalidate_wasm_output_and_trigger_platform_validation() {
    let source = workflow("build.yml");
    let wasm = job(&source, "build-wasm");
    let key = wasm
        .lines()
        .find(|line| line.trim_start().starts_with("key: wasm-"))
        .expect("WASM cache key is missing");
    for input in [
        "rust-toolchain.toml",
        ".github/actions/setup-rust/action.yml",
    ] {
        assert!(
            key.contains(input),
            "WASM cache does not include compiler input {input}"
        );
        for name in [
            "streaming-alpha.yml",
            "hiphi-cloud-connector.yml",
            "windows-connector.yml",
        ] {
            let source = workflow(name);
            let triggers = source.split("jobs:").next().unwrap();
            assert!(
                triggers.contains(input),
                "{name} skips validation for compiler input {input}"
            );
        }
    }
}
