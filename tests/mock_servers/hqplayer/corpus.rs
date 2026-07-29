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

/// Provenance recorded in a fixture's leading XML comment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Provenance {
    pub source: String,
    pub daemon: String,
    pub status: String,
    pub date: String,
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

    Provenance {
        source: field("source"),
        daemon: field("daemon"),
        status: field("status"),
        date: field("date"),
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

/// Load one fixture by version profile and file stem, e.g. `("hqpd-6.0.4-opal", "status_playing")`.
pub fn load(profile: &str, stem: &str) -> Fixture {
    let path = fixtures_root().join(profile).join(format!("{stem}.xml"));
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
