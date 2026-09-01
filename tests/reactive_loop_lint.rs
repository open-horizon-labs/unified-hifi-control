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
//! 3. Inside a `use_effect`/`use_memo` closure declared in a `#[component]` function
//!    with parameters: any reference to a component prop, or to a top-level `let` local
//!    derived from one (e.g. `let route_tab = tab.clone().unwrap_or_default();`). Props
//!    are plain values, not signals -- a hook closure that captures one has no reactive
//!    dependency on it, so a prop-only change re-renders the component without rerunning
//!    the hook and the hook's output goes stale (#566: the Library page's breadcrumb
//!    resync, armed-zone, selected-source, and tab-reload hooks all had exactly this).
//!    The sanctioned fix is `use_reactive!`: `use_effect(use_reactive!(|prop| { .. }))`
//!    diffs the prop across renders and reruns the hook when it changes. Hooks already
//!    wrapped that way are exempt (the prop enters as a closure *parameter*, not a
//!    capture) -- and the lint parses the `use_reactive!(|p| ..)` closure out of the
//!    macro token stream so patterns 1 and 2 keep applying inside it. Derived-local
//!    tracking is deliberately shallow: top-level `let` bindings only, and a binding
//!    whose initializer calls a `use_*` hook (`use_signal`, `use_memo`, ...) is NOT
//!    treated as prop-derived -- it produces a reactive container, which is the fix,
//!    not the bug.
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

use std::collections::{HashMap, HashSet};
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

/// Records references (pattern 3) to component props / prop-derived locals inside a
/// hook closure body. Unlike [`SignalUsageVisitor`], this deliberately does NOT prune
/// `spawn(..)` blocks: the hazard is "the hook can't be *triggered* by this value", and
/// a prop consumed only inside the spawned block still means the hook's behavior should
/// change when the prop does.
struct PropCaptureVisitor<'a> {
    props: &'a HashSet<String>,
    /// Closure parameters (e.g. the prop re-bound by `use_reactive!(|prop| ..)`) --
    /// exempt, they're fed the diffed value, not captured stale.
    excluded: HashSet<String>,
    captures: HashMap<String, Access>,
}

impl<'ast> Visit<'ast> for PropCaptureVisitor<'_> {
    fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
        if let Some(ident) = path.path.get_ident() {
            let name = ident.to_string();
            if self.props.contains(&name) && !self.excluded.contains(&name) {
                use syn::spanned::Spanned;
                self.captures.entry(name).or_insert(Access {
                    line: path.span().start().line,
                });
            }
        }
        syn::visit::visit_expr_path(self, path);
    }
}

/// Collects every identifier bound by a pattern anywhere in the visited tree --
/// `let` bindings, `for` loops, `if let`/`while let`, match arms, nested-closure
/// parameters. Used to exempt closure-local rebindings from pattern 3.
struct BoundNameCollector {
    names: HashSet<String>,
}

impl<'ast> Visit<'ast> for BoundNameCollector {
    fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
        self.names.insert(pat.ident.to_string());
        syn::visit::visit_pat_ident(self, pat);
    }
}

/// Identifier names bound by a closure's parameter list (simple idents and tuple
/// patterns of idents -- the shapes `use_reactive!` and plain hook closures produce).
fn closure_param_names(closure: &ExprClosure) -> HashSet<String> {
    let mut names = HashSet::new();
    for input in &closure.inputs {
        collect_pat_idents(input, &mut names);
    }
    names
}

fn collect_pat_idents(pat: &syn::Pat, out: &mut HashSet<String>) {
    match pat {
        syn::Pat::Ident(pi) => {
            out.insert(pi.ident.to_string());
        }
        syn::Pat::Type(pt) => collect_pat_idents(&pt.pat, out),
        syn::Pat::Tuple(t) => {
            for elem in &t.elems {
                collect_pat_idents(elem, out);
            }
        }
        _ => {}
    }
}

/// Whether an expression contains a call to any `use_*` hook. A `let` binding
/// initialized through one (`let x = use_signal(|| prop.clone());`) yields a reactive
/// container, not a stale prop alias, so pattern 3 must not propagate prop-ness into it.
fn contains_hook_call(expr: &Expr) -> bool {
    struct HookCallFinder {
        found: bool,
    }
    impl<'ast> Visit<'ast> for HookCallFinder {
        fn visit_expr_call(&mut self, call: &'ast ExprCall) {
            if let Expr::Path(path) = &*call.func {
                if let Some(seg) = path.path.segments.last() {
                    if seg.ident.to_string().starts_with("use_") {
                        self.found = true;
                    }
                }
            }
            syn::visit::visit_expr_call(self, call);
        }
    }
    let mut finder = HookCallFinder { found: false };
    finder.visit_expr(expr);
    finder.found
}

/// Whether an expression references any identifier in `names` (as a bare path).
fn references_any(expr: &Expr, names: &HashSet<String>) -> bool {
    struct RefFinder<'a> {
        names: &'a HashSet<String>,
        found: bool,
    }
    impl<'ast> Visit<'ast> for RefFinder<'_> {
        fn visit_expr_path(&mut self, path: &'ast syn::ExprPath) {
            if let Some(ident) = path.path.get_ident() {
                if self.names.contains(&ident.to_string()) {
                    self.found = true;
                }
            }
            syn::visit::visit_expr_path(self, path);
        }
    }
    let mut finder = RefFinder {
        names,
        found: false,
    };
    finder.visit_expr(expr);
    finder.found
}

/// Walks a whole file looking for `use_effect`/`use_memo` calls and analyzing their
/// closures for self-subscribing reads/writes (pattern 1) or any write at all
/// (pattern 2).
struct ReactiveLoopVisitor {
    current_file: String,
    findings: Vec<Finding>,
    /// Props (and top-level prop-derived locals) of the enclosing `#[component]`
    /// function, when there is one with parameters. Drives pattern 3.
    component_props: Option<HashSet<String>>,
}

impl ReactiveLoopVisitor {
    fn new(file: String) -> Self {
        Self {
            current_file: file,
            findings: Vec::new(),
            component_props: None,
        }
    }

    fn analyze_closure(&mut self, closure: &ExprClosure, kind: HookKind) {
        let mut usage = SignalUsageVisitor::new(matches!(kind, HookKind::Effect));
        usage.visit_expr(&closure.body);

        // Pattern 3: component prop (or prop-derived local) captured by the hook
        // closure. Closure parameters are exempt -- `use_reactive!(|prop| ..)` re-binds
        // the prop as a parameter fed from the diffing signal, which is the fix. So is
        // any name the closure body re-binds itself (a `let`, `for`, `if let`, match
        // arm, or nested-closure pattern): `for zone in list` inside the closure is the
        // loop's local, not the `zone` prop. The exclusion is body-wide rather than
        // properly scoped, so a closure that BOTH captures a prop AND rebinds that same
        // name elsewhere in its body would mask the capture -- accepted imprecision,
        // consistent with the sibling lints' name-based resolution.
        if let Some(props) = &self.component_props {
            let mut excluded = closure_param_names(closure);
            let mut bound = BoundNameCollector {
                names: HashSet::new(),
            };
            bound.visit_expr(&closure.body);
            excluded.extend(bound.names);
            let mut capture = PropCaptureVisitor {
                props,
                excluded,
                captures: HashMap::new(),
            };
            capture.visit_expr(&closure.body);
            let mut names: Vec<_> = capture.captures.keys().cloned().collect();
            names.sort();
            for name in names {
                let access = &capture.captures[&name];
                self.findings.push(Finding {
                    file: self.current_file.clone(),
                    line: access.line,
                    message: format!(
                        "`{name}` is a component prop (or a local derived from one) \
                         captured by a {} closure (line {}). Props are plain values -- \
                         the hook has no reactive dependency on them, so a prop-only \
                         change re-renders the component without rerunning the hook and \
                         its output goes stale (#566). Wrap the closure in \
                         `use_reactive!(|{name}| ..)`.",
                        match kind {
                            HookKind::Effect => "use_effect",
                            HookKind::Memo => "use_memo",
                        },
                        access.line
                    ),
                });
            }
        }

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
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let is_component = item.attrs.iter().any(|attr| {
            attr.path()
                .segments
                .last()
                .map(|s| s.ident == "component")
                .unwrap_or(false)
        });

        let saved = self.component_props.take();
        if is_component {
            let mut props: HashSet<String> = item
                .sig
                .inputs
                .iter()
                .filter_map(|arg| match arg {
                    syn::FnArg::Typed(pt) => match &*pt.pat {
                        syn::Pat::Ident(pi) => Some(pi.ident.to_string()),
                        _ => None,
                    },
                    _ => None,
                })
                .collect();

            if !props.is_empty() {
                // Propagate prop-ness through top-level `let` aliases, in statement
                // order (`let route_tab = tab.clone().unwrap_or_default();` makes
                // `route_tab` a prop for pattern 3). Bindings initialized through a
                // `use_*` hook are containers, not aliases -- see the module header.
                for stmt in &item.block.stmts {
                    if let syn::Stmt::Local(local) = stmt {
                        let mut bound = HashSet::new();
                        collect_pat_idents(&local.pat, &mut bound);
                        if let Some(init) = &local.init {
                            if !contains_hook_call(&init.expr) && references_any(&init.expr, &props)
                            {
                                props.extend(bound);
                            }
                        }
                    }
                }
                self.component_props = Some(props);
            }
        }

        syn::visit::visit_item_fn(self, item);
        self.component_props = saved;
    }

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
            match call.args.first() {
                Some(Expr::Closure(closure)) => self.analyze_closure(closure, kind),
                // `use_effect(use_reactive!(|prop| { .. }))`: the macro's token stream
                // is itself closure-shaped -- parse it back out so patterns 1 and 2
                // still see the body, and pattern 3 sees `prop` as a parameter (exempt).
                Some(Expr::Macro(mac))
                    if mac
                        .mac
                        .path
                        .segments
                        .last()
                        .map(|s| s.ident == "use_reactive")
                        .unwrap_or(false) =>
                {
                    if let Ok(closure) = syn::parse2::<ExprClosure>(mac.mac.tokens.clone()) {
                        self.analyze_closure(&closure, kind);
                    }
                }
                _ => {}
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
            "\n\nFound reactive-dependency hazard(s) in use_effect/use_memo!\n\
             Two bug classes are linted here: (a) a synchronous effect that tracked-reads \
             a signal and then writes that same signal re-subscribes itself and \
             re-triggers forever (#560); (b) a hook closure in a #[component] fn that \
             captures a component prop (or a local derived from one) has no reactive \
             dependency on it, so a prop-only change re-renders the component without \
             rerunning the hook and its output goes stale (#566) -- wrap the closure in \
             `use_reactive!(|prop| ..)`.\n\n\
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
fn detects_prop_capture_in_component_effect() {
    // The #566 hazard class: a route prop (here via its conventional local alias)
    // captured by a use_effect closure with no reactive dependency on it.
    let bad_code = r#"
        #[component]
        fn Library(tab: Option<String>) -> Element {
            let route_tab = tab.clone().unwrap_or_default();
            use_effect(move || {
                let _ = route_tab.clone();
                refresh(false);
            });
            rsx! {}
        }
    "#;

    let result = analyze_source("test.rs", bad_code);
    assert_eq!(
        result.violations.len(),
        1,
        "Should flag the untracked prop-alias capture: {:?}",
        result
            .violations
            .iter()
            .map(|f| &f.message)
            .collect::<Vec<_>>()
    );
    assert!(result.violations[0].message.contains("route_tab"));
}

#[test]
fn detects_prop_capture_in_component_memo() {
    let bad_code = r#"
        #[component]
        fn Library(source: Option<String>) -> Element {
            let selected = use_memo(move || resolve_source(source.as_deref()));
            rsx! {}
        }
    "#;

    let result = analyze_source("test.rs", bad_code);
    assert_eq!(
        result.violations.len(),
        1,
        "Should flag the prop captured by a use_memo closure"
    );
}

#[test]
fn allows_use_reactive_wrapped_prop() {
    // The sanctioned fix: the prop enters as a use_reactive! closure parameter.
    let good_code = r#"
        #[component]
        fn Library(tab: Option<String>) -> Element {
            let route_tab = tab.clone().unwrap_or_default();
            use_effect(use_reactive!(|route_tab| {
                let _ = route_tab;
                refresh(false);
            }));
            let selected = use_memo(use_reactive!(|route_tab| resolve_source(&route_tab)));
            rsx! {}
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "use_reactive!-wrapped hooks must not be flagged: {:?}",
        result
            .violations
            .iter()
            .map(|f| &f.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn detects_self_subscribing_write_inside_use_reactive() {
    // Pattern 1 must keep applying inside a use_reactive!-wrapped effect body.
    let bad_code = r#"
        #[component]
        fn Library(tab: Option<String>) -> Element {
            use_effect(use_reactive!(|tab| {
                let next = generation() + 1;
                generation.set(next);
            }));
            rsx! {}
        }
    "#;

    let result = analyze_source("test.rs", bad_code);
    assert_eq!(
        result.violations.len(),
        1,
        "Pattern 1 should still fire inside the use_reactive! closure body"
    );
    assert!(result.violations[0].message.contains("generation"));
}

#[test]
fn allows_signal_derived_from_prop() {
    // `use_signal(|| prop.clone())` deliberately seeds a signal from a prop -- the
    // signal is a reactive container, not a stale alias, so effects tracking it are
    // fine and `armed` must not inherit prop-ness.
    let good_code = r#"
        #[component]
        fn Library(zone: Option<String>) -> Element {
            let armed = use_signal(|| zone.clone());
            use_effect(move || {
                let _ = armed();
            });
            rsx! {}
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "A use_signal-seeded binding is not a prop alias"
    );
}

#[test]
fn allows_closure_local_shadowing_of_prop_names() {
    // `zone` here is the for-loop's local (and the nested closure's param), not the
    // component prop -- the real library.rs effects iterate `for zone in list` inside
    // components whose route prop is also named `zone`.
    let good_code = r#"
        #[component]
        fn Library(zone: Option<String>, source: Option<String>) -> Element {
            use_effect(move || {
                let list = zones_list();
                for zone in list {
                    push(zone.zone_id);
                }
                if let Some(zone) = resolved() {
                    push(zone);
                }
            });
            let picked = use_memo(move || {
                let source = selected_source();
                zones_list().into_iter().find(|z| z.source == source)
            });
            rsx! {}
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "Closure-local rebindings of prop names must not be flagged: {:?}",
        result
            .violations
            .iter()
            .map(|f| &f.message)
            .collect::<Vec<_>>()
    );
}

#[test]
fn ignores_prop_like_captures_outside_component_fns() {
    // Pattern 3 is scoped to #[component] functions: plain helpers and hook fns have
    // arguments, not reactive props.
    let good_code = r#"
        fn use_thing(flag: bool) {
            use_effect(move || {
                let _ = flag;
            });
        }
    "#;

    let result = analyze_source("test.rs", good_code);
    assert!(
        result.violations.is_empty(),
        "Non-#[component] fns are out of pattern 3's scope"
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
