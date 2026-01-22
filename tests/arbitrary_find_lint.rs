//! AST-level test to detect `.find(|_| true)` anti-pattern.
//!
//! This pattern indicates code that arbitrarily selects from a collection
//! instead of properly matching on criteria. It can lead to:
//! - Wrong data being returned when multiple items exist
//! - Silent correctness bugs that are hard to debug
//!
//! Example of bad code:
//! ```ignore
//! // BAD: Takes arbitrary pending request, ignoring the key
//! pending_requests.iter().find(|(_, _)| true)
//! ```
//!
//! Example of correct code:
//! ```ignore
//! // GOOD: Matches on the actual key
//! pending_requests.iter().find(|(_, (key, _))| key == &target_key)
//! ```

use std::fs;
use std::path::Path;
use syn::visit::Visit;
use syn::{Expr, ExprClosure, ExprLit, ExprMethodCall, File, Lit};
use walkdir::WalkDir;

/// Visitor that detects `.find(|_| true)` patterns
struct ArbitraryFindVisitor {
    current_file: String,
    violations: Vec<(String, String)>,
}

impl ArbitraryFindVisitor {
    fn new(file: String) -> Self {
        Self {
            current_file: file,
            violations: Vec::new(),
        }
    }

    /// Check if a closure body is just `true` (always matches)
    fn is_always_true_closure(&self, closure: &ExprClosure) -> bool {
        // Check if the closure body is the literal `true`
        if let Expr::Lit(ExprLit {
            lit: Lit::Bool(lit_bool),
            ..
        }) = &*closure.body
        {
            return lit_bool.value;
        }
        false
    }
}

impl<'ast> Visit<'ast> for ArbitraryFindVisitor {
    fn visit_expr_method_call(&mut self, method_call: &'ast ExprMethodCall) {
        // Check for .find(...) or .find_map(...) calls
        let method_name = method_call.method.to_string();
        if method_name == "find" || method_name == "find_map" {
            // Check if the first argument is a closure that always returns true
            if let Some(first_arg) = method_call.args.first() {
                if let Expr::Closure(closure) = first_arg {
                    if self.is_always_true_closure(closure) {
                        // Extract some context about what collection this is on
                        let receiver_hint = match &*method_call.receiver {
                            Expr::MethodCall(inner) => {
                                format!(".{}().{}", inner.method, method_name)
                            }
                            Expr::Field(field) => {
                                if let syn::Member::Named(ident) = &field.member {
                                    format!("{}.{}", ident, method_name)
                                } else {
                                    format!(".{}", method_name)
                                }
                            }
                            _ => format!(".{}", method_name),
                        };

                        self.violations.push((
                            self.current_file.clone(),
                            format!(
                                "{}(|...| true) - arbitrarily selects instead of matching",
                                receiver_hint
                            ),
                        ));
                    }
                }
            }
        }

        // Continue visiting
        syn::visit::visit_expr_method_call(self, method_call);
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

    let mut visitor = ArbitraryFindVisitor::new(path.display().to_string());
    visitor.visit_file(&syntax);
    visitor.violations
}

/// Unit test to verify the lint detects the bad pattern
#[test]
fn detects_find_true_pattern() {
    let bad_code = r#"
        fn example() {
            let items: Vec<(i32, String)> = vec![];
            let _ = items.iter().find(|(_, _)| true);
        }
    "#;

    let syntax: File = syn::parse_file(bad_code).unwrap();
    let mut visitor = ArbitraryFindVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert_eq!(
        visitor.violations.len(),
        1,
        "Should detect one violation in bad code"
    );
    assert!(
        visitor.violations[0].1.contains("true"),
        "Violation should mention the pattern"
    );
}

/// Unit test to verify the lint allows proper find patterns
#[test]
fn allows_proper_find_pattern() {
    let good_code = r#"
        fn example() {
            let items: Vec<(i32, (String, i32))> = vec![];
            let target = "key".to_string();
            let _ = items.iter().find(|(_, (key, _))| key == &target);
        }
    "#;

    let syntax: File = syn::parse_file(good_code).unwrap();
    let mut visitor = ArbitraryFindVisitor::new("test.rs".to_string());
    visitor.visit_file(&syntax);

    assert!(
        visitor.violations.is_empty(),
        "Should not flag proper find patterns"
    );
}

#[test]
fn no_arbitrary_find_patterns() {
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
            "\n\nFound `.find(|_| true)` anti-patterns!\n\
             This arbitrarily selects from a collection instead of matching on criteria.\n\
             With multiple items, this returns the wrong data and breaks correctness.\n\n\
             Fix by matching on the actual key/criteria:\n\
             ```rust\n\
             // BAD:  .find(|(_, _)| true)\n\
             // GOOD: .find(|(_, (key, _))| key == &target_key)\n\
             ```\n\n\
             Violations:\n",
        );

        for (file, context) in &all_violations {
            error_msg.push_str(&format!("  - {}: {}\n", file, context));
        }

        panic!("{}", error_msg);
    }
}
