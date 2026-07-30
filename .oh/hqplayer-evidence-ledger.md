# Issue #341 — HQPlayer protocol evidence ledger

**Branch:** `feat/issue-341-hqplayer-evidence-ledger`
**Base:** `feat/issue-322-hqplayer-protocol-conformance` at `bc9158e2f8dc787016d0bbd71f2ef1f810184618`
**Ultimate target:** `v3`. **This PR must not merge before #337.** It is stacked deliberately: every
claim it records cites a fixture or test that exists only on the #322 branch.

**Program:** #310 · **Epic:** #311 · **Related:** #322 (PR #337), #328, #329, #330, #332, #347, #348
**OH:** 80222d6d

---

## Aim

A contributor who needs to know how HQPlayer's control protocol behaves can answer four questions in
one place without reconciling documents by hand:

1. **What do we know?**
2. **How do we know it** — whose observation, read directly or via a report?
3. **For which edition/version, on what date, with playback active or idle?**
4. **Which executable test or fixture proves it** — or, if none, what would settle it and who owns that?

The outcome is not "a better protocol document". It is that the next contributor stops re-deriving
dead ends. Three specific dead ends are already documented as having been walked twice: a partial
`POST /config` that answers HTTP 200 without applying, daemon profile save/load as a durable preset
store, and self-generated 4321 session-authentication keys.

**Not the aim:** becoming a third authority. ADR 003 settled that the executable corpus is the
authority. A prose ledger that competes with it re-creates the #322 problem one layer out.

---

## Problem context

`.oh/hqplayer-spec.md` (2026-02-04) and `docs/hqplayer-protocol-reference.md` (2026-02-05) overlap and
disagree. The disagreement is not cosmetic:

| Document | Claim | Line |
|---|---|---|
| `.oh/hqplayer-spec.md` | `SetMode` expects **VALUE** — "mode values: -1=[source], 0=PCM, 1=SDM" | `:57`, `:150` |
| `docs/hqplayer-protocol-reference.md` | `SetMode` expects **INDEX** — "CLI: `--set-mode <index>`" | `:99`, `:114`, `:196` |

Both cannot be right, and the two numbers differ for every mode: `[source]` is index 0 / value −1,
PCM index 1 / value 0, SDM index 2 / value 1. A contributor who lands on the spec file first and
implements from it writes a client that is wrong on all three modes.

Alongside that, the evidence that matters most is scattered across places a contributor will not
find:

- **Negative findings** live in #330's and #341's bodies, not in any document a protocol reader opens.
- **Ambiguous-delivery evidence** — HQPTuner `origin/dev@04ab82e1`, where a `SetMode` on hqplayerd
  6.0.4 was accepted and acted on while the daemon never replied and later dropped the connection —
  exists only in #341's body. Nothing in the repository classifies "timeout after write" as
  *ambiguous* rather than *failed*, and the executable fixture that models it
  (`apply_then_drop_harness`, `tests/hqplayer_conformance.rs:4857`) is not linked to it from any
  document.
- **Unresolved contradictions** are resolved *by prose* today. `docs/hqplayer-protocol-reference.md:186`
  states as a rule: "**Warning:** Status's `active_mode` may show `[source]` even when outputting DSD.
  Always use State's numeric `active_mode`." The #322 work established that this is not a settled
  contradiction at all: `Status.active_mode` echoing the configured mode is *measured*;
  `State.active_mode` under `[source]` is *unmeasured*. The suite deliberately refuses to settle it
  (`the_fake_does_not_settle_the_independent_state_and_status_active_mode_semantics`, `:3283`), while
  the document tells the reader to pick a side.
- **The version boundary is unmarked.** One live run (HQPlayer Embedded 6.0.2, engine 6.0.4,
  2026-07-30, PR #337 comment 5135836825) is the only first-hand UHC evidence that exists. Everything
  else is upstream, read via report, on one Opal rig. Nothing in the documents distinguishes them.

**Why now:** #322 built the machinery that makes a ledger checkable — per-fixture provenance with a
`source_chain` field, a closed `tier` vocabulary, and 212 named conformance tests. Before that, a
ledger could only have been prose. After #337 merges, six issues (#162, #208, #328, #329, #330, #347)
start consuming these claims; they need to know which are load-bearing and which are guesses.

## Evidence base read before generating candidates

Read in full for this stage:

- `AGENTS.md`; issue #341 body plus both comments ([5125948432](https://github.com/open-horizon-labs/unified-hifi-control/issues/341#issuecomment-5125948432), [5126438674](https://github.com/open-horizon-labs/unified-hifi-control/issues/341#issuecomment-5126438674)).
- Issues #310, #311, #322, #328, #329, #330, #332, #343, #347, #348.
- PR #337 body and its live-validation comment [5135836825](https://github.com/open-horizon-labs/unified-hifi-control/pull/337#issuecomment-5135836825) — the only first-hand UHC live evidence in existence.
- `.oh/hqplayer-spec.md`, `docs/hqplayer-protocol-reference.md`, `docs/adr/003-hqplayer-conformance-boundary.md`.
- `.oh/issue-322-hqplayer-protocol-conformance.md` — in particular the HQPTuner Stage 1 amendment's
  four-class split (`:1107`–`:1198`), whose **class D** rows are this issue's inbox, and the two
  corrections at `:1204`–`:1233`.
- `tests/mock_servers/hqplayer/corpus.rs` (the `Provenance` struct, `:30`–`:81`), the 20 fixture
  provenance headers under `tests/fixtures/hqplayer/`, and the test-name inventory of
  `tests/hqplayer_conformance.rs`.
- `tests/oneshot_leak_lint.rs` — read as a **counter-example**: it is a lint that structurally cannot
  fail (`analyze_file` returns `vec![]` unconditionally, `:59`). Whatever this issue builds must not
  be that.

**Not read, and not claimable:** HQPTuner's repository at any ref, `hqp-control` sources, and the two
salvage reports (`UHC-SALVAGE-UI-DATA-INTEGRATION.md`, `UHC-SALVAGE-BETA-DEV.md`) — they were read
during #322 from a temporary path and are not in this repository. Every upstream claim therefore
reaches this ledger third-hand at best: upstream → salvage report → #322 session file or issue body →
here. The ledger has to say so per claim rather than once in a footnote.

## Binding constraints

| # | Constraint | Source |
|---|---|---|
| 1 | Executable fixtures outrank prose; the ledger indexes the corpus, it does not compete with it | #341 hard constraint; ADR 003 |
| 2 | Every empirical claim names source repo/ref, edition/version, capture date, playback state | #341 AC6 |
| 3 | Unresolved contradictions stay explicit; no global winner chosen for a versioned question | #341 hard constraint |
| 4 | One 6.0.2/engine-6.0.4 rig observation is not universalised | #341, #332 |
| 5 | No proprietary/manual text beyond lawful concise paraphrase; licenses and provenance preserved | #341, #348 |
| 6 | Test-first for anything executable; RED captured before GREEN | `AGENTS.md`; #310 |
| 7 | No public API change; no route, payload, or `tests/fixtures/api_routes.txt` edit; no label | `AGENTS.md` |
| 8 | No live HQPlayer host contact in this issue — use the recorded #337 report and repo fixtures | task instruction |
| 9 | No force-push, no merge, no ready-state change, PR stays draft | `AGENTS.md`; task instruction |
| 10 | Docs-only consistency still gets machine-checkable non-vacuity tests where proportionate | task instruction |

---

## Solution Space Analysis
**Updated:** 2026-07-30T21:10Z

**Problem:** UHC's HQPlayer protocol knowledge is spread across two documents that contradict each
other on `SetMode`, an ADR, a 2,300-line session file, six issue bodies and one PR comment — so a
contributor cannot tell a measured fact from a transcription, cannot tell which HQPlayer version a
claim covers, and will re-walk documented dead ends.

**Key constraint:** The corpus is already the authority (ADR 003). The ledger must therefore be a
*derived, mechanically-checked index* — anything it asserts that the corpus can check must be checked,
or the ledger becomes the fourth thing that can be stale.

**Success looks like:** a contributor opens one file, finds the claim, and sees its evidence class,
provenance quadruple, and the test name that proves it — and if the ledger ever disagrees with the
corpus or cites a test that no longer exists, `cargo test` fails.

### Candidates considered

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-Aid | Fix the `SetMode` line in both documents; append the negative findings as a bullet list | Leaves two authorities; nothing enforces it; the next capture desyncs it silently |
| B | Local Optimum | Merge both documents into one hand-maintained reference with a provenance column | One authority, still prose-only: a renamed test, an upgraded `status:`, or a new `read-via-report` fixture all pass unnoticed |
| C | **Reframe** | **Evidence ledger with stable claim IDs and a closed evidence-class vocabulary, plus a lint that checks it against the corpus and the suite**; prose docs demoted with retirement banners rather than deleted | Owns a new lint test; the ledger's *prose* rows still need a human |
| D | Redesign | Generate the ledger from fixture metadata plus per-test claim-ID annotations; the document becomes a build artifact | Highest integrity for code-backed claims, but negative findings, licensing, ambiguity classification and upstream-pending observations have **no test to hang on**, so the generated doc omits exactly the evidence #341 exists to preserve; and annotating a 5,741-line suite owned by an open PR guarantees conflicts |
| E | Reframe | Keep everything in GitHub issues; add only cross-links from the docs | The negative findings already live there and are already being re-derived — that is the observed failure. Not versioned with the code, invisible to a contributor reading the repo, and unlintable |

### Evaluation

**Option A — patch the contradiction**
- Solves the stated problem: **No.** Fixes one row of one symptom. The three numeric domains, the
  ambiguity classification, and the version boundary all stay unrecorded.
- Implementation cost: Low. Maintenance: Low but permanent — every future correction repeats this.
- Second-order: leaves two documents that a reader must diff. That is the defect, not the format.
- Optionality: none. Nothing downstream can be built on it.

**Option B — merge into one prose reference**
- Solves the stated problem: **Partially.** One authority is a real gain; unenforceable provenance is
  the same failure mode ADR 003 already caught once, when `Provenance::is_verified()` accepted
  `verified-shape` while the honesty guard checked only the exact string `verified`
  (ADR 003, "Only byte-for-byte captures may claim `verified`").
- Cost: Medium. Maintenance: **High** — the load-bearing facts (which fixtures are second-hand, which
  tests exist) drift with every commit to the suite.
- Second-order: a reader who trusts an unchecked provenance column is worse off than one who knows
  the document is prose.
- Optionality: moderate.

**Option C — ledger + lint**
- Solves the stated problem: **Yes.** Claim IDs give downstream issues a stable citation
  (`HQP-C-023` survives a rename that `docs/…#L186` does not). The lint makes four classes of drift
  loud: a fabricated or renamed test name, a `read-via-report` fixture missing from the
  pending-confirmation table, an open contradiction with no owner, and a retired document losing its
  banner.
- Cost: Medium — one ~400-line lint plus the ledger. Maintenance: **Low**, and self-announcing: the
  failure mode is a red test with the exact drifted row named.
- Second-order: two real ones. (1) The lint is text-scanning, so it constrains form, not truth — a
  wrong claim with a valid citation passes. (2) A lint that can't fail is worse than none
  (`oneshot_leak_lint.rs`), so each check must be proven RED before the ledger satisfies it.
- Optionality: high. #332's live rows, #347's semantics work and #348's guardrail all get a stable
  place to record outcomes, and the pending-confirmation table *is* #332's inbox.

**Option D — generate from metadata**
- Solves the stated problem: **No, in the part that matters.** Roughly a third of #341's acceptance
  criteria concern claims with no executable proof by nature: three negative findings, one ambiguity
  classification, the licensing trichotomy, and the two rate-list evidence gaps recorded in comment
  5126438674. A generator has nothing to emit for them.
- Cost: High. Maintenance: High while #337 is open — per-test annotations in
  `tests/hqplayer_conformance.rs` conflict with every remediation commit on the base branch.
- Second-order: would fight the base branch for a month to cover the two-thirds that C already covers
  mechanically.
- Optionality: high in principle, unreachable now.

**Option E — issues only**
- Solves the stated problem: **No.** The premise is disproven by observation: the negative findings
  are already in issue bodies and #341 exists because they are being re-derived anyway.

### Recommendation

**Selected: Option C — ledger with stable claim IDs, a closed evidence-class vocabulary, and a
non-vacuous lint. Level: Reframe.**

**Rationale.** The reframe is that this is not a documentation problem. Prose already told the reader
what to believe (`docs/hqplayer-protocol-reference.md:186` — "Always use State's numeric
`active_mode`") and was wrong to be that confident. The deliverable is therefore an *evidence-ranking
discipline* with a mechanical floor under it: ranked classes, a provenance quadruple, a per-claim
proof pointer, and a lint that fails when the pointer rots. Options A and B differ from C only by
lacking the floor, and the repository has already been bitten twice by unfloored provenance — once by
`verified-shape`, once by two factual errors from treating a report's citation as if it had been read.

**Borrowed from D, because it is free:** the parts that *can* be derived are derived, not curated.
The pending-first-hand-confirmation table is computed from fixture `source_chain` values, and the
cited-test check is computed from the suite. Only the rows with no executable proof are hand-written.

**Why not the others:** A leaves the contradiction machinery intact; B ships an unenforceable
provenance column; D cannot express the negative findings and conflicts with the open base branch;
E is the status quo that produced this issue.

**Accepted trade-offs:**

1. **The lint checks form, not truth.** A well-formed row citing a real test can still be wrong. The
   mitigation is that classes E0–E4 make the *strength* of each claim explicit, so a reader knows how
   much weight a row carries. Stated in the ledger itself rather than left implicit.
2. **Claim IDs are a small permanent commitment.** Renumbering breaks downstream citations, so IDs are
   append-only and retirement is a status change, never a deletion.
3. **A fifth document exists.** Net document count goes from two contradicting to one authoritative
   plus two explicitly retired-in-place. The session file and ADR keep their roles.
4. **`.oh/hqplayer-spec.md` keeps its wrong table**, struck through with a banner, following the
   repository's existing convention (`.oh/issue-322-…:1706` — "claims it made that turned out to be
   wrong are struck through in place and corrected here rather than deleted"). A reader who arrives
   via an old link must see the correction, not a clean file that hides the history.

### Local-maximum check

The first instinct was B — one merged document — and it was abandoned for a reason, not for novelty:
B's provenance column is exactly the artefact whose unenforceability ADR 003 already documents as
having failed once in this codebase. D was taken seriously enough to cost, and lost on a
capability gap rather than on effort: two of the four evidence classes it must carry have no test to
attach to. The selected option is one rung below the ceiling, deliberately.

---

## Implementation notes (binding for Stage 2)

### Ledger location and shape

`docs/hqplayer-evidence-ledger.md`. One row per claim:

| Field | Rule |
|---|---|
| **ID** | `HQP-C-NNN`, append-only, never renumbered or reused |
| **Claim** | One sentence, falsifiable, in UHC's own words — never a quotation of manual or upstream prose |
| **Class** | Closed vocabulary, ranked: `E0-uhc-live` > `E1-upstream-verified` > `E2-official-source` > `E3-derived` > `E4-unverified` > `E5-synthetic` |
| **Provenance** | Source + ref · edition/version · capture date · playback state. `unknown` is a legal value and is the honest one for derived rows |
| **Proof** | A test name in `tests/hqplayer_conformance.rs`, a fixture path, or `none — <what would settle it>` |
| **Status** | `settled`, `open`, `pending-live`, `retired` |
| **Owner** | Required when status is `open` or `pending-live` |

`E0-uhc-live` is reserved for the single 2026-07-30 run on HQPlayer Embedded 6.0.2 / engine 6.0.4 and
is lint-gated on that version string, so no row can quietly promote itself to first-hand.

### Lint checks (each must be proven RED before the ledger satisfies it)

`tests/hqplayer_ledger_lint.rs`:

1. Ledger parses; ≥1 claim row; every row has all seven fields non-empty.
2. Claim IDs are unique, well-formed, and contiguous from `HQP-C-001`.
3. Every `Class` is in the closed vocabulary.
4. Every test name in a `Proof` cell exists in the named test file.
5. Every fixture path in a `Proof` cell names an **existing corpus document with complete
   provenance** — a regular `.xml`/`.html` file under `tests/fixtures/hqplayer/` that loads through
   `corpus::load`, which refuses a missing or misplaced provenance header. A directory or an
   arbitrary file that merely exists is not a fixture proof.
6. The pending-first-hand-confirmation table lists **exactly** the set of fixtures whose provenance
   records `source_chain: read-via-report` — computed from the corpus, not curated.
7. Every `open` / `pending-live` row names an owner issue and a settle condition.
8. Every `E0-uhc-live` row carries the 6.0.2 / engine 6.0.4 / 2026-07-30 provenance.
9. Both retired documents carry a banner pointing at the ledger, and neither restates the retired
   `SetMode`-VALUE claim as current guidance.
10. Every #341 acceptance topic has a ledger anchor (SetMode ambiguity, three numeric domains,
    SetRate, `active_mode`, HTTP/profile/session-auth negatives, apply-then-drop, push status,
    `LibraryPicture` interlude, licensing).

The corpus module lives under `tests/mock_servers/`, so the lint reuses `corpus.rs` for check 6
rather than re-implementing provenance parsing — a second parser would be a second thing to drift.

### Prose changes

- `docs/hqplayer-protocol-reference.md`: retirement banner; the `active_mode` "always use State"
  rule reframed as the open versioned question it is; the three verbatim `hqp-control` C++ excerpts
  (`:136`–`:144`, `:150`–`:160`, `:166`–`:174`) replaced by concise paraphrase with the same
  file/line citations —
  Signalyst's source license is not recorded in this repository, so copied code cannot be justified
  where a paraphrase carries the same interoperability fact.
- `.oh/hqplayer-spec.md`: banner; the `SetMode` VALUE row struck in place, not deleted.

### Explicitly out of scope

No `src/` change. No public API change. No live host contact. No new fixture claiming live evidence.
No renumbering of `filters_sdm.xml` (blocked by #341 comment 5125948432). No `THIRD-PARTY-NOTICES`
file — that is #348's, and creating it here would be maintainer-owned licensing policy written
unilaterally. The known contradictory comment at `src/adapters/hqplayer.rs:2473` ("Status's
active_mode string is unreliable") is recorded as a ledger row with owner #347 rather than edited,
because that file is being actively remediated on the base branch.

---

## Execute
**Updated:** 2026-07-30T21:45Z
**Status:** complete (Stage 2)

### Pre-flight

| Check | State |
|---|---|
| Aim clear | Yes — the four questions in the Aim section |
| Constraints known | The ten in the table above, plus the eight binding adjustments from Reports 1/6 and 2/6 |
| Context loaded | Yes — the twenty fixture provenance headers, the 212-test conformance inventory, ADR 003, both superseded documents, and the live-validation comment |
| Scope bounded | `docs/hqplayer-evidence-ledger.md`, `tests/hqplayer_ledger_lint.rs`, retirement edits in `docs/hqplayer-protocol-reference.md` and `.oh/hqplayer-spec.md`. **Not** `src/`, not the API, not fixtures, not the live host |
| Success criteria | Every #341 acceptance criterion has a claim row; every check proven able to fail |

**Environment constraint, recorded because it is not a property of this branch.** `public/tailwind.css`
is gitignored and generated by `make css`, and this fresh worktree had none, so the library could not
compile and no test could run (`include_str!("../../public/tailwind.css")`,
`src/app/embedded_assets.rs:17`). Resolved by copying the generated artefact from a sibling worktree.
Nothing was committed; `make css` downloads a standalone binary and was not run.

### RED before GREEN

**Test-only RED commit `8874707`** — verified test-only: `git diff 8874707^..8874707 --name-only`
returns `tests/hqplayer_ledger_lint.rs` alone.

```text
test result: FAILED. 5 passed; 18 failed
```

Fourteen failed because the ledger did not exist. **Four bit the documents as they were**, which is
the strongest RED available here:

| Check | Observed failure against the unmodified tree |
|---|---|
| `the_retired_set_mode_value_claim_is_struck_where_it_still_appears` | ``.oh/hqplayer-spec.md still asserts "\| Mode \| VALUE \| VALUE \|" unstruck`` |
| `the_reference_document_no_longer_settles_the_active_mode_question_by_fiat` | `still instructs the reader to always use State's numeric active_mode` |
| `the_superseded_documents_point_at_the_ledger` | both documents listed as not pointing at the ledger |
| `no_verbatim_upstream_source_excerpt_remains_in_the_reference_document` | `left: 3, right: 0` — three verbatim Signalyst excerpts |

The five that passed at RED are `corpus.rs`'s own provenance-parser tests, which this target also
compiles. Disclosed in the lint's module doc: it is the price of having one provenance parser rather
than two.

### Non-vacuity: fifteen mutations, fifteen intended failures

A check that cannot fail is worse than no check — `tests/oneshot_leak_lint.rs:59` is the local example.
Each row below was applied to the finished ledger, the named check was observed failing, and the ledger
was restored (`diff` against the pre-mutation copy: identical).

| # | Mutation | Check that caught it |
|---|---|---|
| M1 | schema marker deleted | `the_ledger_exists_and_declares_its_schema` |
| M2 | one provenance part dropped from a row | `every_claim_row_has_every_required_field` |
| M3 | a claim row deleted, leaving a gap | `claim_ids_are_unique_and_contiguous` (`left: [1,2,3,4,6,…]`) |
| M4 | invented class `E9-probably-fine` | `every_class_and_status_is_in_the_closed_vocabulary` |
| M5 | invented chain `trust-me` | `every_source_chain_and_playback_state_is_in_the_closed_vocabulary` |
| M6 | an `E0` row claiming `playback: unknown` | `an_observed_claim_names_a_real_capture_date_and_playback_state` |
| M7 | a cited test renamed away | `every_cited_test_exists_and_is_not_ignored` |
| M8 | a cited fixture path made wrong | `every_cited_fixture_exists` |
| M9 | proof replaced with `obviously-true` | `every_proof_uses_a_known_form` |
| M10 | a `#332`-only claim marked `settled` | `a_claim_proved_only_by_a_future_live_row_is_not_settled` |
| M11 | an open row's settle condition reworded away | `every_unsettled_claim_names_an_owner_and_what_would_settle_it` |
| M12 | an `E0` row citing a run not in the registry | `first_hand_claims_match_a_recorded_live_run` |
| M13 | a second-hand fixture removed from the pending table | `the_pending_confirmation_table_is_exactly_the_second_hand_corpus` |
| M14 | a topic's claim row renumbered out of existence | `every_required_evidence_topic_maps_to_a_claim` |
| M15 | the `State.active_mode` contradiction quietly marked `settled` | `every_required_evidence_topic_maps_to_a_claim` |

### Three design changes the checks forced, stated plainly

1. **The topic map's claim IDs were provisional and wrong.** The map was written before the ledger
   existed, so it named `HQP-C-010` for `SetRate` and `HQP-C-016` for `active_mode`; the ledger's real
   numbering puts those at 015–022 and 023–024. The map was corrected to the ledger's IDs. Recorded
   because "the test was adjusted to match" deserves the reason: the map is the specification of *which
   row carries which acceptance topic*, and it cannot be written before the rows exist.
2. **A sixth evidence class, `E6-documentary`, exists because a check bit.** Licensing rows were
   `E1-upstream-verified`, and `an_observed_claim_names_a_real_capture_date_and_playback_state`
   correctly refused: `E1` asserts a running daemon was watched, and "HQPTuner's LICENSE names Adam
   Goldsmith" watches no daemon. Seven rows moved to the new class rather than the check being relaxed.
3. **`E1` may say `playback: unknown` only with an explicit anchor admission.** Two upstream
   observations (HQP-C-029 apply-then-drop, HQP-C-038 `LibraryPicture`) have no recorded playback
   state. The alternatives were to guess a value or to downgrade a live observation to a transcription;
   both are worse than saying so where a reader sees it. The admission string is itself checked, so the
   escape cannot be taken silently.

### Two heading-detection defects in the lint, both found by its own failures

`text.split("\n#")` and `line.starts_with('#')` treat `#322` and `#341` — which begin body lines
throughout the ledger — as markdown headings. The first truncated HQP-C-022's anchor before its
**What would settle it** line; the second could have ended a table section early. Both now use an ATX
test (hashes followed by a space).

### Scope held

**Base-relative** (`git diff --name-only origin/feat/issue-322-hqplayer-protocol-conformance...HEAD`)
covers five files: this session record, the ledger, the lint, and the two retirement edits. No `src/`, no
`tests/fixtures/`, no `tests/hqplayer_conformance.rs`, no API surface, no label, no live host.

**`v3`-relative is not the same number and must not be quoted as one.** Because this branch is stacked,
`git diff origin/v3...HEAD` additionally carries every #322 change — ADR 003, `hqplayer_conformance.rs`,
`tier1.rs` and the rest of PR #337's 32 files. `sg review` pointed this out, and it is exactly why the
merge order in the PR body is load-bearing rather than a formality. Every scope figure in this session
file and in the PR reports is **base-relative** unless it says otherwise.

**Two edits deliberately not made**, both recorded as ledger rows instead:

- `src/adapters/hqplayer.rs:2473`'s "Status's active_mode string is unreliable" comment (HQP-C-026,
  owner #347) — that file is under active CodeRabbit remediation on the base branch, and a comment-only
  edit from a stacked branch would conflict for no behavioural gain.
- The eight fixture headers still carrying prose inside the closed-vocabulary `source_chain` field
  (HQP-C-057, owner #337) — base-branch files, same reason.

### Stage 2 remediation — independent review at `5d8c607`, three items, all valid

Received mid-stage, all three verified against the evidence before acting.

**R1 — the `E1` playback relaxation is a contract correction and must be documented as one.** Valid.
The stage-1 rule was "classes `E0` and `E1` may not say `unknown`"; stage 2 relaxed it for `E1` with an
anchor admission and left the old rule standing in two places — `OBSERVED_CLASSES`' doc comment and the
ledger's provenance bullet. Both now state the corrected contract and why. **`E0` stays strict**, which
was the other half of the instruction. Dispositioned in Reports 3/6 and 4/6 as a contract change, not as
"making GREEN".

**R2 — a licensing row must not be classed as a daemon observation.** Already done, and the check is
what found it: `an_observed_claim_names_a_real_capture_date_and_playback_state` refused HQP-C-053 as
`E1-upstream-verified` with `playback: n/a`, and seven rows moved to the new `E6-documentary` class
rather than the check being weakened. Recorded here because "already done" is only credible with the
mechanism named.

**R3 — HQP-C-023's playback state was factually wrong.** Valid and corrected: `idle` → `active`, source
re-pointed to `.oh/issue-322-…:1549-1552`, which records this probe as `playback active`. My `idle` was
inferred from the aggregate "upstream probes ran stopped" caveat, and the ledger's provenance bullet now
says explicitly that the aggregate caveat is not a per-probe record.

**What R3 exposed, which was not in the review.** Base-branch commit `ab18874` removed a "mid-playback"
qualifier from the reference document and `model.rs` **because "the ledger (HQP-C-023) records this
upstream probe as idle."** It used this ledger's inference as evidence against the session file's
contemporaneous record — circular, and now recorded as **HQP-C-061** (owner #337) rather than fixed from
here, because those are base-branch files. Two documents agreeing because one copied the other is
precisely what the `chain` field exists to expose, and it happened inside this program within a day.

### The base branch moved under this one, exactly as the stage-1 dissent predicted

Three commits landed on `feat/issue-322-hqplayer-protocol-conformance` while stage 2 was being written
(`cba1731`, `e406866`, `ab18874`), two of them editing `docs/hqplayer-protocol-reference.md` — the one
file this branch also edits. GitHub reported the PR `CONFLICTING`, and `git merge-tree` confirmed one
content conflict in that file.

**Resolved forward-only, with no merge commit and no force-push.** #322 had independently reframed the
same `active_mode` prose, and its wording is better than mine: it distinguishes the explicit PCM/SDM
case from `[source]`, cites ledger row HQP-C-024, and points at `ActiveModeReporting`. So this branch
now carries **#322's exact text** for both overlapping hunks, and keeps only its non-overlapping edits
(the retirement banner and the three paraphrases). A three-way merge sees identical changes on the
overlapping hunks: `git merge-tree --write-tree HEAD origin/feat/issue-322-…` reports no conflict.

One check had to adapt rather than the document: `the_reference_document_no_longer_settles_the_active_mode_question_by_fiat`
demanded a `[retired #341]` marker, which #322's better wording does not carry. It now accepts either
that marker **or** a citation of `HQP-C-024`, because what the check forbids is the *unqualified*
imperative. Insisting on this branch's own marker would have made the check reject the outcome it exists
to produce.

### CodeRabbit remediation at `3b90fe7` — two lint-integrity gaps, both valid, both false passes

CodeRabbit reviewed the exact head and found **two checks that could not catch what they claimed to
check**. Both were verified by mutation before being fixed: the mutation *passed*, which is the defect
shape for a lint — a false absence rather than a false alarm.

| # | Gap | Proof it was real (before) | Proof it is closed (after) |
|---|---|---|---|
| CR1 (P2) | `first_hand_claims_match_a_recorded_live_run` joined the registry row and searched it for the daemon and the date, so it never compared **playback state**. An `E0` row could contradict the registry outright | M16 — flipped one `E0` row's `idle` to `active` while the registry still said `idle`: **`test … ok`** | M16 now fails: *"no registry row records that exact combination"* |
| CR2 (P3) | `every_cited_fixture_exists` only asked whether the path existed, so `fixture:README.md`, an unrelated source file, or a directory satisfied a fixture proof | M17 — repointed a fixture proof at `README.md`: **`test … ok`** | M17 and M18 (a directory) both fail |

**CR1 matters more than its severity suggests, and for a specific reason.** The one substantive error
this ledger has made was a wrong **playback state** (HQP-C-023), and this was the check standing next to
that exact field with its eyes closed. It is now positional and exact against the registry's
`Edition / version`, `Date` and `Playback` columns.

**CR2's fix is stronger than the report asked for.** Rather than checking the extension and directory
only, a `fixture:` proof now **loads through `corpus::load`**, which panics unless the file opens with a
complete provenance header. So a fixture proof cannot cite a document whose own evidence metadata is
missing or malformed. The check was renamed to
`every_cited_fixture_is_a_corpus_document_that_carries_provenance`, because "exists" no longer describes
what it enforces — the mutation table's M8 row refers to the older name.

**Check count is unchanged at 23**: CR2 renamed one check rather than adding one, and CR1 strengthened an
existing one. Seventeen mutations are now on record (M1–M18, less the retired M8 wording), and **two of
them were false passes found by an external reviewer, not by this session.** That is the second time in
this PR that an outside reader found what the internal passes missed — the first being HQP-C-023 — and
it is the most useful thing this record can say about the value of the internal passes alone.

### The base branch moved a second time — and this time it moved *because of* this ledger

Three more commits landed on `feat/issue-issue-322` between the CodeRabbit remediation and the ship gate.
Two of them cite this ledger's claim IDs:

| Base commit | What it did |
|---|---|
| `ff8765b` | *"restore the supported `mid-playback` qualifier for the `Status.active_mode` `[source]` echo"* — **resolves HQP-C-061.** The circular de-qualification is reversed and the qualifier the `97dcc59` pass wrote is back |
| `76b011c` | *"collapse the remaining eight `source_chain` fields and pin the closed vocabulary exactly (#341 HQP-C-057)"* — **remediates HQP-C-057**, citing the row's ID in its subject line |
| `2b00fde` | CodeRabbit remediation on that branch: stale docs and a hardcoded `.xml` fixture lookup |

Both rows are now `retired` here, with the outcome recorded in their anchors. Neither was fixed from this
branch, which was the disposition Report 3/6 argued for, and the result is the clearest available evidence
for it: **recording a finding with an owner and a claim ID got both fixed on the branch that owned the
files, within the hour, without a cross-branch edit.**

It is also direct evidence against the Stage-1 dissent's second pre-mortem — *"nobody opens it"*. The
adoption signal it named (a `HQP-C-` citation appearing outside this PR) arrived before the ship gate, in
a commit subject line. That does not make the ledger permanently useful; it does mean the failure mode is
not the one to worry about first.

**The treadmill P3-6 named is real, though.** The same file conflicted a second time and was re-synced the
same way: this branch adopts #322's current text for the overlapping hunks and keeps only the banner and
the paraphrases. `git merge-tree --write-tree` clean again. That is now **two** resolutions in one session
for one file, and the structural fix remains merging #337 rather than anything available here.

### CodeRabbit's second pass at `799c4c0` — a third false pass, and a state correction I had wrong

**CR3 (P2) — `every_unsettled_claim_names_an_owner_and_what_would_settle_it` accepted a label.** The
check asked whether the anchor *contained* the words "What would settle it", so an anchor whose
designated line was emptied after the colon, or which mentioned the phrase inside other prose, passed.
Proven both ways before fixing:

| # | Mutation | Before | After |
|---|---|---|---|
| M19 | the settle paragraph reduced to `**What would settle it:**` | `test … ok` | fails: *"which is a label rather than an acquisition plan"* |
| M20 | the phrase demoted to mid-sentence prose (`Somebody should decide What would settle it: …`) | would have passed | fails: *"no line beginning `**What would settle it`"* |
| M21 | `**What would settle it:** TBD` | would have passed | fails on the four-word floor |

The check now parses the **designated line** — a line that *begins* with the phrase — continues it across
following non-blank lines, and requires at least 20 characters and four words. The floor is deliberately
low: it is meant to catch an empty cell or a `TBD`, not to legislate prose.

**CodeRabbit's "ideally also" was not implemented, with a reason.** It suggested requiring the settle text
to name the row's owner issue. Several anchors legitimately name a *different* issue in their plan — HQP-C-022's
cites #322's acceptance wording while the row is owned by #332 — so that rule would reject correct rows.
The owner is already checked as a separate condition on the row itself.

**A state correction against me, and it was right.** My review-request comment claimed `MERGEABLE`;
CodeRabbit observed GitHub reported `CONFLICTING`/`DIRTY` at `799c4c0`. Both were true at different
moments — the base branch moved between my check and its read — and the honest version is that a
mergeability claim is only valid at the SHA *and the minute* it was taken. Every such claim in the Stage 3
reports names the head it was measured at.

**Three false passes across two CodeRabbit passes, all in checks written by the author of the artefact they
check.** That is now the most useful sentence in this record: the mutation table proves what its author
thought to break, and an external reviewer found what he did not.

## Ship
**Updated:** 2026-07-30T22:20Z
**Status:** staged — **not deployed, not delivered, not merged**

### Delivery path, named honestly

`local` → **draft PR #364 (here)** → CodeRabbit + human review → #337 merges to `v3` first → this PR
retargets to `v3` → merge → tagged release build → binaries / Docker / packages → user install.

**This PR is at step two of eight.** A draft stacked PR is *staged*. Nothing is deployed, no user has
anything, and no deployment checkbox is ticked below. The content is documentation and one test target,
so even after merge no user-visible behaviour changes — the audience is contributors.

### Delivery-path tax

| Friction | Cost, measured |
|---|---|
| **Merge order** | Hard blocker. #337 must merge first; this PR cites tests and fixtures that exist only on its branch |
| **Base-branch churn** | Six commits landed on the base during this session, three touching the one file this PR also edits. Two conflicts, two forward-only resolutions, and finally the authorised forward-merge below |
| **Base CI lint** | #339 unmerged; the base's Rust 1.97 `unnecessary_sort_by` warning keeps the required Lint check red, independently of this PR |
| **External review latency** | CodeRabbit reviewed twice and found three false passes; its included-review quota briefly rate-limited the third request |
| **Human approval** | Required, and correctly not automatable |

### Forward-merge of the base at `76b011c`

Authorised explicitly. **Not a rebase, not a force-push** — history is append-only, and the project
squash-merges anyway. Done because local `HEAD` lacked the base's newest conformance changes, so a
full-suite run here was not an integrated-stack check.

One conflict, `docs/hqplayer-protocol-reference.md`, resolved **in the base's favour for every
overlapping hunk**, so the restored `mid-playback` qualifier from `ff8765b` survives — this branch must
not regress evidence it argued for. This branch's only remaining delta in that file is the retirement
banner and the three paraphrases.

### Verification at the merged head `7ca50be661b5fd685c1120146696db31d65d7b47`

Every figure re-run at this exact SHA. **No HQPlayer daemon was contacted.**

| Command | Result |
|---|---|
| `cargo test --test hqplayer_ledger_lint` | **23 passed; 0 failed** |
| `cargo test --test hqplayer_conformance` | **215 passed; 0 failed** — including the base's new `source_chain_is_exactly_a_closed_vocabulary_token` |
| `cargo test --test api_contract` | **2 passed** — no route or payload change |
| `cargo test --workspace --no-fail-fast` | **exit 0 — 575 passed; 0 failed; 12 ignored** (integrated stack) |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy -- -D warnings` (CI's invocation) | exit 0, clean |
| `git diff --check` base…HEAD | clean |
| base-relative diff | **5 files, +2070 / −39**; no `src/`, no API-surface, no web-composition path |
| `dx build` | **not required** — no `src/app/`, `assets/`, `public/` or `Dioxus.toml` path in the delta |
| local `HEAD` vs `origin/…` vs worktree | equal; worktree clean |
| live tier 1 / tier 2 | **NOT RUN** — out of scope for #341 by instruction |

**Two full-suite runs at the pre-merge head `799c4c0` both exited 0**, and the merged head's run exited
0. Per ADR 003 that is a **rate, not a verdict**: the documented `/hqp/discover` multicast and LMS
concurrency flake families did not fire in any of the three, which is evidence about this sandbox on this
evening and nothing more.

### CodeRabbit's third pass at `5c97f6c` — a fourth false pass, and one hardening it could not confirm

**CR4 (P2) — a malformed owner token satisfied the owner check.** Ownership was accepted when the cell
*contained* a `#` and *contained* a digit, so `#0`, `#abc1` and `#-1` all passed. None is an issue anyone
can open, and an owner nobody can reach is the same as no owner — which is the one thing that check
exists to forbid.

| # | Owner token on an unresolved row | Before | After |
|---|---|---|---|
| M22 | `#0` | `test … ok` | fails |
| M23 | `#abc1` | `test … ok` | fails |
| M24 | `#-1` | `test … ok` | fails |
| M25 | `#332` (the real value) | ok | **ok** — the fix rejects malformed tokens, not valid ones |

`is_issue_reference` now requires `#` followed by a non-zero decimal number, with markdown decoration
and backticks stripped first. `not-an-issue` already failed before the fix, which is why the earlier
proof needed the three tokens above rather than an obviously-wrong one.

**The hardening CodeRabbit raised but did not confirm, done anyway.** `#[cfg(test)]` contains the
substring `test`, so the cited-test check's attribute scan could treat a plain helper directly beneath one
as a test function — a false pass for a citation naming no test at all. It is now matched narrowly
(`#[test]`, `#[test(…)]`, `…::test`), and `#[ignore]` likewise. **No instance could be constructed**
without editing a base-branch file, so unlike CR1–CR4 this one is argued rather than demonstrated, and it
is recorded as such: one line of cost against a silent failure.

### The count that matters most in this record

**Four false passes, across three CodeRabbit passes, in checks written by the author of the artefact they
check.** Plus one factual ledger error found by a human reviewer. The internal 25-mutation table caught
everything its author thought to break; every one of the five things it missed was found by someone else.
Anyone reading the mutation table as evidence that the ledger is *correct* is making exactly the mistake
this ledger exists to prevent — it is evidence about **schema**, and nothing more.

### Independent verification at `6e84172b85cb3458f4abd9d4bf9db885cc71c700`

**Codex, independently of this session:** `cargo test --test hqplayer_ledger_lint` → **23 passed; 0 failed**
at that exact SHA. **No HQPlayer appliance was contacted.** Recorded as reported to this session, not as
a run this session performed — the distinction is the same `chain` discipline the ledger applies to every
other claim.

**Why this note exists at all.** GitHub's `refs/pull/364/head` and PR API stayed at `5c97f6c7…` after the
`6e84172b…` push while the branch ref had already advanced, so the PR object could not be re-anchored to a
head it did not yet expose. This append-only note is the smallest truthful commit that retriggers PR
synchronisation: no amend, no force-push, no empty commit.

### CodeRabbit's fourth pass at `65b1d4e` — the fifth false pass, and it exposed a real defect in the ledger

**CR5 (P2) — an unresolved row's settle condition could be satisfied by a *hijacked* anchor.**
`anchor_section` took the first heading whose text *contained* the claim ID, so a heading reading
`### Notes on HQP-C-0240`, placed before the real `HQP-C-024` anchor and carrying an unrelated plan,
matched it — and the real anchor's plan could then be deleted with the check still green. The topic-map
check cannot see this either, because the claim **row** is untouched.

| # | Mutation | Before | After |
|---|---|---|---|
| M26 | decoy heading `### Notes on HQP-C-0240` inserted before the real anchor, real plan deleted | `test … ok` | fails: *"HQP-C-024 has no prose anchor section"* |
| M27 | `####### not a heading in CommonMark` inserted inside an anchor | (would have cut the section short) | passes — a seven-hash line is no longer treated as a boundary |

`heading_declares` now requires the ID to **begin** the heading's content and to end at a boundary that
cannot extend an identifier, so `HQP-C-0240` no longer matches `HQP-C-024` while `HQP-C-024 — …` and
`HQP-C-023's …` still do. `is_heading` is limited to one-to-six hashes, per CommonMark.

**The fix immediately failed on real content, which is the part worth recording.** With the stricter rule
in place, `HQP-C-056` had **no anchor of its own**: it had been sharing a combined
`### HQP-C-053 / HQP-C-056` heading, and the old `contains` rule let a heading that declares *another*
row satisfy its settle condition. So CR5 was not hypothetical here — the ledger already contained one
instance. The rows are now split, each with its own acquisition plan, which they needed anyway: #348
landing the notices file is not the same action as #348 stating the correspondence boundary.

### Five false passes, and the dissent that predicted this one

Report 6/6 said, before this pass ran: *"the four false passes were not the last four… a fourth external
pass would likely find a fifth. Stopping here is a budget decision, not a completeness claim."* A fourth
pass ran and found a fifth. **That prediction is now evidence rather than caution**, and it applies
unchanged to a sixth pass: the rate has not fallen, so any decision to stop reviewing this file is a
budget decision.

**Tally: five false passes and one factual ledger error. All six were found by someone other than the
author of the checks** — five by CodeRabbit, one by a human reviewer. The internal 27-mutation table
caught precisely what its author thought to break, which is the whole reason the ledger says out loud
that its checks constrain **schema, not truth**.

### CodeRabbit's fifth pass at `2c74246` — the sixth false pass, in the same check for the third time

**CR6 (P2) — a settle condition could carry a label that merely *parses* like the designated one.**
`settle_condition` accepted any line *beginning with* `**What would settle it` and then took the text
after any later colon, so `**What would settle it is unrelated prose:** …` produced a long, four-word,
schema-valid "plan" that names no acquisition action. Proven (**M28**: `test … ok` before, fails after).
The marker is now matched **exactly** — `SETTLE_MARKER` is a constant and the line must start with it.

**This is the third false pass in one check**, which is the more interesting fact than the fix. CR3
required the designated line to exist; CR5 stopped a *different heading* from supplying it; CR6 stops a
*variant label* from supplying it. Each fix was correct and each left a neighbouring hole — the pattern of
a text scan standing in for a parser, which ADR 003 already names as the standing weakness of this
repository's lint family.

### Six false passes, and what the rate means

| Pass | Finding | Where |
|---|---|---|
| 1 | CR1 playback state never compared to the live-run registry | `first_hand_claims_match_a_recorded_live_run` |
| 1 | CR2 any existing path satisfied a `fixture:` proof | fixture-proof check |
| 2 | CR3 the settle phrase's mere presence counted | settle check |
| 3 | CR4 malformed owner tokens (`#0`, `#abc1`, `#-1`) passed | owner check |
| 4 | CR5 an anchor could be hijacked by ID prefix — **and one real instance existed** | anchor lookup |
| 5 | CR6 a variant label parsed as the designated one | settle check |

Plus one **factual** ledger error (HQP-C-023's playback state) found by a human reviewer.

**Seven defects, none found by the author's own 28-mutation table.** Five external passes, and the rate has
not fallen: every pass has found at least one. Report 6/6 predicted the fifth before it existed and called
stopping *a budget decision, not a completeness claim*; that framing is now supported by five data points
and applies unchanged to a sixth pass. **Anyone reading a green `hqplayer_ledger_lint` as evidence that
the ledger is correct is making the exact inference this ledger exists to prevent.**

### CodeRabbit's sixth pass at `0683760` — `CHANGES_REQUESTED`, nine actionable items, all valid

Review [4823720730](https://github.com/open-horizon-labs/unified-hifi-control/pull/364#pullrequestreview-4823720730). **Stopping at five passes was overridden, and the sixth pass found three more
false passes plus one real bug** — vindicating the dissent's "budget decision, not a completeness claim"
rather than the decision to stop. Every item was verified against the code before being acted on.

#### Three false passes and one shape bug, each RED before GREEN

| # | Defect | RED evidence | Now pinned by |
|---|---|---|---|
| F7 | `cells` re-added the leading pipe when a row lacked a trailing one, shifting every field by one — so a shape error surfaced as *"malformed claim ID"* for a row whose ID was fine | `left: ["", "a", "b", "c"]` | `cells_does_not_reinstate_the_leading_pipe_when_a_row_lacks_a_trailing_one` |
| F8 | `#[ignore="reason"]` **without a space** read as *not ignored*, so a citation to an ignored test would have counted as proof | `left: Some(false)` for `unspaced_ignore` | `an_ignored_test_is_detected_with_or_without_spacing` |
| F9 | a citation naming a nonexistent proof file **panicked** inside the file reader, aborting the aggregated diagnostics with the ledger's own "this file is #341's deliverable" message | probe: `catch_unwind` caught the panic | `a_missing_proof_file_is_reported_rather_than_panicking`, plus M30 which now reports *"HQP-C-052 cites tests/typo_does_not_exist.rs::whatever, but … is not a readable file"* |
| dup | the strikethrough check inspected only the **first** occurrence of each retired claim | M29: a second unstruck *"resolves to VALUE"* passed | `match_indices`, re-proven by M29 |

**Seven focused controls replaced seven throwaway mutations.** Every false pass so far has been in a
*helper* — `contains` where a prefix was meant, a prefix where an exact marker was meant, a fallback that
re-added a delimiter — and a mutation proves a helper's boundary exactly once, then is reverted. The new
tests call the helpers directly with the exploit strings as inputs, so CR5's prefix collision, CR6's
malformed marker, the seven-hash pseudo-heading, the owner-token forms and both `#[ignore]` spellings are
**permanently** pinned. Check count: **23 → 31**.

#### Five documentation findings, all valid

| # | Finding | Fix |
|---|---|---|
| F1 | the `.oh` contract still described the fixture check as *"exists on disk"* after it had been strengthened to load through `corpus::load` | contract text now states the real requirement: an existing corpus document **with complete provenance** |
| F2 | a fenced block with no language (`MD040`) | `text` |
| F3 | both banners said *"which executable test proves it"* — but **19 rows have no executable proof** | *"which proof pointer or acquisition plan supports it"*, and the ledger banner now says the 19 out loud |
| F4 | HQP-C-002 claimed *every* `Set*` speaks a list index — **volume does not**, it is absolute dB; and HQP-C-004 said clients exchange *"names and Hz only"*, which excludes the dB it documents in HQP-C-040 | narrowed to the **enumerated** setters with volume explicitly excluded; the client domain now names its three real value kinds |
| F6 | a blank line between two adjacent blockquotes (`MD028`) | a thematic break, which separates them without pulling base-branch content into this banner |

#### F5 — the finding that improved the ledger's structure

HQP-C-051 asserted two things — the daemon accepts `SetJunkFilter`, **and** UHC's adapter exposes no
setter — behind one conformance test that only supports the first. **Split**, per the review's own "or
split honestly":

- **HQP-C-051** keeps the wire fact, `settled`, proven by the conformance test.
- **HQP-C-062** carries the adapter-surface claim, `open`, owner #329 — and is now **executable**:
  `the_adapter_exposes_no_junk_filter_setter` inspects `src/adapters/hqplayer.rs` — a text scan when this
  entry was written, **structurally parsed with `syn` one commit later** — so if #329 adds the setter
  the check fails and the row must be updated rather than outliving its own truth.

That is the ledger's discipline applied to itself: one claim, one proof.

### Running tally after six external passes

**Nine false passes and one factual error, none of them found by this session's own mutation table.**

| Pass | Findings |
|---|---|
| 1 | CR1 registry playback comparison · CR2 fixture-path validation |
| 2 | CR3 settle-phrase presence |
| 3 | CR4 owner tokens |
| 4 | CR5 anchor hijack — **one real instance already in the ledger** |
| 5 | CR6 variant settle marker |
| 6 | F7 `cells` shape bug · F8 unspaced `#[ignore]` · F9 panicking proof-file read · dup strikethrough first-occurrence-only |

Plus HQP-C-023's playback state, found by a human reviewer.

**The rate still has not fallen**, and this pass is the second consecutive one to find a defect in a
*helper* rather than a check. **A seventh pass is requested rather than assumed unnecessary.** The
prediction in Report 6/6 — that stopping is a budget decision — is now supported six times over, and the
one time it was acted on as if it were a completeness claim, the next pass found four more things.

### Independent Codex review at `ff6ae09` — two findings, both valid, both structural

#### 1. HQP-C-004 was factually overbroad and could not be `settled` as written

It said clients *"never"* exchange list indices or enum IDs. **Two evidenced paths say otherwise**, and
both were verified in the code before the row was touched:

- `HqpSettingRequest { name: String, value: u32 }` with the handler's own comment *"Legacy endpoint -
  convert numeric value to string for name-based lookups"* (`src/api/mod.rs:825-836`). A client sending
  `3` gets `"3"` handed to `set_mode`.
- The adapter's resolvers try `parse::<u32>()` and treat the result as a **direct list index** when the
  name is not in the cached enumeration (`resolve_mode_index:2081`, `resolve_filter_index:2186`,
  `resolve_shaper_index:2247`).

#347 owns both halves in its own words: *"Raw integer-string fallback is removed from production control
paths"* and *"Preserve the existing numeric legacy HTTP request contract … but keep that numeric value at
the compatibility boundary."*

**Fixed by splitting intent from fact**, which is the ledger's own discipline: HQP-C-004 is now explicitly
the **design rule** and points at **HQP-C-063**, a new `open` row owned by #347 carrying the current
shipped behaviour with the exact file and line citations. The stronger future state is **not** recorded as
settled.

**Why HQP-C-063 has no executable proof, deliberately.** A check asserting the fallback *still exists*
would fail the moment #347 removed it — a tripwire that fires on the happy path. HQP-C-062's check is the
opposite shape: it fires when a *capability is added*. That asymmetry is why one is executable and the
other is a plan with an owner.

#### 2. `the_adapter_exposes_no_junk_filter_setter` was itself a text scan

Added one commit earlier to *close* a false-pass finding, and false-pass-prone in the same way. RED:
`declares_fn` missed `pub  async  fn   set_junk_filter (i: u32)` — **ordinary `rustfmt` spacing**, not an
exotic escape — and would also miss a signature broken between `fn` and the name.

Replaced with a **structural** check. `syn` is already a direct dev-dependency, so signatures are *visited*
rather than matched: `visit_signature` covers `ItemFn`, `ImplItemFn` and `TraitItemFn` alike, because every
one carries a `syn::Signature`. `a_junk_filter_setter_is_detected_however_it_is_written` pins seven
declaration forms — free fn, `pub`, odd spacing, a newline between `fn` and the name, an inherent impl
method, a trait method signature, a trait impl — and six non-declarations: a line comment, a doc comment, a
string literal, a call expression, a `let` binding, and a similarly-named different function.

**A three-step parse ladder** makes the negative controls meaningful rather than accidentally-true: as a
whole file, then with a trailing item so a dangling doc comment has something to attach to, then as
statements inside a probe body so a call or a `let` is analysed structurally instead of dismissed as
unparseable. **A parse failure panics** rather than reporting an absence, because reading an unparseable
adapter as "no setter" is the exact false negative the check exists to prevent.

**Residual limit, stated:** a setter generated by a macro is invisible, since nothing is expanded.

#### What this pair says about the pattern

**Ten false-pass-class defects and one factual error, across six external CodeRabbit passes and two
independent reviews — none found by this session's own controls.** The second finding above is the sharpest
instance: a check written *to close* a false-pass finding was itself false-pass-prone within one commit,
and the fix was to stop scanning text and start parsing. That is the same lesson ADR 003 records about this
repository's lint family, arrived at from a different direction.

Checks: **31 → 32**. Claim rows: **62 → 63**.

### CodeRabbit's seventh pass at `e2739d8` — an eleventh false pass, in the last text scan I had named

**CR7 (P2) — the retired-claim guard accepted an unstruck claim when *anything else* on the line was
struck.** The check asked whether the containing line contains `~~`, so
`| Mode | VALUE | VALUE | ~~historical note~~ …` left the retired claim readable as current guidance and
passed. **M31, CodeRabbit's exact mutation, passed before the fix and fails after it** — now reporting
*"still asserts … outside any `~~…~~` span … Strikethrough elsewhere on the line does not retire this
claim."*

`phrase_is_struck` computes the byte ranges *inside* each `~~…~~` span and requires the matched phrase to
lie wholly within one. An unpaired trailing `~~` opens no span, so a half-written strikethrough retires
nothing. Pinned by `a_phrase_counts_as_struck_only_when_it_is_inside_the_span`.

**This was one of the two helpers named for CodeRabbit's attention** in the previous review request. Naming
them did not make them safe; an external pass attacking them did.

**One control of mine was wrong and the suite caught it.** The first version asserted that a *partially*
struck phrase is not struck — a case that **cannot occur**: a span boundary inside the phrase inserts `~~`
into it, so the contiguous needle no longer matches and the loop never reaches the span check. The
assertion constructed a line its own search could not find. Replaced with a reachable case (the phrase
between two spans) plus a comment recording why the unreachable one is absent, rather than deleting it
silently.

### Eleven false-pass-class defects, one factual error, and the pattern behind all of them

| Source | Count | Mechanism |
|---|---|---|
| CodeRabbit, seven passes | **9** (CR1–CR7 plus the duplicate and F7–F9 group) | eight of them a text scan standing in for a parser |
| Independent Codex reviews, two | **2** | one factual overbreadth, one text scan |
| This session's own controls | **0** | — |

**Every external pass has found at least one defect. Seven for seven.** The one time this was treated as
converging — stopping after five — the next pass found four more. The mechanism is now unambiguous: **eight
of eleven were a text scan where a parser was needed**, and the only helper converted to structural parsing
(`declares_fn`, via `syn`) is the only one no later pass has reopened. The markdown helpers cannot take the
same treatment without a markdown parser, which this repository does not have as a dependency — so they are
pinned by controls instead, and that is a weaker guarantee, stated rather than glossed.

Checks: **32 → 33**.

### Independent audit: three descriptions went stale the moment the implementation improved

The structural rewrite replaced a text scan with a `syn` visitor, and **three places still described the
old implementation**. An independent audit caught all three:

| Site | Said | Now says |
|---|---|---|
| `tests/hqplayer_ledger_lint.rs`, directly above the check | *"a text scan of the adapter for a junk-filter setter"* | a **structural inspection** of the adapter's Rust declarations — `syn` parses the file and every function and method signature is visited |
| `docs/hqplayer-evidence-ledger.md`, HQP-C-062's anchor | *"scans the adapter"* | **parses** the adapter with `syn` and visits every signature |
| `.oh/hqplayer-evidence-ledger.md`, the sixth-pass entry | *"scans `src/adapters/hqplayer.rs`"* | inspects it — *"a text scan when this entry was written, structurally parsed with `syn` one commit later"* |

**Swept for the rest.** Every remaining "text scan" mention describes either the implementation that was
*replaced* (`declares_fn`'s own doc, the control that exists to pin the escape) or the general pattern
behind eight of the eleven defects. Both are accurate and both stay.

**Why this is worth a commit of its own.** A ledger whose subject is provenance cannot afford a description
that no longer matches its own mechanism — a reader deciding how much to trust HQP-C-062 would have been
told it was the weaker thing. This is the fourth finding in a row that turned on *stale or overbroad
description* rather than on logic: F1 (the contract text), F3 (both banners), Codex 1 (HQP-C-004), and now
this. Recorded as a pattern in its own right, because it is a different failure mode from the false passes
and needs a different guard: the false passes were found by attacking the checks, and these by reading the
prose against the code.

### CodeRabbit's eighth pass at `f0bc908` — a claim/proof mismatch, and the check it forced

**CR8 (P2) — HQP-C-051's cited test did not prove its claim.** The row said `SetJunkFilter` round-tripped
`filter_junk 0→1→0` with `result="OK"` on L1, and cited
`the_junk_filter_is_read_as_a_list_index_not_a_boolean`. Verified against the test at
`tests/hqplayer_conformance.rs:998`: it mutates the fake's state externally and asserts the adapter *reads*
`filter_junk == 2`. **It never sends the command, never checks a `result`, and never exercises a
transition.** No conformance test drives `SetJunkFilter` at all — the fake implements it (`model.rs:994`)
and nothing calls it.

**Fixed by keeping the live evidence and dropping the false citation.** HQP-C-051 now carries an explicit
`none:` proof naming the missing expectation, and **the lint refuses to let it be `settled`** on that basis
— verified by re-marking it `settled` and watching
`a_claim_proved_only_by_a_future_live_row_is_not_settled` fail. The daemon-side fact is observed; the
client-side coverage is absent; the two are now visibly different things.

**This was the fifth finding of one class — wording outrunning its cited proof — and the first found where I
had asked them to look.** So the response was not another hand-fix.

#### Check 34: every command a claim names must be exercised by one of its own proofs

A hand sweep of all 63 rows found the same shape in five more. A class found five times by readers is a
class that should not need a reader, so the sweep became a check: for each claim, extract every `Set*`
command it names, gather the bodies of its own cited tests and fixtures, and require each command to appear
in at least one. Rows whose only proofs are `none:`/`#332:` are skipped — they have no proof body, and the
not-settled rule already constrains them. An explicit `Commands evidenced elsewhere: …` marker exempts a
command **only by naming where its evidence lives**, in the same cell a reader is looking at.

It flagged six items across three rows on its first run. Two were citation gaps with proofs available
(HQP-C-002 now also cites the mode and rate expectations that exist). Two were legitimate
evidence-elsewhere cases, now marked (HQP-C-029's proofs exercise the reply-loss shape with `Next` and
`VolumeMute`, not `SetMode`; HQP-C-050's test covers the read side only). **And two were a real gap nobody
had recorded.**

#### HQP-C-064 — the check found a coverage gap, not a citation slip

Chasing `SetShaping` produced a fact worth having: **no test in `tests/hqplayer_conformance.rs` calls
`adapter.set_shaper`.** Verified directly — `set_mode` and `set_rate` are each driven by four expectations,
`set_shaper` by none. The shaper setter's behaviour rests entirely on L1 (`shaper 24→0→24`, `result="OK"`,
readback-verified against a real daemon). Good evidence, and **not a regression pin**: a client change
could break `set_shaper` today and the suite would stay green.

Recorded as **HQP-C-064**, `open`, owner #329, with the acquisition plan. That is the ledger doing the job
it was built for — and it took an external reviewer's finding to build the check that found it.

### Twelve defects, and what the shape of them says

| Source | Count |
|---|---|
| CodeRabbit, **eight** passes | **10** |
| Independent Codex reviews, **three** | **3** (one factual, one text scan, one stale-description sweep) |
| This session's own controls, before external prompting | **0** |
| This session's own **check 34**, built in response to CR8 | **1 previously unrecorded coverage gap** |

**Eight for eight: every external pass has found something.** The mechanisms, now clear enough to name:
**eight defects were a text scan standing in for a parser**, and **five were wording that outran its cited
proof**. The first mechanism yields to structural parsing — `declares_fn` via `syn` is the only helper no
later pass reopened. The second now has check 34, which is the first guard in this file aimed at the *claim*
rather than at the *form*. Checks: **33 → 34**. Claim rows: **63 → 64**.

### Independent diff check: a status stated twice drifted, and check 34 scrutinised

#### The drift, and a check for the class

**HQP-C-062's anchor still read *"(HQP-C-051, settled)"*** after HQP-C-051 was moved to `open` in the same
commit. Prose repeating a status is always the copy that goes stale.

**Check 35** makes the class fail: any prose reference of the form `HQP-C-0NN, <status>` must agree with that
row's status cell. The table is authoritative; prose may point at it but not restate it wrongly. **RED was
the real tree** — the check's first run reported *"line 466: prose says HQP-C-051 is `settled` but its row
says `open`"*, and M33 reproduces it.

#### Check 34, scrutinised honestly — it was sound in shape and lexical in three places

Asked whether the command matching is semantically sound rather than merely lexical. **It was not, in three
specific ways. Two are now closed and one is stated.**

| # | Hole | Status |
|---|---|---|
| 1 | **A comment counted.** A test that merely *mentioned* `SetFilter` in a comment credited the claim. Comment lines are now excluded before matching, pinned by `a_command_named_only_in_a_comment_is_not_exercised` | **Closed** |
| 2 | **Only the conformance file was read.** A row citing a test in any other file contributed no proof body, and an empty body **skips** the row — a vacuous pass. Proof files are now resolved per citation, as the cited-test check already did | **Closed** |
| 3 | **The adapter-method name was derived, not known.** `camel_to_snake` turned `SetShaping` into `set_shaping` and `SetJunkFilter` into `set_junkfilter` — **neither is the real method**. A future shaper expectation calling `set_shaper` would not have been credited, and the check would have demanded an exemption for evidence that existed. Replaced by an explicit table, and the derivation helper deleted as dead | **Closed** |
| 4 | **A command in a string literal still counts.** Excluding string literals needs a Rust parse of a fragment that is not a whole item. The failure direction is over-crediting a test, never under-crediting one, so it is asserted as a limit in the control rather than hidden | **Stated** |
| 5 | **A claim naming no `Set*` token is not checked at all.** The rule is keyed on command names; a claim phrased without one escapes it entirely | **Stated** |

**Non-vacuity re-proven after the hardening**, not assumed: M32 removes one exemption and the check names the
row, the command and the method it looked for.

**One defect of my own, caught before commit:** the failure message's format arguments were transposed, so it
read *"set_shaper names SetShaping … (looked for SetShaping or HQP-C-002)"*. A diagnostic that names the
wrong thing is a small defect with a large cost, because it is read exactly when someone is confused.

#### What the two mechanisms now look like, with fourteen defects on record

| Mechanism | Count | Guard |
|---|---|---|
| A text scan standing in for a parser | **8** | Structural parsing where a parser exists (`syn`); focused controls where none does |
| Wording outrunning its cited proof | **5** | **Check 34** — the first check aimed at the claim rather than the form |
| A fact stated in two places, one going stale | **1** | **Check 35** |

**Nine external passes, nine with findings.** Checks: **34 → 36**. The two guards added in response to
findings have each already caught something their author did not: check 34 found HQP-C-064's coverage gap,
and check 35's first run found the drift on line 466.

### Two reported issues in the hardened check 34 — one real, one already fixed

**Reported 1: the unknown-command fallback was a false pass. Valid, and it was the widest one this check
could have had.** `adapter_method_for` returned `"set_"` for anything unmapped, so **any** setter in a proof
body credited **any** unknown command — a claim naming an invented `SetFoo` would have been "proven" by an
unrelated `set_mode` call.

RED first: `command_is_exercised("h.adapter.set_mode(\"PCM\").await;", "SetFoo")` returned **true**. The
function now returns `Option`, an unmapped command must be evidenced by **its own raw wire name**, and
adding a mapping is a deliberate one-line decision to accept a method name as evidence. Three controls pin
it: an unmapped command is not credited by `set_mode`, not by `set_rate`, and *is* credited by a literal
`SetFoo` on the wire.

**Reported 2: the failure message's arguments were reversed. Already fixed at the reviewed head, and worth
being precise about rather than "fixing" twice.** The transposition was real and was caught in the same
turn it was introduced — before the commit — as the previous section records. At `d7d95b4` the call reads
`c.id, adapter_method_for(&cmd)`, and the observed diagnostic is
*"HQP-C-002 names SetShaping but no cited proof exercises it (looked for SetShaping … set_shaper …)"*. The
reviewer was reading the intermediate state of that turn, not the committed code. **The report was right
about the defect and one commit behind on the fix**, which is exactly the kind of thing an exact-SHA
anchoring rule exists to make visible.

The message did change in this pass for a different reason: with `Option`, an unmapped command has no method
to name, so the diagnostic now reads *"and for no mapped adapter method outside comments"* instead of
printing a misleading `set_` prefix.

**Fifteen defects on record. The count of guards that have caught something their author did not is now
three of three:** check 34 found HQP-C-064's coverage gap, check 35's first run found the line-466 status
drift, and check 34's own controls — added under scrutiny — found the `set_` fallback before any reviewer
had to see it fail.

### CodeRabbit at `c9954ef`: check 34 repeated the very failure mode this PR exists to name

The comment landed while two later commits were already pushed, and its finding was not addressed by
either: **check 34 matched source text, so it could not distinguish an executable call from a mention** —
in comments, string literals, fixture metadata or fake setup. Its instruction was explicit: parse the cited
function with `syn` and require a **call expression**, and define separately what a fixture proof can prove.

**It was right, and the live consequence was worse than the report:**

> **HQP-C-001's `SetMode` claim was being credited by the word `SetMode` inside `modes.xml`'s provenance
> comment.** The test it cited — `modes_list_distinguishes_list_index_from_enum_id` — never calls
> `set_mode`; verified by parsing its 13-line body. A command claim was resting on a word in a fixture's
> metadata: the weakest possible evidence, dressed as the strongest.

#### The rewrite

`test_exercises_command` parses the cited test and accepts exactly two things: a **call expression** whose
method or function is the adapter method that emits the command (prefix-matched, so `set_filter` credits
`set_filter_1x`), or a **string literal shaped like the raw wire element** (`"<SetJunkFilter …"`), which is
how a command with no adapter method is actually sent.

Refused, each for a stated reason, and each pinned as a control:

| Refused | Why |
|---|---|
| A comment | A plan is not a proof |
| A **bare** command name in a string literal | `accept_but_ignore("SetRate")` *arms* the fake; it sends nothing. Requiring the `<` separates an arrangement from an invocation — the distinction a substring match cannot make |
| An unrelated setter for an unmapped command | Closed earlier, still pinned |
| A state **read** | `get_state()` is not sending a command |
| **A fixture, at all** | `FIXTURES_NEVER_EXERCISE`: a fixture is a document, an invocation is an action. Wrong in kind, not a near-miss — and it was the live defect above |

`a_fixture_containing_a_command_name_in_its_metadata_is_not_proof_of_that_command` is a tripwire on the real
`modes.xml`: it asserts the word is present in the file and absent from the parsed document, so re-admitting
fixture text as command evidence fails.

**HQP-C-001 now cites `a_delayed_set_mode_still_clamps_indices_into_the_loaded_chain`**, which does call
`set_mode`. Non-vacuity re-proven by removing that citation and watching the check name the row again.

#### One defect of my own, found by my own controls

The first version returned `None` when a test had **no calls and no literals**, conflating *"cannot answer"*
with *"answers no"* — a caller could not tell a missing citation from an empty one. `None` now means the
test is **absent**, and only that; a test that exercises nothing answers `false`. Two controls pin both.

**Sixteen defects on record. The mechanism count is now decisive: nine of the sixteen were a text scan
standing in for a parser** — and this one was in a check written *to guard against wording outrunning
evidence*, which is the same trap one level up. Every text-matching helper that has been converted to
structural parsing has stayed converted; none of the parsed ones has been reopened by a later pass.
Checks: **36 → 37**.

### The doc comment was right and the code did the opposite

Reported while the structural rewrite was still in flight: `test_exercises_command`'s comment said
*"`set_mode` must not credit `set_mode_something_else`"* — and the implementation was
`c.starts_with(method)`, which credits exactly that. **The comment stated the intent correctly and the code
contradicted it**, which is the reverse of the four stale-description findings and, if anything, more
dangerous: a reader checking the comment would have believed the guarantee.

RED first, from the control the comment implied:

| Control | Before | After |
|---|---|---|
| `set_mode_something_else` credits `SetMode` | **true** | false |
| `set_rate_limit` credits `SetRate` | true | false |
| `set_filter_nx` credits `SetFilter` | true | **true** — this is why the rule is a *set*, not one string |

`accepted_calls_for` now returns an **exact set** per command: `SetFilter` accepts `set_filter`,
`set_filter_1x` and `set_filter_nx` — the three real emitters — and `SetMode` accepts `set_mode` and nothing
else. Adding a name is a deliberate, exact, one-line decision, and `None` still forces an unmapped command to
be evidenced by its own raw wire name. The diagnostic now prints the accepted set, so a failure says
*"looked for a call to one of [\"set_mode\"]"* rather than naming a single method that was really a prefix.

**Also reported, and already committed at `889b601`:** that a cited test which exists but has zero calls and
zero literals must answer `Some(false)` rather than the `None` of a missing test. It does, with two controls
pinning both halves — the fix and its controls went in with the structural rewrite, one commit before the
report. Stated rather than re-done.

**Seventeen defects. Ten were a text scan or a loose match standing in for an exact one.** The pattern has
not varied: every time a shortcut stood in for a parse or an exact comparison, a later reader found it, and
every conversion to structural or exact matching has held.

### A ship-gate failure in my own verification set

**Codex independent audit at `d40c431`: `cargo clippy --test hqplayer_ledger_lint -- -D warnings` failed.**
`test_body` — the line-scanning helper that found a test's body by text — became **dead code** the moment
`test_exercises_command` replaced text matching with a parsed call walk. It was never removed.

**Removed, not suppressed.** An `#[allow(dead_code)]` would have left a text-scanning tool lying beside the
structural one that replaced it, for the next person to pick up. That is how the ninth text-scan defect
happened; leaving the tenth in reach would be worse than the warning.

**The gap was in my verification set, not in CI's.** Every prior report ran CI's invocation —
`cargo clippy -- -D warnings`, which covers lib and bin targets — and that was and is clean. It does not
compile test targets, so the file this PR *adds* was never clippy-checked by anything I ran. Corrected: the
focused test-target invocation is now part of this PR's verification set.

| Invocation | Result at this head | Whose |
|---|---|---|
| `cargo clippy -- -D warnings` | **clean** | CI's gate |
| `cargo clippy --test hqplayer_ledger_lint -- -D warnings` | **clean** | this PR's file, now checked |
| `cargo clippy --tests -- -D warnings` | **fails, and not on this branch's work** | baseline |

The third is recorded as a **baseline constraint, distinct from this PR's result**, and attributed rather
than asserted: it reports errors in **32 files, none of which is in this PR's five-file diff**, including
`tests/unbounded_channel_lint.rs` and `tests/oneshot_leak_lint.rs` — both verified **byte-identical to
`origin/v3`**. The pattern is mostly `map_or` simplifications across `src/` and the pre-existing mock
servers. This is the same shape ADR 003 already records for `cargo clippy --all-targets`, and it is not
this PR's to fix.

**Eighteen defects. This one is the first found in the *verification* rather than in the artefact or its
checks** — a reminder that a verification table is itself a claim, and that "clippy clean" without naming
the invocation is exactly the kind of wording this ledger exists to make precise.

### Deferred evidence: a described command is not a sent one

**CodeRabbit at `c2cf67b` (P2), then extended by an independent audit.** `test_exercises_command` recursed
into **deferred contexts**, so a command that was only *described* for later execution counted as sent:

```text
let _never_called = || h.adapter.set_mode("PCM");        // credited SetMode
let _never_called = || send("<SetMode value=\"1\"/>");    // credited SetMode
let _never_polled = async { h.adapter.set_mode("PCM").await; };   // credited SetMode
let _never_polled = async { send("<SetMode value=\"1\"/>"); };     // credited SetMode
```

Four false passes, two expression kinds × two collectors. **All four were reproduced RED before the fix**,
and each is now a control. `visit_expr_closure` and `visit_expr_async` stop the walk; a closure body and an
unawaited `async` block are descriptions, not executions.

**Each override is proven load-bearing separately.** Reverting `visit_expr_closure` alone fails the control
set; reverting `visit_expr_async` alone fails it too. Neither is decorative.

**What is deliberately unaffected, with a control:** an `async fn` test body is a `syn::ItemFn` block, not an
`ExprAsync`, so a direct call inside one is still evidence — and that is the shape of **every** cited test in
the corpus. `#[tokio::test] async fn t() { h.adapter.set_mode("PCM").await; }` is pinned as a positive case,
because a fix that stopped crediting real tests would be worse than the defect.

**Residual, stated precisely rather than left as "unhandled":** an *invoked* closure —
`(|| h.adapter.set_mode("x"))()` — and an inline awaited block are now false **negatives**. That is the
conservative direction, no control demonstrates a need to widen, and widening on speculation is how the
prefix-match defect got in.

### The workspace suite failed once, and the honest reading is a rate

One run during this pass: **exit 101, 3 failures**, all in `tests/adapter_integration.rs` —
`error_handling::lms_fails_gracefully_when_unconfigured`,
`lms_integration::control_fails_when_disconnected`,
`lms_integration::volume_control_fails_when_disconnected`. That file is **not in this PR's diff** and is
byte-identical to `origin/v3`; the family and its mechanism (process-global `UHC_CONFIG_DIR` under
cross-binary concurrency) are ADR 003's documented flake.

**One thing sharpens the record rather than excusing it:** the *isolated* `adapter_integration` binary also
failed that time, **2 of 52** — and earlier passes in this session reported the isolated binary as green.
So the earlier "isolated is green either way" reading was too favourable, exactly as ADR 003 warns about
running tallies. Three subsequent workspace runs at this tree were **exit 0, 589 passed, 0 failed, 12
ignored**. A runner establishes a rate; it does not return a verdict.

**Nineteen defects.** Eleven were a text scan or loose match standing in for an exact one; five were wording
outrunning proof; one a stale status; one in the verification set; and this one — deferred evidence — is the
first where the *shape of the AST walk* was the defect rather than the comparison.
