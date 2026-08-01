# Issue #338 — Restore the v3 lint baseline under Rust 1.97

OH: 80222d6d

**Issue:** [#338](https://github.com/open-horizon-labs/unified-hifi-control/issues/338)
**Branch:** `fix/issue-338-rust-1-97-lint`
**Base:** `v3` @ `0a1b02c2249c3c4f8243c7e1a1cae2bfc2f05257`
**Stage:** 1 (analysis only — no source change in this commit)
**Updated:** 2026-07-30T02:07:35Z

---

## Aim

Restore the documented CI clippy invocation to green on an otherwise unchanged `v3`,
with zone-ordering behavior preserved and no warnings suppressed, delivered
independently of PR #337.

---

## Reproduction Evidence

All commands below were actually run, read-only, against `v3` @ `0a1b02c` with no
source modifications.

### The gate

| Item | Value |
|---|---|
| Lint invocation | `cargo clippy -- -D warnings` |
| Defined at | `.github/workflows/build.yml:369` (also `.github/workflows/docker.yml:43`) |
| Toolchain | `dtolnay/rust-toolchain@stable` — `.github/workflows/build.yml:354` |
| Resolved toolchain | `rustc 1.97.1 (8bab26f4f 2026-07-14)`, `clippy 0.1.97` |
| Gating workflow for this PR | `build.yml` — triggers `pull_request: branches: [master, v3]` (build.yml:4-6) |
| Non-gating | `docker.yml` — triggers on `master` only (docker.yml:5-9), so its Lint job does not run for `v3` |
| Prerequisite | `make css` generates `public/tailwind.css` (gitignored, `.gitignore:24`), consumed by `include_str!` at `src/app/embedded_assets.rs:17`. `build.yml:349-351` runs it before clippy. |

### Real CI failure (evidence from GitHub Actions, not local)

PR #337, run `30506303127`, job `90756714028`:

```
error: consider using `sort_by_key`
   --> src/app/pages/zones.rs:269:9
    |
269 |         result.sort_by(|a, b| priority(&a.0).cmp(&priority(&b.0)));
    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    = note: `-D clippy::unnecessary-sort-by` implied by `-D warnings`
help: try
269 -         result.sort_by(|a, b| priority(&a.0).cmp(&priority(&b.0)));
269 +         result.sort_by_key(|a| priority(&a.0));

error: could not compile `unified-hifi-control` (lib) due to 1 previous error
##[error]Process completed with exit code 101.
```

Note `1 previous error` on x86_64-linux — direct proof from the CI platform that
nothing else in the linux-linted set is failing.

### Local reproduction on unmodified v3

```
$ make css                                   # gitignored artifact, as CI does
$ PATH="$HOME/.rustup/toolchains/stable-aarch64-apple-darwin/bin:$PATH" \
    CARGO_INCREMENTAL=0 cargo clippy -- -D warnings
error: consider using `sort_by_key`
   --> src/app/pages/zones.rs:269:9
error: could not compile `unified-hifi-control` (lib) due to 1 previous error
EXIT=101
```

Byte-identical to CI. `rustc 1.97.1 (8bab26f4f 2026-07-14)` — same build as CI.

### Sufficiency probe — nothing hides behind the abort

Because the lib failed to compile, clippy never reached downstream targets. Probed
read-only by allowing only the offending lint:

```
$ cargo clippy -- -D warnings -A clippy::unnecessary_sort_by
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 7.84s
EXIT=0
```

Zero errors, zero warnings. **The entire 1.97 gap is this one line.** The fix is
sufficient, not merely first in a queue.

### Scope probe — what the gate does *not* cover

```
$ cargo clippy --all-targets -- -D warnings -A clippy::unnecessary_sort_by
error: could not compile `unified-hifi-control` (lib test) due to 77 previous errors
EXIT=101
```

77 errors in `lib test` — `clippy::unwrap_used` / `clippy::panic` inside `#[cfg(test)]`
code, from the crate-level denies at `src/lib.rs:20-24`. CI does **not** pass
`--all-targets`, so this is not part of the baseline. Recorded to rule out
gate-hardening as in-scope for #338, with a number rather than an opinion.

### Behavior-preservation proof

`sort_by_key(f)` is defined as `sort_by(|a, b| f(a).cmp(&f(b)))`; both are stable
sorts. Verified empirically on rustc 1.97.1 with a 9-group fixture containing five
equal-priority ties:

```
current   : ["roon", "LMS", "openhome", "UPnP", "Zulu", "alpha", "Mike", "bravo", "Other"]
suggestion: ["roon", "LMS", "openhome", "UPnP", "Zulu", "alpha", "Mike", "bravo", "Other"]
IDENTICAL = true
```

Exact code shape also compile-probed, because the real code uses a *closure*
(`let priority = |s: &str| -> i32`, zones.rs:259) and `sort_by_key` requires `FnMut`:
`HashMap → into_iter().collect() → sort_by_key(|a| priority(&a.0))` compiles and runs
on 1.97.1.

### Sibling call sites — deliberately untouched

`src/app/pages/zones.rs:256` and `src/adapters/hqplayer.rs:2255` also use `sort_by`.
Both are in the same lib compilation unit clippy linted, and clippy reported
`1 previous error` naming only line 269 — it saw them and did not flag them (their
keys are borrowed `String` fields, where `sort_by_key` would force a clone).
No companion edits are needed; adding them would be unrequested scope.

---

## Solution Space

**Problem:** the `v3` CI lint gate fails under Rust 1.97 on one
`clippy::unnecessary_sort_by` finding, blocking #337 and downstream PRs.
**Key constraint:** preserve behavior, hide no warnings, land independently of #337.

| # | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-Aid | `#[allow(clippy::unnecessary_sort_by)]` at call site or crate root | Hides the warning — excluded by the issue and the instruction |
| A2 | Band-Aid | Pin CI toolchain below 1.97 to dodge the lint | Same evasion, plus a workflow change |
| B | Local Optimum | Apply clippy's machine-applicable fix: `result.sort_by_key(\|a\| priority(&a.0));` | Fixes the instance, not the class |
| C | Reframe | B + a `syn` AST regression test in the repo's existing lint-test idiom | Duplicates clippy; permanent maintenance for zero added detection |
| D | Redesign | Split the gate: pinned-toolchain blocking clippy + advisory floating `@stable` job | Workflow change; outside #338; needs its own issue |
| E | Redesign | Extract grouping into a pure `group_zones_by_source()` + behavioral ordering tests | Real refactor inside a live Dioxus component; itself risks the behavior change #338 forbids |

### Evaluation

- **A / A2 — rejected.** Satisfy the gate by suppressing the signal.
- **B — selected.** Solves the stated problem completely. One line; clippy's own
  suggestion is `MachineApplicable`; equivalence proven twice; the `-A` probe proves
  no cascade. Maintenance burden nil.
- **C — rejected.** The repo's eight AST lint tests (`tests/architecture_lint.rs`,
  `aggregator_lint.rs`, `arbitrary_find_lint.rs`, `await_in_lock_lint.rs`,
  `ignored_send_lint.rs`, `oneshot_leak_lint.rs`, `spawn_cancellation_lint.rs`,
  `unbounded_channel_lint.rs`) exist to catch what clippy *cannot*. This pattern is
  already caught by clippy and already fails CI. `syn`/`walkdir` are present in
  dev-dependencies, so cost is not the objection — redundancy is.
- **D — correct diagnosis of the class, deferred.** `@stable` + `-D warnings` means
  any Rust release can red `v3` with no code change. It is a workflow change,
  explicitly outside this issue, and folding it in recreates the coupling #338 was
  split off to avoid.
- **E — rejected.** Extracting logic from a live Dioxus component to fix a one-line
  lint inverts the cost/risk ratio and puts zone ordering at risk.

### Recommendation

**Option B — Local Optimum.** In `src/app/pages/zones.rs:269`:

```diff
-        result.sort_by(|a, b| priority(&a.0).cmp(&priority(&b.0)));
+        result.sort_by_key(|a| priority(&a.0));
```

**Accepted trade-offs**

- Fixes the instance; the class (D) stays open as a follow-up.
- Pre-existing `HashMap` iteration nondeterminism among equal-priority "other" groups
  (`zones.rs:248`, `:268`) is left as-is — changing it *would* be a behavior change.
- Acceptance criterion 1 ("a regression check demonstrates the current `v3` lint
  failure") is met by the recorded, reproducible gate command above rather than a new
  test file, per the rejection of C. **This is the one judgment call a reviewer may
  want to overturn**; overturning it means adopting C in Stage 2.

---

## Review

**Verdict: Continue**

| Check | Result |
|---|---|
| Still necessary? | Yes — real CI failure, reproduced locally |
| Still aligned? | Yes — touches exactly the line CI names, nothing else |
| Still sufficient? | Yes — one line, one file, no new deps; measured by the `-A` probe |
| Mechanism clear? | Yes — `sort_by_key(f) ≡ sort_by(\|a,b\| f(a).cmp(&f(b)))`, both stable |
| Changes complete? | Yes — sibling `sort_by` sites checked and correctly excluded |

**Drift detected:** none. Stage 1 footprint verified clean: `make css` produced only
gitignored artifacts (`public/tailwind.css`, `./tailwindcss` — `.gitignore:24`, `:13`);
no `src`, `tests`, workflow, API-contract, or dependency changes.

**Carried forward, not absorbed**

1. The class (D) remains open — needs its own issue.
2. Acceptance criterion 6 (#337 rerun/rebased after this lands) happens post-merge,
   outside this PR.

**Honest limit on evidence:** RED is verified twice (real CI + local). **GREEN is not
verified** — no patched tree has been compiled or linted, because Stage 1 forbids
touching `src`. What supports the fix is clippy's `MachineApplicable` suggestion, the
`-A` sufficiency probe, and the two equivalence probes. End-to-end confirmation is
Stage 2 work and is not claimed here.

---

## Dissent

**Verdict: PROCEED** (conditional on two Stage 2 verification steps that do not alter
the approach)

### Contrary evidence

1. **The class recurs; B guarantees a repeat.** Rust ships ~every 6 weeks; 1.97.1 is
   dated 2026-07-14, so 1.98 is due within weeks. This issue is a symptom whose cause
   we are choosing not to treat.
2. **Lint-gate hygiene here is already drifting.** `docker.yml`'s Lint job runs the
   same `cargo clippy -- -D warnings` (line 43) but has **no `make css` step**, while
   `src/app/embedded_assets.rs:17` `include_str!`s the gitignored CSS. That job would
   fail on a missing file, not on lint. It never fires only because `docker.yml` is
   `master`-scoped. A broken lint gate has sat unnoticed.
3. **Nothing in the repo asserts zone group ordering.** "Behavior unchanged" is
   certified by a throwaway probe, not a shipped artifact.
4. **Precedent for CI-adjacent fixes reversing here:** `30bf080 fix(ci): Restore
   tailwind.css to git tracking (#154)` then `b3dcbe0 fix(ci): Generate CSS before
   cargo commands instead of tracking generated file (#155)` — a flip-flop on exactly
   this CSS/CI coupling. Caution about touching workflows in this PR is earned.
5. **Platform gap in local repro — refuted.** macOS aarch64 vs CI x86_64-linux leaves
   two linux-only sites unlinted locally (`src/config/mod.rs:90`, `:244`). The CI log
   reporting exactly `1 previous error` closes this; both blocks are also trivial
   `if let Ok(_) = env::var(…) { return PathBuf::from(…).join(…) }` mirroring
   already-linted macOS equivalents.
6. **Exact-shape attack — refuted** by the closure compile-probe above.

### Pre-mortem

1. **Technical — fix is right, Lint still red.** `cargo fmt --check` runs *before*
   clippy (build.yml:366-369). A non-rustfmt-clean edit reds Lint for an unrelated
   reason and reads as "the fix failed." Highest-probability failure mode.
2. **Adoption — works, changes nothing.** #338 merges, #337 is never rebased, its
   Lint stays red against a stale base, the unblocking benefit never lands.
3. **Opportunity cost.** 1.98 adds a default lint, `v3` reds again; six months on this
   is the third or fourth instance, each consuming a full two-stage ceremony for a
   one-liner, while the workflow change that would end the pattern stays unwritten.
4. **Silent behavior regression.** A later edit to `priority` or the grouping changes
   ordering and nothing fails, because no test asserts it.
5. **Toolchain drift mid-review.** `@stable` floats; a new stable could red this PR
   for something unrelated.

### Hidden assumptions

| Assumption | Evidence | Risk if wrong | Test |
|---|---|---|---|
| `sort_by_key` is semantically exact, ties included | Two 1.97.1 probes; `IDENTICAL = true`; exact shape compiles | Zone groups reorder in UI | Done |
| This is the only 1.97 finding | CI `1 previous error`; `-A` probe `EXIT=0` | Whack-a-mole PR | Done |
| `build.yml` Lint is the gate that matters | build.yml `pull_request: [master, v3]`; docker.yml master-only | Fixing a gate that doesn't gate | Done |
| The edited line is rustfmt-clean | **None** | Lint red for the wrong reason | **Stage 2: `cargo fmt --check`** |
| Toolchain stays 1.97.x through merge | None — `@stable` floats by design | PR reds on an unrelated new lint | Not testable without pinning — that is the class fix |
| Sibling `sort_by` calls need no change | Clippy linted both and flagged neither | Unrequested scope, or incomplete fix | Done |

### Conditions on PROCEED

- **Stage 2 must run `cargo fmt --check` alongside clippy** before claiming the gate
  passes.
- **File a separate follow-up issue for the class** (pinned-toolchain blocking clippy
  + advisory floating `@stable` job), noting `docker.yml`'s missing `make css`.

**ADR:** not warranted — one line, one file, trivially revertible. The gate-design
follow-up is where an ADR would belong.

---

## Superego Review

`sg review staged` (sg 0.9.4) run at this decision point against the staged artifact.
Confirmed the diff contains no `src/`, `tests/`, or `.github/workflows/` changes.

**Verdict: no blocking findings.** Superego endorsed the technical judgment (Option B)
as "sound and well-supported," and specifically credited the sufficiency probe and the
`cargo fmt --check` pre-mortem catch.

### Findings and disposition

| # | Finding | Severity | Disposition |
|---|---|---|---|
| 1 | Proportionality: a 303-line artifact for a one-line fix; if discretionary, the ceremony becomes the burden it aims to prevent | P4 | **Accepted with reason — not discretionary.** The Stage 1 gate mandates persisting Solution Space + Review + Dissent + Superego. Artifact size is set by the gate contract, not the diff. Recorded as a fair observation about the process, not a defect in this change. |
| 2 | Ensure Stage 2 runs `cargo fmt --check` *before* clippy, matching CI order | P2 | **Already addressed** — Stage 2 Plan step 3, pre-existing from the dissent pre-mortem. Will be honored, not assumed. |
| 3 | The class/follow-up finding is "easy to lose if it only lives in this doc's prose" — confirm the issue actually gets filed | P2 | **Valid; escalated rather than actioned.** Filing a new GitHub issue is outside the Stage 1 instruction set (one artifact, one draft PR, four comments) and is outward-facing, so it is not being created unilaterally. Surfaced in the PR body and the Superego PR comment as a required follow-up awaiting user approval. |

No P1 findings. No findings discarded without reason.

---

## Stage 2 Plan (not executed)

TDD order, per AGENTS.md "see it FAIL first":

1. **RED** — `make css` then `cargo clippy -- -D warnings` on unmodified `v3`.
   Expect `error: consider using sort_by_key` at `zones.rs:269:9`, exit 101.
   *Already captured above; re-run at Stage 2 start to confirm the branch state.*
2. **GREEN** — apply the one-line change at `zones.rs:269`.
3. **Verify the gate in the order CI runs it** — `cargo fmt --check`, then
   `cargo clippy -- -D warnings`. Both must pass. (Pre-mortem scenario 1.)
4. **Verify behavior preserved** — `cargo test` for regressions; zone grouping and
   ordering unchanged by construction (proof above).
5. **Confirm on CI** — build.yml Lint green on the PR. This is the only authoritative
   GREEN, since it runs x86_64-linux with the floating `@stable` toolchain.

### Scope

**In scope:** one line in `src/app/pages/zones.rs`.

**Explicitly out of scope:** API routes, request/response schemas,
`tests/fixtures/api_routes.txt`, `.github/workflows/**`, `Cargo.toml` dependencies,
`--all-targets` lint hardening (77 pre-existing test-code findings), the
`HashMap`-ordering nondeterminism, and the gate-design fix (D).

**Public API change:** none.

---

# Stage 2 — Execution

**Updated:** 2026-07-30T02:45Z
**Status:** complete (local); CI confirmation pending
**Stage 1 Codex gate:** PASS at `812bc6750972033b7130e77648caa74dbe6c848a`
**Class follow-up:** filed as #340, out of scope here

## The change

`src/app/pages/zones.rs:269` — `1 file changed, 1 insertion(+), 1 deletion(-)`:

```diff
-        result.sort_by(|a, b| priority(&a.0).cmp(&priority(&b.0)));
+        result.sort_by_key(|a| priority(&a.0));
```

## TDD sequence, in CI's order

| Step | Command | Result |
|---|---|---|
| **RED** (pre-edit, pristine source) | `make css` → `cargo clippy -- -D warnings` | **exit 101** — `error: consider using sort_by_key` at `src/app/pages/zones.rs:269:9` |
| Baseline (pre-edit) | `cargo test` | 1 pre-existing failure, recorded before editing |
| **GREEN** step 1 | `cargo fmt --check` | **exit 0**, no output |
| **GREEN** step 2 | `cargo clippy -- -D warnings` | **exit 0**, zero errors, zero warnings |
| **GREEN** step 3 | `cargo test` | identical to baseline — 0 regressions |
| API gate | `cargo test --test api_contract` | **2 passed** (`api_routes_match_contract`, `golden_file_is_sorted`) |

RED was observed on unmodified source *before* the edit, per AGENTS.md. `cargo fmt
--check` ran before clippy, per the Stage 1 dissent pre-mortem and the Codex gate
condition.

## Pre-existing test failure — reported honestly

`shared_endpoints::get_hqp_discover` (`tests/client_harness.rs:1103`) fails: expected
200, got 500. **It is pre-existing and unrelated to this change.**

- A full `cargo test` baseline was captured on unmodified source *before* editing,
  specifically so attribution would be provable rather than asserted.
- A normalized diff of pre-fix vs post-fix results is **byte-identical**: same nine
  passing binaries, same single failure.
- Cause: `GET /hqp/discover` performs UDP multicast network discovery
  (`src/api/mod.rs:1978-1979`), unavailable in this sandbox.
- CI's Test job passed on PR #337, so this is environment-specific, not a repo defect.
- Deliberately **not** touched — outside #338.

## Behavior preservation, verified on the real source

Stage 1 proofs used hand-written replicas. Stage 2 closes that gap by running the
actual code:

1. Lines 247-271 were mechanically extracted from **both** git `HEAD` and the working
   tree. `diff` confirms the sort line is the **only** textual difference in the block.
2. Both blocks were compiled verbatim as two functions and run against identical
   inputs, 200 iterations:

```text
priority ordering non-decreasing in BOTH, 200 runs : true
identical group content (order-insensitive)        : true
distinct-priority order identical & stable         : true -> ["roon", "LMS", "openhome", "UPnP"]
equal-priority tie order differs between the two   : true  (pre-existing HashMap nondeterminism)
```

3. The last line looks like a regression and is not. **Control experiment:**

```text
CONTROL -- OLD code vs OLD code, tie order varies : true
CONTROL -- NEW code vs NEW code, tie order varies : true
OLD vs OLD, documented Roon/LMS/OpenHome/UPnP order varies : false
NEW vs NEW, documented Roon/LMS/OpenHome/UPnP order varies : false
```

Old-vs-old varies identically to new-vs-new: the nondeterminism is `HashMap` iteration
order (a fresh `RandomState` per call), entirely pre-existing, exactly as flagged at
Stage 1. The documented Roon → LMS → OpenHome → UPnP order never varies in either
version. **The change preserves everything the code actually guarantees.** Without the
control, this would have been misreported as a regression.

## Scope containment (verified)

`git diff --stat` = 1 file, +1/-1. Changed-file counts for forbidden paths, all zero:
`.github` 0, `Cargo.toml` 0, `Cargo.lock` 0, `tests/` 0, `src/main.rs` 0, `src/api/` 0.

Pre-existing warnings surfaced by the IDE in `tests/architecture_lint.rs`,
`tests/mock_servers/`, and `tests/protocol_schema.rs` were left untouched — they live in
`tests/`, which `cargo clippy` without `--all-targets` does not lint, and they match the
77-finding Stage 1 measurement.

**Public API change: none.** `tests/fixtures/api_routes.txt` unchanged;
`api-change-approved` neither needed nor requested.

## Stage 2 Review

**Verdict: Continue.** Necessary (RED re-observed), aligned (one line, forbidden paths
verified zero), sufficient (clippy's own rewrite), mechanism clear (`sort_by_key(f)` ≡
`sort_by(|a,b| f(a).cmp(&f(b)))`, key is `i32`/`Copy` so no clone introduced), complete
(`sort_by_key` returns `()` in place, so `grouped_zones` keeps type
`Vec<(String, Vec<Zone>)>` and no consumer changes). No drift.

**Completion gate:** intent clear ✓, diff reviewed ✓, **CI not yet run on this commit**,
Codex conditions met ✓.

## Stage 2 Dissent

**Verdict: PROCEED.**

The one attack with teeth — "every equivalence proof used a replica, never the real
code" — was answered by extracting and running the real code, and the anomaly that
surfaced was traced to a pre-existing cause by a control rather than explained away.

Untested assumptions, both deferred to CI by design:

- **wasm path is never linted.** `zones.rs` compiles for wasm32 (`zones.rs:146` is
  `cfg(target_arch = "wasm32")`-gated) but `cargo clippy` on the default `server`
  feature never sees it, and `dx build --platform web` was not run. `sort_by_key` is
  core `std` and target-independent, so divergence is not credible — but it was not
  tested. CI's "Build WASM Assets" job is the check.
- **Local is macOS aarch64; CI is x86_64-linux.** `zones.rs:269` is not `cfg`-gated, so
  divergence is not credible, but the authoritative result does not exist yet.

Pre-mortem: CI reds on wasm or a linux-only lint; #337 never rebased so the unblocking
benefit never lands (criterion 6); recurrence via floating `@stable` (#340); the
near-miss false-confidence failure described above; and the standing risk that a
known-red `client_harness` test makes a future genuine failure easier to wave through.

**Confidence:** HIGH on behavior preservation; **MEDIUM-pending** on the end-to-end gate
until build.yml runs on this commit.

## Stage 2 Superego Review

`sg review staged` (sg 0.9.4) run at the Stage 2 decision point against the staged
change (`src/app/pages/zones.rs` +1/-1 and this artifact).

**Verdict: no blocking findings.** Superego: "the change is minimal, correct,
well-verified, and honest about what's still outstanding." It independently confirmed
zero touches to `.github`, `Cargo.toml/lock`, `tests/`, `src/main.rs`, `src/api/`, and
no API-contract change; credited filing #340 separately as correct scope discipline;
and specifically credited the control experiment as "a real save" that "correctly caught
what would've looked like a regression but wasn't."

### Findings and disposition

| # | Finding | Severity | Disposition |
|---|---|---|---|
| 1 | `.oh/.cache/` is not gitignored; worth a one-line `.gitignore` add so it is not swept into a future `git add` | P4 | **Valid; escalated, not actioned.** Pre-existing and untracked (present before this work began), unrelated to #338, and editing `.gitignore` would be an unrelated code change the Stage 2 instructions forbid. Surfaced in the Superego PR comment for a separate change. Confirmed it is untracked and therefore cannot enter this commit. |
| 2 | Proportionality: a 137-line writeup for a one-line clippy autofix; decide once whether this structure is reserved for gated issues or becomes the default | P4 | **Accepted with reason — reserved, not a new baseline.** This structure is mandated by the Stage 1/Stage 2 gate for this specific tracked issue with an independent Codex gate; it is not the default for ordinary fixes. Superego's own framing ("if the former, no action needed") applies. Recorded as a process question for the gate owner, not a defect in this change. |

No P1, P2, or P3 findings. No findings discarded without a reason.

## Honest limits

- **CI has not run on the Stage 2 commit.** build.yml Lint on x86_64-linux is the only
  authoritative GREEN. Everything above is local.
- **The wasm build was not verified locally.**
- No claim is made about any test or job not listed in the tables above.
