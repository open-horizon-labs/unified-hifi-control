//! AST-level test to detect ignored channel send results.
//!
//! Ignoring the result of channel send operations can hide important errors:
//! - Oneshot sends fail if the receiver is dropped (request cancelled)
//! - Broadcast/mpsc sends fail if all receivers are dropped
//!
//! Example of bad code:
//! ```ignore
//! // BAD: Ignoring send result - won't know if receiver is gone
//! let _ = tx.send(data);
//! ```
//!
//! Example of correct code:
//! ```ignore
//! // GOOD: At least log if send fails
//! if tx.send(data).is_err() {
//!     tracing::debug!("Receiver dropped, request may have been cancelled");
//! }
//!
//! // OR: Propagate the error
//! tx.send(data).map_err(|_| anyhow!("Receiver dropped"))?;
//! ```

use std::fs;
use std::path::Path;
use syn::visit::Visit;
use syn::{Expr, File, Pat, Stmt};
use walkdir::WalkDir;

/// Visitor that detects `let _ = something.send(...)` patterns
struct IgnoredSendVisitor {
    current_file: String,
    violations: Vec<(String, String)>,
}

impl IgnoredSendVisitor {
    fn new(file: String) -> Self {
        Self {
            current_file: file,
            violations: Vec::new(),
        }
    }

    /// Check if a method call is a channel send operation
    fn is_send_call(&self, method: &str) -> bool {
        method == "send"
    }

    /// Check if a pattern is a wildcard (underscore)
    fn is_wildcard_pattern(&self, pat: &Pat) -> bool {
        matches!(pat, Pat::Wild(_))
    }
}

impl<'ast> Visit<'ast> for IgnoredSendVisitor {
    fn visit_stmt(&mut self, stmt: &'ast Stmt) {
        // Look for `let _ = expr` statements
        if let Stmt::Local(local) = stmt {
            if self.is_wildcard_pattern(&local.pat) {
                if let Some(init) = &local.init {
                    // Check if the init expression is a .send() call
                    if let Expr::MethodCall(method_call) = &*init.expr {
                        if self.is_send_call(&method_call.method.to_string()) {
                            self.violations.push((
                                self.current_file.clone(),
                                "let _ = ...send(...) - ignoring send result hides failures"
                                    .to_string(),
                            ));
                        }
                    }
                    // Also check for .send().await (for async channels)
                    if let Expr::Await(await_expr) = &*init.expr {
                        if let Expr::MethodCall(method_call) = &*await_expr.base {
                            if self.is_send_call(&method_call.method.to_string()) {
                                self.violations.push((
                                    self.current_file.clone(),
                                    "let _ = ...send(...).await - ignoring async send result"
                                        .to_string(),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // Also catch bare send statements: `tx.send(x);` or `tx.send(x).await;`
        if let Stmt::Expr(expr, Some(_semi)) = stmt {
            // Check for bare .send() call
            if let Expr::MethodCall(method_call) = expr {
                if self.is_send_call(&method_call.method.to_string()) {
                    self.violations.push((
                        self.current_file.clone(),
                        "...send(...); - bare send statement ignores result".to_string(),
                    ));
                }
            }
            // Check for bare .send().await call
            if let Expr::Await(await_expr) = expr {
                if let Expr::MethodCall(method_call) = &*await_expr.base {
                    if self.is_send_call(&method_call.method.to_string()) {
                        self.violations.push((
                            self.current_file.clone(),
                            "...send(...).await; - bare async send statement ignores result"
                                .to_string(),
                        ));
                    }
                }
            }
        }

        syn::visit::visit_stmt(self, stmt);
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

    let mut visitor = IgnoredSendVisitor::new(path.display().to_string());
    visitor.visit_file(&syntax);
    visitor.violations
}

#[test]
fn detects_ignored_send() {
    let bad_code = r#"
        fn example() {
            let (tx, rx) = oneshot::channel();
            let _ = tx.send(42);  // BAD
        }
    "#;

    let syntax: File = syn::parse_file(bad_code).unwrap();
    let mut visitor = IgnoredSendVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert!(
        !visitor.violations.is_empty(),
        "Should detect ignored send result"
    );
}

#[test]
fn detects_bare_send_statement() {
    let bad_code = r#"
        fn example() {
            let (tx, rx) = oneshot::channel();
            tx.send(42);  // BAD - bare statement
        }
    "#;

    let syntax: File = syn::parse_file(bad_code).unwrap();
    let mut visitor = IgnoredSendVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert!(
        !visitor.violations.is_empty(),
        "Should detect bare send statement"
    );
}

#[test]
fn allows_handled_send() {
    let good_code = r#"
        fn example() {
            let (tx, rx) = oneshot::channel();
            if tx.send(42).is_err() {
                println!("Send failed");
            }
        }
    "#;

    let syntax: File = syn::parse_file(good_code).unwrap();
    let mut visitor = IgnoredSendVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert!(
        visitor.violations.is_empty(),
        "Should not flag when send result is handled"
    );
}

/// Allowlist for known-correct patterns where ignoring send is intentional.
/// Format: (file suffix, reason)
const ALLOWLIST: &[(&str, &str)] = &[
    // Bus publish is fire-and-forget by design - it uses broadcast channels
    // where receivers may come and go, and missing a message is acceptable.
    ("bus/mod.rs", "Broadcast bus is fire-and-forget"),
];

fn is_allowed(file: &str) -> bool {
    ALLOWLIST.iter().any(|(suffix, _)| file.ends_with(suffix))
}

#[test]
fn no_ignored_send_violations() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");

    let mut all_violations = Vec::new();

    for entry in WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "rs"))
    {
        let path = entry.path();
        if is_allowed(&path.display().to_string()) {
            continue;
        }
        let violations = analyze_file(path);
        all_violations.extend(violations);
    }

    if !all_violations.is_empty() {
        let mut error_msg = String::from(
            "\n\nFound ignored channel send results!\n\
             Ignoring send results can hide important failures:\n\
             - Oneshot sends fail if the receiver is dropped\n\
             - This often indicates a cancelled request\n\n\
             Fix by handling the result:\n\
             ```rust\n\
             // BAD:  let _ = tx.send(data);\n\
             // GOOD: if tx.send(data).is_err() { log or handle }\n\
             // OR:   tx.send(data).map_err(|_| anyhow!(\"Receiver dropped\"))?;\n\
             ```\n\n\
             Violations:\n",
        );

        for (file, context) in &all_violations {
            error_msg.push_str(&format!("  - {}: {}\n", file, context));
        }

        panic!("{}", error_msg);
    }
}
