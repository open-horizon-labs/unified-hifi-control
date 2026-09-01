//! Public security-documentation contract for the HiPhi connector.

use std::fs;
use std::path::PathBuf;

fn repo_file(path: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(path);
    fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
}

#[test]
fn public_threat_model_states_boundaries_invariants_and_residual_risks() {
    let threat_model = repo_file("docs/hiphi-cloud-threat-model.md");

    for required in [
        "## Security goals",
        "## Trust boundaries",
        "## Data that crosses the boundary",
        "## Security invariants",
        "## Residual risks and non-goals",
        "open source",
        "command-signing authority",
        "source IP",
        "now-playing",
        "provider credentials",
        "arbitrary URL",
        "outbound",
        "does not mean that the cloud service is trustless",
        "cloud implementation is not made auditable by publishing UHC",
    ] {
        assert!(
            threat_model.contains(required),
            "public threat model must state {required:?}"
        );
    }
}

#[test]
fn relay_boundary_adr_links_the_public_threat_model() {
    let adr = repo_file("docs/adr/005-cloud-relay-zero-trust-boundary.md");
    assert!(adr.contains("../hiphi-cloud-threat-model.md"));
}
