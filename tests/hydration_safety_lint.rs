//! AST-level guard for SSR/WASM hydration hazards in Dioxus pages.
//!
//! Hydration must begin from one server-confirmed value. This guard catches two
//! mechanically identifiable regressions without banning legitimate browser
//! work in event handlers:
//!
//! * `initial_*`/`default_*` render helpers reading the browser DOM directly;
//! * paired `cfg(target_arch)` implementations of an `initial_*`/`default_*`
//!   helper, which can silently choose different first-render values on SSR
//!   and WASM.
//!
//! It intentionally does not attempt to prove RSX topology from macro token
//! streams. Async-resource conditionals still require a focused review or a
//! rendered SSR/client comparison; treating every `if let` in `rsx!` as bad
//! would reject legitimate stable `hidden`-gated UI.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use syn::visit::{self, Visit};
use syn::{Attribute, ExprMethodCall, ExprPath, File, ItemFn};

const SETTINGS_FILE: &str = "src/app/pages/settings.rs";

#[derive(Default)]
struct InitializerScan {
    current_function: Option<String>,
    dom_reads: Vec<String>,
    cfg_initializers: BTreeMap<String, BTreeMap<CfgBranch, usize>>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum CfgBranch {
    Wasm,
    NonWasm,
}

impl<'ast> Visit<'ast> for InitializerScan {
    fn visit_item_fn(&mut self, function: &'ast ItemFn) {
        let name = function.sig.ident.to_string();
        let previous = self.current_function.replace(name.clone());

        if is_initializer_name(&name) {
            for branch in cfg_branches(&function.attrs) {
                *self
                    .cfg_initializers
                    .entry(name.clone())
                    .or_default()
                    .entry(branch)
                    .or_default() += 1;
            }
        }

        visit::visit_block(self, &function.block);
        self.current_function = previous;
    }

    fn visit_expr_path(&mut self, path: &'ast ExprPath) {
        if self
            .current_function
            .as_deref()
            .is_some_and(is_initializer_name)
            && path
                .path
                .segments
                .first()
                .is_some_and(|segment| segment.ident == "web_sys")
        {
            self.dom_reads.push(format!(
                "{} references web_sys",
                self.current_function.as_deref().unwrap()
            ));
        }
        visit::visit_expr_path(self, path);
    }

    fn visit_expr_method_call(&mut self, call: &'ast ExprMethodCall) {
        if self
            .current_function
            .as_deref()
            .is_some_and(is_initializer_name)
            && matches!(
                call.method.to_string().as_str(),
                "document" | "query_selector" | "get_element_by_id"
            )
        {
            self.dom_reads.push(format!(
                "{} calls {}()",
                self.current_function.as_deref().unwrap(),
                call.method
            ));
        }
        visit::visit_expr_method_call(self, call);
    }
}

fn is_initializer_name(name: &str) -> bool {
    name.starts_with("initial_") || name.starts_with("default_")
}

fn cfg_branches(attributes: &[Attribute]) -> Vec<CfgBranch> {
    let mut branches = Vec::new();
    for attribute in attributes {
        let syn::Meta::List(list) = &attribute.meta else {
            continue;
        };
        let text = list.tokens.to_string();
        if !text.contains("target_arch") {
            continue;
        }
        if text.contains("wasm32") && !text.contains("not") {
            branches.push(CfgBranch::Wasm);
        } else if text.contains("not") && text.contains("wasm32") {
            branches.push(CfgBranch::NonWasm);
        }
    }
    branches
}

fn scan(source: &str) -> InitializerScan {
    let syntax: File = syn::parse_file(source).expect("hydration guard input must parse");
    let mut scan = InitializerScan::default();
    scan.visit_file(&syntax);
    scan
}

#[test]
fn settings_initializers_do_not_read_route_local_browser_dom() {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(SETTINGS_FILE))
        .expect("read Settings page");
    let scan = scan(&source);

    assert!(
        scan.dom_reads.is_empty(),
        "initial/default Settings helpers must read the app-root SSR snapshot, not route-local browser DOM: {:?}",
        scan.dom_reads
    );
}

#[test]
fn settings_initializers_have_no_cfg_dependent_first_render_value() {
    let source = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(SETTINGS_FILE))
        .expect("read Settings page");
    let scan = scan(&source);
    let divergent: Vec<_> = scan
        .cfg_initializers
        .iter()
        .filter(|(_, branches)| {
            branches.contains_key(&CfgBranch::Wasm) && branches.contains_key(&CfgBranch::NonWasm)
        })
        .map(|(name, _)| name)
        .collect();

    assert!(
        divergent.is_empty(),
        "initial/default helpers must not have separate SSR and WASM implementations: {:?}",
        divergent
    );
}

#[test]
fn fixture_cases_preserve_event_handler_browser_work() {
    let allowed = scan(
        r#"
            fn copy_to_clipboard() {
                let _ = web_sys::window();
            }
            fn start_connection() {
                let _ = web_sys::window().unwrap().document();
            }
        "#,
    );
    assert!(allowed.dom_reads.is_empty());
    assert!(allowed.cfg_initializers.is_empty());
}

#[test]
fn fixture_cases_catch_both_initializer_hazards() {
    let bad = scan(
        r#"
            #[cfg(target_arch = "wasm32")]
            fn initial_snapshot() -> bool {
                web_sys::window().unwrap().document().is_some()
            }
            #[cfg(not(target_arch = "wasm32"))]
            fn initial_snapshot() -> bool { true }
        "#,
    );
    assert!(bad
        .dom_reads
        .iter()
        .any(|finding| finding.contains("initial_snapshot references web_sys")));
    assert!(bad
        .cfg_initializers
        .get("initial_snapshot")
        .is_some_and(|branches| {
            branches.contains_key(&CfgBranch::Wasm) && branches.contains_key(&CfgBranch::NonWasm)
        }));
}
