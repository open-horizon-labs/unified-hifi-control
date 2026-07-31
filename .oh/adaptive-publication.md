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

> **Historical revision 1 — withdrawn.** The separate-broadcast-bus recommendation and its
> lifecycle decisions below are retained only as the audit trail for why revision 2 was
> necessary. They are not current design requirements. The authoritative recommendation,
> decisions, and invariants begin at **“Solution Space — revision 2.”**

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

Seven, each recorded here because "an ADR paragraph is not an owner". Decisions 6 and 7 were
added after dissent review on PR #363.

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

**6. Three more published-and-unapplied invariants. Decision: refuse.**

The same defect class as decision 3 — *the contract published a rule and no code path applied
it* — found in three more places by dissent review on #363.

| Invariant | Refusal | Why refuse rather than repair |
|---|---|---|
| A control publishes one value lane twice | `DuplicateValueLane` | One lane is one reading. Two `desired` lanes make "the staged value" ambiguous, and because `effective_view` resolves a single lane, C1 could be satisfied by one and contradicted by the other in the same document. Picking one invents a disambiguation the producer never made. |
| The document publishes health for one transport lane twice | `DuplicateLaneHealth` | `LaneWitness` is keyed by lane, so a duplicate is not redundant but unrepresentable. Folding it would silently keep whichever entry came last in a `Vec`. |
| `Availability` is not well formed | `AvailabilityNotWellFormed` | A consumer that can see "not usable" but not why cannot explain itself, and the user cannot escape the state that caused it. Inventing a reason tells the user something no producer said. |
| `LaneHealth.last_success` is not RFC 3339 | `LaneTimestampNotRfc3339` | `Timestamp` documents itself as RFC 3339 and every freshness judgement has to parse it. An unparseable value is not a degraded reading but no reading, and admitting one puts a string no consumer can interpret where one it can is expected. Refused on the *first* document too: there is no predecessor to fall back on, so the alternatives are publishing garbage or silently blanking a field, and blanking hides a producer bug only its author can fix. |

**Unrecognized vocabulary members are held to all of these.** Forward compatibility means an
unknown member stays *representable* — the control is kept, its observed value is kept, the
unknown state passes through unnormalized — not that structural invariants lapse when a name is
unfamiliar. An unknown availability state is precisely where a reason matters most, because a
consumer cannot even fall back on knowing what the state means. This is a narrower reading than
decision 3 takes of `LaneValue::is_consistent`, and the difference is real rather than an
inconsistency: that predicate returns `false` *because of* an unrecognized grounding, so
applying it would refuse a document for using a newer vocabulary, whereas these turn only on
shapes every version agrees about.

**7. Adapter lifecycle is settled by internal run identity, never by the public bus.**

`crate::bus::BusEvent` and `AdaptiveEvent` are independent broadcast channels drained by one
`select!` that imposes no order between them. So this interleaving is unremarkable: run *N*
publishes a document; it sits unread in the adaptive channel; `AdapterStopping` then
`AdapterConnected` are both processed from the public channel; only then is the document read
and admitted as current. **Any scheme that treats a public-bus event as the "everything before
this is stale" marker is defeated by that ordering** — including an epoch tombstone cleared on
reconnect, which the reconnect disarms before the straggler it exists to stop arrives. The
mirror case breaks too: a delayed `AdapterStopping` from run *N*, read after run *N+1* has begun
publishing, would flush a live run's producers.

So identity is carried *in the event* and allocated **synchronously**:

* `AdapterRuns::begin` takes a lock, allocates a monotonic `AdapterRunId`, and records it as
  that adapter's live run **before returning** — so "run *N+1* is live" is already true before
  run *N+1* publishes anything.
* Every ingress `AdaptiveEvent` carries a `PublicationOrigin { adapter, run }`. Publications and
  removals from a run that is not live are refused (`StaleAdapterRun`) or ignored, however long
  they sat in a channel and whatever else was processed meanwhile.
* **Withdrawn:** `AdapterStopping` was treated as a hint that triggered a sweep. Revision 2
  proved even that destructive sweep unnecessary and unsafe as a source of state removal.
  Stop events are now informational; ended-run snapshots project `LastKnown`, and only an
  explicit producer retirement removes state.
* Ordering therefore comes from a counter under a lock, not from the relative delivery order of
  two channels — the one thing this design cannot assume.

**Run identity is not producer epoch, and the two lifecycles stay apart.**

| | Whose | Meaning |
|---|---|---|
| `AdapterRunId` | ours | one connection attempt by one adapter in this process |
| `ProducerEpoch` | the producer's | its own restart counter, which only it can bump |

An adapter reconnecting to an unchanged engine is a **new run at the same epoch and must be able
to republish** — so adapter lifecycle never writes an epoch bar, which is what stranded that
case. A new run's first publication also *supersedes* the previous run's snapshot rather than
being ordered against it, because an ended run's view is no longer evidence of anything and
comparing revisions across the boundary would refuse a reconnect that legitimately sees a lower
counter. Lane witnesses survive: those are aggregator-observed and belong to the lane, not to our
connection.

An explicit `ProducerRemoved` is the *other* lifecycle and keeps its own rule: the producer said
it is gone, so only the producer can contradict that, with a strictly higher `ProducerEpoch`. No
adapter reconnect clears it.

**Residual, recorded rather than mitigated.** An adapter that dies without ending its run leaves
that run live, so its stragglers stay admissible until something begins a new run for the same
adapter — `begin` supersedes unconditionally, so one reconnect is enough. Already-admitted
producers of a crashed adapter linger until a stop hint or a new run arrives; nothing times them
out, because the aggregator holds no clock.

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

## Solution checkpoint: the boundary lints
**Updated:** 2026-07-30

Raised after the seventh escape from the hand-written scanner in
`tests/adaptive_publication_lint.rs`. The escapes, in order, each found by an external
reviewer and each failing in the direction that reports success:

| # | Escape | Found by |
|---|---|---|
| 1 | only the nearest `#[derive(` was inspected | CodeRabbit |
| 2 | an attribute wrapped across lines was cleared by its own continuation line | CodeRabbit |
| 3 | `use serde::Serialize as EventWire;` — no literal `Serialize` | CodeRabbit |
| 4 | a crate-local `pub use` re-export — no literal `serde` in the file | CodeRabbit |
| 5 | `r##"… " … ] …"##` — string mode left at the embedded quote | CodeRabbit |
| 6 | `pub use` — the boundary rule rejected exactly that one visibility form | Codex |
| 7 | `'\''` — the char lexer stopped at the escaped apostrophe | Codex |
| 8 | `#[allow(unused_imports)] use …;` — an attribute is not a statement boundary | Codex |

Eight commits, 1,982 lines, 56 helper functions, and the escape rate was not falling.

### Candidates

| Option | Approach | Trade-off |
|---|---|---|
| A | Continue the shared lexer: lex identifier tokens outside comments and literals, as Codex first suggested | Fixes escape 8 and probably 9. Each round has produced a *new* class rather than a repeat, so the fix is another special case in an incrementally hand-written Rust lexer living in a test file |
| B | Parse with `syn`, inspect the AST | Escapes stop being detected and become unrepresentable. Costs a parse-failure path and knows nothing about names |

### Evaluation

**A — continue scanning.** Solves the stated problem: for the reported instance, yes; for
the class, no. Cost: low per round, unbounded in aggregate. Second-order: the file had
become a Rust lexer written by defect report, in a test, maintained by whoever last got
reviewed. The recurring shape is the argument — *a text scanner approximates a parser, and
every approximation has a boundary an adversary finds before its author does.*

**B — `syn` AST.** Solves the stated problem: yes, structurally. Cost: one rewrite, zero
new dependency — `syn = { version = "2", features = ["full", "parsing", "visit"] }` is
already a direct dev-dependency, and six sibling lints already call `syn::parse_file`
(`await_in_lock_lint`, `spawn_cancellation_lint`, `ignored_send_lint`, `oneshot_leak_lint`,
`arbitrary_find_lint`, `unbounded_channel_lint`). Second-order: attributes become a
`Vec<Attribute>` on the item however they were formatted; `UseTree::Rename` becomes a
variant rather than a spelling; visibility and attributes become fields of `ItemUse`
rather than characters preceding it; and lexing raw strings and char literals correctly
becomes the parser's job by definition. Every one of the eight escapes above is closed by
the representation rather than by a check.

### Recommendation

**Selected: B.** The ad-hoc lexer is deleted rather than kept alongside — `code_only`,
`join_wrapped_attributes`, `attributes_in`, `string_literal_end`, `char_literal_end`,
`starts_a_use_statement`, `imports_in`, `derive_tokens`, `collapse_whitespace`,
`split_top_level`. Keeping both would claim a joint sufficiency neither has.

The module gates moved to `syn` too: `ItemMod` with structured `Meta`, so
`any(feature = "server", feature = "web")` is rejected by having no accepting arm rather
than by a string test for `any(`.

1,982 lines → 887. 34 tests → 17, because the probes became table-driven corpora rather
than one test per spelling; assertion count rose.

### Contrary case

**`syn` resolves no names.** `use crate::wire::EventWire;` is an import of *something*;
that it is `serde::Serialize` re-exported lives in another file, and a parser will never
know. If the guarantee had been "detect serialization", B would fail exactly where A did.

It does not fail, because the guarantee is an **allowlist**: parsing makes enumeration
exact, and the allowlist decides without needing to know what a name means. That is the
load-bearing pairing — either alone is insufficient, and the previous rounds failed
precisely because they tried detection alone.

**The residual I first recorded was not one.** I wrote that macro-generated items escape
both architectures and accepted it. Codex rejected that: `#[event_wire] pub enum
AdaptiveEvent {}` and `make_event_wire!(AdaptiveEvent);` are live false passes, and they
close the same way everything else here did — by permission, not detection. Two more
allowlists: `AdaptiveEvent` may carry only `derive` and `doc`; every macro invocation in
`src/producers/event.rs` must be on a list holding only `tracing::trace`.

The reviewer then caught a second-order error in my first attempt at that closure: I
collected only `Item::Macro`, while Rust permits a statement-position macro to expand to
item definitions and a trait impl is valid wherever written. The escape simply moved one
level down into a function body — and the module's real `tracing::trace!` call sits there,
which is why an empty allowlist had passed. Replaced with a `syn::visit::Visit` collector
over every `syn::Macro` at any depth.

A third round found the closure still too narrow. The checks audited `AdaptiveEvent`'s own
attributes, but a proc macro emits arbitrary *items* rather than only code about what it
annotates — so `#[generate_event_wire] fn helper() {}` or
`#[derive(GenerateAdaptiveWire)] struct Helper;` elsewhere in the module can emit
`impl Serialize for AdaptiveEvent` with the enum's attrs, the imports, the `ItemImpl` list
and the macro allowlist all clean. Closed with a location-aware module-wide policy: `doc`
anywhere, `derive` only on `enum AdaptiveEvent` and only holding `Debug`/`Clone`, nothing
else anywhere. That is now the production gate; the enum-specific checks are kept only
because they name an offender more precisely.

**What genuinely remains:** an *allowlisted* macro or derive could expand to anything, and
expansion is not in this source. That is why the lists are minimal rather than convenient —
the guarantee is that nothing generative is permitted, not that generation is understood.

### Evidence

All eight escapes replayed against the new architecture as source snippets through the
production helpers, plus four replayed against the real `src/producers/event.rs` with the
build confirmed clean each time (three fail the allowlists, one fails the derive list).
Five mutations, each verified to have applied first: stop recursing into `cfg_attr`; widen
the derive allowlist; drop `UseTree::Rename`; accept `any(...)` as a server gate; swallow
parse errors. Each is detected.

## Solution Space: lifecycle soundness (revision 2)
**Updated:** 2026-07-31
**Supersedes:** the `AdapterStopping`/`AdapterConnected` tombstone lifecycle recorded in
decision 7 above. That design is withdrawn, not amended.

**Problem:** aggregator-owned adaptive state must be non-regressing, non-resurrectable,
lifecycle-correct and recoverable under concurrency, crash/cancel, reconnect and
bounded-channel lag — with `BusEvent`/HTTP frozen and unadmitted documents internal.

**Key constraint:** lifecycle authority was distributed across two independent async channels
plus a lock-free registry, and every decision read state at a moment unrelated to when it
acted. No patch to a *rule* fixes that; it is a shape problem.

### Root causes behind the nine dissent findings

| Root cause | Findings |
|---|---|
| **R1. Decision and mutation are not atomic** — registry `Mutex` and store `RwLock` are separate; liveness is checked before `store.write().await` | 1, 7 |
| **R2. Lifecycle facts travel as lossy, unordered, under-addressed events** — `broadcast` drops on lag; two channels have no mutual order; retirement carries no key or epoch. Documents survive loss (idempotent snapshots); *transitions* do not | 3, 5 |
| **R3. Ownership and ordering are inferred, not recorded** — cleanup string-matches `producer_type`/id prefix; refusals carry no origin; a new run discards ordering instead of rebasing it; dead runs are never reaped and presence trusts the producer's own flag | 2, 4, 6 |
| **R4. Validation coverage is incomplete** — orthogonal to lifecycle, needed under every candidate | 8, 9 |

Two reframes drove the candidate set. **Commands are not notifications:** publish and retire
must not be lost, must be ordered, and deserve a verdict; admitted/refused are fan-out and
losing one is harmless because a reader can re-read. One lossy fan-out carried both, and that
conflation *is* R2. **Watermarks, not tombstones:** a tombstone is a negative record you must
remember and can fail to create (finding 5); a monotonic watermark keyed on producer identity
makes resurrection unrepresentable, covers keys never seen, and survives reconnect because it
is not keyed on the adapter run.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Local Optimum | Atomic run-registry/store locking; explicit rebase rule replacing `previous = None` | Closes R1 and finding 2; R2/R3 untouched; lock discipline is an unenforceable convention |
| B | Reframe | Single-consumer bounded mpsc command ingress; `broadcast` for admitted/refused egress only | Backpressure replaces loss; needs a queue policy per command kind |
| C | Reframe | Synchronous aggregator command API; adapters await a verdict | Inverts the adapter→aggregator dependency the architecture forbids |
| D | Redesign | Durable journal, per-run sequence, replay | Gap detection and restart recovery; introduces persistence #327 owns |
| E | Redesign | Single-writer actor, bounded mpsc inbox with oneshot replies, watermark ledger keyed on producer identity, RAII run leases, read-only snapshot handle, lossy egress broadcast | Largest change; read projection still needs a short shared lock, while mutation authority stays with one actor |

### Evaluation

**A — atomic locking.** Partial. Closes findings 1, 7 and (with a rebase rule) 2; leaves 3, 4,
5, 6. "Never check liveness outside the store lock" is not expressible in the type system, so
the next inserted `.await` silently reopens finding 1. Couples publication throughput to
lifecycle churn. Subsumed by E.

**B — bounded mpsc ingress.** Substantial. One consumer means no concurrent mutation, so R1
dissolves rather than being guarded; bounded mpsc replaces loss with backpressure (finding 3);
total ingress order removes the "two channels, no mutual order" premise, so `AdapterStopping`
can stop being an input. Needs an explicit per-kind queue policy. Does not by itself fix 2, 4,
5, 6.

**C — synchronous command API.** Mechanically sufficient, disqualified on dependency direction:
adapters would hold the aggregator, which `docs/ARCHITECTURE.md` and `AGENTS.md` forbid — this
is Option E of the original analysis, rejected then for the same reason. It also serialises
adapter poll loops against the aggregator. Its one desirable half — a synchronous verdict — is
recoverable inside E via a oneshot reply, because the publisher then holds only a sender.

**D — journal + sequence + replay.** Right answer to a question #324 was not asked. Durability
is #327's, and a journal breaks #324's cleanly-revertible property ("a plain revert; no data
migration, nothing persists a producer document"). The journal and explicit per-run sequence
remain deferred together; the bounded inbox provides the in-process order #324 requires.

**E — actor + watermark ledger + RAII leases.** Addresses R1, R2 and R3 as one shape rather
than three patch sets: exclusive write ownership plus one run/store linearization guard kills both TOCTOU classes; bounded lossless inbox
kills finding 3; a watermark ledger outliving snapshots kills 2 and 5 including unseen keys;
recorded origins kill 6 and 7's refusal clearing; RAII leases plus read-time presence kill 4.
Cost: the read path becomes a snapshot handle backed by a short read lock, which is an API
decision for #326 rather than an implementation detail.

### Recommendation

**Selected:** Option **E**, absorbing A's atomicity through an explicit run/store guard, B's
channel split as its ingress, and C's oneshot verdict as an option on commands. D's journal and
sequence layer remain deferred with durability to #327.
**Level:** Redesign.

Seven of nine findings are consequences of three structural facts. Patching them individually
is what produced the withdrawn design: each fix was locally correct and the composition was
not. A single writer plus a guard held from liveness decision through store commit makes the
check/act race unrepresentable; a watermark ledger makes resurrection unrepresentable; recorded
origins make ownership a fact rather than a guess.

**Accepted trade-offs.** The read path changes shape. Watermarks grow with distinct producer
identities and nothing evicts them within a process. Backpressure becomes observable to
publishers — the correct failure mode, but a new one. RAII covers cancel, task abort, and
panic-unwind, but not `mem::forget`, process abort, or `panic = "abort"`; unconditional
supersession on `begin` stays as the backstop.

### Decisions fixed for #324 (owner-confirmed)

1. Publishers **may await** bounded backpressure.
2. **No document coalescing.** Every ingress command is lossless and ordered.
3. Retirement is scoped `producer_id` + `epoch` + `origin` and **must cover unseen keys**.
   The explicit epoch is the sole new retirement authority: rejected documents identify keys
   to clear but their claimed epochs never raise a floor.
4. Watermarks stay **in memory**; durable journal/replay deferred to **#327**.
5. `AdapterStopping` is **not** an authoritative ingress path — run leases are. Ended runs
   remain visible as `LastKnown`; only explicit producer retirement removes snapshots.
6. Presence **degrades to `LastKnown`** whenever the admitting run is no longer live.
7. Reads go through a **read-only snapshot/watch handle**, not synchronous aggregator calls.
8. Frozen `BusEvent`/HTTP/API contracts maintained.

### Invariants

- **I1 Serialization.** No `.await` between reading lifecycle state and committing the mutation
  it authorises. One actor owns ingress writes, and the run guard remains held through the
  synchronous store mutation; read projections acquire locks in the same run-then-store order.
- **I2 Watermark monotonicity.** Per key, `high_(epoch, revisions)` and
  `retired_through_epoch` never decrease — including across explicit retirement. A refused
  document changes neither; the applied retirement floor is the explicit request or a higher
  floor already retained, and egress reports that applied value.
- **I3 No resurrection.** Admitted only if `epoch > retired_through_epoch` **and**
  `(epoch, revisions)` does not regress against the watermark — regardless of run, and
  regardless of whether the key was ever seen. Retirement through epoch *N* removes only
  snapshots and diagnostics at epoch *N* or lower; a newer admitted restart remains visible.
- **I4 Run authority has an explicit linearization point.** Publications are checked against
  run liveness in the same synchronous turn as store mutation. Retirement reserves bounded
  capacity and commits authority while the lease is live; once committed, dropping the lease
  cannot revoke the queued retirement. A stale lease returns `StaleAdapterRun` before enqueue.
- **I5 Ownership enforced at the trusted internal namespace boundary.** Every
  publication/retirement must use an allocator-issued live lease whose adapter matches the
  producer-id namespace. Origins cannot be synthesized through the public API; a wrong-owner
  command failure is returned/emitted but never occupies the producer's retained document-
  refusal slot. No cleanup path matches `producer_type`, and the adapter/producer prefix
  convention remains trusted internal input.
- **I6 Retirement is total and lossless through its floor.** Retiring *P* through epoch *N*
  removes every known key/diagnostic of *P* at *N* or lower and installs a producer-wide floor
  covering unseen keys, without removing a newer restart. It cannot be dropped by channel
  pressure and returns an explicit committed, applied, or stale-run outcome.
- **I7 Command losslessness.** No ingress command is dropped. A non-lossy cancellation token
  closes ingress only after producing adapters stop, then drains accepted commands before the
  composition root joins the actor. SSE uses a separate early-cancellation token. Wrong-owner
  commands fail synchronously before queueing (including detached publication), so they never
  depend on a lossy hint. Only state-change egress notifications may be lost; consumers recover
  by re-reading the view.
- **I8 Presence honesty.** `Live` only if the admitting run is live, evaluated at read time so
  it does not depend on a cleanup command having been delivered.
- **I9 Timestamp validity.** Every `Timestamp` in an admitted document parses as RFC 3339 —
  all fields, not only `LaneHealth.last_success`.
- **I10 Alias closure.** No second name for internal-event types under any type syntax, and no
  exported root function, const/static, struct/union field, enum payload, trait associated
  item, trait/impl/generic bound, inherent-method signature, private type/import alias chain,
  forbidden glob, impl target, or exported/re-exported macro can launder contract/publication
  types.

### Unresolved assumptions

- Watermarks are unevicted within a process. Fine for #324; revisit jointly with #327 if they
  must survive restart.
- Retirement is producer-scoped, so it cannot retire one target of a multi-target producer.
  Target-granular retirement remains deferred to #325.
- Presence degradation on a dead run is a semantic #326's consumers have not been told about.
- `AdapterStopping` is informational only. Last-known snapshots remain until explicit producer
  retirement; that retirement emits an egress invalidation hint.
- Direct aggregator helpers remain available in ordinary **debug-profile** builds because Rust
  integration tests compile the library as a dependency and do not activate its `cfg(test)`.
  This is an explicit compatibility seam, not a test-only claim. Release builds structurally
  omit the re-export and direct methods; the actor-only guarantee is a release-artifact
  guarantee. `cargo test --release --test adaptive_publication` consequently does not compile,
  while the release library/application check does.

## Ship
**Updated:** 2026-07-31
**Status:** implementation and local gates green; exact-head review/dissent and push pending;
draft PR only

**Delivery path.** PR #363 (draft) → base `feat/issue-323-adaptive-producer-contract`
(PR #362, draft/CLEAN) → base `fix/issue-338-rust-1-97-lint` (PR #339, draft) → `v3` →
retarget each dependent PR as its base merges → CI on the `v3`-targeting hop → maintainer
review → squash merge. Three stacked hops; neither prerequisite is merged.

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
3. **The pre-existing `v3` Rust 1.97 lint repair is now the bottom stack.** #338 is represented
   by draft PR #339 (`fix/issue-338-rust-1-97-lint` → `v3`); #362 currently targets that branch
   and GitHub reports it CLEAN. This branch still cannot reach `v3` until both prerequisites
   move.
4. **`docker.yml` is `master`-only**, so a `v3` merge publishes no image. Release artifacts
   only on tag `v*` or a published release.
5. **The stack, not a merge conflict, is the current gate.** PR #362 is draft/CLEAN, while
   bottom PR #339 remains draft; no hop is authorized to merge automatically.

**Rollback.** A plain revert. `src/producers/` is referenced from exactly two places outside
itself — `pub mod producers;` in `src/lib.rs` and the construct-and-spawn in `src/main.rs` —
and nothing publishes to it, so reverting removes the bounded actor and its shutdown
subscription. No data migration: nothing persists a producer document. The one caveat is
`ReasonCode::ControlRemoved` and `control_removed_after_advance.json`, which are contract
surface: reverting those after #325 maps onto them would be a compatibility break, so this
rollback is clean only while this branch is the tip.

**Latest local evidence:** the full all-features suite, focused lifecycle 25/25 (also 25/25 in
the release profile), publication 107/107, contract isolation 57/57,
`cargo clippy --lib --all-features -- -D warnings`, and
`cargo check --release --all-features` all pass after the final dissent round.
`tests/fixtures/api_routes.txt`
remains byte-identical to `origin/v3`; zero `.route(` lines were added or removed in
`src/main.rs`; `src/api/` and `src/bus/` are untouched; PR #363 carries no labels. The worktree
is intentionally dirty until the reviewed checkpoint is committed; no force push is used.
