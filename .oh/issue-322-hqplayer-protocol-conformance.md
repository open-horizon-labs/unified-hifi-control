# Issue #322 — HQPlayer executable native-protocol conformance harness

**OH:** 80222d6d
**Program:** #310 · **Epic:** #311 · **Issue:** #322
**Branch:** `feat/issue-322-hqplayer-protocol-conformance` · **Base:** `origin/v3`
**Stage:** 1 (solution decision) — planning artifact only, no production or test behavior changed.

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
