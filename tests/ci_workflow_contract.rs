use std::{fs, path::PathBuf};

const ALPHA: &str = "integration/streaming-ha-alpha";

fn workflow(name: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(".github/workflows")
        .join(name);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()))
}

#[test]
fn alpha_pull_requests_run_rust_and_api_contract_checks() {
    for name in ["build.yml", "api-guard.yml"] {
        let source = workflow(name);
        let pull_request = source
            .split("pull_request:")
            .nth(1)
            .unwrap_or_else(|| panic!("{name} has no pull_request trigger"));
        let trigger = pull_request.split("jobs:").next().unwrap_or(pull_request);
        assert!(
            trigger.contains(ALPHA),
            "{name} must run for pull requests targeting {ALPHA}"
        );
    }
}

#[test]
fn alpha_home_assistant_changes_run_the_ha_workflow() {
    let source = workflow("ha-integration.yml");
    assert_eq!(
        source.matches(ALPHA).count(),
        2,
        "HA integration must cover alpha push and pull-request triggers"
    );
}

#[test]
fn alpha_does_not_enable_edge_image_publication() {
    let source = workflow("docker.yml");
    assert!(
        !source.contains(ALPHA),
        "alpha work must not enter the Docker edge publication workflow"
    );
}
