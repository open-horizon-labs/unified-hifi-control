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
//! Residual, recorded rather than papered over: items produced by macro *expansion* are
//! invisible to a parse of pre-expansion source, as they were to the text scanner. Macro
//! *invocations* are visited and their token streams checked, which is exact because tokens
//! are post-lexing — but a macro assembling an `impl` from fragments escapes both
//! architectures.

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
    let mut found = Vec::new();
    for item in &file.items {
        if let Item::Use(item_use) = item {
            found.extend(flatten_use_tree(&item_use.tree));
        }
    }
    found
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

/// Every trait implemented for `type_name` in `file`.
///
/// Inherent impls (`impl AdaptiveEvent { … }`) are not trait impls and do not appear.
fn trait_impls_for(file: &File, type_name: &str) -> Vec<String> {
    file.items
        .iter()
        .filter_map(|item| match item {
            Item::Impl(item_impl) => {
                let (_, path, _) = item_impl.trait_.as_ref()?;
                let target = match item_impl.self_ty.as_ref() {
                    syn::Type::Path(type_path) => type_path.path.segments.last()?.ident.to_string(),
                    _ => return None,
                };
                if target != type_name {
                    return None;
                }
                Some(path.segments.last()?.ident.to_string())
            }
            _ => None,
        })
        .collect()
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
}

impl ModuleReferenceVisitor {
    fn note(&mut self, root: &str, module: &str) {
        if CRATE_ROOTS.contains(&root) && FORBIDDEN_MODULES.contains(&module) {
            self.found.insert(format!("{root}::{module}"));
        }
    }
}

impl<'ast> Visit<'ast> for ModuleReferenceVisitor {
    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        for leaf in flatten_use_tree(&item.tree) {
            let mut segments = leaf.split("::");
            if let (Some(root), Some(module)) = (segments.next(), segments.next()) {
                // `use crate::adaptive as contract;` renders its leaf as `adaptive as
                // contract`, so the alias is trimmed before comparison.
                let module = module.split(" as ").next().unwrap_or(module);
                self.note(root, module);
            }
        }
        visit::visit_item_use(self, item);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<String> = path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .collect();
        if segments.len() >= 2 {
            self.note(&segments[0], &segments[1]);
        }
        visit::visit_path(self, path);
    }

    fn visit_macro(&mut self, mac: &'ast syn::Macro) {
        let rendered = mac.tokens.to_string().replace(' ', "");
        for root in CRATE_ROOTS {
            for module in FORBIDDEN_MODULES {
                if rendered.contains(&format!("{root}::{module}")) {
                    self.found.insert(format!("{root}::{module}"));
                }
            }
        }
        visit::visit_macro(self, mac);
    }
}

fn forbidden_module_references(file: &File) -> Vec<String> {
    let mut visitor = ModuleReferenceVisitor::default();
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

    assert_eq!(
        module_cfg_gates(&snippet("pub mod adaptive;"), PRODUCERS_MODULE).is_none(),
        true,
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
