//! AST-level test: the HQPlayer pipeline projection may not read the cache it is projecting.
//!
//! `get_pipeline_status` joins `State`'s **list indices** to cached chain-scoped enumerations. That
//! join is only meaningful if the lists it resolves through are the ones the verification step
//! proved coherent with that `State`, and the only way to guarantee it is for the verification step
//! to *hand back the snapshot it verified*. A proof that returns a `bool` and leaves the caller to
//! fetch the data again is a check-then-re-read: between the two there is a window, and a profile
//! load, a reconnect, a `fresh_*` publication or a refresh landing inside it replaces the cache with
//! one that is perfectly coherent in itself and has nothing to do with the `State` being projected.
//!
//! A presence re-check cannot close that. "The lists are still there" and "the lists are still the
//! ones I verified" are different questions, and a *complete* replacement answers the first while
//! failing the second.
//!
//! So the structural rule this pins is narrow and mechanical:
//!
//! ```ignore
//! // BAD: proof returns a verdict, projection re-reads the cache
//! if !self.chain_still_matches_cache().await { /* ... */ }
//! let (modes, filters, ..) = {
//!     let cached = self.state.read().await;      // <-- a different moment
//!     (cached.modes.clone(), cached.filters.clone(), ..)
//! };
//!
//! // GOOD: proof returns the exact snapshot it verified, under one lock
//! let snapshot = self.verified_chain_snapshot(identity, &probe).await?;
//! // ... project from `snapshot`, and from nothing else
//! ```
//!
//! Expressed as: **`get_pipeline_status` acquires no data-bearing lock of its own.** Every value it
//! projects is either something it read from the daemon in this call or something a helper returned
//! to it already verified. The endpoint operation lease is deliberately exempt: it carries no
//! projected data and prevents configure/profile mutation from replacing the endpoint while this
//! multi-request read is in flight. A future edit that reaches back into `self.state` inside the
//! projection is re-opening the window whatever else it does, and this test fails on the shape
//! rather than waiting for a race to be observed.
//!
//! **Label: client-red.** Red at `b7a7a1a`, where the function took the state lock three times after
//! its chain check had already passed.

use std::fs;
use std::path::Path;
use syn::visit::Visit;
use syn::{Expr, ExprIf, ExprMethodCall, File, ImplItemFn};

/// The function whose shape is pinned, and the file it lives in.
const SUBJECT_FILE: &str = "src/adapters/hqplayer.rs";
const SUBJECT_FN: &str = "get_pipeline_status";

/// Lock-acquisition method names, matching the ones `await_in_lock_lint` already recognises.
fn is_lock_acquisition(method: &str) -> bool {
    matches!(
        method,
        "lock" | "read" | "write" | "try_lock" | "try_read" | "try_write"
    )
}

/// Whether the receiver is one of the adapter's own shared fields (`self.state`, `self.connection`).
///
/// Deliberately receiver-shaped rather than name-shaped: `snapshot.modes` and a local `Vec::read`
/// are not what this forbids, and a lock taken on something reached through `self` is.
fn is_own_field(receiver: &Expr) -> Option<String> {
    match receiver {
        Expr::Field(field) => match &*field.base {
            Expr::Path(path) if path.path.is_ident("self") => match &field.member {
                syn::Member::Named(name) => Some(name.to_string()),
                syn::Member::Unnamed(_) => None,
            },
            _ => None,
        },
        _ => None,
    }
}

/// The operation lease contains no cache or projection data. It only keeps endpoint identity and
/// persistent mutation stable around the complete read, closing rather than opening the race this
/// lint guards. Every other adapter-owned lock remains forbidden here.
fn is_projection_data_lock(field: &str) -> bool {
    field != "operation_lock"
}

#[derive(Default)]
struct LockVisitor {
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for LockVisitor {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let method = call.method.to_string();
        if is_lock_acquisition(&method) {
            if let Some(field) = is_own_field(&call.receiver) {
                if is_projection_data_lock(&field) {
                    self.violations
                        .push(format!("self.{field}.{method}()", field = field));
                }
            }
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

struct FnFinder<'ast> {
    wanted: &'static str,
    found: Option<&'ast ImplItemFn>,
}

impl<'ast> Visit<'ast> for FnFinder<'ast> {
    fn visit_impl_item_fn(&mut self, item: &'ast ImplItemFn) {
        if item.sig.ident == self.wanted {
            self.found = Some(item);
        }
        syn::visit::visit_impl_item_fn(self, item);
    }
}

#[test]
fn the_pipeline_projection_never_re_reads_the_cache_it_projects() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(SUBJECT_FILE);
    let source = fs::read_to_string(&path).expect("read the HQPlayer adapter");
    let syntax: File = syn::parse_file(&source).expect("parse the HQPlayer adapter");

    let mut finder = FnFinder {
        wanted: SUBJECT_FN,
        found: None,
    };
    finder.visit_file(&syntax);
    let subject = finder.found.unwrap_or_else(|| {
        panic!(
            "{SUBJECT_FILE} must still define `{SUBJECT_FN}`; if it was renamed, this lint has to \
             follow it rather than silently pass"
        )
    });

    let mut visitor = LockVisitor::default();
    visitor.visit_block(&subject.block);

    assert!(
        visitor.violations.is_empty(),
        "`{SUBJECT_FN}` takes {} data-bearing lock(s) of its own: {:?}.\n\nIt must project only what the \
         verification step returned to it. A lock taken inside the projection is a second moment, \
         and the cache it reads at that moment is not provably the cache the `State` being \
         projected was verified against — a concurrent profile load, reconnect, `fresh_*` \
         publication or refresh can have replaced it with a complete, coherent, *different* one. \
         Move the access into a helper that verifies and returns its snapshot atomically.",
        visitor.violations.len(),
        visitor.violations
    );
}

/// The refresh whose result must not gate the re-read, and the re-read it must not gate.
const REFRESH_CALL: &str = "refresh_lists";
const IDENTITY_CALL: &str = "chain_identity";

/// Whether a call to `wanted` appears anywhere inside this expression.
fn calls(expr: &Expr, wanted: &str) -> bool {
    struct Finder<'a> {
        wanted: &'a str,
        found: bool,
    }
    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
            if call.method == self.wanted {
                self.found = true;
            }
            syn::visit::visit_expr_method_call(self, call);
        }
    }
    let mut finder = Finder {
        wanted,
        found: false,
    };
    finder.visit_expr(expr);
    finder.found
}

/// Every `if` in the subject whose **condition** calls `refresh_lists` and whose **branches** call
/// `chain_identity` — the shape that makes the re-read conditional on the refresh having published.
#[derive(Default)]
struct GatedRereadVisitor {
    violations: Vec<String>,
    saw_refresh: bool,
    saw_identity: bool,
}

impl<'ast> Visit<'ast> for GatedRereadVisitor {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if call.method == REFRESH_CALL {
            self.saw_refresh = true;
        }
        if call.method == IDENTITY_CALL {
            self.saw_identity = true;
        }
        syn::visit::visit_expr_method_call(self, call);
    }

    fn visit_expr_if(&mut self, node: &'ast ExprIf) {
        if calls(&node.cond, REFRESH_CALL) {
            let then_rereads = node.then_branch.stmts.iter().any(|stmt| match stmt {
                syn::Stmt::Expr(expr, _) => calls(expr, IDENTITY_CALL),
                syn::Stmt::Local(local) => local
                    .init
                    .as_ref()
                    .is_some_and(|init| calls(&init.expr, IDENTITY_CALL)),
                _ => false,
            });
            let else_rereads = node
                .else_branch
                .as_ref()
                .is_some_and(|(_, expr)| calls(expr, IDENTITY_CALL));
            if then_rereads || else_rereads {
                self.violations.push(format!(
                    "an `if` whose condition calls `{REFRESH_CALL}` guards a `{IDENTITY_CALL}` \
                     re-read"
                ));
            }
        }
        syn::visit::visit_expr_if(self, node);
    }
}

/// **The winner is re-read after *any* refresh attempt, never only after a successful one.**
///
/// `refresh_lists` returns `false` in two situations that look nothing alike. It failed — a family
/// would not answer, or the bracket disagreed — and the cache is as empty as it found it. Or it was
/// **overtaken**: a profile load, a reconnect's refill or another poll's refresh published a
/// complete, coherent set while this one was gathering, and the loser declined to overwrite it. That
/// is the asymmetry `publish_chain_snapshot` deliberately implements, and its whole point is that
/// the cache ends up *filled* — by the winner.
///
/// So a caller that reads the cache only when the refresh returned `true` throws the winner away. It
/// then finds no identity, spends its one retry, and on an unlucky second pass returns
/// "HQPlayer's setting lists could not be read as one coherent set" about a daemon whose lists were
/// sitting complete and current in the cache the whole time. The bug is not in the refresh; it is in
/// reading the refresh's `bool` as "is there a cache" when it means "did *I* fill it".
///
/// Pinned structurally rather than behaviourally: the losing branch requires a second publisher
/// inside `refresh_lists`' gather window, which no sequence of public calls can arrange without a
/// production hook. What can be pinned honestly is that the re-read is not gated on the refresh's
/// verdict.
///
/// **Label: client-red.** Red against the short-circuit
/// `if captured.is_none() && self.refresh_lists().await { captured = self.chain_identity().await; }`.
#[test]
fn the_pipeline_read_re_reads_the_cache_after_any_refresh_attempt() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(SUBJECT_FILE);
    let source = fs::read_to_string(&path).expect("read the HQPlayer adapter");
    let syntax: File = syn::parse_file(&source).expect("parse the HQPlayer adapter");

    let mut finder = FnFinder {
        wanted: SUBJECT_FN,
        found: None,
    };
    finder.visit_file(&syntax);
    let subject = finder.found.unwrap_or_else(|| {
        panic!(
            "{SUBJECT_FILE} must still define `{SUBJECT_FN}`; if it was renamed, this lint has to \
             follow it rather than silently pass"
        )
    });

    let mut visitor = GatedRereadVisitor::default();
    visitor.visit_block(&subject.block);

    assert!(
        visitor.saw_refresh && visitor.saw_identity,
        "`{SUBJECT_FN}` no longer calls both `{REFRESH_CALL}` and `{IDENTITY_CALL}` (refresh: {}, \
         identity: {}). This lint is about how those two relate, so it must not pass by their \
         absence — follow the rename or delete the lint deliberately.",
        visitor.saw_refresh,
        visitor.saw_identity
    );

    assert!(
        visitor.violations.is_empty(),
        "`{SUBJECT_FN}` gates its cache re-read on the refresh's verdict: {:?}.\n\n\
         `{REFRESH_CALL}` returning `false` does not mean the cache is empty. An overtaken refresh \
         returns `false` precisely because a *newer, complete* set won the race and it declined to \
         overwrite it — the cache is filled, by someone else. Re-read `{IDENTITY_CALL}` after every \
         refresh attempt and use whatever is there, rather than discarding a winner and failing the \
         read with \"could not be read as one coherent set\".",
        visitor.violations
    );
}
