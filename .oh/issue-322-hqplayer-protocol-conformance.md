# Issue #322 — HQPlayer executable native-protocol conformance harness

**OH:** 80222d6d
**Program:** #310 · **Epic:** #311 · **Issue:** #322
**Branch:** `feat/issue-322-hqplayer-protocol-conformance` · **Base:** `origin/v3`
**Stage:** 2 (execution, complete pending Codex review) — the original Stage 1 sections are
unchanged. Stage 2 evidence is appended in the middle; the *red checkpoint* section is an early
snapshot, superseded by **Stage 2 execution record** below.

> **A second Stage 1 is open.** The HQPTuner stable/beta/dev salvage was added to issue #322 *after*
> the original Stage 1 and Stage 2 completed, so its additions have never been through a solution
> decision. **[HQPTuner Stage 1 amendment (2026-07-29)](#hqptuner-stage-1-amendment-2026-07-29)** at
> the end of this file is that decision, and it supersedes nothing above it — the original Stage 1
> and Stage 2 records stand as written. No `src` or `tests` file was changed in the amendment turn.

---

## Aim

**Updated:** 2026-07-29

Change in behavior we want: an HQPlayer maintainer (human or agent) who is about to change the
HQPlayer client can run one command and get a verdict about wire conformance, and that verdict is
trustworthy enough that they stop reading Rust to learn the protocol.

Today the opposite is true. The Rust implementation is the de facto specification, and
`tests/adapter_integration.rs:603-604` says so in a comment:

```rust
// Use reqwest to test the mock directly (not through adapter)
// because HQP adapter uses a complex TCP protocol
```

So the mock has never been driven by the real client. Success for #322 is that this comment becomes
impossible to write: the fake daemon *is* driven by `HqpAdapter`, and it can reproduce failure
conditions the real daemon exhibits.

## Problem context

Inherited framing from #322 (unchanged): shift from protocol prose and one-off fixes to an
**executable conformance boundary that fails before production behavior is changed**.

### Binding constraints (from #310/#311/#322 and AGENTS.md)

| Constraint | Type | Source |
|---|---|---|
| Tests precede fixes; observe red before green | hard | AGENTS.md "Test-Driven Development"; #310 stage-2 gate |
| Harness runs in CI with no HQPlayer present | hard | #322 AC |
| Fixtures distinguish list index, enum ID, semantic name | hard | #322 AC; HQPTuner `architecture.md` §2 |
| Explicit `result="Error"` and ambiguous success both covered | hard | #322 AC; HQPTuner `protocol.md` §4 |
| Arbitrary TCP fragmentation must not terminate on a self-closing child | hard | #322 AC; `<Status><metadata/></Status>` |
| Decimal dB volume round-trips | hard | #322 AC; `State.volume` is a **double** |
| No public API endpoint/payload change without approval; never self-apply `api-change-approved` | hard | AGENTS.md "API Stability" |
| Preserve unrelated user work | hard | session instruction |
| Recorded fixtures vs stateful mock implementation | soft | #322 constraints |
| Breadth of the first version matrix | soft | #322 constraints |

### Evidence base gathered before candidate generation

Primary external evidence is the HQPTuner audit repo at
`/private/tmp/hqptuner-audit.55eb9h/repo`, whose `docs/protocol.md` is derived from official
`hqp-control` 6.0.1 sources and marks live-verified findings against `hqplayerd 6.0.4` (Opal).
Repo-local evidence is `docs/hqplayer-protocol-reference.md` (derived from `hqp-control` v5.2.30,
no live verification) and `.oh/hqplayer-spec.md`.

Concrete defects the current boundary cannot detect. Each is a live-fire target for stage 2:

| # | Defect | Code site | Wire truth |
|---|---|---|---|
| 1 | Mock returns `String::new()` for any line beginning `<?xml`, and `build_request` emits declaration + element on **one** line, so `HqpAdapter` against `MockHqpServer` gets nothing and times out after `RESPONSE_TIMEOUT` (3 s) | `tests/mock_servers/hqplayer.rs:137-139` vs `src/adapters/hqplayer.rs:849-852`, `757-794` | — (mock defect; explains why no test drives the adapter against the mock) |
| 2 | No setter checks `result`. Every `Set*`/transport call is `send_command(...).await?; Ok(())` | `src/adapters/hqplayer.rs:1122-1141`, `1230-1235`, `1272-1301`, `1304-1329`, `1332-1371` | Daemon echoes `<SetFilter result="OK"/>` or `<SetFilter result="Error">invalid filter</SetFilter>`; `protocol.md` §4 |
| 3 | Mock answers `<Ok/>` — a shape the daemon never sends — so even a `result`-checking client could not be tested | `tests/mock_servers/hqplayer.rs:180-184` | `protocol.md` §4 |
| 4 | `State.volume` parsed as `i32`; `"-23.5"` fails `parse()` and `unwrap_or(0)` yields **0 dB = maximum volume** | `src/adapters/hqplayer.rs:869-873`, `914` | `State.volume` is `double`; `<Volume value="-23.5"/>`; `protocol.md` §6 |
| 5 | `VolumeRange.min`/`max` parsed as `i32` | `src/adapters/hqplayer.rs:955-961` | doubles (dB) |
| 6 | `set_volume(&self, value: i32)` cannot express a fractional dB target at all | `src/adapters/hqplayer.rs:1304-1308` | dB double |
| 7 | Framing completes on the first `/>`, so `<Status …><metadata …/>` arriving before `</Status>` is accepted as a whole document | `src/adapters/hqplayer.rs:781-786` | `protocol.md` §6 Status: "the parser must match the closing `</Status>` (not the first self-closing `/>`, which is the `metadata` element)" |
| 8 | `parse_attr` is a substring scan over the whole buffer with no entity decoding | `src/adapters/hqplayer.rs:858-867` | attributes can be **double-escaped**, and bare `&` has been observed; `protocol.md` §1 |
| 9 | `state == 3` ("stop requested") renders as `"Unknown"` | `src/adapters/hqplayer.rs:1449-1454` | `protocol.md` §6 State |
| 10 | Adapter reads `filter_20k` as a bool | `src/adapters/hqplayer.rs:922` | attribute is `filter_junk`, an **int** index into `GetJunkFilters`; wire element is `SetJunkFilter` |
| 11 | Mock modes list is `index=0 PCM, index=1 SDM` — off by one and missing `[source]` | `tests/mock_servers/hqplayer.rs:161-163` | verified `0 = [source]` (value −1), `1 = PCM` (0), `2 = SDM` (1) |
| 12 | Reconnect budget is `MAX_RECONNECT_ATTEMPTS = 2` × `RECONNECT_DELAY = 1 s` ≈ 2 s | `src/adapters/hqplayer.rs:137-139`, `717-754` | restart windows are ~3.4 s (`profile/load`), ~5.6 s (`/restore`), ~9.3 s (`systemctl restart`); connections are **refused**, not hung |
| 13 | No application keep-alive | `src/adapters/hqplayer.rs` (absent) | fully idle connection closed by daemon at ~156 s |
| 14 | Mock is a per-line request/response machine — it cannot emit a coalesced or arbitrarily-chunked byte stream | `tests/mock_servers/hqplayer.rs:113-131` | responses are newline-*hinted*, not newline-*framed*; read until a document parses |

Two claims in `docs/hqplayer-protocol-reference.md` survive this evidence and one does not:

- **Survives (confirmed live):** `Set*` and `State` speak **list index**, not enum ID.
  `<SetFilter value="6"/>` selected index 6 (`poly-sinc-lp`), while enum ID 6 is `poly-sinc-lp-2s`.
- **Survives:** modes have `index ≠ value`.
- **Missing, and it is the largest gap:** the document never mentions the `result` attribute, so the
  implementation it authorised has no verification story at all. `hqplayerd.xml` storing enum IDs
  (e.g. `filter="40"`) is likewise absent — that is precisely the cross-lane fixture #322 demands.

### Blocker-lite note on referenced inputs

`docs/hqplayer-protocol-audit.md` was named in the session brief but does not exist on this branch
or on `origin/v3` (`git cat-file -e origin/v3:docs/hqplayer-protocol-audit.md` → *does not exist*).
It was deleted; `git log --all -- docs/hqplayer-protocol-audit.md` shows it last touched by
`00f81e7`, `2f31b26`, `aaf7457`. Its substantive content survives in
`docs/hqplayer-protocol-reference.md`, which was read in full. Not treated as a blocker.

---

## Solution Space

**Updated:** 2026-07-29

### Repo-local situated knowledge consulted

`rna-mcp search` for `metis`/`guardrail`/`signal` artifacts on "HQPlayer protocol mock conformance
fixture guardrail" returned no metis or guardrail artifacts — `.oh/` contains only
`hqplayer-spec.md` and a regenerable index cache. So the only prior situated judgment is
`.oh/hqplayer-spec.md`, whose 2026-02-04 conclusion ("create a definitive protocol doc first") was
executed and is *the thing that proved insufficient*: a document was produced, the implementation
was aligned to it, and the implementation still cannot detect any of the 14 defects above. That is
the strongest argument in the repo against another prose-first answer, and it is why no candidate
below is "improve the documentation."

## Solution Space Analysis

**Problem:** #322's conformance boundary must be able to *fail* against today's HQPlayer client for
reasons the current mock structurally cannot express — framing, rejection, delayed apply, decimal
dB, and index-vs-enum-ID confusion.

**Key Constraint:** Tests must precede fixes. The boundary therefore has to produce a red result
against the **unmodified** adapter, which rules out any candidate whose first step is a production
refactor.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|--------|-------|----------|-----------|
| A | Band-Aid | Extend `MockHqpServer` in place: add attributes, mutate state on `Set*`, emit `result="OK"` | Per-line request/response engine cannot express fragmentation, coalescing, or timing; no version provenance |
| B | Local Optimum | Byte-level fixture corpus + pure framer/decoder unit tests, no sockets | Cannot reach command verification, timeouts, reconnect, external change; needs a parser boundary that does not exist yet, so it cannot go red first |
| C | Reframe | Split the boundary in two: a scriptable **wire layer** (chunking, coalescing, delay, refuse, close) over a stateful **daemon model** (applies `Set*`, echoes `result`, can reject / accept-but-ignore / delay-apply / change externally), fed by a **versioned document corpus**; assert through `HqpAdapter` over a real socket | Real test infrastructure to own and unit-test (~700–900 lines); socket tests slower than sans-io |
| D | Reframe | Record/replay golden transcripts (VCR) captured from a live `hqplayerd`, with a `--record` refresh mode | Ordering-brittle against the client refactoring #322 exists to enable; cannot synthesize conditions a daemon won't produce on demand; re-recording needs hardware so CI can never regenerate |
| E | Redesign | Extract a sans-io protocol state machine into `src/hqplayer/` (`on_bytes(&[u8]) -> Vec<Event>`), adapter becomes an IO shim; exhaustive split-at-every-offset tests | Production rewrite must land *before* any test can go red; front-loads the lifecycle refactor that #162 owns; largest blast radius on the program's first PR |

Escalation levels represented: Band-Aid (A), Local Optimum (B), Reframe (C, D), Redesign (E).

### Evaluation

**Option A: Extend the existing mock in place**
- Solves stated problem: **No.** Fixes defects 3 and 11 and part of 1, but `handle_connection`
  (`tests/mock_servers/hqplayer.rs:113-131`) is a `read_line` → one-`String`-response loop. Defects
  7 and 14 — the framing conditions #322 names as hard constraints — are not expressible without
  replacing that loop, at which point this is Option C wearing A's name.
- Implementation cost: Low.
- Maintenance burden: Low now, high later — every later issue (#162, #328, #329) re-opens the same
  file to bolt on another behaviour flag.
- Second-order effects: Tests written against a mock that "always succeeds" bake the mock's
  optimism into the assertions. This is the failure mode #322 was filed to end.
- Optionality: None. Dead end.

**Option B: Fixture corpus + pure framer tests**
- Solves stated problem: **Partially.** Excellent for defects 4, 5, 7, 8, 9, 10, 11 — and
  fragmentation becomes trivial, since a corpus document can be sliced at every byte offset.
  Silent on 2, 6, 12, 13: verification, reconnect and idle-close are behaviours over *time*, and a
  corpus has no time.
- Implementation cost: Low–Medium.
- Maintenance burden: Low. Fixture files are the cheapest artifact to review and version.
- Second-order effects: **Cannot go red first.** There is no public framer/decoder to call —
  `parse_attr` and the read loop are private to `HqpAdapter` (`src/adapters/hqplayer.rs:757-885`).
  Extracting one is a production change, which inverts the TDD constraint.
- Optionality: Good — the corpus is reusable by every other candidate.

**Option C: Scripted fake daemon over a versioned corpus**
- Solves stated problem: **Yes**, and it is the only candidate that reaches all fourteen defects.
  The key insight is that the current mock fuses two concerns into one `match` arm: *what the
  daemon says* (documents, per version) and *how and when it says it* (bytes, chunks, delays,
  rejections). Separating them makes each #322 acceptance criterion a direct composition:
  `wire.chunk_after("<metadata")` for defect 7; `model.reject_next("SetFilter", "invalid filter")`
  for explicit error; `model.accept_but_ignore("SetShaping")` for ambiguous success;
  `model.apply_after_polls(2)` for delayed apply; `model.external_set(...)` for another controller;
  `wire.refuse_for(Duration::from_secs(4))` for the restart window in defect 12;
  `wire.close_when_idle(...)` for defect 13.
- Implementation cost: **Medium–High.** Estimate 700–900 lines across `wire.rs`, `model.rs`,
  `corpus.rs`, plus its own unit tests. This is the honest cost and the main thing dissent must
  attack.
- Maintenance burden: Medium. It is real code. Mitigated by keeping the wire layer dumb (it only
  knows bytes and schedules, never XML) and the model layer XML-only (never sockets).
- Second-order effects: **Goes red against the unmodified adapter on day one** — the socket is
  already the adapter's only seam (`configure(host, port, …)`), so no production change is needed
  to observe failure. Serves #162 (lifecycle/reconnect), #208 (multi-instance — a fake per port),
  #328/#329 (verified setters) without redesign. The opt-in real-server mode becomes almost free:
  the same assertion bodies run against a real host behind an env guard.
- Optionality: **Highest.** Subsumes B (the corpus *is* B) and does not block E — if #162 later
  extracts a sans-io machine, these tests keep passing because they assert through the public
  adapter surface, not internals.

**Option D: Record/replay golden transcripts**
- Solves stated problem: **Partially.** Provenance is superb and hand-authored wire shapes vanish.
  But replay matches on request order, so it breaks when the client legitimately reorders its
  calls — and "safe client refactoring" is the first thing #322 says it enables. It also cannot
  produce what the daemon will not produce on demand: arbitrary chunk boundaries,
  `result="Error"`, accept-but-ignore, a 156 s idle close. Re-recording needs an `hqplayerd`, so CI
  can never regenerate a stale transcript.
- Implementation cost: Medium.
- Maintenance burden: **High and externally gated** — every protocol change requires hardware.
- Second-order effects: Whole-transcript equality is the golden-dump anti-pattern; HQPTuner's own
  `docs/testing.md` rule 5 forbids it for exactly this reason ("snapshots re-assert implementation
  back at itself").
- Optionality: Low. Transcripts are a terminal artifact.

**Option E: Sans-io protocol state machine**
- Solves stated problem: **Yes, eventually,** and more cheaply per test than C once it exists —
  microsecond tests, exhaustive split-at-every-offset, no timing at all. `src/hqplayer/` already
  exists as an empty placeholder with a `.gitkeep`, so the repo has anticipated this shape.
- Implementation cost: **High.**
- Maintenance burden: Low once landed — the best long-run answer of the five.
- Second-order effects: Fatal for *this* issue. Nothing can go red until the machine exists, so
  the first PR of the program becomes a large adapter rewrite whose own correctness has no harness
  yet. It also pre-empts #162, which owns the lifecycle refactor, and would strand #322's
  reconnect/idle-close criteria — those are socket behaviours a sans-io core deliberately excludes.
- Optionality: High as a destination, negative as a starting point.

### Recommendation

**Selected:** Option C — scripted fake daemon over a versioned document corpus
**Level:** Reframe

**Rationale:** The hard constraint decides it. "Tests precede fixes" means the boundary must fail
against the adapter *as it is today*, and the adapter's only seam today is a TCP socket. B and E
both require a production change before their first assertion can exist; A cannot express the
framing conditions the issue lists as hard. C fails red on an unmodified `src/adapters/hqplayer.rs`
because it drives the real client, and it is the only candidate that covers verification, timing,
and framing in one boundary.

The reframe is not "build a better mock." It is: **a conformance boundary is a corpus plus a wire
schedule plus a state model, and fusing those three into one `match` arm is why the current mock
always succeeds.** Option B is absorbed rather than rejected — the corpus is C's document layer.

**Why not the others:**
- **A:** Cannot reach #322's hard framing constraints without becoming C.
- **B:** Cannot produce a red test without a production refactor first; no time dimension.
- **D:** Ordering-brittle against the refactoring #322 exists to enable; CI cannot regenerate;
  golden-dump anti-pattern.
- **E:** Right destination, wrong starting point — inverts TDD on the program's first PR and
  pre-empts #162's scope.

**Accepted trade-offs:**
1. **We own real test infrastructure.** The fake daemon is ~700–900 lines with its own unit tests.
   Accepted because it is amortised across #162, #208, #328, #329, #330 — five downstream issues
   that all need a peer that can misbehave on demand.
2. **Socket tests are slower than sans-io** and must not wait on wall clocks. Accepted with a
   rule: no test asserts elapsed time; timeout/idle paths are driven by the fake **closing** the
   connection rather than hanging, wherever that is faithful.
3. **`RESPONSE_TIMEOUT` and `MAX_RECONNECT_ATTEMPTS` are `const`** (`src/adapters/hqplayer.rs:134`,
   `137`). Genuine timeout and restart-window coverage needs them injectable. That is a small
   internal production change with no public API surface — it is **deferred to stage 2** and named
   here so it is not smuggled in. Two tests (defect 12's restart window, defect 13's idle close)
   are marked `#[ignore]` in v1 if the seam is not approved.
4. **The version matrix starts at two profiles, one of them explicitly unverified.**
   `hqpd-6.0.4-opal` carries live-verified provenance from the HQPTuner audit; `hqpd-5.x-legacy` is
   marked *derived, unverified* so the version dimension exists structurally without fabricating
   verified claims. Breadth grows when evidence does.
5. **`docs/hqplayer-protocol-reference.md` will need correcting** in stage 2 — it authorises an
   implementation with no `result` verification and omits the enum-ID/index cross-lane split. The
   corpus, not the prose, becomes authoritative; the doc is demoted to a reader's guide with
   per-claim provenance.

### Implementation Notes

Shape for stage 2, recorded so the review and dissent below have something concrete to attack:

```text
tests/mock_servers/hqplayer/
  mod.rs      — MockHqpServer facade, re-implemented over the layers below
                (keeps tests/adapter_integration.rs and tests/zones_sha_integration.rs green)
  wire.rs     — bytes only: chunk_after(marker) / chunk_every(n) / coalesce_next(n)
                / delay_before(d) / refuse_for(d) / close_after_idle(d) / drop_now()
  model.rs    — XML only: applies Set* to state, echoes <Cmd result="OK"/>,
                reject_next(cmd, reason) / accept_but_ignore(cmd) / apply_after_polls(n)
                / external_set(field, value); never touches a socket
  corpus.rs   — loads tests/fixtures/hqplayer/<version>/*.xml with provenance headers
tests/fixtures/hqplayer/hqpd-6.0.4-opal/   — verified (HQPTuner audit, hqplayerd 6.0.4 Opal)
tests/fixtures/hqplayer/hqpd-5.x-legacy/   — derived, UNVERIFIED (marked in-file)
tests/hqplayer_conformance.rs              — the assertion suite, drives HqpAdapter
```

Non-negotiables carried into stage 2:

- Every assertion goes through `HqpAdapter`'s public surface. No test reaches a private helper, so
  a later sans-io extraction (Option E, under #162) does not invalidate the suite.
- One behaviour per test; test names state the behaviour.
- Each fixture file carries a provenance header: source, version, verified/derived, date.
- Cross-lane fixture pair proves live `State`/`Set*` use **list index** while a persistent config
  document stores **enum ID** (`filter="40"` = `poly-sinc-gauss-long`), with two independent
  conversions that never share a code path.
- Opt-in real-server mode: `UHC_HQP_CONFORMANCE_HOST`, `#[ignore]` by default, **read-only**
  assertions only against real hardware.
- No public API route or payload change. `tests/fixtures/api_routes.txt` is not touched.

---

## Review

**Updated:** 2026-07-29
**Verdict:** ALIGNED

## Review Summary

**Aim:** Establish an executable HQPlayer protocol conformance boundary that fails before any
production behavior is changed, so protocol truth stops living in `src/adapters/hqplayer.rs`.
**Status:** Adjust

### Alignment Check

- **Necessary: Yes.** Not hypothetical. `tests/adapter_integration.rs:603-604` documents in a
  comment that the adapter is never driven against the mock, and the mock returns `String::new()`
  for the exact line shape `build_request` produces
  (`tests/mock_servers/hqplayer.rs:137-139` vs `src/adapters/hqplayer.rs:849-852`) — so it could
  not be, today. Defect 4 (decimal dB → `unwrap_or(0)` → 0 dB, maximum volume) is a live user-harm
  path on a version-6 daemon, not a future concern.
- **Aligned: Yes.** All nine #322 acceptance criteria map onto a named artifact: stateful model +
  corpus (AC1), wire chunking/coalescing/self-closing-child/malformed/timeout/reconnect (AC2),
  `reject_next`/`accept_but_ignore`/`apply_after_polls`/`external_set` (AC3), volume fixtures with
  fractional dB / fixed / adaptive / min/max/step / mute (AC4), name→index asserted from observed
  list+state pairs (AC5), cross-lane index-vs-enum-ID pair (AC6), transport/seek/pipeline/persistent
  families (AC7), CI-without-HQPlayer plus `UHC_HQP_CONFORMANCE_HOST` opt-in (AC8), per-fixture
  provenance headers (AC9). No candidate work outside #322's boundary was selected.
- **Sufficient: Adjust.** Three layers plus a corpus plus a facade is at the yellow edge of the
  complexity signals. It is justified only because each layer maps to a distinct #322 hard
  constraint that the others structurally cannot serve — but the estimate (700–900 lines) is large
  enough that stage 2 must be able to stop early with value delivered. **Adjustment: sequence the
  build so the wire layer and framing tests land and go red first**, since defect 7 is the single
  hardest constraint and the one most likely to reveal that the design is wrong. If the wire layer
  cannot make the adapter fail on `<Status …><metadata …/>` split from `</Status>`, the whole
  approach is suspect and we learn it in the first hour rather than the last.
- **Mechanism clear: Yes.** Because the current mock fuses *what the daemon says* with *how and
  when it says it* into one `match` arm, no test can vary one while holding the other fixed —
  which is precisely why it always succeeds. Separating documents (corpus) from schedule (wire)
  from semantics (model) makes each acceptance criterion a composition of two independent knobs, and
  driving the real `HqpAdapter` over a real socket means the failure observed is the client's, not a
  test double's.
- **Changes complete: Adjust.** Two ripple effects are identified but not yet resolved, and both
  must be settled before stage 2 writes code:
  1. `MockHqpServer` has two live consumers — `tests/adapter_integration.rs:511` and
     `tests/zones_sha_integration.rs:6`. Re-implementing it as a facade keeps them green; deleting
     or renaming it does not. The facade is required, not optional.
  2. `RESPONSE_TIMEOUT` / `MAX_RECONNECT_ATTEMPTS` are `const` (`src/adapters/hqplayer.rs:134`,
     `137`). Without an injectable seam, defect 12 and 13 coverage either waits on wall clocks —
     forbidden by trade-off 2 — or is skipped. **Adjustment: stage 2 opens with an explicit
     decision on this seam and marks the two dependent tests `#[ignore]` with a reason if it is
     declined.** It is an internal constant, not a public API surface, so it does not need
     `api-change-approved`; it does need to be stated rather than smuggled.

### Drift Detected

**Drift: Scope Drift (candidate, rejected before selection)**
Started as: build the conformance boundary for #322.
Became (in Option E): extract a sans-io protocol state machine and rewrite the adapter's IO path.
Impact: would have inverted the TDD constraint (nothing can go red until the rewrite exists) and
consumed #162's scope on the program's first PR. Named and rejected during solution space; not
carried into the selection. No drift in the selected plan.

**Drift: Goal Drift (latent, must be guarded in stage 2)**
The selected plan touches the same 14 defects it is designed to detect. The temptation in stage 2
will be to fix defect 4 while writing the fixture that catches it. That is a stage-2 gate
violation, not a stage-1 one — but the guard belongs on the record now: **red first, in a separate
commit from green.**

### Decision

Adjust, not Continue. The plan is necessary, aligned, and its mechanism is stated cleanly. Two
concrete gaps keep it off Continue: the build order does not currently front-load its own riskiest
assumption, and the injectable-timeout seam is an open question whose answer changes what v1
covers. Both are correctable inside the selected approach — neither reopens the solution space —
so this is Adjust rather than Pause.

### Next Steps

1. Fold into the selected plan: **wire layer + framing tests first**, so defect 7 either goes red
   in the first increment or invalidates the design cheaply.
2. Fold in: `MockHqpServer` **must** survive as a facade over the new layers; verify
   `tests/adapter_integration.rs` and `tests/zones_sha_integration.rs` stay green.
3. Open stage 2 with an explicit decision on the injectable `RESPONSE_TIMEOUT` /
   `MAX_RECONNECT_ATTEMPTS` seam; if declined, mark defect-12/13 tests `#[ignore]` with a stated
   reason rather than dropping the criteria silently.
4. Run `/dissent` on this adjusted plan before any code is written.
5. Commit this artifact only, open the draft PR, post the three reports, and stop at the gate.

---

## Dissent

**Updated:** 2026-07-29
**Decision:** ADJUST

## Dissent Report

**Decision under review:** Option C — build the #322 conformance boundary as a scriptable wire
layer over a stateful daemon model, fed by a versioned document corpus, asserted through
`HqpAdapter` over a real socket.
**Stakes:** First PR of program #310. It sets the protocol-truth mechanism that #162, #208, #328,
#329 and #330 will all build on, and it is the artifact that decides whether "HQPlayer conformance"
means a corpus or means the Rust source. Hard to reverse cheaply once four issues depend on it.
**Confidence before dissent:** HIGH — the "tests precede fixes" constraint eliminated B and E
almost mechanically, and mechanical eliminations are exactly where hidden assumptions hide.

### Steel-Man Position

The current boundary cannot fail. Its mock answers `<Ok/>` to every setter
(`tests/mock_servers/hqplayer.rs:180-184`), a shape the real daemon never sends; it never mutates
state on `Set*`; and it returns `String::new()` for the one line shape the adapter actually emits,
so no test drives the adapter through it — a fact the codebase admits in a comment at
`tests/adapter_integration.rs:603-604`. Meanwhile the client silently discards `result="Error"` on
every setter and turns a decimal dB volume into 0 dB, i.e. maximum output. The reason these
coexist is structural: the mock fuses documents, timing, and semantics into one `match` arm, so no
test can vary one while pinning the others. Option C separates those three concerns, which turns
every one of #322's acceptance criteria into a two-knob composition, and it asserts through the
real client over a real socket — the adapter's only existing seam — so it produces a red result
against unmodified production code on day one. No other candidate does that: B and E need a
production refactor before their first assertion exists, A cannot express fragmentation, D cannot
synthesize conditions a daemon won't produce on demand. It is also the only candidate that
simultaneously serves reconnect (#162), multi-instance (#208), and verified setters (#329) without
being rebuilt.

### Contrary Evidence

1. **The single strongest external precedent chose a different shape for its fast tests.**
   HQPTuner solved this exact problem against this exact daemon, and its `docs/testing.md` rule 7
   is explicit that its suite went from 84 s to 7 s only by removing real sleeps and virtualizing
   the clock through injectable seams — "a test reintroducing wall-clock wait is defective even
   when it passes." Option C's socket-plus-timing design is the shape HQPTuner had to escape. Its
   rule 4 does mandate a wire-speaking fake over a real socket, so the fake is vindicated; the
   *timing* dimension is not. This is the sharpest evidence against C as specified, and it
   converges on the seam the review already flagged as unresolved.

2. **The plan's own trade-off 3 concedes that two acceptance criteria may be unreachable in v1.**
   `RESPONSE_TIMEOUT` and `MAX_RECONNECT_ATTEMPTS` are `const`
   (`src/adapters/hqplayer.rs:134`, `137`), and #322 AC2 names "timeouts, and reconnect boundaries"
   as required. A plan that opens by marking required criteria `#[ignore]` is a plan whose scope and
   whose gate disagree. Either the seam is in scope, or AC2 is partially deferred with the epic's
   consent — the current plan says neither.

3. **The 700–900 line estimate has no comparable in this repo, and the nearest neighbour is
   smaller than the estimate by a wide margin.** The largest existing mock is
   `tests/mock_servers/lms.rs` at 14,056 bytes; `openhome.rs` is 13,005 and `hqplayer.rs` is 8,089.
   The proposal is for the HQPlayer fake to become substantially the largest test fixture in the
   tree, in the same PR that introduces a fixture corpus and a new test binary. Estimates that
   exceed every local precedent usually mean the design has absorbed work that belongs to a later
   issue.

4. **Option D's provenance argument was rejected on brittleness, but C inherits D's real weakness
   without D's real strength.** C's corpus is hand-transcribed from a *document about* a live
   daemon, not captured from one. `docs/hqplayer-protocol-reference.md` is itself an object lesson:
   it was written from reference-implementation sources, was internally consistent, authorised the
   current implementation — and is silent on the `result` attribute, the single most consequential
   thing the client gets wrong. A corpus transcribed by hand can be wrong the same way, and unlike
   D nothing in CI will ever contradict it.

5. **"Goes red on day one" is an argument about convenience, not correctness.** A conformance
   suite's job is to encode the wire contract; whether the current client happens to be callable
   without refactoring is a property of today's code, not of the contract. Weighting it as the
   decisive criterion is what selected a socket-and-timing design over a sans-io one, and defect 7
   — the hardest constraint in the issue — is exactly the kind of thing exhaustive
   split-at-every-offset testing does better than scripted chunk boundaries.

### Pre-Mortem Scenarios

1. **Technical failure — the fake becomes the specification, again.**
   Six months on, `tests/mock_servers/hqplayer/` is 1,100 lines. A conformance test fails; the
   fastest fix is to adjust `model.rs`, because the fake is closer to hand than a real daemon and
   nobody can say which of the two is right. The corpus's `hqpd-5.x-legacy` profile — shipped
   *derived, unverified* — has quietly become the one everything is written against because it was
   first in the directory listing. We have rebuilt "the implementation is the spec" one layer out,
   and it is harder to see because it lives in `tests/`.
   *Warning signs to watch:* a commit that edits `model.rs` and a conformance assertion together;
   any fixture whose provenance header still says `UNVERIFIED` after stage 2 ships; a test named
   after a mock capability rather than a wire behaviour.

2. **Adoption failure — nothing downstream uses it, so nothing keeps it honest.**
   The suite lands green and is never extended. #162 needs reconnect-under-restart, finds the two
   relevant tests `#[ignore]`d for want of an injectable timeout (contrary evidence 2), and writes
   its own ad-hoc mock rather than negotiating the seam. #208 does the same for multi-instance.
   The harness's justification was amortisation across five issues; if the first successor routes
   around it, the amortisation never happens and we have paid 900 lines for one PR's assertions.
   *Warning signs:* a second HQPlayer mock appearing anywhere under `tests/`; `#[ignore]` count not
   decreasing across the program.

3. **Opportunity cost — we built the harness the sans-io machine would have made unnecessary.**
   #162 extracts the protocol state machine into `src/hqplayer/` (the placeholder directory is
   already there with its `.gitkeep`). Framing, decoding and index resolution become pure functions
   testable in microseconds with exhaustive byte-offset splitting. The wire layer's chunk scripting
   is then dead weight for those cases, retained only for reconnect and idle-close. We spent the
   program's first PR building scaffolding for a shape we abandoned two PRs later, and the
   conformance suite's slowest tests are the ones with the least remaining value.
   *Warning signs:* #162's design review proposing sans-io extraction; conformance suite wall time
   growing past the rest of the test tree.

### Hidden Assumptions

| Assumption | Evidence | Risk if Wrong | Test |
|---|---|---|---|
| The socket is the only seam the adapter offers, so socket-level testing is forced | `HqpAdapter::configure(host, port, …)` (`src/adapters/hqplayer.rs:459`) is the only injection point; `parse_attr`, the read loop and `RESPONSE_TIMEOUT` are private/const (`src/adapters/hqplayer.rs:757-885`, `134`) | If a small, non-API-visible seam is acceptable, Option B's fast pure tests become available for defects 4,5,7,8,9,10,11 and C shrinks to the timing/verification cases only | Ask the epic owner whether making the framer and its timeouts internally injectable is in #322's scope or #162's; the answer changes the plan's size by hundreds of lines |
| Hand-transcribed fixtures from the HQPTuner audit faithfully represent hqplayerd 6.0.4 | `docs/protocol.md` marks findings **verified** against a live 6.0.4 Opal daemon, with concrete round-trips (`SetFilter value="6"` → index 6 `poly-sinc-lp`; `State filterNx="72"` ↔ `Status active_filter="sinc-Lh"`) | The corpus becomes confidently wrong, exactly as `docs/hqplayer-protocol-reference.md` was confidently silent on `result` | Run the `UHC_HQP_CONFORMANCE_HOST` opt-in suite read-only against a real daemon before stage 3 and diff every fixture claim; treat any unverified fixture as a known gap, not a fact |
| ~700–900 lines is the real cost | No comparable in-repo: largest existing mock is `tests/mock_servers/lms.rs` at 14 KB | Stage 2 blows its budget mid-build and lands a half-wired harness with no red-first evidence | Front-load the wire layer + defect-7 framing test (the review's adjustment) and re-estimate at that checkpoint before building `model.rs` |
| Downstream issues will use this harness rather than route around it | Program order in #310 puts #322 first *because* it is the foundation; no successor has been written yet | Amortisation never materialises; 900 lines serve one PR | Require #162's plan to cite specific `tests/hqplayer_conformance.rs` cases it extends; treat a second HQPlayer mock as a program-level regression |
| `docs/hqplayer-protocol-reference.md`'s index-not-enum-ID claim is right | Independently confirmed live by HQPTuner: `<SetFilter value="6"/>` selected index 6, while enum ID 6 is a different filter; `hqplayerd.xml` stores enum IDs (`filter="40"`) | If reversed, every existing setter is wrong in the opposite direction and #322's AC6 fixture encodes the error | The AC6 cross-lane fixture asserts both domains from the same observed list, so a reversal fails the pair rather than passing both |
| Conformance is achievable without a live daemon in CI | #322 AC8 requires exactly this; HQPTuner's `docs/testing.md` runs its default suite offline against wire-speaking fakes | Nothing — this one is well supported. It is listed because it is the assumption that makes the fake mandatory in some form, which is why the fake survives all dissent | Keep the default suite hermetic; gate every real-daemon assertion behind the env var and `#[ignore]` |

### Decision

**Recommendation:** ADJUST

**Reasoning:** The steel-man survives its two strongest attacks. Contrary evidence 5 — that
"goes red on day one" is convenience, not correctness — is the most interesting objection, and it
fails on the repo's own rule: AGENTS.md requires the failure to be *observed* before the fix, and
an unobservable contract is not a conformance boundary. Contrary evidence 4 is real but not
differentiating: every candidate except D transcribes rather than captures, and D was rejected for
reasons unrelated to provenance, so the answer is to make provenance a first-class artifact (it
already is, per-fixture) and to close the loop with the opt-in real-daemon mode.

What does *not* survive is the plan's treatment of timing. Contrary evidence 1 and 2 converge from
different directions on the same unresolved seam, and pre-mortem 2's most plausible trigger is
exactly the `#[ignore]` that trade-off 3 shipped. A plan cannot both claim AC2 coverage and
pre-concede that two of AC2's clauses may be skipped. Contrary evidence 3 compounds it: an estimate
with no local comparable, in a PR that also introduces a corpus and a new test binary, needs a
mid-build checkpoint rather than a single commitment.

None of this reopens the solution space. C remains the only candidate that satisfies the binding
constraint, and every objection above is an amendment to C rather than a case for A, B, D or E.
Hence ADJUST, not RECONSIDER.

**If ADJUST — the specific modifications, now folded into the selected plan:**

1. **Resolve the timing seam before writing `model.rs`, not after.** Stage 2's first action is an
   explicit, recorded decision on making `RESPONSE_TIMEOUT` and `MAX_RECONNECT_ATTEMPTS`
   internally injectable (no public API surface, so no `api-change-approved`). If it is declined,
   the two dependent criteria are recorded as an explicit partial deferral of AC2 with the epic's
   consent — never as a silent `#[ignore]`.
2. **No test asserts elapsed wall-clock time.** Adopt HQPTuner's rule 7 directly: assert what a
   retry/verify loop *concludes* and how many passes it makes, never how long it takes. Where a
   timeout must be provoked, the fake closes the connection rather than hanging, wherever that is
   faithful to the daemon.
3. **Build in risk order with a checkpoint.** Wire layer plus the defect-7 framing test
   (`<Status …><metadata …/>` split from `</Status>`) lands and goes red first. Re-estimate before
   building `model.rs`. If the framing test cannot be made to fail against the unmodified adapter,
   the design is wrong and we learn it in the first increment.
4. **Make the fake structurally unable to become the specification.** Every fixture file carries a
   provenance header (source, daemon version, verified|derived, date). `hqpd-5.x-legacy` is marked
   `UNVERIFIED` in-file and no assertion treats it as authoritative. A commit that edits both a
   fake's behaviour and a conformance assertion is a review flag.
5. **Bind the successors.** #162's plan must cite the specific `tests/hqplayer_conformance.rs`
   cases it extends; a second HQPlayer mock anywhere under `tests/` is a program-level regression.
6. **Close the provenance loop before stage 3.** Run the `UHC_HQP_CONFORMANCE_HOST` suite
   read-only against a real daemon and diff every fixture claim; ship remaining gaps as stated
   gaps, not as facts.
7. **Preserve `MockHqpServer` as a facade** (already required by the review) so
   `tests/adapter_integration.rs:511` and `tests/zones_sha_integration.rs:6` stay green.

**Confidence after dissent:** MEDIUM-HIGH. High that Option C is the right boundary — the binding
constraint is unambiguous and no objection produced a better candidate. Medium on cost and on the
timing seam, which is why modifications 1 and 3 exist to surface both early and cheaply.

**Create ADR?** **Yes — but not yet.** This is a one-way door: the choice of "corpus + scriptable
fake as protocol truth" will be inherited by #162, #208, #328, #329 and #330, and the reasoning
depends on context (`hqp-control` 6.0.1 / `hqplayerd` 6.0.4 evidence, the state of the mock in July
2026) that will not be obvious later. The right ADR is
`docs/adr/NNNN-hqplayer-conformance-boundary.md`, and it should be written in **stage 2** alongside
the code, once modification 1 has settled the timing seam and modification 3 has produced a real
cost figure — an ADR written now would have to hedge its two most consequential sections. Recorded
here so the stage-2 gate can check for it; `docs/adr/` already exists.

---

## Stage-1 gate status

| Gate requirement (#310 per-issue workflow) | Status |
|---|---|
| `/solution-space` run, ≥3 genuine candidates across ≥2 escalation levels | Met — 5 candidates across 4 levels |
| `/review` verdict Continue or Adjust, no unresolved blocker | Met — **Adjust**; both findings folded into the plan |
| `/dissent` verdict PROCEED or ADJUST, not RECONSIDER | Met — **ADJUST**; 7 modifications folded in |
| One-way architectural decision has an ADR or a stated reason not to create one yet | Met — ADR deferred to stage 2 with reason stated above |
| Outputs persisted to `.oh/issue-<number>-<slug>.md` | Met — this file |
| Planning artifact committed, draft PR opened, three reports posted as separate PR comments | See PR |
| Stop for Codex contract review before `/execute` | **Stopping here** |

No production or test behavior was changed in stage 1. No API route or payload was touched.
`api-change-approved` was not applied. Nothing was merged. #322 remains open.

---

## Stage 2 red checkpoint

**Updated:** 2026-07-29
**Status:** superseded — this was the first checkpoint only. It describes the branch at `9ac224c`,
before any production change. For the state of the branch now, read **Stage 2 execution record**
below. Kept verbatim because the gate asked for the checkpoint evidence to be durable.

### Pre-flight

| Check | Result |
|---|---|
| Aim clear | Yes — make the HQPlayer wire contract executable so a client defect is observable before it is fixed |
| Constraints known | Codex Stage 1 Gate Review binding decisions 1–5; AGENTS.md TDD and API Stability; no wall-clock assertions |
| Context loaded | `src/adapters/hqplayer.rs` framing loop (757-794), `connect()` (538-601), `configure()`/`save_config()` (404-513); `tests/mock_servers/hqplayer.rs`; `tests/adapter_integration.rs`; `src/config/mod.rs` `UHC_CONFIG_DIR` |
| Scope bounded | This increment: corpus + wire + one failing framing expectation. **Not** in this increment: daemon model, any production change, remaining ACs |
| Success criteria | The named test fails against the unmodified adapter *for the framing defect* — not for a compile error and not because the fake could not start |
| Baseline | `cargo build --tests` green before any edit |

A prior attempt in this session wrote `tests/mock_servers/hqplayer/model.rs` and 18 fixtures
*before* observing red. That was a gate violation. Both were removed from the worktree before this
checkpoint; `git diff --stat -- src/` was empty throughout.

### Changed files (RED commit `9ac224c`)

| File | Lines | Role |
|---|---|---|
| `tests/mock_servers/hqplayer/corpus.rs` | +116 | Document layer: loads provenance-carrying fixtures, parses the header, refuses a fixture without one |
| `tests/mock_servers/hqplayer/wire.rs` | +169 | Byte layer: serves a responder over TCP, can split one reply across TCP writes at a marker |
| `tests/fixtures/hqplayer/hqpd-6.0.4-opal/status_playing_with_metadata.xml` | +11 | The document under test, with provenance |
| `tests/hqplayer_conformance.rs` | +135 | The expectation, driving the real `HqpAdapter` over a real socket |
| `tests/mock_servers/hqplayer.rs` | +8 | Two `pub mod` declarations and a note. `MockHqpServer` itself untouched |

`public/tailwind.css` had to be regenerated with `make css` for the lib to compile
(`src/app/embedded_assets.rs:17` `include_str!`s it and it is gitignored). Not a code change.

### Command and failing output

```text
$ cargo test --test hqplayer_conformance

running 20 tests
test mock_servers::hqplayer::tests::mock_hqp_responds_to_getinfo ... ok
... (18 further mock_servers tests) ... ok
test state_read_after_status_with_metadata_child_reports_the_daemon_state ... FAILED

---- state_read_after_status_with_metadata_child_reports_the_daemon_state stdout ----
thread '...' panicked at tests/hqplayer_conformance.rs:125:5:
assertion `left == right` failed: State read after a Status document with a self-closing
metadata child must report the daemon's playback state (2 = playing). Got 0, which means the
Status read stopped at the metadata child's `/>` and left `</Status>` in the socket for this
command to consume. Full state: HqpState { state: 0, mode: 0, filter: 0, filter1x: None,
filter_nx: None, shaper: 0, rate: 0, volume: 0, active_mode: 0, active_rate: 0, invert: false,
convolution: false, repeat: 0, random: false, adaptive: false, filter_20k: false,
matrix_profile: "" }
  left: 0
 right: 2

test result: FAILED. 19 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out
```

**RED commit SHA:** `9ac224c8f852187ad00cb35cf06bebe38fae0959`

Not pushed. `.github/workflows/build.yml` triggers on `pull_request` to `v3` with
`types: [opened, synchronize, reopened, labeled]` and no draft filter, so pushing a red tip would
run full CI against a deliberately failing commit. Per the gate instruction the SHA and output are
recorded here instead; the branch will be pushed once the fix lands.

### Why this proves the production defect

1. **It is not a setup or compile failure.** The binary compiled and the other 19 tests in it
   passed. `adapter.connect().await.expect("connect to fake daemon")` succeeded, so the fake bound,
   accepted, and answered `GetInfo`. The single failure is the assertion.
2. **The failure signature is the desync, uniquely.** Every field of `HqpState` is its default:
   `state: 0`, `volume: 0`, `matrix_profile: ""`. The `State` document the fake sends carries
   `state="2" mode="1" volume="-23.5" matrix_profile="Default"`. A wholly default struct is what
   `parse_attr` returns when handed a document with no attributes at all — i.e. the bare
   `</Status>` left over from the previous read. Any other explanation (wrong attribute name, bad
   number parse) would corrupt some fields and not others.
3. **The mechanism is in the source.** `src/adapters/hqplayer.rs:781-786` ends a read when
   `trimmed.ends_with("/>")`. The verified `Status` shape puts a self-closing `<metadata …/>` child
   before `</Status>`, so that condition is true one document too early. The reference explicitly
   warns that a parser "must match the closing `</Status>` (not the first self-closing `/>`, which
   is the `metadata` element)" —
   <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>
   §6.
4. **The wire layer removes buffering luck.** `Chunking::AfterMarker("/>")` puts `</Status>` in a
   separate TCP segment, so the client cannot pass by accident of a single large read.
5. **The repo already knew and worked around it.** `connect()` carries the comment: *"Background
   fetch was removed due to response desync bugs - it used single-line read_line() which corrupted
   the TCP buffer when interleaved with multi-line responses."* A feature was deleted rather than
   the framer fixed, because there was no executable expectation. There is one now.

### Verification that the facade survived

```text
$ cargo test --test adapter_integration --test zones_sha_integration
running 42 tests ... test result: ok. 42 passed; 0 failed
running 20 tests ... test result: ok. 20 passed; 0 failed
```

Codex binding decision 3 is satisfied: `MockHqpServer` gained nothing but two module
declarations, and both existing consumers are green.

### Re-estimate of remaining work

Stage-1 dissent put the fake at 700–900 lines and required a re-estimate here before building the
model. Measured: the two layers that exist are **285 lines** (`wire.rs` 169, `corpus.rs` 116) and
the wire layer is close to complete for the framing and split-read criteria. Revised estimate:

| Remaining piece | Estimate | Note |
|---|---|---|
| `model.rs` — stateful daemon, mode-relative enumerations, `result` echo, fault injection (`reject_next`, `accept_but_ignore`, `apply_after_polls`, `external_change`) | 420–480 | Largest single piece. A working draft was written and discarded at this checkpoint; its shape is known, which is why this number is tighter than stage 1's |
| `wire.rs` additions — coalesced replies, connection drop, idle close, refuse-then-rebind for the restart window, request counters | 120–150 | Counters exist so reconnect tests can assert attempt counts rather than elapsed time |
| Corpus growth — ~17 further fixtures (enumerations per mode, volume-range variants, junk filters, matrix, persistent-config enum-ID document, UNVERIFIED 5.x profile) | ~200 (mostly data) | The discarded set is re-creatable directly |
| `MockHqpServer` re-implemented as a facade over the layers | 60–80 | Deferred from this checkpoint deliberately: the facade is only worth rewriting once the model exists |
| Conformance suite — remaining AC2/AC3/AC4/AC5/AC6/AC7/AC8 cases | 550–650 | |
| Production fixes — framer, `result` verification on setters, decimal-dB parse, injectable timeout/retry seam | 180–230 | Payload types stay put: `PipelineVolume.value`, `HqpState.volume` and `HqpVolumeRequest.value` remain `i32`, with decimal dB carried on new `#[serde(skip)]` fields, so no public route or payload changes |
| ADR `docs/adr/NNNN-hqplayer-conformance-boundary.md` | ~90 | Written once the timing seam is real, per the stage-1 dissent |

Total remaining ≈ **1,620–1,880 lines**, against stage 1's 700–900 for the fake alone. The fake
itself now looks like **825–995** (285 built + 540–710 remaining), which lands inside the stage-1
estimate; the growth is in the assertion suite and the fixtures, which stage 1 costed separately
and loosely. **The selected mechanism holds** — the framing defect was reproduced through the real
client on the first attempt, with no production change and no timing assertion, which was the
specific thing this checkpoint existed to test. Recommend continuing to the model.

### Not done at this checkpoint, by instruction

No `model.rs`, no production fix, no remaining ACs, no Stage 2 final reports, no `/review`,
no `/dissent`, no `/ship`, no push, draft unchanged, #322 open.

---

## Stage 2 execution record

**Updated:** 2026-07-29
**Status:** complete, stopped for Codex review. Not shipped, not merged, draft unchanged.

This section supersedes the *Stage 2 red checkpoint* snapshot above, which described the branch at
`9ac224c` only. Superego flagged that mismatch as its first finding; this is the fix.

### Commit chain (red before green, alternating, all local until this section was written)

| SHA | Kind | What |
|---|---|---|
| `1425baa` | docs | Stage-1 solution decision |
| `9ac224c` | **RED** | `Status` metadata child desynchronises the read stream |
| `bb32165` | docs | Red checkpoint evidence |
| `c7011cb` | **GREEN** | Frame by document, not by first `/>`; injectable `HqpTimeouts` |
| `a34e26a` | **RED** | Five client protocol defects the corpus exposes |
| `eb3d0b1` | **GREEN** | Scope attributes to the root tag, decode entities, decimal dB, verify `result` |
| `28c5a73` | **RED** | A setter reports success it never confirmed |
| `06cb40e` | **GREEN** | `verify_applied` readback primitive |
| `ebd5f1c` | style | `cargo fmt` |
| `5a261cb` | **RED** | Coalesced frames and mismatched nesting (no `src/` change: `git diff HEAD^..HEAD -- src` empty) |
| `4d419d8` | **GREEN** | Answer each command from its own reply; reject mismatched nesting |

Every production change is preceded by a failing expectation in an earlier commit. The exact failing
command and output for each RED is in that commit's message.

### Acceptance-criterion audit — all nine

62 conformance expectations, 0 ignored, 0 skipped, 0 encoding a known defect as expected behaviour.

| AC | Requirement | Status | Expectations |
|---|---|---|---|
| **AC1** | Stateful mock/fixtures cover GetInfo, State, Status with child metadata, VolumeRange, modes, rates, filters, shapers, representative advanced settings | **Met** | `get_info_reports_the_verified_daemon_identity`, `state_reports_settings_as_list_indices`, `state_carries_representative_advanced_settings`, `status_with_a_metadata_child_yields_the_playback_fields`, `status_reports_active_settings_as_display_names`, `volume_range_reports_bounds_and_flags`, `modes_list_distinguishes_list_index_from_enum_id`, `filters_list_is_parsed_in_full_from_a_multiline_container`, `shapers_list_is_parsed_in_full`, `rates_list_reports_hz_and_has_no_enum_id`, `enumerations_are_mode_relative_and_are_refetched_after_a_mode_change` |
| **AC2** | Split reads, coalesced responses, self-closing children, malformed/truncated XML, timeouts, reconnect boundaries | **Met** | split: `a_document_split_mid_attribute_across_tcp_writes_is_still_parsed`; coalesced (through `HqpAdapter`): `a_reply_coalesced_with_an_unsolicited_frame_still_answers_the_current_command`, `an_unsolicited_frame_coalesced_with_a_reply_does_not_corrupt_the_next_command`, plus `the_framer_ends_a_coalesced_buffer_at_the_first_document`; self-closing child: `state_read_after_status_with_metadata_child_reports_the_daemon_state`, `the_framer_finds_the_end_of_a_document_at_every_split_point`; malformed/truncated: `a_truncated_document_fails_instead_of_returning_partial_state`, `a_stray_closing_tag_is_rejected_as_malformed`, `a_document_with_mismatched_nesting_is_rejected_as_malformed`; timeouts: `a_silent_daemon_surfaces_a_timeout_rather_than_hanging`, `a_silent_daemon_is_retried_exactly_the_configured_number_of_times`; reconnect: `a_connection_dropped_mid_command_is_recovered_by_reconnecting` |
| **AC3** | Explicit `result=Error`, syntactic OK without state change, delayed state change, external changes | **Met** | `an_explicitly_rejected_setter_reports_the_daemon_reason`, `a_setter_accepted_but_not_applied_does_not_report_success`, `a_setter_whose_change_lands_after_a_poll_still_reports_success`, `a_change_made_by_another_controller_is_visible_on_the_next_read`, `an_unknown_command_is_reported_as_an_error_without_dropping_the_connection` |
| **AC4** | Fractional negative dB, fixed volume, adaptive volume, min/max/step, mute | **Met** | `a_fractional_negative_db_volume_round_trips`, `a_whole_db_volume_is_not_turned_into_a_fraction_on_the_wire`, `a_rounded_volume_is_never_reported_as_zero_db`, `a_fixed_volume_daemon_rejects_a_volume_change`, `a_fixed_volume_daemon_reports_volume_as_unavailable`, `an_adaptive_volume_daemon_reports_the_adaptive_flag`, `a_fractional_volume_step_is_preserved_rather_than_rounded_away`, `a_volume_range_that_omits_step_reports_it_as_absent`, `a_volume_below_the_daemon_floor_is_rejected`, `mute_is_toggled_on_the_daemon`, `a_volume_step_moves_the_level_by_the_advertised_step` |
| **AC5** | Name→native index asserted from observed list/state pairs, not hardcoded position | **Met** | `a_filter_name_is_sent_as_the_index_the_observed_list_gives_it`, `a_filter_name_is_not_sent_as_its_enum_id`, `the_same_filter_name_resolves_to_a_different_index_on_a_differently_ordered_daemon` |
| **AC6** | Cross-lane fixture: live uses list indices, persistent config stores enum IDs; conversions independent and never shared | **Met** | `the_persistent_configuration_lane_stores_enum_ids_not_list_indices`, `the_two_lanes_give_the_same_filter_name_different_numbers`, `feeding_a_persistent_enum_id_to_the_live_lane_is_rejected`. `corpus::index_of` and `corpus::enum_id_of` are separate functions with no shared body, by construction |
| **AC7** | Transport, seek, pipeline, persistent-restore response families have executable examples | **Met** | transport: `the_transport_family_moves_the_daemon_between_playback_states`, `the_track_change_family_advances_and_rewinds_the_queue`; seek: `the_seek_family_moves_the_playback_position`; pipeline: `the_pipeline_family_resolves_indices_back_to_display_names`, `the_matrix_profile_family_round_trips_a_name_containing_an_entity`; persistent-restore: `the_persistent_config_form_carries_the_verified_field_names`, `the_persistent_config_form_separates_the_unnamed_base_from_named_profiles`, `the_restore_response_family_carries_no_outcome_signal`, `the_restore_fixture_records_why_its_status_code_proves_nothing` |
| **AC8** | Runs in CI without HQPlayer; documented opt-in real-server mode | **Met** | The default suite needs no HQPlayer. `real_daemon_smoke_check_when_opted_in` documents `UHC_HQP_CONFORMANCE_HOST` / `UHC_HQP_CONFORMANCE_PORT`, is read-only, and *skips with a printed note* rather than being permanently `#[ignore]`d. AC8 asks for a documented opt-in mode to exist, and it does. **It is a connectivity/identity smoke check** — `GetInfo` plus `GetFilters` — and settles no fixture; the live corpus diff is stage-3 tier 1, specified in ADR 003. No real HQPlayer was available here, so no live result is claimed |
| **AC9** | Evidence and version provenance documented alongside each fixture | **Met** | Every one of the 17 fixtures carries a `source` / `daemon` / `status` / `date` / `notes` header. Enforced by `every_corpus_fixture_records_its_provenance`, `the_legacy_profile_is_marked_unverified_so_it_cannot_pass_as_protocol_truth`, `the_verified_profile_marks_excerpts_honestly` |

**No AC is deferred, ignored, or partially met beyond the one explicitly-pending item in AC8**, which
requires hardware this environment does not have.

### AC7's persistent-restore boundary, justified

Codex directed that the restore example must not reach a private production parser. It is therefore a
**corpus contract**: the fixtures assert the verified field names of the read side and the verified
absence of any outcome signal on the write side. Reasoning, also recorded in the test file:

- The profile-list *parse* already has public-surface coverage — `GET /hqplayer/profiles` is
  exercised in `tests/protocol_integration.rs` — so asserting it again here would duplicate coverage
  and would mean widening production visibility purely for test reach.
- What #322 owes this lane is the *response-family semantics*, which are properties of the documents.
- The restore transport — multipart upload, daemon self-restart, `/backup` readback polling — is
  #330's and is untouched.

### Production changes, and what they did not touch

All inside the native protocol client in `src/adapters/hqplayer.rs`:

| Change | Why |
|---|---|
| `pub mod framing` — `classify`, `root_open_tag`, `root_element`, `root_text`, `decode_entities` | Read until a document parses; reject mismatched nesting and stray closing tags; scope attribute lookups to the root element |
| `HqpTimeouts` + `set_timeouts`/`timeouts` | Codex-mandated internal seam so timeout and reconnect boundaries are testable without wall-clock waits. Defaults are the shipped constants |
| `check_result` | An explicit `result="Error"` is a failure; an absent `result` stays a success |
| `verify_applied` | Readback confirmation for `set_mode`, `set_filter_1x`, `set_filter_nx`, `set_shaper`, `set_rate` |
| `parse_attr_f64`, rounding fallbacks, `set_volume_db` | Decimal dB throughout |
| Reply-element matching in `send_command_inner` | Answer each command from its own reply |

**Not touched:** no route, no request schema, no response schema. The new decimal-dB and
`filter_junk` fields are `#[serde(skip)]`, so serialized payloads are byte-identical;
`HqpVolumeRequest.value` remains an integer. `tests/fixtures/api_routes.txt` is unchanged and
`api-change-approved` was not applied. `MockHqpServer` gained only module declarations.

One behaviour change is deliberate and worth naming: a setter the daemon silently ignores now returns
an error where it previously returned success, so `POST /hqplayer/pipeline` setter routes can surface
a failure they could not before. That is epic #311's no-false-success constraint, inside the
"command result/readback verification primitives" scope Codex set.

### Verification

| Command | Result |
|---|---|
| `cargo test --test hqplayer_conformance` | **81 passed; 0 failed; 0 ignored** (62 conformance + 19 pre-existing mock_servers) |
| `cargo test --test adapter_integration` | 42 passed — `MockHqpServer` facade intact |
| `cargo test --test zones_sha_integration` | 20 passed — second facade consumer intact |
| `cargo test --test api_contract` | 2 passed — no route drift |
| `cargo test --no-fail-fast` | All green except the two pre-existing `/hqp/discover` tests |
| `cargo fmt --check` | Clean |
| `cargo clippy -- -D warnings` | Clean (this is CI's invocation, per `.github/workflows/build.yml:369`) |
| `dx build --release --platform web --features web` | **Not run — `dx` is not installed here, and not applicable:** no file under `src/app/` changed |

**Pre-existing, environmental, not a regression:**
`client_harness::shared_endpoints::get_hqp_discover` and
`protocol_integration::api_endpoints::hqp_discover_returns_json` both return 500 because UDP
multicast to `239.192.0.199` is unavailable in this sandbox. A direct socket probe gives
`OSError [Errno 65] No route to host`. Both fail identically with this branch's `src` changes
stashed, and neither test file was modified.

**`cargo clippy --all-targets` is not the gate and was already failing on `v3`:** it lints
`#[cfg(test)]` modules inside the lib, which trip the crate's `deny(clippy::unwrap_used)` etc. The
untouched `src/config/mod.rs:406` alone accounts for 25 findings. Per Codex's instruction, unrelated
baseline lint was left alone.

### Superego review disposition

`sg review pr` (superego 0.9.4) against `v3`. Six findings, every one dispositioned:

| # | Finding | Disposition |
|---|---|---|
| 1 | The `.oh` doc claims a red checkpoint with no production code, but the diff has model, fixtures and production fixes — self-documentation not trustworthy | **Fixed.** This section supersedes the snapshot, which is now marked as such. The gate itself *was* respected: the commit chain above alternates RED and GREEN, and `5a261cb` contains no `src/` change |
| 2 | ~2,700 lines of test infrastructure in one PR is hard to review | **Accepted, with mitigation.** The PR is not splittable — #310's contract is one PR per issue — but it is reviewable commit-by-commit, each RED naming its own failing output and each GREEN naming what it turns green |
| 3 | `set_timeouts` is `pub` and callable from production | **Fixed.** An integration-test crate cannot reach `pub(crate)`, so the guard is a lint instead: `no_production_code_retunes_the_timeout_seam` fails if any `src/` file calls it, in the same spirit as the repo's existing `architecture_lint` |
| 4 | `verify_applied` costs an extra round trip per setter in production | **Accepted and documented** in ADR 003 under Consequences. It is the point: #311 forbids unverified success. The lever if it ever hurts is the retry budget, not removing the readback |
| 5 | Volume skips readback verification — intentional asymmetry, worth being explicit | **Confirmed, no change.** Already stated in `set_volume_db`'s doc comment and now in ADR 003 |
| 6 | `decode_entities`' `semi <= 10` bound reads as arbitrary | **Fixed.** Comment added: named references reach 6 chars (`&apos;`), the longest numeric form is `&#x10FFFF;` at 10, so a `;` beyond that belongs to later text |

### ADR

`docs/adr/003-hqplayer-conformance-boundary.md`, written now rather than in stage 1 exactly as the
stage-1 dissent required — after the timing seam was settled and the cost was measured, so its two
most consequential sections did not have to hedge.

`docs/hqplayer-protocol-reference.md` is demoted from authority to reader's guide, with a correction
table naming each claim the corpus overturned and the expectation that pins the correction.

### Stage-2 dissent adjustments

`/dissent` on the implemented mechanism returned **ADJUST** with five modifications. One was
implementable inside #322's scope and is implemented; the rest are recorded obligations.

| # | Modification | Status |
|---|---|---|
| 1 | Pin what a verified setter does when another controller intervenes between acknowledgement and readback | **Implemented.** `a_setter_overridden_by_another_controller_fails_and_names_the_observed_value`. The behaviour turned out to be already correct — the setter fails and the error names the daemon's actual value — so no production change was needed. The gap was that it was *unspecified and untested*, which is the one thing a conformance boundary must not leave open. Now a contract, and recorded in ADR 003 |
| 2 | Make the unsolicited-document skip path observable, and have stage 3 assert the counter stays zero across every command family | **Recorded for stage 3**, and its safety rationale is now obsolete: the per-command deadline bounds the wait regardless of the count, so an observable counter is diagnostics rather than protection. Still worth having; still not worth production surface whose only consumer is a stage-3 assertion |
| 3 | Bind stage 3 to the fidelity close-out — every `derived-excerpt`/`UNVERIFIED` fixture either re-provenanced from a live diff or shipped as a stated gap | **Recorded for stage 3** |
| 4 | Require every new malformed-input expectation to cite a reference passage or an observed capture | **Recorded as a suite convention** |
| 5 | Hand the failure-versus-divergence question to #329 in writing | **Recorded in ADR 003 Consequences** |

The gap dissent found is worth naming plainly: the harness already had `external_change`, and no
expectation combined it with a verified setter. The tests were green and blind at the same time.

### Correction: what the opt-in real-daemon mode does and does not do

An earlier draft of this artifact and of the Stage-2 reports described "running the opt-in suite" as
the merge-gate verification that settles the corpus. That overstated it, and the gate caught it.

`real_daemon_smoke_check_when_opted_in` (renamed from `real_daemon_conformance_when_opted_in`, which
was itself part of the overclaim) calls `GetInfo` and `GetFilters` and asserts the daemon identifies
itself and returns a multi-entry container. That is a smoke check. It satisfies AC8's
existence-and-documentation requirement and nothing beyond it.

The live corpus diff is **stage-3 work that does not exist yet**, and ADR 003 now specifies it in two
tiers that must not be conflated:

- **Tier 1, read-only** — capture and diff `GetInfo`, `GetModes`, `GetFilters`, `GetShapers`,
  `GetRates`, `GetJunkFilters`, `State`, `Status`, `VolumeRange`, the matrix family and the `/config`
  read side against the corpus, per mode. Safe against any daemon. **This is what the real-daemon
  merge gate means.**
- **Tier 2, mutating** — the `Set*` anchors, whose evidence is a state change and which therefore
  need a dedicated or expendable daemon, a separate opt-in variable, capture-and-restore, and a human
  present. Never a merge gate, never in CI.

The consequence worth stating plainly: the `Set*` anchor claims stay **derived** until someone runs
tier 2 deliberately. No read-only run, however thorough, can confirm them.

### Correction: the unsolicited-document bound

Recorded because the mistake is instructive and the gate caught it, not me.

Superego flagged the original `MAX_UNSOLICITED_DOCUMENTS = 8` as an unjustified constant outside the
injectable seam. I "fixed" it by deriving 64 from the daemon's ~1–2 Hz push cadence and exposing it as
`HqpTimeouts::max_unsolicited`. Codex rejected that, correctly: `response` bounds a single **read**, so
every skipped document reset it, and a bigger count meant a reply-less command could survive roughly
32–64 s instead of ~4–8 s. I had reasoned about frame counts while the clock was the thing at risk.

The actual fix is one overall per-command deadline: `send_command_inner` computes it once and gives
each read only the remainder, so skipping an unsolicited document costs exactly what waiting for a
wanted one costs. Unsolicited traffic cannot extend a command's wait by construction rather than by
tuning. The public seam went back to four fields; the skip ceiling is a private
`MAX_UNSOLICITED_BACKLOG = 256` whose only job is to stop unbounded work, and whose doc comment
records why the public knob was wrong so nobody re-adds it.

Reproduced at test scale before the fix — `continuous_unsolicited_traffic_cannot_extend_the_command_deadline`
consumed **133 frames on a 300 ms budget** (~2.7 s), and the suite's own wall time went 0.65 s → 2.89 s.
Both are back to normal after it.

---

## Stage 3 — tier-1 read-only live verification

**Updated:** 2026-07-29 · **HEAD `75eeecc`** · draft PR, no live result claimed

Implements the merge gate ADR 003 specifies. Read-only by construction: every call on the capture
path is a query — no `Set*`, no `Volume*`, no transport, no matrix set — so it is safe against a
daemon someone is listening to.

| Commit | Kind | What |
|---|---|---|
| `bd87e21` | **RED** (with `src` stub) | Differ, skip counter, inactive-mode disclosure. `86/3` |
| `89549d4` | GREEN | Tier-1 capture + diff, live gate, runbook |
| `d7f62c2` | **RED** (with `src` stub) | Junk filters, current matrix profile, JSON artifact, deadline verdicts. `92/4` |
| `7086093` | GREEN | All four |
| `f3f8166` | **RED** (test-only) | Full capture artifact, marker emission, matrix diffing, matrix semantics, read-failure distinction. `96/8` |
| `75eeecc` | GREEN | All six, plus two bugs the coverage exposed |

Two REDs (`bd87e21`, `d7f62c2`) each added a deliberately-failing **production** stub to
`src/adapters/hqplayer.rs` so their expectations could run rather than fail to compile —
`unsolicited_skipped()` → `0`, and `get_junk_filters()` → `Ok(vec![])`. Both were replaced in the
following GREEN. They are **not** test-only REDs and must not be read as such. `f3f8166` is test-only:
`git diff -- src` is empty for it.

### Two bugs the new coverage exposed, both mine

`75eeecc`'s commit message was truncated by a shell-quoting accident — an unescaped ampersand — and
the branch is already pushed, so amending it would need a force-push that AGENTS.md forbids. The full
text belongs somewhere durable, so it is here:

1. **The differ compared escaped names against decoded ones.** Corpus documents hold attribute values
   in escaped wire form; the client returns them decoded. So a matrix profile named `Rock &amp; Roll`
   in the fixture and returned as `Rock & Roll` by the client diverged *from itself* — two false
   divergences, on real hardware, for any name carrying an entity. Fixed by decoding expected names
   with `framing::decode_entities`, the same function the client uses, so the comparison is exactly
   apples to apples. This was invisible until `MatrixListProfiles` joined the diffed families, because
   the matrix fixture is the only one with an entity in a name.
2. **`tier1_diffs_the_matrix_profile_list_against_the_corpus` was vacuous.** It trimmed `"Headphones"`,
   which lives in the `/config` form fixture, not `matrix_profiles.xml`, so it removed nothing and
   asserted on a divergence that could never appear. It now trims `"Speakers"`, which is actually in
   the matrix family.

### Verification at `75eeecc` (historical — superseded, see the final section)

| Command | Result |
|---|---|
| `cargo test --test hqplayer_conformance` | **104 passed; 0 failed; 0 ignored** (85 conformance + 19 pre-existing `mock_servers`) |
| lib unit tests | 84 passed |
| `--test adapter_integration` / `--test zones_sha_integration` | 42 / 20 |
| `--test protocol_schema` / `--test api_contract` | 41 / 2 |
| `cargo fmt --check` | clean |
| `cargo clippy -- -D warnings` | clean |
| `git diff --check origin/v3...HEAD` | clean |
| opt-in gate with `UHC_HQP_CONFORMANCE_HOST` unset | skips with a note; `1 passed` |

Twenty-eight expectations across the branch failed before they passed. Three known failures remain,
all pre-existing: the two deterministic `/hqp/discover` multicast 500s and the ~1-in-10
`error_handling::lms_fails_gracefully_when_unconfigured` concurrency flake.

### What tier 1 cannot do, stated so a clean run cannot be over-read

- **Per-mode enumerations.** `GetFilters`/`GetShapers`/`GetRates` are mode-relative and reaching the
  inactive mode needs `SetMode`. Tier 1 captures whichever mode the daemon is already in and records
  which; the other mode stays derived until tier 2.
- **The `Set*` anchors.** Their evidence *is* a state change. Tier 2 only, with an expendable daemon,
  a separate opt-in, capture-and-restore and a human present. Never a merge gate.
- **The live gate's own env handling and connect path.** `capture`/`diff`/`render`/`artifact_block`
  are all proven hermetically against the fake; the gate's variable parsing and its connection to real
  hardware are exercised only by an actual run.

---

## Fourth Codex acceptance audit — closure at `698bdf3`

The audit named seven findings. Six were closed in `5d12c3c`. Two survived it, and both were found by
my own post-fix audit rather than by a test — which is itself the finding worth recording.

### The record `5d12c3c` got wrong

Its message stated *"The page never enters the capture."* That was false when written. A string
replacement in that commit had silently failed, so `tier1.rs` still ran
`c.raw_documents.insert("config_form".to_string(), clean)` — the sanitised `/config` page was retained,
which is exactly the vector finding 3 named. No test asserted the page's *absence*, so nothing failed.
The commit could not be amended (no force-push), so the correction lives in `698bdf3`'s message.

### Two defects that survived the fix for them

| # | Defect | Evidence |
|---|---|---|
| 3 | `sanitize()` redacted *tags* only, stranding secret element text. `<password>SEKRET</password>` → `<!-- redacted -->SEKRET<!-- redacted -->` | RED: the leak test listed every hostile document whose secret survived |
| 7 | `capture()` sourced `config_profiles` from the **semantic** parser — assigned from the raw `/config` projection, then unconditionally overwritten in *both* arms of the `fetch_profiles()` match | RED: `got ["semantic-z"]` where the raw lane had served `raw-a`/`raw-b` |

Finding 7 is the more instructive one: *evidence reconstructed after semantic parsing* is the exact
thing Codex told me not to do, and I reintroduced it inside the commit that claimed to fix it. The raw
lane is now authoritative and `fetch_profiles()` is demoted to a cross-check under
`config_profiles_semantic`; when the two lanes disagree the differ raises
`DivergenceKind::LaneDisagreement` rather than resolving it in either side's favour. A disagreement
between lanes is a finding about the *client*, and a conformance tool that silently picks a winner has
destroyed the only evidence that mattered.

### Why both survived: no hermetic coverage of the web lane

Every other family had a fake. The `/config` read side had none — it was reachable only from live
hardware, so nothing exercised it in CI, and two defects sat in it undetected. `FakeConfigWeb` now
serves `/config` and `/config/profile/load` with deliberately different profile names, which makes the
*source* of the evidence observable rather than merely its content.

### The drift the fix nearly introduced

Consolidating revealed the redaction marker list had become **three copies**, already diverged
(`"apikey"` in the sanitiser, `"key"` in the config projection). That is the same failure mode the list
exists to prevent: extend one copy, miss the others, reopen the hole. Now one
`raw::SENSITIVE_MARKERS`/`is_sensitive` using the union, with
`the_redaction_marker_list_has_exactly_one_definition` as a standing lint. The hostile-tag test was
likewise duplicated across the `Start` and `Empty` arms and is now one `is_hostile_tag()`.

Two deliberate accepted trade-offs, recorded rather than left implicit:

- `"key"` is broad and now drives *text* redaction too, so an element named `monkey` would lose its
  text. Accepted: HQPlayer's vocabulary (`State`, `Status`, `FiltersItem`, `index`, `value`, `arg`,
  `rate`) contains no such name, and over-redaction costs evidence while under-redaction costs a secret.
- `"hidden"` matching was *narrowed* to `type="hidden"` specifically, so an unrelated `status="Hidden"`
  no longer costs a whole element. Secret-*named* attributes remain covered by `is_sensitive`.

### Verification at `698bdf3` (final head of this stage)

| Command | Result |
|---|---|
| `cargo test --test hqplayer_conformance` | **138 passed; 0 failed; 0 ignored** (was 132) |
| `cargo test --no-fail-fast` (whole suite) | **447 passed** |
| `cargo test --test api_contract` | 2 passed — public routes and payloads untouched |
| `cargo fmt --check` | clean |
| `cargo clippy -- -D warnings` | clean |
| `git diff --check` | clean |
| `sg review` | no blocking concerns; every point it raised was fixed, not deferred |

Three `sg review` findings were acted on rather than acknowledged: the dead `config_profiles`
assignment (which *was* defect 7), the unescaped-text well-formedness regression, and the
over-broad `"hidden"` match. Sanitised output must still reparse — the artifact embeds these documents
as evidence, and output that no longer parses is not evidence.

**Two suite failures remain and are not from this branch.**
`protocol_integration::api_endpoints::hqp_discover_returns_json` and
`client_harness::shared_endpoints::get_hqp_discover` both reproduce identically on an untouched `v3`
worktree (`0a1b02c`): `/hqp/discover` returns 500 in this sandbox, where network discovery is
unavailable. Verified by building and running `v3` in a throwaway worktree, not inferred.

A quiet semantic change worth flagging: the `config_profiles` latency key now measures the `/config`
page fetch, and the semantic call's timing moved to `config_profiles_semantic`. Both are test-only —
no production timing path or `HqpTimeouts` value reads either.

### Still at the hardware gate

Nothing here has touched a real daemon. Every tier-1 result to date is hermetic, against the fake.
The corpus cannot be settled until the gate below is run on hardware, and no live claim will be made
before then.

---
---

# HQPTuner Stage 1 amendment (2026-07-29)

**Status:** Stage 1 decision only. **No `src` or `tests` file was changed in this turn.** Nothing was
merged, the PR stays draft, no API route or payload was touched, `api-change-approved` was not
applied, and Stage 2 has not begun.

**Why this exists.** Issue #322 gained two new acceptance sections — *HQPTuner salvage regression
coverage (2026-07-30)* and *Beta/dev state-model fixtures (2026-07-30)* — after the original Stage 1
decision and the whole of Stage 2 were complete. Seventeen bullets were added to a plan that had
already been decided, so they have never been through `/solution-space`, `/review` or `/dissent`.
PR #337's own body warns against exactly this: *"Its existing checks must not be described as
covering these additions unless the exact regression is present."* This amendment reframes only
those additions.

**Evidence base.** Read in full for this amendment: `AGENTS.md`; the complete current #322 body;
issues #347, #341, #348; PR #337 including reconciliation comment 5125711271; both salvage reports
(`/tmp/hqptuner-salvage.Ahcwce/UHC-SALVAGE-UI-DATA-INTEGRATION.md`,
`.../uhc-salvage-beta-dev/UHC-SALVAGE-BETA-DEV.md`). **Every classification below was checked against
the tests and the model at `6b8e97f`, not against issue prose.** `rna-mcp` returned no metis,
guardrail or signal artifacts for this area, and no local outcome artifact exists for `80222d6d`, so
the only prior situated judgment remains `.oh/hqplayer-spec.md` and ADR 003.

**What was not read.** HQPTuner's own repository. Every upstream claim here is sourced from the two
salvage reports — reports *about* a repo, not the repo. That distinction produced a real error in
this amendment's first draft; see **Two corrections** below.

## Framing rejected up front

- **HQPTuner framing is not imported.** Its fake reads 4096 bytes and splits on `?>`, satisfying none
  of UHC's fragmentation, coalescing, root-boundary or response-size criteria. UHC's `wire.rs` is
  strictly stronger here and stays. Only the *state model* is a design reference.
- **No DSP causal claims.** The junk-filter advisor's cause verdicts, any spectral-signature →
  cause inference, the metering side channel, and any measurement of a filter's specification
  (corner, transition width, slope, taps, stop-band attenuation) are out of #322 and out of UHC's
  permitted evidence-acquisition boundary. #348 owns that guardrail.
- **Upstream ≠ qualified.** Every `[source]`, chain and pin behaviour below is an upstream
  observation on one daemon (hqplayerd 6.0.4, Opal, one host), pending #332.

## Classification of all seventeen added bullets

Legend: **A** exact coverage already present · **B** missing harness/fixture capability, belongs in
#322 · **C** production client semantics, owned by #347 · **D** evidence/provenance, owned by #341 or
#348. Bullets spanning more than one class are split.

### A — already covered, with test and line evidence

| # | Added behaviour | Where it is already satisfied |
|---|---|---|
| A1 | Setters mutate state (bullet 45) | `Inner::apply` (`tests/mock_servers/hqplayer/model.rs:566-575`) applies a `Change` to `DaemonState`; every `Set*` arm routes through it |
| A2 | Responses echo the command element with real result semantics (45) | `Inner::ok`/`Inner::error` (`model.rs:528-541`) echo the request element; `check_result` (`src/adapters/hqplayer.rs:1054`) reads it. Tests `an_explicitly_rejected_setter_reports_the_daemon_reason` (`tests/hqplayer_conformance.rs:637`), `an_unknown_command_is_reported_as_an_error_without_dropping_the_connection` (`:674`) |
| A3 | Index 0 means Auto where observed (45) | `rates_pcm.xml`/`rates_sdm.xml` both carry `RatesItem index="0" rate="0"`; enforced live by `tier1_requires_rate_index_zero_to_be_auto` (`:2250`) |
| A4 | Mode lists include `[source]` (45) | `modes.xml` — `index="0" name="[source]" value="-1"`. Test `modes_list_distinguishes_list_index_from_enum_id` (`:224`) |
| A5 | `Status` carries child metadata (45) | `render_status` emits a self-closing `<metadata/>` (`model.rs:466-483`); tests `:182`, `:313`, `:2130` |
| A6 | Decimal volume exercised (45) | `DaemonState::volume_db` defaults to `-23.5` (`model.rs:143`); eleven AC4 tests, `:696`–`:857` |
| A7 | `VolumeRange` does not invent a wire step (45) | `step_db: Option<f64>` with `None` reproducing the verified sample (`model.rs:57-58`); test `a_volume_range_that_omits_step_reports_it_as_absent` (`:792`) |
| A8 | Framing waits for the actual document root close (46) | `framing::classify` + root-element matching (`src/adapters/hqplayer.rs:1131-1197`); tests `:313`, `:341`, `:548`, `:569` |
| A9 | EOF mid-document is rejected (46) | `"Connection closed mid-document after {} bytes"` (`src/adapters/hqplayer.rs:1211-1216`); test `a_truncated_document_fails_instead_of_returning_partial_state` (`:362`) |
| A10 | Bare-ampersand tolerance (46) | Already handled — `decode_entities`' fall-through pushes `&` and continues rather than swallowing text. **Untested, not broken**; see B7 |
| A11 | `result=Error` is failure; `OK` with unchanged `State` is also failure; bare response is command-specific (47) | `check_result` (`:1054`), `verify_applied` (`:1537`), and the `SetAdaptiveVolume` bare-reply arm (`model.rs:779-785`). Tests `:593`, `:612`, `:637`, `:1375` |
| A12 | `SetFilter value` alone changes both chains — *daemon side* (48) | Modelled at `model.rs:723-724`: `s.filter_1x_index = one_x.unwrap_or(nx)` |
| A13 | External mode change is covered (49) | `a_change_made_by_another_controller_is_visible_on_the_next_read` (`:658`), `a_setter_overridden_by_another_controller_fails_and_names_the_observed_value` (`:1375`) |
| A14 | Never switch modes merely to pre-capture an inactive list (49) | Tier 1 is read-only by construction and *discloses* the gap instead of reaching for it: `tier1_records_the_inactive_mode_lists_as_not_captured` (`:1526`) |
| A15 | `SetRate` distinguishes list index from Hz (50) | `set_rate` resolves Hz → index and sends the index (`src/adapters/hqplayer.rs:1787-1815`); `rates_*.xml` carry `rate` and no `value`, pinned by `rates_list_reports_hz_and_has_no_enum_id` (`:270`) |
| A16 | Fixture provenance records edition/version/date (52) | `Provenance{source,daemon,status,date,notes}` (`corpus.rs:26-32`), enforced by `every_corpus_fixture_records_its_provenance` (`:1264`) |
| A17 | Command-keyed deaf behaviour, not only a magic-value sentinel (62) | **Already exactly this.** `Faults::accept_but_ignore: Vec<String>` is keyed by *element name* (`model.rs:169`), consumed at `:552` and `:566`. UHC never had a value-keyed sentinel, so the "not only" clause is satisfied by construction. Test `a_setter_accepted_but_not_applied_does_not_report_success` (`:593`) |
| A18 | `SetMode` resets `State.rate` to Auto/0 — *daemon side* (63) | `model.rs:700-708` sets `rate_index = 0` and `active_rate_hz = 0`, and a same-mode write takes the same path |
| A19 | One mutable daemon state shared across connections (65) | Structural: one `Arc<dyn Responder>` serves every accepted connection (`tests/hqplayer_conformance.rs:73`, `wire.rs:175-181`) over `Arc<Mutex<Inner>>` (`model.rs:241-244`). Observable via `:658`. **Caveat:** no test opens two simultaneous client connections; the guarantee is the `Arc`, not an assertion |
| A20 | Do not import HQPTuner framing (66) | Held. `wire.rs` owns chunking, coalescing, silence, unsolicited streams and drops; nothing from HQPTuner's framing is present or proposed |

**Consequence for the issue body:** bullets 45, 47, 62, 65 and 66 are met in full today, and 46, 48,
49, 50, 52 and 63 are partly met. Leaving them unticked misreports #322's state in the opposite
direction from the over-claiming PR #337 was warned about.

### B — missing harness/fixture capability, belongs in #322

| # | Gap | Evidence that it is absent |
|---|---|---|
| B1 | **No loaded chain distinct from configured mode** (59) | `Inner::sdm()` is literally `self.state.mode_index == 2` (`model.rs:334`) and is the sole enumeration resolver (`:338-352`). Under `[source]` (`mode_index == 0`) the model serves the **PCM** lists unconditionally, and no `DaemonState` field can express an SDM chain while `State.mode` stays 0 |
| B2 | **No source-following chain change mid-session** (59) | Follows from B1; `external_change` has no field to move |
| B3 | **Chain-scoped enumerations with divergent IDs/indices** (60) | *Indices* already diverge (`poly-sinc-gauss-long` is index 7 in `filters_pcm.xml`, index 1 in `filters_sdm.xml`). *Enum IDs* do not — 40 in both chains and in the legacy profile; `sinc-Lh` 72, `poly-sinc-ext2` 16, `poly-sinc-short-lp` 30 likewise. **See correction 1: the live-lane half of this is an open evidence question, not a fixture defect.** The *evidenced* half is B4 |
| B4 | **The persistent lane has no second numbering domain** | `persistent_config.xml` is `<output mode="0" filter="40" filter1x="6" shaper="5" rate="0"/>` — no `oversampling` attribute at all, so the cross-lane fixture cannot express the SDM persistent domain the salvage evidence actually names |
| B5 | **No device-mode-omission fixture** (61) | `hqpd-6.0.4-opal/modes.xml` and `hqpd-5.x-legacy/modes.xml` both carry all three entries. No fixture omits SDM with remaining indices intact |
| B6 | **No response accumulation cap, and no way to provoke one** (46) | `response.push_str(&line)` (`src/adapters/hqplayer.rs:1156`) is unbounded. Only a document *count* (`MAX_UNSOLICITED_BACKLOG = 256`, `:414`) and the per-command deadline bound it. `WirePolicy` (`wire.rs:54-79`) cannot emit an oversized document. `grep` for `oversiz|size_limit|MAX_RESPONSE|max_bytes` across `src` and the suite: no matches |
| B7 | **No bare-`&` or double-escaped attribute fixture** (46) | Only single-escaped decode is covered (`the_matrix_profile_family_round_trips_a_name_containing_an_entity`, `:1163`). Bare `&` works but is unpinned; double-escaping is an evidence question — see D3 |
| B8 | **No root recovery when a `metadata` child will not parse** (46) | No `_recover_root` equivalent exists (`grep recover` in `src/adapters/hqplayer.rs` and the suite: no matches). A `Status` whose `<metadata>` carries unescaped `<` or `"` classifies `Malformed` and errors — **on every poll while a track is loaded** |
| B9 | **No `SetRate` expectation whatsoever** (50) | `grep -n "set_rate\|SetRate" tests/hqplayer_conformance.rs` returns **no matches**. The model implements `SetRate` (`model.rs:741-760`) but nothing drives `adapter.set_rate()`. Exact-Hz-pin semantics, `[source]` accept-and-ignore, mode-varying and empty rate lists: all unexercised |
| B10 | **`[source]` cannot accept-and-ignore a rate pin** (50) | `SetRate` applies unconditionally (`model.rs:741-760`); no mode gate. `accept_but_ignore("SetRate")` yields OK-without-mutation but not the mode-conditional semantics, and `Status.active_rate` is not held independently |
| B11 | **No expectation observes `SetMode` clearing the pin, same-mode or otherwise** (63) | The model does it (A18) but `set_mode` appears in the suite only at `:286` (list refetch, which asserts list *length* only) and `:678` (error path) |
| B12 | **`Status.active_mode` echo and `active_rate` family are not fixture-driven** (64) | `render_state` sets `active_mode = s.mode_index` (`model.rs:411`) and `render_status` sets it from `name_at_index(GetModes, mode_index)` (`:431`). Both echo the configured mode, so the fake agrees with itself by construction and no fixture carries the disputed case |
| B13 | **Provenance has no playback-state field** (52) | `Provenance` is source/daemon/status/date/notes (`corpus.rs:26-32`); `daemon` is free text. The added bullet requires playback state as a recorded dimension |
| B14 | **Unevidenced fake behaviour: `SetMode` resets filters and shaper to index 0** | `model.rs:704-706` sets `filter_1x_index`, `filter_nx_index` and `shaper_index` to 0 on every `SetMode`. Upstream evidence says `SetMode` clears the *rate pin* and reloads the chain; it does not say filters reset to index 0, and no fixture provenance carries it |
| B15 | **A #322 expectation depends on a behaviour #347 must delete** | `an_unknown_command_is_reported_as_an_error_without_dropping_the_connection` (`:674`) drives `set_mode("99")`, which reaches the daemon **only** via the numeric-string fallback at `src/adapters/hqplayer.rs:1603`. Retargeting it is #322 work |
| B16 | **No fixture where the source rate and the output rate differ** | The model can express it (`model.rs:455-459` takes root `samplerate` from metadata while `active_rate` is separate) but nothing pins the domain split. `Status.active_bits` itself *is* already parsed — `src/adapters/hqplayer.rs:1247`, `:1390`, consumed `:2638` — so only the fixture is missing. The consuming semantics belong to **#328**, which is outside this amendment's four classes and is named here rather than force-fitted |

### C — production client semantics, owned by #347

Every row is a defect in `src/adapters/hqplayer.rs` today. **None of them is #322 work**, and #322 must
not be described as covering them.

| # | Behaviour | Site |
|---|---|---|
| C1 | No `[source]` guard on rate writes; `set_rate` sends unconditionally | `:1787-1815` |
| C2 | No client-held per-family rate memory and no re-assertion after a mode change | absent |
| C3 | A no-op `SetMode` is written rather than skipped, destroying the pin | `set_mode`, `:1574-1584` |
| C4 | No loaded-chain observer; `refresh_lists()` runs only after UHC's own `set_mode` | `:1580` |
| C5 | `SetFilter` may be sent with an absent sibling, and the one-sided helpers *guess* it | `set_filter(value, value1x: Option<u32>)` `:1630`; `state.filter_nx.unwrap_or(state.filter)` `:1661`; `state.filter1x.unwrap_or(state.filter)` `:1678` |
| C6 | Numeric-string fallback in every resolver | `resolve_mode_index:1603`, `resolve_filter_index:1708`, `resolve_shaper_index:1769` |
| C7 | Mode matching is exact equality, so `SDM (DSD)` needs prefix/alias handling | `:1591` and the `eq_ignore_ascii_case` calls around it |
| C8 | Volume is result-checked but deliberately not readback-verified | `set_volume_db` doc comment |
| C9 | Auto (index 0) in `[source]` reports **success** for an ignored command — readback compares 0 against 0 | consequence of `verify_applied("rate", …)` at `:1815` |

C9 was found by dissent and is not in any issue's AC list yet. It belongs to #347's *ignored*
outcome, and #322 structurally cannot catch it.

### D — evidence and provenance, owned by #341 or #348

| # | Item | Owner |
|---|---|---|
| D1 | `State.active_mode` vs `Status.active_mode` resolved as a **versioned** question rather than a global choice (51) | #341 (its AC already names it). #322 owns only *not pretending* — see B12 |
| D2 | The `SetRate` semantics ledger: index on the wire, exact runtime Hz pin, Auto behaviour, mode/filter/device-dependent lists, mode switch clears the pin, marked upstream-pending (50) | #341 |
| D3 | Whether the daemon emits **double-escaped** attribute values on the wire, or whether the double-escape is an artefact of `hqp-control`'s own XML parse. UHC substring-scans and decodes once; HQPTuner XML-parses then decodes again. One pass may be correct *for UHC's pipeline* | #341 — an evidence question, not a coverage gap |
| D4 | Negative findings recorded so they are not retried: partial `POST /config` returns HTTP 200 without applying; `profile/save`/`profile/load` are unsafe as a durable preset store; self-generated 4321 session-authentication keys were rejected (53) | #341 |
| D5 | The `.oh/hqplayer-spec.md` vs `docs/hqplayer-protocol-reference.md` `SetMode` value-vs-index contradiction | #341 |
| D6 | MIT attribution for lifted HQPTuner or official `hqp-control` material (52). **No `THIRD-PARTY-NOTICES` file exists in this repository** — verified, and no commit has ever added one | #348 |
| D7 | Manual-derived Signalyst prose not copied; evidence-acquisition boundary (no DSP characterisation, no disassembly, ambiguous probes stop for approval) | #348 |
| D8 | Private upstream correspondence cited at a high level only, never reproduced | #348 |

**Tally:** 20 already-covered findings across 11 bullets, 16 genuine #322 gaps, 9 #347 defects, 8
#341/#348 items. Not one #347 row is proposed as #322 work.

## Two corrections, both found by challenge rather than by the suite

Recorded prominently because in both cases the mistake was mine and the mechanism that caught it is
the part worth keeping.

### Correction 1 — the "corpus contradicts the evidence" finding was wrong

The `/review` pass concluded that `filters_pcm.xml` and `filters_sdm.xml` "encode the opposite of the
evidence" by giving `poly-sinc-gauss-long` enum ID 40 in both chains, and proposed renumbering the SDM
fixture to 38 as the amendment's **first** bite.

`/dissent` re-read the source quotations. Both salvage reports say the same thing in the same words:
`poly-sinc-gauss-long` is "enum 40 under PCM **`filter`** and 38 under SDM **`oversampling`**", and
`sinc-M` is "25 under `filter`, 23 under `oversampling`" (`livemap.py:17-18`). **`filter` and
`oversampling` are `hqplayerd.xml` / config-form attribute names, not two modes of `GetFilters`.** The
cited evidence establishes that the *persistent* lane carries two separately-numbered enumerations. It
does **not** establish that `GetFilters` returns a different enum ID for the same name in SDM.

So the corpus does not encode the opposite of the evidence; it encodes an *unproven invariance*, which
is a weaker and different charge. Renumbering 40 → 38 would have replaced one unverified number with
another while newly asserting a live-lane fact nobody measured — the exact failure the original Stage
1 dissent predicted for a hand-transcribed corpus (contrary evidence 4), arriving through the one
channel nobody was watching: a **report about** a repository rather than the repository. The
amendment's first bite is withdrawn and replaced by B4, which is what the evidence actually supports.

### Correction 2 — two "biting" items do not bite

The `/review` pass listed `SetFilter value` alone and bare-ampersand tolerance as expectations that
would fail against today's client. Inspection says otherwise:

- **`SetFilter` with an absent sibling is unreachable from production.** Both callers pass
  `Some(...)` — `src/adapters/hqplayer.rs:1662` and `:1679`. The reachable hazard is the *guess* at
  `:1661`/`:1678`, which is C5, i.e. #347's.
- **Bare `&` is already handled.** `decode_entities`' fall-through pushes `&` and advances rather than
  swallowing the following text. It is untested, not broken.

## Solution Space

**Problem:** #322's seventeen new bullets must be reframed into a #322-scoped plan that says which
additions are already met, which are #347's, and what is actually built — without absorbing #347.

**Key constraint:** ADR 003's three-layer split (corpus / wire / model) is committed and inherited by
#162/#208/#328/#329/#330, so every addition must land in an existing layer.

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-Aid | Paste the seventeen bullets into the AC list; add a fixture per bullet | 11 bullets are partly or wholly met; four are #347's and would be built in the wrong issue |
| B | Local Optimum | Add each fault/fixture to the existing layers; keep `Inner::sdm()` and special-case `[source]` inside each setter arm | Encodes the conflation the evidence exists to prevent; N special cases that can disagree |
| C | **Reframe** | One new state axis — loaded chain, distinct from configured mode — then compose the chain/pin/mode items from that axis × existing faults; short independent tail | Touches every enumeration path; forces a position on an open evidence question |
| D | Reframe | Port HQPTuner's `fake_control.py` state model as a second responder | Two state models that can drift; its framing is rejected by #322's own criteria; a literal port is blocked on #348's missing `THIRD-PARTY-NOTICES` |
| E | Redesign | Split: #322 closes on document/framing conformance, a new issue owns the live state model | Foreclosed by the issue body — the maintainer has already placed both sections inside #322 |

**Selected: Option C.** The seventeen bullets are not seventeen requirements. The #322 remainder is
dominated by one structural absence — the model has no loaded chain, so `mode_index` answers two
questions the evidence says are independent. Adding the axis makes the chain, pin and mode items
compositions of machinery that already exists.

**The reframe, stated once:** *#322 owns making each daemon misbehaviour expressible and observed;
#347 owns the quality of the client's response.* The line is mechanical, not a judgement call — an
expectation belongs to #322 if the fake plus the **unmodified** client can satisfy it, and to #347
otherwise. That is why no #347 row above is smuggled in.

**Why not the others:** A restates instead of classifying. B encodes the conflation. D's wire layer
fails #322's own criteria and its code path needs #348 first. E asks the right question — #337 is
already 4,902 insertions and the live state model is genuinely different work — but the issue body
records the opposite decision, so raising it is legitimate and acting on it is not. **It is raised, as
a maintainer decision, in the gate section below.**

## Review — verdict: ADJUST

Five findings kept this off *Continue*. All are corrections inside Option C; none reopens the space.

1. **Only three of the sixteen #322 gaps can fail against today's client** — B6 (no cap at all), B8
   (root recovery; fails every `Status` poll while a track is loaded), and the stale-index
   misselection behind B1–B3. The other thirteen are *capability* or *fidelity* work that cannot
   produce a client red. **Adjustment: label every new expectation `client-conformance` or
   `model-fidelity`.** This matters because PR #337 has already twice had to disclose REDs that added
   a deliberately-failing production stub so an expectation could fail behaviourally (`bd87e21`,
   `d7f62c2`). Without the label, Stage 2 manufactures more.
2. **The headline chain fixture as specified would prove the wrong thing.** `poly-sinc-gauss-long` is
   PCM index 7; `filters_sdm.xml` has 5 entries. Sending the stale index 7 hits the range check at
   `model.rs:718` and returns `result="Error"`, so the test would observe a **spurious error** while
   the real hazard is a **silent wrong selection**. The SDM fixture must be long enough that index 7
   exists and names a different filter.
3. **The corpus fidelity item deserved first position** — subsequently corrected by dissent; see
   correction 1.
4. **One production change is implied and contested.** The response cap is claimed by both #322
   (bullet 46) and #347 ("Accumulated native responses have an explicit size limit"). Raised as a
   maintainer decision rather than assumed either way.
5. **Two ripple effects were unnamed:** B14 (unevidenced `SetMode` filter reset) and B15
   (`set_mode("99")` depending on the fallback #347 deletes).

**Drift detected.** *Scope drift, latent:* the loaded-chain axis sits one step from #347's chain
observer. The guard is mechanical — an expectation needing new client behaviour is #347's. *Goal
drift, present in the issue body itself:* bullets 45, 47, 62, 65 and 66 are already met, so leaving
them unticked misreports #322's state in the opposite direction from the over-claiming the
reconciliation comment warned about.

## Dissent — verdict: ADJUST

**Confidence before:** MEDIUM-HIGH. **After:** MEDIUM.

Six lines of contrary evidence. The two that changed the plan:

1. **The chain-divergence evidence is about the persistent lane** — correction 1 above. Fatal to the
   plan's opening move, and it inverted the first bite.
2. **The tier-1 merge gate can never settle chain-scoped claims.** ADR 003 and PR #337 both state
   that tier 1 captures whichever mode the daemon is *already* in, because reaching the other needs
   `SetMode`, which is mutating. Chain-scoped enumerations are two modes' worth of evidence by
   definition. So chain fixtures join a `derived` population the merge gate structurally cannot
   promote — labelling them "pending #332" overstates what will ever arrive.

The others: six of ten new expectations can only observe the fake, and a label makes that legible
rather than smaller; the axis cannot be added without taking a position on the `State`/`Status`
contradiction #341 owns (`model.rs:411` vs `:431`); and the truthful-outcome floor has a hole exactly
where `[source]` is most used (C9).

**Pre-mortems.** (i) *The fake resolves a contradiction the project agreed not to resolve* — an
`active_mode` value chosen for fixture convenience becomes de-facto truth across nine passing
expectations with no provenance behind it. (ii) *#347 routes around the fixtures built for it*,
cannot tell verified daemon facts from 2026-07 modelling choices, and writes its own fake — the
program-level regression the original dissent named. (iii) *The cheap wins wait behind the expensive
unverifiable ones* — B8 is a live user-harm path of the same class as the 0 dB volume defect this PR
already fixed.

**Modifications folded into the plan:**

1. Withdraw the `filters_sdm.xml` renumbering; record the live-lane question in #341 with both
   citations attached. No fixture changes numbers on this evidence.
2. Replace it with B4 — add `oversampling` to `persistent_config.xml` as a second, separately
   numbered persistent attribute, and extend the cross-lane expectation to cover it.
3. Ship the three biting items **first and independently**; none needs the axis.
4. Split capability from expectation: #322 builds the axis and the `[source]` faults so #347 is
   unblocked; expectations that can only observe the fake travel with #347's client change.
5. Make `active_mode` fixture-driven rather than model-derived, so the fake cannot resolve the
   `State`/`Status` contradiction by construction. If that costs more than the axis is worth, that is
   itself the finding.
6. Label chain claims **tier-2-only**, not merely "pending #332".
7. Hand C9 (Auto-in-`[source]` false success) to #347 in writing.
8. Keep B14 and B15 as named bites.

**Create ADR?** **No new ADR.** ADR 003 already owns this boundary and the capability-versus-
expectation policy is a Consequence of it, not a new one-way door. Amend ADR 003 in Stage 2 — after
the axis prototype settles whether `active_mode` can stay fixture-driven — so the amendment does not
have to hedge its most consequential paragraph.

## Proposed Stage 2 bite plan

Six bites, ordered so the highest-value and most-verifiable land first, and so the plan can stop
after any bite with value delivered. **Every bite is RED-first in its own commit, and every new
expectation carries a `client-conformance` or `model-fidelity` label in its doc comment.** Line counts
are estimates for a re-estimate checkpoint after bite 2, per the original Stage 1 discipline.

| # | Bite | Class | Delivers | Est. |
|---|---|---|---|---|
| **1** | **Root recovery for an unparseable `metadata` child** (B8) | client-conformance | A `Status` whose child carries unescaped `<`/`"` but whose root is complete yields the playback fields instead of failing. Today this errors on **every** poll while a track is loaded. Needs a `wire.rs` malformed-child document and a `framing` recovery path | 120–160 |
| **2** | **Response accumulation cap** (B6) | client-conformance | `wire.rs` gains an oversized-document fault; the framer gains an explicit byte bound; an oversized reply fails without wedging the next poll. **Blocked on the ownership decision below** | 80–110 |
| **3** | **Loaded-chain axis** (B1, B2) | capability | `LoadedChain{Pcm,Sdm}` on `DaemonState`, `Inner::chain()` replacing `Inner::sdm()` as the enumeration resolver, `external_change` able to move the chain with `mode_index` untouched. `active_mode` becomes fixture-driven in both renderers. One `model-fidelity` expectation that a `[source]` chain change swaps the served lists | 180–240 |
| **4** | **Stale-index misselection** (B3, on top of bite 3) | client-conformance | `filters_sdm.xml` extended so the stale PCM index is *in range* and names a different filter, then a source-driven chain change followed by a filter write by name. This is the one chain expectation that bites the unmodified client. Fixtures marked `derived-upstream`, **tier-2-only** | 90–130 |
| **5** | **Persistent-lane second numbering domain** (B4) | client-conformance | `oversampling` added to `persistent_config.xml` with its own enum domain; the cross-lane expectation extended so a PCM `filter` number and an SDM `oversampling` number for the same name are not interchangeable. Evidenced, cheap, needs no axis | 60–90 |
| **6** | **Fidelity tail** (B5, B7, B9–B16) | mostly model-fidelity | Device-mode-omission fixture; bare-`&` pinned; `SetRate` expectations incl. `[source]` accept-and-ignore and mode-clears-pin; `Status.active_mode` echo fixture; `Provenance.playback` as a required field; remove the unevidenced `SetMode` filter/shaper reset (B14); retarget `set_mode("99")` off the numeric fallback (B15); source-rate ≠ output-rate fixture | 260–340 |

Total ≈ **790–1,070** lines, overwhelmingly fixtures and expectations. **Bites 3, 4 and 6 deliver the
capability #347 is blocked on; the expectations that can only bite after #347's client change travel
with #347, not here.**

Carried forward unchanged: every assertion through `HqpAdapter`'s public surface; one behaviour per
test; no wall-clock assertions; every fixture carries provenance; `MockHqpServer` stays a facade for
`tests/adapter_integration.rs` and `tests/zones_sha_integration.rs`; no route, request or response
change; `tests/fixtures/api_routes.txt` untouched; `api-change-approved` never self-applied.

## Maintainer decisions this amendment will not make

1. **Who owns the response accumulation cap** — #322 bullet 46 or #347's AC? It lives in the framing
   path #322 already changed. Recommendation: #322 owns it, #347's duplicate AC is struck as
   inherited. Bite 2 is blocked until this is answered.
2. **Should #322 be split** (Option E)? The issue body places both salvage sections inside #322, and
   #337 is already 4,902 insertions. Splitting the live state model into its own issue is defensible;
   reversing a recorded maintainer decision is not an agent's call.
3. **Should bullets 45, 47, 62, 65 and 66 be ticked** in the issue, given the evidence above?
4. **How should tier-2-only claims be marked** in fixture provenance, given that no read-only gate
   can ever promote them?

## Superego disposition (`sg review`, superego 0.9.4)

Run against the staged artifact at the Stage 1 decision point. **No blocking concerns.** It
independently spot-checked the citations against the worktree — `Inner::apply`, `Inner::sdm()`, the
`SetMode`/`SetFilter` handlers, both negative-evidence greps, correction 1's enum-ID claim, and B4's
missing `oversampling` attribute — and found them accurate rather than confidently wrong. It confirmed
the diff is one file, 344 insertions / 3 deletions, with no `src` or `tests` touched.

One follow-up raised, and closed:

| # | Finding | Disposition |
|---|---|---|
| 1 | Re-confirm the `rna-mcp` "no metis/guardrail/signal, no outcome artifact for `80222d6d`" claim, since it underwrites "the only prior situated judgment is `.oh/hqplayer-spec.md` and ADR 003" | **Closed.** Re-run in the same turn: the artifact-typed search returns only code symbols and this file; `outcome_progress("80222d6d")` returns *not found in `.oh/outcomes/`*. `.oh/` contains only `.cache/`, `hqplayer-spec.md` and this artifact — there are no `metis/`, `guardrails/` or `outcomes/` directories to search |

## Stage-1 amendment gate status

| Gate requirement (#310 per-issue workflow) | Status |
|---|---|
| `/solution-space` run, ≥3 genuine candidates across ≥2 escalation levels | Met — 5 candidates across 4 levels |
| `/review` verdict Continue or Adjust, no unresolved blocker | Met — **ADJUST**; five findings folded in |
| `/dissent` verdict PROCEED or ADJUST, not RECONSIDER | Met — **ADJUST**; 8 modifications folded in |
| `sg review` at the Stage 1 decision point | Run against this artifact; disposition recorded in the superego comment on PR #337 |
| One-way architectural decision has an ADR or a stated reason not to create one | Met — ADR 003 amended in Stage 2, reason stated |
| Outputs persisted to `.oh/issue-<number>-<slug>.md` | Met — this section |
| Planning artifact committed, four reports posted as separate PR comments | See PR #337 |
| Stop for the independent Codex Stage 1 gate before `/execute` | **Stopping here** |

**Not done in this turn, by instruction:** no `src` change, no `tests` change, no Stage 2 work, no
merge, no ready-for-review, no auto-merge, no API route or payload change, no
`api-change-approved`, no force push. **No live verification is claimed anywhere in this amendment**
— the suite was not run in this turn, and no HQPlayer daemon was contacted. Every classification is
sourced to a file and line read at `6b8e97f`.

---

## Amendment Stage 2 — execution record

**Updated:** 2026-07-29 · **Planning HEAD:** `34dcfbb` · **Implementation HEAD:** `63e6bc0`
**Codex Stage 1 amendment gate: PASS** (PR #337 comment 5125951929)

Six bites, all landed. `cargo test` was run throughout; **no HQPlayer daemon was contacted and no
tier-1 or tier-2 live run was performed** — the rig is offline and no mutating permission exists.

### Binding resolutions carried in

From the gate and the three issue comments (#322 5125948519, #341 5125948432, #347 5125915480):

| Resolution | Where it landed |
|---|---|
| #322 owns the accumulated-response byte cap and oversized recovery | Bite 2 — `MAX_RESPONSE_BYTES` |
| Keep the loaded-chain model in #322 | Bite 3 — `LoadedChain` on `DaemonState` |
| Every new expectation labelled | All 20 carry `client-conformance`, `model-fidelity` or `regression-pin` in the doc comment |
| Model unit tests allowed for fake capability; adapter-facing semantics stay in #347 | Bites 3–6 assert daemon-side facts; no #347 behaviour implemented |
| Stale index must be an **in-range silent misselection** | Bite 4 — `filters_sdm.xml` extended 5 → 9 so PCM index 7 exists in SDM and names `poly-sinc-lp` |
| Do **not** renumber live `GetFilters` enum IDs | No enum ID changed; live-lane question left on #341 |
| Add the separately-numbered persistent `oversampling` domain | Bite 5 — `persistent_config.xml` gains `oversampling="38"` |
| `active_mode` fixture/profile-driven | Bite 3 — two independent `ActiveModeReporting` policies |
| Chain claims `derived-upstream` + `tier-2-only` | Fixture provenance and doc comments on every chain behaviour |
| Retarget the numeric-fallback-dependent test | Bite 6 — now uses low-level `set_filter` |
| Remove the unevidenced `SetMode` filter/shaper reset | Bite 3 |
| Preserve UHC's wire/framing design; port no HQPTuner code | Held — `wire.rs` unchanged in shape; nothing ported |

### Commit chain — RED before GREEN for every client-conformance bite

| SHA | Kind | What |
|---|---|---|
| `6d96004` | **RED** (test-only) | A closed root wedges every poll when a child tag never terminates |
| `38b706e` | GREEN | `framing::root_frame_closed` — narrow root recovery |
| `c96d050` | **RED** (test-only) | An unbounded reply is bounded only by the clock |
| `dd2cb46` | GREEN | `MAX_RESPONSE_BYTES` byte ceiling |
| `97dcc59` | capability | Loaded-chain axis; policy-driven `active_mode`; unevidenced reset removed |
| `333bae4` | fixture + capability | Stale-chain hazard made in-range and expressible |
| `8dd2c0b` | fixture + expectation | Persistent lane's second numbering domain |
| `e0315eb` | corrections + tail | `[source]` rate refusal, device modes, `playback` provenance, retarget |
| `829256f` | style | `cargo fmt` |
| `d4fec9b` | fix | Deferred-`SetMode` clamp hole; coalescing stated-limit test; cap peak documented |
| `e92f46d` | test | Assert the source-pin failure *mechanism*, not just that it failed |
| `63e6bc0` | refactor | Share one tokeniser; process narration out of permanent comments |

`git diff HEAD^..HEAD -- src` is **empty** for `6d96004`, `c96d050`, `97dcc59`, `333bae4`, `8dd2c0b`
and `e0315eb`. **No production stub was ever added to manufacture a red** — the two REDs are
test-only and failed for behavioural reasons, and every model-fidelity expectation was written
alongside its capability and is labelled as not being a client red.

### Bite 1 — root recovery. RED observed, then GREEN

The plan called this the highest-value client-conformance item. **Its premise was wrong, and the
correction is the finding.** A hostile *attribute value* — unescaped `<`, `"`, `>`, bare `&` inside
`<metadata …/>` — is already tolerated: UHC never parses the children and scopes attribute reads to
the root's opening tag, so it is immune where HQPTuner needed `_recover_root` (which XML-parses whole
documents). That is pinned as a **regression-pin**, not claimed as a fix.

What *does* reach the client is a **structurally** malformed child. Probing found the real boundary:

| Hostile shape | `classify` before |
|---|---|
| unescaped `<`, `"`, `>`, bare `&` in a child attribute | `Complete` — already fine |
| **child tag never terminates** | **`Incomplete`** → waits out the deadline |
| **stray `<` among the children** | **`Incomplete`** → waits out the deadline |
| nested unclosed child | `Malformed` — a hard error, does not wedge |

Observed RED at `6d96004`:

```text
test the_framer_recovers_a_closed_root_whose_child_tag_never_terminates ... FAILED
test a_status_whose_child_tag_never_terminates_still_reports_the_root_fields ... FAILED
  assertion `left == right` failed: a closed root frame is a complete document even
    when a child tag never terminates
  a Status whose root frame is closed must be readable …: Response timeout
test result: FAILED. 139 passed; 2 failed; 0 ignored
```

`Response timeout` is the signature: the reply was complete on the wire and the client waited out its
deadline anyway. The mechanism turned out to be subtler than predicted — an unterminated child makes
quick_xml swallow the following `</Status>` into the child's own attribute soup, so the parse reaches
`Eof` with children still open and the closing tag sits in the buffer never having been seen as an end
event. Reaching that state with the buffer ending in `</root>` *is* the diagnosis.

`framing::root_frame_closed` recovers it, narrowly: keyed on the root's **own** name (so
`<State …></Status>` stays a mismatched-nesting rejection), requiring the closing tag to be the
buffer's **last** token (so a truncated document stays incomplete and a `</Status>` inside an
attribute value cannot pass for a frame end), and consulted only when the parse could not complete on
its own (so every well-formed document takes the unchanged path).

### Bite 2 — byte cap. RED observed, then GREEN

Observed RED at `c96d050`, with a deliberately generous 4 s budget so a timeout would be the *wrong*
answer:

```text
test an_unbounded_reply_is_rejected_by_an_explicit_byte_cap_and_the_next_command_still_works ... FAILED
  the failure must name the byte ceiling it hit … a bare timeout here would mean the buffer
  is still unbounded and only the clock stopped it. Got: Response timeout
test result: FAILED. 0 passed; 1 failed; finished in 4.03s
```

`MAX_RESPONSE_BYTES = 4 MiB` is the third of three bounds and the only one that bounds **memory**;
the deadline bounds time and `MAX_UNSOLICITED_BACKLOG` bounds frame count. Recovery needed no new
code — `send_command`'s wrapper already marks the connection disconnected on any inner failure — but
the expectation asserts it rather than assuming it, since a cap that wedges the connection is not a
fix.

**One cost shape recorded rather than fixed:** `framing::classify` re-parses the *whole* accumulated
buffer after every line, so accumulation is O(bytes × lines). That is why the fake's oversized frames
are 256 KiB rather than 1 KiB — with small frames the client cannot reach a multi-megabyte buffer
inside any sane deadline and the test would measure parser throughput instead of the ceiling.
Worst-case CPU is now bounded by the cap. #347 inherits the primitive and should know this.

### Re-estimate after bite 2, as the amendment required

| Bite | Estimated | Actual | Ratio |
|---|---|---|---|
| 1 | 120–160 | **221** | 1.4–1.8× |
| 2 | 80–110 | **164** | 1.5–2.1× |
| Cumulative | 200–270 | **385** | ~1.45× |

Extrapolated total at that rate: **≈1,240–1,545** against the plan's 790–1,070. The overrun driver was
consistent and is not scope creep: doc-comment density, and the label/provenance discipline the
amendment itself mandated. Judgement recorded at the checkpoint: continue, because bites 3–5 are
load-bearing and bite 6 is the trimmable tail. **Final actual: 1,257 insertions across 23 files** —
just under the extrapolation's low end, and the tail did not need trimming. The extra 61 lines over the
first count are the review, dissent and superego adjustments below.

### Bites 3–6

**Bite 3 — the loaded-chain axis.** `Inner::sdm()` was `mode_index == 2`; it now reads
`DaemonState::loaded_chain`. `source_loads_chain()` moves the chain with the configured mode
untouched. `State.active_mode` and `Status.active_mode` are driven by **two separate**
`ActiveModeReporting` policies — one shared field would re-impose the agreement this removes.
Defaults reproduce today's behaviour exactly, and each variant records whether it is verified
(`Status` echoing `[source]`: hqplayerd 6.0.4, 2026-07-29, playback active) or unverified (either
field resolving). The unevidenced `SetMode` reset of filter/shaper to index 0 is **gone**; upstream
says `SetMode` clears the rate pin and reloads the chain and says nothing about selections returning
to the first entry. `clamp_to_loaded_chain()` replaces it, stated as a self-consistency invariant — a
daemon never reports an index outside its own list — and explicitly *not* a claim that selections
survive a chain change.

**Bite 4 — the stale-chain hazard.** `filters_sdm.xml` extended 5 → 9 entries reusing names and enum
IDs verbatim from `filters_pcm.xml`, so PCM index 7 exists in SDM and names `poly-sinc-lp`. Observed
through the real client while writing it:

```text
requested name 'poly-sinc-gauss-long'; cached PCM index 7; sent value=Some("7");
client outcome ok=true; daemon now reports active_filter="poly-sinc-lp"
```

The client reports **success** for a filter it did not select. The realistic trigger is ordinary:
`get_pipeline_status()` populates the list cache via `refresh_lists()`, which is the path both
`GET /hqplayer/pipeline` and the MCP status tool already take; then the source moves the chain, the
configured mode never changes, and nothing prompts a re-read. The fix is a loaded-chain observer —
**#347's** first acceptance criterion — so the committed test asserts only the daemon-side outcome and
deliberately does not assert what the client returned. Asserting success would encode a defect as
expected; asserting failure would fail until #347 lands.

Worth recording: a first probe of this scenario sent the **correct** index, because `get_filters()`
does not populate the cache at all — only `refresh_lists()` does. Without first taking a
cache-populating path the hazard does not reproduce. A fixture built from prose would have got that
wrong.

**Bite 5 — the persistent lane's second numbering domain.** `persistent_config.xml` gains
`oversampling="38"`, the SDM chain's separate stored-ID domain for the same semantic filter that
`filter="40"` names in PCM. Its provenance states that the upstream evidence names the persistent
*field names* and is not a `GetFilters` capture, that the live-lane question is open on #341, and that
this corpus therefore still reports `value="40"` in both live chain lists rather than answering it.

**Bite 6 — the tail.** Mode-conditional `Faults::source_refuses_rate_pin` (distinct from the
unconditional `accept_but_ignore`, which cannot express "deaf *in this mode*"), covering both the
nonzero pin and the Auto case #347 comment 5125915480 asked for. The nonzero case errors today
because readback compares the requested index against 0 — the test says plainly that this is the
floor holding by arithmetic rather than by understanding. The Auto case compares 0 to 0 and reports
success, so it asserts daemon-side facts only. New device profile
`hqpd-6.0.4-pcm-only-dac` omits SDM with surviving indices 0 and 1 intact. `Provenance` gains a
**required** `playback` field, backfilled across all 18 fixtures; most say `unknown` because they are
derived from a protocol reference rather than captured from a session, and **that most say `unknown`
is the finding**. Bare `&` pinned as a regression-pin.

### Stage-2 amendment `/review` — verdict ADJUST, adjustments applied

Three holes, all found by reviewing the six bites rather than by the suite, all closed in `d4fec9b`:

1. **A deferred `SetMode` escaped the chain invariant.** `apply()` defers a delayed change onto
   `faults.pending`, so the setter arm's clamp ran against *unchanged* state and the real mode change
   landed later in `tick_pending()` with no clamp at all — leaving an index the newly loaded chain's
   enumeration could not resolve. Unexercised today, which is why it was worth finding rather than
   waiting for.
2. ~~**Root recovery does not survive coalescing.**~~ **SUPERSEDED — see the Codex-adjustment
   section.** This shipped as a passing `stated-limit` test asserting `Incomplete`, which encoded a
   known defect as expected behaviour. The Codex Stage 2 gate rejected it and recovery now finds the
   first defensible boundary instead.
3. ~~**The cap's true peak is 4 MiB plus one line.**~~ **SUPERSEDED — the reasoning was wrong.**
   One line is unbounded, so naming the hole did not close it. The cap is now a bound on allocation;
   see the Codex-adjustment section.

### Stage-2 amendment `/dissent` — verdict PROCEED, with the value framing corrected

**The honest accounting, which is the dissent's main product:** of the new expectations, **two** went
red against unmodified production code, both in framing (bites 1–2, 385 lines). Bites 3–6 are **811
lines and zero client defects**. The review predicted three biters; bite 4's stale index turned out to
be #347's and became model-fidelity. So the amendment's product is overwhelmingly a **capability whose
consumer does not exist yet**, and its justification now rests on #347 actually using the axis rather
than writing its own fixtures. That is the original Stage-1 pre-mortem 2, and it should be checked once
rather than assumed twice.

Applied: assert the source-pin failure *mechanism* rather than bare `is_err()` (`e92f46d`) — the old
assertion was satisfied by any error at all, so it passed while saying nothing about why.

Recorded, not resolved:

| # | Finding | Disposition |
|---|---|---|
| 1 | **Four labels where the gate specified two.** `regression-pin` and `stated-limit` were added mid-flight. The refinement is defensible — a test that passed before the amendment neither proves fake capability nor can fail for a new reason — but it is a deviation from a binding instruction | **Flagged for the gate to accept or reject.** Not treated as settled |
| 2 | **`clamp_to_loaded_chain` is unevidenced behaviour replacing unevidenced behaviour.** A real daemon might keep the filter by name and remap the index, reset to a device default, or refuse the mode change. Clamping is a fourth behaviour nobody observed; "invariant, not a claim" is a modelling argument | Belongs on **#341** as a one-capture tier-2 question: set a filter, switch mode, read `State.filter` |
| 3 | **The fake now has more switches than evidence, and no coherence check.** Nothing stops a test configuring a daemon that has never existed — `ResolvesLoadedChain` on both fields, or `source_refuses_rate_pin` with a configured PCM mode. Defaults reproduce the status quo and every variant names its provenance, but the space is combinatorial and unguarded | Recorded. A conformance verdict about an impossible daemon is worse than none |
| 4 | The shipped recovery reduces but does not close the poll wedge (finding 2 above) | #347 inherits the framing primitive and should decide whether to widen |

### Stage-2 amendment `sg review` (superego 0.9.4) disposition

Run as `sg review pr` against `v3`, so it assessed the **whole branch**, not only the amendment. Seven
findings; the two that concern this amendment's own additions were **fixed** rather than
dispositioned:

| # | Finding | Disposition |
|---|---|---|
| 3 | Agent-review narration inside permanent comments | **Fixed** in `63e6bc0`. Four doc-comment blocks named the pass that produced them; rewritten as plain rationale. Fixture provenance, capture dates and issue references were deliberately kept — provenance is a #322 acceptance criterion, and the issue references are the ownership boundary that stops #347's semantics reading as #322's |
| 5 | Three parsing implementations that must stay in sync | **Partly fixed** in `63e6bc0`: `root_frame_closed` had a hand-rolled root-name scan and now uses `framing::root_element`, so no fourth tokeniser was added. The pre-existing production/`model.rs`/`raw.rs` triple stands and is a real standing risk |
| 4 | Should the amendment have been its own issue? | **Already decided.** Issue #322 comment 5125948519: *"No split: keep the loaded-chain model in #322."* Raised by the Stage-1 amendment as maintainer decision 2 and answered before Stage 2 began |
| 7 | Why not lean on HQPTuner more directly? | **Already done for the state model** — the loaded chain, command-keyed deafness and mode-conditional refusal are its design, adapted. A literal code port stays blocked on #348: no `THIRD-PARTY-NOTICES` exists, and its fake's framing (4096-byte read split at `?>`) satisfies none of #322's criteria |
| 1 | ~8,000 lines of harness against a small production diff | Acknowledged, and it is the whole branch rather than this amendment, whose share is 1,257 lines. Already a standing maintainer question in the PR body ("Is this size acceptable?") |
| 2 | The `.oh` file has become a process novel | Acknowledged, and it restates an open documentation-policy question the PR body already flags — superego previously suggested the ADR be authoritative and this file a frozen transcript. This stage's instruction required the artifact carry implementation, RED/GREEN, provenance, scope and verification evidence, so it could not be thinned here. **Maintainer decision** |
| 6 | "verified" density may over-claim | Partly addressed: the amendment made `playback` a required field (mostly `unknown`) and added `tier` markers, which weakens rather than strengthens the verified language. A single corpus-directory legend is a good suggestion and is recorded for the maintainer |

### Verification at `63e6bc0`

| Command | Result |
|---|---|
| `cargo test --test hqplayer_conformance` | **156 passed; 0 failed; 0 ignored** (was 138 at `6b8e97f`) |
| lib unit tests | 84 passed |
| `--test adapter_integration` / `--test zones_sha_integration` | 42 / 20 — both `MockHqpServer` facade consumers intact |
| `--test protocol_schema` / `--test api_contract` | 41 / 2 — no route or payload drift |
| `cargo fmt --check` | clean |
| `cargo clippy -- -D warnings` | clean, 0 findings (CI's invocation) |
| `git diff --check origin/v3...HEAD` | clean |
| `cargo test --no-fail-fast` | **465 passed; 2 failed; 12 ignored** |
| live tier 1 / tier 2 | **not run** — daemon offline, no mutating permission |

**The two failures are environmental and pre-existing.**
`client_harness::shared_endpoints::get_hqp_discover` and
`protocol_integration::api_endpoints::hqp_discover_returns_json` both fail on `/hqp/discover`
returning 500. Attributed rather than assumed: a direct probe in this sandbox gives
`multicast send FAILED: [Errno 65] No route to host` for `239.192.0.199`, and
`git diff --name-only 34dcfbb..HEAD -- tests/client_harness.rs tests/protocol_integration.rs` returns
**zero files** — neither test was touched by this amendment. Both were previously verified to
reproduce identically on an untouched `v3` worktree (`0a1b02c`). The
`error_handling::lms_fails_gracefully_when_unconfigured` flake did not fire in this run.

**#339 remains unmerged**, so PR CI may still show the known Rust 1.97 base lint failure on `v3`.
That is a base dependency, not a defect in this work, and it is not hidden.

### Scope held

One `src` file changed — `src/adapters/hqplayer.rs`, +92 lines, entirely inside the `framing` path and
`send_command_inner`. `git diff --name-only 34dcfbb..HEAD -- src/api src/app src/main.rs src/mcp
tests/fixtures/api_routes.txt` returns **zero files**. No route, request schema, response schema or
payload changed; `api-change-approved` was not applied.

**No #347 change was implemented.** Not source-rate suppression, not no-op `SetMode` skipping, not
loaded-chain cache refresh, not sibling-safe filter writes, not numeric-fallback removal, not semantic
alias matching, not volume readback. Every one of those remains a #347 row, and the amendment's
classification table above still describes them accurately.

### ADR 003

**Not amended.** The capability boundary that landed is the one ADR 003 already describes: three
layers, corpus/wire/model, asserted through the adapter's public surface. The loaded-chain axis is a
field on the model layer and the byte cap is a bound in the framing path — neither changes the
decision the ADR records, and the capability-versus-expectation labelling is a suite convention rather
than an architectural one. Amending it would add words without changing a decision. Recorded here so
the Stage 2 gate can see the judgement rather than infer an omission.

---

## Codex Stage 2 gate adjustment (2026-07-30)

**Gate verdict on `ff70554`: ADJUST** (PR #337 comment 5126227665), eight blocking findings. All
eight are addressed below. **The prior attempt above is left intact**; claims it made that turned out
to be wrong are struck through in place and corrected here rather than deleted, because the reasoning
that produced them is the part worth keeping visible.

**Implementation HEAD:** `5f37974` at time of writing this section.

### The gate was right on all eight, and two were my own factual errors

| # | Finding | Resolution |
|---|---|---|
| 1 | The 4 MiB cap is **not a memory bound** — `read_line` allocates an unbounded newline-free line before the check | **Fixed, RED first.** `send_command_inner` now accumulates a `Vec<u8>` from fixed 8 KiB stack reads and checks `held + n` *before* appending. Newline stays a framing *hint*; UTF-8 split handling via `valid_up_to()`; deadline and unsolicited/coalesced behaviour preserved. My prior comment made this worse by naming the hole as though naming closed it |
| 2 | A realistic malformed-plus-push sequence still wedges, and the suite **encoded that defect as a passing test** | **Fixed, RED first.** Recovery finds the **first defensible** root-frame boundary, quote-aware, so a `</Status>` literal inside an attribute value is still not a boundary and truncated/mismatched roots are still rejected. The `stated-limit` test is replaced by a client-conformance success case |
| 3 | The stale-chain test drives today's broken adapter cache and will regress when #347 fixes it | **Removed.** Replaced by `the_same_filter_index_selects_a_different_filter_per_loaded_chain`, which drives the fake through `Responder` with **no adapter**. The observed probe is recorded on #347 |
| 4 | The Opal SDM fixture is **padded with invented cross-chain rows** | **Reverted** to its 5 Opal-derived rows. The hazard moved to `synthetic-chain-hazard`: fictional `SYN-*` names, `status: synthetic`, `tier: never-promotable` |
| 5 | Two statements are **factually wrong** | **Both corrected** — see below |
| 6 | The delayed-`SetMode` repair has no focused regression | **Added**, and verified non-vacuous by disabling the clamp: *"SDM has 5 entries and the fake reports 1x=11 Nx=11"* |
| 7 | The agreed fidelity tail is incomplete | Source-vs-output rate **added**; mode-varying rate resolution **added**; empty rate list **reclassified to #341** with the exact gap |
| 8 | Four labels drifted from the binding two | **Normalised.** 20 contract labels, 0 non-contract. A pinned pre-existing property is `client-conformance` and described as a regression pin in prose |

### My two factual errors, stated plainly

**`tests/fake_control.py` does not exist at stable `67557939`.** I cited it there; it is a dev-era
file, present at `22dfe5cc`. The fixture now cites the salvage report at the dev ref and says
explicitly that HQPTuner was not read directly — every upstream claim in this amendment came from a
report *about* the repo.

**HQPTuner does not rely on `Status.active_mode` as its chain resolver.** It *rejects* that field and
falls back to the `Status.active_rate` family in `livemap._chain_from_status`. I took "HQPTuner
*relies* on `Status.active_mode`… Both cannot be fully right" from the stable-branch salvage report
(§C3) — and the beta/dev delta had already superseded it: `0eeb1ae` built a fallback on `active_mode`,
`c646bc1` replaced it with `active_rate`, and the delta records "the intermediate state was wrong."

So the correct framing is **independent unresolved semantics, not a contradiction**: `Status.active_mode`
echoing under `[source]` is *measured*; `State.active_mode` under `[source]` is simply *unmeasured*.
"Both cannot be right" was wrong, and the `ActiveModeReporting` prose and test name now say so.

This is the second time in this amendment that a stale reading from a *report about* a repository beat
the checks — the first was the enum-ID renumbering the Stage 1 dissent caught. Same channel, twice.

### Two improvements the rewrite forced

- **`classify` and the new `first_document_end` are projections of one walk** (`scan`), so there is a
  single traversal to keep correct. This *removes* a parser rather than adding one, which is the
  direction superego finding 5 asked for.
- **A coalesced follower is now counted and dropped rather than left in the stream.** Leaving it was
  worse in two ways: a stale pushed `Status` is the right *element* for a later `Status` command and
  could be handed over as that command's reply, and the returned reply could carry a second document
  concatenated onto it — unnoticed only because attribute reads scope to the root tag. Caught because
  `tier1_records_how_many_unsolicited_documents_the_client_skipped` dropped to 0 after the read change.

### Blocker 7 disposition, in full

| Case | Disposition |
|---|---|
| Source `metadata.samplerate` ≠ `Status.active_rate` | **Covered.** `the_source_rate_and_the_output_rate_are_reported_separately`, with `active_bits` as an output-domain field. Consuming semantics are **#328**'s |
| Mode-varying rate resolution | **Covered.** `a_rate_valid_in_one_chain_is_refused_in_the_other`; the two enumerations do not overlap at all |
| **Empty** rate enumeration | **Reclassified to #341.** Issue #322's AC says the list "can vary by mode, filter, device, and playback state or be empty", but **neither audited salvage report contains any observation of an empty `GetRates`** — searching both returns only unrelated backup-archive and now-playing matches. Modelling one would invent a device claim |
| Filter-varying rate list | **Reclassified to #341.** Evidenced (`livelane.py:33-38`: the list depends on mode **and** selected filter) but unmodelled; the corpus has no filter→rates dependency and adding one needs a capture |

### Verification at `5f37974`

| Command | Result |
|---|---|
| `--test hqplayer_conformance` | **162 passed; 0 failed; 0 ignored** (157 at `ff70554`, 138 at `6b8e97f`) |
| lib unit tests | 84 passed |
| `--test adapter_integration` / `--test zones_sha_integration` | 42 / 20 |
| `--test api_contract` / `--test protocol_schema` | 2 / 41 — no route or payload drift |
| `cargo fmt --check` | clean |
| `cargo clippy -- -D warnings` | clean, 0 findings |
| `git diff --check origin/v3...HEAD` | clean |
| `cargo test --no-fail-fast` | **471 passed; 2 failed; 12 ignored** |
| live tier 1 / tier 2 | **not run** — daemon offline, no mutating permission |

The two failures remain the reproduced `/hqp/discover` baseline: 500 in this sandbox, direct probe
`multicast send FAILED: [Errno 65] No route to host`, and neither failing test file touched by this
amendment. **#339 remains unmerged**, so PR CI may still show the known Rust 1.97 base lint failure on
`v3`.

### Scope still held

One `src` file — `src/adapters/hqplayer.rs`, the `framing` module and `send_command_inner`. No route,
request, response or payload change; `api-change-approved` not applied. **No #347 change was
implemented.** ADR 003 still not amended: the framing internals were restructured, but the decision it
records — three layers, asserted through the adapter's public surface — is unchanged.

### Adjustment `/review`, `/dissent` and `sg review`

**`/review` — one finding, applied.** The blocker-2 defence assertion was passing for the wrong reason.
`<Status note="</Status>"` reads `Incomplete` because the root tag never closes, so `root_element`
returns `None` and the quote-aware scan never runs — it would have passed whatever the boundary rule
did. Replaced with a closing-tag literal inside a *child* attribute with the root open, plus the case
where a real close follows the literal. Label audit: **24 tests in the amendment section, 24 contract
labels, none missing, no non-contract labels.**

**`/dissent` — two findings, both applied, both mine.**

1. **First-defensible-boundary opened a hole the last-token rule did not have.** The scan was
   quote-aware but not comment- or CDATA-aware, and neither form is quoted:

   ```text
   <Status a="1"><!-- </Status> -->        classify=Complete  first_end=Some(28)
   <Status a="1"><![CDATA[ </Status> ]]>   classify=Complete  first_end=Some(46)
   ```

   A **truncated** document read as complete — the single failure recovery exists to prevent. Fixed
   with `comment_or_cdata_len`; an unterminated comment or CDATA consumes the remainder, because there
   is no boundary inside something that has not ended.

   **This is the second time in this amendment that a fix introduced the class of defect it was
   closing** — the first was reintroducing semantic-parser evidence inside the fix for it. Worth
   naming as a tendency rather than a coincidence.

2. **Three factual errors, one channel.** All three came from reading a *report about* HQPTuner as
   though it were HQPTuner: the enum-ID renumbering (Stage 1 dissent), the nonexistent
   `fake_control.py` path, and the superseded `active_mode` claim (both this gate). **None of my own
   review passes caught any of the three.** So it is a channel, not three mistakes — and 14 fixtures
   were citing upstream URLs as `source:` with the same defect, just without a broken path to expose
   it. All 14 now record `source_chain: read-via-report`, enforced by
   `every_upstream_citation_records_how_it_was_obtained`.

**`sg review pr` — no new blocking finding on the adjustment.** Four of its five points are standing
maintainer questions it credits this PR for surfacing: proportionality (~8,000 lines of harness against
a focused production diff), the three parsing implementations, four nested self-review rounds in one
PR, and the `.oh` file being closer to a transcript than a design doc. Its remaining point — that the
corpus's `verified` labels rest on second-hand summaries — was **partly addressed by the `source_chain`
work in the same pass, and the residual is now fixed**: three fixtures claimed bare `verified` while
resting on a salvage report, and are now `verified-upstream`, guarded by
`no_fixture_claims_uhc_verified_status_on_second_hand_evidence`. A bare `verified` is reserved for a
first-hand UHC capture, which this corpus does not have. `is_verified()` is a prefix match, so the
observed-versus-derived distinction is preserved while the claim now names whose observation it is.

**sg's explicit ask, carried forward unresolved:** a human sign-off on the
infrastructure-versus-immediate-value ratio, because "amortised across five future issues" only pays
off if those issues consume this harness rather than routing around it. This amendment's own dissent
reached the same conclusion from the other direction.

### Recorded, not resolved

- `classify` builds a `Vec<String>` of open element names, so 4 MiB of pathological nesting yields tens
  of megabytes. Bounded, not unbounded. "Nothing unbounded is allocated" is precise about the
  accumulation buffer and should not be read as covering every derived allocation.
- The synthetic profile is not hermetic: it defines two filter documents and falls back to the Opal
  profile for everything else.
- The two-label contract is audited mechanically but not structurally guarded; a 25th test could omit
  a label.
- `source_chain` makes the read-via-report channel visible without closing it. **#341** owns the
  first-hand or live confirmations for all 14 flagged claims.

### Verification at the pushed HEAD

| Command | Result |
|---|---|
| `--test hqplayer_conformance` | **164 passed; 0 failed; 0 ignored** (162 at `5f37974`, 157 at `ff70554`, 138 at `6b8e97f`) |
| lib unit tests | 84 passed |
| `--test adapter_integration` / `--test zones_sha_integration` | 42 / 20 |
| `--test api_contract` / `--test protocol_schema` | 2 / 41 — no route or payload drift |
| `cargo fmt --check` | clean |
| `cargo clippy -- -D warnings` | clean, 0 findings |
| `git diff --check origin/v3...HEAD` | clean |
| `cargo test --no-fail-fast` | **473 passed; 2 failed; 12 ignored** |
| live tier 1 / tier 2 | **not run** — daemon offline, no mutating permission |

The two failures remain the reproduced `/hqp/discover` baseline. **#339 remains unmerged**, so PR CI
may still show the known Rust 1.97 base lint failure on `v3`.
