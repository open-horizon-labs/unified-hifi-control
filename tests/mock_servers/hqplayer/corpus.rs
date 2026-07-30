//! Versioned HQPlayer document corpus — the *document* layer of the conformance boundary.
//!
//! The corpus says **what** the daemon replies. It knows nothing about sockets, chunking or
//! timing ([`super::wire`]'s job) and nothing about state transitions.
//!
//! Every fixture carries a provenance header as a leading XML comment recording its source, the
//! daemon it was observed against, and whether it is `verified` or `derived`/`UNVERIFIED`.
//! [`Provenance`] parses that header so tests can assert on it and so a reader can always tell a
//! live-observed fact from a transcription — the corpus, not the Rust implementation, is meant to
//! be the protocol truth, and a fixture without provenance would quietly re-create the problem
//! issue #322 exists to end.

use std::fs;
use std::path::{Path, PathBuf};

/// Live-verified version profile, observed on hqplayerd 6.0.4 (Opal).
pub const VERIFIED_PROFILE: &str = "hqpd-6.0.4-opal";

/// Source-derived profile. Never authoritative — exists only to vary list ordering for
/// version-regression coverage.
pub const LEGACY_PROFILE: &str = "hqpd-5.x-legacy";

/// Wholly **synthetic** profile. Not evidence, not derived from any capture, and never promotable.
///
/// It exists to construct one hazard shape — a stale index from one chain landing in range in the
/// other and naming a different filter — which the Opal-derived corpus must not be padded to produce.
/// Every filter name in it is fictional so a row cannot be copied into the evidence corpus by mistake.
pub const SYNTHETIC_HAZARD_PROFILE: &str = "synthetic-chain-hazard";

/// Provenance recorded in a fixture's leading XML comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub source: String,
    pub daemon: String,
    pub status: String,
    pub date: String,
    /// How the `source` was obtained: `read-via-report` when the cited upstream file was not read
    /// directly and a salvage report is the immediate source, `direct` when it was read here.
    ///
    /// Exists because three errors in this amendment came from treating a report's citation as if it
    /// had been read. Recording the chain lets a reader weigh a claim correctly and tells #341 which
    /// claims still need first-hand or live confirmation. Defaults to `unrecorded` for fixtures that
    /// cite no upstream file.
    pub source_chain: String,
    /// Whether any tier could ever promote this fixture's claims to UHC-qualified evidence.
    ///
    /// `tier-1` means a read-only live run can settle it. `tier-2-only` means settling it requires a
    /// mutating run with a human present, so the read-only merge gate never can. `never-promotable`
    /// marks a synthetic fixture, which is a constructed shape rather than an observation and must not
    /// be promoted by anything. Absent in older fixtures, which default to `unspecified`.
    pub tier: String,
    /// Playback state at capture: `active`, `idle`, or `unknown`.
    ///
    /// Required by the HQPTuner amendment, because every "live, verified" claim in the upstream
    /// evidence base carries the caveat that its probes ran with the engine **stopped**, while UHC's
    /// users are playing. A behaviour verified idle is not thereby verified under load, and a corpus
    /// that does not record which it was cannot tell the difference.
    ///
    /// `unknown` is the honest answer for a document derived from a protocol reference rather than
    /// captured from a session, and most of this corpus is derived. That most fixtures say `unknown`
    /// is itself the finding: the corpus was built without recording this, which is why the
    /// acceptance criterion exists.
    pub playback: String,
    pub notes: String,
}

impl Provenance {
    /// True when the fixture claims to have been observed on a live daemon.
    pub fn is_verified(&self) -> bool {
        self.status.starts_with("verified")
    }
}

/// One corpus document plus the provenance of the file it came from.
#[derive(Debug, Clone)]
pub struct Fixture {
    pub name: String,
    pub provenance: Provenance,
    /// The XML document, with the provenance comment and surrounding whitespace removed.
    pub document: String,
}

fn fixtures_root() -> PathBuf {
    // Integration tests run with the crate root as the working directory — the same assumption
    // `tests/api_contract.rs` already makes for `tests/fixtures/api_routes.txt`.
    PathBuf::from("tests/fixtures/hqplayer")
}

fn parse_provenance(raw: &str, path: &Path) -> Provenance {
    let start = raw.find("<!--").unwrap_or_else(|| {
        panic!(
            "fixture {} has no provenance comment; every corpus document must carry one",
            path.display()
        )
    });
    let end = start
        + raw[start..].find("-->").unwrap_or_else(|| {
            panic!(
                "fixture {} has an unterminated provenance comment",
                path.display()
            )
        });
    let body = &raw[start + 4..end];

    let field = |key: &str| -> String {
        body.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(&format!("{key}:")))
            .map(|v| v.trim().to_string())
            .unwrap_or_else(|| {
                panic!(
                    "fixture {} provenance is missing the `{key}` field",
                    path.display()
                )
            })
    };

    let optional = |key: &str| -> Option<String> {
        body.lines()
            .map(str::trim)
            .find_map(|line| line.strip_prefix(&format!("{key}:")))
            .map(|v| v.trim().to_string())
    };

    Provenance {
        source: field("source"),
        daemon: field("daemon"),
        status: field("status"),
        date: field("date"),
        playback: field("playback"),
        // Optional, unlike the rest: most fixtures predate the distinction and `unspecified` is the
        // honest reading for them. A synthetic fixture must say `never-promotable` explicitly, and
        // `a_synthetic_profile_is_never_evidence` enforces that.
        tier: optional("tier").unwrap_or_else(|| "unspecified".to_string()),
        source_chain: optional("source_chain").unwrap_or_else(|| "unrecorded".to_string()),
        notes: field("notes"),
    }
}

fn strip_provenance(raw: &str) -> String {
    match (raw.find("<!--"), raw.find("-->")) {
        (Some(s), Some(e)) if e > s => {
            let mut out = String::with_capacity(raw.len());
            out.push_str(&raw[..s]);
            out.push_str(&raw[e + 3..]);
            out.trim().to_string()
        }
        _ => raw.trim().to_string(),
    }
}

/// Extensions the corpus recognises. `.html` covers the persistent HTTP lane's response family,
/// whose documents are pages rather than XML.
const EXTENSIONS: [&str; 2] = ["xml", "html"];

/// Load one fixture by version profile and file stem, e.g. `("hqpd-6.0.4-opal", "status_playing")`.
pub fn load(profile: &str, stem: &str) -> Fixture {
    let dir = fixtures_root().join(profile);
    let path = EXTENSIONS
        .iter()
        .map(|ext| dir.join(format!("{stem}.{ext}")))
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "no fixture named {stem} (.xml or .html) in {}",
                dir.display()
            )
        });
    let raw = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("failed to read fixture {}: {e}", path.display()));
    Fixture {
        name: stem.to_string(),
        provenance: parse_provenance(&raw, &path),
        document: strip_provenance(&raw),
    }
}

/// Load just the document body of a fixture.
pub fn document(profile: &str, stem: &str) -> String {
    load(profile, stem).document
}

/// Every fixture in a version profile, sorted by file stem. Used to assert corpus-wide invariants.
pub fn all_in(profile: &str) -> Vec<Fixture> {
    let dir = fixtures_root().join(profile);
    let mut stems: Vec<String> = fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to list {}: {e}", dir.display()))
        .map(|entry| entry.expect("readable dir entry").path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| EXTENSIONS.contains(&ext))
        })
        .map(|p| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .expect("fixture stem is valid UTF-8")
                .to_string()
        })
        .collect();
    stems.sort();
    stems.iter().map(|s| load(profile, s)).collect()
}

/// Every version profile directory in the corpus.
pub fn profiles() -> Vec<String> {
    let mut out: Vec<String> = fs::read_dir(fixtures_root())
        .expect("tests/fixtures/hqplayer exists")
        .map(|e| e.expect("readable dir entry").path())
        .filter(|p| p.is_dir())
        .map(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .expect("profile dir name is valid UTF-8")
                .to_string()
        })
        .collect();
    out.sort();
    out
}

/// One entry of an enumeration document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumEntry {
    /// Position in the list. This is the domain `State` and `Set*` speak.
    pub index: u32,
    pub name: String,
    /// The numeric enumeration ID. This is the domain `hqplayerd.xml` stores.
    /// `None` for `RatesItem`, which carries no `value` attribute.
    pub enum_id: Option<i32>,
    /// `RatesItem` only: the rate in Hz.
    pub rate: Option<u32>,
    /// `FiltersItem` flags bitfield; bit 0 is apodizing. ADR 003 requires this be diffed, so it must
    /// survive the trip through the fake rather than being dropped on re-emission.
    pub arg: Option<u32>,
    /// The engine's own description string, when the document carries one. Presence is the claim.
    pub description: Option<String>,
}

fn attr(fragment: &str, key: &str) -> Option<String> {
    let pat = format!(" {key}=\"");
    let start = fragment.find(&pat)? + pat.len();
    let rest = &fragment[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Parse the `<…Item …/>` children of an enumeration document.
///
/// Deliberately a hand-rolled scan rather than a call into the adapter: the corpus must never
/// depend on the production parser it exists to check.
pub fn enum_entries(document: &str, item_tag: &str) -> Vec<EnumEntry> {
    let pat = format!("<{item_tag}");
    document
        .split(&pat)
        .skip(1)
        .filter_map(|part| {
            let end = part.find("/>")?;
            let item = &part[..end];
            Some(EnumEntry {
                index: attr(item, "index")?.parse().ok()?,
                name: attr(item, "name").unwrap_or_default(),
                enum_id: attr(item, "value").and_then(|v| v.parse().ok()),
                rate: attr(item, "rate").and_then(|v| v.parse().ok()),
                arg: attr(item, "arg").and_then(|v| v.parse().ok()),
                description: attr(item, "description"),
            })
        })
        .collect()
}

/// Resolve a semantic name to its **list index** from an observed enumeration document.
///
/// The live-wire lane: `State` reports and `Set*` accepts list indices.
///
/// Kept deliberately separate from [`enum_id_of`], with no shared body, so the two numeric domains
/// cannot be quietly unified by a later refactor (issue #322 AC6).
pub fn index_of(document: &str, item_tag: &str, name: &str) -> Option<u32> {
    enum_entries(document, item_tag)
        .into_iter()
        .find(|e| e.name == name)
        .map(|e| e.index)
}

/// Resolve a semantic name to its **enum ID** from an observed enumeration document.
///
/// The persistent-configuration lane: `hqplayerd.xml` stores enum IDs.
pub fn enum_id_of(document: &str, item_tag: &str, name: &str) -> Option<i32> {
    enum_entries(document, item_tag)
        .into_iter()
        .find(|e| e.name == name)
        .and_then(|e| e.enum_id)
}

/// Read one attribute off an element of a persistent-configuration document.
pub fn config_attr(document: &str, element: &str, key: &str) -> Option<String> {
    let start = document.find(&format!("<{element}"))?;
    let rest = &document[start..];
    let end = rest.find('>')?;
    attr(&rest[..end], key)
}

/// Element name of a protocol line such as `<?xml version="1.0"?><SetFilter value="6"/>`.
///
/// Lives here, in the document layer, because both [`super::wire`] and [`super::model`] need it and
/// two copies would drift. The layer separation this boundary rests on is about *responsibility* —
/// bytes versus documents versus state — not about refusing to share a five-line tokeniser.
pub fn element_name(line: &str) -> Option<String> {
    let mut rest = line.trim();
    if rest.starts_with("<?") {
        rest = rest[rest.find("?>")? + 2..].trim_start();
    }
    let rest = rest.strip_prefix('<')?;
    let end = rest
        .find(|c: char| c.is_whitespace() || c == '/' || c == '>')
        .unwrap_or(rest.len());
    Some(rest[..end].to_string())
}
