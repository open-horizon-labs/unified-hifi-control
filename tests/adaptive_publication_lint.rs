//! Boundary lints for adaptive producer publication (issue #324).
//!
//! #324 publishes producer documents to **in-repository consumers only**. Two boundaries
//! keep that true, and neither is visible from a passing build:
//!
//! 1. **The public bus cannot carry adaptive data.** `src/api/mod.rs` serializes every
//!    [`BusEvent`] verbatim into `GET /events`, which `docs/ARCHITECTURE.md` documents as
//!    consumable by any HTTP client including ESP32 firmware. A variant carrying a producer
//!    document would be a response-schema change to a public endpoint *and* publication of
//!    the v1 contract outside this repository.
//! 2. **No surface reaches the contract or the publication layer.** The one deliberate code
//!    path that would expose it is a handler serializing a snapshot, and
//!    `ProducerDocument` derives `Serialize`, so that is a one-line accident away.
//!
//! ## Why this file carries probes
//!
//! `tests/adaptive_dependency_lint.rs` has been remediated twice for blind spots that made
//! it *report success on a violation* — `1577948` (three) and `d6da952` (a fourth, where a
//! stacked `#[allow]` between the gate and the declaration discarded the gate). Every one
//! failed in the direction that passes. A lint whose non-vacuity was never demonstrated
//! should be treated as absent, so each scanner here is probed in **both** directions.
//!
//! The polarity matters and is the opposite of `pub mod adaptive;`. That declaration must be
//! **ungated**, so a scanner that misses a gate fails safe. `pub mod producers;` must be
//! **gated**, so a scanner that *hallucinates* a gate passes wrongly — which is why the
//! probes below include "delete the gate, the lint must fail".

use std::fs;
use std::path::Path;
use walkdir::WalkDir;

const PRODUCERS_DECLARATION: &str = "pub mod producers;";
const ADAPTIVE_EVENT_DECLARATION: &str = "pub enum AdaptiveEvent {";

/// Exactly what `src/producers/event.rs` may import, whitespace-normalized.
///
/// An **allowlist**, not a denylist, and that inversion is the point. Name-based detection
/// has now been bypassed four times, most recently by a crate-local re-export:
///
/// ```ignore
/// // src/wire.rs
/// pub use serde::Serialize as EventWire;
/// // src/producers/event.rs
/// use crate::wire::EventWire;
/// #[derive(EventWire)]
/// pub enum AdaptiveEvent { … }
/// ```
///
/// That file contains no `serde`, imports nothing from `serde`, and no resolver short of a
/// real name resolver learns that `EventWire` serializes. Verified by compiling it: the
/// build finished clean and all 24 lints passed. Found by CodeRabbit at `993d35e`.
///
/// Chasing re-export chains would be a partial resolver wearing a guarantee's clothes —
/// it would still miss `pub use` through several modules, glob re-exports, and macros. An
/// allowlist does not care what a name means: **any** import not on this list fails, so
/// every re-export form is closed at once. The cost is that a legitimate new import has to
/// be added here, deliberately, which for this module is the point rather than the tax.
const EVENT_ALLOWED_IMPORTS: &[&str] = &[
    "use std::sync::Arc;",
    "use tokio::sync::broadcast;",
    "use super::admission::{AdmissionRefusal,ProducerKey};",
    "use crate::adaptive::{DocumentRevisions,ProducerDocument};",
];

/// Exactly what `AdaptiveEvent` may derive.
///
/// The companion to [`EVENT_ALLOWED_IMPORTS`], and necessary because a derive needs no
/// import at all: `#[derive(crate::wire::EventWire)]` names the macro by path. Matching on
/// permitted names rather than forbidden ones makes the spelling irrelevant.
const ADAPTIVE_EVENT_ALLOWED_DERIVES: &[&str] = &["Debug", "Clone"];
/// The readable spelling, used in failure messages.
const SERVER_GATE: &str = "#[cfg(feature = \"server\")]";

/// Whether an extracted attribute gates its item on the `server` feature and nothing looser.
///
/// Two ways to get this wrong, both of which accept a gate that is not server-only:
///
/// 1. **Whitespace.** `#[cfg(feature="server")]` is the same gate as
///    `#[cfg(feature = "server")]`; matching one spelling silently accepts the other. That
///    is the blind spot `1577948` fixed in `tests/adaptive_dependency_lint.rs`, and
///    `gate_scanner_detects_a_gate_it_must_not_miss` caught it here before it shipped.
/// 2. **Disjunction.** `#[cfg(any(feature = "server", feature = "web"))]` *contains*
///    `feature="server"` while compiling the module for `web` **without** `server` — which
///    is exactly the WASM breakage this lint exists to prevent, admitted by the lint that
///    was supposed to prevent it. Found by CodeRabbit at `86e5bd2`.
///
/// So: the normalized attribute must mention the server feature and must contain neither
/// `any(` nor `not(`. A stricter conjunctive gate (`all(feature = "server", unix)`) is still
/// server-only and is accepted; loosening it later has to be deliberate, because the lint
/// fails until someone edits this function.
fn is_server_gate(attribute: &str) -> bool {
    let squeezed: String = attribute.chars().filter(|c| !c.is_whitespace()).collect();
    squeezed.contains("feature=\"server\"")
        && !squeezed.contains("any(")
        && !squeezed.contains("not(")
}

/// The module paths a source file reaches, including through grouped `use` trees.
///
/// A needle scan for `crate::adaptive` misses `use crate::{adaptive, producers};`, which
/// names neither literal. That is not hypothetical: `src/main.rs` imports this repository's
/// modules in exactly that style, so a surface adopting the house style would have slipped
/// straight past the boundary check. Found by CodeRabbit at `86e5bd2`.
fn forbidden_module_references(code: &str) -> Vec<String> {
    const ROOTS: &[&str] = &["crate", "unified_hifi_control"];
    const FORBIDDEN: &[&str] = &["adaptive", "producers"];
    let mut found = Vec::new();

    // Fully-spelled paths, anywhere - `use`, a turbofish, a type position.
    for root in ROOTS {
        for module in FORBIDDEN {
            let needle = format!("{root}::{module}");
            if code.contains(&needle) {
                found.push(needle);
            }
        }
    }

    // Grouped use trees: `use <root>::{ a, b::c, d as e };`
    //
    // Whitespace is collapsed rather than removed. Removing it entirely turns
    // `adaptive as contract` into `adaptiveascontract`, so the alias can no longer be split
    // off and the item stops matching - a probe caught exactly that here.
    let normalized = collapse_whitespace(code);
    for root in ROOTS {
        let opener = format!("use {root}::{{");
        let mut rest = normalized.as_str();
        while let Some(at) = rest.find(&opener) {
            let after = &rest[at + opener.len()..];
            let mut depth = 1usize;
            let mut end = after.len();
            for (index, c) in after.char_indices() {
                match c {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = index;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            for item in split_top_level(&after[..end]) {
                let head = item
                    .split("::")
                    .next()
                    .unwrap_or_default()
                    .split(" as ")
                    .next()
                    .unwrap_or_default();
                if FORBIDDEN.contains(&head) {
                    found.push(format!("{root}::{{… {head} …}}"));
                }
            }
            rest = &after[end.min(after.len())..];
        }
    }

    found.sort();
    found.dedup();
    found
}

/// If a string literal begins at `chars[index]`, the index just past its end.
///
/// Recognizes ordinary `"…"`, byte `b"…"`, and **raw** `r"…"` / `r#"…"#` / `r##"…"##` /
/// `br#"…"#`. Raw strings were the gap: a scanner that leaves string mode at the first `"`
/// reads the rest of `r##"quote: " then ] remains data"##` as code, takes the `]` for an
/// attribute close, and silently stops joining whatever follows. Found by CodeRabbit at
/// `993d35e`, with the exploit supplied.
///
/// The terminator of a raw string is `"` followed by **the same number of hashes** it
/// opened with, so an inner `"#` inside a `r##"…"##` literal is data.
///
/// `None` means no literal starts here. Callers must also check that the preceding
/// character is not part of an identifier, or the `r` in `counter` looks like a prefix.
fn string_literal_end(chars: &[char], index: usize) -> Option<usize> {
    let mut cursor = index;
    if chars.get(cursor) == Some(&'b') {
        cursor += 1;
    }
    let raw = chars.get(cursor) == Some(&'r');
    if raw {
        cursor += 1;
    }
    let mut hashes = 0usize;
    if raw {
        while chars.get(cursor) == Some(&'#') {
            hashes += 1;
            cursor += 1;
        }
    }
    if chars.get(cursor) != Some(&'"') {
        return None;
    }

    let mut scan = cursor + 1;
    if raw {
        while scan < chars.len() {
            if chars[scan] == '"' && (1..=hashes).all(|k| chars.get(scan + k) == Some(&'#')) {
                return Some(scan + hashes + 1);
            }
            scan += 1;
        }
        return Some(chars.len());
    }
    while scan < chars.len() {
        match chars[scan] {
            '\\' => scan += 2,
            '"' => return Some(scan + 1),
            _ => scan += 1,
        }
    }
    Some(chars.len())
}

/// If a character literal begins at `chars[index]`, the index just past its end.
///
/// Distinguished from a lifetime, which is the whole difficulty: `']'` is a literal whose
/// bracket is data, while `'de` in `impl<'de> Deserialize<'de>` is not a literal at all and
/// must not swallow the rest of the line.
fn char_literal_end(chars: &[char], index: usize) -> Option<usize> {
    if chars.get(index) != Some(&'\'') {
        return None;
    }
    if chars.get(index + 1) == Some(&'\\') {
        // The escaped character comes first, then the closing quote. Scanning forward for
        // "the next `'`" stops at the *escaped* apostrophe in `'\''` and leaves a dangling
        // quote behind that reads as the start of another literal. Found by Codex at
        // `20926e1`.
        let mut scan = index + 2;
        if chars.get(scan) == Some(&'u') && chars.get(scan + 1) == Some(&'{') {
            scan += 2;
            while scan < chars.len() && chars[scan] != '}' {
                scan += 1;
            }
            scan += 1;
        } else {
            scan += 1;
        }
        return if chars.get(scan) == Some(&'\'') {
            Some(scan + 1)
        } else {
            None
        };
    }
    if chars.get(index + 2) == Some(&'\'') {
        return Some(index + 3);
    }
    None
}

/// Whether a literal prefix at `index` is a real prefix rather than the tail of a name.
fn starts_a_literal(chars: &[char], index: usize) -> bool {
    index == 0 || !(chars[index - 1].is_alphanumeric() || chars[index - 1] == '_')
}

/// The index just past a literal beginning at `index`, if one does.
fn literal_end(chars: &[char], index: usize) -> Option<usize> {
    match chars.get(index) {
        Some('"') => string_literal_end(chars, index),
        Some('r') if starts_a_literal(chars, index) => string_literal_end(chars, index),
        // `b'x'` is a byte-char literal; `b"…"` and `br#"…"#` are byte strings.
        Some('b') if starts_a_literal(chars, index) => {
            if chars.get(index + 1) == Some(&'\'') {
                char_literal_end(chars, index + 1)
            } else {
                string_literal_end(chars, index)
            }
        }
        Some('\'') => char_literal_end(chars, index),
        _ => None,
    }
}

/// Every `use` statement in `code`, whitespace-normalized, one per statement.
///
/// Run over comment-stripped code and anchored at a statement boundary: a doc comment
/// containing the word "use" produced a spurious 700-character "import" when this was
/// first written against raw source.
fn imports_in(code: &str) -> Vec<String> {
    let normalized = collapse_whitespace(code);
    let chars: Vec<char> = normalized.chars().collect();
    let mut found = Vec::new();
    let mut index = 0usize;

    while index < chars.len() {
        let is_use = chars[index..].starts_with(&['u', 's', 'e', ' ']);
        if is_use && starts_a_use_statement(&chars, index) {
            let mut scan = index;
            while scan < chars.len() && chars[scan] != ';' {
                scan += 1;
            }
            let statement: String = chars[index..scan.min(chars.len())].iter().collect();
            found.push(format!("{statement};"));
            index = scan + 1;
            continue;
        }
        index += 1;
    }
    found
}

/// Whether the `use` at `index` opens a statement rather than ending an identifier.
///
/// The previous rule rejected "alphanumeric then space" before `use`, to stop `misuse` and
/// prose from registering. It also rejected `pub use` — and *only* `pub use`, since
/// `pub(crate) use` ends in `)` — so the plainest re-export form in Rust was invisible to
/// the allowlist while its parenthesised siblings were caught. Wrong in the direction that
/// passes. Found by Codex at `20926e1`.
///
/// Now: the `use` must not be the tail of an identifier, and whatever immediately precedes
/// it must be a statement boundary or a visibility qualifier — `pub`, `pub(crate)`,
/// `pub(super)`, `pub(in some::path)`.
fn starts_a_use_statement(chars: &[char], index: usize) -> bool {
    if index == 0 {
        return true;
    }
    // `misuse ` must not count: `use` has to begin a word.
    let previous = chars[index - 1];
    if previous.is_alphanumeric() || previous == '_' {
        return false;
    }
    if matches!(previous, ';' | '{' | '}' | '\n') {
        return true;
    }
    if previous != ' ' {
        return false;
    }
    // A space precedes it. Accept only a statement start or a visibility qualifier.
    let before: String = chars[..index - 1].iter().collect();
    let head = before.trim_end();
    if head.is_empty() {
        return true;
    }
    if head.ends_with(';') || head.ends_with('{') || head.ends_with('}') {
        return true;
    }
    let last_word = head
        .rsplit(|c: char| c == ';' || c == '{' || c == '}' || c == '\n')
        .next()
        .unwrap_or_default()
        .trim();
    last_word == "pub" || (last_word.starts_with("pub(") && last_word.ends_with(')'))
}

/// Every trait implemented for `type_name` in `code`, as written.
///
/// The companion to the derive allowlist: a hand-written `impl` needs no derive and no
/// import. Terminator-checked so `AdaptiveEventLog` is not read as `AdaptiveEvent`.
fn trait_impls_for(code: &str, type_name: &str) -> Vec<String> {
    let normalized = collapse_whitespace(code);
    let needle = format!("for {type_name}");
    let mut found = Vec::new();
    let mut search_from = 0usize;

    while let Some(at) = normalized[search_from..].find(&needle) {
        let absolute = search_from + at;
        let after = normalized[absolute + needle.len()..].chars().next();
        let terminated = after.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if terminated {
            if let Some(impl_at) = normalized[..absolute].rfind("impl") {
                let header = normalized[impl_at + "impl".len()..absolute].trim();
                // Strip any generic parameter list introduced by `impl<…>`.
                let trait_part = header.strip_prefix('<').map_or(header, |rest| {
                    rest.split_once('>').map_or(rest, |(_, tail)| tail).trim()
                });
                if !trait_part.is_empty() {
                    found.push(trait_part.to_string());
                }
            }
        }
        search_from = absolute + needle.len();
    }
    found
}

/// Collapse whitespace runs to one space, then close it up around path punctuation.
///
/// Leaves ` as ` intact — which is the whole reason this is not a plain whitespace strip.
fn collapse_whitespace(code: &str) -> String {
    let mut out = String::with_capacity(code.len());
    let mut last_was_space = false;
    for c in code.chars() {
        if c.is_whitespace() {
            if !last_was_space {
                out.push(' ');
                last_was_space = true;
            }
        } else {
            out.push(c);
            last_was_space = false;
        }
    }
    for token in ["::", "{", "}", ",", ";"] {
        out = out.replace(&format!(" {token}"), token);
        out = out.replace(&format!("{token} "), token);
    }
    out
}

/// Split a brace group on commas that are not inside a nested group.
fn split_top_level(group: &str) -> Vec<&str> {
    let mut items = Vec::new();
    let mut depth = 0usize;
    let mut start = 0usize;
    for (index, c) in group.char_indices() {
        match c {
            '{' => depth += 1,
            '}' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                items.push(&group[start..index]);
                start = index + 1;
            }
            _ => {}
        }
    }
    if start < group.len() {
        items.push(&group[start..]);
    }
    items
        .into_iter()
        .map(str::trim)
        .filter(|i| !i.is_empty())
        .collect()
}

// =============================================================================
// Source scanning
// =============================================================================

/// Strip comments while keeping string literals.
///
/// Prose must not be linted: this file, `src/producers/mod.rs` and `src/bus/events.rs` all
/// discuss the boundary in doc comments, and punishing that would push the explanation out
/// of the code. String literals are kept so a forbidden path written as a string is still
/// caught.
fn code_only(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0usize;
    let mut block_depth = 0usize;

    while index < chars.len() {
        let c = chars[index];

        if block_depth > 0 {
            if c == '*' && chars.get(index + 1) == Some(&'/') {
                block_depth -= 1;
                index += 2;
                continue;
            }
            if c == '/' && chars.get(index + 1) == Some(&'*') {
                block_depth += 1;
                index += 2;
                continue;
            }
            if c == '\n' {
                out.push('\n');
            }
            index += 1;
            continue;
        }

        // A literal is data. `//` and `/*` inside one are not comment markers, and a `"`
        // inside a raw string does not end it.
        if let Some(end) = literal_end(&chars, index) {
            out.extend(&chars[index..end.min(chars.len())]);
            index = end;
            continue;
        }

        if c == '/' && chars.get(index + 1) == Some(&'/') {
            while index < chars.len() && chars[index] != '\n' {
                index += 1;
            }
            continue;
        }
        if c == '/' && chars.get(index + 1) == Some(&'*') {
            block_depth += 1;
            index += 2;
            continue;
        }
        out.push(c);
        index += 1;
    }
    out
}

fn sources_under(root: &str) -> Vec<(String, String)> {
    let path = Path::new(root);
    if !path.exists() {
        return Vec::new();
    }
    let mut sources = Vec::new();
    for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
        let file = entry.path();
        if file.extension().is_some_and(|ext| ext == "rs") {
            let text = fs::read_to_string(file)
                .unwrap_or_else(|error| panic!("failed to read {}: {error}", file.display()));
            sources.push((file.display().to_string(), text));
        }
    }
    sources
}

/// Join attributes that wrap across lines onto a single line.
///
/// [`attributes_attached_to`] and [`declaration_gates`] walk lines, and a line that is not
/// itself an attribute clears the pending set — that is the rule which stops one item's
/// attributes leaking onto the next. A wrapped attribute defeats it:
///
/// ```ignore
/// #[derive(          // seen as an unterminated attribute, held as pending
///     Debug,         // not an attribute -> pending cleared, the derive is lost
///     Serialize,
/// )]
/// pub enum AdaptiveEvent { … }
/// ```
///
/// The declaration then reports only `#[derive(Debug, Clone)]` and the scanner passes.
/// Verified against the real module before this existed: a wrapped
/// `#[cfg_attr(…, derive(serde::Serialize))]` compiled and
/// `lint_adaptive_event_derives_no_serialization` passed. Found by CodeRabbit at
/// `b35af10`.
///
/// Newlines are replaced by spaces only while inside an attribute's brackets, so the line
/// structure everywhere else — and therefore the pending-clearing rule — is untouched.
fn join_wrapped_attributes(code: &str) -> String {
    let chars: Vec<char> = code.chars().collect();
    let mut out = String::with_capacity(code.len());
    let mut index = 0usize;
    let mut depth = 0usize;

    while index < chars.len() {
        // A literal is copied through whole, whatever it contains. This is the raw-string
        // fix: `r##"quote: " then ] remains data"##` no longer ends at its embedded quote,
        // so its `]` is data rather than an attribute close.
        if let Some(end) = literal_end(&chars, index) {
            let slice: String = chars[index..end.min(chars.len())].iter().collect();
            if depth > 0 {
                out.push_str(&slice.replace('\n', " "));
            } else {
                out.push_str(&slice);
            }
            index = end;
            continue;
        }

        let c = chars[index];
        if c == '#' {
            let bracket = if chars.get(index + 1) == Some(&'!') {
                index + 2
            } else {
                index + 1
            };
            if chars.get(bracket) == Some(&'[') {
                out.extend(&chars[index..=bracket]);
                depth += 1;
                index = bracket + 1;
                continue;
            }
        }
        match c {
            '[' if depth > 0 => {
                depth += 1;
                out.push(c);
            }
            ']' if depth > 0 => {
                depth -= 1;
                out.push(c);
            }
            '\n' if depth > 0 => out.push(' '),
            _ => out.push(c),
        }
        index += 1;
    }
    out
}

/// Every `#[...]` attribute written in `text`, whole.
///
/// Bracket-depth aware and string-literal aware, so `#[cfg_attr(feature = "server",
/// derive(Serialize))]` and `#[doc = "contains a ] bracket"]` are each captured entire
/// rather than truncated at the first `]`.
fn attributes_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0usize;

    while index + 1 < chars.len() {
        if chars[index] != '#' || (chars[index + 1] != '[' && chars[index + 1] != '!') {
            index += 1;
            continue;
        }
        let open = if chars[index + 1] == '!' {
            index + 2
        } else {
            index + 1
        };
        if chars.get(open) != Some(&'[') {
            index += 1;
            continue;
        }
        let inner = chars[index + 1] == '!';
        let mut depth = 0usize;
        let mut cursor = open;
        let mut end = None;
        while cursor < chars.len() {
            if let Some(past) = literal_end(&chars, cursor) {
                cursor = past;
                continue;
            }
            match chars[cursor] {
                '[' => depth += 1,
                ']' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(cursor);
                        break;
                    }
                }
                _ => {}
            }
            cursor += 1;
        }
        match end {
            Some(close) => {
                if !inner {
                    found.push(chars[index..=close].iter().collect());
                }
                index = close + 1;
            }
            None => {
                found.push(chars[index..].iter().collect());
                break;
            }
        }
    }
    found
}

/// Every `#[cfg(...)]` attribute written in `text`.
///
/// Extracted rather than matched whole-line, because a gate may share a line with the item
/// it gates. `#[cfg_attr(...)]` must not match: it cannot exclude a module.
fn cfg_attributes_in(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("#[cfg(") {
        let tail = &rest[start..];
        match tail.find(")]") {
            Some(end) => {
                found.push(tail[..end + 2].to_string());
                rest = &tail[end + 2..];
            }
            None => {
                found.push(tail.trim().to_string());
                break;
            }
        }
    }
    found
}

/// Every `#[cfg(...)]` that applies to `declaration` in `source`.
///
/// `None` means the declaration is absent; an empty vector means it is present and ungated.
///
/// Pending attributes **accumulate** across consecutive attribute lines, because Rust allows
/// any number in any order — `#[cfg(...)]` / `#[allow(...)]` / `pub mod x;` is gated. The
/// accumulation stops at whatever consumes it (the declaration, a block opener, or the next
/// item), or every ungated `pub mod` below a gated one would be reported as gated.
fn declaration_gates(source: &str, declaration: &str) -> Option<Vec<String>> {
    let mut enclosing: Vec<Vec<String>> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut gates: Option<Vec<String>> = None;

    // Wrapped attributes are joined first: a line-oriented walk would see only `#[cfg(` and
    // then clear it on the continuation line. See `join_wrapped_attributes`.
    let joined = join_wrapped_attributes(&code_only(source));
    for line in joined.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(prefix) = trimmed.strip_suffix(declaration) {
            let mut found = cfg_attributes_in(prefix);
            found.append(&mut pending);
            found.extend(enclosing.iter().flatten().cloned());
            gates = Some(found);
            continue;
        }
        if trimmed.ends_with('{') {
            let mut frame = cfg_attributes_in(trimmed);
            frame.append(&mut pending);
            enclosing.push(frame);
            continue;
        }
        if trimmed == "}" {
            enclosing.pop();
            continue;
        }
        if trimmed.starts_with('#') {
            pending.extend(cfg_attributes_in(trimmed));
            continue;
        }
        pending.clear();
    }
    gates
}

/// Every attribute attached to `declaration` in `source`, including those on an enclosing
/// block.
///
/// `None` means the declaration is absent; an empty vector means it carries no attributes.
///
/// Same accumulation rule as [`declaration_gates`] — Rust allows any number of attributes in
/// any order, so they gather across consecutive attribute lines and are consumed by whatever
/// comes next. Inspecting only the *nearest* attribute is the blind spot this replaced: an
/// earlier `#[derive(Serialize)]` sitting above a later `#[derive(Debug, Clone)]` was
/// invisible.
fn attributes_attached_to(source: &str, declaration: &str) -> Option<Vec<String>> {
    let mut enclosing: Vec<Vec<String>> = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let mut attached: Option<Vec<String>> = None;

    // Wrapped attributes are joined first. Without this a `#[derive(` spanning lines is
    // held as pending, then discarded by its own continuation line, and the declaration
    // reports only the attributes that happened to fit on one line. See
    // `join_wrapped_attributes`.
    let joined = join_wrapped_attributes(&code_only(source));
    for line in joined.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Some(prefix) = trimmed.strip_suffix(declaration) {
            let mut found = attributes_in(prefix);
            found.append(&mut pending);
            found.extend(enclosing.iter().flatten().cloned());
            attached = Some(found);
            continue;
        }
        if trimmed.ends_with('{') {
            let mut frame = attributes_in(trimmed);
            frame.append(&mut pending);
            enclosing.push(frame);
            continue;
        }
        if trimmed == "}" {
            enclosing.pop();
            continue;
        }
        if trimmed.starts_with('#') {
            pending.extend(attributes_in(trimmed));
            continue;
        }
        pending.clear();
    }
    attached
}

/// Every name by which serde's `Serialize`/`Deserialize` can be spelled in `code`.
///
/// Always includes the canonical two. Adds any **alias**, because
/// `use serde::Serialize as EventWire;` binds the trait *and* the derive macro under that
/// name — so `#[derive(EventWire)]` and `impl EventWire for AdaptiveEvent` both compile and
/// neither contains the substring `Serialize`. Verified by compiling both forms against the
/// real module before this was written. Found by CodeRabbit at `b35af10`.
///
/// Matching is on the **exact final path segment**, never a prefix: `serde::Serializer` and
/// `serde::de::DeserializeOwned` are ordinary serde imports that must not register as the
/// serialization traits, and both would match a substring test.
fn serialization_names(code: &str) -> Vec<String> {
    let mut names: Vec<String> = vec!["Serialize".to_string(), "Deserialize".to_string()];
    let normalized = collapse_whitespace(code);
    let mut rest = normalized.as_str();

    while let Some(at) = rest.find("use serde") {
        let tail = &rest[at + 4..];
        let statement = match tail.find(';') {
            Some(end) => &tail[..end],
            None => tail,
        };
        rest = &tail[statement.len().min(tail.len())..];

        // `use serde::{a, b}` or `use serde::a` (possibly via `ser::` / `de::`).
        let items: Vec<String> = match (statement.find('{'), statement.rfind('}')) {
            (Some(open), Some(close)) if open < close => {
                let base = &statement[..open];
                split_top_level(&statement[open + 1..close])
                    .into_iter()
                    .map(|item| format!("{base}{item}"))
                    .collect()
            }
            _ => vec![statement.to_string()],
        };

        for item in items {
            let (path, alias) = match item.split_once(" as ") {
                Some((path, alias)) => (path.trim(), Some(alias.trim())),
                None => (item.trim(), None),
            };
            let Some(last) = path.rsplit("::").next() else {
                continue;
            };
            if last != "Serialize" && last != "Deserialize" {
                continue;
            }
            if let Some(alias) = alias {
                if !alias.is_empty() && !names.iter().any(|held| held == alias) {
                    names.push(alias.to_string());
                }
            }
        }
    }
    names
}

/// The trait names an attribute would derive, as final path segments.
///
/// Reads every `derive(...)` list in the attribute, including one nested inside
/// `cfg_attr(...)`, and splits it into tokens so matching is exact rather than substring —
/// `#[derive(Serializer)]` must not be mistaken for `#[derive(Serialize)]`.
fn derive_tokens(attribute: &str) -> Vec<String> {
    let normalized = collapse_whitespace(attribute);
    let mut tokens = Vec::new();
    let mut rest = normalized.as_str();
    while let Some(at) = rest.find("derive(") {
        let after = &rest[at + "derive(".len()..];
        let mut depth = 1usize;
        let mut end = after.len();
        for (index, c) in after.char_indices() {
            match c {
                '(' => depth += 1,
                ')' => {
                    depth -= 1;
                    if depth == 0 {
                        end = index;
                        break;
                    }
                }
                _ => {}
            }
        }
        for item in after[..end].split(',') {
            let token = item.trim().rsplit("::").next().unwrap_or_default().trim();
            if !token.is_empty() {
                tokens.push(token.to_string());
            }
        }
        rest = &after[end.min(after.len())..];
    }
    tokens
}

/// Whether an attribute would make its item serializable, under any name.
///
/// Covers `#[derive(Serialize)]`, `#[cfg_attr(…, derive(Serialize))]` and
/// `#[derive(EventWire)]` where `EventWire` is an alias for one of the traits. A `cfg_attr`
/// gate does not make it safe — it makes it conditional, and the condition is somebody
/// else's to set.
fn introduces_serialization(attribute: &str, names: &[String]) -> Option<String> {
    derive_tokens(attribute)
        .into_iter()
        .find(|token| names.iter().any(|name| name == token))
}

/// Whether `code` hand-writes a serialization impl for `type_name`, under any name.
///
/// Scans back from each `for <type_name>` to the `impl` that opened it, so
/// `impl<'de> Deserialize<'de> for AdaptiveEvent` is caught — the previous check looked for
/// the literal `DeserializeforAdaptiveEvent` and that idiomatic form never produces it.
fn handwritten_serialization_impl(code: &str, type_name: &str, names: &[String]) -> Option<String> {
    let normalized = collapse_whitespace(code);
    let needle = format!("for {type_name}");
    let mut search_from = 0usize;
    while let Some(at) = normalized[search_from..].find(&needle) {
        let absolute = search_from + at;
        // The character after the match must end the type name, or `AdaptiveEventLog` would
        // count as `AdaptiveEvent`.
        let after = normalized[absolute + needle.len()..].chars().next();
        let terminated = after.is_none_or(|c| !c.is_alphanumeric() && c != '_');
        if terminated {
            if let Some(impl_at) = normalized[..absolute].rfind("impl") {
                let header = &normalized[impl_at..absolute];
                for name in names {
                    let mentioned = header
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|token| token == name);
                    if mentioned {
                        return Some(name.clone());
                    }
                }
            }
        }
        search_from = absolute + needle.len();
    }
    None
}

// =============================================================================
// Boundary 1: the public bus cannot carry adaptive data
// =============================================================================

#[test]
fn lint_public_bus_cannot_carry_adaptive_types() {
    // `BusEvent` is a wire payload, not merely an internal enum: `src/api/mod.rs`
    // serializes it verbatim into `GET /events`. A variant naming an adaptive type would
    // change that endpoint's response schema and publish the v1 contract outside this
    // repository, both of which #324 is scoped not to do.
    let mut violations = Vec::new();
    for (path, text) in sources_under("src/bus") {
        let code = code_only(&text);
        for needle in ["adaptive", "producers", "ProducerDocument", "AdaptiveEvent"] {
            if code.contains(needle) {
                violations.push(format!("{path}: references `{needle}`"));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "the public bus must not carry adaptive data - `GET /events` serializes every \
         BusEvent verbatim, so a variant here is a public response-schema change:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_the_sse_projection_does_not_mention_the_publication_layer() {
    let api = fs::read_to_string("src/api/mod.rs").expect("src/api/mod.rs readable");
    let code = code_only(&api);
    for needle in ["crate::producers", "crate::adaptive"] {
        assert!(
            !code.contains(needle),
            "src/api/mod.rs references `{needle}`. Any HTTP or SSE exposure of the producer \
             document needs explicit API approval and an `api-change-approved` label that \
             must never be self-applied."
        );
    }
}

// =============================================================================
// Boundary 2: no surface reaches the contract or the publication layer
// =============================================================================

#[test]
fn lint_only_the_composition_root_names_the_contract_or_the_publication_layer() {
    // Swept over *all* of `src/` rather than an enumerated list of surfaces. An enumerated
    // list is the same shape of blind spot that has now been remediated twice in
    // `tests/adaptive_dependency_lint.rs`: it silently stops covering whatever is added
    // next. The first draft of this lint listed six directories and omitted `src/mqtt`,
    // which publishes to Home Assistant — an out-of-repository consumer, and therefore the
    // single worst omission available.
    //
    // Exempt, with reasons:
    //   src/adaptive, src/producers — the layers themselves
    //   src/lib.rs, src/main.rs     — the composition root, which must name both to wire them
    const EXEMPT: &[&str] = &["src/adaptive", "src/producers", "src/lib.rs", "src/main.rs"];
    let mut violations = Vec::new();
    let mut swept = 0usize;
    for (path, text) in sources_under("src") {
        if EXEMPT.iter().any(|exempt| path.starts_with(exempt)) {
            continue;
        }
        swept += 1;
        for reference in forbidden_module_references(&code_only(&text)) {
            violations.push(format!("{path}: references `{reference}`"));
        }
    }
    assert!(
        swept > 50,
        "the sweep covered only {swept} files, which means it is not actually walking src/"
    );
    assert!(
        violations.is_empty(),
        "#324 publishes to in-repository consumers only, and `ProducerDocument` derives \
         `Serialize` — one `Json(snapshot)` re-exports the whole contract. A surface reading \
         the producer document is #326's decision and needs its own API approval:\n{}",
        violations.join("\n")
    );
}

#[test]
fn lint_adaptive_event_derives_no_serialization() {
    // Belt to the braces above: even if a variant were added to the wrong enum, the
    // internal event type itself cannot be written to a wire.
    //
    // Inspects *every* attached attribute, not the nearest one. An earlier
    // `#[derive(Serialize)]` above a later `#[derive(Debug, Clone)]` was invisible to the
    // previous `rfind("#[derive(")` scan, as was any `#[cfg_attr(…, derive(Serialize))]`,
    // which the scan never looked for at all. Found by CodeRabbit at `eebde11`.
    //
    // Aliases are resolved rather than assumed away: `use serde::Serialize as EventWire;`
    // binds the derive macro *and* the trait under a name containing no forbidden
    // substring. Found by CodeRabbit at `b35af10`.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let code = code_only(&event);
    let names = serialization_names(&code);

    let attributes =
        attributes_attached_to(&event, ADAPTIVE_EVENT_DECLARATION).unwrap_or_else(|| {
            panic!("src/producers/event.rs must declare `{ADAPTIVE_EVENT_DECLARATION}`")
        });
    let offenders: Vec<String> = attributes
        .iter()
        .filter_map(|attribute| {
            introduces_serialization(attribute, &names)
                .map(|trait_name| format!("{trait_name} via {attribute}"))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "AdaptiveEvent would be serializable. The internal bus exists precisely so producer \
         lifecycle cannot reach a wire, and a derive is the only structural part of that \
         guarantee. Found: {offenders:?}"
    );

    // A hand-written impl defeats the same guarantee without any derive at all.
    if let Some(name) = handwritten_serialization_impl(&code, "AdaptiveEvent", &names) {
        panic!(
            "AdaptiveEvent has a hand-written `{name}` impl, which reaches a wire just as \
             surely as a derive."
        );
    }
}

#[test]
fn lint_the_internal_event_module_imports_only_what_it_is_allowed_to() {
    // The durable half. Every name-based guard in this file has now been bypassed —
    // aliases, wrapped attributes, raw strings, and finally a crate-local re-export that
    // leaves no trace of serde in the file at all. Detection loses that race by
    // construction; permission does not.
    //
    // Anything not on `EVENT_ALLOWED_IMPORTS` fails, whatever it is named and wherever it
    // was re-exported from.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let imports = imports_in(&code_only(&event));

    let unexpected: Vec<&String> = imports
        .iter()
        .filter(|import| !EVENT_ALLOWED_IMPORTS.contains(&import.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "src/producers/event.rs imports something not on the allowlist: {unexpected:?}\n\
         This module holds the one producer type that must not reach a wire. A new import \
         may be entirely fine — add it to EVENT_ALLOWED_IMPORTS deliberately, having \
         checked it is not a serialization trait re-exported under another name."
    );
    assert!(
        !imports.is_empty(),
        "no imports were found at all, which means the scanner is not reading the file"
    );
}

#[test]
fn lint_adaptive_event_derives_only_what_it_is_allowed_to() {
    // A derive needs no import: `#[derive(crate::wire::EventWire)]` names the macro by
    // path, so the import allowlist alone does not close it. Permitting `Debug` and `Clone`
    // and nothing else closes every spelling at once.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let attributes =
        attributes_attached_to(&event, ADAPTIVE_EVENT_DECLARATION).unwrap_or_else(|| {
            panic!("src/producers/event.rs must declare `{ADAPTIVE_EVENT_DECLARATION}`")
        });

    let derived: Vec<String> = attributes.iter().flat_map(|a| derive_tokens(a)).collect();
    let unexpected: Vec<&String> = derived
        .iter()
        .filter(|token| !ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&token.as_str()))
        .collect();
    assert!(
        unexpected.is_empty(),
        "AdaptiveEvent derives something not on the allowlist: {unexpected:?}\n\
         Permitted: {ADAPTIVE_EVENT_ALLOWED_DERIVES:?}. A derive under an alias or a \
         re-exported path is indistinguishable from any other name, so the list is what \
         decides."
    );
    assert!(
        !derived.is_empty(),
        "no derives were found at all, which means the scanner is not reading the type"
    );
}

#[test]
fn lint_adaptive_event_implements_no_traits() {
    // The third surface a name-based guard has to cover, and the one a re-export hides
    // best: `impl EventWire for AdaptiveEvent` needs neither serde in the file nor a
    // derive. AdaptiveEvent implements nothing by hand today, so the allowlist is empty.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let impls = trait_impls_for(&code_only(&event), "AdaptiveEvent");
    assert!(
        impls.is_empty(),
        "AdaptiveEvent has hand-written trait impls: {impls:?}\n\
         None are expected. If one becomes necessary, allow it here having checked it is \
         not a serialization trait under another name."
    );
}

#[test]
fn lint_the_internal_event_module_imports_no_serialization_trait() {
    // The structural half, and the one that makes aliasing moot rather than merely detected.
    //
    // Resolving aliases catches `use serde::Serialize as EventWire;`. Forbidding the import
    // outright removes the question: this module exists to hold the one producer type that
    // cannot reach a wire, so it has no legitimate need for a serialization trait under any
    // name. If that ever changes it should be a deliberate edit that has to delete this
    // test, not an import nobody noticed.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let code = code_only(&event);

    let aliases: Vec<String> = serialization_names(&code)
        .into_iter()
        .filter(|name| name != "Serialize" && name != "Deserialize")
        .collect();
    assert!(
        aliases.is_empty(),
        "src/producers/event.rs aliases a serialization trait: {aliases:?}. An alias makes \
         `#[derive(…)]` and `impl … for AdaptiveEvent` invisible to any literal scan."
    );

    assert!(
        !code.contains("serde"),
        "src/producers/event.rs references serde. This module exists to hold the one \
         producer type that cannot reach a wire; a serialization import here needs a \
         deliberate justification, not a silent addition."
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
// The publication layer is server-only
// =============================================================================

#[test]
fn lint_producers_module_is_server_gated() {
    // Opposite polarity to `pub mod adaptive;`, which must be *ungated* so the WASM build
    // can use the contract types. `src/producers/` depends on `crate::bus` and `tokio`, so
    // an ungated declaration breaks `dx build --platform web` in CI rather than anything a
    // host `cargo test` would notice.
    let lib = fs::read_to_string("src/lib.rs").expect("src/lib.rs readable");
    let gates = declaration_gates(&lib, PRODUCERS_DECLARATION)
        .unwrap_or_else(|| panic!("src/lib.rs must declare `{PRODUCERS_DECLARATION}`"));
    assert!(
        gates.iter().any(|gate| is_server_gate(gate)),
        "`{PRODUCERS_DECLARATION}` must be `{SERVER_GATE}`; it depends on crate::bus and \
         tokio, neither of which exists in the WASM build. Found gates: {gates:?}"
    );
}

#[test]
fn lint_publication_layer_is_reachable_only_from_the_composition_root() {
    // If nothing outside `src/producers/` names it except `src/lib.rs` and `src/main.rs`,
    // then the exposure surface is exactly one file and reviewing it is tractable.
    let mut referrers = Vec::new();
    for (path, text) in sources_under("src") {
        if path.starts_with("src/producers") || path == "src/lib.rs" || path == "src/main.rs" {
            continue;
        }
        if forbidden_module_references(&code_only(&text))
            .iter()
            .any(|reference| reference.contains("producers"))
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
// Non-vacuity probes
// =============================================================================

#[test]
fn gate_scanner_detects_a_gate_it_must_not_miss() {
    // Each case was verified against the shapes that defeated earlier versions of the
    // sibling lint in `tests/adaptive_dependency_lint.rs`.
    for source in [
        // Directly above.
        "#[cfg(feature = \"server\")]\npub mod producers;\n",
        // Stacked behind another attribute - the `d6da952` blind spot.
        "#[cfg(feature = \"server\")]\n#[allow(dead_code)]\npub mod producers;\n",
        // Other order.
        "#[allow(dead_code)]\n#[cfg(feature = \"server\")]\npub mod producers;\n",
        // Unspaced.
        "#[cfg(feature=\"server\")]\npub mod producers;\n",
        // Sharing the line.
        "#[cfg(feature = \"server\")] pub mod producers;\n",
        // Behind a doc comment that names the declaration, so a `find`-based scan would
        // inspect the wrong line.
        "/// see `pub mod producers;`\n#[cfg(feature = \"server\")]\npub mod producers;\n",
        // Gated by an enclosing block.
        "#[cfg(feature = \"server\")]\nmod inner {\npub mod producers;\n}\n",
    ] {
        let gates = declaration_gates(source, PRODUCERS_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        assert!(
            gates.iter().any(|gate| is_server_gate(gate)),
            "a server gate was missed in:\n{source}\nfound: {gates:?}"
        );
    }
}

#[test]
fn gate_scanner_does_not_invent_a_gate_that_is_not_there() {
    // The direction that matters for this module's polarity. A scanner that hallucinates a
    // gate would let an ungated `pub mod producers;` pass and break the WASM build in CI.
    for source in [
        // No gate at all.
        "pub mod producers;\n",
        // A gate that belongs to the item above it, not to this declaration.
        "#[cfg(feature = \"server\")]\npub mod bus;\npub mod producers;\n",
        // A gate consumed by a block that has already closed.
        "#[cfg(feature = \"server\")]\nmod inner {\npub mod other;\n}\npub mod producers;\n",
        // `cfg_attr` cannot exclude a module.
        "#[cfg_attr(feature = \"server\", allow(dead_code))]\npub mod producers;\n",
        // A different feature is not the server gate.
        "#[cfg(feature = \"web\")]\npub mod producers;\n",
        // Disjunctive: contains `feature="server"` but compiles the module for `web`
        // *without* `server`, which is the WASM breakage this lint exists to prevent.
        // Passed the pre-fix scanner. Found by CodeRabbit at `86e5bd2`.
        "#[cfg(any(feature = \"server\", feature = \"web\"))]\npub mod producers;\n",
        "#[cfg(any(feature=\"server\",feature=\"web\"))]\npub mod producers;\n",
        // Negation, for the same reason in the opposite direction.
        "#[cfg(not(feature = \"server\"))]\npub mod producers;\n",
    ] {
        let gates = declaration_gates(source, PRODUCERS_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        assert!(
            !gates.iter().any(|gate| is_server_gate(gate)),
            "a server gate was invented for:\n{source}\nfound: {gates:?}"
        );
    }
}

#[test]
fn declaration_scanner_reports_absence_rather_than_guessing() {
    assert_eq!(
        declaration_gates("pub mod adaptive;\n", PRODUCERS_DECLARATION),
        None
    );
    // A doc comment mentioning it is not a declaration.
    assert_eq!(
        declaration_gates("/// pub mod producers;\n", PRODUCERS_DECLARATION),
        None
    );
}

#[test]
fn a_stricter_conjunctive_gate_is_still_server_only() {
    // `all(feature = "server", unix)` compiles the module in strictly fewer configurations
    // than the plain gate, so it cannot reintroduce the WASM breakage. Accepted, unlike the
    // disjunctive form above - the distinction is which direction the gate is loosened.
    let source = "#[cfg(all(feature = \"server\", unix))]\npub mod producers;\n";
    let gates = declaration_gates(source, PRODUCERS_DECLARATION).expect("declaration found");
    assert!(gates.iter().any(|gate| is_server_gate(gate)));
}

#[test]
fn a_serializing_adaptive_event_is_rejected_however_it_is_spelled() {
    // Non-vacuity for `lint_adaptive_event_derives_no_serialization`. Cases 2 and 4 through
    // 7 all passed the previous scanner, which read only the *nearest* `#[derive(` and never
    // looked for `cfg_attr` at all. Found by CodeRabbit at `eebde11`.
    for source in [
        // 1. The obvious form.
        "#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n",
        // 2. Stacked behind the innocent derive - the reported blind spot.
        "#[derive(Serialize, Deserialize)]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        // 3. The other order.
        "#[derive(Debug, Clone)]\n#[derive(Deserialize)]\npub enum AdaptiveEvent {\n}\n",
        // 4. Behind an unrelated attribute.
        "#[derive(Serialize)]\n#[non_exhaustive]\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n",
        // 5. Conditional. A cfg_attr gate does not make it safe, it makes it somebody
        //    else's build flag.
        "#[cfg_attr(feature = \"server\", derive(Serialize))]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        // 6. Conditional, unspaced, and behind the innocent derive.
        "#[derive(Debug,Clone)]\n#[cfg_attr(test,derive(Deserialize))]\npub enum AdaptiveEvent {\n}\n",
        // 7. Sharing the line with the declaration.
        "#[derive(Debug)] #[derive(Serialize)] pub enum AdaptiveEvent {\n}\n",
    ] {
        let names = serialization_names(source);
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        assert!(
            attributes
                .iter()
                .any(|attribute| introduces_serialization(attribute, &names).is_some()),
            "a serializing AdaptiveEvent was admitted:\n{source}\nattributes: {attributes:?}"
        );
    }
}

#[test]
fn a_non_serializing_adaptive_event_is_accepted() {
    // The other direction. A scanner that rejects everything is as useless as one that
    // rejects nothing, and case 3 is the false positive the previous implementation
    // avoided by accident and this one must avoid on purpose: a *different* type's
    // serializing derive higher up the file.
    for source in [
        "#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        "#[derive(Debug, Clone)]\n#[non_exhaustive]\npub enum AdaptiveEvent {\n}\n",
        "#[derive(Serialize, Deserialize)]\npub struct Unrelated {\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        "#[cfg_attr(test, derive(Default))]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
    ] {
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
        let names = serialization_names(source);
        let offenders: Vec<_> = attributes
            .iter()
            .filter(|attribute| introduces_serialization(attribute, &names).is_some())
            .collect();
        assert!(
            offenders.is_empty(),
            "the scanner invented a serialization derive in:\n{source}\nfound: {offenders:?}"
        );
    }
}

#[test]
fn a_crate_local_re_export_is_rejected_by_the_allowlists() {
    // The exploit CodeRabbit supplied at `993d35e`, verbatim in shape. `src/wire.rs` does
    // `pub use serde::Serialize as EventWire;` and `event.rs` imports `crate::wire::EventWire`
    // — no `serde` anywhere in the file, and nothing short of a real name resolver learns
    // what `EventWire` is. Verified by compiling it: the build was clean and all 24 lints
    // passed.
    //
    // The allowlists do not try to learn. They refuse the import and refuse the derive.
    let re_export_derive = "use crate::wire::EventWire;\nuse std::sync::Arc;\n#[derive(EventWire)]\npub enum AdaptiveEvent {\n}\n";
    let imports = imports_in(&code_only(re_export_derive));
    assert!(
        imports
            .iter()
            .any(|import| !EVENT_ALLOWED_IMPORTS.contains(&import.as_str())),
        "the re-exported import was admitted: {imports:?}"
    );
    let attributes = attributes_attached_to(re_export_derive, ADAPTIVE_EVENT_DECLARATION)
        .expect("declaration found");
    let derived: Vec<String> = attributes.iter().flat_map(|a| derive_tokens(a)).collect();
    assert!(
        derived
            .iter()
            .any(|token| !ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&token.as_str())),
        "the re-exported derive was admitted: {derived:?}"
    );

    // A derive by fully-qualified path needs no import at all, so the import allowlist
    // alone would not have closed it.
    let qualified = "#[derive(crate::wire::EventWire)]\npub enum AdaptiveEvent {\n}\n";
    let attributes =
        attributes_attached_to(qualified, ADAPTIVE_EVENT_DECLARATION).expect("declaration");
    let derived: Vec<String> = attributes.iter().flat_map(|a| derive_tokens(a)).collect();
    assert!(
        derived
            .iter()
            .any(|token| !ADAPTIVE_EVENT_ALLOWED_DERIVES.contains(&token.as_str())),
        "a fully-qualified re-exported derive was admitted: {derived:?}"
    );

    // The hand-written form, which has the same gap and no derive to catch.
    for source in [
        "use crate::wire::EventWire;\nimpl EventWire for AdaptiveEvent {}\n",
        "impl crate::wire::EventWire for AdaptiveEvent {}\n",
        "impl<'de> crate::wire::EventRead<'de> for AdaptiveEvent {}\n",
    ] {
        let impls = trait_impls_for(&code_only(source), "AdaptiveEvent");
        assert!(
            !impls.is_empty(),
            "a re-exported hand-written impl was admitted:\n{source}"
        );
    }
}

#[test]
fn the_allowlists_accept_the_module_as_it_actually_is() {
    // Positive control, and the one that keeps the allowlists honest rather than merely
    // strict: the real module must pass without exception. A list that only ever fails is
    // as useless as one that only ever passes.
    let event = fs::read_to_string("src/producers/event.rs").expect("src/producers/event.rs");
    let code = code_only(&event);

    for import in imports_in(&code) {
        assert!(
            EVENT_ALLOWED_IMPORTS.contains(&import.as_str()),
            "the allowlist rejects an import the module legitimately has: {import:?}"
        );
    }
    assert!(
        trait_impls_for(&code, "AdaptiveEvent").is_empty(),
        "the impl scanner invented an impl in the real module"
    );

    // And an unrelated impl for a *different* type in the same file must not register.
    let other = "impl Default for AdaptiveBus {}\nimpl AdaptiveEvent {}\n";
    assert!(
        trait_impls_for(other, "AdaptiveEvent").is_empty(),
        "an inherent impl or another type's impl was counted: {:?}",
        trait_impls_for(other, "AdaptiveEvent")
    );
}

#[test]
fn raw_strings_do_not_break_the_shared_lexer() {
    // A raw string ends at `"` followed by its own hash count, not at the first `"`. A
    // scanner that leaves string mode at an embedded quote then reads the following `]` as
    // the attribute close, so the attribute ends early, the next wrapped attribute is never
    // joined, and the line walk discards it. Found by CodeRabbit at `993d35e`, which also
    // supplied the working exploit.
    let raw_then_wrapped = "#[doc = r##\"quote: \" then ] remains data\"##]\n\
                            #[cfg_attr(\n    feature = \"x\",\n    derive(serde::Serialize)\n)]\n\
                            pub enum AdaptiveEvent {\n}\n";

    // Asserted on the property rather than on exact spacing: the wrapped attribute and its
    // derive must end up on one line. Indentation inside the attribute is preserved, so a
    // literal comparison would pin formatting rather than behaviour.
    let joined = join_wrapped_attributes(raw_then_wrapped);
    let cfg_line = joined
        .lines()
        .find(|line| line.contains("#[cfg_attr("))
        .unwrap_or_default();
    assert!(
        cfg_line.contains("derive(serde::Serialize)") && cfg_line.trim_end().ends_with(")]"),
        "a raw string ended the attribute early, so the wrapped derive was never joined:\n{joined}"
    );

    let names = serialization_names(raw_then_wrapped);
    let attributes = attributes_attached_to(raw_then_wrapped, ADAPTIVE_EVENT_DECLARATION)
        .expect("declaration found");
    assert!(
        attributes
            .iter()
            .any(|attribute| introduces_serialization(attribute, &names).is_some()),
        "a raw doc attribute hid a serializing derive:\n{attributes:?}"
    );

    // The same limitation in `code_only`: a `//` or `/*` inside a raw string is data, not a
    // comment, and treating it as one silently deletes the rest of the line.
    let raw_with_comment_markers = "#[doc = r\"a // b /* c\"]\nuse crate::adaptive::X;\n";
    assert!(
        code_only(raw_with_comment_markers).contains("use crate::adaptive::X;"),
        "a raw string containing comment markers swallowed the code after it: {:?}",
        code_only(raw_with_comment_markers)
    );

    // Hash-count matching: an inner `"#` must not terminate a `r##"…"##` literal.
    let nested_hashes = "#[doc = r##\"ends with \"# not here\"##]\n#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n";
    let attributes =
        attributes_attached_to(nested_hashes, ADAPTIVE_EVENT_DECLARATION).expect("declaration");
    assert!(
        attributes.iter().any(|attribute| introduces_serialization(
            attribute,
            &["Serialize".to_string()]
        )
        .is_some()),
        "hash-count matching failed, hiding the derive:\n{attributes:?}"
    );
}

#[test]
fn char_literals_are_data_not_brackets_or_quotes() {
    // An earlier version of this file claimed to test char literals in a case whose source
    // contained none — `#[doc = "x"]` is an ordinary string. Codex caught the mislabel and
    // the real defect underneath it. These are actual char literals.
    for (label, source) in [
        // A bracket inside a char literal must not close the attribute.
        (
            "bracket",
            "#[x = ']']\n#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n",
        ),
        // An escaped apostrophe. `char_literal_end` returned at the *escaped* quote rather
        // than the closing one, leaving a dangling `'` that reads as another literal.
        (
            "escaped quote",
            "#[x = '\\'']\n#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "escaped backslash",
            "#[x = '\\\\']\n#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "byte char",
            "#[x = b']']\n#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "unicode escape that is a closing bracket",
            "#[x = '\\u{5D}']\n#[derive(Serialize)]\npub enum AdaptiveEvent {\n}\n",
        ),
    ] {
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found for {label}:\n{source}"));
        assert!(
            attributes.iter().any(|attribute| introduces_serialization(
                attribute,
                &["Serialize".to_string()]
            )
            .is_some()),
            "{label}: a char literal ate the derive:\n{attributes:?}"
        );
    }

    // The lexer directly, so a failure names the cause rather than a symptom.
    for (literal, expected) in [
        ("']'", Some(3)),
        ("'\\''", Some(4)),
        ("'\\\\'", Some(4)),
        ("'\\n'", Some(4)),
        ("'\\u{5D}'", Some(8)),
        ("'a'", Some(3)),
    ] {
        let chars: Vec<char> = literal.chars().collect();
        assert_eq!(
            char_literal_end(&chars, 0),
            expected,
            "char literal {literal:?} was mis-measured"
        );
    }

    // Lifetimes are not char literals and must consume nothing.
    for lifetime in ["'de", "'a", "'static"] {
        let chars: Vec<char> = lifetime.chars().collect();
        assert_eq!(
            char_literal_end(&chars, 0),
            None,
            "lifetime {lifetime:?} was read as a char literal"
        );
    }
    assert!(
        handwritten_serialization_impl(
            "impl<'de> Deserialize<'de> for AdaptiveEvent {}\n",
            "AdaptiveEvent",
            &["Deserialize".to_string()]
        )
        .is_some(),
        "a lifetime swallowed a hand-written impl"
    );
}

#[test]
fn visibility_qualified_use_statements_are_still_imports() {
    // `pub use serde::Serialize as EventWire;` written directly in the module is the same
    // re-export exploit as the previous round, one file closer. `imports_in` explicitly
    // rejected "alphanumeric then space" before `use`, so `pub use` — and only `pub use` —
    // was invisible, while `pub(crate) use` was caught because `)` is not alphanumeric.
    // Inconsistent, and wrong in the direction that passes. Found by Codex at `20926e1`.
    for (label, source) in [
        ("plain", "use serde::Serialize as W;"),
        ("pub", "pub use serde::Serialize as W;"),
        ("pub(crate)", "pub(crate) use serde::Serialize as W;"),
        ("pub(super)", "pub(super) use serde::Deserialize as R;"),
        (
            "pub(in path)",
            "pub(in crate::producers) use serde::Serialize as W;",
        ),
        ("indented pub", "    pub use serde::Serialize as W;"),
    ] {
        let found = imports_in(&code_only(source));
        assert_eq!(
            found.len(),
            1,
            "{label}: a visibility-qualified use was missed: {found:?}\n{source}"
        );
        assert!(
            !EVENT_ALLOWED_IMPORTS.contains(&found[0].as_str()),
            "{label}: the allowlist admitted a re-export: {found:?}"
        );
    }
}

#[test]
fn the_import_scanner_does_not_invent_imports() {
    // The other direction. `use` appears inside identifiers and prose, and a scanner that
    // fires on those makes the allowlist unmaintainable — which is how it ends up deleted.
    for (label, source) in [
        (
            "identifier containing use",
            "let misuse = 1;\nlet reuse_count = 2;\n",
        ),
        ("word in a string literal", "let s = \"please use this\";\n"),
        ("method named use_default", "thing.use_default();\n"),
    ] {
        let found = imports_in(&code_only(source));
        assert!(
            found.is_empty(),
            "{label}: the scanner invented an import: {found:?}\n{source}"
        );
    }

    // Prose is stripped before the scan, so a doc comment mentioning `use` is not one.
    let prose = "/// callers use crate::adaptive::X for this\nuse std::sync::Arc;\n";
    assert_eq!(
        imports_in(&code_only(prose)),
        vec!["use std::sync::Arc;".to_string()],
        "a doc comment was read as an import"
    );
}

#[test]
fn raw_string_lexing_does_not_over_consume() {
    // Positive controls for the lexer. Over-consuming is as bad as under-consuming: it would
    // swallow real code into a phantom string and hide whatever came next.
    let ordinary = "#[doc = \"plain\"]\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n";
    let attributes =
        attributes_attached_to(ordinary, ADAPTIVE_EVENT_DECLARATION).expect("declaration");
    assert_eq!(
        attributes.len(),
        2,
        "ordinary strings mis-lexed: {attributes:?}"
    );

    // `r` as the tail of an identifier is not a raw-string prefix.
    let identifier_r = "let counter = 1; let other = \"x\";\nuse crate::adaptive::Y;\n";
    assert!(
        code_only(identifier_r).contains("use crate::adaptive::Y;"),
        "an identifier ending in `r` was read as a raw string: {:?}",
        code_only(identifier_r)
    );

    // A lifetime is not a char literal.
    let lifetime = "impl<'de> Deserialize<'de> for AdaptiveEvent {}\n";
    assert!(
        handwritten_serialization_impl(lifetime, "AdaptiveEvent", &["Deserialize".to_string()])
            .is_some(),
        "a lifetime was mistaken for a char literal and lost the impl"
    );

    // Comments still get stripped when they are actually comments.
    assert!(!code_only("// crate::adaptive\nlet x = 1;\n").contains("crate::adaptive"));
}

#[test]
fn a_wrapped_attribute_is_not_lost_by_the_line_walk() {
    // The scanners walk lines, and a line that is not an attribute clears the pending set —
    // that rule is what stops one item's attributes leaking onto the next. A wrapped
    // attribute was destroyed by it: `#[derive(` was held as pending and then discarded by
    // its own continuation line, so the declaration reported only the attributes that
    // happened to fit on one line.
    //
    // Verified against the real module before the fix: a wrapped
    // `#[cfg_attr(…, derive(serde::Serialize))]` compiled, and
    // `lint_adaptive_event_derives_no_serialization` passed while
    // `attributes_attached_to` reported only `["#[derive(Debug, Clone)]"]`. Found by
    // CodeRabbit at `b35af10`.
    for (label, source) in [
        (
            "wrapped derive with a serializing member",
            "#[derive(\n    Debug,\n    Serialize,\n)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "wrapped derive above the innocent one",
            "#[derive(\n    Serialize,\n)]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "wrapped cfg_attr - the form proved against the real module",
            "#[cfg_attr(\n    feature = \"x\",\n    derive(serde::Serialize)\n)]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "wrapped derive of an alias",
            "use serde::Serialize as EventWire;\n#[derive(\n    Debug,\n    EventWire,\n)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "deeply wrapped, one member per line",
            "#[cfg_attr(\n    all(\n        feature = \"x\",\n        unix,\n    ),\n    derive(\n        Deserialize,\n    )\n)]\npub enum AdaptiveEvent {\n}\n",
        ),
    ] {
        let names = serialization_names(source);
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found for {label}:\n{source}"));
        assert!(
            attributes
                .iter()
                .any(|attribute| introduces_serialization(attribute, &names).is_some()),
            "{label} was admitted:\n{source}\nattributes seen: {attributes:?}"
        );
    }
}

#[test]
fn joining_wrapped_attributes_does_not_attach_them_to_the_wrong_item() {
    // Positive controls for the joiner. Collapsing newlines inside brackets must not
    // collapse the line structure that separates one item from the next, or every wrapped
    // attribute in a file would leak onto whatever declaration came after it.
    for (label, source) in [
        (
            "a wrapped serializing derive on a different item",
            "#[derive(\n    Serialize,\n)]\npub struct Unrelated {\n}\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "a wrapped innocent derive on the right item",
            "#[derive(\n    Debug,\n    Clone,\n)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "a wrapped cfg_attr deriving something harmless",
            "#[cfg_attr(\n    test,\n    derive(Default)\n)]\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "a wrapped doc comment containing a bracket and the word Serialize",
            "#[doc = \"see Serialize ]\"]\n#[derive(\n    Debug,\n)]\npub enum AdaptiveEvent {\n}\n",
        ),
    ] {
        let names = serialization_names(source);
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found for {label}:\n{source}"));
        let offenders: Vec<_> = attributes
            .iter()
            .filter_map(|attribute| introduces_serialization(attribute, &names))
            .collect();
        assert!(
            offenders.is_empty(),
            "{label} produced a false positive: {offenders:?}\n{source}\nattributes: {attributes:?}"
        );
    }

    // The joiner itself: newlines inside brackets become spaces, newlines outside do not.
    let joined = join_wrapped_attributes("#[derive(\n  Debug,\n)]\npub enum X {\n}\n");
    assert!(
        joined.starts_with("#[derive(   Debug, )]\n"),
        "attribute was not joined onto one line: {joined:?}"
    );
    assert!(
        joined.contains("]\npub enum X"),
        "the joiner collapsed the line structure between the attribute and its item: {joined:?}"
    );
}

#[test]
fn a_wrapped_server_gate_is_still_detected() {
    // `declaration_gates` had the identical defect, so it takes the identical fix. A gate
    // that wraps is still a gate.
    let source = "#[cfg(\n    feature = \"server\"\n)]\npub mod producers;\n";
    let gates = declaration_gates(source, PRODUCERS_DECLARATION)
        .unwrap_or_else(|| panic!("declaration not found in:\n{source}"));
    assert!(
        gates.iter().any(|gate| is_server_gate(gate)),
        "a wrapped server gate was missed: {gates:?}"
    );

    // And the false-positive direction: a wrapped gate belonging to the item above must
    // not carry onto this declaration.
    let neighbour = "#[cfg(\n    feature = \"server\"\n)]\npub mod bus;\npub mod producers;\n";
    let carried = declaration_gates(neighbour, PRODUCERS_DECLARATION)
        .unwrap_or_else(|| panic!("declaration not found in:\n{neighbour}"));
    assert!(
        !carried.iter().any(|gate| is_server_gate(gate)),
        "a wrapped gate leaked from the item above: {carried:?}"
    );
}

#[test]
fn an_aliased_serialization_trait_is_rejected() {
    // `use serde::Serialize as EventWire;` binds the derive macro *and* the trait under a
    // name containing neither "Serialize" nor "Deserialize", so every literal scan passes
    // while the type is fully serializable. Both forms were compiled against the real
    // module to confirm this before the test was written: `cargo build --lib` finished
    // clean with an aliased derive and an aliased hand-written impl in place, and
    // `lint_adaptive_event_derives_no_serialization` passed. Found by CodeRabbit at
    // `b35af10`.
    for (label, source) in [
        (
            "aliased derive",
            "use serde::Serialize as EventWire;\n#[derive(EventWire)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "aliased derive stacked behind the innocent one",
            "use serde::Serialize as EventWire;\n#[derive(EventWire)]\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "aliased Deserialize",
            "use serde::Deserialize as EventRead;\n#[derive(Debug)]\n#[derive(EventRead)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "aliased inside a grouped import",
            "use serde::{Serialize as EventWire, Deserializer};\n#[derive(EventWire)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "aliased via cfg_attr",
            "use serde::Serialize as EventWire;\n#[cfg_attr(feature = \"x\", derive(EventWire))]\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "aliased through a submodule path",
            "use serde::ser::Serialize as EventWire;\n#[derive(EventWire)]\npub enum AdaptiveEvent {\n}\n",
        ),
    ] {
        let names = serialization_names(source);
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found for {label}:\n{source}"));
        assert!(
            attributes
                .iter()
                .any(|attribute| introduces_serialization(attribute, &names).is_some()),
            "{label} was admitted:\n{source}\nresolved names: {names:?}"
        );
    }
}

#[test]
fn an_aliased_handwritten_impl_is_rejected() {
    for (label, source) in [
        (
            "aliased impl",
            "use serde::Serialize as EventWire;\nimpl EventWire for AdaptiveEvent {}\n",
        ),
        (
            "aliased Deserialize impl with a lifetime",
            "use serde::Deserialize as EventRead;\nimpl<'de> EventRead<'de> for AdaptiveEvent {}\n",
        ),
        (
            "canonical impl with a lifetime, which the previous literal scan also missed",
            "impl<'de> Deserialize<'de> for AdaptiveEvent {}\n",
        ),
        ("canonical impl", "impl Serialize for AdaptiveEvent {}\n"),
    ] {
        let names = serialization_names(source);
        assert!(
            handwritten_serialization_impl(source, "AdaptiveEvent", &names).is_some(),
            "{label} was admitted:\n{source}\nresolved names: {names:?}"
        );
    }
}

#[test]
fn unrelated_aliases_and_imports_do_not_false_positive() {
    // Positive controls. Two of these are the specific traps in exact-name matching:
    // `Serializer` has `Serialize` as a prefix, and `DeserializeOwned` has `Deserialize`
    // as one. Both are ordinary serde imports that must not register as the traits.
    for (label, source) in [
        (
            "Serializer is not Serialize",
            "use serde::Serializer;\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "DeserializeOwned is not Deserialize",
            "use serde::de::DeserializeOwned;\n#[derive(Debug, Clone)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "an alias of something else entirely",
            "use std::fmt::Display as EventWire;\n#[derive(EventWire)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "a serializing impl for a different type",
            "use serde::Serialize as EventWire;\nimpl EventWire for SomethingElse {}\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "a prefix-sharing type name",
            "impl Serialize for AdaptiveEventLog {}\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n",
        ),
        (
            "an unrelated impl for the right type",
            "impl Clone for AdaptiveEvent {}\n#[derive(Debug)]\npub enum AdaptiveEvent {\n}\n",
        ),
    ] {
        let names = serialization_names(source);
        let attributes = attributes_attached_to(source, ADAPTIVE_EVENT_DECLARATION)
            .unwrap_or_else(|| panic!("declaration not found for {label}:\n{source}"));
        let derived: Vec<_> = attributes
            .iter()
            .filter_map(|attribute| introduces_serialization(attribute, &names))
            .collect();
        assert!(
            derived.is_empty(),
            "{label} produced a false positive on a derive: {derived:?}\n{source}"
        );
        assert!(
            handwritten_serialization_impl(source, "AdaptiveEvent", &names).is_none(),
            "{label} produced a false positive on an impl\n{source}\nnames: {names:?}"
        );
    }
}

#[test]
fn attribute_extraction_survives_nesting_and_string_literals() {
    // `attributes_in` has to close on the matching bracket, not the first one. A
    // `cfg_attr` carrying a derive and a doc comment carrying a `]` are the two shapes
    // that break a naive scan, and both are reachable in ordinary code.
    let nested = attributes_in("#[cfg_attr(feature = \"server\", derive(Serialize))]");
    assert_eq!(nested.len(), 1);
    assert!(
        nested[0].ends_with("))]"),
        "attribute was truncated: {nested:?}"
    );

    let bracketed = attributes_in("#[doc = \"a ] bracket\"]\n#[derive(Debug)]");
    assert_eq!(
        bracketed.len(),
        2,
        "a string literal split an attribute: {bracketed:?}"
    );
    assert_eq!(bracketed[1], "#[derive(Debug)]");

    // Inner attributes belong to the enclosing module, not to the next item.
    let inner = attributes_in("#![deny(unsafe_code)]\n#[derive(Debug)]");
    assert_eq!(inner, vec!["#[derive(Debug)]".to_string()]);
}

#[test]
fn grouped_use_trees_do_not_hide_a_forbidden_module() {
    // A needle scan for `crate::adaptive` misses every one of these. This is the house
    // style in this repository - `src/main.rs` imports its modules exactly this way - so a
    // surface writing idiomatic code would have slipped past the boundary check entirely.
    // Found by CodeRabbit at `86e5bd2`.
    for source in [
        "use crate::{adaptive, bus};",
        "use crate::{bus, producers};",
        "use unified_hifi_control::{adaptive, producers};",
        "use unified_hifi_control::{\n    adapters, aggregator, api,\n    producers,\n};",
        // Nested and aliased forms.
        "use crate::{producers::ProducerAggregator, bus::SharedBus};",
        "use crate::{adaptive as contract};",
        "use crate::{bus::{SharedBus, BusEvent}, adaptive};",
    ] {
        let found = forbidden_module_references(source);
        assert!(
            !found.is_empty(),
            "a grouped import hid a forbidden module:\n{source}"
        );
    }
}

#[test]
fn grouped_use_trees_do_not_produce_false_positives() {
    // The scanner must not fire on modules that merely share a prefix, on unrelated
    // groups, or on a group belonging to a different root.
    for source in [
        "use crate::{bus, config};",
        "use crate::{adapters, aggregator};",
        "use std::{collections::BTreeMap, sync::Arc};",
        "use serde::{Deserialize, Serialize};",
        "use other_crate::{adaptive, producers};",
    ] {
        let found = forbidden_module_references(source);
        assert!(
            found.is_empty(),
            "the scanner invented a reference in:\n{source}\nfound: {found:?}"
        );
    }
}

#[test]
fn comment_stripper_keeps_strings_and_drops_prose() {
    // Prose discussing the boundary must not trip the dependency checks...
    let prose = "// this must not reference crate::adaptive\nlet x = 1;\n";
    assert!(!code_only(prose).contains("crate::adaptive"));

    let trailing = "let x = 1; // not crate::producers\n";
    assert!(!code_only(trailing).contains("crate::producers"));

    let block = "/* crate::adaptive /* nested */ still comment */\nlet x = 1;\n";
    assert!(!code_only(block).contains("crate::adaptive"));

    // ...but a forbidden path written as a string literal is still a reference.
    let literal = "let path = \"crate::producers\";\n";
    assert!(code_only(literal).contains("crate::producers"));

    // A `//` inside a string must not truncate the line.
    let url = "let u = \"https://example.com\"; let y = crate::producers::X;\n";
    assert!(code_only(url).contains("crate::producers"));
}
