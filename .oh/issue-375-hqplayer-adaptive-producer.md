# Issue 375 — HQPlayer Native Adaptive Producer

**Updated:** 2026-07-31
**Status:** execute complete / pre-commit review
**Issue:** #375
**Draft PR:** #376

## Aim

Make HQPlayer the first truthful producer of the generic adaptive-control document: a consumer with no HQPlayer-specific code can inspect current native controls, values, choices, ranges, availability, and apply semantics without seeing native indexes or combining observations from different producer sessions.

## Pre-flight

- [x] Aim is explicit in #375 and parent epic #325.
- [x] Exact prerequisite tips are converged: HQPlayer `48555cf3bcfd8e4a62d1d2b986821844aa48bd39`; adaptive publication `6acb5a60cb9938b9dafae7b033455964d560dede`.
- [x] Combined baseline passed `cargo test --all-features --no-fail-fast -q` before feature edits.
- [x] Existing HTTP, SSE, MCP, Muse, and device contracts are out of scope without explicit approval.
- [x] Native projection is bounded; persistent forms, catalogs, narrowing, telemetry, matrices/composites, and public consumers remain in their owning issues.
- [x] Success is observable through projector, revision, lifecycle/publication, generic-consumer, API-contract, release, and full-suite tests.

## Selected solution

1. The adapter gathers one generation-fenced coherent native observation.
2. A pure projector accepts that value and cannot reach an adapter, socket, cache, clock, or lease.
3. Runtime enumerations remain authoritative. Reviewed constants identify control concepts; deterministic engine-scoped IDs identify unrecognised runtime choices without leaking indexes.
4. Canonical control-plane and state views assign independent revisions. Lane health and its timestamps are outside both revision views and use #324's health-refresh path.
5. The managed HQPlayer run publishes through the bounded adaptive handle; the aggregator remains the only retained-state owner.

## Solution dissent dispositions

- Whole-document equality is rejected for revision classification; volatile fields are structurally excluded.
- The existing `get_pipeline_status` proof already fences transport generation, retries once, validates all four enumeration identities, and refuses a torn result.
- Control existence is structural presence. Situational `Availability` is state; a new shared-contract presence field is not required.
- Omitted lanes mean not applicable; `LaneState::Unconfigured` is already a distinct explicit state.
- `HqpAdapter::producer_epoch` already advances only after a coherent connected snapshot. It is not the adaptive adapter-run identity.
- Stopped retained metadata cannot be described as current now-playing.
- No public route is changed merely to expose this internal producer state.

## TDD evidence

### RED — 2026-07-31

Command:

```text
cargo test --all-features --lib adapters::hqplayer::adaptive::tests -- --nocapture
```

Observed result: **0 passed, 5 failed**. The module compiled; every failure stopped at one of the two explicit missing-behaviour markers:

- `RED: HQPlayer native projection is not implemented`
- `RED: revision classification is not implemented`

The tests require:

- native-only projection with exact dB and no invented volume step;
- runtime choices with no native index leakage and unknown-choice survival;
- rejection when a selected value is outside its verified choice set;
- stopped metadata represented as ungrounded;
- fixed volume visible but non-mutable;
- independent no-op/volatile, state-only, control-plane-only, and combined revision movement.

## Execute

**Status:** complete

Completed locally:

- the pure projector/revision core passes 13 focused tests and rejects invalid native evidence;
- source-family semantics come from the coherent reader rather than the `[source]` display spelling;
- fixed-volume, stopped metadata, transport action descriptors, unknown choice identities, volatile-only refresh, counter exhaustion, and regressing producer epochs have explicit coverage;
- the adapter now gathers one exact generation-fenced native observation and derives both its unchanged legacy zone/pipeline projection and its contract-free native DTO from that value;
- `src/producers` owns adaptive projection, per-instance revisions, the single HQPlayer adapter run, last-known recovery, and definitive producer retirement; the adapter imports no adaptive contract or producer runtime;
- manager startup begins the adaptive run before child workers, shutdown joins children before ending the run, and removal joins a child before retiring only that producer;
- failed adaptive retirement is transactional: removal returns false, restores the instance, and restarts its worker rather than leaving an orphan retained producer;
- one internal 11-entry registry maps every capability-maximal mutable descriptor exactly once to the existing #347 semantic operation; projection refuses an unmapped or duplicate mutable control;
- `origin/v3` at `8ad7ad7` is incorporated and no existing HTTP, SSE, MCP, Muse, or device contract changed.

The generic command preflight, typed attempt receipts, and public consumer dispatch remain in #331. That issue must consume the registry added here rather than create a second control-to-operation mapping.

## Live Embedded qualification — 2026-07-31

Target reachability is proven on TCP 4321/4322 and HTTP 8019/8088, but the read-only tier-1 gate currently stops before `GetInfo`:

- `tier1_live_read_only_verification_when_opted_in`: failed at the coherent initial snapshot;
- both remote and localhost native sessions accept TCP but return no `GetInfo` reply;
- authenticated `/`, `/config`, `/auth`, and `/log` all redirect to `/about`;
- `/about` identifies Embedded 6.0.2 and reports an About-only Trial state.

This is not evidence of a UHC protocol defect or successful live qualification. Hermetic execution continues; read/write qualification remains blocked until HQPlayer serves its operational native/configuration surfaces again.

## Drift check

- Original scope: coherent Native-lane producer document and publication.
- Current work: pure native projector, coherent adapter seam, managed publication lifecycle, and semantic-operation registry.
- Gap: none; this is the first bounded execution unit.
- Verdict: aligned.

## Execute verification

1. [x] Projector/revision core: 13/13.
2. [x] HQPlayer managed lifecycle: 43/43.
3. [x] HQPlayer protocol conformance: 271/271.
4. [x] Adaptive publication boundary lint: 57/57.
5. [x] API contract: 2/2; no route fixture change.
6. [x] Full all-feature Rust suite: pass; one operator-triggered restart test and 12 environment-gated tests remain intentionally ignored.
7. [x] Strict production clippy, formatting, and diff checks: pass.
8. [x] Dioxus release server + WASM build: pass with pinned `dioxus-cli` 0.7.10 and the rustup toolchain that owns `wasm32-unknown-unknown`; the global Homebrew `sccache` wrapper was disabled for this command only.

## Execute review

**Verdict:** ALIGNED / continue to commit.

- Necessary: yes — this is the first real producer validating #323/#324 against an unreliable native protocol.
- Aligned: yes — every changed production path either creates the coherent observation or publishes its adaptive projection.
- Sufficient: yes — generic command execution stays in #331 while this PR supplies and enforces its one semantic registry.
- Mechanism: one coherent native gather feeds both projections; one producer-owned run and per-instance revision tracker feed the sole retained-state aggregator.
- Completeness: the independent audit's one P2 retirement finding was repaired with a red/green rollback test; no remaining actionable finding is known.

Superego's staged static review found no blocking concern. Its one concrete cleanup, a redundant unused `HqpNativeObservation::instance_name()` accessor beside the public field, was removed before commit. It flagged the projector module's size as reviewer-bandwidth cost, not a design defect; splitting the tightly coupled projection/revision/registry/publisher unit without a behavioral reason is deferred.

## Execute dissent

**Recommendation:** PROCEED.

Strongest contrary cases and dispositions:

- A sink failure could orphan an admitted producer after instance removal. Confirmed; removal now rolls back atomically and restarts the worker.
- A descriptor registry could merely name methods without proving runtime invokability. The registry is intentionally only the compile-time bijection; #347 owns verified semantic setters and #331 must add typed preflight/attempt receipts before exposing commands.
- Fail-closed coherence may retain stale state during daemon instability. This is explicit LastKnown/disconnected state, bounded retry, and preferable to publishing torn settings.
- The real Embedded 6.0.2 target is not yet a live qualification. Correct: its current About-only Trial state prevents native/config observation, so no live success is claimed.

No new ADR is needed: the dependency inversion and lifecycle ownership are already captured by #375, #323/#324, and the architecture boundary tests.

## Remaining delivery gates

1. [ ] Stage and run Superego review; fix P1-P3 findings.
2. [ ] Commit and push the exact reviewed tree.
3. [ ] Post execute review/dissent reports 3/6 and 4/6 at the exact head SHA; request CodeRabbit and Superego review.
4. [ ] Keep PR draft and unmerged pending explicit maintainer approval.
