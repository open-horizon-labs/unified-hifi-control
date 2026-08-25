//! AST-level test to forbid self-subscribing signal writes in `use_effect`/`use_memo`
//! (the reactive-loop bug class behind #557 and #560).
//!
//! A Dioxus effect that tracked-reads a signal and then writes that same signal in its
//! own synchronous body subscribes itself to its own write: the write immediately
//! re-triggers the effect, forever. #560's `search_generation()` read + `.set()` in one
//! effect body (src/app/pages/library.rs) is exactly this pattern -- it pegged the wasm
//! main thread at mount. This lint walks `src/app/**` (the Dioxus client code) and flags
//! two patterns, following the repo's existing syn-based AST lints
//! (tests/adapter_boundary_lint.rs, tests/await_in_lock_lint.rs,
//! tests/spawn_cancellation_lint.rs):
//!
//! 1. Inside a `use_effect(move || { .. })` closure's *synchronous* body: any identifier
//!    that is both tracked-read (a bare call-expression `ident()`, or `ident.read()`) and
//!    written (`ident.set(..)`, `ident.write()`, `ident.with_mut(..)`) in that same body.
//!    `ident.peek()` reads are exempt -- that is the sanctioned escape hatch (#560's fix:
//!    `*g.peek() + 1` instead of `g() + 1`), because `peek()` does not subscribe the
//!    effect to the signal.
//! 2. Inside a `use_memo(move || { .. })` closure: any signal write at all. Memos must be
//!    pure projections of their inputs; writing a signal from one is a side effect that
//!    can create the same kind of feedback loop (and violates memo purity regardless).
//!
//! Identifier resolution is name-based within the closure scope, matching the precision
//! level the sibling lints already accept (e.g. `await_in_lock_lint`'s guard tracking).
//!
//! Scope note (pattern 1 only): a `spawn(async move { .. })` block, or a
//! `Closure::wrap(..)`/`Closure::new(..)` wasm-bindgen JS event callback (e.g.
//! `EventSource::onmessage` in `src/app/sse.rs`), nested inside a `use_effect` closure is
//! *not* walked for pattern 1. Both run later -- after the effect's synchronous body has
//! already returned, in response to a future poll or a browser event -- so a read/write
//! inside them doesn't re-trigger the effect synchronously the way a sync self-write
//! does. See the `global_searching`/`search_generation` effect in
//! `src/app/pages/library.rs`, whose spawned block re-reads `search_generation()`
//! (tracked) to detect supersession without looping. A future, weaker heuristic could
//! still flag *unconditional* async writes back to a signal read synchronously by the
//! same effect, but that requires tracking data flow across the spawn/callback boundary
//! and is left as follow-up. `use_memo` closures are not expected to spawn or register JS
//! callbacks (memos must be synchronous/pure), so pattern 2 does not carve out either.
//!
//! Escape hatch: an inline `// reactive-loop-lint: allow <reason>` comment on the same
//! source line as the flagged write suppresses that finding, counted like
//! `await_in_lock_lint`'s `ALLOWLIST` debt entries. The tree starts with **zero** allows:
//! #560's fix (`.peek()`) removed the only known violation, so any allow added later is
//! new debt that must be reviewed, not inherited.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use syn::visit::Visit;
use syn::{Expr, ExprCall, ExprClosure, ExprMethodCall, File};
use walkdir::WalkDir;

/// One captured signal access: the identifier name and the source line it occurred on.
#[derive(Clone, Debug)]
struct Access {
    line: usize,
}

/// Walks a single `use_effect`/`use_memo` closure body and records tracked-reads and
/// writes by identifier name. `.peek()` is intentionally not recorded as a read.
struct SignalUsageVisitor {
    /// When true (pattern 1, `use_effect`), don't descend into `spawn(async move { .. })`
    /// arguments -- async writes/reads don't re-trigger the effect synchronously.
    skip_spawn: bool,
    tracked_reads: HashMap<String, Access>,
    writes: HashMap<String, Access>,
}

impl SignalUsageVisitor {
    fn new(skip_spawn: bool) -> Self {
        Self {
            skip_spawn,
            tracked_reads: HashMap::new(),
            writes: HashMap::new(),
        }
    }

    fn is_spawn_call(call: &ExprCall) -> bool {
        if let Expr::Path(path) = &*call.func {
            return path
                .path
                .segments
                .last()
                .map(|s| s.ident == "spawn")
                .unwrap_or(false);
        }
        false
    }

    /// `Closure::wrap(..)` / `Closure::new(..)` (wasm-bindgen JS event-callback closures,
    /// e.g. `EventSource::onmessage`) run later, in response to a browser event -- not
    /// synchronously as part of the effect's render pass. Same rationale as `spawn`.
    fn is_js_closure_call(call: &ExprCall) -> bool {
        if let Expr::Path(path) = &*call.func {
            let segs: Vec<String> = path
                .path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect();
            if let [.., second_last, last] = segs.as_slice() {
                return second_last == "Closure" && (last == "wrap" || last == "new");
            }
        }
        false
    }
}

impl<'ast> Visit<'ast> for SignalUsageVisitor {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        if self.skip_spawn && (Self::is_spawn_call(call) || Self::is_js_closure_call(call)) {
            // Prune the subtree: don't record signal usage inside spawned async blocks
            // or JS event-callback closures for the synchronous self-trigger check --
            // neither runs as part of the effect's synchronous body.
            return;
        }

        // A bare no-arg call `ident()` is a tracked read of a signal binding.
        if let Expr::Path(path) = &*call.func {
            if call.args.is_empty() {
                if let Some(ident) = path.path.get_ident() {
                    self.tracked_reads
                        .entry(ident.to_string())
                        .or_insert(Access {
                            line: call_line(call),
                        });
                }
            }
        }

        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, method_call: &'ast ExprMethodCall) {
        let method = method_call.method.to_string();
        if let Expr::Path(recv_path) = &*method_call.receiver {
            if let Some(ident) = recv_path.path.get_ident() {
                let name = ident.to_string();
                match method.as_str() {
                    "read" => {
                        self.tracked_reads.entry(name).or_insert(Access {
                            line: method_call_line(method_call),
                        });
                    }
                    "set" | "write" | "with_mut" => {
                        self.writes.entry(name).or_insert(Access {
                            line: method_call_line(method_call),
                        });
                    }
                    // "peek" (and anything else) is not a tracked read -- exempt.
                    _ => {}
                }
            }
        }

        syn::visit::visit_expr_method_call(self, method_call);
    }
}

fn call_line(call: &ExprCall) -> usize {
    use syn::spanned::Spanned;
    call.span().start().line
}

fn method_call_line(call: &ExprMethodCall) -> usize {
    use syn::spanned::Spanned;
    call.span().start().line
}

/// A raw finding before the allow-comment escape hatch is applied.
struct Finding {
    file: String,
    line: usize,
    message: String,
}

/// Walks a whole file looking for `use_effect`/`use_memo` calls and analyzing their
/// closures for self-subscribing reads/writes (pattern 1) or any write at all
/// (pattern 2).
struct ReactiveLoopVisitor {
    current_file: String,
    findings: Vec<Finding>,
}

impl ReactiveLoopVisitor {
    fn new(file: String) -> Self {
        Self {
            current_file: file,
            findings: Vec::new(),
        }
    }

    fn analyze_closure(&mut self, closure: &ExprClosure, kind: HookKind) {
        let mut usage = SignalUsageVisitor::new(matches!(kind, HookKind::Effect));
        usage.visit_expr(&closure.body);

        match kind {
            HookKind::Effect => {
                // Pattern 1: identifier both tracked-read and written in the same body.
                let mut names: Vec<_> = usage.writes.keys().cloned().collect();
                names.sort();
                for name in names {
                    if let Some(read) = usage.tracked_reads.get(&name) {
                        let write = &usage.writes[&name];
                        self.findings.push(Finding {
                            file: self.current_file.clone(),
                            line: write.line,
                            message: format!(
                                "`{name}` is tracked-read (line {}) and written (line {}) in the \
                                 same use_effect body -- this subscribes the effect to its own \
                                 write and re-triggers it. Use `.peek()` to read without \
                                 subscribing.",
                                read.line, write.line
                            ),
                        });
                    }
                }
            }
            HookKind::Memo => {
                // Pattern 2: any signal write at all inside a use_memo closure.
                let mut names: Vec<_> = usage.writes.keys().cloned().collect();
                names.sort();
                for name in names {
                    let write = &usage.writes[&name];
                    self.findings.push(Finding {
                        file: self.current_file.clone(),
                        line: write.line,
                        message: format!(
                            "`{name}` is written inside a use_memo closure -- memos must be pure \
                             projections of their inputs; writing a signal is a side effect."
                        ),
                    });
                }
            }
        }
    }
}

#[derive(Clone, Copy)]
enum HookKind {
    Effect,
    Memo,
}

impl<'ast> Visit<'ast> for ReactiveLoopVisitor {
    fn visit_expr_call(&mut self, call: &'ast ExprCall) {
        let hook = if let Expr::Path(path) = &*call.func {
            path.path.segments.last().and_then(|seg| {
                if seg.ident == "use_effect" {
                    Some(HookKind::Effect)
                } else if seg.ident == "use_memo" {
                    Some(HookKind::Memo)
                } else {
                    None
                }
            })
        } else {
            None
        };

        if let Some(kind) = hook {
            if let Some(Expr::Closure(closure)) = call.args.first() {
                self.analyze_closure(closure, kind);
            }
        }

        syn::visit::visit_expr_call(self, call);
    }
}

/// The line-comment marker that suppresses one finding. Must be on the same source line
/// as the flagged write.
const ALLOW_MARKER: &str = "reactive-loop-lint: allow";

/// Findings surviving the allow-comment escape hatch, plus a count of how many findings
/// were suppressed (the "debt" the module header talks about).
struct AnalysisResult {
    violations: Vec<Finding>,
    allowed_count: usize,
}

fn analyze_source(file_label: &str, content: &str) -> AnalysisResult {
    let syntax: File = match syn::parse_file(content) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("Warning: Failed to parse {file_label}: {e}");
            return AnalysisResult {
                violations: Vec::new(),
                allowed_count: 0,
            };
        }
    };

    let mut visitor = ReactiveLoopVisitor::new(file_label.to_string());
    visitor.visit_file(&syntax);

    let source_lines: Vec<&str> = content.lines().collect();
    let mut violations = Vec::new();
    let mut allowed_count = 0;

    for finding in visitor.findings {
        let line_text = source_lines
            .get(finding.line.saturating_sub(1))
            .copied()
            .unwrap_or("");
        if line_text.contains(ALLOW_MARKER) {
            allowed_count += 1;
        } else {
            violations.push(finding);
        }
    }

    AnalysisResult {
        violations,
        allowed_count,
    }
}

fn analyze_file(path: &Path) -> AnalysisResult {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => {
            return AnalysisResult {
                violations: Vec::new(),
                allowed_count: 0,
            }
        }
    };
    analyze_source(&path.display().to_string(), &content)
}

/// The reviewed baseline of sanctioned `// reactive-loop-lint: allow` comments. Starts
/// empty: #560's fix removed the only known violation, and any new allow comment is new
/// debt that must be reviewed and added here explicitly (mirroring
/// `await_in_lock_lint::ALLOWLIST`).
const EXPECTED_ALLOW_COUNT: usize = 0;

#[test]
fn no_self_subscribing_signal_writes() {
    let src_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("src/app");

    let mut all_violations = Vec::new();
    let mut total_allowed = 0;

    for entry in WalkDir::new(&src_dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map_or(false, |ext| ext == "rs"))
    {
        let result = analyze_file(entry.path());
        total_allowed += result.allowed_count;
        all_violations.extend(result.violations);
    }

    if !all_violations.is_empty() {
        let mut error_msg = String::from(
            "\n\nFound self-subscribing signal write(s) in use_effect/use_memo!\n\
             A synchronous effect that tracked-reads a signal and then writes that same \
             signal re-subscribes itself and re-triggers forever (#560).\n\n\
             Fix by reading with `.peek()` instead of the tracked call/`.read()` when the \
             value isn't meant to drive the effect:\n\
             ```rust\n\
             // BAD:\n\
             use_effect(move || {\n\
                 let next = generation() + 1;  // tracked read\n\
                 generation.set(next);          // re-triggers this effect\n\
             });\n\n\
             // GOOD:\n\
             use_effect(move || {\n\
                 let next = *generation.peek() + 1;  // not tracked\n\
                 generation.set(next);\n\
             });\n\
             ```\n\n\
             Or suppress a reviewed false positive with an inline comment on the write line:\n\
             `generation.set(next); // reactive-loop-lint: allow <reason>`\n\n\
             Violations:\n",
        );

        for finding in &all_violations {
            error_msg.push_str(&format!(
                "  - {}:{}: {}\n",
                finding.file, finding.line, finding.message
            ));
        }

        panic!("{}", error_msg);
    }

    assert_eq!(
        total_allowed, EXPECTED_ALLOW_COUNT,
        "reactive-loop-lint allow-comment count changed ({total_allowed} found, {EXPECTED_ALLOW_COUNT} expected). \
         Every `// reactive-loop-lint: allow` is reviewed debt -- update EXPECTED_ALLOW_COUNT \
         in tests/reactive_loop_lint.rs alongside the review that approves the new suppression."
    );
}

#[test]
fn detects_use_effect_self_subscribing_write() {
    // Reproduces the pre-#564 shape of the library.rs bug: tracked read + write of the
    // same signal identifier in one synchronous use_effect body.
    let bad_code = r#"
        fn Component() {
            use_effect(move || {
                let generation = search_generation() + 1;
                search_generation.set(generation);
            });
        }
    "#;

    let result = analyze_source("test.rs", bad_code);
    assert_eq!(
        result.violations.len(),
        1,
        "Should flag the self-subscribing read+write of search_generation"
    );
}

#[test]
fn allows_peek_read_in_use_effect() {
    // The actual #564 fix: `.peek()` reads the current value without subscribing.
    let good_code = r#"
        fn Component() {
            use_effect(move || {
                let generation = *search_generation.peek() + 1;
                search_generation.set(generation);
            });
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "Should not flag .peek() reads paired with a write"
    );
}

#[test]
fn allows_tracked_read_write_of_different_signals() {
    let good_code = r#"
        fn Component() {
            use_effect(move || {
                let query = search_query();
                results.set(query.len());
            });
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "Should not flag independent signals being read and written"
    );
}

#[test]
fn allows_spawned_async_read_after_sync_write() {
    // Mirrors the real, fixed library.rs shape: the spawned block re-reads the signal
    // (tracked) to detect supersession, but that's off the synchronous effect body, so
    // it must not be flagged.
    let good_code = r#"
        fn Component() {
            use_effect(move || {
                let next = *generation.peek() + 1;
                generation.set(next);
                spawn(async move {
                    if generation() != next {
                        return;
                    }
                });
            });
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "Should not walk into spawn(async move {{ .. }}) for the synchronous self-trigger check"
    );
}

#[test]
fn detects_use_memo_signal_write() {
    let bad_code = r#"
        fn Component() {
            let doubled = use_memo(move || {
                counter.set(0);
                counter() * 2
            });
        }
    "#;

    let result = analyze_source("test.rs", bad_code);
    assert_eq!(
        result.violations.len(),
        1,
        "Should flag any signal write inside a use_memo closure"
    );
}

#[test]
fn allows_pure_use_memo() {
    let good_code = r#"
        fn Component() {
            let doubled = use_memo(move || counter() * 2);
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "Should not flag a pure use_memo with no writes"
    );
}

#[test]
fn allow_comment_suppresses_and_is_counted() {
    let code = r#"
        fn Component() {
            use_effect(move || {
                let generation = search_generation() + 1;
                search_generation.set(generation); // reactive-loop-lint: allow test fixture, not a real violation
            });
        }
    "#;

    let result = analyze_source("test.rs", code);
    assert!(
        result.violations.is_empty(),
        "Allow-commented line should be suppressed from violations"
    );
    assert_eq!(
        result.allowed_count, 1,
        "Suppressed finding should still be counted as reviewed debt"
    );
}
