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
5. Every fixture path in a `Proof` cell exists on disk.
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

```
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
