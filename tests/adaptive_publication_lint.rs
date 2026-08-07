//! Boundary lints for adaptive producer publication (issue #324).
//!
//! #324 publishes producer documents to **in-repository consumers only**. Two boundaries
//! keep that true, and neither is visible from a passing build:
//!
//! 1. **The public bus cannot carry adaptive data.** `src/api/mod.rs` serializes every
//!    `BusEvent` verbatim into `GET /events`, which `docs/ARCHITECTURE.md` documents as
//!    consumable by any HTTP client including ESP32 firmware. A variant carrying a producer
//!    document would be a response-schema change to a public endpoint *and* publication of
//!    the v1 contract outside this repository.
//! 2. **No surface reaches the contract or the publication layer.** `ProducerDocument`
//!    derives `Serialize`, so one `Json(snapshot)` in a handler re-exports the contract.
//!
//! ## Why this file parses instead of scanning
//!
//! It used to scan text. Over eight commits that scanner was escaped **seven** times, every
//! time in the direction that reports success:
//!
//! | Escape | Found by |
//! |---|---|
//! | `#[derive(Serialize)]` stacked above the innocent derive — only the nearest was read | CodeRabbit |
//! | an attribute wrapped across lines — the continuation line cleared it | CodeRabbit |
//! | `use serde::Serialize as EventWire;` — no literal `Serialize` anywhere | CodeRabbit |
//! | a crate-local `pub use` re-export — no literal `serde` anywhere | CodeRabbit |
//! | `r##"… " … ] …"##` — the lexer left string mode at the embedded quote | CodeRabbit |
//! | `pub use` — the statement-boundary rule rejected exactly that one form | Codex |
//! | `'\''` — the char lexer stopped at the escaped apostrophe | Codex |
//! | `#[allow(unused_imports)] use …;` — an attribute is not a statement boundary | Codex |
//!
//! The escape rate was not falling, and each fix was another special case in what had
//! become an incrementally hand-written Rust lexer living in a test file. The pattern is
//! the signal: **a text scanner approximates a parser, and every approximation has a
//! boundary that an adversary finds before its author does.**
//!
//! So this file uses `syn::parse_file`. `syn` is already a direct dev-dependency with
//! `features = ["full", "parsing", "visit"]`, and six sibling lints
//! (`await_in_lock_lint`, `spawn_cancellation_lint`, `ignored_send_lint`,
//! `oneshot_leak_lint`, `arbitrary_find_lint`, `unbounded_channel_lint`) already parse this
//! way — so precedent and dependency both cost nothing.
//!
//! Against an AST every escape above stops being *detected* and becomes
//! *unrepresentable*: attributes are a `Vec<Attribute>` on the item however they were
//! formatted, `UseTree::Rename` is a variant rather than a spelling, visibility and
//! attributes are fields of `ItemUse` rather than characters preceding it, and lexing raw
//! strings and char literals correctly is the parser's job by definition. The whole ad-hoc
//! lexer — `code_only`, `join_wrapped_attributes`, `attributes_in`, `string_literal_end`,
//! `char_literal_end`, `imports_in`, `derive_tokens` — is **deleted** rather than kept
//! alongside, because keeping both would claim a joint sufficiency neither has.
//!
//! ## What parsing does not buy
//!
//! `syn` resolves no names. `use crate::wire::EventWire;` is an import of *something*; that
//! it is `serde::Serialize` re-exported lives in another file. That is exactly why the
//! guarantee is an **allowlist** rather than a detector: parsing makes enumeration exact,
//! and the allowlist decides without needing to know what a name means. The two compose;
//! neither is sufficient alone.
//!
//! ## Macros, which parsing alone does not close either
//!
//! An attribute macro (`#[event_wire] pub enum AdaptiveEvent {}`) and an item-position
//! macro (`make_event_wire!(AdaptiveEvent);`) each generate code while leaving imports,
//! derives and `ItemImpl` entirely clean. An earlier commit recorded that as an accepted
//! residual; Codex was right to reject it, because it is a live false pass and it closes
//! the same way everything else here did — by permission. `AdaptiveEvent` may carry only
//! `derive` and `doc`; every attribute **anywhere in the module** is policed, because a
//! proc macro emits arbitrary items rather than only code about what it annotates, so
//! `#[generate_event_wire] fn helper() {}` elsewhere can emit an impl for `AdaptiveEvent`
//! with every enum-specific check clean; and every macro invocation — at any
//! depth, not only item position, since a statement-position macro can expand to items —
//! must be on an allowlist that currently holds only `tracing::trace`.
//!
//! What genuinely remains: a macro *already allowlisted* could expand to anything, and
//! expansion is not in this source. That is why both lists are empty or minimal — the
//! guarantee is that nothing generative is permitted, not that generation is understood.

use proc_macro2::TokenTree;
use std::collections::BTreeSet;
use std::fs;
use std::path::Path;
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Expr, ExprLit, File, Item, ItemEnum, Lit, Meta, Token, Type, UseTree, Visibility,
};
use walkdir::WalkDir;

const EVENT_MODULE: &str = "src/producers/event.rs";
const ADAPTIVE_EVENT: &str = "AdaptiveEvent";
const PRODUCERS_MODULE: &str = "producers";

/// Exactly what `src/producers/event.rs` may import, as flattened leaf paths.
///
/// An **allowlist**, not a denylist, and the inversion is the point. Name-based detection
/// was bypassed by an alias, then by a crate-local re-export that left no trace of serde in
/// the file at all. Chasing re-export chains would be a partial name resolver wearing a
/// guarantee's clothes — still blind to multi-hop `pub use`, glob re-exports and macros.
///
/// An allowlist does not care what a name means: any import not on this list fails, so
/// every re-export form is closed at once. Leaves are flattened, so grouping is irrelevant —
/// `use a::{b, c}` and two separate statements normalize identically.
const EVENT_ALLOWED_IMPORTS: &[&str] = &[
    "std::sync::Arc",
    "tokio::sync::broadcast",
    "super::admission::AdmissionRefusal",
    "super::admission::ProducerKey",
    "crate::adaptive::DocumentRevisions",
    "crate::adaptive::ProducerEpoch",
];

/// Exactly what `AdaptiveEvent` may derive.
///
/// Needed separately from the import allowlist because a derive requires no import at all:
/// `#[derive(crate::wire::EventWire)]` names the macro by path.
const ADAPTIVE_EVENT_ALLOWED_DERIVES: &[&str] = &["Debug", "Clone"];

/// Exactly what attributes `AdaptiveEvent` may carry.
///
/// The third allowlist, and the one that closes what an earlier commit merely *documented*
/// as residual. An **attribute macro** generates code without being a derive:
///
/// ```ignore
/// #[event_wire]
/// pub enum AdaptiveEvent { … }
/// ```
///
/// leaves imports, derives and `ItemImpl` entirely clean while expanding to whatever it
/// likes, including a `Serialize` impl. Inspecting derives alone cannot see it, and no
/// amount of inspecting *can*, because the expansion is not in this file.
///
/// So the decision is again permission rather than detection: `derive` and `doc` are what
/// the type actually carries, and anything else fails until somebody adds it deliberately.
/// `cfg_attr` is deliberately **absent** — a conditional attribute is a conditional
/// expansion, and the condition is somebody else's build flag.
const ADAPTIVE_EVENT_ALLOWED_ATTRIBUTES: &[&str] = &["derive", "doc"];

/// Macro invocations permitted **anywhere** in `src/producers/event.rs`.
///
/// `make_event_wire!(AdaptiveEvent);` generates an `impl` while leaving imports, derives
/// and `ItemImpl` clean. Restricting the check to item position is not enough: Rust lets a
/// macro in statement position expand to item definitions, and a trait impl is valid
/// wherever it is written — so the escape simply moves one level down, into a function
/// body. The collector therefore visits every `syn::Macro` at any depth.
///
/// `tracing::trace` is the module's one real invocation, and listing it is the price of the
/// broader net: an allowlist that excluded it would be deleted the first time somebody
/// added a log line.
const EVENT_ALLOWED_MACROS: &[&str] = &["tracing::trace"];

/// Directories exempt from the source sweep. Trailing separator, so a sibling directory
/// sharing a prefix - `src/adaptive_extras/` - is not accidentally exempted too.
const EXEMPT_DIRS: &[&str] = &["src/adaptive/", "src/producers/"];
/// Files exempt from the source sweep, matched by exact equality rather than prefix.
const EXEMPT_FILES: &[&str] = &["src/lib.rs", "src/main.rs"];

/// Whether a source path is outside the surface sweep.
///
/// Raw prefix matching would exempt `src/adaptive_extras/` along with `src/adaptive/`, and
/// anything beginning with `src/lib.rs`. A too-broad exemption is silent - the sweep just
/// stops covering a directory - which is the same failure class as a lint that passes.
fn is_sweep_exempt(path: &str) -> bool {
    EXEMPT_FILES.contains(&path) || EXEMPT_DIRS.iter().any(|dir| path.starts_with(dir))
}

/// How many modules deep a source file sits, so `super::` can be resolved.
///
/// `src/lib.rs` and `src/main.rs` are the crate root (0). `src/api/mod.rs` is `api` (1).
/// `src/adapters/hqplayer.rs` is `adapters::hqplayer` (2).
fn module_depth(path: &str) -> usize {
    // Two separate steps, because chaining them makes the second `unwrap_or` fall back to
    // the untrimmed `path` and undo the first: `src/api/mod` would count `src` as a module.
    let trimmed = path.strip_prefix("src/").unwrap_or(path);
    let trimmed = trimmed.strip_suffix(".rs").unwrap_or(trimmed);
    if trimmed == "lib" || trimmed == "main" {
        return 0;
    }
    let mut parts: Vec<&str> = trimmed.split('/').collect();
    if parts.last() == Some(&"mod") {
        parts.pop();
    }
    parts.len()
}

/// Modules a surface may not reach.
const FORBIDDEN_MODULES: &[&str] = &["adaptive", "producers"];
/// Crate roots those modules can be addressed through.
const CRATE_ROOTS: &[&str] = &["crate", "unified_hifi_control"];

// =============================================================================
// Parsing
// =============================================================================

/// Parse a source file, failing loudly.
///
/// A lint that silently treats an unparsable file as empty passes vacuously, which is the
/// defect class this file exists to stop reproducing.
fn parse_source(path: &str, source: &str) -> File {
    match syn::parse_file(source) {
        Ok(file) => file,
        Err(error) => panic!("failed to parse {path}: {error}"),
    }
}

fn parse_file_at(path: &str) -> File {
    let source = fs::read_to_string(path).unwrap_or_else(|error| panic!("read {path}: {error}"));
    parse_source(path, &source)
}

fn rust_sources_under(root: &str) -> Vec<(String, String)> {
    let base = Path::new(root);
    if !base.exists() {
        return Vec::new();
    }
    let mut sources = Vec::new();
    for entry in WalkDir::new(base).into_iter().filter_map(Result::ok) {
        let file = entry.path();
        if file.extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(file)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", file.display()));
            sources.push((file.display().to_string(), text));
        }
    }
    sources
}

// =============================================================================
// Structural inspection
// =============================================================================

/// Every import in `file`, flattened to leaf paths.
///
/// Structural, so attributes, visibility, grouping, renaming and formatting are fields and
/// variants rather than characters to be recognized. `#[allow(unused_imports)] pub use
/// a::B as C;` is an `ItemUse` exactly as `use a::B;` is.
fn imports_of(file: &File) -> Vec<String> {
    #[derive(Default)]
    struct ImportVisitor {
        found: Vec<String>,
    }
    impl<'ast> Visit<'ast> for ImportVisitor {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            self.found.extend(flatten_use_tree(&item.tree));
            visit::visit_item_use(self, item);
        }
    }
    let mut visitor = ImportVisitor::default();
    visitor.visit_file(file);
    visitor.found
}

fn flatten_use_tree(tree: &UseTree) -> Vec<String> {
    match tree {
        UseTree::Path(path) => flatten_use_tree(&path.tree)
            .into_iter()
            .map(|leaf| format!("{}::{leaf}", path.ident))
            .collect(),
        UseTree::Name(name) => vec![name.ident.to_string()],
        UseTree::Rename(rename) => vec![format!("{} as {}", rename.ident, rename.rename)],
        UseTree::Glob(_) => vec!["*".to_string()],
        UseTree::Group(group) => group.items.iter().flat_map(flatten_use_tree).collect(),
    }
}

/// The `AdaptiveEvent` enum, located by identity rather than by matching a declaration
/// string.
fn adaptive_event_enum(file: &File) -> Option<&ItemEnum> {
    file.items.iter().find_map(|item| match item {
        Item::Enum(item_enum) if item_enum.ident == ADAPTIVE_EVENT => Some(item_enum),
        _ => None,
    })
}

/// Every trait derived by `attrs`, as final path segments.
///
/// Recurses through `cfg_attr`, because a conditional derive is not a safe derive — it is
/// one whose condition is somebody else's build flag.
fn derives_in(attrs: &[Attribute]) -> Vec<String> {
    let mut found = Vec::new();
    for attr in attrs {
        collect_derives(&attr.meta, &mut found);
    }
    found
}

fn collect_derives(meta: &Meta, out: &mut Vec<String>) {
    let Meta::List(list) = meta else {
        return;
    };
    if list.path.is_ident("derive") {
        if let Ok(paths) =
            list.parse_args_with(Punctuated::<syn::Path, Token![,]>::parse_terminated)
        {
            for path in paths {
                if let Some(last) = path.segments.last() {
                    out.push(last.ident.to_string());
                }
            }
        }
        return;
    }
    if list.path.is_ident("cfg_attr") {
        if let Ok(metas) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) {
            // The first element is the predicate; everything after it is what gets applied.
            for nested in metas.iter().skip(1) {
                collect_derives(nested, out);
            }
        }
    }
}

/// The path of every attribute in `attrs`, as a dotted name.
///
/// `#[derive(Debug)]` is `derive`, `#[doc = "…"]` is `doc`, `#[event_wire]` is
/// `event_wire`, and `#[serde::skip]` is `serde::skip`.
fn attribute_paths(attrs: &[Attribute]) -> Vec<String> {
    attrs
        .iter()
        .map(|attr| {
            attr.path()
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::")
        })
        .collect()
}

/// Attributes not on [`ADAPTIVE_EVENT_ALLOWED_ATTRIBUTES`].
fn disallowed_attributes(attrs: &[Attribute]) -> Vec<String> {
    attribute_paths(attrs)
        .into_iter()
        .filter(|path| !ADAPTIVE_EVENT_ALLOWED_ATTRIBUTES.contains(&path.as_str()))
        .collect()
}

/// Every macro invocation in `file`, at any depth, as a dotted path.
///
/// Item position, statement position, expression position, inside an `impl` method, inside
/// a closure — all of them. A `syn::visit::Visit` walk reaches each one, so there is no
/// position for a code-generating macro to hide in.
#[derive(Default)]
struct MacroInvocationVisitor {
    found: Vec<String>,
}

impl<'ast> Visit<'ast> for MacroInvocationVisitor {
    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.found.push(
            mac.path
                .segments
                .iter()
                .map(|segment| segment.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        );
        visit::visit_macro(self, mac);
    }
}

fn macro_invocations(file: &File) -> Vec<String> {
    let mut visitor = MacroInvocationVisitor::default();
    visitor.visit_file(file);
    visitor.found
}

/// Every attribute in `file`, at any depth, paired with the item that owns it.
///
/// Attributes on functions, structs, impl blocks, fields, variants and nested modules all
/// appear. The enum-specific checks audit only `AdaptiveEvent`'s own attributes, and a
/// procedural macro emits arbitrary *items* rather than only code about the thing it
/// annotates — so `#[generate_event_wire] fn helper() {}` elsewhere in the module can emit
/// `impl Serialize for AdaptiveEvent` with every enum-specific check clean. Found by Codex
/// at `4a817c9`.
#[derive(Default)]
struct AttributeVisitor {
    owners: Vec<String>,
    found: Vec<(Vec<String>, String)>,
}

fn item_label(item: &Item) -> String {
    match item {
        Item::Enum(inner) => format!("enum {}", inner.ident),
        Item::Struct(inner) => format!("struct {}", inner.ident),
        Item::Fn(inner) => format!("fn {}", inner.sig.ident),
        Item::Mod(inner) => format!("mod {}", inner.ident),
        Item::Impl(inner) => match inner.self_ty.as_ref() {
            syn::Type::Path(path) => path
                .path
                .segments
                .last()
                .map(|segment| format!("impl {}", segment.ident))
                .unwrap_or_else(|| "impl".to_string()),
            _ => "impl".to_string(),
        },
        Item::Type(inner) => format!("type {}", inner.ident),
        Item::Const(inner) => format!("const {}", inner.ident),
        Item::Static(inner) => format!("static {}", inner.ident),
        Item::Trait(inner) => format!("trait {}", inner.ident),
        Item::Use(_) => "use".to_string(),
        Item::Macro(inner) => inner
            .mac
            .path
            .segments
            .last()
            .map(|segment| format!("{}!", segment.ident))
            .unwrap_or_else(|| "macro".to_string()),
        _ => "item".to_string(),
    }
}

impl<'ast> Visit<'ast> for AttributeVisitor {
    fn visit_item(&mut self, item: &'ast Item) {
        self.owners.push(item_label(item));
        visit::visit_item(self, item);
        self.owners.pop();
    }

    /// A variant is a distinct location from the enum that contains it.
    ///
    /// Attributing a variant's attributes to the enum let a `derive` there inherit the
    /// enum's permission. A built-in derive in that position does not compile, but a
    /// proc-macro attribute does — and the policy should say where an attribute *is*
    /// rather than lean on a later compile.
    fn visit_variant(&mut self, variant: &'ast syn::Variant) {
        self.owners.push(format!("variant {}", variant.ident));
        visit::visit_variant(self, variant);
        self.owners.pop();
    }

    /// Fields are tracked for the same reason, though with no reachable exploit today: a
    /// field is always inside a variant or a struct, and either frame already moves the
    /// stack away from the one permitted owner. Removing this tracking does not fail any
    /// probe — stated rather than implied, because a mutation that changes nothing is not
    /// evidence that the code it changed is load-bearing.
    fn visit_field(&mut self, field: &'ast syn::Field) {
        let label = field
            .ident
            .as_ref()
            .map(|ident| format!("field {ident}"))
            .unwrap_or_else(|| "field".to_string());
        self.owners.push(label);
        visit::visit_field(self, field);
        self.owners.pop();
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        let path = attr
            .path()
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect::<Vec<_>>()
            .join("::");
        self.found.push((self.owners.clone(), path));
        visit::visit_attribute(self, attr);
    }
}

/// Every attribute in `file`, paired with the full owner stack that carries it.
///
/// The **stack**, not its last frame. Storing only the innermost label made a nested item
/// sharing a name indistinguishable from the real one:
///
/// ```ignore
/// mod inner { #[derive(GenerateAdaptiveWire)] enum AdaptiveEvent {} }
/// #[derive(Debug, Clone)] pub enum AdaptiveEvent {}
/// ```
///
/// Both derives reported the owner `enum AdaptiveEvent`, so the decoy inherited the real
/// type's permission — and the token audit only inspects `adaptive_event_enum`, which
/// searches top-level items, so nobody audited the decoy at all. Found by Codex at
/// `24c9f56`.
fn module_attributes(file: &File) -> Vec<(Vec<String>, String)> {
    let mut visitor = AttributeVisitor::default();
    visitor.visit_file(file);
    visitor.found
}

/// The one owner stack permitted to carry a `derive`.
fn adaptive_event_owner() -> Vec<String> {
    vec![format!("enum {ADAPTIVE_EVENT}")]
}

/// Attributes anywhere in `file` that the module-wide policy forbids.
///
/// The policy, location-aware because a blanket list would have to permit `derive`
/// everywhere and that is exactly the hole:
///
/// * `doc` — anywhere. Doc comments desugar to it and the module is heavily documented.
/// * `derive` — **only** on `enum AdaptiveEvent`, and only holding
///   [`ADAPTIVE_EVENT_ALLOWED_DERIVES`]. A custom derive on a helper struct emits arbitrary
///   items just as an attribute macro does.
/// * anything else — rejected, wherever it appears.
///
/// This is the production gate. The enum-specific checks are kept because they name the
/// offender more precisely, but they are no longer what carries the guarantee.
fn disallowed_module_attributes(file: &File) -> Vec<String> {
    let permitted = adaptive_event_owner();
    let mut rejected = Vec::new();
    for (owner, path) in module_attributes(file) {
        let where_it_is = owner.join(" > ");
        match path.as_str() {
            "doc" => {}
            // Depth-exact: the single top-level `enum AdaptiveEvent`, and nothing that
            // merely shares its name at some other depth.
            "derive" if owner == permitted => {}
            _ => rejected.push(format!("#[{path}] on {where_it_is}")),
        }
    }
    // A derive in the right place holding the wrong token is still forbidden, so the policy
    // cannot be satisfied by relocating a bad derive onto the enum.
    if let Some(item) = adaptive_event_enum(file) {
        for token in derives_in(&item.attrs) {
            if !ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&token.as_str()) {
                rejected.push(format!("derive({token}) on enum {ADAPTIVE_EVENT}"));
            }
        }
    }
    rejected
}

/// Every trait implemented for `type_name` in `file`.
///
/// Inherent impls (`impl AdaptiveEvent { … }`) are not trait impls and do not appear.
fn trait_impls_for(file: &File, type_name: &str) -> Vec<String> {
    struct ImplVisitor<'a> {
        type_name: &'a str,
        found: Vec<String>,
    }
    impl<'ast> Visit<'ast> for ImplVisitor<'_> {
        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            if let Some((_, path, _)) = item.trait_.as_ref() {
                if let syn::Type::Path(type_path) = item.self_ty.as_ref() {
                    let target = type_path
                        .path
                        .segments
                        .last()
                        .map(|segment| segment.ident.to_string());
                    if target.as_deref() == Some(self.type_name) {
                        if let Some(last) = path.segments.last() {
                            self.found.push(last.ident.to_string());
                        }
                    }
                }
            }
            visit::visit_item_impl(self, item);
        }
    }
    let mut visitor = ImplVisitor {
        type_name,
        found: Vec::new(),
    };
    visitor.visit_file(file);
    visitor.found
}

/// Every alias in `file` that gives the internal event a second name.
///
/// [`trait_impls_for`] matches an impl's self type by its last path segment against the literal
/// `AdaptiveEvent`. So this implements a serialization trait for the internal event while every
/// allowlist in `event.rs` stays clean and the crate-wide impl sweep sees an impl for a type it
/// has never heard of:
///
/// ```ignore
/// type Ev = AdaptiveEvent;
/// impl serde::Serialize for Ev { /* ... */ }
/// ```
///
/// **Prohibited rather than resolved.** Following aliases needs name resolution, which this
/// file deliberately does not attempt — a half-built resolver would fail silently, which is the
/// one outcome a lint must never have. The alias is banned instead: if the internal event can
/// only ever be spelled `AdaptiveEvent`, matching that single spelling is sufficient. The cost
/// is a rule to be told about; the benefit is that the guarantee does not depend on this test
/// file being cleverer than the next escape.
fn adaptive_event_aliases(file: &File) -> Vec<String> {
    #[derive(Default)]
    struct AliasVisitor {
        found: Vec<String>,
    }
    impl<'ast> Visit<'ast> for AliasVisitor {
        fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
            if type_tail_is(&item.ty, ADAPTIVE_EVENT) {
                self.found
                    .push(format!("type {} = {ADAPTIVE_EVENT}", item.ident));
            }
            visit::visit_item_type(self, item);
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            for leaf in flatten_use_tree(&item.tree) {
                let Some((original, rename)) = leaf.split_once(" as ") else {
                    continue;
                };
                if original.rsplit("::").next() == Some(ADAPTIVE_EVENT) {
                    self.found.push(format!("use {ADAPTIVE_EVENT} as {rename}"));
                }
            }
            visit::visit_item_use(self, item);
        }
    }
    let mut visitor = AliasVisitor::default();
    visitor.visit_file(file);
    visitor.found
}

/// Whether `ty` is a path type whose final segment is `name`.
fn type_tail_is(ty: &Type, name: &str) -> bool {
    struct TailVisitor<'a> {
        name: &'a str,
        found: bool,
    }
    impl<'ast> Visit<'ast> for TailVisitor<'_> {
        fn visit_type_path(&mut self, path: &'ast syn::TypePath) {
            if path
                .path
                .segments
                .last()
                .is_some_and(|segment| segment.ident == self.name)
            {
                self.found = true;
            }
            visit::visit_type_path(self, path);
        }
    }
    let mut visitor = TailVisitor { name, found: false };
    visitor.visit_type(ty);
    visitor.found
}

/// Whether a `::`-joined path names a forbidden module as a whole segment.
///
/// Split on separators and compared for equality, so `adaptive_extras` is not `adaptive` — the
/// same boundary-anchoring [`is_sweep_exempt`] applies to directory names.
fn path_names_forbidden_module(path: &str) -> bool {
    path.split(" as ")
        .next()
        .unwrap_or(path)
        .split("::")
        .any(|segment| FORBIDDEN_MODULES.contains(&segment.trim()))
}

/// Exported re-exports and aliases that launder a forbidden module's types through a root.
///
/// `src/lib.rs` and `src/main.rs` are exempt from both reference sweeps, because the
/// composition root must name `producers` to construct the aggregator. That exemption is a hole
/// if the root may also *re-export*: this in `lib.rs`
///
/// ```ignore
/// pub use crate::adaptive::ProducerDocument as Doc;
/// ```
///
/// lets `src/api/mod.rs` write `use crate::Doc;` and `Json(doc)`, which names neither forbidden
/// module and so passes [`forbidden_module_references`] untouched — publishing the v1 contract
/// from a public endpoint with every lint green.
///
/// Exported uses, aliases, functions, constants and statics are covered. Visibility is what
/// makes a root item reachable from another module; private helpers remain composition details.
/// Type syntax is traversed recursively, so wrapping a forbidden type does not launder it.
fn root_launderings(file: &File) -> Vec<String> {
    #[derive(Default)]
    struct AliasCollector<'ast> {
        aliases: Vec<(String, &'ast Type)>,
        imports: Vec<(String, String)>,
        macros: std::collections::BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for AliasCollector<'ast> {
        fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
            self.aliases.push((item.ident.to_string(), &item.ty));
            visit::visit_item_type(self, item);
        }
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            for leaf in flatten_use_tree(&item.tree) {
                let (source, introduced) = leaf.split_once(" as ").map_or_else(
                    || (leaf.as_str(), leaf.rsplit("::").next().unwrap_or(&leaf)),
                    |(source, rename)| (source, rename),
                );
                self.imports
                    .push((introduced.to_string(), source.to_string()));
            }
            visit::visit_item_use(self, item);
        }
        fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
            if let Some(name) = &item.ident {
                self.macros.insert(name.to_string());
            }
            visit::visit_item_macro(self, item);
        }
    }
    let mut collector = AliasCollector::default();
    collector.visit_file(file);
    let mut tainted_aliases = std::collections::BTreeSet::new();
    loop {
        let before = tainted_aliases.len();
        for (name, ty) in &collector.aliases {
            if type_names_forbidden_module_or_alias(ty, &tainted_aliases) {
                tainted_aliases.insert(name.clone());
            }
        }
        for (introduced, source) in &collector.imports {
            if path_names_forbidden_module(source)
                || source
                    .split("::")
                    .any(|segment| tainted_aliases.contains(segment))
            {
                tainted_aliases.insert(introduced.clone());
            }
        }
        if tainted_aliases.len() == before {
            break;
        }
    }

    struct RootVisitor<'a> {
        found: Vec<String>,
        tainted_aliases: &'a std::collections::BTreeSet<String>,
        macro_names: &'a std::collections::BTreeSet<String>,
    }
    impl RootVisitor<'_> {
        fn type_is_forbidden(&self, ty: &Type) -> bool {
            type_names_forbidden_module_or_alias(ty, self.tainted_aliases)
        }

        fn signature_is_forbidden(&self, signature: &syn::Signature) -> bool {
            signature_names_forbidden_module_or_alias(signature, self.tainted_aliases)
        }
    }
    impl<'ast> Visit<'ast> for RootVisitor<'_> {
        fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
            for leaf in flatten_use_tree(&item.tree) {
                if !is_exported(&item.vis)
                    && leaf.rsplit("::").next() == Some("*")
                    && (path_names_forbidden_module(&leaf)
                        || leaf
                            .split("::")
                            .any(|segment| self.tainted_aliases.contains(segment)))
                {
                    self.found
                        .push(format!("private glob from forbidden module {leaf}"));
                }
                if is_exported(&item.vis) {
                    if path_names_forbidden_module(&leaf)
                        || leaf
                            .split("::")
                            .any(|segment| self.tainted_aliases.contains(segment))
                    {
                        self.found.push(format!("exported use {leaf}"));
                    }
                    let source = leaf
                        .split_once(" as ")
                        .map_or(leaf.as_str(), |(source, _)| source);
                    if source
                        .rsplit("::")
                        .next()
                        .is_some_and(|name| self.macro_names.contains(name))
                    {
                        self.found.push(format!("exported macro use {leaf}"));
                    }
                }
            }
            visit::visit_item_use(self, item);
        }
        fn visit_item_type(&mut self, item: &'ast syn::ItemType) {
            if is_exported(&item.vis)
                && (self.type_is_forbidden(&item.ty)
                    || generics_names_forbidden_module_or_alias(
                        &item.generics,
                        self.tainted_aliases,
                    ))
            {
                self.found.push(format!("exported type {}", item.ident));
            }
            visit::visit_item_type(self, item);
        }
        fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
            if is_exported(&item.vis) && self.signature_is_forbidden(&item.sig) {
                self.found.push(format!("exported fn {}", item.sig.ident));
            }
            visit::visit_item_fn(self, item);
        }
        fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
            if is_exported(&item.vis)
                && (generics_names_forbidden_module_or_alias(&item.generics, self.tainted_aliases)
                    || item
                        .fields
                        .iter()
                        .any(|field| is_exported(&field.vis) && self.type_is_forbidden(&field.ty)))
            {
                self.found.push(format!("exported struct {}", item.ident));
            }
            visit::visit_item_struct(self, item);
        }
        fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
            if is_exported(&item.vis)
                && (generics_names_forbidden_module_or_alias(&item.generics, self.tainted_aliases)
                    || item
                        .variants
                        .iter()
                        .flat_map(|variant| variant.fields.iter())
                        .any(|field| self.type_is_forbidden(&field.ty)))
            {
                self.found.push(format!("exported enum {}", item.ident));
            }
            visit::visit_item_enum(self, item);
        }
        fn visit_item_union(&mut self, item: &'ast syn::ItemUnion) {
            if is_exported(&item.vis)
                && (generics_names_forbidden_module_or_alias(&item.generics, self.tainted_aliases)
                    || item
                        .fields
                        .named
                        .iter()
                        .any(|field| is_exported(&field.vis) && self.type_is_forbidden(&field.ty)))
            {
                self.found.push(format!("exported union {}", item.ident));
            }
            visit::visit_item_union(self, item);
        }
        fn visit_item_trait(&mut self, item: &'ast syn::ItemTrait) {
            if is_exported(&item.vis)
                && (generics_names_forbidden_module_or_alias(&item.generics, self.tainted_aliases)
                    || bounds_name_forbidden_module_or_alias(
                        &item.supertraits,
                        self.tainted_aliases,
                    )
                    || item.items.iter().any(|trait_item| match trait_item {
                        syn::TraitItem::Fn(method) => self.signature_is_forbidden(&method.sig),
                        syn::TraitItem::Const(item) => self.type_is_forbidden(&item.ty),
                        syn::TraitItem::Type(item) => {
                            trait_item_type_names_forbidden_module_or_alias(
                                item,
                                self.tainted_aliases,
                            )
                        }
                        _ => false,
                    }))
            {
                self.found.push(format!("exported trait {}", item.ident));
            }
            visit::visit_item_trait(self, item);
        }
        fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
            if generics_names_forbidden_module_or_alias(&item.generics, self.tainted_aliases)
                || self.type_is_forbidden(&item.self_ty)
                || item.trait_.as_ref().is_some_and(|(_, path, _)| {
                    syn_path_names_forbidden_module_or_alias(path, self.tainted_aliases)
                })
            {
                self.found.push("impl on forbidden type".to_string());
            }
            let trait_impl = item.trait_.is_some();
            for impl_item in &item.items {
                match impl_item {
                    syn::ImplItem::Fn(method)
                        if (trait_impl || is_exported(&method.vis))
                            && self.signature_is_forbidden(&method.sig) =>
                    {
                        self.found
                            .push(format!("exported method {}", method.sig.ident));
                    }
                    syn::ImplItem::Const(item)
                        if (trait_impl || is_exported(&item.vis))
                            && self.type_is_forbidden(&item.ty) =>
                    {
                        self.found
                            .push(format!("exported associated const {}", item.ident));
                    }
                    syn::ImplItem::Type(item) if self.type_is_forbidden(&item.ty) => {
                        self.found
                            .push(format!("exported associated type {}", item.ident));
                    }
                    _ => {}
                }
            }
            visit::visit_item_impl(self, item);
        }
        fn visit_item_foreign_mod(&mut self, item: &'ast syn::ItemForeignMod) {
            for foreign in &item.items {
                match foreign {
                    syn::ForeignItem::Fn(function)
                        if is_exported(&function.vis)
                            && self.signature_is_forbidden(&function.sig) =>
                    {
                        self.found
                            .push(format!("exported foreign fn {}", function.sig.ident));
                    }
                    syn::ForeignItem::Static(item)
                        if is_exported(&item.vis) && self.type_is_forbidden(&item.ty) =>
                    {
                        self.found
                            .push(format!("exported foreign static {}", item.ident));
                    }
                    _ => {}
                }
            }
            visit::visit_item_foreign_mod(self, item);
        }
        fn visit_item_const(&mut self, item: &'ast syn::ItemConst) {
            if is_exported(&item.vis) && self.type_is_forbidden(&item.ty) {
                self.found.push(format!("exported const {}", item.ident));
            }
            visit::visit_item_const(self, item);
        }
        fn visit_item_static(&mut self, item: &'ast syn::ItemStatic) {
            if is_exported(&item.vis) && self.type_is_forbidden(&item.ty) {
                self.found.push(format!("exported static {}", item.ident));
            }
            visit::visit_item_static(self, item);
        }
        fn visit_item_macro(&mut self, item: &'ast syn::ItemMacro) {
            if item
                .attrs
                .iter()
                .any(|attribute| attribute.path().is_ident("macro_export"))
            {
                self.found.push("exported macro".to_string());
            }
            if token_stream_names_forbidden_module_or_alias(&item.mac.tokens, self.tainted_aliases)
            {
                self.found
                    .push("macro tokens name a forbidden type".to_string());
            }
            visit::visit_item_macro(self, item);
        }
    }
    let mut visitor = RootVisitor {
        found: Vec::new(),
        tainted_aliases: &tainted_aliases,
        macro_names: &collector.macros,
    };
    visitor.visit_file(file);
    visitor.found
}

fn token_stream_names_forbidden_module_or_alias(
    tokens: &proc_macro2::TokenStream,
    aliases: &std::collections::BTreeSet<String>,
) -> bool {
    tokens.clone().into_iter().any(|token| match token {
        proc_macro2::TokenTree::Ident(ident) => {
            let ident = ident.to_string();
            FORBIDDEN_MODULES.contains(&ident.as_str()) || aliases.contains(&ident)
        }
        proc_macro2::TokenTree::Group(group) => {
            token_stream_names_forbidden_module_or_alias(&group.stream(), aliases)
        }
        proc_macro2::TokenTree::Punct(_) | proc_macro2::TokenTree::Literal(_) => false,
    })
}

fn signature_names_forbidden_module_or_alias(
    signature: &syn::Signature,
    aliases: &std::collections::BTreeSet<String>,
) -> bool {
    let mut visitor = ForbiddenPathVisitor::new(aliases);
    visitor.visit_signature(signature);
    visitor.found
}

fn generics_names_forbidden_module_or_alias(
    generics: &syn::Generics,
    aliases: &std::collections::BTreeSet<String>,
) -> bool {
    let mut visitor = ForbiddenPathVisitor::new(aliases);
    visitor.visit_generics(generics);
    visitor.found
}

fn bounds_name_forbidden_module_or_alias(
    bounds: &syn::punctuated::Punctuated<syn::TypeParamBound, syn::token::Plus>,
    aliases: &std::collections::BTreeSet<String>,
) -> bool {
    let mut visitor = ForbiddenPathVisitor::new(aliases);
    for bound in bounds {
        visitor.visit_type_param_bound(bound);
    }
    visitor.found
}

fn trait_item_type_names_forbidden_module_or_alias(
    item: &syn::TraitItemType,
    aliases: &std::collections::BTreeSet<String>,
) -> bool {
    let mut visitor = ForbiddenPathVisitor::new(aliases);
    visitor.visit_trait_item_type(item);
    visitor.found
}

fn syn_path_names_forbidden_module_or_alias(
    path: &syn::Path,
    aliases: &std::collections::BTreeSet<String>,
) -> bool {
    let mut visitor = ForbiddenPathVisitor::new(aliases);
    visitor.visit_path(path);
    visitor.found
}

/// Whether a type's path names a forbidden module as a whole segment.
fn type_names_forbidden_module_or_alias(
    ty: &Type,
    aliases: &std::collections::BTreeSet<String>,
) -> bool {
    let mut visitor = ForbiddenPathVisitor::new(aliases);
    visitor.visit_type(ty);
    visitor.found
}

struct ForbiddenPathVisitor<'a> {
    found: bool,
    aliases: &'a std::collections::BTreeSet<String>,
}

impl<'a> ForbiddenPathVisitor<'a> {
    fn new(aliases: &'a std::collections::BTreeSet<String>) -> Self {
        Self {
            found: false,
            aliases,
        }
    }
}

impl<'ast> Visit<'ast> for ForbiddenPathVisitor<'_> {
    fn visit_path(&mut self, path: &'ast syn::Path) {
        if path.segments.iter().any(|segment| {
            FORBIDDEN_MODULES.contains(&segment.ident.to_string().as_str())
                || self.aliases.contains(&segment.ident.to_string())
        }) {
            self.found = true;
        }
        visit::visit_path(self, path);
    }
}

/// Whether this visibility makes a name reachable from another module.
fn is_exported(vis: &Visibility) -> bool {
    !matches!(vis, Visibility::Inherited)
}

/// Every `#[cfg(...)]` argument applying to module `name`, including from an enclosing
/// module. `None` means the module is not declared here.
fn module_cfg_gates(file: &File, name: &str) -> Option<Vec<Meta>> {
    fn search(items: &[Item], name: &str, inherited: &[Meta]) -> Option<Vec<Meta>> {
        for item in items {
            let Item::Mod(item_mod) = item else {
                continue;
            };
            let mut gates = inherited.to_vec();
            gates.extend(cfg_arguments(&item_mod.attrs));
            if item_mod.ident == name {
                return Some(gates);
            }
            if let Some((_, nested)) = &item_mod.content {
                if let Some(found) = search(nested, name, &gates) {
                    return Some(found);
                }
            }
        }
        None
    }
    search(&file.items, name, &[])
}

fn cfg_arguments(attrs: &[Attribute]) -> Vec<Meta> {
    attrs
        .iter()
        .filter_map(|attr| match &attr.meta {
            Meta::List(list) if list.path.is_ident("cfg") => list.parse_args::<Meta>().ok(),
            _ => None,
        })
        .collect()
}

/// Whether a `cfg` argument compiles its item **only** when the `server` feature is on.
///
/// `all(feature = "server", …)` narrows and is accepted. `any(…)` widens — it would compile
/// the module for `web` without `server`, the WASM breakage the gate exists to prevent —
/// and `not(…)` inverts. Neither has an accepting arm.
fn gate_is_server_only(meta: &Meta) -> bool {
    match meta {
        Meta::NameValue(name_value) => {
            name_value.path.is_ident("feature")
                && matches!(
                    &name_value.value,
                    Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) if text.value() == "server"
                )
        }
        Meta::List(list) if list.path.is_ident("all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .map(|items| items.iter().any(gate_is_server_only))
            .unwrap_or(false),
        _ => false,
    }
}

/// Forbidden module paths referenced anywhere in a file.
///
/// Visits use trees, every path in type and expression position, and macro token streams.
/// Macro tokens are checked as *tokens* rather than text, so a forbidden path inside
/// `rsx! { … }` is found while a raw string or comment inside one cannot fabricate a match.
#[derive(Default)]
struct ModuleReferenceVisitor {
    found: BTreeSet<String>,
    /// Names that address this crate's root in the module currently being visited.
    roots: BTreeSet<String>,
    saved: Vec<BTreeSet<String>>,
    /// How many modules deep the file being scanned sits, so `super::` can be resolved.
    depth: usize,
}

impl ModuleReferenceVisitor {
    fn note(&mut self, root: &str, module: &str) {
        if self.roots.contains(root) && FORBIDDEN_MODULES.contains(&module) {
            self.found.insert(format!("{root}::{module}"));
        }
    }

    /// Check every adjacent segment pair, not only the first two.
    ///
    /// A root is not always the leading segment: `super::internal::adaptive::X` puts the
    /// aliased root second. Scanning pairs also covers `self::`, and costs nothing because
    /// a pair only matches when its left half is a known root.
    fn note_pairs(&mut self, segments: &[String]) {
        // A leading run of `super` is resolved against the file's own module depth: from
        // `src/api/mod.rs`, one module deep, `super::adaptive::X` *is* `crate::adaptive::X`.
        // Exactly `depth` supers reach the crate root; fewer land in an intermediate module
        // and more are not expressible. Found by CodeRabbit at `9c53079`.
        let supers = segments.iter().take_while(|s| *s == "super").count();
        if supers > 0 && supers == self.depth {
            if let Some(module) = segments.get(supers) {
                if FORBIDDEN_MODULES.contains(&module.as_str()) {
                    self.found.insert(format!("super x{supers}::{module}"));
                }
            }
        }
        for window in segments.windows(2) {
            self.note(&window[0], &window[1]);
        }
    }

    /// Collect maximal `Ident (:: Ident)*` runs and resolve each like an ordinary path,
    /// recursing into groups.
    ///
    /// The previous implementation stringified the whole token stream and substring-matched
    /// it, so `println!("crate::adaptive")` registered as a reference — while the comment
    /// above it claimed a literal could not fabricate a match. It could, and did. Found by
    /// Codex at `4129b87`.
    ///
    /// Walking `TokenTree` fixes that by construction rather than by exclusion: a `Literal`
    /// is a different variant from an `Ident`, so its contents are never sequence material.
    ///
    /// Collecting whole runs rather than adjacent four-token windows is what makes macro
    /// arguments obey the same rules as everything else. A window can only ever see one
    /// pair, and a leading `super` run is by definition wider than a pair — so
    /// `super::adaptive::X` was caught by [`Visit::visit_path`] and missed here, in the one
    /// place a path can be written without syn parsing it as a path. Feeding the full run
    /// through [`Self::note_pairs`] gives macro tokens the same depth resolution.
    fn scan_tokens(&mut self, stream: &proc_macro2::TokenStream) {
        let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
        let mut index = 0;
        while index < trees.len() {
            let TokenTree::Ident(first) = &trees[index] else {
                index += 1;
                continue;
            };
            // Extend through every following `:: Ident`, so the run is the whole path.
            let mut segments = vec![first.to_string()];
            let mut end = index + 1;
            while let (
                Some(TokenTree::Punct(left)),
                Some(TokenTree::Punct(right)),
                Some(TokenTree::Ident(next)),
            ) = (trees.get(end), trees.get(end + 1), trees.get(end + 2))
            {
                if left.as_char() != ':' || right.as_char() != ':' {
                    break;
                }
                segments.push(next.to_string());
                end += 3;
            }
            if segments.len() > 1 {
                self.note_pairs(&segments);
            }
            // Resume past the run: its interior idents were already considered as segments,
            // and restarting inside it would only rediscover suffixes of the same path.
            index = end.max(index + 1);
        }
        for tree in trees {
            if let TokenTree::Group(group) = tree {
                self.scan_tokens(&group.stream());
            }
        }
    }
}

impl<'ast> Visit<'ast> for ModuleReferenceVisitor {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        for leaf in flatten_use_tree(&item.tree) {
            // `use crate::adaptive as contract;` renders its leaf as `adaptive as
            // contract`, so the alias is trimmed before comparison.
            let segments: Vec<String> = leaf
                .split("::")
                .map(|part| part.split(" as ").next().unwrap_or(part).trim().to_string())
                .collect();
            self.note_pairs(&segments);
        }
        visit::visit_item_use(self, item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<String> = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        self.note_pairs(&segments);
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        self.scan_tokens(&mac.tokens);
        visit::visit_macro(self, mac);
    }

    /// Enter a module with its own crate-root scope: aliases it declares become roots for
    /// its subtree, and a `mod` it declares shadows an inherited alias of the same name.
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        self.saved.push(self.roots.clone());
        let outer_depth = self.depth;
        if let Some((_, items)) = &item.content {
            let (aliases, shadowed) = scope_delta(items, &self.roots);
            self.roots.extend(aliases);
            for name in &shadowed {
                self.roots.remove(name);
            }
            // An *inline* module puts its contents one module further down, so `super::`
            // inside it needs one more hop to reach the crate root. `depth` was fixed per
            // file, which got both directions backwards inside a nested module. Raised by
            // Codex against the first draft of the `super::` fix.
            //
            // Gating on `content` is semantic, not load-bearing: an external `mod x;` has
            // nothing beneath it to scan, so incrementing there too would be paired with the
            // restore below and observable by nothing. No probe can distinguish the two -
            // stated so nobody mistakes that surviving mutation for coverage.
            self.depth += 1;
        }
        visit::visit_item_mod(self, item);
        self.depth = outer_depth;
        if let Some(previous) = self.saved.pop() {
            self.roots = previous;
        }
    }
}

/// Crate-root names declared directly among `items`, and local module names that shadow.
///
/// A file-global alias set is wrong in both directions. `mod a { use crate as internal; }`
/// must not make `internal` a crate root inside `mod b`, and a genuine
/// `use crate as internal;` must stop being one inside a module that declares its own
/// `mod internal`. Found by Codex reviewing the first alias fix.
///
/// This is scope tracking, not name resolution: aliases are inherited by descendants and
/// shadowed by a sibling `mod` of the same name. That covers the reachable cases without
/// pretending to resolve Rust's full name lookup.
fn scope_delta(
    items: &[Item],
    known_roots: &BTreeSet<String>,
) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut aliases: BTreeSet<String> = BTreeSet::new();
    let mut shadowed = BTreeSet::new();

    for item in items {
        if let Item::Mod(item_mod) = item {
            shadowed.insert(item_mod.ident.to_string());
        }
    }

    // Fixed point, because an alias may be bound from another alias:
    //
    //     use crate as internal;
    //     use self::internal as also;
    //
    // The second rename's *source* is `internal`, which is not an original root, so a
    // single pass over the items would bind `internal` and miss `also`. Iterating until
    // nothing new is bound also handles longer chains and any declaration order. Raised by
    // Codex against `24ac3de`.
    loop {
        let mut roots: BTreeSet<String> = known_roots.clone();
        roots.extend(aliases.iter().cloned());
        let before = aliases.len();

        for item in items {
            match item {
                Item::Use(item_use) => {
                    collect_crate_aliases(&item_use.tree, &roots, false, &mut aliases)
                }
                // `extern crate self as internal;` binds the crate root without being a
                // `use` at all, and `extern crate unified_hifi_control as uhc;` does the
                // same by name.
                Item::ExternCrate(item_extern) => {
                    if let Some((_, rename)) = &item_extern.rename {
                        let source = item_extern.ident.to_string();
                        if source == "self" || roots.contains(&source) {
                            aliases.insert(rename.to_string());
                        }
                    }
                }
                _ => {}
            }
        }

        if aliases.len() == before {
            break;
        }
    }

    (aliases, shadowed)
}

/// Names bound to a known crate root by a use tree.
///
/// `at_root` records whether the subtree being walked is rooted at a known crate root, which
/// is what makes grouped `self` work: in `use crate::{self as internal};` the rename's source
/// ident is `self`, not a root name, so matching only against `roots` binds nothing. Inside a
/// group under `crate`, `self` *is* the crate root. Raised by an independent audit at
/// `6980b7b`; both that form and `use internal::{self as also};` compile clean.
///
/// The source is matched against the roots known *so far* rather than the two original
/// spellings, which is what lets an alias be bound from another alias.
///
/// Prefixes stay strict: a leading `self::` is transparent for lookup but is not itself the
/// crate root, and any other prefix stops the descent - `use foo::{self as also};` renames
/// `foo`, so recognizing grouped `self` must not make every group transparent.
fn collect_crate_aliases(
    tree: &UseTree,
    roots: &BTreeSet<String>,
    at_root: bool,
    out: &mut BTreeSet<String>,
) {
    match tree {
        UseTree::Rename(rename) => {
            let source = rename.ident.to_string();
            if roots.contains(&source) || (at_root && source == "self") {
                out.insert(rename.rename.to_string());
            }
        }
        UseTree::Group(group) => {
            for item in &group.items {
                collect_crate_aliases(item, roots, at_root, out);
            }
        }
        UseTree::Path(path) => {
            let head = path.ident.to_string();
            if roots.contains(&head) {
                // Descending through a crate root: a `self` below it names that root.
                collect_crate_aliases(&path.tree, roots, true, out);
            } else if head == "self" {
                // `self::x` looks up in the current module; transparent for finding a
                // rename of a known alias, but `self` here is not the crate root.
                //
                // Passing `false` rather than `true` is semantically right and currently
                // unobservable: the only shape that would distinguish them is
                // `use self::{self as x};`, which rustc rejects with
                // `error[E0432]: unresolved import self`. Flipping it fails no probe.
                // Stated rather than implied, so nobody mistakes the surviving mutation
                // for dead code.
                collect_crate_aliases(&path.tree, roots, false, out);
            }
        }
        _ => {}
    }
}

/// Forbidden module references in `file`, resolving crate-root aliases within their scope.
fn forbidden_module_references(file: &File, depth: usize) -> Vec<String> {
    let mut roots: BTreeSet<String> = CRATE_ROOTS.iter().map(|r| (*r).to_string()).collect();
    let (aliases, shadowed) = scope_delta(&file.items, &roots);
    roots.extend(aliases);
    for name in &shadowed {
        roots.remove(name);
    }
    let mut visitor = ModuleReferenceVisitor {
        found: BTreeSet::new(),
        roots,
        saved: Vec::new(),
        depth,
    };
    visitor.visit_file(file);
    visitor.found.into_iter().collect()
}

// =============================================================================
// Boundary 1: the public bus cannot carry adaptive data
// =============================================================================

#[test]
fn lint_public_bus_cannot_carry_adaptive_types() {
    // `BusEvent` is a wire payload, not merely an internal enum: `src/api/mod.rs`
    // serializes it verbatim into `GET /events`. A variant naming an adaptive type would
    // change that endpoint's response schema and publish the v1 contract outside this
    // repository.
    // Both layouts supported, and the collection asserted non-empty before it is iterated:
    // `rust_sources_under` returns an empty vector for a path that does not exist, so
    // renaming `src/bus/` to `src/bus.rs` would have made this pass by scanning nothing.
    // Found by CodeRabbit at `9c53079`.
    let mut sources = rust_sources_under("src/bus");
    if let Ok(text) = fs::read_to_string("src/bus.rs") {
        sources.push(("src/bus.rs".to_string(), text));
    }
    assert!(
        !sources.is_empty(),
        "no bus sources found under src/bus/ or at src/bus.rs; this lint would pass by \
         scanning nothing"
    );
    // The vacuity this guards against is real, not hypothetical: a path that does not exist
    // yields an empty collection rather than an error, so the loop below would simply not
    // run. Asserted directly, because a mutation that disables the guard cannot fail while
    // `src/bus/` still exists.
    assert!(
        rust_sources_under("src/bus_does_not_exist").is_empty(),
        "the source walker is expected to return empty for a missing path"
    );

    let mut violations = Vec::new();
    for (path, text) in sources {
        let file = parse_source(&path, &text);
        for reference in forbidden_module_references(&file, module_depth(&path)) {
            violations.push(format!("{path}: references `{reference}`"));
        }
    }
    assert!(
        violations.is_empty(),
        "the public bus must not carry adaptive data - `GET /events` serializes every \
         BusEvent variant verbatim, so a variant here is a public response-schema \
         change:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_the_sse_projection_does_not_mention_the_publication_layer() {
    let file = parse_file_at("src/api/mod.rs");
    let references = forbidden_module_references(&file, module_depth("src/api/mod.rs"));
    assert!(
        references.is_empty(),
        "src/api/mod.rs references {references:?}. Any HTTP or SSE exposure of the producer \
         document needs explicit API approval and an `api-change-approved` label that must \
         never be self-applied."
    );
}

// =============================================================================
// Boundary 2: no surface reaches the contract or the publication layer
// =============================================================================

#[test]
fn lint_only_the_composition_root_names_the_contract_or_the_publication_layer() {
    // Swept over all of `src/` rather than an enumerated list of surfaces. An enumerated
    // list stops covering whatever is added next; the first draft listed six directories
    // and omitted `src/mqtt`, which publishes to Home Assistant.
    let mut violations = Vec::new();
    let mut swept = 0usize;
    for (path, text) in rust_sources_under("src") {
        if is_sweep_exempt(&path) {
            continue;
        }
        swept += 1;
        let file = parse_source(&path, &text);
        for reference in forbidden_module_references(&file, module_depth(&path)) {
            violations.push(format!("{path}: references `{reference}`"));
        }
    }
    assert!(
        swept > 50,
        "the sweep covered only {swept} files, which means it is not walking src/"
    );
    assert!(
        violations.is_empty(),
        "#324 publishes to in-repository consumers only, and `ProducerDocument` derives \
         `Serialize` - one `Json(snapshot)` re-exports the whole contract:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_publication_layer_adds_no_routes() {
    let routes = fs::read_to_string("tests/fixtures/api_routes.txt")
        .expect("tests/fixtures/api_routes.txt readable");
    for forbidden in [
        "/adaptive",
        "/producer",
        "/producers",
        "/change_set",
        "/changeset",
        "/snapshot",
    ] {
        assert!(
            !routes.contains(forbidden),
            "tests/fixtures/api_routes.txt contains `{forbidden}`: #324 adds no routes. \
             Any HTTP/SSE exposure needs explicit approval, and `api-change-approved` must \
             never be self-applied."
        );
    }
}

// =============================================================================
// The internal event module: three allowlists
// =============================================================================

#[test]
fn lint_the_internal_event_module_imports_only_what_it_is_allowed_to() {
    let file = parse_file_at(EVENT_MODULE);
    let imports = imports_of(&file);

    let unexpected: Vec<&String> = imports
        .iter()
        .filter(|import| !EVENT_ALLOWED_IMPORTS.contains(&import.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{EVENT_MODULE} imports something not on the allowlist: {unexpected:?}\n\
         This module holds the one producer type that must not reach a wire. A new import \
         may be entirely fine - add it to EVENT_ALLOWED_IMPORTS deliberately, having \
         checked it is not a serialization trait re-exported under another name."
    );
    assert!(
        !imports.is_empty(),
        "no imports were found at all, which means the parse is not reaching the file"
    );
}

#[test]
fn lint_adaptive_event_derives_only_what_it_is_allowed_to() {
    let file = parse_file_at(EVENT_MODULE);
    let item = adaptive_event_enum(&file)
        .unwrap_or_else(|| panic!("{EVENT_MODULE} must declare `enum {ADAPTIVE_EVENT}`"));

    let derived = derives_in(&item.attrs);
    let unexpected: Vec<&String> = derived
        .iter()
        .filter(|token| !ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&token.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{ADAPTIVE_EVENT} derives something not on the allowlist: {unexpected:?}\n\
         Permitted: {ADAPTIVE_EVENT_ALLOWED_DERIVES:?}. A derive under an alias or a \
         re-exported path is indistinguishable from any other name, so the list decides."
    );
    assert!(
        !derived.is_empty(),
        "no derives were found at all, which means the parse is not reaching the type"
    );
}

#[test]
fn lint_adaptive_event_carries_only_allowed_attributes() {
    // Closes what the previous commit recorded as residual. An attribute macro generates
    // code without being a derive, so inspecting derives alone cannot see it — and nothing
    // can, because the expansion is not in this file. Permission rather than detection.
    let file = parse_file_at(EVENT_MODULE);
    let item = adaptive_event_enum(&file)
        .unwrap_or_else(|| panic!("{EVENT_MODULE} must declare `enum {ADAPTIVE_EVENT}`"));

    let unexpected = disallowed_attributes(&item.attrs);
    assert!(
        unexpected.is_empty(),
        "{ADAPTIVE_EVENT} carries an attribute not on the allowlist: {unexpected:?}\n\
         Permitted: {ADAPTIVE_EVENT_ALLOWED_ATTRIBUTES:?}. An attribute macro expands to \
         code this file does not contain, so the list is the only thing that can decide."
    );
    assert!(
        !item.attrs.is_empty(),
        "no attributes were found at all, which means the parse is not reaching the type"
    );
}

#[test]
fn lint_the_internal_event_module_carries_only_allowed_attributes_anywhere() {
    // The production gate for attribute-driven generation. The enum-specific check below
    // audits only `AdaptiveEvent`'s own attributes, and a procedural macro emits arbitrary
    // items rather than only code about what it annotates — so an attribute macro on a
    // helper function, or a custom derive on a helper struct, could emit
    // `impl Serialize for AdaptiveEvent` with every enum-specific check clean.
    let file = parse_file_at(EVENT_MODULE);
    let rejected = disallowed_module_attributes(&file);
    assert!(
        rejected.is_empty(),
        "{EVENT_MODULE} carries attributes the module-wide policy forbids: {rejected:?}\n\
         Policy: `doc` anywhere; `derive` only on `enum {ADAPTIVE_EVENT}` holding only \
         {ADAPTIVE_EVENT_ALLOWED_DERIVES:?}; nothing else. A proc macro anywhere in this \
         module can emit an impl for {ADAPTIVE_EVENT}, so permission is module-wide."
    );

    // The policy must describe the module, not merely permit it: if the one real derive
    // vanished, the policy would be silently over-broad.
    let derive_owners: Vec<Vec<String>> = module_attributes(&file)
        .into_iter()
        .filter(|(_, path)| path == "derive")
        .map(|(owner, _)| owner)
        .collect();
    assert_eq!(
        derive_owners,
        vec![adaptive_event_owner()],
        "the module's derives changed; the policy now describes something else"
    );
}

#[test]
fn lint_the_internal_event_module_invokes_only_allowed_macros() {
    // A macro expands to arbitrary code - an `impl`, a `use`, another type - while
    // imports, derives and `ItemImpl` all stay clean. Checked at *every* depth, not only
    // item position: Rust lets a statement-position macro expand to item definitions, and
    // a trait impl is valid wherever it is written.
    let file = parse_file_at(EVENT_MODULE);
    let invocations = macro_invocations(&file);
    let unexpected: Vec<&String> = invocations
        .iter()
        .filter(|name| !EVENT_ALLOWED_MACROS.contains(&name.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "{EVENT_MODULE} invokes macros not on the allowlist: {unexpected:?}\n\
         A macro can generate an impl for {ADAPTIVE_EVENT} without appearing in any \
         import, derive or impl. Permitted: {EVENT_ALLOWED_MACROS:?}. If another becomes \
         necessary, add it here having checked what it expands to."
    );

    // The allowlist must describe the module rather than merely permit it: if the one real
    // invocation disappeared, the list would be silently over-broad.
    assert_eq!(
        invocations,
        vec!["tracing::trace".to_string()],
        "the module's macro invocations changed; the allowlist now describes something else"
    );
}

#[test]
fn lint_no_module_anywhere_implements_a_trait_for_adaptive_event() {
    // The crate-wide half. Rust lets an external trait be implemented for a crate-local
    // type from *any* module, and `src/producers/aggregator.rs` already imports
    // `AdaptiveEvent` — so a sibling can make it serializable while every `event.rs`
    // allowlist stays clean: no new import, derive, attribute, macro or impl in that file.
    //
    // The stated invariant is that the internal event cannot become serializable by
    // accident. That invariant is crate-wide, so the check is too. Found by CodeRabbit at
    // `59523f8`.
    let mut violations = Vec::new();
    let mut swept = 0usize;
    for (path, text) in rust_sources_under("src") {
        swept += 1;
        let file = parse_source(&path, &text);
        for implemented in trait_impls_for(&file, ADAPTIVE_EVENT) {
            violations.push(format!("{path}: impl {implemented} for {ADAPTIVE_EVENT}"));
        }
    }
    assert!(
        swept > 50,
        "the sweep covered only {swept} files, which means it is not walking src/"
    );
    assert!(
        violations.is_empty(),
        "{ADAPTIVE_EVENT} has hand-written trait impls somewhere in the crate: \
         {violations:?}\nAn external trait implemented for a crate-local type is legal from \
         any module, so this guarantee cannot be enforced in one file. If an impl becomes \
         necessary, allow it here having checked it is not a serialization trait under \
         another name."
    );
}

// =============================================================================
// Boundary 4: a forbidden type cannot be renamed out of reach of the other three
//
// Every lint above matches a *name*: `AdaptiveEvent` for the impl sweeps, `adaptive` and
// `producers` for the reference sweeps. A second name defeats all of them at once, and none of
// them would report anything - the failure mode is a green test file, which is exactly the
// class this file exists to stop reproducing. Rather than resolve aliases, the aliases are
// banned; see `adaptive_event_aliases` and `root_launderings` for why that trade is the right
// way round.
// =============================================================================

/// The files exempt from both reference sweeps, and therefore the ones that can launder.
const ROOT_FILES: &[&str] = &["src/lib.rs", "src/main.rs"];

#[test]
fn an_alias_for_the_internal_event_is_rejected() {
    // Mutation probes. Each is an escape that the impl sweeps alone cannot see, because the
    // impl's self type is not spelled `AdaptiveEvent`.
    for (label, source) in [
        (
            "a bare local alias plus a serde impl on it",
            "type Ev = AdaptiveEvent;\nimpl serde::Serialize for Ev {}\n",
        ),
        (
            "an alias written through the module path",
            "type Ev = crate::producers::event::AdaptiveEvent;\n",
        ),
        (
            "an alias reached through super",
            "mod wire {\n    type Ev = super::AdaptiveEvent;\n}\n",
        ),
        (
            "a renamed import",
            "use crate::producers::event::AdaptiveEvent as Ev;\n",
        ),
        (
            "a renamed import inside a grouped use tree",
            "use crate::producers::event::{AdaptiveEvent as Ev, ProducerKey};\n",
        ),
        (
            "an alias declared inside a nested module",
            "mod outer {\n    mod inner {\n        type Ev = crate::producers::event::AdaptiveEvent;\n    }\n}\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !adaptive_event_aliases(&file).is_empty(),
            "{label}: an alias for the internal event went unreported:\n{source}"
        );
    }
}

#[test]
fn a_type_that_merely_resembles_the_internal_event_is_not_rejected() {
    // Near misses, so the prohibition cannot be satisfied by being indiscriminate.
    for (label, source) in [
        (
            "a prefix-sharing type name",
            "type Ev = AdaptiveEventLog;\n",
        ),
        (
            "an import with no rename",
            "use crate::producers::event::AdaptiveEvent;\n",
        ),
        (
            "an unrelated alias",
            "type Zones = std::collections::BTreeMap<String, Zone>;\n",
        ),
        (
            "a rename of something else entirely",
            "use serde::Serialize as S;\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            adaptive_event_aliases(&file).is_empty(),
            "{label}: the alias prohibition over-reached: {:?}\n{source}",
            adaptive_event_aliases(&file)
        );
    }
}

#[test]
fn lint_no_source_aliases_the_internal_event() {
    let mut violations = Vec::new();
    let mut swept = 0usize;
    for (path, text) in rust_sources_under("src") {
        swept += 1;
        let file = parse_source(&path, &text);
        for alias in adaptive_event_aliases(&file) {
            violations.push(format!("{path}: {alias}"));
        }
    }
    assert!(
        swept > 50,
        "the sweep covered only {swept} files, which means it is not walking src/"
    );
    assert!(
        violations.is_empty(),
        "{ADAPTIVE_EVENT} is reachable under a second name, which defeats every impl and \
         reference lint at once: {violations:?}\nName it directly, or extend the impl sweeps to \
         follow the alias before allowing one."
    );
}

#[test]
fn a_root_reexport_of_a_forbidden_type_is_rejected() {
    // The other half. These live in a file both reference sweeps skip, so nothing else looks.
    for (label, source) in [
        (
            "a renamed pub use of a contract type",
            "pub use crate::adaptive::ProducerDocument as Doc;\n",
        ),
        (
            "an un-renamed pub use",
            "pub use crate::adaptive::ProducerDocument;\n",
        ),
        (
            "a pub(crate) use of the publication layer",
            "pub(crate) use crate::producers::ProducerAggregator as Agg;\n",
        ),
        (
            "a pub use of a whole forbidden module",
            "pub use crate::adaptive;\n",
        ),
        (
            "a public type alias",
            "pub type Doc = crate::adaptive::ProducerDocument;\n",
        ),
        (
            "a re-export nested in a public shim module",
            "pub mod shim {\n    pub use crate::adaptive::ProducerDocument as D;\n}\n",
        ),
        (
            "a grouped re-export hiding one forbidden leaf",
            "pub use crate::{bus::BusEvent, adaptive::ProducerDocument as D};\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !root_launderings(&file).is_empty(),
            "{label}: a root re-export laundered a forbidden type:\n{source}"
        );
    }
}

#[test]
fn a_root_use_that_launders_nothing_is_not_rejected() {
    for (label, source) in [
        (
            "a private use, which no other module can reach",
            "use crate::producers::ProducerAggregator;\n",
        ),
        (
            "a public re-export of an unrelated module",
            "pub use crate::bus::BusEvent;\n",
        ),
        (
            "a prefix-sharing sibling module",
            "pub use crate::adaptive_extras::Helper;\n",
        ),
        (
            "a public alias of an unrelated type",
            "pub type Zones = crate::bus::ZoneMap;\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            root_launderings(&file).is_empty(),
            "{label}: the re-export prohibition over-reached: {:?}\n{source}",
            root_launderings(&file)
        );
    }
}

#[test]
fn the_impl_sweep_alone_is_blind_to_an_aliased_serde_impl() {
    // The reason the prohibition exists rather than an extension of `trait_impls_for`, kept as
    // an executable statement rather than a claim in a comment. If `trait_impls_for` ever
    // learns to resolve aliases, this fails and the prohibition can be reconsidered - which is
    // the right way for that decision to come back up.
    let source = "type Ev = AdaptiveEvent;\nimpl serde::Serialize for Ev {}\n";
    let file = parse_source("probe.rs", source);
    assert!(
        trait_impls_for(&file, ADAPTIVE_EVENT).is_empty(),
        "trait_impls_for now sees through an alias, so the alias prohibition may be redundant"
    );
    assert!(
        !adaptive_event_aliases(&file).is_empty(),
        "an escape no other lint can see must be caught by the alias prohibition"
    );
}

#[test]
fn the_reference_sweep_alone_is_blind_to_a_laundered_root_reexport() {
    // The surface half of the escape: `crate::Doc` names neither forbidden module, so the
    // sweep over `src/` has nothing to report even though this is a contract type in a
    // serializable position.
    let surface = parse_source("probe.rs", "use crate::Doc;\nfn f(d: Doc) -> Doc { d }\n");
    assert!(
        forbidden_module_references(&surface, module_depth("src/api/mod.rs")).is_empty(),
        "the reference sweep now resolves a laundered alias, so the root prohibition may be \
         redundant"
    );
    // Which is why it has to be stopped where it is created.
    let root = parse_source(
        "src/lib.rs",
        "pub use crate::adaptive::ProducerDocument as Doc;\n",
    );
    assert!(
        !root_launderings(&root).is_empty(),
        "the escape must be caught at the root that creates the name"
    );
}

#[test]
fn lint_the_composition_root_launders_no_forbidden_type() {
    let mut violations = Vec::new();
    let mut checked = 0usize;
    for path in ROOT_FILES {
        let Ok(text) = fs::read_to_string(path) else {
            continue;
        };
        checked += 1;
        let file = parse_source(path, &text);
        for laundering in root_launderings(&file) {
            violations.push(format!("{path}: {laundering}"));
        }
    }
    assert_eq!(
        checked,
        ROOT_FILES.len(),
        "a composition root named in ROOT_FILES was not found, so this lint checked {checked} \
         of {} files and would pass by reading nothing",
        ROOT_FILES.len()
    );
    assert!(
        violations.is_empty(),
        "a crate root re-exports a contract or publication type under a name the reference \
         sweeps cannot see: {violations:?}\nThe roots are exempt from those sweeps precisely \
         because they must name these modules to wire them; that exemption does not extend to \
         handing the names onward."
    );
}

#[test]
fn red_alias_prohibition_sees_through_non_path_type_syntax() {
    // The alias checker matched only `Type::Path`, so any wrapping syntax slipped past it and
    // took the impl sweep with it - the impl's self type is then not spelled `AdaptiveEvent`.
    for (label, source) in [
        (
            "a parenthesised alias",
            "type Ev = (AdaptiveEvent);\nimpl serde::Serialize for Ev {}\n",
        ),
        (
            "a doubly parenthesised alias",
            "type Ev = ((AdaptiveEvent));\n",
        ),
        (
            "the event as a generic argument",
            "type Ev = Box<AdaptiveEvent>;\n",
        ),
        (
            "the event inside a nested generic argument",
            "type Ev = Result<Vec<AdaptiveEvent>, ()>;\n",
        ),
        (
            "the event behind a reference",
            "type Ev = &'static AdaptiveEvent;\n",
        ),
        ("the event in a tuple", "type Ev = (AdaptiveEvent, u8);\n"),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !adaptive_event_aliases(&file).is_empty(),
            "{label}: an alias for the internal event went unreported:\n{source}"
        );
    }
}

#[test]
fn red_root_laundering_covers_exported_function_signatures() {
    // The root exemption also covers `pub fn`, which the checker never looked at: a public
    // function whose signature names a contract type hands it to any surface that calls it,
    // and the reference sweeps skip the roots entirely.
    for (label, source) in [
        (
            "a public function returning a contract type",
            "pub fn leak() -> crate::adaptive::ProducerDocument { todo!() }\n",
        ),
        (
            "a public function taking one",
            "pub fn sink(d: crate::adaptive::ProducerDocument) {}\n",
        ),
        (
            "a public function returning the publication layer",
            "pub fn agg() -> crate::producers::ProducerAggregator { todo!() }\n",
        ),
        (
            "a public function returning it wrapped",
            "pub fn maybe() -> Option<crate::adaptive::ProducerDocument> { None }\n",
        ),
        (
            "a public const",
            "pub const D: crate::adaptive::ProducerDocument = todo!();\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !root_launderings(&file).is_empty(),
            "{label}: a root laundered a forbidden type through a signature:\n{source}"
        );
    }
}

#[test]
fn red_root_laundering_resolves_private_alias_chains_in_exported_signatures() {
    for (label, source) in [
        (
            "a private alias returned publicly",
            "type Hidden = crate::adaptive::ProducerDocument; pub fn leak() -> Hidden { todo!() }\n",
        ),
        (
            "a transitive private alias returned publicly",
            "type Hidden = crate::adaptive::ProducerDocument; type MoreHidden = Option<Hidden>; pub fn leak() -> MoreHidden { todo!() }\n",
        ),
        (
            "a private publication alias in a public field",
            "type Hidden = crate::producers::ProducerSnapshot; pub struct Envelope { pub value: Hidden }\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !root_launderings(&file).is_empty(),
            "{label}: a private alias laundered a forbidden type:\n{source}"
        );
    }
}

#[test]
fn red_root_laundering_resolves_private_import_aliases_and_shadowing() {
    for (label, source) in [
        (
            "a private import alias returned publicly",
            "use crate::adaptive::ProducerDocument as Hidden; pub fn leak() -> Hidden { todo!() }\n",
        ),
        (
            "a private import without rename returned publicly",
            "use crate::adaptive::ProducerDocument; pub fn leak() -> ProducerDocument { todo!() }\n",
        ),
        (
            "a nested benign alias cannot overwrite a forbidden root alias",
            "type Hidden = crate::adaptive::ProducerDocument; mod nested { type Hidden = u8; } pub fn leak() -> Hidden { todo!() }\n",
        ),
        (
            "a private forbidden glob feeding a public signature",
            "use crate::adaptive::*; pub fn leak() -> ProducerDocument { todo!() }\n",
        ),
        (
            "a transitive private module alias feeding a public signature",
            "use crate::adaptive as hidden; use hidden::ProducerDocument as Doc; pub fn leak() -> Doc { todo!() }\n",
        ),
        (
            "a public re-export through a private module alias",
            "use crate::adaptive as hidden; pub use hidden::ProducerDocument;\n",
        ),
        (
            "a private glob through a private module alias",
            "use crate::adaptive as hidden; use hidden::*; pub fn leak() -> ProducerDocument { todo!() }\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !root_launderings(&file).is_empty(),
            "{label}: a private binding laundered a forbidden type:\n{source}"
        );
    }
}

#[test]
fn red_root_laundering_prohibits_exported_macros_in_exempt_roots() {
    for source in [
        "#[macro_export] macro_rules! leak { () => { crate::adaptive::ProducerDocument } }\n",
        "macro_rules! leak { () => { crate::adaptive::ProducerDocument } } pub(crate) use leak;\n",
        "macro_rules! leak { () => { pub fn leak() -> crate::adaptive::ProducerDocument { todo!() } } } leak!();\n",
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !root_launderings(&file).is_empty(),
            "an exported macro could launder an arbitrary forbidden path:\n{source}"
        );
    }
}

#[test]
fn red_root_laundering_covers_exported_aggregate_and_method_signatures() {
    for (label, source) in [
        (
            "a public tuple-struct field",
            "pub struct Envelope(pub crate::adaptive::ProducerDocument);\n",
        ),
        (
            "a public named struct field",
            "pub struct Envelope { pub doc: crate::adaptive::ProducerDocument }\n",
        ),
        (
            "a public enum payload",
            "pub enum Message { Document(crate::adaptive::ProducerDocument) }\n",
        ),
        (
            "a public trait method",
            "pub trait Source { fn document(&self) -> crate::adaptive::ProducerDocument; }\n",
        ),
        (
            "a public inherent method",
            "pub struct Root; impl Root { pub fn document(&self) -> crate::adaptive::ProducerDocument { todo!() } }\n",
        ),
        (
            "a public union field",
            "pub union Envelope { pub doc: std::mem::ManuallyDrop<crate::adaptive::ProducerDocument> }\n",
        ),
        (
            "a public trait associated const",
            "pub trait Source { const DOCUMENT: crate::adaptive::ProducerDocument; }\n",
        ),
        (
            "a public trait associated type default",
            "pub trait Source { type Document = crate::adaptive::ProducerDocument; }\n",
        ),
        (
            "a trait impl associated type",
            "pub struct Root; impl Iterator for Root { type Item = crate::adaptive::ProducerDocument; fn next(&mut self) -> Option<Self::Item> { None } }\n",
        ),
        (
            "a public function where-clause",
            "pub fn leak<T>() where T: Iterator<Item = crate::adaptive::ProducerDocument> {}\n",
        ),
        (
            "a public trait supertrait bound",
            "pub trait Source: Iterator<Item = crate::adaptive::ProducerDocument> {}\n",
        ),
        (
            "a public associated-type bound without a default",
            "pub trait Source { type Documents: Iterator<Item = crate::adaptive::ProducerDocument>; }\n",
        ),
        (
            "a root trait impl on a forbidden publication type",
            "impl serde::Serialize for crate::producers::ProducerSnapshot { fn serialize<S>(&self, _: S) -> Result<S::Ok, S::Error> where S: serde::Serializer { todo!() } }\n",
        ),
        (
            "a root impl on a private forbidden alias",
            "type Hidden = crate::producers::ProducerSnapshot; impl Hidden { pub fn leak(&self) {} }\n",
        ),
        (
            "an impl-level where-clause",
            "pub struct Root<T>(T); impl<T> Root<T> where T: Iterator<Item = crate::adaptive::ProducerDocument> { pub fn ok(&self) {} }\n",
        ),
        (
            "an exported foreign function",
            "unsafe extern \"Rust\" { pub fn leak() -> crate::adaptive::ProducerDocument; }\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            !root_launderings(&file).is_empty(),
            "{label}: a root laundered a forbidden type through an exported aggregate or method:\n{source}"
        );
    }
}

#[test]
fn red_root_laundering_still_ignores_innocent_signatures() {
    for (label, source) in [
        (
            "a private function naming a forbidden type",
            "fn wire() -> crate::producers::ProducerAggregator { todo!() }\n",
        ),
        (
            "a public function over unrelated types",
            "pub fn zones() -> Vec<crate::bus::Zone> { vec![] }\n",
        ),
        (
            "a prefix-sharing sibling module",
            "pub fn helper() -> crate::adaptive_extras::Helper { todo!() }\n",
        ),
    ] {
        let file = parse_source("probe.rs", source);
        assert!(
            root_launderings(&file).is_empty(),
            "{label}: the root prohibition over-reached: {:?}\n{source}",
            root_launderings(&file)
        );
    }
}

#[test]
fn every_adaptive_timestamp_field_is_in_the_admission_validation_ledger() {
    #[derive(Default)]
    struct TimestampFieldVisitor {
        fields: std::collections::BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for TimestampFieldVisitor {
        fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
            for (index, field) in item.fields.iter().enumerate() {
                if type_tail_is(&field.ty, "Timestamp") {
                    let field_name = field
                        .ident
                        .as_ref()
                        .map_or_else(|| index.to_string(), ToString::to_string);
                    self.fields.insert(format!("{}.{field_name}", item.ident));
                }
            }
            visit::visit_item_struct(self, item);
        }
    }

    let sources = rust_sources_under("src/adaptive");
    assert!(!sources.is_empty(), "no adaptive sources were inspected");
    let mut visitor = TimestampFieldVisitor::default();
    for (path, source) in sources {
        visitor.visit_file(&parse_source(&path, &source));
    }
    let expected = std::collections::BTreeSet::from([
        "ChangeSet.created_at".to_string(),
        "ChangeSet.updated_at".to_string(),
        "Divergence.detected_at".to_string(),
        "Freshness.observed_at".to_string(),
        "LaneHealth.last_success".to_string(),
        "OutcomeTransition.at".to_string(),
        "Retention.expires_at".to_string(),
    ]);
    assert_eq!(
        visitor.fields, expected,
        "Timestamp fields changed; add admission validation and a malformed-value counterexample before updating this ledger"
    );
}

#[test]
fn lint_the_run_module_stays_serialization_free_and_std_only() {
    // `AdapterRunId` and `PublicationOrigin` are stamped onto every ingress `AdaptiveEvent`, so
    // they are now part of the internal bus's payload shape. The event enum's own guarantee -
    // that it cannot reach a wire - is only as strong as the types it carries, and none of the
    // allowlists above look at this file. Held to the same rule: no serde, and nothing from
    // outside `std`, so a serialization trait cannot arrive as a transitive dependency either.
    let path = "src/producers/run.rs";
    let file = parse_file_at(path);

    let foreign: Vec<String> = imports_of(&file)
        .into_iter()
        .filter(|import| !import.starts_with("std::"))
        .collect();
    assert!(
        foreign.is_empty(),
        "{path} imports something outside std: {foreign:?}\nTypes carried by AdaptiveEvent must \
         stay as unserializable as the event itself."
    );

    let mut serializable = Vec::new();
    for item in &file.items {
        let (name, attrs) = match item {
            Item::Struct(inner) => (inner.ident.to_string(), &inner.attrs),
            Item::Enum(inner) => (inner.ident.to_string(), &inner.attrs),
            _ => continue,
        };
        for derive in derives_in(attrs) {
            if derive.contains("Serialize") || derive.contains("Deserialize") {
                serializable.push(format!("derive({derive}) on {name}"));
            }
        }
    }
    assert!(
        serializable.is_empty(),
        "{path} derives serde for a type the internal bus carries: {serializable:?}"
    );
    assert!(
        !file.items.is_empty(),
        "no items were found at all, which means the parse is not reaching {path}"
    );
}

#[test]
fn lint_adaptive_event_implements_no_traits() {
    // The third surface, and the one a re-export hides best: `impl EventWire for
    // AdaptiveEvent` needs neither serde in the file nor a derive.
    let file = parse_file_at(EVENT_MODULE);
    let impls = trait_impls_for(&file, ADAPTIVE_EVENT);
    assert!(
        impls.is_empty(),
        "{ADAPTIVE_EVENT} has hand-written trait impls: {impls:?}\n\
         None are expected. If one becomes necessary, allow it here having checked it is \
         not a serialization trait under another name."
    );
}

// =============================================================================
// The publication layer is server-only
// =============================================================================

#[test]
fn lint_producers_module_is_server_gated() {
    // Opposite polarity to `pub mod adaptive;`, which must be *ungated* so the WASM build
    // can use the contract types. `src/producers/` depends on `crate::bus` and `tokio`, so
    // an ungated declaration breaks `dx build --platform web` in CI rather than anything a
    // host `cargo test` would notice.
    let file = parse_file_at("src/lib.rs");
    let gates = module_cfg_gates(&file, PRODUCERS_MODULE)
        .unwrap_or_else(|| panic!("src/lib.rs must declare `mod {PRODUCERS_MODULE}`"));
    assert!(
        gates.iter().any(gate_is_server_only),
        "`mod {PRODUCERS_MODULE}` must be gated on `feature = \"server\"` and nothing \
         looser; it depends on crate::bus and tokio, neither of which exists in the WASM \
         build."
    );
}

#[test]
fn lint_publication_layer_is_reachable_only_from_the_composition_root() {
    let mut referrers = Vec::new();
    for (path, text) in rust_sources_under("src") {
        if path.starts_with("src/producers/") || path == "src/lib.rs" || path == "src/main.rs" {
            continue;
        }
        let file = parse_source(&path, &text);
        if forbidden_module_references(&file, module_depth(&path))
            .iter()
            .any(|reference| reference.ends_with("::producers"))
        {
            referrers.push(path);
        }
    }
    assert!(
        referrers.is_empty(),
        "only the composition root may reference the publication layer, found: {referrers:?}"
    );
}

// =============================================================================
// The exploit corpus
//
// Every escape found against the previous text scanner, kept as source and run through the
// production helpers. This is the regression cover for the architecture change: if a future
// edit reintroduces text scanning, these are what fail.
// =============================================================================

/// Parse a snippet exactly as the lints parse a file.
fn snippet(source: &str) -> File {
    parse_source("<probe>", source)
}

#[test]
fn every_known_serialization_escape_is_rejected() {
    let exploits = [
        // CodeRabbit: only the nearest derive was inspected.
        (
            "stacked derive",
            "#[derive(Serialize)]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
        // CodeRabbit: a conditional derive was never looked for.
        (
            "cfg_attr derive",
            "#[cfg_attr(feature = \"x\", derive(Serialize))]\n#[derive(Debug)]\npub enum AdaptiveEvent {}\n",
        ),
        // CodeRabbit: an attribute wrapped across lines was discarded by the line walk.
        (
            "wrapped derive",
            "#[derive(\n    Debug,\n    Serialize,\n)]\npub enum AdaptiveEvent {}\n",
        ),
        (
            "wrapped cfg_attr",
            "#[cfg_attr(\n    feature = \"x\",\n    derive(serde::Serialize)\n)]\n#[derive(Debug)]\npub enum AdaptiveEvent {}\n",
        ),
        // CodeRabbit: an alias contains no forbidden substring.
        (
            "aliased derive",
            "use serde::Serialize as EventWire;\n#[derive(EventWire)]\npub enum AdaptiveEvent {}\n",
        ),
        // CodeRabbit: a raw string ended the attribute early and hid what followed.
        (
            "raw string then wrapped derive",
            "#[doc = r##\"quote: \" then ] remains data\"##]\n#[cfg_attr(\n    feature = \"x\",\n    derive(serde::Serialize)\n)]\npub enum AdaptiveEvent {}\n",
        ),
        // Codex: an escaped apostrophe under-consumed and mis-lexed everything after it.
        (
            "escaped-quote char literal then derive",
            "#[doc = \"sep is '\\''\"]\n#[derive(Serialize)]\npub enum AdaptiveEvent {}\n",
        ),
        // A derive by fully-qualified path, needing no import at all.
        (
            "fully-qualified re-exported derive",
            "#[derive(crate::wire::EventWire)]\npub enum AdaptiveEvent {}\n",
        ),
        // Nested conditionals.
        (
            "nested cfg_attr",
            "#[cfg_attr(unix, cfg_attr(feature = \"x\", derive(Serialize)))]\npub enum AdaptiveEvent {}\n",
        ),
    ];

    for (label, source) in exploits {
        let file = snippet(source);
        let item = adaptive_event_enum(&file)
            .unwrap_or_else(|| panic!("{label}: enum not located:\n{source}"));
        let derived = derives_in(&item.attrs);
        assert!(
            derived
                .iter()
                .any(|token| !ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&token.as_str())),
            "{label}: a forbidden derive was admitted. Derives seen: {derived:?}\n{source}"
        );
    }
}

#[test]
fn every_known_import_escape_is_rejected() {
    let exploits = [
        ("plain alias", "use serde::Serialize as W;"),
        ("crate-local re-export", "use crate::wire::EventWire;"),
        // Codex: `pub use` was the one visibility form the boundary rule rejected.
        ("pub use", "pub use serde::Serialize as W;"),
        ("pub(crate) use", "pub(crate) use serde::Serialize as W;"),
        ("pub(super) use", "pub(super) use serde::Deserialize as R;"),
        (
            "pub(in path) use",
            "pub(in crate::producers) use serde::Serialize as W;",
        ),
        // Codex: an attribute is not a statement boundary, so every decorated import was
        // invisible - including a perfectly ordinary `#[allow(unused_imports)]`.
        (
            "cfg-decorated",
            "#[cfg(any())]\nuse crate::wire::EventWire;",
        ),
        (
            "allow-decorated",
            "#[allow(unused_imports)]\nuse crate::wire::EventWire;",
        ),
        (
            "cfg_attr-decorated",
            "#[cfg_attr(test, allow(unused))]\nuse crate::wire::EventWire;",
        ),
        (
            "attribute and visibility together",
            "#[cfg(any())]\npub use crate::wire::EventWire;",
        ),
        ("grouped", "use serde::{Serialize as W, Deserializer};"),
        ("glob", "use crate::wire::*;"),
        (
            "wrapped across lines",
            "use crate::wire::{\n    EventWire,\n};",
        ),
    ];

    for (label, source) in exploits {
        let file = snippet(source);
        let imports = imports_of(&file);
        assert!(
            !imports.is_empty(),
            "{label}: no import was seen at all:\n{source}"
        );
        assert!(
            imports
                .iter()
                .any(|import| !EVENT_ALLOWED_IMPORTS.contains(&import.as_str())),
            "{label}: the allowlist admitted {imports:?}\n{source}"
        );
    }
}

#[test]
fn code_generating_macros_cannot_target_adaptive_event() {
    // The residual the previous commit *documented* rather than closed. Codex was right
    // that it is a live false pass, not a theoretical one: both of these leave imports,
    // derives and `ItemImpl` entirely clean while generating whatever they like.
    //
    // An attribute macro on the item:
    let attribute_macro = "#[event_wire]\npub enum AdaptiveEvent {}\n";
    let file = snippet(attribute_macro);
    let item = adaptive_event_enum(&file).expect("enum located");
    let unexpected = disallowed_attributes(&item.attrs);
    assert!(
        !unexpected.is_empty(),
        "an attribute macro was admitted: {:?}\n{attribute_macro}",
        attribute_paths(&item.attrs)
    );

    // A function-like macro in item position:
    let item_macro = "make_event_wire!(AdaptiveEvent);\npub enum AdaptiveEvent {}\n";
    let invocations = macro_invocations(&snippet(item_macro));
    assert!(
        !invocations.is_empty(),
        "an item-position macro invocation was invisible:\n{item_macro}"
    );

    // Nested inside a module, so the walk cannot be top-level only.
    let nested = "mod inner {\n    make_event_wire!(AdaptiveEvent);\n}\n";
    assert!(
        !macro_invocations(&snippet(nested)).is_empty(),
        "a nested item macro was invisible:\n{nested}"
    );

    // Inside a function body. Rust permits a macro in statement position to expand to item
    // definitions, and a trait impl is valid wherever it is written — so restricting the
    // walk to item position leaves the escape open one level down. Raised by the reviewer
    // against the first version of this closure, which did exactly that.
    for (label, source) in [
        (
            "statement position",
            "fn f() {\n    make_event_wire!(AdaptiveEvent);\n}\n",
        ),
        (
            "nested block",
            "fn f() {\n    if x {\n        make_event_wire!(AdaptiveEvent);\n    }\n}\n",
        ),
        (
            "expression position",
            "fn f() {\n    let _ = make_event_wire!(AdaptiveEvent);\n}\n",
        ),
        (
            "inside an impl method",
            "impl Bus {\n    fn f(&self) {\n        make_event_wire!(AdaptiveEvent);\n    }\n}\n",
        ),
        (
            "inside a nested closure",
            "fn f() {\n    let g = || { make_event_wire!(AdaptiveEvent); };\n}\n",
        ),
    ] {
        let found = macro_invocations(&snippet(source));
        assert!(
            found.iter().any(|name| name == "make_event_wire"),
            "{label}: a macro invocation below item position was invisible: {found:?}\n{source}"
        );
        assert!(
            found
                .iter()
                .any(|name| !EVENT_ALLOWED_MACROS.contains(&name.as_str())),
            "{label}: the allowlist admitted {found:?}\n{source}"
        );
    }

    // Derive-adjacent attributes that are inert on their own must still be rejected,
    // because inertness is a property of the *expansion*, which is not in this file.
    for source in [
        "#[serde(rename_all = \"snake_case\")]\npub enum AdaptiveEvent {}\n",
        "#[event_wire(with = \"json\")]\npub enum AdaptiveEvent {}\n",
        "#[cfg_attr(feature = \"x\", event_wire)]\npub enum AdaptiveEvent {}\n",
    ] {
        let file = snippet(source);
        let item = adaptive_event_enum(&file).expect("enum located");
        assert!(
            !disallowed_attributes(&item.attrs).is_empty(),
            "a non-derive attribute was admitted:\n{source}"
        );
    }
}

#[test]
fn a_proc_macro_on_any_other_item_cannot_generate_for_adaptive_event() {
    // The enum-specific checks audit `AdaptiveEvent`'s own attributes. A procedural macro
    // emits arbitrary *items*, not only code about the thing it annotates — so an
    // attribute macro on a helper function, or a custom derive on a helper struct, can emit
    // `impl Serialize for AdaptiveEvent` while the enum's attrs, the imports, the
    // `ItemImpl` list and the function-like macro allowlist all stay clean.
    //
    // Found by Codex at `4a817c9`. Each case asserts both halves: that the enum-only check
    // passes it (the false pass), and that the module-wide policy rejects it (the fix).
    for (label, source) in [
        (
            "attribute macro on a helper fn",
            "#[generate_event_wire]\nfn helper() {}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
        (
            "custom derive on a helper struct",
            "#[derive(GenerateAdaptiveWire)]\nstruct Helper;\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
        (
            "attribute macro on an impl block",
            "#[generate_event_wire]\nimpl Helper {}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
        (
            "attribute macro on a nested item",
            "mod inner {\n    #[generate_event_wire]\n    fn helper() {}\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
        (
            "custom derive on a helper enum",
            "#[derive(GenerateAdaptiveWire)]\nenum Helper { A }\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
        (
            "attribute macro on a struct field",
            "struct Helper {\n    #[generate_event_wire]\n    field: u8,\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
    ] {
        let file = snippet(source);

        // The false pass: every enum-specific check is clean.
        let item = adaptive_event_enum(&file).expect("enum located");
        assert!(
            disallowed_attributes(&item.attrs).is_empty(),
            "{label}: precondition failed - the enum's own attrs were not clean"
        );
        assert!(
            derives_in(&item.attrs)
                .iter()
                .all(|token| ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&token.as_str())),
            "{label}: precondition failed - the enum's derives were not clean"
        );
        assert!(
            imports_of(&file).is_empty() && trait_impls_for(&file, ADAPTIVE_EVENT).is_empty(),
            "{label}: precondition failed - imports or impls were not clean"
        );
        assert!(
            macro_invocations(&file).is_empty(),
            "{label}: precondition failed - a function-like macro was present"
        );

        // The fix: the module-wide policy rejects it.
        let rejected = disallowed_module_attributes(&file);
        assert!(
            !rejected.is_empty(),
            "{label}: a proc macro on another item was admitted:\n{source}"
        );
    }
}

#[test]
fn a_nested_decoy_named_adaptive_event_cannot_borrow_the_real_ones_permission() {
    // The owner was stored as a lossy label — the last frame only — so a *nested* item that
    // happens to share the name produced the same string as the real one and inherited its
    // permission to derive. The token audit looks at `adaptive_event_enum`, which searches
    // top-level items only, so the decoy's derive was audited by nobody.
    //
    // Found by Codex at `24c9f56`.
    let collision = "mod inner {\n    #[derive(GenerateAdaptiveWire)]\n    enum AdaptiveEvent {}\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n";
    let file = snippet(collision);

    // The false pass, spelled out: the top-level enum is clean, so every enum-specific
    // check passes.
    let item = adaptive_event_enum(&file).expect("top-level enum located");
    assert_eq!(
        derives_in(&item.attrs),
        vec!["Debug".to_string(), "Clone".to_string()],
        "precondition failed - the top-level enum's derives were not clean"
    );

    assert!(
        !disallowed_module_attributes(&file).is_empty(),
        "a nested decoy sharing the name inherited permission to derive:\n{collision}"
    );

    // Depth matters, not just the name: the same decoy two modules down.
    let deeper = "mod a {\n    mod b {\n        #[derive(GenerateAdaptiveWire)]\n        enum AdaptiveEvent {}\n    }\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n";
    assert!(
        !disallowed_module_attributes(&snippet(deeper)).is_empty(),
        "a decoy two modules down inherited permission:\n{deeper}"
    );

    // And a decoy carrying an *allowed* derive is still wrong: permission belongs to the
    // one top-level item, not to anything wearing its name.
    let allowed_token_decoy = "mod inner {\n    #[derive(Debug)]\n    enum AdaptiveEvent {}\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n";
    assert!(
        !disallowed_module_attributes(&snippet(allowed_token_decoy)).is_empty(),
        "a nested decoy was permitted to derive at all:\n{allowed_token_decoy}"
    );
}

#[test]
fn attributes_on_variants_and_fields_are_located_precisely() {
    // A variant or field attribute inherited the enclosing item's label, so a `derive`
    // there read as a derive on the enum and was permitted. A built-in derive in that
    // position does not compile — but the policy should say where an attribute *is* rather
    // than lean on a later compile to catch it, because a proc-macro attribute there does
    // compile. Raised alongside the decoy finding.
    for (label, source) in [
        (
            "derive on a variant",
            "pub enum AdaptiveEvent {\n    #[derive(GenerateAdaptiveWire)]\n    A,\n}\n",
        ),
        (
            "derive on a field",
            "pub enum AdaptiveEvent {\n    A {\n        #[derive(GenerateAdaptiveWire)]\n        x: u8,\n    },\n}\n",
        ),
        (
            "attribute macro on a variant",
            "pub enum AdaptiveEvent {\n    #[generate_event_wire]\n    A,\n}\n",
        ),
    ] {
        assert!(
            !disallowed_module_attributes(&snippet(source)).is_empty(),
            "{label}: an attribute below the item borrowed the item's permission:\n{source}"
        );
    }

    // Doc comments in those positions remain fine.
    let documented = "#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n    /// variant docs\n    A {\n        /// field docs\n        x: u8,\n    },\n}\n";
    assert!(
        disallowed_module_attributes(&snippet(documented)).is_empty(),
        "documented variants or fields were rejected: {:?}",
        disallowed_module_attributes(&snippet(documented))
    );
}

#[test]
fn the_module_wide_attribute_policy_accepts_the_module_as_it_is() {
    // Positive control. Doc comments anywhere, and one `derive` on `AdaptiveEvent` holding
    // only `Debug` and `Clone`, is exactly what `src/producers/event.rs` carries — so the
    // policy describes the module rather than merely constraining it.
    for (label, source) in [
        ("bare enum", "#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n"),
        (
            "doc comments on helpers and variants",
            "/// helper docs\nfn helper() {}\n/// enum docs\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n    /// variant docs\n    A,\n}\n",
        ),
        (
            "doc on a struct field",
            "struct Helper {\n    /// field docs\n    field: u8,\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
        (
            "an inherent impl with documented methods",
            "impl Helper {\n    /// method docs\n    fn f(&self) {}\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        ),
    ] {
        assert!(
            disallowed_module_attributes(&snippet(source)).is_empty(),
            "{label}: the policy rejected something the module legitimately has: {:?}\n{source}",
            disallowed_module_attributes(&snippet(source))
        );
    }

    // And a derive on the enum that is *not* allowlisted is still rejected module-wide, so
    // the two checks agree rather than one masking the other.
    assert!(
        !disallowed_module_attributes(&snippet(
            "#[derive(Debug, Serialize)]\npub enum AdaptiveEvent {}\n"
        ))
        .is_empty(),
        "a forbidden derive on the enum passed the module-wide policy"
    );
}

#[test]
fn the_attribute_allowlist_accepts_what_the_type_actually_carries() {
    // Positive control. `#[derive(...)]` and doc comments are what the real enum has;
    // rejecting either would make the list unmaintainable.
    for source in [
        "#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        "/// docs\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n",
        "#[doc = \"explicit\"]\n#[derive(Debug)]\npub enum AdaptiveEvent {}\n",
    ] {
        let file = snippet(source);
        let item = adaptive_event_enum(&file).expect("enum located");
        assert!(
            disallowed_attributes(&item.attrs).is_empty(),
            "an allowed attribute was rejected: {:?}\n{source}",
            disallowed_attributes(&item.attrs)
        );
    }
    assert!(
        macro_invocations(&snippet("pub enum AdaptiveEvent {}\n")).is_empty(),
        "an item macro was invented where there is none"
    );
}

#[test]
fn a_nested_module_cannot_hide_an_import_or_a_trait_impl() {
    // `imports_of` and `trait_impls_for` iterated `File.items` only, so a nested module was
    // a blind spot for both at once:
    //
    //     mod wire {
    //         impl serde::Serialize for super::AdaptiveEvent { … }
    //     }
    //
    // No new attribute, no derive, no macro invocation - and `AdaptiveEvent` is crate-local
    // while serde is already a dependency, so the impl is legal. Found by CodeRabbit at
    // `59523f8`.
    let exploit = "mod wire {\n    use serde::Serialize;\n    impl serde::Serialize for super::AdaptiveEvent {}\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {}\n";
    let file = snippet(exploit);

    assert!(
        !trait_impls_for(&file, ADAPTIVE_EVENT).is_empty(),
        "a nested trait impl for AdaptiveEvent was invisible:\n{exploit}"
    );
    let imports = imports_of(&file);
    assert!(
        imports.iter().any(|i| i == "serde::Serialize"),
        "an import inside a nested module was invisible: {imports:?}\n{exploit}"
    );
    assert!(
        imports
            .iter()
            .any(|i| !EVENT_ALLOWED_IMPORTS.contains(&i.as_str())),
        "the allowlist admitted a nested import: {imports:?}"
    );

    // Deeper nesting, and an impl written without the `super::` qualifier.
    for (label, source) in [
        (
            "two modules deep",
            "mod a {\n    mod b {\n        impl Serialize for crate::producers::event::AdaptiveEvent {}\n    }\n}\n",
        ),
        (
            "nested import only",
            "mod wire {\n    use crate::wire::EventWire;\n}\n",
        ),
        (
            "nested inside an inline module with items after it",
            "mod wire {\n    impl Serialize for super::AdaptiveEvent {}\n}\nfn after() {}\n",
        ),
    ] {
        let file = snippet(source);
        let hit = !trait_impls_for(&file, ADAPTIVE_EVENT).is_empty()
            || imports_of(&file)
                .iter()
                .any(|i| !EVENT_ALLOWED_IMPORTS.contains(&i.as_str()));
        assert!(hit, "{label}: nesting hid the escape:\n{source}");
    }
}

#[test]
fn a_sibling_module_cannot_implement_a_trait_for_adaptive_event() {
    // The no-trait-impl guarantee was enforced only inside `event.rs`. Rust allows an
    // external trait to be implemented for a crate-local type from anywhere in the crate,
    // and `src/producers/aggregator.rs` already imports `AdaptiveEvent` — so a sibling can
    // make it serializable while all four `event.rs` allowlists stay clean: no new import,
    // derive, attribute, macro or impl *in that file*. Found by CodeRabbit at `59523f8`.
    for (label, source) in [
        (
            "sibling importing the type by name",
            "use super::event::AdaptiveEvent;\nimpl serde::Serialize for AdaptiveEvent {}\n",
        ),
        (
            "sibling using a qualified self type",
            "impl serde::Serialize for super::event::AdaptiveEvent {}\n",
        ),
        (
            "sibling using the full crate path",
            "impl serde::Serialize for crate::producers::event::AdaptiveEvent {}\n",
        ),
        (
            "sibling with the impl nested in a module",
            "mod helper {\n    impl serde::Serialize for crate::producers::event::AdaptiveEvent {}\n}\n",
        ),
    ] {
        assert!(
            !trait_impls_for(&snippet(source), ADAPTIVE_EVENT).is_empty(),
            "{label}: a sibling module's impl for AdaptiveEvent was invisible:\n{source}"
        );
    }

    // Positive control: an impl for a *different* type in a sibling must not register.
    for source in [
        "impl serde::Serialize for super::event::AdaptiveEventLog {}\n",
        "impl Default for AdaptiveBus {}\n",
        "impl AdaptiveEvent {}\n",
    ] {
        assert!(
            trait_impls_for(&snippet(source), ADAPTIVE_EVENT).is_empty(),
            "the scanner invented an impl in:\n{source}"
        );
    }
}

#[test]
fn a_crate_root_alias_cannot_launder_a_forbidden_module_reference() {
    // `use crate as internal;` then `internal::adaptive::X`. The alias statement has a
    // single path segment so it registered nothing, and the reference begins with a root
    // the visitor had never heard of. Found by CodeRabbit at `59523f8`.
    for (label, source) in [
        (
            "aliased crate root in expression position",
            "use crate as internal;\nfn handler() { let _ = internal::adaptive::ProducerDocument; }\n",
        ),
        (
            "aliased crate root reaching the publication layer",
            "use crate as internal;\nfn handler() { let _ = internal::producers::ProducerAggregator; }\n",
        ),
        (
            "aliased crate root in type position",
            "use crate as internal;\nfn f(x: internal::adaptive::X) {}\n",
        ),
        (
            "aliased crate root inside a macro token stream",
            "use crate as internal;\nfn f() { println!(\"{:?}\", internal::producers::P); }\n",
        ),
        (
            "aliased external crate root",
            "use unified_hifi_control as uhc;\nfn f() { let _ = uhc::adaptive::X; }\n",
        ),
        (
            "aliased root used in a nested module",
            "use crate as internal;\nmod inner {\n    fn f() { let _ = super::internal::adaptive::X; }\n}\n",
        ),
    ] {
        assert!(
            !forbidden_module_references(&snippet(source), 0).is_empty(),
            "{label}: an aliased crate root laundered the reference:\n{source}"
        );
    }
}

#[test]
fn crate_root_aliases_are_scoped_to_the_module_that_declares_them() {
    // A file-global alias set is wrong in both directions. Found by Codex reviewing the
    // alias fix at `1c48c2e`.
    //
    // Forward: an alias declared inside one module must not make an unrelated *local*
    // module of the same name look like the crate root somewhere else.
    let sibling_scope = "mod a {\n    use crate as internal;\n}\nmod b {\n    mod internal {\n        pub mod adaptive {}\n    }\n    fn f() { let _ = internal::adaptive::X; }\n}\n";
    assert!(
        forbidden_module_references(&snippet(sibling_scope), 0).is_empty(),
        "an alias declared in a sibling module leaked: {:?}\n{sibling_scope}",
        forbidden_module_references(&snippet(sibling_scope), 0)
    );

    // The same leak without any shadowing to mask it: `b` declares no `mod internal`, so
    // only restoring the outer scope on the way out of `a` keeps this clean.
    let sibling_no_shadow = "mod a {\n    use crate as internal;\n}\nmod b {\n    fn f() { let _ = internal::adaptive::X; }\n}\n";
    assert!(
        forbidden_module_references(&snippet(sibling_no_shadow), 0).is_empty(),
        "an alias leaked out of the module that declared it: {:?}\n{sibling_no_shadow}",
        forbidden_module_references(&snippet(sibling_no_shadow), 0)
    );

    // Reverse: a real crate alias shadowed by a local module of the same name is no longer
    // the crate root inside that module.
    let shadowed = "use crate as internal;\nmod b {\n    mod internal {\n        pub mod adaptive {}\n    }\n    fn f() { let _ = internal::adaptive::X; }\n}\n";
    assert!(
        forbidden_module_references(&snippet(shadowed), 0).is_empty(),
        "a shadowed alias still counted as the crate root: {:?}\n{shadowed}",
        forbidden_module_references(&snippet(shadowed), 0)
    );

    // The three genuine exploits must stay caught, so the scoping does not buy its
    // precision with a false negative.
    for (label, source) in [
        (
            "file-level alias",
            "use crate as internal;\nfn f() { let _ = internal::adaptive::X; }\n",
        ),
        (
            "alias inherited by a nested module",
            "use crate as internal;\nmod inner {\n    fn f() { let _ = internal::producers::P; }\n}\n",
        ),
        (
            "alias declared and used in the same module",
            "mod a {\n    use crate as internal;\n    fn f() { let _ = internal::adaptive::X; }\n}\n",
        ),
    ] {
        assert!(
            !forbidden_module_references(&snippet(source), 0).is_empty(),
            "{label}: scoping introduced a false negative:\n{source}"
        );
    }
}

#[test]
fn extern_crate_self_and_transitive_aliases_are_crate_roots_too() {
    // `scope_delta` recognized only a `UseTree::Rename` whose source ident is literally
    // `crate` or `unified_hifi_control`. Two further legal forms bind the crate root under
    // a new name and neither is a direct rename of those idents. Raised by Codex against
    // `24ac3de`.
    for (label, source) in [
        // `extern crate self as X;` is an ItemExternCrate, not an ItemUse at all.
        (
            "extern crate self",
            "extern crate self as internal;\nfn f() { let _ = internal::adaptive::X; }\n",
        ),
        (
            "extern crate self reaching the publication layer",
            "extern crate self as internal;\nfn f() { let _ = internal::producers::P; }\n",
        ),
        (
            "extern crate by name",
            "extern crate unified_hifi_control as uhc;\nfn f() { let _ = uhc::adaptive::X; }\n",
        ),
        // Transitive: the second rename's source is an alias, not an original root.
        (
            "transitive alias",
            "use crate as internal;\nuse self::internal as also;\nfn f() { let _ = also::producers::P; }\n",
        ),
        (
            "transitive alias two hops",
            "use crate as internal;\nuse self::internal as also;\nuse self::also as third;\nfn f() { let _ = third::adaptive::X; }\n",
        ),
        (
            "transitive from extern crate self",
            "extern crate self as internal;\nuse self::internal as also;\nfn f() { let _ = also::adaptive::X; }\n",
        ),
        (
            "transitive without the self:: prefix",
            "use crate as internal;\nuse internal as also;\nfn f() { let _ = also::adaptive::X; }\n",
        ),
        // `use crate::{self as internal};` binds the crate root through a *group*, where the
        // rename's source ident is `self` rather than a root name. Both of these compile
        // clean under `rustc --crate-type lib`. Raised by an independent audit at
        // `6980b7b`.
        (
            "grouped self",
            "use crate::{self as internal};\nfn f() { let _ = internal::adaptive::X; }\n",
        ),
        (
            "grouped self reaching the publication layer",
            "use crate::{self as internal};\nfn f() { let _ = internal::producers::P; }\n",
        ),
        (
            "transitive grouped self",
            "use crate::{self as internal};\nuse internal::{self as also};\nfn f() { let _ = also::producers::P; }\n",
        ),
        (
            "grouped self alongside a sibling leaf",
            "use crate::{self as internal, bus};\nfn f() { let _ = internal::adaptive::X; }\n",
        ),
        (
            "grouped self from the external crate name",
            "use unified_hifi_control::{self as uhc};\nfn f() { let _ = uhc::adaptive::X; }\n",
        ),
    ] {
        assert!(
            !forbidden_module_references(&snippet(source), 0).is_empty(),
            "{label}: an alias form bypassed the boundary:\n{source}"
        );
    }
}

#[test]
fn extended_alias_forms_remain_scope_aware_and_shadowable() {
    // The extra forms must not loosen the boundary into a file-global set again.
    for (label, source) in [
        (
            "extern crate self scoped to a sibling module",
            "mod a {\n    extern crate self as internal;\n}\nmod b {\n    mod internal { pub mod adaptive {} }\n    fn f() { let _ = internal::adaptive::X; }\n}\n",
        ),
        (
            "extern crate self leaking to a sibling with no shadow",
            "mod a {\n    extern crate self as internal;\n}\nmod b {\n    fn f() { let _ = internal::adaptive::X; }\n}\n",
        ),
        (
            "transitive alias shadowed by a local module",
            "use crate as internal;\nuse self::internal as also;\nmod b {\n    mod also { pub mod adaptive {} }\n    fn f() { let _ = also::adaptive::X; }\n}\n",
        ),
        (
            "an extern crate that is not this crate",
            "extern crate serde as s;\nfn f() { let _ = s::adaptive::X; }\n",
        ),
        (
            "an alias of an alias of something unrelated",
            "use std as s;\nuse self::s as t;\nfn f() { let _ = t::fmt::Debug; }\n",
        ),
        // The prefix guard: `foo::internal` is a real module that merely shares a name with
        // the alias, so renaming *it* binds nothing. Only `self::` and a known root are
        // transparent prefixes; treating every prefix as transparent would bind `also` here
        // and flag clean code.
        (
            "a rename under an unrelated path prefix",
            "use crate as internal;\nmod foo {\n    pub mod internal { pub mod adaptive {} }\n}\nuse foo::internal as also;\nfn f() { let _ = also::adaptive::X; }\n",
        ),
        // The same guard for the grouped form: `foo::{self as also}` renames `foo`, not the
        // crate root, so recognizing grouped `self` must not make every group transparent.
        (
            "grouped self under an unrelated prefix",
            "mod foo {\n    pub mod adaptive {}\n}\nuse foo::{self as also};\nfn f() { let _ = also::adaptive::X; }\n",
        ),
        (
            "grouped self under an unrelated alias chain",
            "use std::{self as s};\nuse s::{self as t};\nfn f() { let _ = t::fmt::Debug; }\n",
        ),
        (
            "grouped self scoped to a sibling module",
            "mod a {\n    use crate::{self as internal};\n}\nmod b {\n    fn f() { let _ = internal::adaptive::X; }\n}\n",
        ),
    ] {
        assert!(
            forbidden_module_references(&snippet(source), 0).is_empty(),
            "{label}: the extended forms invented a reference: {:?}\n{source}",
            forbidden_module_references(&snippet(source), 0)
        );
    }
}

#[test]
fn super_resolves_to_the_crate_root_at_the_right_depth() {
    // `super::adaptive::X` written in `src/api/mod.rs` *is* `crate::adaptive::X`, because
    // that file is one module deep. The visitor only knew `crate` and `unified_hifi_control`
    // as roots, so every `super::` route was invisible. Found by CodeRabbit at `9c53079`.
    for (label, depth, source) in [
        (
            "one super at depth 1",
            1,
            "fn f() { let _ = super::adaptive::X; }",
        ),
        (
            "one super at depth 1, publication layer",
            1,
            "fn f() { let _ = super::producers::P; }",
        ),
        (
            "two supers at depth 2",
            2,
            "fn f() { let _ = super::super::adaptive::X; }",
        ),
        (
            "three supers at depth 3",
            3,
            "fn f() { let _ = super::super::super::producers::P; }",
        ),
        (
            "super in a use tree at depth 1",
            1,
            "use super::adaptive::X;",
        ),
        (
            "super in type position at depth 1",
            1,
            "fn f(x: super::adaptive::X) {}",
        ),
    ] {
        assert!(
            !forbidden_module_references(&snippet(source), depth).is_empty(),
            "{label}: a super:: route to the crate root was missed:\n{source}"
        );
    }
}

#[test]
fn super_does_not_reach_the_crate_root_from_the_wrong_depth() {
    // Near misses. Too few supers lands in an intermediate module, too many is not
    // expressible, and a file at the crate root has no `super` at all.
    for (label, depth, source) in [
        (
            "one super at depth 2 lands one module short",
            2,
            "fn f() { let _ = super::adaptive::X; }",
        ),
        (
            "two supers at depth 3 lands one module short",
            3,
            "fn f() { let _ = super::super::adaptive::X; }",
        ),
        (
            "two supers at depth 1 over-shoots",
            1,
            "fn f() { let _ = super::super::adaptive::X; }",
        ),
        (
            "no super at depth 1 is a sibling module",
            1,
            "fn f() { let _ = adaptive::X; }",
        ),
        (
            "super at depth 0 has no parent",
            0,
            "fn f() { let _ = super::adaptive::X; }",
        ),
        (
            "a prefix-sharing module through super",
            1,
            "fn f() { let _ = super::adaptive_extras::X; }",
        ),
    ] {
        assert!(
            forbidden_module_references(&snippet(source), depth).is_empty(),
            "{label}: super:: resolution over-reached: {:?}\n{source}",
            forbidden_module_references(&snippet(source), depth)
        );
    }
}

#[test]
fn inline_modules_shift_the_effective_super_depth() {
    // `depth` is the file's depth, but an inline `mod nested { … }` puts its contents one
    // module further down. At file depth 1, inside one inline module the effective depth is
    // 2: `super::super::adaptive` reaches the crate root and `super::adaptive` only reaches
    // the file's own module. A fixed per-file depth gets both backwards. Raised by Codex
    // against the first draft of the `super::` fix.
    for (label, depth, source) in [
        (
            "two supers inside one inline module at file depth 1",
            1,
            "mod nested {\n    fn f() { let _ = super::super::adaptive::X; }\n}\n",
        ),
        (
            "three supers inside two inline modules at file depth 1",
            1,
            "mod a {\n    mod b {\n        fn f() { let _ = super::super::super::producers::P; }\n    }\n}\n",
        ),
        (
            "one super inside an inline module at file depth 0",
            0,
            "mod nested {\n    fn f() { let _ = super::adaptive::X; }\n}\n",
        ),
    ] {
        assert!(
            !forbidden_module_references(&snippet(source), depth).is_empty(),
            "{label}: an inline module's effective depth was not applied:\n{source}"
        );
    }

    for (label, depth, source) in [
        (
            "one super inside an inline module at file depth 1 stops at the file's module",
            1,
            "mod nested {\n    fn f() { let _ = super::adaptive::X; }\n}\n",
        ),
        (
            "two supers inside two inline modules at file depth 1 falls short",
            1,
            "mod a {\n    mod b {\n        fn f() { let _ = super::super::adaptive::X; }\n    }\n}\n",
        ),
        (
            "depth must be restored after leaving an inline module",
            1,
            "mod nested {\n    fn inner() {}\n}\nfn after() { let _ = super::super::adaptive::X; }\n",
        ),
        (
            "an external module declaration does not add depth",
            1,
            "mod external;\nfn f() { let _ = super::super::adaptive::X; }\n",
        ),
    ] {
        assert!(
            forbidden_module_references(&snippet(source), depth).is_empty(),
            "{label}: super:: over-reached: {:?}\n{source}",
            forbidden_module_references(&snippet(source), depth)
        );
    }

    // And the file-level case still holds inside a file that also has inline modules.
    let mixed = "fn top() { let _ = super::adaptive::X; }\nmod nested { fn f() {} }\n";
    assert!(
        !forbidden_module_references(&snippet(mixed), 1).is_empty(),
        "a file-level super:: stopped being caught once the file gained an inline module"
    );
}

#[test]
fn macro_token_paths_resolve_super_against_the_same_depth_as_ordinary_paths() {
    // A path written inside a macro invocation is compiled exactly like one written outside
    // it, so the sweep must resolve it identically. The token scanner checked only adjacent
    // `Ident :: Ident` windows, which cannot see a leading `super` run at all - so
    // `super::adaptive::X` was caught as a plain path and missed as a macro argument. That
    // asymmetry is the whole bypass. Raised before commit against the `super::` fix.
    for (label, depth, source) in [
        (
            "one super at file depth 1",
            1,
            "fn f() { do_thing!(super::adaptive::X); }\n",
        ),
        (
            "two supers at file depth 2",
            2,
            "fn f() { do_thing!(super::super::adaptive::X); }\n",
        ),
        (
            "two supers inside one inline module at file depth 1",
            1,
            "mod nested {\n    fn f() { do_thing!(super::super::adaptive::X); }\n}\n",
        ),
        (
            "three supers inside two inline modules at file depth 1",
            1,
            "mod a {\n    mod b {\n        fn f() { do_thing!(super::super::super::producers::P); }\n    }\n}\n",
        ),
        (
            "nested inside a delimiter group",
            1,
            "fn f() { do_thing!(vec![(super::adaptive::X)]); }\n",
        ),
        (
            "an aliased root reached through super still resolves",
            1,
            "use crate as internal;\nfn f() { do_thing!(super::internal::adaptive::X); }\n",
        ),
    ] {
        assert!(
            !forbidden_module_references(&snippet(source), depth).is_empty(),
            "{label}: a forbidden path inside a macro escaped depth resolution:\n{source}"
        );
    }

    for (label, depth, source) in [
        (
            "one super at file depth 2 falls short",
            2,
            "fn f() { do_thing!(super::adaptive::X); }\n",
        ),
        (
            "two supers at file depth 1 over-reach",
            1,
            "fn f() { do_thing!(super::super::adaptive::X); }\n",
        ),
        (
            "one super inside an inline module at file depth 1 stops at the file's module",
            1,
            "mod nested {\n    fn f() { do_thing!(super::adaptive::X); }\n}\n",
        ),
    ] {
        assert!(
            forbidden_module_references(&snippet(source), depth).is_empty(),
            "{label}: super:: over-reached inside a macro: {:?}\n{source}",
            forbidden_module_references(&snippet(source), depth)
        );
    }

    // The literal controls must survive the change: a string is a different TokenTree
    // variant from an Ident, so its contents can never become path segments. This is the
    // regression Codex found at `4129b87` and it stays covered.
    for (label, source) in [
        (
            "a plain string literal",
            r#"fn f() { println!("crate::adaptive"); }"#,
        ),
        (
            "a raw string literal",
            "fn f() { println!(r#\"super::super::adaptive::X\"#); }",
        ),
        (
            "a literal inside a nested group",
            r#"fn f() { do_thing!(vec![("super::adaptive::X")]); }"#,
        ),
    ] {
        assert!(
            forbidden_module_references(&snippet(source), 1).is_empty(),
            "{label}: a string literal fabricated a module reference:\n{source}"
        );
    }
}

#[test]
fn module_depth_is_derived_from_the_file_path() {
    for (path, expected) in [
        ("src/lib.rs", 0),
        ("src/main.rs", 0),
        ("src/aggregator.rs", 1),
        ("src/api/mod.rs", 1),
        ("src/adapters/hqplayer.rs", 2),
        ("src/producers/event.rs", 2),
        ("src/app/pages/zones.rs", 3),
        ("src/server/routes/mod.rs", 2),
        // A path with no `.rs` suffix must still lose its `src/` prefix. Chaining the two
        // `unwrap_or`s made the second fall back to the *original* path, silently undoing
        // the first trim and counting `src` as a module.
        ("src/api/mod", 1),
        ("src/adapters/hqplayer", 2),
        ("src/lib", 0),
    ] {
        assert_eq!(
            module_depth(path),
            expected,
            "wrong module depth for {path}"
        );
    }
}

#[test]
fn sweep_exemptions_do_not_match_sibling_paths() {
    // The exemptions were raw prefixes, so `src/adaptive` would also exempt a future
    // `src/adaptive_extras/`, and a file exemption would match anything beginning with it.
    // A too-broad exemption is silent: the sweep simply stops covering a directory.
    // Found by CodeRabbit at `9c53079`.
    for exempt in [
        "src/adaptive/mod.rs",
        "src/adaptive/value.rs",
        "src/producers/event.rs",
        "src/lib.rs",
        "src/main.rs",
    ] {
        assert!(is_sweep_exempt(exempt), "{exempt} should be exempt");
    }
    for covered in [
        "src/adaptive_extras/mod.rs",
        "src/producers_extra/thing.rs",
        "src/lib.rs.bak.rs",
        "src/main.rs.orig.rs",
        "src/lib/helper.rs",
        "src/api/mod.rs",
        "src/mqtt/mod.rs",
    ] {
        assert!(
            !is_sweep_exempt(covered),
            "{covered} must stay inside the sweep"
        );
    }
}

#[test]
fn unrelated_crate_aliases_do_not_false_positive() {
    // Positive controls for the alias tracking. Aliasing something else, or aliasing the
    // crate and then not reaching a forbidden module, must stay clean.
    for (label, source) in [
        (
            "alias of another crate",
            "use std as s;\nfn f() { let _ = s::fmt::Debug; }\n",
        ),
        (
            "crate alias used for something permitted",
            "use crate as internal;\nfn f() { let _ = internal::bus::SharedBus; }\n",
        ),
        ("the alias statement alone", "use crate as internal;\n"),
        (
            "a module whose name merely shares a prefix, through an alias",
            "use crate as internal;\nfn f() { let _ = internal::adaptive_extras::X; }\n",
        ),
    ] {
        assert!(
            forbidden_module_references(&snippet(source), 0).is_empty(),
            "{label}: alias tracking invented a reference: {:?}\n{source}",
            forbidden_module_references(&snippet(source), 0)
        );
    }
}

#[test]
fn every_known_impl_escape_is_rejected() {
    for (label, source) in [
        ("canonical", "impl Serialize for AdaptiveEvent {}"),
        (
            "with a lifetime",
            "impl<'de> Deserialize<'de> for AdaptiveEvent {}",
        ),
        (
            "aliased",
            "use serde::Serialize as EventWire;\nimpl EventWire for AdaptiveEvent {}",
        ),
        (
            "fully-qualified re-export",
            "impl crate::wire::EventWire for AdaptiveEvent {}",
        ),
        (
            "escaped-quote char literal nearby",
            "const SEP: char = '\\'';\nimpl Serialize for AdaptiveEvent {}",
        ),
        (
            "raw string nearby",
            "const D: &str = r##\"] \" ]\"##;\nimpl Serialize for AdaptiveEvent {}",
        ),
    ] {
        let file = snippet(source);
        assert!(
            !trait_impls_for(&file, ADAPTIVE_EVENT).is_empty(),
            "{label}: a hand-written impl was admitted:\n{source}"
        );
    }
}

// =============================================================================
// Positive controls
// =============================================================================

#[test]
fn the_allowlists_accept_the_module_as_it_actually_is() {
    // A list that only ever fails is as useless as one that only ever passes.
    let file = parse_file_at(EVENT_MODULE);
    for import in imports_of(&file) {
        assert!(
            EVENT_ALLOWED_IMPORTS.contains(&import.as_str()),
            "the allowlist rejects an import the module legitimately has: {import:?}"
        );
    }
    for derive in derives_in(
        &adaptive_event_enum(&file)
            .expect("AdaptiveEvent declared")
            .attrs,
    ) {
        assert!(
            ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&derive.as_str()),
            "the allowlist rejects a derive the type legitimately has: {derive:?}"
        );
    }
    assert!(trait_impls_for(&file, ADAPTIVE_EVENT).is_empty());
}

#[test]
fn the_inspectors_do_not_invent_findings() {
    // Prose, identifiers and string content must not register as imports; an inherent impl
    // and another type's impl must not register as trait impls; an innocent derive must not
    // register as forbidden. Under a parser these are not near-misses to be excluded - they
    // are different syntax - but they are pinned so a regression to scanning is visible.
    let benign = "//! doc: callers use crate::adaptive::X for this\n\
                  use std::sync::Arc;\n\
                  const S: &str = \"please use crate::adaptive::Z\";\n\
                  fn f(thing: T) { let misuse = 1; let reuse_count = 2; thing.use_default(); \
                  let _ = (misuse, reuse_count); }\n\
                  impl Default for AdaptiveBus {}\n\
                  impl AdaptiveEvent {}\n\
                  #[derive(Debug, Clone)]\n\
                  pub enum AdaptiveEvent {}\n";
    let file = snippet(benign);

    assert_eq!(
        imports_of(&file),
        vec!["std::sync::Arc".to_string()],
        "prose or a string literal registered as an import"
    );
    assert!(
        trait_impls_for(&file, ADAPTIVE_EVENT).is_empty(),
        "an inherent impl or another type's impl was counted: {:?}",
        trait_impls_for(&file, ADAPTIVE_EVENT)
    );
    let derived = derives_in(&adaptive_event_enum(&file).expect("declared").attrs);
    assert_eq!(derived, vec!["Debug".to_string(), "Clone".to_string()]);

    // A doc comment and a string mentioning a forbidden module are not references.
    let prose_only = "//! crate::adaptive is discussed here\n\
                      const S: &str = \"crate::producers\";\n\
                      fn f() {}\n";
    assert!(
        forbidden_module_references(&snippet(prose_only), 0).is_empty(),
        "prose or a string literal registered as a module reference"
    );

    // A string literal *inside a macro* is the same thing, and the previous
    // implementation got it wrong: it stringified the whole token stream and
    // substring-matched, so `println!("crate::adaptive")` registered as a reference while
    // the doc comment above the function claimed literals could not fabricate a match.
    // Found by Codex at `4129b87`.
    for (label, source) in [
        (
            "string literal in a macro",
            "fn f() { println!(\"crate::adaptive\"); }",
        ),
        (
            "raw string literal in a macro",
            "fn f() { println!(r#\"crate::producers\"#); }",
        ),
        (
            "literal nested in a macro group",
            "fn f() { assert_eq!(x, vec![\"crate::adaptive\"]); }",
        ),
    ] {
        assert!(
            forbidden_module_references(&snippet(source), 0).is_empty(),
            "{label}: a literal inside a macro fabricated a module reference: {:?}\n{source}",
            forbidden_module_references(&snippet(source), 0)
        );
    }

    // A module whose name merely shares a prefix is not forbidden.
    let near_miss = "use crate::adaptive_extras::X;\nuse crate::production::Y;\n";
    assert!(
        forbidden_module_references(&snippet(near_miss), 0).is_empty(),
        "a prefix-sharing module name registered: {:?}",
        forbidden_module_references(&snippet(near_miss), 0)
    );
}

#[test]
fn module_references_are_found_in_every_position() {
    for (label, source) in [
        ("use", "use crate::adaptive::X;"),
        ("grouped use", "use crate::{adaptive, bus};"),
        ("renamed use", "use crate::adaptive as contract;"),
        (
            "external crate root",
            "use unified_hifi_control::producers::P;",
        ),
        ("type position", "fn f(x: crate::adaptive::X) {}"),
        (
            "expression position",
            "fn f() { let _ = crate::producers::g(); }",
        ),
        (
            "inside a macro",
            "fn f() { println!(\"{:?}\", crate::adaptive::X); }",
        ),
        // `syn` strips a macro's outer delimiter, so the case above puts the path at the
        // top level of the token stream and never exercises group recursion. A mutation
        // that deleted the recursion still passed until this case existed.
        (
            "nested in a group inside a macro",
            "fn f() { assert_eq!(a, wrap(crate::adaptive::X)); }",
        ),
        (
            "nested two groups deep",
            "fn f() { assert_eq!(a, wrap(vec![crate::producers::P])); }",
        ),
        (
            "attribute-decorated use",
            "#[allow(unused_imports)]\nuse crate::producers::P;",
        ),
    ] {
        assert!(
            !forbidden_module_references(&snippet(source), 0).is_empty(),
            "{label}: a module reference was missed:\n{source}"
        );
    }
}

#[test]
fn server_gate_detection_is_structural() {
    // Accepted: the plain gate, and a conjunction that only narrows it.
    for source in [
        "#[cfg(feature = \"server\")]\npub mod producers;",
        "#[cfg(feature=\"server\")]\npub mod producers;",
        "#[cfg(feature = \"server\")]\n#[allow(dead_code)]\npub mod producers;",
        "#[allow(dead_code)]\n#[cfg(feature = \"server\")]\npub mod producers;",
        "#[cfg(all(feature = \"server\", unix))]\npub mod producers;",
        "#[cfg(\n    feature = \"server\"\n)]\npub mod producers;",
        "#[cfg(feature = \"server\")]\nmod inner {\n    pub mod producers;\n}",
    ] {
        let gates = module_cfg_gates(&snippet(source), PRODUCERS_MODULE)
            .unwrap_or_else(|| panic!("module not located:\n{source}"));
        assert!(
            gates.iter().any(gate_is_server_only),
            "a server gate was missed:\n{source}"
        );
    }

    // Rejected: absent, a different feature, a disjunction that widens it, a negation, a
    // gate belonging to a neighbouring item, and `cfg_attr`, which cannot exclude a module.
    for source in [
        "pub mod producers;",
        "#[cfg(feature = \"web\")]\npub mod producers;",
        "#[cfg(any(feature = \"server\", feature = \"web\"))]\npub mod producers;",
        "#[cfg(any(feature=\"server\",feature=\"web\"))]\npub mod producers;",
        "#[cfg(not(feature = \"server\"))]\npub mod producers;",
        "#[cfg(feature = \"server\")]\npub mod bus;\npub mod producers;",
        "#[cfg_attr(feature = \"server\", allow(dead_code))]\npub mod producers;",
    ] {
        let gates = module_cfg_gates(&snippet(source), PRODUCERS_MODULE)
            .unwrap_or_else(|| panic!("module not located:\n{source}"));
        assert!(
            !gates.iter().any(gate_is_server_only),
            "a server gate was invented:\n{source}"
        );
    }

    assert!(
        module_cfg_gates(&snippet("pub mod adaptive;"), PRODUCERS_MODULE).is_none(),
        "an absent module was reported as present"
    );
}

#[test]
fn an_unparsable_file_fails_loudly_rather_than_vacuously() {
    // A lint that treats an unparsable file as empty passes for the wrong reason. That is
    // the failure mode this file exists to stop reproducing, so it is pinned.
    let broken = std::panic::catch_unwind(|| parse_source("<probe>", "fn broken( {"));
    assert!(
        broken.is_err(),
        "an unparsable file was accepted, which would make every lint over it vacuous"
    );
}
