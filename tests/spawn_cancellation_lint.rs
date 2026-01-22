//! AST-level test to ensure all tokio::spawn loops have proper cancellation handling.
//!
//! This test parses Rust source files and finds patterns like:
//! ```ignore
//! tokio::spawn(async move {
//!     loop {
//!         // ... must have tokio::select! with cancellation check
//!     }
//! });
//! ```
//!
//! Any spawned infinite loop without a cancellation mechanism will cause the process
//! to hang on shutdown (Ctrl+C won't work properly).

use std::fs;
use std::path::Path;
use syn::visit::Visit;
use syn::{Expr, ExprCall, ExprLoop, ExprMacro, ExprMethodCall, File, Macro, StmtMacro};
use walkdir::WalkDir;

/// Tracks spawned async blocks and whether they contain uncancellable loops
struct SpawnLoopVisitor {
    /// Current file being analyzed
    current_file: String,
    /// Stack tracking if we're inside a tokio::spawn
    in_spawn_depth: usize,
    /// Stack tracking if we're inside a loop within a spawn
    in_loop_depth: usize,
    /// Whether we've seen a select! macro in the current loop
    has_select_in_loop: bool,
    /// Violations found: (file, approximate context)
    violations: Vec<(String, String)>,
    /// Track loop starts to give better error context
    loop_context: String,
}

impl SpawnLoopVisitor {
    fn new(file: String) -> Self {
        Self {
            current_file: file,
            in_spawn_depth: 0,
            in_loop_depth: 0,
            has_select_in_loop: false,
            violations: Vec::new(),
            loop_context: String::new(),
        }
    }

    fn is_tokio_spawn_call(&self, call: &ExprCall) -> bool {
        // Check for tokio::spawn(...) pattern
        if let Expr::Path(path) = &*call.func {
            let segments: Vec<_> = path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            return segments == vec!["tokio", "spawn"];
        }
        false
    }

    fn is_joinset_spawn_method(&self, method_call: &ExprMethodCall) -> bool {
        // Check for handles.spawn(...) or joinset.spawn(...) pattern
        // JoinSet::spawn has same cancellation requirements as tokio::spawn
        // Look for method named "spawn" - this catches JoinSet.spawn(), handles.spawn(), etc.
        method_call.method == "spawn"
    }

    fn is_select_macro_path(&self, mac: &Macro) -> bool {
        // Check for tokio::select! or select! pattern
        let path_str: String = mac
            .path
            .segments
            .iter()
            .map(|s| s.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        path_str == "tokio::select" || path_str == "select"
    }
}

impl<'ast> Visit<'ast> for SpawnLoopVisitor {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if self.is_tokio_spawn_call(call) {
            self.in_spawn_depth += 1;
            // Visit the arguments (the async block)
            for arg in &call.args {
                self.visit_expr(arg);
            }
            self.in_spawn_depth -= 1;
        } else {
            // Continue visiting normally
            syn::visit::visit_expr_call(self, call);
        }
    }

    fn visit_expr_method_call(&mut self, method_call: &'ast ExprMethodCall) {
        if self.is_joinset_spawn_method(method_call) {
            self.in_spawn_depth += 1;
            // Visit the arguments (the async block)
            for arg in &method_call.args {
                self.visit_expr(arg);
            }
            self.in_spawn_depth -= 1;
        } else {
            // Continue visiting normally
            syn::visit::visit_expr_method_call(self, method_call);
        }
    }

    fn visit_expr_loop(&mut self, loop_expr: &'ast ExprLoop) {
        if self.in_spawn_depth > 0 {
            // We're in a spawned task and found a loop
            self.in_loop_depth += 1;
            let old_has_select = self.has_select_in_loop;
            self.has_select_in_loop = false;

            // Try to get some context about this loop
            self.loop_context = loop_expr
                .label
                .as_ref()
                .map(|l| format!("'{}", l.name.ident))
                .unwrap_or_else(|| "loop".to_string());

            // Visit the loop body
            syn::visit::visit_expr_loop(self, loop_expr);

            // After visiting, check if we found a select!
            if !self.has_select_in_loop {
                self.violations.push((
                    self.current_file.clone(),
                    format!(
                        "Spawned {} without cancellation handling",
                        self.loop_context
                    ),
                ));
            }

            self.has_select_in_loop = old_has_select;
            self.in_loop_depth -= 1;
        } else {
            syn::visit::visit_expr_loop(self, loop_expr);
        }
    }

    fn visit_expr_macro(&mut self, mac: &'ast ExprMacro) {
        if self.in_loop_depth > 0 && self.is_select_macro_path(&mac.mac) {
            // Found a select! inside a loop - this is good
            self.has_select_in_loop = true;
        }
        syn::visit::visit_expr_macro(self, mac);
    }

    fn visit_stmt_macro(&mut self, mac: &'ast StmtMacro) {
        if self.in_loop_depth > 0 && self.is_select_macro_path(&mac.mac) {
            // Found a select! inside a loop - this is good
            self.has_select_in_loop = true;
        }
        syn::visit::visit_stmt_macro(self, mac);
    }
}

fn analyze_file(path: &Path) -> Vec<(String, String)> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let syntax: File = match syn::parse_file(&content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: Failed to parse {}: {}", path.display(), e);
            return vec![];
        }
    };

    let mut visitor = SpawnLoopVisitor::new(path.display().to_string());
    visitor.visit_file(&syntax);
    visitor.violations
}

#[test]
fn spawned_loops_must_have_cancellation() {
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

    if !all_violations.is_empty() {
        let mut error_msg = String::from(
            "\n\nFound spawned loops without cancellation handling!\n\
             These will prevent graceful shutdown (Ctrl+C will hang).\n\n\
             Fix by adding tokio::select! with a cancellation token:\n\
             ```rust\n\
             loop {\n\
                 tokio::select! {\n\
                     _ = shutdown.cancelled() => break,\n\
                     _ = ticker.tick() => { /* work */ }\n\
                 }\n\
             }\n\
             ```\n\n\
             Violations:\n",
        );

        for (file, context) in &all_violations {
            error_msg.push_str(&format!("  - {}: {}\n", file, context));
        }

        panic!("{}", error_msg);
    }
}
