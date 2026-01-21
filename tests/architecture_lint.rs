//! Architecture enforcement lint - ensures API handlers use aggregator, not direct adapter access.
//!
//! The ZoneAggregator is the single source of truth for zone state. API handlers should
//! query the aggregator, not individual adapters, to ensure consistent state across
//! all clients (SSE, REST, etc.).
//!
//! This test parses the API module and flags any direct adapter state queries:
//! - `state.roon.get_zones()` should be `state.aggregator.get_zones()`
//! - `state.lms.get_cached_players()` should use aggregator
//! - etc.
//!
//! Exceptions:
//! - Adapter-specific status endpoints (e.g., `/roon/status`) are allowed
//! - Control/action endpoints that must talk to adapters directly are allowed
//! - Configuration endpoints are allowed

use std::fs;
use std::path::Path;
use walkdir::WalkDir;

/// Patterns that indicate direct adapter zone/player state queries
/// These should go through the aggregator instead
const DISALLOWED_PATTERNS: &[(&str, &str)] = &[
    // Roon adapter direct zone queries
    (
        "state.roon.get_zones()",
        "Use state.aggregator.get_zones() instead",
    ),
    (
        "state.roon.get_zone(",
        "Use state.aggregator.get_zone() instead",
    ),
    // LMS adapter direct player queries
    (
        "state.lms.get_cached_players()",
        "Use state.aggregator.get_zones() instead",
    ),
    (
        "state.lms.get_cached_player(",
        "Use state.aggregator.get_zone() instead",
    ),
    // OpenHome direct zone queries
    (
        "state.openhome.get_zones()",
        "Use state.aggregator.get_zones() instead",
    ),
    // UPnP direct zone queries
    (
        "state.upnp.get_zones()",
        "Use state.aggregator.get_zones() instead",
    ),
];

/// Files that are allowed to have direct adapter access
/// (e.g., adapter-specific status endpoints, tests)
const ALLOWED_FILES: &[&str] = &[
    // Legacy endpoints that need migration (tracked in separate issue)
    // TODO: Remove these as endpoints are migrated to use aggregator
];

/// Functions/handlers that are allowed to access adapters directly
/// (status checks, control actions, configuration, adapter-specific endpoints)
const ALLOWED_CONTEXTS: &[&str] = &[
    // Status endpoints need direct adapter access
    "_status_handler",
    // Control endpoints send commands to adapters
    "_control_handler",
    "_volume_handler",
    // Configuration endpoints
    "_configure_handler",
    "_config_handler",
    // Image endpoints (Roon-specific image API)
    "_image_handler",
    // Discovery endpoints
    "_discover_handler",
    "_detect_handler",
    // HQPlayer-specific (not zone data)
    "hqp_",
    // Instance management
    "_instance",
    "_instances",
    // Zone linking (HQP-specific feature)
    "_zone_link",
    "_zone_unlink",
    "_zone_pipeline",
    // Adapter-specific zone/player endpoints (these are intentionally not unified)
    // Roon-specific endpoints
    "roon_zones_handler",
    "roon_zone_handler",
    // LMS-specific endpoints
    "lms_players_handler",
    "lms_player_handler",
    // OpenHome-specific endpoints
    "openhome_zones_handler",
    "openhome_now_playing_handler",
    // UPnP-specific endpoints
    "upnp_zones_handler",
    "upnp_now_playing_handler",
];

fn is_in_allowed_context(content: &str, pattern_pos: usize) -> bool {
    // Find the function containing this pattern by looking backwards for "fn "
    let before = &content[..pattern_pos];

    // Find the last function definition before this pattern
    // Look for patterns like "pub async fn foo_handler" or "async fn foo"
    let fn_markers = ["pub async fn ", "async fn ", "pub fn ", "fn "];

    for marker in fn_markers {
        if let Some(fn_pos) = before.rfind(marker) {
            let fn_start = fn_pos + marker.len();
            let after_marker = &before[fn_start..];

            // Extract function name (up to the opening paren or end of identifier)
            let fn_end = after_marker
                .find(|c: char| c == '(' || c == '<' || c.is_whitespace())
                .unwrap_or(after_marker.len().min(50));

            let fn_name = &after_marker[..fn_end];

            // Check if this function is in an allowed context
            for allowed in ALLOWED_CONTEXTS {
                if fn_name.contains(allowed) || allowed.contains(fn_name) {
                    return true;
                }
            }

            // Found the function, but it's not in allowed list
            return false;
        }
    }

    false
}

fn analyze_file(path: &Path) -> Vec<(String, String, String)> {
    let path_str = path.display().to_string();

    // Skip allowed files
    for allowed in ALLOWED_FILES {
        if path_str.contains(allowed) {
            return vec![];
        }
    }

    // Only check api/ directory
    if !path_str.contains("/api/") && !path_str.contains("\\api\\") {
        return vec![];
    }

    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut violations = Vec::new();

    for (pattern, suggestion) in DISALLOWED_PATTERNS {
        let mut search_from = 0;
        while let Some(pos) = content[search_from..].find(pattern) {
            let absolute_pos = search_from + pos;

            // Check if this is in an allowed context
            if !is_in_allowed_context(&content, absolute_pos) {
                // Find line number
                let line_num = content[..absolute_pos].matches('\n').count() + 1;

                violations.push((
                    format!("{}:{}", path_str, line_num),
                    (*pattern).to_string(),
                    (*suggestion).to_string(),
                ));
            }

            search_from = absolute_pos + pattern.len();
        }
    }

    violations
}

#[test]
fn api_handlers_must_use_aggregator() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut all_violations = Vec::new();

    for entry in WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let violations = analyze_file(entry.path());
        all_violations.extend(violations);
    }

    if !all_violations.is_empty() {
        let mut error_msg = String::from(
            "\n\n\
            ╔══════════════════════════════════════════════════════════════════════════════╗\n\
            ║  ARCHITECTURE VIOLATION: API handlers must use ZoneAggregator                ║\n\
            ╚══════════════════════════════════════════════════════════════════════════════╝\n\n\
            The ZoneAggregator is the single source of truth for zone state.\n\
            API handlers should query the aggregator, not individual adapters.\n\n\
            This ensures:\n\
            - Consistent state across all clients (SSE, REST, UI)\n\
            - Proper event-driven updates via the bus\n\
            - Clean separation between adapters and API layer\n\n\
            Violations found:\n\n",
        );

        for (location, pattern, suggestion) in &all_violations {
            error_msg.push_str(&format!("  {} \n", location));
            error_msg.push_str(&format!("    Found: {}\n", pattern));
            error_msg.push_str(&format!("    Fix: {}\n\n", suggestion));
        }

        error_msg.push_str(
            "If this is intentional (e.g., adapter-specific endpoint), add the function\n\
            name pattern to ALLOWED_CONTEXTS in tests/architecture_lint.rs\n",
        );

        panic!("{}", error_msg);
    }
}

#[test]
fn aggregator_exists_in_app_state() {
    // Verify that AppState has an aggregator field
    let api_mod = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("api")
        .join("mod.rs");

    let content = fs::read_to_string(&api_mod).expect("Failed to read api/mod.rs");

    assert!(
        content.contains("pub aggregator: Arc<ZoneAggregator>"),
        "AppState must have a `pub aggregator: Arc<ZoneAggregator>` field"
    );
}
