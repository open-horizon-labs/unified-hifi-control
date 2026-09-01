use std::{fs, path::PathBuf};

const DEVELOPMENT_BRANCH: &str = "v4";

fn workflow(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
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
