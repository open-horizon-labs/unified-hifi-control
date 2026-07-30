//! Dependency lint for the adaptive-control contract (issue #323).
//!
//! `src/adaptive/` is the contract layer. Two properties depend on it staying free of
//! transport and state dependencies, and neither is visible from a passing build:
//!
//! 1. **The module is shared, not `#[cfg(feature = "server")]`.** Web/WASM consumers and
//!    the MCP server use the same types, so a server-only dependency here breaks the
//!    `dx build --platform web` bundle in CI rather than anything a host `cargo test`
//!    would notice.
//! 2. **The aggregator keeps ownership of state.** The contract describes producer state;
//!    it must not be able to reach an adapter, the aggregator or the bus to fetch any.
//!    A contract type that can call an adapter is how surfaces start bypassing the
//!    aggregator (see `docs/ARCHITECTURE.md` and `tests/architecture_lint.rs`).
//!
//! It also keeps the deferred extraction into a standalone `uhc-adaptive-contract` crate
//! (Option D in `.oh/adaptive-producer-contract.md`) mechanical rather than a rewrite.

use std::fs;
use std::path::Path;
use walkdir::WalkDir;

/// Crates and modules the contract layer must never reference, with the reason.
const FORBIDDEN: &[(&str, &str)] = &[
    // Transport and runtime: would make the module server-only.
    ("axum", "contract types must not depend on the HTTP layer"),
    ("tokio", "contract types must be runtime-agnostic"),
    ("reqwest", "contract types must not perform I/O"),
    ("hyper", "contract types must not depend on the HTTP layer"),
    ("dioxus", "contract types must not depend on the UI framework"),
    ("rust_mcp_sdk", "contract types must not depend on the MCP server"),
    ("quick_xml", "backend wire formats must not leak into the contract"),
    ("roon_api", "backend clients must not leak into the contract"),
    // State ownership: the aggregator owns state; the contract only describes it.
    (
        "crate::adapters",
        "the contract must not reach an adapter - adapters map onto it (#325)",
    ),
    (
        "crate::aggregator",
        "the aggregator owns state and publishes documents (#324); the contract must not depend on it",
    ),
    (
        "crate::bus",
        "bus events carry documents (#324); the contract must not depend on the bus",
    ),
    (
        "crate::api",
        "route handlers project documents; the contract must not depend on them",
    ),
    ("crate::mcp", "the MCP server is a consumer, not a dependency"),
    ("crate::app", "the web UI is a consumer, not a dependency"),
    ("crate::config", "the contract must not read configuration"),
    // Ambient state, which would make a document non-deterministic.
    (
        "std::time::SystemTime",
        "timestamps are supplied by the producer, not read from the clock",
    ),
    (
        "std::fs",
        "the contract must not touch the filesystem",
    ),
    (
        "std::net",
        "the contract must not touch the network",
    ),
];

/// Feature gates that would make the module server-only.
const FORBIDDEN_GATES: &[&str] = &["feature = \"server\"", "feature = \"web\""];

fn adaptive_sources() -> Vec<(String, String)> {
    let root = Path::new("src/adaptive");
    assert!(
        root.is_dir(),
        "src/adaptive must exist - it is the contract layer for #323"
    );
    let mut sources = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if path.extension().is_some_and(|extension| extension == "rs") {
            let text = fs::read_to_string(path)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", path.display()));
            sources.push((path.display().to_string(), text));
        }
    }
    assert!(
        sources.len() >= 5,
        "expected the contract layer to be split across modules, found {}",
        sources.len()
    );
    sources
}

/// Strip line comments and doc comments so prose mentioning a crate is not a violation.
///
/// The doc comments in this module deliberately discuss axum, the aggregator and the bus
/// in order to explain the boundary. Linting the prose would punish documenting it.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with("//") && !trimmed.starts_with("*")
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn lint_contract_layer_has_no_transport_or_state_dependencies() {
    let mut violations = Vec::new();
    for (path, text) in adaptive_sources() {
        let code = code_only(&text);
        for (needle, reason) in FORBIDDEN {
            if code.contains(needle) {
                violations.push(format!("{path}: references `{needle}` - {reason}"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "src/adaptive must depend only on serde, serde_json and std:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_contract_layer_is_not_feature_gated() {
    // If this module became server-only, the web UI and firmware-facing projections
    // would each need their own copy of the wire types - the duplication #312 exists to
    // remove.
    let mut violations = Vec::new();
    for (path, text) in adaptive_sources() {
        let code = code_only(&text);
        for gate in FORBIDDEN_GATES {
            if code.contains(gate) {
                violations.push(format!("{path}: gated on `{gate}`"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the contract layer must be shared across server and web builds:\n{}",
        violations.join("\n")
    );

    let lib = fs::read_to_string("src/lib.rs").expect("src/lib.rs readable");
    let declaration = lib
        .find("pub mod adaptive;")
        .expect("src/lib.rs must declare `pub mod adaptive;`");
    let preceding = &lib[..declaration];
    let last_line = preceding.lines().last().unwrap_or_default().trim();
    assert!(
        !last_line.starts_with("#[cfg("),
        "`pub mod adaptive;` must not be preceded by a cfg attribute, found: {last_line}"
    );
}

#[test]
fn lint_contract_layer_uses_no_panicking_paths() {
    // The library denies unwrap/expect/panic (src/lib.rs). A contract type that can
    // panic on malformed input would turn a compatibility problem into a crash in a
    // consumer, so this is checked at the module level too.
    let forbidden = ["unwrap()", "expect(", "panic!", "todo!", "unimplemented!"];
    let mut violations = Vec::new();
    for (path, text) in adaptive_sources() {
        let code = code_only(&text);
        for needle in forbidden {
            if code.contains(needle) {
                violations.push(format!("{path}: uses `{needle}`"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the contract layer must degrade rather than panic:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_public_api_route_contract_is_untouched() {
    // #323 defines a contract; it adds no routes. This guards against a later commit on
    // this branch quietly editing the route contract, which requires explicit maintainer
    // approval and an `api-change-approved` label that must never be self-applied.
    let routes = fs::read_to_string("tests/fixtures/api_routes.txt")
        .expect("tests/fixtures/api_routes.txt readable");
    for forbidden in [
        "/adaptive",
        "/producer",
        "/producers",
        "/change_set",
        "/changeset",
    ] {
        assert!(
            !routes.contains(forbidden),
            "tests/fixtures/api_routes.txt contains `{forbidden}`: #323 must not add routes. \
             Any HTTP/SSE exposure of the producer document belongs to #324 and needs \
             explicit approval."
        );
    }
}
