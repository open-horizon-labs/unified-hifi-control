#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
//! AST-level test to detect `.await` while holding a lock guard.
//!
//! Holding a lock (Mutex, RwLock) across an await point can cause deadlocks
//! because the task may be suspended while still holding the lock, blocking
//! other tasks that need it.
//!
//! Example of bad code:
//! ```ignore
//! // BAD: Lock held across await point
//! let guard = state.write().await;
//! some_async_call().await;  // Deadlock risk!
//! drop(guard);
//! ```
//!
//! Example of correct code:
//! ```ignore
//! // GOOD: Release lock before await
//! let data = {
//!     let guard = state.read().await;
//!     guard.clone()
//! };
//! some_async_call().await;
//! ```

use std::fs;
use std::path::Path;
use syn::visit::Visit;
use syn::{Expr, ExprAwait, File, Local, Pat};
use walkdir::WalkDir;

/// Tracks lock guards and detects awaits while they're held
struct AwaitInLockVisitor {
    current_file: String,
    /// Names of variables that are lock guards
    active_guards: Vec<String>,
    /// Depth tracking for scopes
    scope_depth: usize,
    /// Guard scope depths (guard_name, depth when created)
    guard_scopes: Vec<(String, usize)>,
    violations: Vec<(String, String)>,
}

impl AwaitInLockVisitor {
    fn new(file: String) -> Self {
        Self {
            current_file: file,
            active_guards: Vec::new(),
            scope_depth: 0,
            guard_scopes: Vec::new(),
            violations: Vec::new(),
        }
    }

    /// Check if a method call acquires a lock
    fn is_lock_acquisition(&self, method: &str) -> bool {
        matches!(
            method,
            "lock" | "read" | "write" | "try_lock" | "try_read" | "try_write"
        )
    }

    /// Check if an expression is a lock guard type (heuristic based on method chain)
    fn is_lock_call(&self, expr: &Expr) -> bool {
        if let Expr::Await(await_expr) = expr {
            if let Expr::MethodCall(method_call) = &*await_expr.base {
                return self.is_lock_acquisition(&method_call.method.to_string());
            }
        }
        if let Expr::MethodCall(method_call) = expr {
            return self.is_lock_acquisition(&method_call.method.to_string());
        }
        false
    }
}

impl<'ast> Visit<'ast> for AwaitInLockVisitor {
    fn visit_local(&mut self, local: &'ast Local) {
        // Check if this is a let binding that acquires a lock
        if let Some(init) = &local.init {
            if self.is_lock_call(&init.expr) {
                // Extract variable name
                if let Pat::Ident(pat_ident) = &local.pat {
                    let guard_name = pat_ident.ident.to_string();
                    self.active_guards.push(guard_name.clone());
                    self.guard_scopes.push((guard_name, self.scope_depth));
                }
            }
        }
        syn::visit::visit_local(self, local);
    }

    fn visit_expr_await(&mut self, await_expr: &'ast ExprAwait) {
        // Skip if this is the lock acquisition itself
        if let Expr::MethodCall(method_call) = &*await_expr.base {
            if self.is_lock_acquisition(&method_call.method.to_string()) {
                syn::visit::visit_expr_await(self, await_expr);
                return;
            }
        }

        // Check if we're holding any guards
        if !self.active_guards.is_empty() {
            let guards = self.active_guards.join(", ");
            self.violations.push((
                self.current_file.clone(),
                format!(".await while holding lock guard(s): {}", guards),
            ));
        }

        syn::visit::visit_expr_await(self, await_expr);
    }

    fn visit_block(&mut self, block: &'ast syn::Block) {
        self.scope_depth += 1;
        let _guards_before = self.guard_scopes.len();

        syn::visit::visit_block(self, block);

        // Remove guards that went out of scope
        self.guard_scopes
            .retain(|(_, depth)| *depth < self.scope_depth);
        self.active_guards = self
            .guard_scopes
            .iter()
            .map(|(name, _)| name.clone())
            .collect();

        self.scope_depth -= 1;
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        // Check for explicit drop() calls
        if let Expr::Path(path) = &*call.func {
            if path.path.is_ident("drop") {
                // Remove the dropped guard from active guards
                if let Some(Expr::Path(arg_path)) = call.args.first() {
                    if let Some(ident) = arg_path.path.get_ident() {
                        let name = ident.to_string();
                        self.active_guards.retain(|g| g != &name);
                        self.guard_scopes.retain(|(g, _)| g != &name);
                    }
                }
            }
        }
        syn::visit::visit_expr_call(self, call);
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

    let mut visitor = AwaitInLockVisitor::new(path.display().to_string());
    visitor.visit_file(&syntax);
    visitor.violations
}

#[test]
fn detects_await_in_lock() {
    let bad_code = r#"
        async fn example() {
            let guard = state.write().await;
            some_async_call().await;  // BAD
        }
    "#;

    let syntax: File = syn::parse_file(bad_code).unwrap();
    let mut visitor = AwaitInLockVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert!(
        !visitor.violations.is_empty(),
        "Should detect await while holding lock"
    );
}

#[test]
fn allows_lock_released_before_await() {
    let good_code = r#"
        async fn example() {
            let data = {
                let guard = state.read().await;
                guard.value.clone()
            };
            some_async_call().await;  // GOOD - guard dropped
        }
    "#;

    let syntax: File = syn::parse_file(good_code).unwrap();
    let mut visitor = AwaitInLockVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert!(
        visitor.violations.is_empty(),
        "Should not flag when lock is released before await"
    );
}

#[test]
fn allows_explicit_drop_before_await() {
    let good_code = r#"
        async fn example() {
            let guard = state.write().await;
            let value = guard.value.clone();
            drop(guard);
            some_async_call().await;  // GOOD - explicitly dropped
        }
    "#;

    let syntax: File = syn::parse_file(good_code).unwrap();
    let mut visitor = AwaitInLockVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert!(
        visitor.violations.is_empty(),
        "Should not flag when lock is explicitly dropped before await"
    );
}

/// Allowlist for known-correct patterns where holding locks across awaits is intentional.
/// Format: (file suffix, guard name, reason)
const ALLOWLIST: &[(&str, &str, &str)] = &[
    // HQPlayer protocol requires exclusive connection access during entire command/response cycle.
    // Releasing the lock between send and receive would allow interleaving from other tasks.
    (
        "adapters/hqplayer.rs",
        "conn_guard",
        "Protocol requires exclusive connection access",
    ),
];

fn is_allowed(file: &str, guard: &str) -> bool {
    ALLOWLIST.iter().any(|(file_suffix, allowed_guard, _)| {
        file.ends_with(file_suffix) && guard == *allowed_guard
    })
}

#[test]
fn no_await_in_lock_violations() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut all_violations = Vec::new();

    for entry in WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "rs"))
    {
        let violations = analyze_file(entry.path());
        // Filter out allowed patterns
        let filtered: Vec<_> = violations
            .into_iter()
            .filter(|(file, context)| {
                // Extract guard name from context like ".await while holding lock guard(s): conn_guard"
                if let Some(guards) = context.strip_prefix(".await while holding lock guard(s): ") {
                    !guards.split(", ").all(|g| is_allowed(file, g))
                } else {
                    true
                }
            })
            .collect();
        all_violations.extend(filtered);
    }

    if !all_violations.is_empty() {
        let mut error_msg = String::from(
            "\n\nFound .await while holding lock guard!\n\
             This can cause deadlocks - the task may suspend while holding the lock,\n\
             blocking other tasks that need it.\n\n\
             Fix by releasing the lock before awaiting:\n\
             ```rust\n\
             // BAD:\n\
             let guard = state.write().await;\n\
             async_call().await;  // Deadlock risk!\n\n\
             // GOOD:\n\
             let data = {\n\
                 let guard = state.read().await;\n\
                 guard.clone()\n\
             };\n\
             async_call().await;\n\
             ```\n\n\
             Violations:\n",
        );

        for (file, context) in &all_violations {
            error_msg.push_str(&format!("  - {}: {}\n", file, context));
        }

        panic!("{}", error_msg);
    }
}
