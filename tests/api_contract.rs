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

/// Extract routes from main.rs source
fn extract_routes_from_source() -> BTreeSet<String> {
    let content = fs::read_to_string("src/main.rs").expect("Failed to read main.rs");
    let mut routes = BTreeSet::new();

    // Join all lines into one string with spaces, then find .route() calls
    // This handles both single-line and multi-line formatted routes
    let joined = content
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.starts_with("//"))
        .collect::<Vec<_>>()
        .join(" ");
    // Normalize whitespace so .route( "/path" becomes .route("/path"
    let joined = joined.replace(".route( ", ".route(");

    // Find all .route("path", handler) patterns
    let mut search_from = 0;
    while let Some(pos) = joined[search_from..].find(".route(\"") {
        let abs_pos = search_from + pos + 8; // skip .route("
        if let Some(end) = joined[abs_pos..].find('"') {
            let path = &joined[abs_pos..abs_pos + end];

            // Find the matching closing ) for this .route() call
            // Look at the handler expression between the path and the closing )
            let after_path = abs_pos + end;
            // Find the balanced closing paren
            let mut depth = 1; // we're inside .route(
            let handler_start = after_path;
            let mut handler_end = handler_start;
            for (i, ch) in joined[handler_start..].char_indices() {
                match ch {
                    '(' => depth += 1,
                    ')' => {
                        depth -= 1;
                        if depth == 0 {
                            handler_end = handler_start + i;
                            break;
                        }
                    }
                    _ => {}
                }
            }

            let handler_text = &joined[handler_start..handler_end];
            for (pattern, method) in [
                ("get(", "GET"),
                ("post(", "POST"),
                ("put(", "PUT"),
                ("delete(", "DELETE"),
            ] {
                if handler_text.contains(pattern) {
                    routes.insert(format!("{} {}", method, path));
                }
            }

            search_from = handler_end;
        } else {
            break;
        }
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
