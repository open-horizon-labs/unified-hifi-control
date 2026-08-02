//! Deterministic architecture boundary for production surfaces.
//!
//! `docs/ARCHITECTURE.md` makes the in-app bus the adapter communication boundary and the
//! `ZoneAggregator` the readable state authority. This test uses Rust syntax, not formatting or
//! handler-name heuristics, to inventory every concrete adapter dependency that still leaks into
//! production application modules. The temporary debt baseline is exact: new bypasses and
//! accidental reintroduction fail CI, while each migration removes a stable entry until the
//! baseline is empty.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprField, FnArg, ImplItemFn, ItemFn, ItemImpl, ItemMod, ItemStruct, ItemUse,
    Member, Pat, Type, UseTree,
};
use walkdir::WalkDir;

/// Concrete adapters may exist only in their implementation, lifecycle/state infrastructure, and
/// the binary composition root. Every other production module is inside the enforced boundary.
const SANCTIONED_PATHS: &[&str] = &[
    "src/adapters/",
    "src/bus/",
    "src/aggregator.rs",
    "src/coordinator.rs",
    "src/main.rs",
    "src/bin/",
];

const FORBIDDEN_APP_STATE_FIELDS: &[&str] = &[
    "roon",
    "hqplayer",
    "hqp_instances",
    "lms",
    "openhome",
    "upnp",
    "startable_adapters",
];

const CONCRETE_ADAPTER_TYPES: &[&str] = &[
    "RoonAdapter",
    "HqpAdapter",
    "HqpInstanceManager",
    "LmsAdapter",
    "OpenHomeAdapter",
    "UPnPAdapter",
    "Startable",
];

/// Exact migration debt on `origin/v3` when #436 was opened. Entries are stable syntax identities,
/// not line numbers. Do not add entries: new bypasses must use the bus/aggregator. Remove an entry
/// in the same change that removes the corresponding dependency. #436 completes at an empty list.
const EXPECTED_DEBT: &str = include_str!("fixtures/adapter_boundary_debt.txt");

#[derive(Default)]
struct BoundaryVisitor {
    relative_path: String,
    current_item: String,
    app_state_bindings: BTreeSet<String>,
    in_app_state_impl: bool,
    findings: BTreeSet<String>,
}

impl BoundaryVisitor {
    fn finding(&mut self, kind: &str, symbol: &str) {
        self.findings.insert(format!(
            "{}|{}|{}|{}",
            self.relative_path, self.current_item, kind, symbol
        ));
    }

    fn with_function_context<F>(
        &mut self,
        name: String,
        inputs: &syn::punctuated::Punctuated<FnArg, syn::token::Comma>,
        visit: F,
    ) where
        F: FnOnce(&mut Self),
    {
        let previous_item = std::mem::replace(&mut self.current_item, name);
        let previous_bindings = std::mem::take(&mut self.app_state_bindings);

        for input in inputs {
            if let FnArg::Typed(argument) = input {
                if type_mentions(argument.ty.as_ref(), "AppState") {
                    collect_pat_idents(argument.pat.as_ref(), &mut self.app_state_bindings);
                }
            }
        }

        visit(self);
        self.current_item = previous_item;
        self.app_state_bindings = previous_bindings;
    }
}

impl<'ast> Visit<'ast> for BoundaryVisitor {
    fn visit_item_struct(&mut self, node: &'ast ItemStruct) {
        let previous_item = std::mem::replace(&mut self.current_item, node.ident.to_string());
        if node.ident == "AppState" {
            for field in &node.fields {
                let Some(ident) = field.ident.as_ref() else {
                    continue;
                };
                let name = ident.to_string();
                if FORBIDDEN_APP_STATE_FIELDS.contains(&name.as_str()) {
                    self.finding("app-state-field", &name);
                }
            }
        }
        visit::visit_item_struct(self, node);
        self.current_item = previous_item;
    }

    fn visit_item_mod(&mut self, node: &'ast ItemMod) {
        if has_cfg_test(&node.attrs) {
            return;
        }
        visit::visit_item_mod(self, node);
    }

    fn visit_item_use(&mut self, node: &'ast ItemUse) {
        let mut names = BTreeSet::new();
        collect_use_names(&node.tree, &mut names);
        let previous_item = std::mem::replace(&mut self.current_item, "<module-use>".to_string());
        for concrete in CONCRETE_ADAPTER_TYPES {
            if names.contains(*concrete) {
                self.finding("concrete-adapter-import", concrete);
            }
        }
        self.current_item = previous_item;
        visit::visit_item_use(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        let previous = self.in_app_state_impl;
        self.in_app_state_impl = type_mentions(node.self_ty.as_ref(), "AppState");
        visit::visit_item_impl(self, node);
        self.in_app_state_impl = previous;
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        self.with_function_context(node.sig.ident.to_string(), &node.sig.inputs, |this| {
            for input in &node.sig.inputs {
                this.visit_fn_arg(input);
            }
            this.visit_return_type(&node.sig.output);
            visit::visit_block(this, &node.block);
        });
    }

    fn visit_impl_item_fn(&mut self, node: &'ast ImplItemFn) {
        self.with_function_context(node.sig.ident.to_string(), &node.sig.inputs, |this| {
            for input in &node.sig.inputs {
                this.visit_fn_arg(input);
            }
            this.visit_return_type(&node.sig.output);
            visit::visit_block(this, &node.block);
        });
    }

    fn visit_expr_field(&mut self, node: &'ast ExprField) {
        let Member::Named(member) = &node.member else {
            visit::visit_expr_field(self, node);
            return;
        };
        let field = member.to_string();
        if FORBIDDEN_APP_STATE_FIELDS.contains(&field.as_str()) {
            if let Some(root) = expression_root_ident(node.base.as_ref()) {
                if self.app_state_bindings.contains(&root)
                    || (self.in_app_state_impl && root == "self")
                {
                    self.finding("direct-field-access", &field);
                }
            }
        }
        visit::visit_expr_field(self, node);
    }

    fn visit_type(&mut self, node: &'ast Type) {
        for concrete in CONCRETE_ADAPTER_TYPES {
            if type_mentions(node, concrete) {
                self.finding("concrete-adapter-type", concrete);
            }
        }
        visit::visit_type(self, node);
    }
}

fn has_cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attribute| {
        let syn::Meta::List(list) = &attribute.meta else {
            return false;
        };
        attribute.path().is_ident("cfg")
            && list
                .tokens
                .to_string()
                .split(|character: char| !character.is_alphanumeric() && character != '_')
                .any(|token| token == "test")
    })
}

fn collect_use_names(tree: &UseTree, names: &mut BTreeSet<String>) {
    match tree {
        UseTree::Path(path) => {
            names.insert(path.ident.to_string());
            collect_use_names(path.tree.as_ref(), names);
        }
        UseTree::Name(name) => {
            names.insert(name.ident.to_string());
        }
        UseTree::Rename(rename) => {
            names.insert(rename.ident.to_string());
            names.insert(rename.rename.to_string());
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_use_names(item, names);
            }
        }
        UseTree::Glob(_) => {}
    }
}

fn type_mentions(ty: &Type, expected: &str) -> bool {
    struct TypeNameVisitor<'a> {
        expected: &'a str,
        found: bool,
    }
    impl<'ast> Visit<'ast> for TypeNameVisitor<'_> {
        fn visit_path_segment(&mut self, segment: &'ast syn::PathSegment) {
            if segment.ident == self.expected {
                self.found = true;
            }
            visit::visit_path_segment(self, segment);
        }
    }

    let mut visitor = TypeNameVisitor {
        expected,
        found: false,
    };
    visitor.visit_type(ty);
    visitor.found
}

fn collect_pat_idents(pat: &Pat, output: &mut BTreeSet<String>) {
    struct PatVisitor<'a>(&'a mut BTreeSet<String>);
    impl<'ast> Visit<'ast> for PatVisitor<'_> {
        fn visit_pat_ident(&mut self, node: &'ast syn::PatIdent) {
            self.0.insert(node.ident.to_string());
            visit::visit_pat_ident(self, node);
        }
    }
    PatVisitor(output).visit_pat(pat);
}

fn expression_root_ident(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Path(path) if path.path.segments.len() == 1 => path
            .path
            .segments
            .first()
            .map(|segment| segment.ident.to_string()),
        Expr::Field(field) => expression_root_ident(field.base.as_ref()),
        Expr::MethodCall(call) => expression_root_ident(call.receiver.as_ref()),
        Expr::Paren(paren) => expression_root_ident(paren.expr.as_ref()),
        Expr::Reference(reference) => expression_root_ident(reference.expr.as_ref()),
        _ => None,
    }
}

fn analyze_source(relative_path: &str, source: &str) -> BTreeSet<String> {
    let syntax = syn::parse_file(source)
        .unwrap_or_else(|error| panic!("failed to parse {relative_path}: {error}"));
    let mut visitor = BoundaryVisitor {
        relative_path: relative_path.to_string(),
        ..BoundaryVisitor::default()
    };
    visitor.visit_file(&syntax);
    visitor.findings
}

fn production_sources(manifest_dir: &Path) -> Vec<PathBuf> {
    let mut sources = Vec::new();
    for entry in WalkDir::new(manifest_dir.join("src"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "rs"))
    {
        let relative = entry
            .path()
            .strip_prefix(manifest_dir)
            .expect("source below manifest")
            .to_string_lossy()
            .replace('\\', "/");
        if is_sanctioned_path(&relative) {
            continue;
        }
        sources.push(entry.into_path());
    }
    sources.sort();
    sources
}

fn is_sanctioned_path(relative: &str) -> bool {
    SANCTIONED_PATHS.iter().any(|sanctioned| {
        relative == sanctioned.trim_end_matches('/') || relative.starts_with(sanctioned)
    })
}

fn current_debt() -> BTreeSet<String> {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut findings = BTreeSet::new();
    for path in production_sources(manifest_dir) {
        let relative = path
            .strip_prefix(manifest_dir)
            .expect("surface path below manifest")
            .to_string_lossy()
            .replace('\\', "/");
        let source = fs::read_to_string(&path).expect("read production surface");
        findings.extend(analyze_source(&relative, &source));
    }
    findings
}

#[test]
fn production_surfaces_match_the_exact_migration_debt() {
    let actual = current_debt();
    let expected = EXPECTED_DEBT
        .lines()
        .filter(|entry| !entry.trim().is_empty() && !entry.starts_with('#'))
        .map(str::to_string)
        .collect::<BTreeSet<_>>();

    if actual != expected {
        let added = actual.difference(&expected).cloned().collect::<Vec<_>>();
        let removed = expected.difference(&actual).cloned().collect::<Vec<_>>();
        panic!(
            "adapter-boundary debt changed. New entries are forbidden; removed entries must be \
             deleted from EXPECTED_DEBT in the same change\n\nNEW (forbidden):\n{}\n\nREMOVED \
             (delete these baseline entries):\n{}",
            added.join("\n"),
            removed.join("\n")
        );
    }
}

#[test]
fn syntax_lint_finds_aliases_nested_access_and_concrete_types() {
    let findings = analyze_source(
        "src/mcp/example.rs",
        r#"
            use crate::adapters::RoonAdapter;
            async fn handler(State(app): State<AppState>) {
                let cloned = (&app.roon).clone();
                cloned.control("roon:1", "play").await;
            }
            fn concrete(_: Arc<RoonAdapter>) {}
        "#,
    );

    assert!(findings.contains("src/mcp/example.rs|handler|direct-field-access|roon"));
    assert!(
        findings.contains("src/mcp/example.rs|<module-use>|concrete-adapter-import|RoonAdapter")
    );
    assert!(findings.contains("src/mcp/example.rs|concrete|concrete-adapter-type|RoonAdapter"));
}

#[test]
fn syntax_lint_allows_aggregator_and_bus_access() {
    let findings = analyze_source(
        "src/api/example.rs",
        r#"
            async fn handler(State(state): State<AppState>) {
                let zone = state.aggregator.get_zone("roon:1").await;
                state.bus.publish(command_for(zone));
            }
        "#,
    );
    assert!(
        findings.is_empty(),
        "sanctioned boundary produced {findings:?}"
    );
}

#[test]
fn only_native_infrastructure_and_composition_are_path_exempt() {
    for allowed in [
        "src/adapters/roon.rs",
        "src/bus/events.rs",
        "src/aggregator.rs",
        "src/coordinator.rs",
        "src/main.rs",
        "src/bin/protocol_checker.rs",
    ] {
        assert!(is_sanctioned_path(allowed), "{allowed} must be sanctioned");
    }

    for enforced in [
        "src/api/mod.rs",
        "src/knobs/routes.rs",
        "src/mcp/tools/transport.rs",
        "src/services/new_backend_service.rs",
        "src/lib.rs",
    ] {
        assert!(
            !is_sanctioned_path(enforced),
            "{enforced} must stay inside the enforced boundary"
        );
    }
}
