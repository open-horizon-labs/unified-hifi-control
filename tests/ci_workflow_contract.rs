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
    assert!(!linux_x64.contains("sudo mv zig-linux"));
    assert!(linux_x64.contains(r#"test -x "$STAGED_ROOT/zig""#));
    assert!(linux_x64.contains(r#"rm -rf "$ZIG_ROOT""#));
}

#[test]
fn parallel_fleet_workers_do_not_share_mutable_rust_toolchains() {
    let source = workflow("build.yml");

    for name in ["lint", "test", "build-wasm", "build-linux-x64"] {
        let body = job(&source, name);
        assert!(body.contains(
            r#"echo "CARGO_HOME=${RUNNER_TOOL_CACHE}/uhc/${RUNNER_NAME}/cargo" >> "$GITHUB_ENV""#
        ));
        assert!(body.contains(
            r#"echo "RUSTUP_HOME=${RUNNER_TOOL_CACHE}/uhc/${RUNNER_NAME}/rustup" >> "$GITHUB_ENV""#
        ));
    }
}
