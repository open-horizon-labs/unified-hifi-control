//! Boundary lints for adaptive producer publication (issue #324).
//!
//! #324 publishes producer documents to **in-repository consumers only**. Two boundaries
//! keep that true, and neither is visible from a passing build:
//!
//! 1. **The public bus cannot carry adaptive data.** `src/api/mod.rs` serializes every
//!    [`BusEvent`] verbatim into `GET /events`, which `docs/ARCHITECTURE.md` documents as
//!    consumable by any HTTP client including ESP32 firmware. A variant carrying a producer
//!    document would be a response-schema change to a public endpoint *and* publication of
//!    the v1 contract outside this repository.
//! 2. **No surface reaches the contract or the publication layer.** The one deliberate code
//!    path that would expose it is a handler serializing a snapshot, and
//!    `ProducerDocument` derives `Serialize`, so that is a one-line accident away.
//!
//! ## Why this file carries probes
//!
//! `tests/adaptive_dependency_lint.rs` has been remediated twice for blind spots that made
//! it *report success on a violation* — `1577948` (three) and `d6da952` (a fourth, where a
//! stacked `#[allow]` between the gate and the declaration discarded the gate). Every one
//! failed in the direction that passes. A lint whose non-vacuity was never demonstrated
//! should be treated as absent, so each scanner here is probed in **both** directions.
//!
//! The polarity matters and is the opposite of `pub mod adaptive;`. That declaration must be
//! **ungated**, so a scanner that misses a gate fails safe. `pub mod producers;` must be
//! **gated**, so a scanner that *hallucinates* a gate passes wrongly — which is why the
//! probes below include "delete the gate, the lint must fail".

use std::fs;
use std::path::Path;
use walkdir::WalkDir;

const PRODUCERS_DECLARATION: &str = "pub mod producers;";
/// The readable spelling, used in failure messages.
const SERVER_GATE: &str = "#[cfg(feature = \"server\")]";

/// Whether an extracted attribute is the server gate, whitespace notwithstanding.
///
/// `#[cfg(feature="server")]` is the same gate as `#[cfg(feature = "server")]`. A substring
/// match against one spelling silently accepts the other, which is the blind spot
/// `1577948` fixed in `tests/adaptive_dependency_lint.rs` — and which
/// `gate_scanner_detects_a_gate_it_must_not_miss` caught in this file before it shipped.
fn is_server_gate(attribute: &str) -> bool {
    let squeezed: String = attribute.chars().filter(|c| !c.is_whitespace()).collect();
    squeezed.contains("feature=\"server\"")
}

// =============================================================================
// Source scanning
// =============================================================================

/// Strip comments while keeping string literals.
///
/// Prose must not be linted: this file, `src/producers/mod.rs` and `src/bus/events.rs` all
/// discuss the boundary in doc comments, and punishing that would push the explanation out
/// of the code. String literals are kept so a forbidden path written as a string is still
/// caught.
fn code_only(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    let mut block_depth = 0usize;

    while let Some(c) = chars.next() {
        if block_depth > 0 {
            if c == '*' && chars.peek() == Some(&'/') {
                chars.next();
                block_depth -= 1;
            } else if c == '/' && chars.peek() == Some(&'*') {
                chars.next();
                block_depth += 1;
            } else if c == '\n' {
                out.push('\n');
            }
            continue;
        }
        if in_string {
            out.push(c);
            if c == '\\' {
                if let Some(escaped) = chars.next() {
                    out.push(escaped);
                }
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_string = true;
                out.push(c);
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
                block_depth += 1;
            }
            _ => out.push(c),
        }
    }
    out
}

fn sources_under(root: &str) -> Vec<(String, String)> {
    let path = Path::new(root);
    if !path.exists() {
        return Vec::new();
    }
    let mut sources = Vec::new();
    for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
        let file = entry.path();
        if file.extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(file)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", file.display()));
            sources.push((file.display().to_string(), text));
        }
    }
    sources
}

/// Every `#[cfg(...)]` attribute written in `text`.
///
/// Extracted rather than matched whole-line, because a gate may share a line with the item
/// it gates. `#[cfg_attr(...)]` must not match: it cannot exclude a module.
fn cfg_attributes_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("#[cfg(") {
        let tail = &rest[start..];
        match tail.find(")]") {
            Some(end) => {
                found.push(tail[..end + 2].to_string());
                rest = &tail[end + 2..];
            }
            None => {
                found.push(tail.trim().to_string());
                break;
            }
        }
    }
    found
}

/// Every `#[cfg(...)]` that applies to `declaration` in `source`.
///
/// `None` means the declaration is absent; an empty vector means it is present and ungated.
///
/// Pending attributes **accumulate** across consecutive attribute lines, because Rust allows
/// any number in any order — `#[cfg(...)]` / `#[allow(...)]` / `pub mod x;` is gated. The
/// accumulation stops at whatever consumes it (the declaration, a block opener, or the next
/// item), or every ungated `pub mod` below a gated one would be reported as gated.
fn declaration_gates(source: &str, declaration: &str) -> Option<Vec<String>> {
    let mut enclosing: Vec<Vec<String>> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut gates: Option<Vec<String>> = None;

    for line in code_only(source).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(prefix) = trimmed.strip_suffix(declaration) {
            let mut found = cfg_attributes_in(prefix);
            found.append(&mut pending);
            found.extend(enclosing.iter().flatten().cloned());
            gates = Some(found);
            continue;
        }
        if trimmed.ends_with('{') {
            let mut frame = cfg_attributes_in(trimmed);
            frame.append(&mut pending);
            enclosing.push(frame);
            continue;
        }
        if trimmed == "}" {
            enclosing.pop();
            continue;
        }
        if trimmed.starts_with('#') {
            pending.extend(cfg_attributes_in(trimmed));
            continue;
        }
        pending.clear();
    }
    gates
}

// =============================================================================
// Boundary 1: the public bus cannot carry adaptive data
// =============================================================================

#[test]
fn lint_public_bus_cannot_carry_adaptive_types() {
    // `BusEvent` is a wire payload, not merely an internal enum: `src/api/mod.rs`
    // serializes it verbatim into `GET /events`. A variant naming an adaptive type would
    // change that endpoint's response schema and publish the v1 contract outside this
    // repository, both of which #324 is scoped not to do.
    let mut violations = Vec::new();
    for (path, text) in sources_under("src/bus") {
        let code = code_only(&text);
        for needle in ["adaptive", "producers", "ProducerDocument", "AdaptiveEvent"] {
            if code.contains(needle) {
                violations.push(format!("{path}: references `{needle}`"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the public bus must not carry adaptive data - `GET /events` serializes every \
         BusEvent verbatim, so a variant here is a public response-schema change:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_the_sse_projection_does_not_mention_the_publication_layer() {
    let api = fs::read_to_string("src/api/mod.rs").expect("src/api/mod.rs readable");
    let code = code_only(&api);
    for needle in ["crate::producers", "crate::adaptive"] {
        assert!(
            !code.contains(needle),
            "src/api/mod.rs references `{needle}`. Any HTTP or SSE exposure of the producer \
             document needs explicit API approval and an `api-change-approved` label that \
             must never be self-applied."
        );
    }
}

// =============================================================================
// Boundary 2: no surface reaches the contract or the publication layer
// =============================================================================

#[test]
fn lint_surfaces_do_not_import_the_contract_or_the_publication_layer() {
    // `ProducerDocument` derives `Serialize`, so a single `Json(snapshot)` in a handler
    // re-exports the whole contract. `src/main.rs` is deliberately not listed: it is the
    // composition root and must name both to wire them together.
    const SURFACES: &[&str] = &[
        "src/api",
        "src/mcp",
        "src/app",
        "src/knobs",
        "src/devices",
        "src/components",
    ];
    let mut violations = Vec::new();
    for surface in SURFACES {
        for (path, text) in sources_under(surface) {
            let code = code_only(&text);
            for needle in [
                "crate::adaptive",
                "crate::producers",
                "unified_hifi_control::adaptive",
                "unified_hifi_control::producers",
            ] {
                if code.contains(needle) {
                    violations.push(format!("{path}: references `{needle}`"));
                }
            }
        }
    }
    assert!(
        violations.is_empty(),
        "#324 publishes to in-repository consumers only. A surface reading the producer \
         document is #326's decision and needs its own API approval:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_adaptive_event_derives_no_serialization() {
    // Belt to the braces above: even if a variant were added to the wrong enum, the
    // internal event type itself cannot be written to a wire.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let code = code_only(&event);
    let derive = code
        .split("pub enum AdaptiveEvent")
        .next()
        .and_then(|before| before.rfind("#[derive(").map(|at| before[at..].to_string()))
        .expect("AdaptiveEvent must carry a derive attribute");
    for forbidden in ["Serialize", "Deserialize"] {
        assert!(
            !derive.contains(forbidden),
            "AdaptiveEvent derives `{forbidden}`. The internal bus exists precisely so that \
             producer lifecycle cannot reach a wire; deriving serialization removes the \
             only structural part of that guarantee. Found: {derive}"
        );
    }
}

#[test]
fn lint_publication_layer_adds_no_routes() {
    let routes = fs::read_to_string("tests/fixtures/api_routes.txt")
        .expect("tests/fixtures/api_routes.txt readable");
    for forbidden in [
        "/adaptive",
        "/producer",
        "/producers",
        "/change_set",
        "/changeset",
        "/snapshot",
    ] {
        assert!(
            !routes.contains(forbidden),
            "tests/fixtures/api_routes.txt contains `{forbidden}`: #324 adds no routes. \
             Any HTTP/SSE exposure needs explicit approval, and `api-change-approved` must \
             never be self-applied."
        );
    }
}

// =============================================================================
// The publication layer is server-only
// =============================================================================

#[test]
fn lint_producers_module_is_server_gated() {
    // Opposite polarity to `pub mod adaptive;`, which must be *ungated* so the WASM build
    // can use the contract types. `src/producers/` depends on `crate::bus` and `tokio`, so
    // an ungated declaration breaks `dx build --platform web` in CI rather than anything a
    // host `cargo test` would notice.
    let lib = fs::read_to_string("src/lib.rs").expect("src/lib.rs readable");
    let gates = declaration_gates(&lib, PRODUCERS_DECLARATION)
        .unwrap_or_else(|| panic!("src/lib.rs must declare `{PRODUCERS_DECLARATION}`"));
    assert!(
        gates.iter().any(|gate| is_server_gate(gate)),
        "`{PRODUCERS_DECLARATION}` must be `{SERVER_GATE}`; it depends on crate::bus and \
         tokio, neither of which exists in the WASM build. Found gates: {gates:?}"
    );
}

#[test]
fn lint_publication_layer_is_reachable_only_from_the_composition_root() {
    // If nothing outside `src/producers/` names it except `src/lib.rs` and `src/main.rs`,
    // then the exposure surface is exactly one file and reviewing it is tractable.
    let mut referrers = Vec::new();
    for (path, text) in sources_under("src") {
        if path.starts_with("src/producers") || path == "src/lib.rs" || path == "src/main.rs" {
            continue;
        }
        if code_only(&text).contains("crate::producers") {
            referrers.push(path);
        }
    }
    assert!(
        referrers.is_empty(),
        "only the composition root may reference the publication layer, found: {referrers:?}"
    );
}

// =============================================================================
// Non-vacuity probes
// =============================================================================

#[test]
fn gate_scanner_detects_a_gate_it_must_not_miss() {
    // Each case was verified against the shapes that defeated earlier versions of the
    // sibling lint in `tests/adaptive_dependency_lint.rs`.
    for source in [
        // Directly above.
        "#[cfg(feature = \"server\")]\npub mod producers;\n",
        // Stacked behind another attribute - the `d6da952` blind spot.
        "#[cfg(feature = \"server\")]\n#[allow(dead_code)]\npub mod producers;\n",
        // Other order.
        "#[allow(dead_code)]\n#[cfg(feature = \"server\")]\npub mod producers;\n",
        // Unspaced.
        "#[cfg(feature=\"server\")]\npub mod producers;\n",
        // Sharing the line.
        "#[cfg(feature = \"server\")] pub mod producers;\n",
        // Behind a doc comment that names the declaration, so a `find`-based scan would
        // inspect the wrong line.
        "/// see `pub mod producers;`\n#[cfg(feature = \"server\")]\npub mod producers;\n",
        // Gated by an enclosing block.
        "#[cfg(feature = \"server\")]\nmod inner {\npub mod producers;\n}\n",
    ] {
        let gates = declaration_gates(source, PRODUCERS_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        assert!(
            gates.iter().any(|gate| is_server_gate(gate)),
            "a server gate was missed in:\n{source}\nfound: {gates:?}"
        );
    }
}

#[test]
fn gate_scanner_does_not_invent_a_gate_that_is_not_there() {
    // The direction that matters for this module's polarity. A scanner that hallucinates a
    // gate would let an ungated `pub mod producers;` pass and break the WASM build in CI.
    for source in [
        // No gate at all.
        "pub mod producers;\n",
        // A gate that belongs to the item above it, not to this declaration.
        "#[cfg(feature = \"server\")]\npub mod bus;\npub mod producers;\n",
        // A gate consumed by a block that has already closed.
        "#[cfg(feature = \"server\")]\nmod inner {\npub mod other;\n}\npub mod producers;\n",
        // `cfg_attr` cannot exclude a module.
        "#[cfg_attr(feature = \"server\", allow(dead_code))]\npub mod producers;\n",
        // A different feature is not the server gate.
        "#[cfg(feature = \"web\")]\npub mod producers;\n",
    ] {
        let gates = declaration_gates(source, PRODUCERS_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        assert!(
            !gates.iter().any(|gate| is_server_gate(gate)),
            "a server gate was invented for:\n{source}\nfound: {gates:?}"
        );
    }
}

#[test]
fn declaration_scanner_reports_absence_rather_than_guessing() {
    assert_eq!(
        declaration_gates("pub mod adaptive;\n", PRODUCERS_DECLARATION),
        None
    );
    // A doc comment mentioning it is not a declaration.
    assert_eq!(
        declaration_gates("/// pub mod producers;\n", PRODUCERS_DECLARATION),
        None
    );
}

#[test]
fn comment_stripper_keeps_strings_and_drops_prose() {
    // Prose discussing the boundary must not trip the dependency checks...
    let prose = "// this must not reference crate::adaptive\nlet x = 1;\n";
    assert!(!code_only(prose).contains("crate::adaptive"));

    let trailing = "let x = 1; // not crate::producers\n";
    assert!(!code_only(trailing).contains("crate::producers"));

    let block = "/* crate::adaptive /* nested */ still comment */\nlet x = 1;\n";
    assert!(!code_only(block).contains("crate::adaptive"));

    // ...but a forbidden path written as a string literal is still a reference.
    let literal = "let path = \"crate::producers\";\n";
    assert!(code_only(literal).contains("crate::producers"));

    // A `//` inside a string must not truncate the line.
    let url = "let u = \"https://example.com\"; let y = crate::producers::X;\n";
    assert!(code_only(url).contains("crate::producers"));
}
