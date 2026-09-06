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

#[test]
fn trusted_expensive_linux_jobs_use_the_nuc_with_a_hosted_fork_fallback() {
    let source = workflow("build.yml");
    let selector = r#"vars.LOCAL_LINUX_CI_ENABLED == 'true' && (github.event_name != 'pull_request' || github.event.pull_request.head.repo.full_name == github.repository) && fromJSON('["self-hosted","linux","x64","nuc14"]') || 'ubuntu-latest'"#;

    for name in [
        "lint",
        "test",
        "build-wasm",
        "build-linux-x64",
        "smoke-test",
        "build-qnap-x64",
    ] {
        assert!(
            job(&source, name).contains(selector),
            "{name} must use nuc14 for trusted work and ubuntu-latest for fork PRs"
        );
    }
}

#[test]
fn linux_x64_tool_install_is_safe_on_a_persistent_runner() {
    let source = workflow("build.yml");
    let linux_x64 = job(&source, "build-linux-x64");

    assert!(linux_x64.contains("RUNNER_TOOL_CACHE"));
    assert!(!linux_x64.contains("sudo mv zig-linux"));
}

#[test]
fn parallel_nuc_workers_do_not_share_mutable_rust_toolchains() {
    let source = workflow("build.yml");

    for name in ["lint", "test", "build-wasm", "build-linux-x64"] {
        let body = job(&source, name);
        assert!(body.contains("CARGO_HOME: ${{ runner.tool_cache }}/uhc/${{ runner.name }}/cargo"));
        assert!(
            body.contains("RUSTUP_HOME: ${{ runner.tool_cache }}/uhc/${{ runner.name }}/rustup")
        );
    }
}
