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
use syn::{Attribute, Expr, ExprLit, File, Item, ItemEnum, Lit, Meta, Token, UseTree};
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
    "crate::adaptive::ProducerDocument",
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
    /// Names that address this crate's root, including aliases bound in this file.
    roots: BTreeSet<String>,
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
        for window in segments.windows(2) {
            self.note(&window[0], &window[1]);
        }
    }

    /// Look for an `Ident :: Ident` token sequence, recursing into groups.
    ///
    /// The previous implementation stringified the whole token stream and substring-matched
    /// it, so `println!("crate::adaptive")` registered as a reference — while the comment
    /// above it claimed a literal could not fabricate a match. It could, and did. Found by
    /// Codex at `4129b87`.
    ///
    /// Walking `TokenTree` fixes it by construction rather than by exclusion: a `Literal`
    /// is a different variant from an `Ident`, so its contents are never sequence material.
    /// A real path inside a macro is still four tokens and is still found.
    fn scan_tokens(&mut self, stream: &proc_macro2::TokenStream) {
        let trees: Vec<TokenTree> = stream.clone().into_iter().collect();
        for window in trees.windows(4) {
            let (
                TokenTree::Ident(root),
                TokenTree::Punct(first),
                TokenTree::Punct(second),
                TokenTree::Ident(module),
            ) = (&window[0], &window[1], &window[2], &window[3])
            else {
                continue;
            };
            if first.as_char() == ':' && second.as_char() == ':' {
                self.note(&root.to_string(), &module.to_string());
            }
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
}

/// Names bound to this crate's root in `file`, including `use crate as internal;`.
///
/// `use crate as internal;` then `internal::adaptive::X` reached the forbidden module
/// through a root the visitor had never heard of: the alias statement has a single path
/// segment so it registered nothing, and the reference began with `internal`. Found by
/// CodeRabbit at `59523f8`.
fn crate_root_names(file: &File) -> BTreeSet<String> {
    #[derive(Default)]
    struct AliasVisitor {
        aliases: BTreeSet<String>,
    }
    impl<'ast> Visit<'ast> for AliasVisitor {
        fn visit_use_tree(&mut self, tree: &'ast UseTree) {
            if let UseTree::Rename(rename) = tree {
                if CRATE_ROOTS.contains(&rename.ident.to_string().as_str()) {
                    self.aliases.insert(rename.rename.to_string());
                }
            }
            visit::visit_use_tree(self, tree);
        }
    }
    let mut visitor = AliasVisitor::default();
    visitor.visit_file(file);
    let mut roots: BTreeSet<String> = CRATE_ROOTS.iter().map(|r| (*r).to_string()).collect();
    roots.extend(visitor.aliases);
    roots
}

/// Forbidden module references in `file`, resolving crate-root aliases first.
fn forbidden_module_references(file: &File) -> Vec<String> {
    let mut visitor = ModuleReferenceVisitor {
        found: BTreeSet::new(),
        roots: crate_root_names(file),
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
    let mut violations = Vec::new();
    for (path, text) in rust_sources_under("src/bus") {
        let file = parse_source(&path, &text);
        for reference in forbidden_module_references(&file) {
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
    let references = forbidden_module_references(&file);
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
    const EXEMPT: &[&str] = &["src/adaptive", "src/producers", "src/lib.rs", "src/main.rs"];
    let mut violations = Vec::new();
    let mut swept = 0usize;
    for (path, text) in rust_sources_under("src") {
        if EXEMPT.iter().any(|exempt| path.starts_with(exempt)) {
            continue;
        }
        swept += 1;
        let file = parse_source(&path, &text);
        for reference in forbidden_module_references(&file) {
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
        if path.starts_with("src/producers") || path == "src/lib.rs" || path == "src/main.rs" {
            continue;
        }
        let file = parse_source(&path, &text);
        if forbidden_module_references(&file)
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
            !forbidden_module_references(&snippet(source)).is_empty(),
            "{label}: an aliased crate root laundered the reference:\n{source}"
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
            forbidden_module_references(&snippet(source)).is_empty(),
            "{label}: alias tracking invented a reference: {:?}\n{source}",
            forbidden_module_references(&snippet(source))
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
        forbidden_module_references(&snippet(prose_only)).is_empty(),
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
            forbidden_module_references(&snippet(source)).is_empty(),
            "{label}: a literal inside a macro fabricated a module reference: {:?}\n{source}",
            forbidden_module_references(&snippet(source))
        );
    }

    // A module whose name merely shares a prefix is not forbidden.
    let near_miss = "use crate::adaptive_extras::X;\nuse crate::production::Y;\n";
    assert!(
        forbidden_module_references(&snippet(near_miss)).is_empty(),
        "a prefix-sharing module name registered: {:?}",
        forbidden_module_references(&snippet(near_miss))
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
            !forbidden_module_references(&snippet(source)).is_empty(),
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
