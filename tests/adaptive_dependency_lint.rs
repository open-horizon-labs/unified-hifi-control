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

/// Feature gates that would make the module server-only, in their readable spelling.
///
/// Matched after whitespace normalization, so `feature="server"` is caught too. The
/// readable form is kept because it is what a failure message prints.
const FORBIDDEN_GATES: &[&str] = &["feature = \"server\"", "feature = \"web\""];

/// The declaration `src/lib.rs` must carry, ungated.
const ADAPTIVE_DECLARATION: &str = "pub mod adaptive;";

/// Drop every whitespace character.
///
/// `#[cfg(feature="server")]` and `#[cfg(feature = "server")]` are the same gate, and a
/// lint that only matches one spelling passes on a gated module - the exact failure it
/// exists to catch.
fn without_whitespace(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// The forbidden feature gates present in `code`, in readable form.
fn forbidden_gates_in(code: &str) -> Vec<&'static str> {
    let normalized = without_whitespace(code);
    FORBIDDEN_GATES
        .iter()
        .copied()
        .filter(|gate| normalized.contains(&without_whitespace(gate)))
        .collect()
}

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

/// Strip comments so prose mentioning a crate is not a violation, keeping code intact.
///
/// The doc comments in `src/adaptive/` deliberately discuss axum, the aggregator and the
/// bus in order to explain the boundary. Linting the prose would punish documenting it -
/// and that applies just as much to a trailing `// not tokio` or a `/* … */` block as to a
/// whole-line comment, so all three are stripped.
///
/// String literals are *kept*: a forbidden path written as a string is still a reference,
/// and a `//` inside one (a URL, say) must not truncate the line. Newlines are preserved
/// so callers can still reason line by line.
fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut escaped = false;
    // Rust block comments nest, so this is a depth rather than a flag.
    let mut block_depth = 0usize;

    while let Some(current) = chars.next() {
        if block_depth > 0 {
            if current == '*' && chars.peek() == Some(&'/') {
                chars.next();
                block_depth -= 1;
            } else if current == '/' && chars.peek() == Some(&'*') {
                chars.next();
                block_depth += 1;
            } else if current == '\n' {
                out.push('\n');
            }
            continue;
        }
        if in_string {
            out.push(current);
            if escaped {
                escaped = false;
            } else if current == '\\' {
                escaped = true;
            } else if current == '"' {
                in_string = false;
            }
            continue;
        }
        match current {
            '"' => {
                in_string = true;
                out.push(current);
            }
            '/' if chars.peek() == Some(&'/') => {
                for skipped in chars.by_ref() {
                    if skipped == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                block_depth = 1;
            }
            other => out.push(other),
        }
    }
    out
}

/// Every `#[cfg(...)]` attribute that applies to `pub mod adaptive;` in `source`.
///
/// `None` means the declaration is absent. An empty vector means it is present and ungated.
///
/// Scans for the declaration line rather than the first textual occurrence, because
/// `str::find` would happily match a doc comment above the real site and then assert about
/// the wrong line. Brace depth is tracked so a gate on an *enclosing* `mod` block counts
/// too: wrapping the declaration in `#[cfg(feature = "server")] mod inner { … }` makes the
/// contract layer server-only just as surely as gating it directly.
fn adaptive_declaration_gates(source: &str) -> Option<Vec<String>> {
    let mut enclosing: Vec<Option<String>> = Vec::new();
    let mut pending: Option<String> = None;
    let mut gates: Option<Vec<String>> = None;

    for line in code_only(source).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(prefix) = trimmed.strip_suffix(ADAPTIVE_DECLARATION) {
            let mut found = Vec::new();
            if prefix.contains("#[cfg(") {
                found.push(prefix.trim().to_string());
            }
            found.extend(pending.take());
            found.extend(enclosing.iter().flatten().cloned());
            gates = Some(found);
            continue;
        }
        if trimmed.starts_with("#[cfg(") {
            pending = Some(trimmed.to_string());
            continue;
        }
        if trimmed.ends_with('{') {
            enclosing.push(pending.take());
            continue;
        }
        if trimmed == "}" {
            enclosing.pop();
            continue;
        }
        pending = None;
    }
    gates
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
        for gate in forbidden_gates_in(&code_only(&text)) {
            violations.push(format!("{path}: gated on `{gate}`"));
        }
    }
    assert!(
        violations.is_empty(),
        "the contract layer must be shared across server and web builds:\n{}",
        violations.join("\n")
    );

    let lib = fs::read_to_string("src/lib.rs").expect("src/lib.rs readable");
    let gates = adaptive_declaration_gates(&lib)
        .unwrap_or_else(|| panic!("src/lib.rs must declare `{ADAPTIVE_DECLARATION}`"));
    assert!(
        gates.is_empty(),
        "`{ADAPTIVE_DECLARATION}` must not be cfg-gated, found: {gates:?}"
    );
}

#[test]
fn gate_detection_survives_whitespace_and_the_declaration_scan_finds_the_real_site() {
    // These three cases are the lint's own regression cover. Each was verified to slip past
    // the previous implementation before this test existed.
    for spelling in [
        "#[cfg(feature = \"server\")]",
        "#[cfg(feature=\"server\")]",
        "#[cfg(feature\t=\t\"web\")]",
    ] {
        assert_eq!(
            forbidden_gates_in(spelling).len(),
            1,
            "{spelling} must be detected as a forbidden gate"
        );
    }
    assert!(
        forbidden_gates_in("#[cfg(test)]").is_empty(),
        "an unrelated gate must not be a violation"
    );

    // A doc comment mentioning the declaration must not become the assertion target.
    let hidden = "/// Declared as `pub mod adaptive;` without a gate.\n\
                  #[cfg(feature = \"server\")]\n\
                  pub mod adaptive;\n";
    assert_eq!(
        adaptive_declaration_gates(hidden),
        Some(vec!["#[cfg(feature = \"server\")]".to_string()]),
        "a gate on the real declaration must be found even when prose mentions it first"
    );

    let clean = "//! Mentions `pub mod adaptive;` in prose.\n\
                 // Deliberately not feature-gated.\n\
                 pub mod adaptive;\n";
    assert_eq!(adaptive_declaration_gates(clean), Some(Vec::new()));

    // A gate on an enclosing module block makes the contract layer server-only too.
    let enclosed = "#[cfg(feature = \"web\")]\nmod inner {\n    pub mod adaptive;\n}\n";
    assert_eq!(
        adaptive_declaration_gates(enclosed),
        Some(vec!["#[cfg(feature = \"web\")]".to_string()])
    );

    // And a same-line gate is still a gate.
    assert_eq!(
        adaptive_declaration_gates("#[cfg(feature = \"server\")] pub mod adaptive;\n"),
        Some(vec!["#[cfg(feature = \"server\")]".to_string()])
    );

    assert_eq!(adaptive_declaration_gates("pub mod other;\n"), None);
}

#[test]
fn code_only_strips_trailing_and_block_comments_without_touching_strings() {
    // Documenting the boundary inline must not be a lint failure - that is the whole reason
    // comments are stripped, and the previous implementation only dropped whole lines.
    let source = "use std::fmt; // deliberately not tokio\n\
                  /* and never axum,\n\
                     nor /* nested */ dioxus */\n\
                  let route = \"https://example.invalid/reqwest\"; // nor hyper\n\
                  let last = 3; /* crate::bus */ let kept = 4;\n";
    let code = code_only(source);
    for absent in ["tokio", "axum", "dioxus", "hyper", "crate::bus"] {
        assert!(
            !code.contains(absent),
            "`{absent}` survived comment stripping:\n{code}"
        );
    }
    assert!(
        code.contains("https://example.invalid/reqwest"),
        "a `//` inside a string literal must not truncate the line:\n{code}"
    );
    assert!(
        code.contains("let kept = 4;"),
        "code after a closed block comment must survive:\n{code}"
    );
    assert_eq!(
        code.lines().count(),
        source.lines().count(),
        "line structure must be preserved so callers can scan line by line"
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
