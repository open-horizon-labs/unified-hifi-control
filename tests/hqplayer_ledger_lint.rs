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
const CLASSES: [&str; 7] = [
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
    // A fact about a document, repository, licence or issue record rather than about daemon
    // behaviour. Orthogonal to the ranking above rather than weaker than all of it: its strength
    // comes from the `chain` field, not from its position here. Exists because the observed-claim
    // check caught a licensing row claiming class `E1-upstream-verified`, which asserts that a
    // running daemon was watched.
    "E6-documentary",
];

/// How the immediate source was obtained. Mirrors `Provenance::source_chain` in the corpus, whose
/// vocabulary CodeRabbit review 4816484338 closed for the same reason.
const CHAINS: [&str; 4] = ["direct", "read-via-report", "read-via-issue", "read-via-pr"];

const STATUSES: [&str; 4] = ["settled", "open", "pending-live", "retired"];

const PLAYBACK: [&str; 4] = ["active", "idle", "unknown", "n/a"];

/// Classes whose claim is an observation of a running daemon, so a vague capture date or playback
/// state is not acceptable for them.
///
/// **Contract correction, made during stage 2 and not a silent relaxation.** The stage-1 rule was
/// "classes `E0` and `E1` may not say `unknown`" for playback. `E0` still may not: UHC ran the daemon,
/// so UHC knows. `E1` may, and only when the row's prose anchor states
/// [`UNRECORDED_PLAYBACK_ADMISSION`] — because two upstream observations genuinely have no recorded
/// playback state, and the alternatives were to guess a value or to relabel a live observation as a
/// transcription. The escape is itself checked, so it cannot be taken silently. The capture **date**
/// rule is unchanged for both: an ISO date is required.
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
    // Each strip must feed the next. Falling back to `trimmed` after stripping the prefix re-adds the
    // leading pipe, so a row missing its trailing pipe yields a leading empty cell and every field
    // shifts by one — surfacing as "malformed claim ID" for a row whose ID was fine.
    let inner = trimmed.strip_prefix('|').unwrap_or(trimmed);
    let inner = inner.strip_suffix('|').unwrap_or(inner);
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
            provenance: get(3).split(" · ").map(|p| p.trim().to_string()).collect(),
            proof: get(4),
            status: get(5),
            owner: get(6),
        });
    }
    out
}

/// Whether a line is a markdown ATX heading.
///
/// Two narrowings, both from defects rather than from taste:
///
/// * The leading-`#` test alone is wrong in this document — `#322` and `#341` start body lines all over
///   the ledger, and treating those as headings truncated a section before its `What would settle it`
///   line. An ATX heading requires a space after the run of hashes.
/// * The run is **one to six** hashes. CodeRabbit pointed out that `#######` is not a heading in
///   CommonMark, so accepting it would let a non-heading line create an artificial section boundary and
///   cut an anchor short.
fn is_heading(line: &str) -> bool {
    let h = line.trim_start();
    let hashes = h.chars().take_while(|c| *c == '#').count();
    (1..=6).contains(&hashes) && h.chars().nth(hashes) == Some(' ')
}

/// Whether a heading line *declares* `id` — the ID begins its content and ends at a boundary.
///
/// A `contains` test is exploitable, and CodeRabbit demonstrated it: a heading reading
/// `### Notes on HQP-C-0240` sits before the real `HQP-C-024` anchor, matches it on substring, and can
/// carry an unrelated acquisition plan while the real anchor's plan is deleted — so
/// `every_unsettled_claim_names_an_owner_and_what_would_settle_it` passes on a hijacked section. The
/// topic-map check cannot see it either, because the claim row itself is untouched.
fn heading_declares(line: &str, id: &str) -> bool {
    let h = line.trim_start();
    let hashes = h.chars().take_while(|c| *c == '#').count();
    let content = h[hashes..].trim_start();
    match content.strip_prefix(id) {
        // A boundary is anything that cannot extend the identifier: `HQP-C-0240` must not match
        // `HQP-C-024`, while `HQP-C-024 — …` and `HQP-C-023's …` must.
        Some(rest) => rest
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric() && c != '-' && c != '_'),
        None => false,
    }
}

/// The lines of the section whose heading declares `id`, up to the next heading.
fn anchor_section(text: &str, id: &str) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines
        .iter()
        .position(|l| is_heading(l) && heading_declares(l, id))?;
    let end = lines[start + 1..]
        .iter()
        .position(|l| is_heading(l))
        .map_or(lines.len(), |p| start + 1 + p);
    Some(lines[start..end].join("\n"))
}

/// Rows of a `|`-delimited table under the heading whose text contains `heading_marker`, excluding the
/// header and separator rows.
fn table_rows_under(heading_marker: &str) -> Vec<Vec<String>> {
    let text = ledger();
    let mut rows = Vec::new();
    let mut inside = false;
    for line in text.lines() {
        if is_heading(line) {
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
        let is_separator = c
            .iter()
            .all(|x| !x.is_empty() && x.chars().all(|ch| ch == '-' || ch == ':' || ch == ' '));
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
    parse_test_functions(&read(path))
}

/// [`test_functions`] for a file that may not exist: `None` when it cannot be read.
///
/// The panicking form aborted the aggregated diagnostics, so a claim citing `test:tests/typo.rs::name`
/// produced the ledger reader's own "this file is issue #341's deliverable" message instead of naming
/// the offending claim. A citation to a file that is not there is a missing citation.
fn test_functions_of_existing_file(path: &Path) -> Option<HashMap<String, bool>> {
    fs::read_to_string(path)
        .ok()
        .map(|t| parse_test_functions(&t))
}

fn parse_test_functions(text: &str) -> HashMap<String, bool> {
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
                // Matched narrowly on purpose. `#[cfg(test)]` contains the substring `test`, so a
                // contains-check would let a plain helper directly beneath one be accepted as a test
                // function — a false pass for a citation that names no test at all. CodeRabbit raised
                // this and could not confirm an instance; neither could I, because constructing one
                // needs an edit to a base-branch file. Narrowed anyway: the cost is one line and the
                // failure it prevents is silent.
                let attr = p.trim_start_matches("#[").trim_end_matches(']');
                if attr == "test" || attr.starts_with("test(") || attr.ends_with("::test") {
                    is_test = true;
                }
                // Matched on the bare name plus a delimiter, because `#[ignore="flaky"]` without a
                // space is valid Rust and `starts_with("ignore =")` misses it — and an ignored test
                // read as a live proof is exactly the false absence this check exists to prevent.
                let ignore_rest = attr.strip_prefix("ignore").map(str::trim_start);
                if matches!(ignore_rest, Some(r) if r.is_empty() || r.starts_with('(') || r.starts_with('='))
                {
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

/// Whether an owner cell names a usable GitHub issue: `#` followed by a non-zero decimal number, with
/// markdown decoration and backticks stripped first.
///
/// CodeRabbit found the previous rule — *contains a `#` and contains a digit* — accepted `#0`, `#abc1`
/// and `#-1`, none of which is an issue anyone can open. An owner nobody can reach is the same as no
/// owner, which is the thing this check exists to forbid.
fn is_issue_reference(cell: &str) -> bool {
    let token = cell
        .trim()
        .trim_matches(|c| c == '*' || c == '`' || c == ' ');
    let Some(number) = token.strip_prefix('#') else {
        return false;
    };
    !number.is_empty() && !number.starts_with('0') && number.chars().all(|c| c.is_ascii_digit())
}

/// Shortest acquisition plan that counts as one rather than as a label. Four words and twenty
/// characters is deliberately low: the check is meant to catch an empty or one-word cell, not to
/// legislate prose length.
const MIN_SETTLE_CHARS: usize = 20;

/// The exact label a settle condition must carry. Nothing else counts.
const SETTLE_MARKER: &str = "**What would settle it:**";

/// The acquisition plan on an anchor's designated [`SETTLE_MARKER`] line, continued across following
/// non-blank lines.
///
/// Returns `None` unless a line begins with the marker **exactly**. Two earlier forms were both false
/// passes CodeRabbit found: a `contains` test let the phrase buried in unrelated prose satisfy the
/// requirement, and a prefix test that then split on any later colon let
/// `**What would settle it is unrelated prose:** …` through — a line that reads like the label, parses
/// like the label, and names no acquisition action.
fn settle_condition(section: &str) -> Option<String> {
    let lines: Vec<&str> = section.lines().collect();
    let i = lines
        .iter()
        .position(|l| l.trim_start().starts_with(SETTLE_MARKER))?;
    let after = lines[i].trim_start().strip_prefix(SETTLE_MARKER)?.trim();
    let mut plan = after.to_string();
    for line in &lines[i + 1..] {
        if line.trim().is_empty() {
            break;
        }
        plan.push(' ');
        plan.push_str(line.trim());
    }
    Some(plan.trim().to_string())
}

/// Fixtures whose provenance records that the cited upstream file was not read directly.
///
/// Derived from the corpus rather than curated, so the ledger's pending-confirmation table cannot
/// drift from the evidence base it describes.
fn second_hand_fixtures() -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for profile in corpus::profiles() {
        for fixture in corpus::all_in(&profile) {
            if fixture.provenance.source_chain.contains("read-via-report") {
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
        match id
            .strip_prefix("HQP-C-")
            .and_then(|n| n.parse::<usize>().ok())
        {
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

/// The admission an `E1` row must carry in its prose anchor to be allowed `playback: unknown`.
///
/// `E0` rows have no such escape: UHC ran the daemon, so UHC knows.
const UNRECORDED_PLAYBACK_ADMISSION: &str = "Playback state was not recorded upstream";

#[test]
fn an_observed_claim_names_a_real_capture_date_and_playback_state() {
    let iso = |d: &str| {
        d.len() == 10
            && d.as_bytes()[4] == b'-'
            && d.as_bytes()[7] == b'-'
            && d.chars().filter(char::is_ascii_digit).count() == 8
    };
    let text = ledger();
    let mut bad = Vec::new();
    for c in claims() {
        if !OBSERVED_CLASSES.contains(&c.class.as_str()) {
            continue;
        }
        if !iso(c.date()) {
            bad.push(format!("{} date {:?}", c.id, c.date()));
        }
        match c.playback() {
            "active" | "idle" => {}
            // An upstream observation whose playback state nobody wrote down is a real case, and the
            // two dishonest ways out are to guess a value or to reclassify a live observation as a
            // transcription. The third way is to say so in the row's anchor, where a reader sees it.
            "unknown" if c.class == "E1-upstream-verified" => {
                let admitted = anchor_section(&text, &c.id)
                    .is_some_and(|s| s.contains(UNRECORDED_PLAYBACK_ADMISSION));
                if !admitted {
                    bad.push(format!(
                        "{} is E1 with playback \"unknown\" and its anchor does not state \
                         {UNRECORDED_PLAYBACK_ADMISSION:?}",
                        c.id
                    ));
                }
            }
            other => bad.push(format!("{} playback {other:?}", c.id)),
        }
    }
    assert!(
        bad.is_empty(),
        "a claim in class {OBSERVED_CLASSES:?} says a running daemon was observed, so `unknown` is \
         not an available answer for when, or for whether audio was playing — a behaviour verified \
         idle is not thereby verified under load. An `E1` row may say `unknown` only if its anchor \
         admits the upstream record is silent. Offending: {bad:#?}"
    );
}

// ===========================================================================================
// Proof checks — the ledger's link to executable truth
// ===========================================================================================

#[test]
fn every_cited_test_exists_and_is_not_ignored() {
    let mut cache: HashMap<String, Option<HashMap<String, bool>>> = HashMap::new();
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
            let path = repo_root().join(&file);
            let Some(fns) = cache
                .entry(file.clone())
                .or_insert_with(|| test_functions_of_existing_file(&path))
                .as_ref()
            else {
                missing.push(format!(
                    "{} cites {file}::{name}, but {file} is not a readable file",
                    c.id
                ));
                continue;
            };
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

/// A `fixture:` proof must name a **corpus document**, not merely a path that happens to exist.
///
/// CodeRabbit found the existence-only version accepted `fixture:README.md`, a test source, or a
/// directory. The documented contract is that a fixture proof points at a corpus document *carrying
/// provenance*, so the check enforces all three: the path lives under the corpus root, it is a regular
/// `.xml`/`.html` file, and it loads through `corpus::load`, which panics unless the file opens with a
/// complete provenance header. Loading it is the strongest available form — it means a fixture proof
/// cannot cite a document whose own evidence metadata is missing or malformed.
#[test]
fn every_cited_fixture_is_a_corpus_document_that_carries_provenance() {
    const CORPUS_ROOT: &str = "tests/fixtures/hqplayer/";
    let mut bad = Vec::new();
    for c in claims() {
        for proof in c.proof.split(" · ") {
            let Some(rel) = proof.trim().trim_matches('`').strip_prefix("fixture:") else {
                continue;
            };
            let Some(tail) = rel.strip_prefix(CORPUS_ROOT) else {
                bad.push(format!(
                    "{} cites {rel}, which is not under {CORPUS_ROOT}",
                    c.id
                ));
                continue;
            };
            let path = repo_root().join(rel);
            if !path.is_file() {
                bad.push(format!("{} cites {rel}, which is not a regular file", c.id));
                continue;
            }
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or_default()
                .to_ascii_lowercase();
            if ext != "xml" && ext != "html" {
                bad.push(format!(
                    "{} cites {rel}, whose extension {ext:?} is not a corpus document type",
                    c.id
                ));
                continue;
            }
            let Some((profile, file)) = tail.split_once('/') else {
                bad.push(format!(
                    "{} cites {rel}, which is not <profile>/<stem>.<ext> under the corpus root",
                    c.id
                ));
                continue;
            };
            let stem = file.rsplit_once('.').map(|(s, _)| s).unwrap_or(file);
            // Panics with the loader's own diagnosis if the provenance header is absent, misplaced or
            // missing a required field — which is the outcome we want a red test to report.
            let fixture = corpus::load(profile, stem);
            if fixture.provenance.status.is_empty() {
                bad.push(format!(
                    "{} cites {rel}, whose provenance records no status",
                    c.id
                ));
            }
        }
    }
    assert!(
        bad.is_empty(),
        "a `fixture:` proof must name a corpus document carrying provenance, because the provenance \
         header is what makes the citation evidence rather than a file path: {bad:#?}"
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
            let has_value = p.split_once(':').is_some_and(|(_, v)| !v.trim().is_empty());
            if !known || !has_value {
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
        let proofs: Vec<&str> = c
            .proof
            .split(" · ")
            .map(|p| p.trim().trim_matches('`'))
            .collect();
        let executable = proofs
            .iter()
            .any(|p| p.starts_with("test:") || p.starts_with("fixture:"));
        let future_only = !executable
            && proofs
                .iter()
                .any(|p| p.starts_with("#332:") || p.starts_with("none:"));
        if future_only && c.status == "settled" {
            bad.push(format!(
                "{} status {:?} proof {:?}",
                c.id, c.status, c.proof
            ));
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
        if !is_issue_reference(&c.owner) {
            bad.push(format!(
                "{} owner {:?} is not a usable issue reference",
                c.id, c.owner
            ));
        }
        // The settle condition lives in the claim's prose anchor, because one table cell cannot hold
        // it honestly. The anchor is the heading that carries the ID.
        //
        // CodeRabbit found the substring form of this check to be a false pass: an anchor whose
        // `**What would settle it:**` line was emptied after the colon still reported green, and so
        // would one that mentioned the phrase in unrelated prose. So the *designated line* is parsed
        // and its content is required to be a sentence, not a label.
        match anchor_section(&text, &c.id) {
            None => bad.push(format!("{} has no prose anchor section", c.id)),
            Some(section) => match settle_condition(&section) {
                None => bad.push(format!(
                    "{} anchor has no line beginning with the exact marker {SETTLE_MARKER:?}; the \
                     phrase inside other prose, or a variant label that merely parses like it, does \
                     not count",
                    c.id
                )),
                Some(plan) if plan.len() < MIN_SETTLE_CHARS || plan.split_whitespace().count() < 4 => {
                    bad.push(format!(
                        "{} names its settle condition as {plan:?}, which is a label rather than an \
                         acquisition plan",
                        c.id
                    ))
                }
                Some(_) => {}
            },
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
    // Columns of the `Live runs` table: Run | Edition / version | Date | Playback | Evidence.
    //
    // Compared **positionally and exactly**, not by substring over a joined row. CodeRabbit found the
    // substring version accepted an `E0` row whose playback state contradicted the registry: flipping
    // one row's `idle` to `active` still passed, because the joined string was only searched for the
    // daemon and the date. Playback state is part of the provenance contract, so it is part of the
    // match — and after HQP-C-023, a playback state nobody recorded is the specific error this ledger
    // has already made once.
    let mut bad = Vec::new();
    for c in claims() {
        if c.class != "E0-uhc-live" {
            continue;
        }
        let matched = registry.iter().any(|row| {
            let col = |n: usize| row.get(n).map(String::as_str).unwrap_or_default();
            col(1) == c.daemon() && col(2) == c.date() && col(3) == c.playback()
        });
        if !matched {
            bad.push(format!(
                "{} claims first-hand evidence on {:?} at {:?} with playback {:?}; no registry row \
                 records that exact combination",
                c.id,
                c.daemon(),
                c.date(),
                c.playback()
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
        (
            "SetRate semantics: index on the wire, the exact Hz pin, Auto, mode/filter/device \
             dependence, and mode switches clearing the pin",
            &[
                "HQP-C-015",
                "HQP-C-016",
                "HQP-C-017",
                "HQP-C-018",
                "HQP-C-019",
                "HQP-C-020",
                "HQP-C-021",
                "HQP-C-022",
            ],
        ),
        ("active_mode contradiction", &["HQP-C-023", "HQP-C-024"]),
        (
            "HTTP / profile / session-auth negative findings",
            &["HQP-C-043", "HQP-C-044", "HQP-C-045", "HQP-C-048"],
        ),
        ("apply-then-drop ambiguity", &["HQP-C-029"]),
        ("push status cadence", &["HQP-C-037"]),
        ("LibraryPicture binary interlude", &["HQP-C-038"]),
        (
            "licensing and provenance",
            &["HQP-C-052", "HQP-C-053", "HQP-C-054"],
        ),
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
        // HQP-C-023 (`Status.active_mode` echoes the configured mode) *is* measured and settled. The
        // half that must stay open is HQP-C-024, `State.active_mode` under `[source]`, which nobody
        // has measured.
        ("State.active_mode under [source]", "HQP-C-024"),
        ("apply-then-drop ambiguity", "HQP-C-029"),
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

/// HQP-C-062: the daemon accepts `SetJunkFilter` and UHC's adapter exposes no setter for it.
///
/// The wire half of that pair is proven by a conformance test; the *adapter-surface* half was not proven
/// by anything, and CodeRabbit was right that one citation cannot carry both. This is the missing half:
/// a text scan of the adapter for a junk-filter setter. If #329 adds one, this test fails and the ledger
/// row must be updated — which is the behaviour wanted, because the claim would then be false.
/// Whether `src` **declares** a function or method named `name`, determined structurally.
///
/// A text scan for `fn <name>` is evadable and was: `pub  async  fn   set_junk_filter` with ordinary
/// `rustfmt` spacing slipped past it, as does a signature broken across lines. `syn` is already a direct
/// dev-dependency, so the signatures are visited instead of matched — which covers `ItemFn`,
/// `ImplItemFn` and `TraitItemFn` alike, because every one of them carries a [`syn::Signature`].
///
/// A fragment that is not a whole file is re-parsed inside a probe function body, so a *call*, a string
/// literal or a `let` binding is still analysed structurally rather than dismissed as unparseable —
/// which is what makes the negative controls meaningful.
///
/// **Residual limit, stated rather than hidden:** a setter generated by a macro is invisible here, since
/// nothing is expanded. A parse failure panics instead of reporting "no setter", because reading an
/// unparseable adapter as an absence is the false negative this check exists to prevent.
fn declares_fn(src: &str, name: &str) -> bool {
    use syn::visit::Visit;

    struct Finder<'a> {
        want: &'a str,
        found: bool,
    }
    impl<'ast> Visit<'ast> for Finder<'_> {
        fn visit_signature(&mut self, sig: &'ast syn::Signature) {
            if sig.ident == self.want {
                self.found = true;
            }
            syn::visit::visit_signature(self, sig);
        }
    }

    let scan = |file: &syn::File| {
        let mut f = Finder {
            want: name,
            found: false,
        };
        f.visit_file(file);
        f.found
    };

    // A ladder, because a fragment can fail to be a file for two different reasons and neither should
    // be read as "no such function". First as a whole file (the real adapter, and any item form). Then
    // with a trailing item, so a dangling doc comment or attribute has something to attach to. Then as
    // statements in a probe body, so a call, a `let` or a string literal is analysed structurally
    // rather than dismissed.
    let attempts = [
        src.to_string(),
        format!("{src}\nstruct __UhcProbeItem;"),
        format!("fn __uhc_probe() {{ {src} }}"),
    ];
    for attempt in &attempts {
        if let Ok(file) = syn::parse_file(attempt) {
            return scan(&file);
        }
    }
    panic!(
        "cannot parse the source given to declares_fn in any of the three forms tried; refusing to \
         report an absence from a parse failure, because that is exactly the false negative this check \
         exists to prevent"
    )
}

#[test]
fn the_adapter_exposes_no_junk_filter_setter() {
    let adapter = read(&repo_root().join("src/adapters/hqplayer.rs"));
    let found = declares_fn(&adapter, "set_junk_filter");
    assert!(
        !found,
        "HQP-C-062 records that UHC exposes no junk-filter setter, and the adapter now has one: \
         The daemon capability is real (HQP-C-051) — if it is now offered, update ledger row \
         HQP-C-062 rather than this test"
    );
}

/// A text scan for `fn set_junk_filter` is evadable, and the point of this control is to say how.
///
/// Codex raised it: `fn` and the name can be split across lines, qualifiers and spacing vary, and a
/// trait or impl method reads differently from a free function — while a comment, a string literal or a
/// call expression must *not* count as a declaration.
#[test]
fn a_junk_filter_setter_is_detected_however_it_is_written() {
    for declared in [
        "fn set_junk_filter(&self) {}",
        "pub fn set_junk_filter(&self) {}",
        "    pub  async  fn   set_junk_filter (&self) {}",
        // The formatting escape: `rustfmt` can and does break here on a long signature.
        "fn\nset_junk_filter(&self) {}",
        "impl HqpAdapter { pub async fn set_junk_filter(&self, i: u32) -> Result<()> { Ok(()) } }",
        "trait JunkFilter { fn set_junk_filter(&self, i: u32); }",
        "impl JunkFilter for HqpAdapter { fn set_junk_filter(&self, i: u32) {} }",
    ] {
        assert!(
            declares_fn(declared, "set_junk_filter"),
            "must be detected as a declaration: {declared:?}"
        );
    }

    for not_declared in [
        "// fn set_junk_filter would go here",
        "/// See `fn set_junk_filter` in the daemon docs",
        "let msg = \"fn set_junk_filter\";",
        "self.set_junk_filter(3).await?;",
        "let f = set_junk_filter;",
        "fn set_junk_filter_helper_name_differs(&self) {}",
    ] {
        assert!(
            !declares_fn(not_declared, "set_junk_filter"),
            "must not be read as a declaration: {not_declared:?}"
        );
    }
}

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
        for (pos, _) in spec.match_indices(needle) {
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
    // Two phrasings, because the document said it twice — once as a warning under **State vs Status**
    // and once as a checklist item — and fixing only the phrasing a test names is how the other
    // survives. What is forbidden is the *unqualified imperative*, not the words appearing at all, so
    // either marker discharges it:
    //
    // * `[retired #341]` — this issue struck the line in place.
    // * `HQP-C-024` — the sentence qualifies itself by citing the open ledger row. The #322 branch
    //   independently reframed both phrasings this way while this branch was being written, keeping
    //   "use State's active_mode (INDEX), not Status's string" as the *explicit PCM/SDM* case and
    //   naming the `[source]` case unmeasured. That is the outcome this check exists to produce, so
    //   it must pass, and the alternative — demanding this branch's own marker — would have made the
    //   check reject the better wording.
    let retired_imperatives = [
        "Always use State's numeric",
        "use State's active_mode (INDEX), not Status's string",
    ];
    let qualifiers = ["[retired #341]", "HQP-C-024"];
    let mut found = Vec::new();
    for line in text.lines() {
        for phrase in retired_imperatives {
            if line.contains(phrase) && !qualifiers.iter().any(|q| line.contains(q)) {
                found.push(line.trim().to_string());
            }
        }
    }
    assert!(
        found.is_empty(),
        "`docs/hqplayer-protocol-reference.md` still instructs the reader to pick `State.active_mode` \
         globally. `Status.active_mode` echoing the configured mode is measured; `State.active_mode` \
         under `[source]` is unmeasured — the conformance suite deliberately refuses to settle it \
         (`the_fake_does_not_settle_the_independent_state_and_status_active_mode_semantics`), so the \
         document must not. Offending lines: {found:#?}"
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

// ===========================================================================================
// Focused controls for the parsing helpers (CodeRabbit review 4823720730)
//
// Every false pass this file has shipped was in a helper, not in a check: `contains` where a prefix
// was meant, a prefix where an exact marker was meant, a fallback that re-added a delimiter. Mutating
// the ledger to prove each one works once and proves nothing afterwards, because the mutation is
// reverted. These call the helpers directly with the exploit strings as inputs, so the boundary stays
// pinned.
// ===========================================================================================

/// `heading_declares` must match the claim ID as the **leading identifier**, not as a substring.
#[test]
fn heading_declares_requires_the_id_to_lead_and_to_end_at_a_boundary() {
    // Accepted: the ID leads, and what follows cannot extend an identifier.
    for good in [
        "### HQP-C-024 — `State.active_mode` under `[source]`",
        "#### HQP-C-023's playback state was corrected",
        "###### HQP-C-024",
        "  ### HQP-C-024 trailing prose",
        "### HQP-C-024, with a comma",
        "### HQP-C-024: with a colon",
    ] {
        assert!(
            heading_declares(good, "HQP-C-024") || heading_declares(good, "HQP-C-023"),
            "should declare its ID: {good:?}"
        );
    }

    // Rejected: prefix collision. This is CodeRabbit's exact exploit — a decoy heading placed before
    // the real anchor, hijacking the settle condition of a row whose plan has been deleted.
    for bad in [
        "### Notes on HQP-C-0240",
        "### HQP-C-0240",
        "### HQP-C-024_note",
        "### HQP-C-024-bis",
        "### Notes on HQP-C-024",
        "not a heading at all HQP-C-024",
    ] {
        assert!(
            !heading_declares(bad, "HQP-C-024"),
            "must not declare HQP-C-024: {bad:?}"
        );
    }
}

/// Only one-to-six hashes open a section, per CommonMark. A seven-hash line is paragraph text, and
/// treating it as a heading would let it cut an anchor short before its acquisition plan.
#[test]
fn only_one_to_six_hashes_open_a_section() {
    for good in ["# a", "## a", "###### a", "   ### indented"] {
        assert!(is_heading(good), "should be a heading: {good:?}");
    }
    for bad in [
        "####### seven is not a heading",
        "#no-space",
        "#322 starts a body line",
        "#341's row",
        "",
        "text",
    ] {
        assert!(!is_heading(bad), "must not be a heading: {bad:?}");
    }
}

/// The settle condition must carry the **exact** marker. A label that merely parses like it is the
/// false pass CodeRabbit found: it reads as a plan, satisfies a length floor, and names no action.
#[test]
fn settle_condition_requires_the_exact_marker_and_reads_its_continuation() {
    let plan = settle_condition(
        "### HQP-C-099 — a row\n\n**What would settle it:** a read-only capture of the thing,\nrecorded per device.\n",
    );
    assert_eq!(
        plan.as_deref(),
        Some("a read-only capture of the thing, recorded per device."),
        "the plan must continue across following non-blank lines"
    );

    // A blank line ends the plan, so unrelated prose below it is not absorbed.
    let bounded =
        settle_condition("**What would settle it:** capture it live.\n\nUnrelated prose.\n");
    assert_eq!(bounded.as_deref(), Some("capture it live."));

    for malformed in [
        "**What would settle it is unrelated prose:** this sufficiently long text is not the label.",
        "Somebody should decide What would settle it: a read-only capture.",
        "**what would settle it:** lower case is a different marker",
        "What would settle it: unbolded",
        "**What would settle it** no colon at all",
    ] {
        assert!(
            settle_condition(malformed).is_none(),
            "must not be read as a settle condition: {malformed:?}"
        );
    }
}

/// A row missing its trailing pipe must be reported as a table-shape problem, not silently shifted by
/// one field — which surfaced as a confusing "malformed claim ID" for a row whose ID was fine.
#[test]
fn cells_does_not_reinstate_the_leading_pipe_when_a_row_lacks_a_trailing_one() {
    assert_eq!(cells("| a | b | c |"), vec!["a", "b", "c"]);
    assert_eq!(
        cells("| a | b | c"),
        vec!["a", "b", "c"],
        "a missing trailing pipe must not re-add the leading empty cell"
    );
    assert_eq!(cells("a | b | c |"), vec!["a", "b", "c"]);
    assert_eq!(cells("a | b"), vec!["a", "b"]);
}

/// An owner must be a reachable issue. `#0`, `#abc1` and `#-1` are not.
#[test]
fn is_issue_reference_requires_a_hash_and_a_non_zero_decimal() {
    for good in ["#332", "#1", "`#348`", "**#337**", " #341 "] {
        assert!(is_issue_reference(good), "should be an issue ref: {good:?}");
    }
    for bad in ["#0", "#00", "#abc1", "#-1", "#", "332", "", "—", "#3a2"] {
        assert!(
            !is_issue_reference(bad),
            "must not be an issue ref: {bad:?}"
        );
    }
}

/// `#[ignore]` must be detected however it is spelled. `#[ignore="flaky"]` without a space is valid
/// Rust, and an ignored test proves nothing — so a citation to one must not read as proof.
#[test]
fn an_ignored_test_is_detected_with_or_without_spacing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("probe.rs");
    std::fs::write(
        &path,
        r#"
#[test]
fn plain_test() {}

#[test]
#[ignore]
fn bare_ignore() {}

#[test]
#[ignore = "spaced reason"]
fn spaced_ignore() {}

#[test]
#[ignore="unspaced reason"]
fn unspaced_ignore() {}

#[tokio::test]
#[ignore("parenthesised")]
fn parenthesised_ignore() {}

#[cfg(test)]
fn not_a_test_despite_the_substring() {}
"#,
    )
    .expect("write probe");

    let fns = test_functions(&path);
    assert_eq!(
        fns.get("plain_test"),
        Some(&false),
        "plain test, not ignored"
    );
    for ignored in [
        "bare_ignore",
        "spaced_ignore",
        "unspaced_ignore",
        "parenthesised_ignore",
    ] {
        assert_eq!(
            fns.get(ignored),
            Some(&true),
            "{ignored} carries #[ignore] in some spelling and must be reported as ignored"
        );
    }
    assert!(
        !fns.contains_key("not_a_test_despite_the_substring"),
        "`#[cfg(test)]` contains the substring `test` but does not make a function a test"
    );
}

/// A citation naming a proof file that does not exist must be reported as a missing citation, naming
/// the offending claim — not panic inside the file reader and abort the aggregated diagnostics.
#[test]
fn a_missing_proof_file_is_reported_rather_than_panicking() {
    let missing = repo_root().join("tests/this_file_does_not_exist_341.rs");
    assert!(!missing.exists(), "precondition: the path is absent");
    assert!(
        test_functions_of_existing_file(&missing).is_none(),
        "an absent proof file yields None so the caller can aggregate it as a missing citation"
    );
    assert!(
        test_functions_of_existing_file(&repo_root().join(DEFAULT_PROOF_FILE)).is_some(),
        "the real conformance suite is readable"
    );
}
