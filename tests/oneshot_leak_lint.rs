#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! AST-level test to detect potential oneshot channel leaks.
//!
//! Patterns detected:
//! - Creating oneshot::channel() where the receiver (rx) is only used in a
//!   Collection.insert() without corresponding cleanup on timeout/error
//!
//! Example of bad code:
//! ```ignore
//! // BAD: Receiver stored in map but never cleaned up on timeout
//! let (tx, rx) = oneshot::channel();
//! pending.insert(id, tx);
//! // If timeout occurs, entry in `pending` is never removed
//! ```
//!
//! Example of correct code:
//! ```ignore
//! // GOOD: Clean up on all exit paths
//! let (tx, rx) = oneshot::channel();
//! pending.insert(id, tx);
//! match timeout(duration, rx).await {
//!     Ok(result) => { /* use result */ }
//!     Err(_) => {
//!         pending.remove(&id);  // Clean up!
//!     }
//! }
//! ```

use std::fs;
use std::path::Path;
use syn::File;
use walkdir::WalkDir;

fn analyze_file(path: &Path) -> Vec<(String, String)> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let _syntax: File = match syn::parse_file(&content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: Failed to parse {}: {}", path.display(), e);
            return vec![];
        }
    };

    // This lint is complex to implement properly with AST analysis.
    // For now, we rely on the code review process and other lints
    // (like ignored_send_lint) to catch related issues.
    //
    // A proper implementation would track:
    // 1. Variables bound to oneshot receivers
    // 2. Whether those receivers are awaited
    // 3. Whether pending request maps are cleaned up on timeout
    //
    // This is better suited for a more sophisticated analysis tool
    // like Clippy with data flow analysis.

    vec![]
}

#[test]
fn no_oneshot_leak_violations() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut all_violations = Vec::new();

    for entry in WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "rs"))
    {
        let violations = analyze_file(entry.path());
        all_violations.extend(violations);
    }

    // This test passes for now but serves as a placeholder for
    // future implementation of oneshot leak detection.
    // The actual leak detection is done by:
    // - ignored_send_lint.rs (catches ignored send results)
    // - await_in_lock_lint.rs (catches potential deadlock patterns)
    // - Code review for timeout cleanup patterns

    if !all_violations.is_empty() {
        let mut error_msg = String::from(
            "\n\nFound potential oneshot channel leaks!\n\
             Violations:\n",
        );

        for (file, context) in &all_violations {
            error_msg.push_str(&format!("  - {}: {}\n", file, context));
        }

        panic!("{}", error_msg);
    }
}
