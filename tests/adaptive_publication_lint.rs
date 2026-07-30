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
const ADAPTIVE_EVENT_DECLARATION: &str = "pub enum AdaptiveEvent {";
/// The readable spelling, used in failure messages.
const SERVER_GATE: &str = "#[cfg(feature = \"server\")]";

/// Whether an extracted attribute gates its item on the `server` feature and nothing looser.
///
/// Two ways to get this wrong, both of which accept a gate that is not server-only:
///
/// 1. **Whitespace.** `#[cfg(feature="server")]` is the same gate as
///    `#[cfg(feature = "server")]`; matching one spelling silently accepts the other. That
///    is the blind spot `1577948` fixed in `tests/adaptive_dependency_lint.rs`, and
///    `gate_scanner_detects_a_gate_it_must_not_miss` caught it here before it shipped.
/// 2. **Disjunction.** `#[cfg(any(feature = "server", feature = "web"))]` *contains*
///    `feature="server"` while compiling the module for `web` **without** `server` — which
///    is exactly the WASM breakage this lint exists to prevent, admitted by the lint that
///    was supposed to prevent it. Found by CodeRabbit at `86e5bd2`.
///
/// So: the normalized attribute must mention the server feature and must contain neither
/// `any(` nor `not(`. A stricter conjunctive gate (`all(feature = "server", unix)`) is still
/// server-only and is accepted; loosening it later has to be deliberate, because the lint
/// fails until someone edits this function.
fn is_server_gate(attribute: &str) -> bool {
    let squeezed: String = attribute.chars().filter(|c| !c.is_whitespace()).collect();
    squeezed.contains("feature=\"server\"")
        && !squeezed.contains("any(")
        && !squeezed.contains("not(")
}

/// The module paths a source file reaches, including through grouped `use` trees.
///
/// A needle scan for `crate::adaptive` misses `use crate::{adaptive, producers};`, which
/// names neither literal. That is not hypothetical: `src/main.rs` imports this repository's
/// modules in exactly that style, so a surface adopting the house style would have slipped
/// straight past the boundary check. Found by CodeRabbit at `86e5bd2`.
fn forbidden_module_references(code: &str) -> Vec<String> {
    const ROOTS: &[&str] = &["crate", "unified_hifi_control"];
    const FORBIDDEN: &[&str] = &["adaptive", "producers"];
    let mut found = Vec::new();

    // Fully-spelled paths, anywhere - `use`, a turbofish, a type position.
    for root in ROOTS {
        for module in FORBIDDEN {
            let needle = format!("{root}::{module}");
            if code.contains(&needle) {
                found.push(needle);
            }
        }
    }

    // Grouped use trees: `use <root>::{ a, b::c, d as e };`
    //
    // Whitespace is collapsed rather than removed. Removing it entirely turns
    // `adaptive as contract` into `adaptiveascontract`, so the alias can no longer be split
    // off and the item stops matching - a probe caught exactly that here.
    let normalized = collapse_whitespace(code);
    for root in ROOTS {
        let opener = format!("use {root}::{{");
        let mut rest = normalized.as_str();
        while let Some(at) = rest.find(&opener) {
            let after = &rest[at + opener.len()..];
            let mut depth = 1usize;
            let mut end = after.len();
            for (index, c) in after.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = index;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            for item in split_top_level(&after[..end]) {
                let head = item
                    .split("::")
                    .next()
                    .unwrap_or_default()
                    .split(" as ")
                    .next()
                    .unwrap_or_default();
                if FORBIDDEN.contains(&head) {
                    found.push(format!("{root}::{{… {head} …}}"));
                }
            }
            rest = &after[end.min(after.len())..];
        }
    }

    found.sort();
    found.dedup();
    found
}

/// Collapse whitespace runs to one space, then close it up around path punctuation.
///
/// Leaves ` as ` intact — which is the whole reason this is not a plain whitespace strip.
fn collapse_whitespace(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut last_was_space = false;
    for c in code.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    for token in ["::", "{", "}", ",", ";"] {
        out = out.replace(&format!(" {token}"), token);
        out = out.replace(&format!("{token} "), token);
    }
    out
}

/// Split a brace group on commas that are not inside a nested group.
fn split_top_level(group: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, c) in group.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&group[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < group.len() {
        items.push(&group[start..]);
    }
    items
        .into_iter()
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .collect()
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

/// Every `#[...]` attribute written in `text`, whole.
///
/// Bracket-depth aware and string-literal aware, so `#[cfg_attr(feature = "server",
/// derive(Serialize))]` and `#[doc = "contains a ] bracket"]` are each captured entire
/// rather than truncated at the first `]`.
fn attributes_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let bytes: Vec<char> = text.chars().collect();
    let mut index = 0usize;
    while index + 1 < bytes.len() {
        if bytes[index] != '#' || (bytes[index + 1] != '[' && bytes[index + 1] != '!') {
            index += 1;
            continue;
        }
        // `#![...]` is an inner attribute; it applies to the enclosing module, not to the
        // next item, so it is skipped rather than attributed to a declaration below it.
        let open = if bytes[index + 1] == '!' {
            index + 2
        } else {
            index + 1
        };
        if open >= bytes.len() || bytes[open] != '[' {
            index += 1;
            continue;
        }
        let inner = bytes[index + 1] == '!';
        let mut depth = 0usize;
        let mut in_string = false;
        let mut cursor = open;
        let mut end = None;
        while cursor < bytes.len() {
            let c = bytes[cursor];
            if in_string {
                if c == '\\' {
                    cursor += 2;
                    continue;
                }
                if c == '"' {
                    in_string = false;
                }
            } else {
                match c {
                    '"' => in_string = true,
                    '[' => depth += 1,
                    ']' => {
                        depth -= 1;
                        if depth == 0 {
                            end = Some(cursor);
                            break;
                        }
                    }
                    _ => {}
                }
            }
            cursor += 1;
        }
        match end {
            Some(close) => {
                if !inner {
                    found.push(bytes[index..=close].iter().collect());
                }
                index = close + 1;
            }
            // Unterminated. Record what is there rather than dropping an attribute.
            None => {
                found.push(bytes[index..].iter().collect());
                break;
            }
        }
    }
    found
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

/// Every attribute attached to `declaration` in `source`, including those on an enclosing
/// block.
///
/// `None` means the declaration is absent; an empty vector means it carries no attributes.
///
/// Same accumulation rule as [`declaration_gates`] — Rust allows any number of attributes in
/// any order, so they gather across consecutive attribute lines and are consumed by whatever
/// comes next. Inspecting only the *nearest* attribute is the blind spot this replaced: an
/// earlier `#[derive(Serialize)]` sitting above a later `#[derive(Debug, Clone)]` was
/// invisible.
fn attributes_attached_to(source: &str, declaration: &str) -> Option<Vec<String>> {
    let mut enclosing: Vec<Vec<String>> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut attached: Option<Vec<String>> = None;

    for line in code_only(source).lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(prefix) = trimmed.strip_suffix(declaration) {
            let mut found = attributes_in(prefix);
            found.append(&mut pending);
            found.extend(enclosing.iter().flatten().cloned());
            attached = Some(found);
            continue;
        }
        if trimmed.ends_with('{') {
            let mut frame = attributes_in(trimmed);
            frame.append(&mut pending);
            enclosing.push(frame);
            continue;
        }
        if trimmed == "}" {
            enclosing.pop();
            continue;
        }
        if trimmed.starts_with('#') {
            pending.extend(attributes_in(trimmed));
            continue;
        }
        pending.clear();
    }
    attached
}

/// Whether an attribute would make its item serializable.
///
/// Covers `#[derive(Serialize)]` and `#[cfg_attr(…, derive(Serialize))]` alike: the question
/// is whether the attribute introduces a serialization derive under *any* configuration, not
/// which spelling it used. A `cfg_attr` gate does not make it safe — it makes it conditional,
/// and the condition is somebody else's to set.
fn introduces_serialization(attribute: &str) -> Option<&'static str> {
    let squeezed: String = attribute.chars().filter(|c| !c.is_whitespace()).collect();
    if !squeezed.contains("derive(") {
        return None;
    }
    // Matched on the capitalized trait names, which do not overlap: `Deserialize` contains
    // a lower-case `serialize`, never `Serialize`, so neither can be mistaken for the other.
    ["Serialize", "Deserialize"]
        .into_iter()
        .find(|forbidden| squeezed.contains(forbidden))
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
fn lint_only_the_composition_root_names_the_contract_or_the_publication_layer() {
    // Swept over *all* of `src/` rather than an enumerated list of surfaces. An enumerated
    // list is the same shape of blind spot that has now been remediated twice in
    // `tests/adaptive_dependency_lint.rs`: it silently stops covering whatever is added
    // next. The first draft of this lint listed six directories and omitted `src/mqtt`,
    // which publishes to Home Assistant — an out-of-repository consumer, and therefore the
    // single worst omission available.
    //
    // Exempt, with reasons:
    //   src/adaptive, src/producers — the layers themselves
    //   src/lib.rs, src/main.rs     — the composition root, which must name both to wire them
    const EXEMPT: &[&str] = &["src/adaptive", "src/producers", "src/lib.rs", "src/main.rs"];
    let mut violations = Vec::new();
    let mut swept = 0usize;
    for (path, text) in sources_under("src") {
        if EXEMPT.iter().any(|exempt| path.starts_with(exempt)) {
            continue;
        }
        swept += 1;
        for reference in forbidden_module_references(&code_only(&text)) {
            violations.push(format!("{path}: references `{reference}`"));
        }
    }
    assert!(
        swept > 50,
        "the sweep covered only {swept} files, which means it is not actually walking src/"
    );
    assert!(
        violations.is_empty(),
        "#324 publishes to in-repository consumers only, and `ProducerDocument` derives \
         `Serialize` — one `Json(snapshot)` re-exports the whole contract. A surface reading \
         the producer document is #326's decision and needs its own API approval:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_adaptive_event_derives_no_serialization() {
    // Belt to the braces above: even if a variant were added to the wrong enum, the
    // internal event type itself cannot be written to a wire.
    //
    // Inspects *every* attached attribute, not the nearest one. An earlier
    // `#[derive(Serialize)]` above a later `#[derive(Debug, Clone)]` was invisible to the
    // previous `rfind("#[derive(")` scan, as was any `#[cfg_attr(…, derive(Serialize))]`,
    // which the scan never looked for at all. Found by CodeRabbit at `eebde11`.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let attributes =
        attributes_attached_to(&event, ADAPTIVE_EVENT_DECLARATION).unwrap_or_else(|| {
            panic!("src/producers/event.rs must declare `{ADAPTIVE_EVENT_DECLARATION}`")
        });
    let offenders: Vec<String> = attributes
        .iter()
        .filter_map(|attribute| {
            introduces_serialization(attribute)
                .map(|trait_name| format!("{trait_name} via {attribute}"))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "AdaptiveEvent would be serializable. The internal bus exists precisely so producer \
         lifecycle cannot reach a wire, and a derive is the only structural part of that \
         guarantee. Found: {offenders:?}"
    );

    // A hand-written impl defeats the same guarantee without any derive at all.
    let squeezed: String = code_only(&event)
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect();
    for forbidden in ["Serialize", "Deserialize"] {
        assert!(
            !squeezed.contains(&format!("{forbidden}forAdaptiveEvent")),
            "AdaptiveEvent has a hand-written `{forbidden}` impl, which reaches a wire just \
             as surely as a derive."
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
        if forbidden_module_references(&code_only(&text))
            .iter()
            .any(|reference| reference.contains("producers"))
        {
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
        // Disjunctive: contains `feature="server"` but compiles the module for `web`
        // *without* `server`, which is the WASM breakage this lint exists to prevent.
        // Passed the pre-fix scanner. Found by CodeRabbit at `86e5bd2`.
        "#[cfg(any(feature = \"server\", feature = \"web\"))]\npub mod producers;\n",
        "#[cfg(any(feature=\"server\",feature=\"web\"))]\npub mod producers;\n",
        // Negation, for the same reason in the opposite direction.
        "#[cfg(not(feature = \"server\"))]\npub mod producers;\n",
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
fn a_stricter_conjunctive_gate_is_still_server_only() {
    // `all(feature = "server", unix)` compiles the module in strictly fewer configurations
    // than the plain gate, so it cannot reintroduce the WASM breakage. Accepted, unlike the
    // disjunctive form above - the distinction is which direction the gate is loosened.
    let source = "#[cfg(all(feature = \"server\", unix))]\npub mod producers;\n";
    let gates = declaration_gates(source, PRODUCERS_DECLARATION).expect("declaration found");
    assert!(gates.iter().any(|gate| is_server_gate(gate)));
}

#[test]
fn a_serializing_adaptive_event_is_rejected_however_it_is_spelled() {
    // Non-vacuity for `lint_adaptive_event_derives_no_serialization`. Cases 2 and 4 through
    // 7 all passed the previous scanner, which read only the *nearest* `#[derive(` and never
    // looked for `cfg_attr` at all. Found by CodeRabbit at `eebde11`.
    for source in [
        // 1. The obvious form.
        "#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n",
        // 2. Stacked behind the innocent derive - the reported blind spot.
        "#[derive(Serialize, Deserialize)]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        // 3. The other order.
        "#[derive(Debug, Clone)]\n#[derive(Deserialize)]\npub enum AdaptiveEvent {\n}\n",
        // 4. Behind an unrelated attribute.
        "#[derive(Serialize)]\n#[non_exhaustive]\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n",
        // 5. Conditional. A cfg_attr gate does not make it safe, it makes it somebody
        //    else's build flag.
        "#[cfg_attr(feature = \"server\", derive(Serialize))]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        // 6. Conditional, unspaced, and behind the innocent derive.
        "#[derive(Debug,Clone)]\n#[cfg_attr(test,derive(Deserialize))]\npub enum AdaptiveEvent {\n}\n",
        // 7. Sharing the line with the declaration.
        "#[derive(Debug)] #[derive(Serialize)] pub enum AdaptiveEvent {\n}\n",
    ] {
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        assert!(
            attributes
                .iter()
                .any(|attribute| introduces_serialization(attribute).is_some()),
            "a serializing AdaptiveEvent was admitted:\n{source}\nattributes: {attributes:?}"
        );
    }
}

#[test]
fn a_non_serializing_adaptive_event_is_accepted() {
    // The other direction. A scanner that rejects everything is as useless as one that
    // rejects nothing, and case 3 is the false positive the previous implementation
    // avoided by accident and this one must avoid on purpose: a *different* type's
    // serializing derive higher up the file.
    for source in [
        "#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        "#[derive(Debug, Clone)]\n#[non_exhaustive]\npub enum AdaptiveEvent {\n}\n",
        "#[derive(Serialize, Deserialize)]\npub struct Unrelated {\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        "#[cfg_attr(test, derive(Default))]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
    ] {
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        let offenders: Vec<_> = attributes
            .iter()
            .filter(|attribute| introduces_serialization(attribute).is_some())
            .collect();
        assert!(
            offenders.is_empty(),
            "the scanner invented a serialization derive in:\n{source}\nfound: {offenders:?}"
        );
    }
}

#[test]
fn attribute_extraction_survives_nesting_and_string_literals() {
    // `attributes_in` has to close on the matching bracket, not the first one. A
    // `cfg_attr` carrying a derive and a doc comment carrying a `]` are the two shapes
    // that break a naive scan, and both are reachable in ordinary code.
    let nested = attributes_in("#[cfg_attr(feature = \"server\", derive(Serialize))]");
    assert_eq!(nested.len(), 1);
    assert!(
        nested[0].ends_with("))]"),
        "attribute was truncated: {nested:?}"
    );

    let bracketed = attributes_in("#[doc = \"a ] bracket\"]\n#[derive(Debug)]");
    assert_eq!(
        bracketed.len(),
        2,
        "a string literal split an attribute: {bracketed:?}"
    );
    assert_eq!(bracketed[1], "#[derive(Debug)]");

    // Inner attributes belong to the enclosing module, not to the next item.
    let inner = attributes_in("#![deny(unsafe_code)]\n#[derive(Debug)]");
    assert_eq!(inner, vec!["#[derive(Debug)]".to_string()]);
}

#[test]
fn grouped_use_trees_do_not_hide_a_forbidden_module() {
    // A needle scan for `crate::adaptive` misses every one of these. This is the house
    // style in this repository - `src/main.rs` imports its modules exactly this way - so a
    // surface writing idiomatic code would have slipped past the boundary check entirely.
    // Found by CodeRabbit at `86e5bd2`.
    for source in [
        "use crate::{adaptive, bus};",
        "use crate::{bus, producers};",
        "use unified_hifi_control::{adaptive, producers};",
        "use unified_hifi_control::{\n    adapters, aggregator, api,\n    producers,\n};",
        // Nested and aliased forms.
        "use crate::{producers::ProducerAggregator, bus::SharedBus};",
        "use crate::{adaptive as contract};",
        "use crate::{bus::{SharedBus, BusEvent}, adaptive};",
    ] {
        let found = forbidden_module_references(source);
        assert!(
            !found.is_empty(),
            "a grouped import hid a forbidden module:\n{source}"
        );
    }
}

#[test]
fn grouped_use_trees_do_not_produce_false_positives() {
    // The scanner must not fire on modules that merely share a prefix, on unrelated
    // groups, or on a group belonging to a different root.
    for source in [
        "use crate::{bus, config};",
        "use crate::{adapters, aggregator};",
        "use std::{collections::BTreeMap, sync::Arc};",
        "use serde::{Deserialize, Serialize};",
        "use other_crate::{adaptive, producers};",
    ] {
        let found = forbidden_module_references(source);
        assert!(
            found.is_empty(),
            "the scanner invented a reference in:\n{source}\nfound: {found:?}"
        );
    }
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
