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

    // Only check api/ and knobs/ directories
    let is_api = path_str.contains("/api/") || path_str.contains("\\api\\");
    let is_knobs = path_str.contains("/knobs/") || path_str.contains("\\knobs\\");
    if !is_api && !is_knobs {
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

/// Adapter-to-prefix mapping for zone_id consistency check
/// Format: (adapter_file, format_prefix, prefixed_zone_id_constructor)
const ADAPTER_PREFIXES: &[(&str, &str, &str)] = &[
    ("roon.rs", "roon:", "PrefixedZoneId::roon("),
    ("lms.rs", "lms:", "PrefixedZoneId::lms("),
    ("openhome.rs", "openhome:", "PrefixedZoneId::openhome("),
    ("upnp.rs", "upnp:", "PrefixedZoneId::upnp("),
];

/// Bus events that require prefixed zone_ids
const ZONE_ID_BUS_EVENTS: &[&str] = &[
    "BusEvent::ZoneUpdated",
    "BusEvent::ZoneRemoved",
    "BusEvent::NowPlayingChanged",
    "BusEvent::SeekPositionChanged",
];

/// Bus events that require prefixed output_ids (for volume control matching)
const OUTPUT_ID_BUS_EVENTS: &[&str] = &["BusEvent::VolumeChanged"];

/// Patterns that indicate VolumeControl struct creation (output_id must be prefixed)
const VOLUME_CONTROL_PATTERNS: &[&str] = &["VolumeControl {", "VolumeControl{"];

#[test]
fn bus_events_use_prefixed_zone_ids() {
    // Issue: Roon adapter was emitting bus events with raw zone_ids (e.g., "1601bb42...")
    // but the aggregator stores zones with prefixed IDs (e.g., "roon:1601bb42...").
    // This caused state updates to be silently dropped.
    //
    // This test ensures all adapters emit bus events with properly prefixed zone_ids.

    let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("adapters");

    let mut violations = Vec::new();

    for (adapter_file, expected_prefix, expected_constructor) in ADAPTER_PREFIXES {
        let path = adapters_dir.join(adapter_file);
        if !path.exists() {
            continue;
        }

        let content = fs::read_to_string(&path).expect("Failed to read adapter file");

        for event_type in ZONE_ID_BUS_EVENTS {
            // Find all occurrences of this event type
            let mut search_from = 0;
            while let Some(event_pos) = content[search_from..].find(event_type) {
                let absolute_pos = search_from + event_pos;

                // Look for zone_id field within the next ~200 chars (the event struct)
                let event_end = (absolute_pos + 300).min(content.len());
                let event_block = &content[absolute_pos..event_end];

                // Check if this block contains a zone_id field
                if let Some(zone_id_pos) = event_block.find("zone_id:") {
                    let after_zone_id = &event_block[zone_id_pos..];

                    // Valid patterns (look in first 100 chars after zone_id:):
                    // - PrefixedZoneId::xxx( - compile-time enforced prefix (preferred)
                    // - format!("prefix:..." - direct prefix in format string (legacy)
                    // - prefixed_zone_id - variable that was set with PrefixedZoneId or format!
                    // - zone.zone_id - from a Zone struct that already has prefix
                    // - zone_id - variable (check if PrefixedZoneId::xxx was used earlier in block)
                    let check_region = &after_zone_id[..after_zone_id.len().min(100)];
                    let has_prefixed_zone_id_type = check_region.contains(expected_constructor);
                    let has_format_prefix =
                        check_region.contains("format!") && check_region.contains(expected_prefix);
                    let has_prefixed_var = check_region.contains("prefixed_zone_id");
                    let has_zone_struct = check_region.contains("zone.zone_id");

                    // Check if zone_id variable was constructed with PrefixedZoneId earlier
                    // Look backwards up to 2000 chars (~25 lines) for "zone_id = PrefixedZoneId::"
                    let lookback_start = absolute_pos.saturating_sub(2000);
                    let lookback_region = &content[lookback_start..absolute_pos];
                    let has_zone_id_from_prefixed =
                        lookback_region.contains("zone_id = PrefixedZoneId::");

                    // Check if the value after "zone_id:" is a variable named zone_id
                    // (e.g., "zone_id: zone_id.clone()" or "zone_id: zone_id,")
                    // Skip past "zone_id:" to get the value
                    let value_part = after_zone_id
                        .strip_prefix("zone_id:")
                        .map(|s| s.trim_start())
                        .unwrap_or("");
                    let uses_zone_id_var = (value_part.starts_with("zone_id,")
                        || value_part.starts_with("zone_id.")
                        || value_part.starts_with("zone_id\n"))
                        && has_zone_id_from_prefixed;

                    if !has_prefixed_zone_id_type
                        && !has_format_prefix
                        && !has_prefixed_var
                        && !has_zone_struct
                        && !uses_zone_id_var
                    {
                        // Find line number
                        let line_num = content[..absolute_pos].matches('\n').count() + 1;
                        violations.push((
                            adapter_file.to_string(),
                            line_num,
                            event_type.to_string(),
                            expected_prefix.to_string(),
                        ));
                    }
                }

                search_from = absolute_pos + event_type.len();
            }
        }
    }

    if !violations.is_empty() {
        let mut error_msg = String::from(
            "\n\n\
            ╔══════════════════════════════════════════════════════════════════════════════╗\n\
            ║  ARCHITECTURE VIOLATION: Bus events must use prefixed zone_ids              ║\n\
            ╚══════════════════════════════════════════════════════════════════════════════╝\n\n\
            The ZoneAggregator stores zones with prefixed IDs (e.g., 'roon:xxx').\n\
            Bus events must use the same format or updates will be silently dropped.\n\n\
            Violations found:\n\n",
        );

        for (file, line, event, prefix) in &violations {
            error_msg.push_str(&format!("  {}:{}\n", file, line));
            error_msg.push_str(&format!("    Event: {}\n", event));
            error_msg.push_str(&format!(
                "    Expected: zone_id with '{}' prefix\n\n",
                prefix
            ));
        }

        error_msg.push_str(
            "Fix: Use PrefixedZoneId::xxx() constructor (preferred) or format!(\"prefix:{}\", raw_id).\n",
        );

        panic!("{}", error_msg);
    }
}

#[test]
fn bus_events_use_prefixed_output_ids() {
    // VolumeChanged events use output_id to match zones.
    // The aggregator matches by comparing zone.volume_control.output_id with event.output_id.
    // Both must use the same format - prefixed IDs (e.g., "lms:xx:xx:xx").
    //
    // Without this, volume updates silently fail to match zones.

    let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("adapters");

    let mut violations = Vec::new();

    for (adapter_file, expected_prefix, expected_constructor) in ADAPTER_PREFIXES {
        let path = adapters_dir.join(adapter_file);
        if !path.exists() {
            continue;
        }

        let content = fs::read_to_string(&path).expect("Failed to read adapter file");

        for event_type in OUTPUT_ID_BUS_EVENTS {
            // Find all occurrences of this event type
            let mut search_from = 0;
            while let Some(event_pos) = content[search_from..].find(event_type) {
                let absolute_pos = search_from + event_pos;

                // Look for output_id field within the next ~200 chars (the event struct)
                let event_end = (absolute_pos + 300).min(content.len());
                let event_block = &content[absolute_pos..event_end];

                // Check if this block contains an output_id field
                if let Some(output_id_pos) = event_block.find("output_id:") {
                    let after_output_id = &event_block[output_id_pos..];

                    // Valid patterns:
                    // - PrefixedZoneId::xxx( - compile-time enforced prefix (preferred)
                    // - format!("prefix:..." - direct prefix in format string
                    let check_region = &after_output_id[..after_output_id.len().min(100)];
                    let has_prefixed_zone_id_type = check_region.contains(expected_constructor);
                    let has_format_prefix =
                        check_region.contains("format!") && check_region.contains(expected_prefix);

                    if !has_prefixed_zone_id_type && !has_format_prefix {
                        // Find line number
                        let line_num = content[..absolute_pos].matches('\n').count() + 1;
                        violations.push((
                            adapter_file.to_string(),
                            line_num,
                            event_type.to_string(),
                            expected_prefix.to_string(),
                        ));
                    }
                }

                search_from = absolute_pos + event_type.len();
            }
        }
    }

    if !violations.is_empty() {
        let mut error_msg = String::from(
            "\n\n\
            ╔══════════════════════════════════════════════════════════════════════════════╗\n\
            ║  ARCHITECTURE VIOLATION: VolumeChanged must use prefixed output_ids          ║\n\
            ╚══════════════════════════════════════════════════════════════════════════════╝\n\n\
            The ZoneAggregator matches VolumeChanged events by comparing:\n\
              event.output_id == zone.volume_control.output_id\n\n\
            Both must use the same prefixed format (e.g., 'lms:xx:xx:xx').\n\
            Without this, volume updates silently fail to match zones.\n\n\
            Violations found:\n\n",
        );

        for (file, line, event, prefix) in &violations {
            error_msg.push_str(&format!("  {}:{}\n", file, line));
            error_msg.push_str(&format!("    Event: {}\n", event));
            error_msg.push_str(&format!(
                "    Expected: output_id with '{}' prefix\n\n",
                prefix
            ));
        }

        error_msg.push_str(
            "Fix: Use PrefixedZoneId::xxx() for output_id in VolumeChanged events.\n\
             Also ensure zone.volume_control.output_id uses the same prefixed format.\n",
        );

        panic!("{}", error_msg);
    }
}

#[test]
fn volume_control_uses_prefixed_output_ids() {
    // VolumeControl.output_id must match the format used in VolumeChanged events.
    // Both must use prefixed IDs (e.g., "lms:xx:xx:xx", "roon:output-id").
    //
    // Without this, volume updates silently fail to match zones.

    let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("adapters");

    let mut violations = Vec::new();

    for (adapter_file, expected_prefix, expected_constructor) in ADAPTER_PREFIXES {
        let path = adapters_dir.join(adapter_file);
        if !path.exists() {
            continue;
        }

        let content = fs::read_to_string(&path).expect("Failed to read adapter file");

        for pattern in VOLUME_CONTROL_PATTERNS {
            // Find all occurrences of VolumeControl struct creation
            let mut search_from = 0;
            while let Some(vc_pos) = content[search_from..].find(pattern) {
                let absolute_pos = search_from + vc_pos;

                // Look for output_id field within the next ~500 chars (the struct)
                let struct_end = (absolute_pos + 500).min(content.len());
                let struct_block = &content[absolute_pos..struct_end];

                // Find the closing brace to limit our search
                let brace_end = struct_block.find('}').unwrap_or(struct_block.len());
                let struct_block = &struct_block[..brace_end];

                // Check if this block contains an output_id field
                if let Some(output_id_pos) = struct_block.find("output_id:") {
                    let after_output_id = &struct_block[output_id_pos..];

                    // Valid patterns:
                    // - PrefixedZoneId::xxx( - compile-time enforced prefix
                    // - format!("prefix:..." - direct prefix in format string
                    // - zone_id (variable) - if previously constructed with PrefixedZoneId
                    let check_region = &after_output_id[..after_output_id.len().min(100)];
                    let has_prefixed_zone_id_type = check_region.contains("PrefixedZoneId::");
                    let has_format_prefix =
                        check_region.contains("format!") && check_region.contains(expected_prefix);
                    // Check if using a zone_id variable (which should already be prefixed)
                    let has_zone_id_var = check_region.contains("zone_id");

                    if !has_prefixed_zone_id_type && !has_format_prefix && !has_zone_id_var {
                        // Find line number
                        let line_num = content[..absolute_pos].matches('\n').count() + 1;
                        violations.push((
                            adapter_file.to_string(),
                            line_num,
                            expected_prefix.to_string(),
                        ));
                    }
                }

                search_from = absolute_pos + pattern.len();
            }
        }
    }

    if !violations.is_empty() {
        let mut error_msg = String::from(
            "\n\n\
            ╔══════════════════════════════════════════════════════════════════════════════╗\n\
            ║  ARCHITECTURE VIOLATION: VolumeControl must use prefixed output_ids          ║\n\
            ╚══════════════════════════════════════════════════════════════════════════════╝\n\n\
            VolumeControl.output_id must match the format used in VolumeChanged events.\n\
            Both must use prefixed IDs (e.g., 'lms:xx:xx:xx', 'roon:output-id').\n\n\
            Violations found:\n\n",
        );

        for (file, line, prefix) in &violations {
            error_msg.push_str(&format!("  {}:{}\n", file, line));
            error_msg.push_str(&format!(
                "    Expected: output_id with '{}' prefix\n\n",
                prefix
            ));
        }

        error_msg.push_str(
            "Fix: Use PrefixedZoneId::xxx().to_string() or format!(\"prefix:{}\", id) for output_id.\n",
        );

        panic!("{}", error_msg);
    }
}

// =============================================================================
// Bus Event Schema Enforcement
// =============================================================================

/// Events that the ZoneAggregator handles (updates zone state)
/// If you want to update aggregator state, you MUST use one of these events.
const AGGREGATOR_HANDLED_EVENTS: &[&str] = &[
    "BusEvent::ZoneDiscovered",      // Adds a new zone
    "BusEvent::ZoneUpdated",         // Updates zone name/state
    "BusEvent::ZoneRemoved",         // Removes a zone
    "BusEvent::NowPlayingChanged",   // Updates now_playing metadata
    "BusEvent::VolumeChanged",       // Updates volume
    "BusEvent::SeekPositionChanged", // Updates seek position
    "BusEvent::AdapterStopping",     // Triggers zone cleanup
    "BusEvent::ShuttingDown",        // Triggers shutdown
];

/// Events that are adapter-specific and NOT handled by aggregator
/// These are for SSE/UI updates ONLY - they do NOT update aggregator state.
/// WARNING: Publishing these will NOT update /zones endpoint or aggregator state!
#[allow(dead_code)]
const ADAPTER_SPECIFIC_EVENTS: &[&str] = &[
    "BusEvent::LmsPlayerStateChanged", // LMS-specific, SSE only
    "BusEvent::LmsConnected",          // LMS-specific, SSE only
    "BusEvent::LmsDisconnected",       // LMS-specific, SSE only
    "BusEvent::RoonConnected",         // Roon-specific, SSE only
    "BusEvent::RoonDisconnected",      // Roon-specific, SSE only
];

/// Patterns that indicate playback state changes that should also emit ZoneUpdated
/// If you see these patterns, ZoneUpdated should be emitted nearby (within ~50 lines)
///
/// Note: LmsPlayerStateChanged has been removed - we now only emit ZoneUpdated which
/// SSE uses directly (checking for "lms:" prefix in zone_id). The lint heuristic
/// looks for zone_id = PrefixedZoneId:: in the preceding 2000 chars, which could have
/// false negatives if the variable is named differently or passed through a function.
const STATE_CHANGE_PATTERNS: &[(&str, &str)] = &[
    // Add adapter-specific state events here if they need to also emit ZoneUpdated
    // Currently empty - all state changes go through ZoneUpdated directly
];

#[test]
fn bus_event_schema_documented() {
    // This test documents the bus event schema and serves as a reference.
    // It will always pass - it exists to make the schema visible in test output.
    println!("\n");
    println!("╔══════════════════════════════════════════════════════════════════════════════╗");
    println!("║                         BUS EVENT SCHEMA                                     ║");
    println!("╚══════════════════════════════════════════════════════════════════════════════╝");
    println!();
    println!("Events handled by ZoneAggregator (updates /zones endpoint):");
    for event in AGGREGATOR_HANDLED_EVENTS {
        println!("  ✓ {}", event);
    }
    println!();
    println!("Adapter-specific events (SSE/UI only, NOT aggregator):");
    for event in ADAPTER_SPECIFIC_EVENTS {
        println!("  ⚠ {} (does NOT update aggregator)", event);
    }
    println!();
    println!("RULE: To update zone state visible in /zones, you MUST emit an");
    println!("      AGGREGATOR_HANDLED_EVENT. Adapter-specific events are for UI only.");
    println!();
}

#[test]
fn state_changes_emit_zone_updated() {
    // Ensures that when adapters emit state change events (like LmsPlayerStateChanged),
    // they ALSO emit ZoneUpdated so the aggregator gets the state change.
    //
    // This prevents the bug where CLI events updated the adapter's internal cache
    // but didn't propagate to the aggregator (visible via /zones endpoint).

    let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("adapters");

    let mut violations = Vec::new();

    for entry in WalkDir::new(&adapters_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|s| s == "rs").unwrap_or(false))
    {
        let path = entry.path();
        let content = fs::read_to_string(path).expect("Failed to read file");
        let filename = path.file_name().unwrap().to_string_lossy();

        for (pattern, explanation) in STATE_CHANGE_PATTERNS {
            // Find all occurrences of the pattern
            let mut search_from = 0;
            while let Some(pos) = content[search_from..].find(pattern) {
                let absolute_pos = search_from + pos;

                // Look for ZoneUpdated within a reasonable range (50 lines ~ 2500 chars)
                // Either before (within 1000 chars) or after (within 2500 chars)
                let check_start = absolute_pos.saturating_sub(1000);
                let check_end = (absolute_pos + 2500).min(content.len());
                let check_region = &content[check_start..check_end];

                // Also check if this is in a context where ZoneUpdated is emitted nearby
                let has_zone_updated = check_region.contains("BusEvent::ZoneUpdated");

                // Skip if ZoneUpdated is emitted nearby
                if !has_zone_updated {
                    let line_num = content[..absolute_pos].matches('\n').count() + 1;
                    violations.push((filename.to_string(), line_num, explanation.to_string()));
                }

                search_from = absolute_pos + pattern.len();
            }
        }
    }

    if !violations.is_empty() {
        let mut error_msg = String::from(
            "\n\n\
            ╔══════════════════════════════════════════════════════════════════════════════╗\n\
            ║  BUS EVENT SCHEMA VIOLATION: State changes must update aggregator            ║\n\
            ╚══════════════════════════════════════════════════════════════════════════════╝\n\n\
            The ZoneAggregator is the single source of truth for zone state.\n\
            Adapter-specific events (e.g., LmsPlayerStateChanged) do NOT update the aggregator.\n\
            You MUST also emit ZoneUpdated to propagate state changes.\n\n\
            Violations found:\n\n",
        );

        for (file, line, explanation) in &violations {
            error_msg.push_str(&format!("  {}:{}\n", file, line));
            error_msg.push_str(&format!("    {}\n\n", explanation));
        }

        error_msg.push_str(
            "Fix: When emitting adapter-specific events, also emit the corresponding\n\
             aggregator-handled event (e.g., ZoneUpdated for state changes).\n",
        );

        panic!("{}", error_msg);
    }
}

// =============================================================================
// Volume Safety Lint
// =============================================================================

/// SAFETY CRITICAL: These patterns indicate unsafe volume defaults.
/// For dB-based zones (where 0 = max volume), using unwrap_or(0.0) or similar
/// could snap volume to maximum, risking equipment damage.
///
/// Safe patterns:
///   - `if let Some(value) = vol.value { ... }` - only use valid values
///   - `v.value.unwrap_or(min)` - default to minimum (safest)
///
/// Unsafe patterns (flagged):
///   - `vol.value.unwrap_or(0.0)` - 0 = max for dB zones!
///   - `vol.value.unwrap_or(50.0)` - arbitrary default, may be out of range
const UNSAFE_VOLUME_PATTERNS: &[(&str, &str)] = &[
    (
        ".value.unwrap_or(0.0)",
        "DANGEROUS: 0.0 = max volume for dB zones. Use `if let Some(value)` or default to min.",
    ),
    (
        ".value.unwrap_or(50.0)",
        "Unsafe: 50.0 may be out of range for dB zones. Use zone's min value as default.",
    ),
];

#[test]
fn volume_values_use_safe_defaults() {
    // SAFETY CRITICAL: Volume values must not use unsafe defaults.
    //
    // The Roon API can return vol.value = None transiently. Using unwrap_or(0.0)
    // would set dB zones to max volume (0 dB = maximum), risking equipment damage.
    //
    // This test flags unsafe patterns in adapter code.

    let adapters_dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("adapters");

    let mut violations = Vec::new();

    for entry in WalkDir::new(&adapters_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|s| s == "rs").unwrap_or(false))
    {
        let path = entry.path();
        let content = fs::read_to_string(path).expect("Failed to read file");
        let filename = path.file_name().unwrap().to_string_lossy();

        for (pattern, explanation) in UNSAFE_VOLUME_PATTERNS {
            for (line_idx, line) in content.lines().enumerate() {
                if line.contains(pattern) {
                    violations.push((filename.to_string(), line_idx + 1, explanation.to_string()));
                }
            }
        }
    }

    if !violations.is_empty() {
        let mut error_msg = String::from(
            "\n\n\
            ╔══════════════════════════════════════════════════════════════════════════════╗\n\
            ║  SAFETY VIOLATION: Unsafe volume default detected                            ║\n\
            ╚══════════════════════════════════════════════════════════════════════════════╝\n\n\
            Volume values can be None transiently. Using unwrap_or(0.0) or unwrap_or(50.0)\n\
            is DANGEROUS for dB-based zones where 0 dB = MAXIMUM VOLUME.\n\n\
            This could cause volume to snap to max, risking equipment damage.\n\n\
            Violations found:\n\n",
        );

        for (file, line, explanation) in &violations {
            error_msg.push_str(&format!("  {}:{}\n", file, line));
            error_msg.push_str(&format!("    {}\n\n", explanation));
        }

        error_msg.push_str(
            "Fix: Use `if let Some(value) = vol.value { ... }` to only process valid values,\n\
             or default to the zone's min volume: `v.value.unwrap_or(min)`.\n",
        );

        panic!("{}", error_msg);
    }
}
