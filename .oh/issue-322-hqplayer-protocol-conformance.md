# Issue #322 — HQPlayer executable native-protocol conformance harness

**OH:** 80222d6d
**Program:** #310 · **Epic:** #311 · **Issue:** #322
**Branch:** `feat/issue-322-hqplayer-protocol-conformance` · **Base:** `origin/v3`
**Stage:** 2 (execution, complete pending Codex review) — Stage 1 sections are unchanged. Stage 2
evidence is appended at the end; the *red checkpoint* section is an early snapshot, superseded by
**Stage 2 execution record** below.

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
