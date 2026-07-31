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
//! Expressed as: **`get_pipeline_status` acquires no lock of its own.** Every value it projects is
//! either something it read from the daemon in this call or something a helper returned to it
//! already verified. A future edit that reaches back into `self.state` inside the projection is
//! re-opening the window whatever else it does, and this test fails on the shape rather than waiting
//! for a race to be observed.
//!
//! **Label: client-red.** Red at `b7a7a1a`, where the function took the state lock three times after
//! its chain check had already passed.

use std::fs;
use std::path::Path;
use syn::visit::Visit;
use syn::{Expr, ExprMethodCall, File, ImplItemFn};

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

#[derive(Default)]
struct LockVisitor {
    violations: Vec<String>,
}

impl<'ast> Visit<'ast> for LockVisitor {
    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        let method = call.method.to_string();
        if is_lock_acquisition(&method) {
            if let Some(field) = is_own_field(&call.receiver) {
                self.violations
                    .push(format!("self.{field}.{method}()", field = field));
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
        "`{SUBJECT_FN}` takes {} lock(s) of its own: {:?}.\n\nIt must project only what the \
         verification step returned to it. A lock taken inside the projection is a second moment, \
         and the cache it reads at that moment is not provably the cache the `State` being \
         projected was verified against — a concurrent profile load, reconnect, `fresh_*` \
         publication or refresh can have replaced it with a complete, coherent, *different* one. \
         Move the access into a helper that verifies and returns its snapshot atomically.",
        visitor.violations.len(),
        visitor.violations
    );
}
