# Adaptive Producer Publication (UHC #324)

Session for GitHub issue #324 — *Adaptive control: publish producer documents through the
bus and aggregator*. Parent epic #312. Prerequisite #323 / PR #362, consumed at exact head
`2f185938ec18d542e424448afb3f65d499977569`.

## Aim

The aggregator owns every current adaptive producer document, admits them under one gate
that no path bypasses, and serves atomic snapshots to in-repo consumers — while nothing
about this contract becomes visible outside the repository, and no HTTP route, SSE payload
or request/response schema changes.

Out of scope for #324: HQPlayer mapping (#325), matcher/manifest composition (#326),
persisted bindings (#327), catalog provenance (#343), and **any** consumer-facing exposure
(HTTP, SSE, MCP, device). See "The reshaping window" below for why the last one is a
decision rather than an omission.

## Solution Space

**Updated:** 2026-07-30

**Problem:** Adapters must publish versioned producer documents and the aggregator must own
them as authoritative state — but the obvious carrier for "publish an event" in this
codebase is `BusEvent`, and `BusEvent` *is* a public wire payload.

**Key constraint (binding, and it is not the one the issue title implies):**
`src/api/mod.rs:1210` serializes **every** `BusEvent` verbatim into the `GET /events` SSE
stream. `docs/ARCHITECTURE.md:96` documents that stream as consumable by "any HTTP client
(curl, ESP32, etc.)". Therefore *adding a variant to `BusEvent` is a response-schema change
to an existing public endpoint, and it publishes the v1 contract to consumers outside this
repository* — two of this issue's hard prohibitions, tripped by the single most idiomatic
move available. Every candidate below is scored first on whether it trips that wire.

**Success looks like:**

* An incoherent document cannot reach a consumer, because the only object a consumer can
  obtain is one the gate produced.
* `tests/fixtures/api_routes.txt` byte-identical; `cargo test --test api_contract` green;
  the SSE payload set unchanged, proved rather than asserted.
* Multiple producers, restart (epoch), reconnect (lane health), removal, out-of-order
  delivery, unknown additive fields, and live-command isolation each have a
  consumer-expectation test that failed before its implementation existed.

### Situated knowledge

RNA MCP (`oh_search_context`) is **not connected** in this session, so no repo-local metis
or guardrails were retrieved. Prior-art substitute, read directly: `.oh/`,
`docs/architecture/adaptive-producer-contract-v1.md`, `docs/adr/003-*`, `docs/ARCHITECTURE.md`,
`tests/architecture_lint.rs`, `tests/aggregator_lint.rs`, `tests/adaptive_dependency_lint.rs`,
and the #362 remediation commits `1577948`, `fdc1ca4`, `2f18593`.

Two constraints come from that reading rather than from the issue text, and both bind:

1. **`fdc1ca4` assigned zone-prefix validation to this issue.** `ProducerTarget.zone_id` is
   a plain `String`; `PrefixedZoneId` is `#[serde(transparent)]` so its derived `Deserialize`
   never calls `parse`. The field's own doc comment (`src/adaptive/document.rs:241`) says
   validation "belongs to whoever admits the document into the aggregator (#324), where the
   vocabulary of valid prefixes lives."
2. **`pub mod bus` is `#[cfg(feature = "server")]`** (`src/lib.rs:46`) while `pub mod adaptive`
   is deliberately shared. Any publication layer that touches both is server-only by
   construction, and may not live inside `src/adaptive/`.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|--------|-------|----------|-----------|
| A | Band-Aid | `BusEvent::AdaptiveProducerPublished`; store documents in `ZoneAggregator` | Fastest; changes the `/events` payload set and publishes v1 outside the repo |
| B | Local Optimum | New `BusEvent` variants + a denylist filter in `events_handler` | Keeps SSE stable *if the filter is maintained*; raw unvalidated documents still reach every bus subscriber |
| C | Local Optimum | Adapters publish a tiny "revision advanced" event; aggregator pulls the document from the adapter | Small events; but this *is* aggregator-queries-adapter, and the pull races the next poll so snapshots stop being atomic |
| D | Reframe | Separate internal `AdaptiveBus`; the aggregator is the sole admission gate and the sole holder of an admitted document | Two buses to explain; a lint must prove the second one never reaches the wire |
| E | Reframe | No bus: adapters hold `Arc<ProducerAggregator>` and call `publish()` directly | Simplest path, synchronous refusal back to the adapter; inverts the adapter→aggregator dependency the architecture forbids |
| F | Redesign | Generalize `ZoneAggregator` into a typed snapshot store keyed by `(domain, identity)` and migrate zones onto it | The uniform end state; rewrites the drop-in-compatible zone path during an additive contract issue |

### Evaluation

**Option A — `BusEvent` variant + `ZoneAggregator`**
- Solves stated problem: No.
- Implementation cost: Low.
- Maintenance burden: High.
- Second-order effects: Disqualifying on three counts, any one of which is fatal.
  `events_handler` would emit a new SSE message type to every connected knob, iOS client
  and browser — a response-schema change to `GET /events` without approval, and
  out-of-repo exposure of a contract this issue is explicitly told to keep internal. It
  also puts producer state into a zone-shaped aggregator whose `run()` loop and
  `AdapterStopping` flush are zone-specific, and it leaves the C1/C2 gate as a match arm
  rather than a chokepoint. Rejected.

**Option B — `BusEvent` variants + SSE denylist**
- Solves stated problem: Partially.
- Implementation cost: Low-medium.
- Maintenance burden: Medium-high.
- Second-order effects: The SSE payload set can be held stable, but only by a negative
  rule someone must remember. Worse, the *unvalidated* document is on the public bus, so
  "an incoherent document never reaches a consumer" degrades from a structural property
  to a convention: any future subscriber that reads the ingress event bypasses the gate,
  and nothing stops it. A denylist also cannot prevent the enum from being *serializable*
  with adaptive payloads — one `serde_json::to_string(&event)` anywhere re-exports the
  contract. Rejected, but its SSE-stability test is worth keeping.

**Option C — revision-advanced event, aggregator pulls**
- Solves stated problem: No.
- Implementation cost: Medium.
- Maintenance burden: High.
- Second-order effects: Directly contradicts `docs/ARCHITECTURE.md` ("UI talks to
  aggregator only", "adapters are event publishers") and `AGENTS.md:165` ("Don't bypass
  the aggregator to query adapters directly") — inverted, but the same coupling. The pull
  also races the producer's next poll, so the aggregator can compose a snapshot that
  never existed, defeating the atomicity requirement that motivated the whole contract.
  Rejected.

**Option D — internal `AdaptiveBus`, aggregator as sole admission gate**
- Solves stated problem: Yes.
- Implementation cost: Medium.
- Maintenance burden: Low.
- Second-order effects: `BusEvent` is untouched, so the SSE payload set cannot change —
  not because a filter suppresses adaptive events, but because no adaptive type is
  reachable from the serialized enum. The architecture is preserved exactly: adapters
  publish, the aggregator owns, nobody queries an adapter. Because the gate is the only
  writer *and* the only thing that ever holds an admitted document, "incoherent documents
  never reach consumers" becomes checkable as an invariant over the store rather than a
  property of a code path. Costs: a second channel to document, and a lint owed to prove
  the separation is real rather than intended.

**Option E — direct `Arc<ProducerAggregator>` handle**
- Solves stated problem: Yes, mechanically.
- Implementation cost: Low.
- Maintenance burden: Medium.
- Second-order effects: Inverts the dependency — adapters would import the aggregator,
  which is the coupling `tests/architecture_lint.rs` exists to prevent, and it forecloses
  multi-consumer fan-out without re-adding a bus later. Rejected. **One idea salvaged:**
  its synchronous `Result` means the adapter learns its document was refused. Option D
  loses that, so refusals must be made observable another way — see "Refusals are
  observable" below.

**Option F — generalized snapshot store**
- Solves stated problem: Yes, and best for the epic's end state.
- Implementation cost: High *here*.
- Maintenance burden: Low.
- Second-order effects: Rewrites `ZoneAggregator`, which backs the API surface
  `tests/client_harness.rs` pins as a drop-in replacement for the Node.js server. Doing
  that inside an additive contract issue spends the issue's risk budget on a refactor
  nobody asked for. Deferred, not rejected; Option D leaves it available because the
  producer store is a separate type that could later be one instantiation of it.

### Recommendation

**Selected:** Option D — separate internal `AdaptiveBus`, aggregator as sole admission gate
**Level:** Reframe

**Rationale:** The reframe is *what kind of thing `BusEvent` is*. Reading it as "the event
bus" makes A and B look idiomatic; reading it as what it actually is — a public,
serialized wire payload with out-of-repo consumers — disqualifies both immediately. Once
the carrier is internal, the second reframe follows for free: the aggregator stops being a
store that happens to validate and becomes an **admission gate that happens to store**, so
the safety property is structural. A consumer cannot obtain a document that did not come
out of `admit()`, because no other object of that type exists in the process.

**Accepted trade-offs:**
- Two buses. Mitigated by a lint that proves the internal one is unreachable from the
  serialized enum and from `src/api/`, and by the internal one carrying no `Serialize`
  derive at all — it cannot be written to a wire even by accident.
- No synchronous refusal to the publishing adapter. Mitigated below.
- The producer store is a second store rather than a generalization of the first
  (Option F). Recorded as the deferred end state.

### The decisions this issue must make explicitly

Five, each recorded here because "an ADR paragraph is not an owner".

**1. C1/C2 incoherence → demote, never refuse, never fabricate.**

| Violation | Publication policy | Why |
|---|---|---|
| `MissingDesiredLane`, `DesiredLaneDisagrees` (C1) | demote entry to `RequiresProducerValidation` | The only repair that would preserve `valid` is inventing a `desired` lane — fabricating intent the user never staged. Demotion blocks apply and makes the effective projection advisory, which is exactly what spec §6 tells consumers to do with a non-`valid` entry. |
| `MultipleValidDrafts` (C2) | demote **every** claiming entry to `Conflicts` | The aggregator cannot know which draft the user meant. Keeping one `valid` would silently authorize an apply nobody confirmed, and any "keep the first" rule is a function of `Vec` order, which is producer-arbitrary. Demoting all is order-independent and satisfies C2 literally — "at most one valid" is satisfied by zero. |
| `UnknownControl` | demote to `DraftInvalid` with reason `control_removed` | Required to be *distinct and visible*: users must see "this control no longer exists", not the generic "needs producer validation". |

Demotion only ever lowers validity. It never adds a lane, never edits a value, never
touches a control. That is testable as a byte-level assertion — the repaired document must
differ from the published one in `change_sets[..].entries[..].validity` and nowhere else —
and it is the mechanical form of "never fabricate desired intent".

**2. Envelope violations → refuse the whole document.**

Unsupported major, unparsable/missing version, malformed body, `ConstraintTooComplex`,
revision regression, epoch regression, duplicate revision, invalid zone-id prefix, and the
two new invariants below. Refusal is safe *specifically because the aggregator retains the
last admitted snapshot*: a refused document does not blank a producer, it fails to advance
one. That asymmetry is the whole reason envelope problems can be strict while intent
problems must be lenient.

**3. New invariant — `LaneValue` consistency. Decision: refuse.**

`LaneValue::is_consistent` (`src/adaptive/value.rs:411`) is published as a rule and called
by nothing on the admission path. A deserialized document may therefore assert `grounded`
with no value, or `ungrounded` with one — simultaneously "I have no reading" and a
reading. This is the same defect class `fdc1ca4` fixed three times over: *the contract
published a rule and no code path applied it*.

Refuse rather than repair, because every repair picks a side. Dropping the value discards
producer data; flipping the grounding fabricates a reading. Neither is the aggregator's
call.

**4. New invariant — deserialized operation-history legality. Decision: refuse.**

`OperationRecord::transition` enforces `CommandOutcome::may_transition_to`, but
`history: Vec<OutcomeTransition>` deserializes unchecked, so an admitted document can carry
an outcome chain the contract calls impossible. Refuse the document rather than repair it:
the alternative repairs are dropping illegal transitions or truncating the chain, and both
destroy audit history that spec §7 declares append-only and that "a new operation cannot
erase". A corrupt audit trail must be visible as a refusal, not silently tidied.

Both 3 and 4 are enforced at the publication gate. Whether they also belong in
`admit_document` is an execute-gate question: the gate must hold for typed documents that
never passed through JSON, so gate enforcement is necessary regardless; contract-layer
enforcement would be defence in depth, and is only worth taking if it does not disturb #323.

**5. Zone identity is validated at admission.**

`ProducerTarget.zone_id`, when present, must parse as a prefixed zone id
(`roon:` / `lms:` / `openhome:` / `upnp:` / `hqplayer:`) via `bus::events::PrefixedZoneId`.
This discharges the obligation `fdc1ca4` recorded at the field, and it can only be done
here: the prefix vocabulary lives in the server-only bus module, which the shared contract
layer may not name.

### The reshaping window — decision, not deferral

The ship-gate dissent on #323 asked this issue to decide, before any outside-repository
consumer exists, whether v1's shape is right. **Decision: the window stays open, and this
issue deliberately does not close it.** #324 publishes to in-repo consumers only; no HTTP,
SSE, MCP or device surface is added. The evidence that would justify reshaping — which
constructs a real producer actually populates — is #325's to produce, and #325 is blocked
by this issue. Committing v1's shape now would be committing it on no evidence.

One reshaping is taken, and it is minimal: **`ReasonCode::ControlRemoved`** is added,
because decision 1 requires a removed control to be distinguishable from a control awaiting
validation, and no existing member carries that meaning. It is additive, and
`every_vocabulary_member_has_a_worked_example` will force the fixture that #324's own
acceptance criteria already demand — a change set whose base predates a control-plane
advance that removed its target. `CONSUMER_SCHEMA_VERSION` is **not** bumped: 1.0 has never
been released outside this repository, so this completes 1.0 rather than adding to it.

### Implementation Notes

1. `src/producers/` — new, `#[cfg(feature = "server")]`, because it necessarily touches
   both `crate::bus` and `crate::adaptive`.
   - `event.rs` — `AdaptiveEvent` lifecycle: discovered / updated / removed, plus the
     gate's own admitted / refused outcomes. **No `Serialize`/`Deserialize` derive**, so it
     cannot reach a wire even by accident.
   - `admission.rs` — pure functions: `admit(previous, incoming) -> Admission`. No tokio,
     no locks, directly unit-testable.
   - `aggregator.rs` — `ProducerAggregator`: owns `BTreeMap<ProducerKey, ProducerEntry>`,
     subscribes to `AdaptiveBus` for ingress and to the public bus for `AdapterStopping` /
     `ShuttingDown` only.
2. `src/aggregator.rs` is **not** moved or restructured: `tests/aggregator_lint.rs` reads
   it by literal path.
3. `ProducerKey { producer_id, role, zone_id }`, ordered, so snapshots are deterministic.
4. Ordering: epoch dominates. Higher epoch → accept unconditionally (revisions are
   incomparable across epochs). Same epoch → `DocumentRevisions::regresses_from` refuses.
   Equal revisions → refuse as a duplicate; lane-health changes are document state and
   must bump `state`. Lower epoch → refuse as out-of-order.
5. Per-lane last-good is aggregator-observed history kept **beside** the document, never
   merged into it: `LaneWitness { state, last_success, last_error, at }`, where
   `last_success` is monotonic across admissions. Editing the producer's own words would be
   the same fabrication C1 forbids.
6. Atomic snapshots: `ProducerSnapshot { document: Arc<ProducerDocument>, lanes, presence,
   repairs, admitted_at }`, produced under one read lock.
7. **Refusals are observable**, replacing what Option E would have returned synchronously:
   retained per key as `last_refusal`, queryable from the aggregator, emitted on the
   internal bus, and logged with `tracing::warn!`. A refusal that only appears in a log
   line is not observable to a test.
8. `recv()` must handle `RecvError::Lagged` by continuing, not by exiting. `ZoneAggregator`
   uses `while let Ok(event) = rx.recv().await`, which terminates the loop on lag; the new
   loop must not copy that. (Pre-existing, out of scope, worth reporting.)
9. Live-command isolation is structural: the aggregator mutates producer state on adaptive
   ingress only. Every other bus event is inert, and the test asserts a staged change set is
   byte-identical after a stream of `CommandReceived` / `ControlCommand` / `VolumeChanged` /
   `HqpPipelineChanged`.
10. Lints owed: no adaptive type reachable from `BusEvent`; `src/api/` never names
    `crate::producers`; `api_routes.txt` unchanged; the internal event type derives no
    serde.
11. No route changes. `tests/fixtures/api_routes.txt` untouched, `api-change-approved` never
    applied.

## Execute
**Updated:** 2026-07-30
**Status:** complete

Prerequisite #323 was remediated twice during this session. This branch was merged
forward-only onto `d6da952` at `49a55ed`; reports 1/6 and 2/6 were re-anchored there.

**TDD evidence.** `tests/adaptive_publication.rs` was written first against a deliberately
naive stub (admit everything, store blindly). RED: **29 failed, 24 passed**. After the real
gate and aggregator: **53 passed, 0 failed**. Full suite 408 → **472 passed, 0 failed**
across 24 binaries. `cargo fmt --check` clean; `cargo clippy --lib` clean.

### What the two new invariants found on day one

Enforcing decision 4 immediately refused a **canonical #323 fixture**.
`command_outcomes.json` recorded `disconnected -> indeterminate` on `op-provisional-9001`,
which `CommandOutcome::may_transition_to` forbids — nothing may regress into an unresolved
state — and which contradicted the same operation's `write_attempt:
acknowledged_provisional`, since `disconnected` means the transport was down *before* the
write was attempted. Nothing may legally transition *into* `indeterminate`, so that
operation's history is correctly empty.

The defect was load-bearing for a passing test:
`a_new_operation_cannot_erase_the_audit_trail` asserted `history` was non-empty and that
some transition had `to == Indeterminate`, which only the illegal entry could satisfy. It is
retargeted at `op-divergent-9015` and asserts `from == Indeterminate`, which is what the
comment above it always claimed to be testing.

This is the argument for the invariant, not an argument against it: a rule the contract
published and no code path applied had already produced a wrong fixture and a test shaped
around it.

### Decisions taken during execution

* **Equal revisions (obligation A4).** Neither binary. A republication at the same revision
  whose difference is confined to `lanes`/`stale` is admitted as `HealthRefresh`; anything
  else at the same revision is refused as `NotAdvanced`. Demanding a `state` bump for lane
  transitions would invalidate every open change set on every lane flap, because drafts are
  validated against the state revision. Checked by rebuilding the incoming document with the
  held lane health and comparing for equality.
* **Lane-value predicate (obligation A3).** `LaneValue::is_consistent` is **not** used at the
  door: it returns `false` for `Grounding::Unrecognized`, so admission would refuse a
  forward-compatible document wholesale. The gate refuses only recognized-grounding defects.
  `CommandOutcome::may_transition_to` needs no such narrowing — it already returns `true` for
  unrecognized outcomes, so decision 4 is forward-compatible as written.
* **C2 demotes both claimants (D2).** `draft_policy.on_conflict` is deliberately not
  consulted: it governs staging, and #324 implements no staging path. Tested with a document
  declaring `last_actor_takeover`.
* **`ReasonCode::ControlRemoved`** added, forced by the requirement that a removed control be
  distinguishable from one awaiting validation. `every_vocabulary_member_has_a_worked_example`
  then forced the fixture #324's acceptance criteria already demanded:
  `control_removed_after_advance.json`, a draft whose base predates the advance that removed
  its target. No `CONSUMER_SCHEMA_VERSION` bump: 1.0 has never left this repository.

### Obligations discharged

| # | Where |
|---|---|
| A1, A8 | `tests/adaptive_publication_lint.rs` — 11 tests, bidirectional probes |
| A2 | `ProducerAggregator::run` handles `Lagged` and continues; `record_lag`; `a_lagging_aggregator_keeps_running_rather_than_exiting` |
| A3 | `first_lane_defect`; `an_unrecognized_grounding_from_a_newer_minor_is_admitted_not_refused` |
| A4 | `ordering()` health-refresh branch; two `envelope::republishing_*` tests |
| A5, P2-2 | demotion keyed on `(change_set, control)`; `one_draft_holding_one_control_on_two_apply_lanes_is_left_alone` |
| A6 | every `AdmissionRefusal` names its offender; asserted in the envelope and lane-value tests |
| A7 | every test document originates from a canonical #323 fixture |
| D2 | `a_declared_last_actor_takeover_policy_does_not_change_publication_time_repair` |
| D4 | `apply_demotions` writes only `entry.validity`; `repair_changes_validity_and_nothing_else` |
| D5 | `IllegalOutcomeHistory` names operation and transition pair; `tracing::warn!` on refusal |
| D6 | `poll_loop::a_sequence_of_polls_always_serves_exactly_one_published_document` |

D1, D3, D7 are records rather than code and are discharged in this file and ADR 003.

**The probe earned its keep immediately.** `gate_scanner_detects_a_gate_it_must_not_miss`
failed on first run: `is_server_gate` compared against the spaced spelling only, so
`#[cfg(feature="server")]` read as absent — the same whitespace blind spot `1577948` fixed
in the sibling lint, caught here before it shipped rather than by a later review.

**Drift check:** none. `tests/fixtures/api_routes.txt` byte-identical to `v3`; zero `.route(`
lines added or removed in `src/main.rs`; `src/api/` and `src/bus/` untouched; no label on
PR #363.

**Verification gap:** `dx` is not installed here, so the WASM/fullstack build is proved only
by CI's `build-wasm` job. `lint_producers_module_is_server_gated` is the host-runnable proxy
and is the reason the module is gated at all.

### Review remediation

| Source | Finding | Fixed |
|---|---|---|
| execute review (3/6) | canonical fixture published an illegal outcome transition | `12a8e07` |
| execute review (3/6) | lane witness ordered timestamps lexicographically | `56cd0a9` |
| execute review (3/6) | surface lint enumerated six dirs, missed `src/mqtt` | `56cd0a9` |
| execute review (3/6) | coherence invariant test covered 3 of 4 cases | `56cd0a9` |
| execute review (3/6) | a refusal outlived the producer it named | `6a2ad99` |
| execute dissent (4/6) | fixed point argued, not checked → `IrreparableIntent` | `f34f28b` |
| execute dissent (4/6) | module doc overclaimed the structural guarantee | `f34f28b` |
| execute dissent (4/6) | "refusal is safe" untrue for a first document | `f34f28b` |
| execute dissent (4/6) | `ProducerRemoved` cannot retire one target of many | `f34f28b` |
| **CodeRabbit** | `is_server_gate` accepted `any(feature="server", …)` | `f098202` |
| **CodeRabbit** | surface sweep missed `use crate::{adaptive, producers};` | `f098202` |

Both CodeRabbit findings were in the lints, both were reachable, and the second was
reachable *using this repository's own import style* — `src/main.rs` groups its imports
exactly that way. That is now three consecutive rounds in this epic where the defect of
record was a lint reporting success on a violation, and the third where external review
found it before the internal gates did.

## Ship
**Updated:** 2026-07-30
**Status:** staged (draft PR, nothing merged, deployed or marked ready)

**Delivery path.** PR #363 (draft) → base `feat/issue-323-adaptive-producer-contract`
(PR #362, also draft) → #362 merges to `v3` → #363 retargets to `v3` → CI → maintainer
review → squash merge. Two hops, because this is a stacked PR and the prerequisite is not
merged.

**Delivery-path tax, measured rather than guessed:**

1. **This branch has no CI at all.** `build.yml` triggers on pull requests targeting
   `master`/`v3` and on pushes to `v3` or `feature/**`. This branch is `feat/…` and the PR
   targets `feat/issue-323-…`, so **no workflow has run on any commit here**
   (`gh run list --branch feat/issue-324-adaptive-publication` is empty). Everything green
   is green on a laptop. The first CI signal arrives only when the base becomes `v3`.
2. **`api-guard` will trigger once retargeted, and this PR touches one of its paths.**
   It watches `src/main.rs`, `src/api/**` and `tests/fixtures/api_routes.txt`; `src/main.rs`
   changed. Read rather than assumed: the job's only assertion is whether
   `tests/fixtures/api_routes.txt` differs from the base. It does not, so the job takes the
   `api_changed=false` branch and reports "No API contract changes detected". No label is
   needed and none must be added.
3. **A pre-existing `v3` lint failure blocks merge**, not caused here:
   `src/app/pages/zones.rs:269` trips `clippy::unnecessary_sort_by` on CI's newer
   toolchain. Owned by #338 ("CI: restore the v3 lint baseline under Rust 1.97"), which is
   open. #362 hit the same wall.
4. **`docker.yml` is `master`-only**, so a `v3` merge publishes no image. Release artifacts
   only on tag `v*` or a published release.
5. **PR #362 is `BLOCKED`** on GitHub's own mergeability check, so the first hop is not
   currently available regardless of this branch's state.

**Rollback.** A plain revert. `src/producers/` is referenced from exactly two places outside
itself — `pub mod producers;` in `src/lib.rs` and the construct-and-spawn in `src/main.rs` —
and nothing publishes to it, so reverting removes a task that subscribes to two channels and
waits. No data migration: nothing persists a producer document. The one caveat is
`ReasonCode::ControlRemoved` and `control_removed_after_advance.json`, which are contract
surface: reverting those after #325 maps onto them would be a compatibility break, so this
rollback is clean only while this branch is the tip.

**Verified rather than asserted:** `tests/fixtures/api_routes.txt` byte-identical to
`origin/v3`; zero `.route(` lines added or removed in `src/main.rs`; `src/api/` and
`src/bus/` untouched; PR #363 carries no labels; worktree clean; every commit
forward-only, no force push.
