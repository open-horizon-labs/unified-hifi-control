//! Machine checks for the HQPlayer evidence ledger (`docs/hqplayer-evidence-ledger.md`), issue #341.
//!
//! The ledger is prose. Prose in this repository has been confidently wrong twice — `.oh/hqplayer-spec.md`
//! says `SetMode` takes an enum VALUE while `docs/hqplayer-protocol-reference.md` says a list INDEX, and
//! ADR 003 records a provenance guard that accepted `verified-shape` as verified. So the parts of the
//! ledger a machine *can* check are checked here, and the parts it cannot are labelled with an evidence
//! class so a reader can weigh them.
//!
//! **What these tests do not do.** They constrain *form*, never truth. A well-formed row citing a real
//! test can still assert something false. That limit is stated in the ledger itself rather than left for
//! a reader to discover.
//!
//! **Non-vacuity is the point.** `tests/oneshot_leak_lint.rs` is a lint in this repository whose
//! `analyze_file` returns `vec![]` unconditionally, so it cannot fail. Every check below was observed
//! failing against a deliberately broken ledger before the ledger satisfied it; the observed failure text
//! is recorded in `.oh/hqplayer-evidence-ledger.md`.
//!
//! **Why `corpus.rs` is included by path.** Check `the_pending_confirmation_table_is_exactly_the_second_hand_corpus`
//! derives its expected set from fixture provenance, and `tests/mock_servers/hqplayer/corpus.rs` already
//! parses that header. A second parser for the same format would be a second thing to drift. `mod
//! mock_servers;` — what `tests/hqplayer_conformance.rs` does — would compile four unrelated mock servers
//! into this binary, so only `corpus.rs` is pulled in. Consequence, stated rather than hidden: that file's
//! five `#[cfg(test)]` provenance-parser tests also run in this target, so they execute twice per
//! `cargo test`. Two runs of five cheap tests is the price of one parser.

#[allow(dead_code)]
#[path = "mock_servers/hqplayer/corpus.rs"]
mod corpus;

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

// ===========================================================================================
// Closed vocabularies. Each is closed for the same reason the fixture `tier` vocabulary is closed:
// an open vocabulary lets a row invent a strength label nobody has to justify.
// ===========================================================================================

/// Evidence classes, strongest first. The order is the ranking the ledger promises.
const CLASSES: [&str; 6] = [
    // First-hand UHC observation against a real daemon, matching a recorded run in the ledger's
    // live-run registry.
    "E0-uhc-live",
    // Verified upstream against a live daemon; reaches UHC through a report, never read first-hand.
    "E1-upstream-verified",
    // Derived from official `hqp-control` sources.
    "E2-official-source",
    // Transcribed or derived; shape is right, the specific numbers are excerpt-local.
    "E3-derived",
    // Asserted in earlier UHC prose with no observation behind it.
    "E4-unverified",
    // Constructed to build a hazard shape. Never evidence.
    "E5-synthetic",
];

/// How the immediate source was obtained. Mirrors `Provenance::source_chain` in the corpus, whose
/// vocabulary CodeRabbit review 4816484338 closed for the same reason.
const CHAINS: [&str; 4] = [
    "direct",
    "read-via-report",
    "read-via-issue",
    "read-via-pr",
];

const STATUSES: [&str; 4] = ["settled", "open", "pending-live", "retired"];

const PLAYBACK: [&str; 4] = ["active", "idle", "unknown", "n/a"];

/// Classes whose claim is an observation of a running daemon, so `unknown` is not an acceptable
/// capture date or playback state for them.
const OBSERVED_CLASSES: [&str; 2] = ["E0-uhc-live", "E1-upstream-verified"];

/// The lowest number of claim rows that counts as a ledger rather than a stub. #341 names nine
/// evidence topics; a ledger that could satisfy `every_required_topic_maps_to_a_claim` with one row
/// per topic would be a table of contents.
const MIN_CLAIMS: usize = 25;

/// Test file a bare `test:<name>` proof refers to.
const DEFAULT_PROOF_FILE: &str = "tests/hqplayer_conformance.rs";

// ===========================================================================================
// Parsing
// ===========================================================================================

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn ledger_path() -> PathBuf {
    repo_root().join("docs/hqplayer-evidence-ledger.md")
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| {
        panic!(
            "cannot read {}: {e}. Issue #341's deliverable is this file; without it the ledger's \
             claims have nowhere to live and nothing checks them",
            path.display()
        )
    })
}

fn ledger() -> String {
    read(&ledger_path())
}

/// One claim row of the ledger's index table.
#[derive(Debug, Clone)]
struct Claim {
    id: String,
    /// 1-based line number in the ledger, so a failure names where to look.
    line: usize,
    claim: String,
    class: String,
    /// `source · chain · daemon/version · date · playback`, split and trimmed.
    provenance: Vec<String>,
    proof: String,
    status: String,
    owner: String,
}

impl Claim {
    fn source(&self) -> &str {
        self.provenance.first().map_or("", String::as_str)
    }
    fn chain(&self) -> &str {
        self.provenance.get(1).map_or("", String::as_str)
    }
    fn daemon(&self) -> &str {
        self.provenance.get(2).map_or("", String::as_str)
    }
    fn date(&self) -> &str {
        self.provenance.get(3).map_or("", String::as_str)
    }
    fn playback(&self) -> &str {
        self.provenance.get(4).map_or("", String::as_str)
    }
}

/// Split one markdown table row into trimmed cells, dropping the empty leading and trailing fields
/// that the surrounding pipes produce.
fn cells(line: &str) -> Vec<String> {
    let trimmed = line.trim();
    let inner = trimmed
        .strip_prefix('|')
        .unwrap_or(trimmed)
        .strip_suffix('|')
        .unwrap_or(trimmed);
    inner.split('|').map(|c| c.trim().to_string()).collect()
}

/// Every claim row in the ledger. A claim row is any table row whose first cell is a `HQP-C-` ID, so
/// the parser does not depend on the row's position in the document or on which section holds it.
fn claims() -> Vec<Claim> {
    let text = ledger();
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        if !line.trim_start().starts_with("| HQP-C-") {
            continue;
        }
        let c = cells(line);
        // A short row is reported by `every_claim_row_has_every_required_field`, not swallowed here.
        let get = |n: usize| c.get(n).cloned().unwrap_or_default();
        out.push(Claim {
            id: get(0),
            line: i + 1,
            claim: get(1),
            class: get(2),
            provenance: get(3)
                .split(" · ")
                .map(|p| p.trim().to_string())
                .collect(),
            proof: get(4),
            status: get(5),
            owner: get(6),
        });
    }
    out
}

/// Rows of a `|`-delimited table under the heading whose text contains `heading_marker`, excluding the
/// header and separator rows.
fn table_rows_under(heading_marker: &str) -> Vec<Vec<String>> {
    let text = ledger();
    let mut rows = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if line.starts_with('#') {
            // A new heading always ends the previous section, so a table cannot leak across one.
            inside = line.contains(heading_marker);
            continue;
        }
        if !inside {
            continue;
        }
        let t = line.trim();
        if !t.starts_with('|') {
            continue;
        }
        let c = cells(t);
        let is_separator = c.iter().all(|x| {
            !x.is_empty() && x.chars().all(|ch| ch == '-' || ch == ':' || ch == ' ')
        });
        if is_separator {
            continue;
        }
        rows.push(c);
    }
    // Drop the header row of each table in the section.
    rows.into_iter()
        .filter(|r| {
            let head = r.first().map(String::as_str).unwrap_or_default();
            !head.eq_ignore_ascii_case("id")
                && !head.eq_ignore_ascii_case("fixture")
                && !head.eq_ignore_ascii_case("run")
                && !head.eq_ignore_ascii_case("topic")
        })
        .collect()
}

/// Names of `#[test]`/`#[tokio::test]` functions in a test file, and whether each carries `#[ignore]`.
///
/// `#[ignore]` matters: a name-existence check would accept an ignored test as proof of a claim, and
/// an ignored test proves nothing. The suite carries none today, which is a fact about today.
fn test_functions(path: &Path) -> HashMap<String, bool> {
    let text = read(path);
    let lines: Vec<&str> = text.lines().collect();
    let mut out = HashMap::new();
    for (i, line) in lines.iter().enumerate() {
        let t = line.trim_start();
        let name = t
            .strip_prefix("async fn ")
            .or_else(|| t.strip_prefix("fn "))
            .and_then(|rest| rest.split('(').next())
            .map(str::trim);
        let Some(name) = name else { continue };
        if name.is_empty() || !name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            continue;
        }
        // Walk back over the attribute block directly above the signature.
        let mut is_test = false;
        let mut ignored = false;
        for prev in lines[..i].iter().rev() {
            let p = prev.trim();
            if p.starts_with("#[") {
                if p.contains("test") {
                    is_test = true;
                }
                if p.contains("ignore") {
                    ignored = true;
                }
                continue;
            }
            if p.starts_with("///") || p.starts_with("//") || p.is_empty() {
                continue;
            }
            break;
        }
        if is_test {
            out.insert(name.to_string(), ignored);
        }
    }
    out
}

/// Fixtures whose provenance records that the cited upstream file was not read directly.
///
/// Derived from the corpus rather than curated, so the ledger's pending-confirmation table cannot
/// drift from the evidence base it describes.
fn second_hand_fixtures() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for profile in corpus::profiles() {
        for fixture in corpus::all_in(&profile) {
            if fixture
                .provenance
                .source_chain
                .contains("read-via-report")
            {
                out.insert(format!("{profile}/{}", fixture.name));
            }
        }
    }
    out
}

// ===========================================================================================
// Structural checks
// ===========================================================================================

#[test]
fn the_ledger_exists_and_declares_its_schema() {
    let text = ledger();
    assert!(
        text.contains("<!-- uhc-hqp-ledger/v1 -->"),
        "the ledger must declare a versioned schema marker so a later format change is visible to \
         this lint rather than silently accepted"
    );
    let n = claims().len();
    assert!(
        n >= MIN_CLAIMS,
        "the ledger holds {n} claim rows; fewer than {MIN_CLAIMS} means the index is a stub and the \
         topic checks below could pass on a table of contents"
    );
}

#[test]
fn every_claim_row_has_every_required_field() {
    let mut bad = Vec::new();
    for c in claims() {
        let mut missing = Vec::new();
        for (label, value) in [
            ("claim", &c.claim),
            ("class", &c.class),
            ("proof", &c.proof),
            ("status", &c.status),
            ("owner", &c.owner),
        ] {
            if value.is_empty() {
                missing.push(label);
            }
        }
        if c.provenance.len() != 5 {
            missing.push("provenance must be `source · chain · daemon/version · date · playback`");
        } else if let Some(empty) = c.provenance.iter().position(String::is_empty) {
            missing.push(match empty {
                0 => "provenance.source",
                1 => "provenance.chain",
                2 => "provenance.daemon/version",
                3 => "provenance.date",
                _ => "provenance.playback",
            });
        }
        if !missing.is_empty() {
            bad.push(format!("{} (line {}): {:?}", c.id, c.line, missing));
        }
    }
    assert!(
        bad.is_empty(),
        "#341 AC6 requires every empirical claim to name source, edition/version, capture date and \
         playback state. These rows do not: {bad:#?}"
    );
}

#[test]
fn claim_ids_are_unique_and_contiguous() {
    let ids: Vec<String> = claims().into_iter().map(|c| c.id).collect();
    let mut seen = BTreeSet::new();
    let mut dupes = Vec::new();
    let mut malformed = Vec::new();
    let mut numbers = Vec::new();
    for id in &ids {
        if !seen.insert(id.clone()) {
            dupes.push(id.clone());
        }
        match id.strip_prefix("HQP-C-").and_then(|n| n.parse::<usize>().ok()) {
            Some(n) if id.len() == "HQP-C-000".len() => numbers.push(n),
            _ => malformed.push(id.clone()),
        }
    }
    assert!(malformed.is_empty(), "malformed claim IDs: {malformed:?}");
    assert!(
        dupes.is_empty(),
        "duplicate claim IDs, which would make a cross-issue citation ambiguous: {dupes:?}"
    );
    numbers.sort_unstable();
    let expected: Vec<usize> = (1..=numbers.len()).collect();
    assert_eq!(
        numbers, expected,
        "claim IDs must run contiguously from HQP-C-001. A gap means a row was deleted, and a \
         deleted row is how a retired claim disappears without leaving the record that it was \
         retired — retirement is a status, never a deletion"
    );
}

#[test]
fn every_class_and_status_is_in_the_closed_vocabulary() {
    let mut bad = Vec::new();
    for c in claims() {
        if !CLASSES.contains(&c.class.as_str()) {
            bad.push(format!("{} class {:?}", c.id, c.class));
        }
        if !STATUSES.contains(&c.status.as_str()) {
            bad.push(format!("{} status {:?}", c.id, c.status));
        }
    }
    assert!(
        bad.is_empty(),
        "an open vocabulary lets a row invent a strength label nobody has to justify. Allowed \
         classes {CLASSES:?}, statuses {STATUSES:?}. Offending: {bad:#?}"
    );
}

#[test]
fn every_source_chain_and_playback_state_is_in_the_closed_vocabulary() {
    let mut bad = Vec::new();
    for c in claims() {
        if !CHAINS.contains(&c.chain()) {
            bad.push(format!("{} chain {:?}", c.id, c.chain()));
        }
        if !PLAYBACK.contains(&c.playback()) {
            bad.push(format!("{} playback {:?}", c.id, c.playback()));
        }
    }
    assert!(
        bad.is_empty(),
        "`source_chain` is the field that distinguishes an observation from a report about one — the \
         distinction that produced three factual errors in #322 when it was left to prose. Allowed \
         chains {CHAINS:?}, playback {PLAYBACK:?}. Offending: {bad:#?}"
    );
}

#[test]
fn an_observed_claim_names_a_real_capture_date_and_playback_state() {
    let iso = |d: &str| {
        d.len() == 10
            && d.as_bytes()[4] == b'-'
            && d.as_bytes()[7] == b'-'
            && d.chars().filter(char::is_ascii_digit).count() == 8
    };
    let mut bad = Vec::new();
    for c in claims() {
        if !OBSERVED_CLASSES.contains(&c.class.as_str()) {
            continue;
        }
        if !iso(c.date()) {
            bad.push(format!("{} date {:?}", c.id, c.date()));
        }
        if c.playback() == "unknown" || c.playback() == "n/a" {
            bad.push(format!("{} playback {:?}", c.id, c.playback()));
        }
    }
    assert!(
        bad.is_empty(),
        "a claim in class {OBSERVED_CLASSES:?} says a running daemon was observed, so `unknown` is \
         not an available answer for when, or for whether audio was playing. A behaviour verified \
         idle is not thereby verified under load. Offending: {bad:#?}"
    );
}

// ===========================================================================================
// Proof checks — the ledger's link to executable truth
// ===========================================================================================

#[test]
fn every_cited_test_exists_and_is_not_ignored() {
    let mut cache: HashMap<String, HashMap<String, bool>> = HashMap::new();
    let mut missing = Vec::new();
    let mut ignored = Vec::new();
    for c in claims() {
        for proof in c.proof.split(" · ") {
            let Some(spec) = proof.trim().trim_matches('`').strip_prefix("test:") else {
                continue;
            };
            let (file, name) = match spec.split_once("::") {
                Some((f, n)) => (f.to_string(), n.to_string()),
                None => (DEFAULT_PROOF_FILE.to_string(), spec.to_string()),
            };
            let fns = cache
                .entry(file.clone())
                .or_insert_with(|| test_functions(&repo_root().join(&file)));
            match fns.get(&name) {
                None => missing.push(format!("{} cites {file}::{name}", c.id)),
                Some(true) => ignored.push(format!("{} cites {file}::{name}", c.id)),
                Some(false) => {}
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these claims cite a test that does not exist. A citation that names nothing is the failure \
         this ledger exists to prevent, one layer out: {missing:#?}"
    );
    assert!(
        ignored.is_empty(),
        "these claims cite an `#[ignore]`d test. An ignored test exists and proves nothing, so a \
         name-existence check alone would accept a hollow proof: {ignored:#?}"
    );
}

#[test]
fn every_cited_fixture_exists() {
    let mut missing = Vec::new();
    for c in claims() {
        for proof in c.proof.split(" · ") {
            let Some(rel) = proof.trim().trim_matches('`').strip_prefix("fixture:") else {
                continue;
            };
            if !repo_root().join(rel).exists() {
                missing.push(format!("{} cites {rel}", c.id));
            }
        }
    }
    assert!(
        missing.is_empty(),
        "these claims cite a corpus fixture that is not on disk: {missing:#?}"
    );
}

#[test]
fn every_proof_uses_a_known_form() {
    let mut bad = Vec::new();
    for c in claims() {
        for proof in c.proof.split(" · ") {
            let p = proof.trim().trim_matches('`');
            let known = p.starts_with("test:")
                || p.starts_with("fixture:")
                || p.starts_with("#332:")
                || p.starts_with("none:");
            if !known || p.split_once(':').map(|(_, v)| v.trim().is_empty()) != Some(false) {
                bad.push(format!("{} proof {:?}", c.id, p));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a proof must be `test:<name>`, `test:<file>::<name>`, `fixture:<path>`, `#332:<row>` — the \
         live-qualification form #341's AC explicitly permits — or `none:<what would settle it>`, \
         each with a non-empty value. Offending: {bad:#?}"
    );
}

#[test]
fn a_claim_proved_only_by_a_future_live_row_is_not_settled() {
    let mut bad = Vec::new();
    for c in claims() {
        let proofs: Vec<&str> = c.proof.split(" · ").map(|p| p.trim().trim_matches('`')).collect();
        let executable = proofs
            .iter()
            .any(|p| p.starts_with("test:") || p.starts_with("fixture:"));
        let future_only = !executable
            && proofs
                .iter()
                .any(|p| p.starts_with("#332:") || p.starts_with("none:"));
        if future_only && c.status == "settled" {
            bad.push(format!("{} status {:?} proof {:?}", c.id, c.status, c.proof));
        }
    }
    assert!(
        bad.is_empty(),
        "a claim whose only proof is a live-qualification row that has not run, or an explicit \
         `none`, cannot be `settled` — that is the shape of exactly the over-claiming #332 exists to \
         prevent: {bad:#?}"
    );
}

#[test]
fn every_unsettled_claim_names_an_owner_and_what_would_settle_it() {
    let text = ledger();
    let mut bad = Vec::new();
    for c in claims() {
        if c.status == "settled" || c.status == "retired" {
            continue;
        }
        if !c.owner.contains('#') || !c.owner.chars().any(char::is_numeric) {
            bad.push(format!("{} owner {:?} names no issue", c.id, c.owner));
        }
        // The settle condition lives in the claim's prose anchor, because one table cell cannot hold
        // it honestly. The anchor is the heading that carries the ID.
        let anchor = text
            .split("\n#")
            .find(|section| section.lines().next().is_some_and(|h| h.contains(&c.id)));
        match anchor {
            None => bad.push(format!("{} has no prose anchor section", c.id)),
            Some(section) if !section.contains("What would settle it") => {
                bad.push(format!("{} anchor has no `What would settle it` line", c.id))
            }
            Some(_) => {}
        }
    }
    assert!(
        bad.is_empty(),
        "#341 requires unresolved contradictions to stay explicit. An `open` or `pending-live` row \
         without an owner or a settle condition is not explicit, it is a shrug: {bad:#?}"
    );
}

// ===========================================================================================
// Derived checks — the ledger against the evidence base it describes
// ===========================================================================================

#[test]
fn first_hand_claims_match_a_recorded_live_run() {
    let registry: Vec<Vec<String>> = table_rows_under("Live runs");
    assert!(
        !registry.is_empty(),
        "the ledger must carry a `Live runs` registry table; without it `E0-uhc-live` is a label \
         anybody can apply"
    );
    let mut bad = Vec::new();
    for c in claims() {
        if c.class != "E0-uhc-live" {
            continue;
        }
        let matched = registry.iter().any(|row| {
            let joined = row.join(" · ");
            joined.contains(c.daemon()) && joined.contains(c.date())
        });
        if !matched {
            bad.push(format!(
                "{} claims first-hand evidence on {:?} at {:?}, which no registry row records",
                c.id,
                c.daemon(),
                c.date()
            ));
        }
    }
    assert!(
        bad.is_empty(),
        "`E0-uhc-live` means UHC observed it on a daemon UHC ran. Every such row must trace to a \
         recorded run, so adding a rig is an explicit registry edit and never a loosened check: \
         {bad:#?}"
    );
}

#[test]
fn the_pending_confirmation_table_is_exactly_the_second_hand_corpus() {
    let expected = second_hand_fixtures();
    assert!(
        !expected.is_empty(),
        "no fixture records `source_chain: read-via-report`; this check would then be vacuous. If \
         the corpus really has none, delete the check rather than let it pass on an empty set"
    );
    let listed: BTreeSet<String> = table_rows_under("Pending first-hand confirmation")
        .into_iter()
        .filter_map(|row| row.first().cloned())
        .map(|cell| cell.trim().trim_matches('`').to_string())
        .filter(|cell| cell.contains('/'))
        .collect();
    let missing: Vec<&String> = expected.difference(&listed).collect();
    let extra: Vec<&String> = listed.difference(&expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "the pending-confirmation table must equal the set of fixtures whose provenance records \
         `source_chain: read-via-report`, derived from the corpus rather than curated by hand.\n\
         missing from the ledger: {missing:#?}\nlisted but no longer second-hand: {extra:#?}"
    );
}

#[test]
fn every_required_evidence_topic_maps_to_a_claim() {
    // #341's acceptance criteria, each mapped to the claim IDs that must carry it. The map is the
    // check: a topic whose claim ID vanishes fails here, and a keyword scan over prose would not.
    let required: [(&str, &[&str]); 9] = [
        ("SetMode value-vs-index contradiction", &["HQP-C-001"]),
        (
            "three numeric domains",
            &["HQP-C-002", "HQP-C-003", "HQP-C-004"],
        ),
        ("SetRate semantics", &["HQP-C-010"]),
        ("active_mode contradiction", &["HQP-C-016"]),
        (
            "HTTP / profile / session-auth negative findings",
            &["HQP-C-020", "HQP-C-021", "HQP-C-022"],
        ),
        ("apply-then-drop ambiguity", &["HQP-C-023"]),
        ("push status cadence", &["HQP-C-025"]),
        ("LibraryPicture binary interlude", &["HQP-C-026"]),
        ("licensing and provenance", &["HQP-C-027"]),
    ];
    let by_id: HashMap<String, Claim> = claims().into_iter().map(|c| (c.id.clone(), c)).collect();
    let mut bad = Vec::new();
    for (topic, ids) in required {
        for id in ids {
            match by_id.get(*id) {
                None => bad.push(format!("{topic}: {id} is not in the ledger")),
                Some(c) if c.claim.is_empty() => bad.push(format!("{topic}: {id} has no claim")),
                Some(_) => {}
            }
        }
    }
    // The two topics #341 requires to stay *unresolved* must not be quietly settled.
    for (topic, id) in [
        ("active_mode contradiction", "HQP-C-016"),
        ("apply-then-drop ambiguity", "HQP-C-023"),
    ] {
        if let Some(c) = by_id.get(id) {
            assert_ne!(
                c.status, "settled",
                "{topic} ({id}) is recorded `settled`. #341 requires it tracked as an unresolved \
                 versioned question; settling it here is choosing a global winner on one rig's \
                 evidence"
            );
        }
    }
    assert!(
        bad.is_empty(),
        "these #341 evidence topics have no claim row: {bad:#?}"
    );
}

// ===========================================================================================
// Retirement checks — the superseded documents
// ===========================================================================================

#[test]
fn the_superseded_documents_point_at_the_ledger() {
    let mut bad = Vec::new();
    for rel in [
        "docs/hqplayer-protocol-reference.md",
        ".oh/hqplayer-spec.md",
    ] {
        let text = read(&repo_root().join(rel));
        if !text.contains("hqplayer-evidence-ledger.md") {
            bad.push(format!("{rel} does not point at the ledger"));
        }
    }
    assert!(
        bad.is_empty(),
        "a reader arriving at a superseded document from an old link must be sent to the ledger. \
         That is the whole mechanism by which retiring prose beats deleting it: {bad:#?}"
    );
}

#[test]
fn the_retired_set_mode_value_claim_is_struck_where_it_still_appears() {
    let spec = read(&repo_root().join(".oh/hqplayer-spec.md"));
    // The historical claim stays — the repository's convention is to strike a wrong claim in place
    // rather than delete it, so a reader sees the correction and not a clean file. What must not
    // remain is the claim standing unmarked.
    for needle in ["| Mode | VALUE | VALUE |", "resolves to VALUE"] {
        if let Some(pos) = spec.find(needle) {
            let line_start = spec[..pos].rfind('\n').map_or(0, |n| n + 1);
            let line_end = spec[pos..].find('\n').map_or(spec.len(), |n| pos + n);
            let line = &spec[line_start..line_end];
            assert!(
                line.contains("~~"),
                "`.oh/hqplayer-spec.md` still asserts {needle:?} unstruck, on line {line:?}. The \
                 client sends the list index (src/adapters/hqplayer.rs `set_mode`) and the live \
                 6.0.2 run confirmed it; leaving this readable as current guidance is the \
                 contradiction #341 exists to retire"
            );
        }
    }
}

#[test]
fn the_reference_document_no_longer_settles_the_active_mode_question_by_fiat() {
    let text = read(&repo_root().join("docs/hqplayer-protocol-reference.md"));
    assert!(
        !text.contains("Always use State's numeric"),
        "`docs/hqplayer-protocol-reference.md` still instructs the reader to always use State's \
         numeric `active_mode`. `Status.active_mode` echoing the configured mode is measured; \
         `State.active_mode` under `[source]` is unmeasured — the conformance suite deliberately \
         refuses to settle it (`the_fake_does_not_settle_the_independent_state_and_status_active_mode_semantics`), \
         so the document must not"
    );
}

#[test]
fn no_verbatim_upstream_source_excerpt_remains_in_the_reference_document() {
    let text = read(&repo_root().join("docs/hqplayer-protocol-reference.md"));
    let fences = text.matches("```cpp").count();
    assert_eq!(
        fences, 0,
        "{fences} verbatim C++ excerpt(s) from Signalyst's `hqp-control` sources remain in \
         docs/hqplayer-protocol-reference.md. This repository records no license for those sources, \
         and the interoperability fact each excerpt carries survives a paraphrase with the same \
         file/line citation. #348 owns the standing guardrail"
    );
}
