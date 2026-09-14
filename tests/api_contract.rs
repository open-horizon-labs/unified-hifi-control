//! API Contract Tests
//!
//! Ensures API routes don't change without explicit approval.
//! The golden file at tests/fixtures/api_routes.txt is the source of truth.
//!
//! If this test fails:
//! 1. Review the route changes carefully
//! 2. Update api_routes.txt if the change is intentional
//! 3. Add 'api-change-approved' label to PR
//!
//! Run with: cargo test --test api_contract

use std::collections::BTreeSet;
use std::fs;

/// Extract routes from the golden file
fn load_golden_routes() -> BTreeSet<String> {
    let content =
        fs::read_to_string("tests/fixtures/api_routes.txt").expect("Failed to read api_routes.txt");

    content
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .map(|line| line.trim().to_string())
        .collect()
}

/// Collapse every `.route(...)` call's *internal* whitespace (including newlines) to single
/// spaces, so a multi-line call like
/// ```ignore
/// .route(
///     "/hqp/discover",
///     get(api::hqp_discover_handler),
/// )
/// ```
/// becomes indistinguishable, to the per-line scanner below, from writing it on one line. Only
/// text between the call's own matching parens is touched; everything else is untouched
/// byte-for-byte. Paren depth is tracked from the `.route(`'s own opening paren, so a nested call
/// like `get(handler)` inside it does not close the scan early.
fn normalize_multiline_route_calls(content: &str) -> String {
    let marker = ".route(";
    let mut out = String::with_capacity(content.len());
    let mut rest = content;
    while let Some(pos) = rest.find(marker) {
        out.push_str(&rest[..pos + marker.len()]);
        let after = &rest[pos + marker.len()..];
        let mut depth: i32 = 1;
        let mut end = after.len();
        for (idx, ch) in after.char_indices() {
            match ch {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = idx;
                        break;
                    }
                }
                _ => {}
            }
        }
        let call_body = &after[..end];
        out.push_str(&call_body.split_whitespace().collect::<Vec<_>>().join(" "));
        if end < after.len() {
            out.push(')');
            rest = &after[end + 1..];
        } else {
            // Unbalanced (should not happen in valid Rust source); stop rewriting rather than
            // silently dropping the remainder of the file.
            rest = "";
        }
    }
    out.push_str(rest);
    out
}

/// Scan already-newline-normalized source for `.route("/path", method(handler))` lines.
fn scan_route_lines(content: &str) -> BTreeSet<String> {
    let mut routes = BTreeSet::new();

    for line in content.lines() {
        let line = line.trim();

        // Skip comments
        if line.starts_with("//") {
            continue;
        }

        // Match .route("/path", method(handler))
        if let Some(start) = line.find(".route(\"") {
            let rest = &line[start + 8..];
            if let Some(end) = rest.find('"') {
                let path = &rest[..end];

                // Determine HTTP method from the handler
                let method = if line.contains("get(") {
                    "GET"
                } else if line.contains("post(") {
                    "POST"
                } else if line.contains("put(") {
                    "PUT"
                } else if line.contains("delete(") {
                    "DELETE"
                } else {
                    continue; // Unknown method
                };

                routes.insert(format!("{} {}", method, path));
            }
        }
    }

    routes
}

/// Extract routes from main.rs source, plus (only when main.rs actually shows evidence of
/// merging it in) the routes a separate module's own `routes()` helper registers. This is
/// deliberately conservative: a module that defines `.route(...)` calls but that main.rs never
/// merges must NOT count as a registered API route just because the function exists — that would
/// let a test claim main-router registration a custom/unwired helper never earned. The evidence
/// required is main.rs literally calling `api::hqp_outputs_http::routes()`, the exact call
/// `src/main.rs` makes today; this is intentionally a named, narrow allowance for that one module
/// rather than a general "scan every file" rule, so the contract stays anchored to what main.rs
/// itself demonstrably wires.
fn extract_routes_from_source() -> BTreeSet<String> {
    let main_content = fs::read_to_string("src/main.rs").expect("Failed to read main.rs");
    let mut routes = scan_route_lines(&normalize_multiline_route_calls(&main_content));

    if main_content.contains("api::hqp_outputs_http::routes()") {
        let merged_content = fs::read_to_string("src/api/hqp_outputs_http.rs")
            .expect("Failed to read src/api/hqp_outputs_http.rs");
        routes.extend(scan_route_lines(&normalize_multiline_route_calls(
            &merged_content,
        )));
    }

    routes
}

#[test]
fn api_routes_match_contract() {
    let golden = load_golden_routes();
    let actual = extract_routes_from_source();

    let added: Vec<_> = actual.difference(&golden).collect();
    let removed: Vec<_> = golden.difference(&actual).collect();

    if !added.is_empty() || !removed.is_empty() {
        let mut msg = String::from("\n\nAPI CONTRACT VIOLATION!\n\n");

        if !added.is_empty() {
            msg.push_str("Routes ADDED (not in contract):\n");
            for route in &added {
                msg.push_str(&format!("  + {}\n", route));
            }
            msg.push('\n');
        }

        if !removed.is_empty() {
            msg.push_str("Routes REMOVED (missing from implementation):\n");
            for route in &removed {
                msg.push_str(&format!("  - {}\n", route));
            }
            msg.push('\n');
        }

        msg.push_str("To fix:\n");
        msg.push_str("1. If intentional: update tests/fixtures/api_routes.txt\n");
        msg.push_str("2. Add 'api-change-approved' label to PR\n");
        msg.push_str("3. Get explicit approval for API changes\n");

        panic!("{}", msg);
    }
}

#[test]
fn golden_file_is_sorted() {
    let content =
        fs::read_to_string("tests/fixtures/api_routes.txt").expect("Failed to read api_routes.txt");

    let routes: Vec<_> = content
        .lines()
        .filter(|line| !line.starts_with('#') && !line.trim().is_empty())
        .collect();

    let mut sorted = routes.clone();
    sorted.sort();

    assert_eq!(
        routes, sorted,
        "api_routes.txt is not sorted! Please sort alphabetically."
    );
}
