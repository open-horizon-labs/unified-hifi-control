# Adaptive-Control Producer Document, v1

**Status:** specified, not yet published
**Schema version:** 1.0
**Issue:** [#323](https://github.com/open-horizon-labs/unified-hifi-control/issues/323) · **Epic:** #312
**Implementation:** `src/adaptive/` · **Contract tests:** `tests/adaptive_contract.rs`
**Decision record:** [ADR 003](../adr/003-adaptive-producer-document-v1.md)

This document is normative for humans. `tests/adaptive_contract.rs` is normative for
consumers: where prose and tests disagree, the tests are the contract and the prose is
the bug.

---

## 1. Purpose and scope

A **producer** is anything that owns controllable state — an HQPlayer engine, a
room-correction matrix, a future Home Assistant domain. A **producer document** is an
immutable, versioned snapshot of that producer's identity, controls, current truths,
constraints, staged intent and command outcomes, expressed so that a consumer with no
backend knowledge can render it, explain it and act on it.

The completion signal for the epic is one sentence: *a consumer with no HQPlayer-specific
code can inspect the document, render valid controls, explain disabled choices, invoke an
abstract action, and reconcile the resulting revision.*

### In scope for v1

Types, vocabularies, evaluation rules and the compatibility policy.

### Explicitly out of scope

| Concern | Owner |
|---|---|
| Publishing documents through the bus; aggregator ownership of snapshots | #324 |
| Mapping HQPlayer's protocol onto these types | #325 |
| Matcher-selected layouts and manifest v2 composition | #326 |
| Stable identity persistence and device bindings | #327 |
| Human-facing labels, help text and their licensing/provenance | #343 |
| Device capability and protocol-version negotiation | #314 |
| Any HTTP, SSE or MCP exposure | #324, with separate API approval |

**v1 adds no public API surface.** No routes are added, removed or modified;
`tests/fixtures/api_routes.txt` is unchanged. `tests/adaptive_dependency_lint.rs` fails if
a later commit on this line adds an adaptive route to the contract file.

### Architectural position

```
adapter ──publishes──▶ aggregator ──serves snapshots──▶ web / MCP / device / HTTP
  (#325)                  (#324)                              (#326)
                            │
                            ▼
                  producer document (this contract)
```

The aggregator remains the single owner of authoritative state. This contract is a
*description* of state, never a means of obtaining it: `src/adaptive/` depends on nothing
but `serde`, `serde_json` and `std`, and may not reference an adapter, the aggregator, the
bus, a route handler, the filesystem, the network or the clock. That is enforced by
`tests/adaptive_dependency_lint.rs`, which also keeps a future extraction into a
standalone `uhc-adaptive-contract` crate mechanical.

---

## 2. The central design decision: five truths, not one value

A control does not have "a value". It has up to five simultaneously legitimate values,
each with its own authority and freshness.

| Lane | Meaning | Authority |
|---|---|---|
| `observed` | what the running engine reports now | `runtime` |
| `persisted` | what the saved configuration says it will be next start | `persisted_config` |
| `desired` | staged intent accepted but not applied | `staged_intent` |
| `held` | intent for a chain, profile or feature that is not loaded | `held_intent` |
| `effective` | what an editor should display, projected from the others | `editor_projection` |

Only `observed` is evidence of audible reality. `effective` is never authoritative on its
own.

Collapsing these into one scalar is the failure the contract exists to prevent. The
worked case is fixed volume: on some daemon builds, a *disabled* fixed-volume feature
retains its last level in a commented configuration element. Feature-enabled, observed
active value and retained inactive value are three facts. A projection that shows one as
"the" value will eventually delete another — and a verifier that only re-reads the field
it wrote will report success after doing so.

Disagreement between lanes is **first-class data**, not an error to reconcile:
`Divergence` records which pair disagrees (`observed_vs_persisted`, `observed_vs_desired`,
`persisted_vs_desired`, `held_unreachable`) and when it was detected. A producer never
picks a winner and hides the conflict.

### Grounded-empty is not ungrounded

`null` is ambiguous, so grounding is explicit.

* `grounding: "grounded"` with `value: {"type": "empty"}` — the producer reports the
  *empty or default identity*. An unnamed default matrix profile is this. It is a real,
  deliberate selection.
* `grounding: "ungrounded"` with an `ungrounded_reason` — the producer has no reading.

A consumer that conflates them will display a configured default as missing, or write a
default over a value it merely failed to read. Grounded requires a value; ungrounded
requires no value and a reason (`LaneValue::is_consistent`).

### Which lanes are expected to be absent

Absence of a lane is normal and must not be read as breakage. A producer family
legitimately grounds only the lanes it has:

| Producer family | Typically grounded | Typically ungrounded |
|---|---|---|
| HQPlayer, native lane only | `observed`, `effective` | `persisted` (`lane_unconfigured`), `held` |
| HQPlayer with settings interface | `observed`, `persisted`, `held`, `effective` | — |
| Roon | `observed`, `effective` | `persisted` (no configuration lane exists), `held` |
| LMS | `observed`, `persisted`, `effective` | `held` |
| Home Assistant | `observed`, `effective` | `persisted`, `held` |

Consumers must therefore treat `persisted` and `held` as optional and must not disable a
control because they are missing.

---

## 3. Envelope

```json
{
  "schema_version": "1.0",
  "producer": { "producer_id": "hqplayer:living-room", "producer_type": "hqplayer",
                "instance_label": "HQP-Main", "product_version": "6.0.4", "epoch": 7 },
  "target":   { "role": "dsp_engine", "zone_id": "roon:1601bb…" },
  "revisions": { "control_plane": 12, "state": 4193 },
  "lanes":    [ … ], "groups": [ … ], "controls": [ … ], "constraints": [ … ],
  "draft_policy": { … }, "change_sets": [ … ], "operations": [ … ], "stale": false
}
```

One document is **one atomic revision**. A consumer must never combine fields from two
documents: a mode read at state 4191 and a filter enumeration read at 4193 can describe a
combination that never existed.

`producer_id` is stable semantic identity — it survives restarts and address changes and
is never derived from an IP address or socket handle. `product_version` is the backend's
version, informational only, and is not a schema version.

`epoch` increments whenever the producer re-establishes identity (restart, a reconnect
that lost state, a configuration reload). **Revisions are comparable only within one
epoch.**

### Two revisions

| Revision | Bumped when | Rate |
|---|---|---|
| `control_plane` | control identity, choices, ranges, constraints or apply semantics change | slow |
| `state` | observed values change | fast |

They are split because a polling producer bumps `state` continuously. If that also bumped
control identity, every consumer would re-render its catalog on every poll and every open
draft would look stale.

The ordering rules, in full:

1. Neither revision may regress within an epoch. A regressing document is out-of-order
   delivery and must be **dropped**, not merged — merging would resurrect removed controls.
2. A change set is validated against a `RevisionRef` carrying epoch *and* both revisions,
   so a consumer never has to choose which pair to compare.
3. Comparison precedence is total: **epoch change ▸ control plane ▸ state**
   (`Staleness::evaluate`).
4. Anything other than an exact match **requires revalidation before mutation**. A
   control-plane advance additionally means cached descriptors must be discarded, not just
   re-checked.

### Per-lane transport health

`lanes[]` reports each transport independently (`native`, `persistent`, `telemetry`) with
state `connected` / `degraded` / `disconnected` / `unconfigured`, last success, last error
as reason data, and freshness.

A degraded persistence or telemetry lane must not blank a producer whose native lane
works. Last-known values from a failing lane may still be shown, marked stale — but never
presented as current. `unconfigured` is distinct from `disconnected`: nothing is broken.

---

## 4. Controls

### Primitives

`action`, `boolean`, `enumeration`, `numeric_range`, `text`, `group`, `composite`.

`composite` is an atomic value with named members that must be read and written as a
unit — a matrix cell set, a coupled enabled/level pair. Composite member keys are ordered,
so the serialization is canonical and can be compared or hashed.

### Identity

A `ControlId` names a **concept**, not a storage location.

* `hqplayer.pipeline.filter_1x` survives the backend renumbering its filter list.
* An id is **never** a backend index, handle or array position. Those are ephemeral and
  would break persisted bindings on restart (#327).
* An id is never reused for a different concept. Retiring a control means marking it
  `deprecated`, not recycling its id.

### Presentation carries keys, never prose

`label_key`, `description_key`, `Choice::label_key`, `Reason::display_text_key` and
`Divergence::display_text_key` are **catalog keys**. No human-facing sentence is stored in
a producer document. This keeps a wording change from being a contract change and keeps
catalog licensing and provenance governed by #343 rather than smuggled in as unaudited
strings.

`Choice::engine_name` is the exception and the point: an option the catalog has never
heard of stays renderable by the engine's own name, and stays invokable.

`widget_hint` is advisory. A surface may honour, substitute or ignore it. **A presentation
hint can never make an unavailable control mutable or override a constraint.**

### Availability is reason-carrying data

State is `available` / `unavailable` / `read_only` / `deprecated`, and every non-available
state carries at least one `Reason` with a machine-readable `code`, a `scope`
(`observed` / `draft` / `lane` / `producer`) and a catalog key.

Two rules that hold without exception:

* **Unavailability restricts mutation, never visibility.**
  `Availability::permits_display()` is unconditionally true. Hiding a control removes the
  user's ability to see — and often to escape — the state that made it unavailable.
* **`deprecated` still renders and still applies.** A consumer must keep showing it so
  existing layouts do not silently lose controls.

An unrecognized availability state is **not** assumed mutable.

### Enumeration and range authority

`ChoiceSet` and `NumericRange` carry an `authority`, a `source` and the revision they were
read at. **Runtime enumeration wins.** A catalog may add label keys to options the engine
reported; it may never add, remove, reorder or re-permit options, because the engine is
the only thing that knows what it will accept. `ChoiceSet::enrich_from_catalog` implements
exactly that and nothing more, and never overrides a label the producer already supplied.

### Apply semantics

```
lane:        immediate | staged | persistent | matrix_profile | composite
effect:      live_immediate | verified_pending | restart_required
             | persistent_only | held_until_chain_loaded
disruption:  none | audible_glitch | playback_interruption | engine_restart | reconnect
risk:        safe | caution | disruptive | destructive
```

Lanes are independent. **A write to `immediate` must not read, clear or flush staged
intent.**

`invalidates` lists controls whose choices, ranges or observability become invalid once
this is written — a consumer must re-read them rather than trust a cached enumeration.
`coupled_with` lists controls that must be written together as **one server-side composite
plan**. Coupling is the producer's job: a consumer issuing the extra writes itself is how
a level edit silently enables a feature nobody asked for.

### Verification is data

```json
"verification": { "verify_via": "hqplayer.settings.buffer_time",
                  "verify_lane": "persisted",
                  "provisional_ack": true,
                  "preserves": ["hqplayer.volume.fixed.level"] }
```

* `verify_via` + `verify_lane` name the authoritative field to read back, so a producer
  implementation drives readback from the descriptor instead of from setter-specific UI
  logic. That is how a verifier ends up checking a different field than the write touched.
* `provisional_ack: true` means the transport's acknowledgement proves nothing. A dropped
  connection after such a write is `indeterminate`, not a failure.
* `preserves` lists sibling state that must be asserted **unchanged**. A verifier that
  only checks the field it set will report success after destroying a retained value it
  never looked at.

---

## 5. Draft-dependent constraints

A control can be perfectly available on the running engine while a staged sibling would
make the proposed combination invalid or inert. Expressing that means shipping
*conditions* to consumers — and the one thing v1 will not ship is producer code. A
serialized predicate function is arbitrary logic executing inside a browser, an ESP32 and
an MCP client, with no cost bound and no way to audit it.

So constraints are **bounded pure data**: operators `eq`, `not_eq`, `one_of`, `in_range`,
`is_grounded`, `all`, `any`, `not`; maximum depth 8; maximum 128 nodes. A producer needing
deeper logic must reproject the change set server-side instead.

Effects: `invalidates`, `makes_inert`, `requires_restart`, `hides_choice`. `hides_choice`
hides *a choice*, never the control.

### Three-valued evaluation

`Evaluation` is `True`, `False`, or `Unevaluatable { reason }`. The third value is the
point: "I could not decide" differs from "no", and collapsing them is what lets a stale
consumer authorise an invalid combination. Kleene rules apply — `all` is `False` if any
child is `False`; only then does an undecidable child make it undecidable. A definite
answer from one child is enough.

An ungrounded operand yields `Unevaluatable`, never `False`.

### Degradation: visibility fails open, permission fails closed

Unknown operators are *expected*, not exceptional — a v1.0 consumer will meet v1.4
operators.

| | Rule | Why |
|---|---|---|
| **Visibility** | fails **open** — the control and its observed value are always shown, annotated with a reason | hiding the control the user needs to escape the state is worse than showing an unconstrained one |
| **Permission** | fails **closed** — `RequiresProducerValidation`, never "satisfied" | a v1.0 consumer must not authorise a combination a v1.1 producer marked invalid |

These are not in tension: one governs what the user can **see**, the other what the system
will **accept**. `Permission::asserts_satisfied()` is false for
`RequiresProducerValidation`, and the producer re-validates server-side regardless of what
any consumer concluded.

Only `invalidates` denies. An inert or restart-requiring combination is a legitimate thing
for a user to choose deliberately.

### One evaluation path

Every surface evaluates through `ProducerDocument::effective_view(change_set)`. The
effective lane resolves to staged intent when the draft has it, otherwise the producer's
own effective lane, otherwise observed. Because there is one implementation, a web page,
an MCP client and a device cannot reach different verdicts about the same draft.

---

## 6. Change sets: multi-surface staged intent

Staged intent is server-side state several surfaces can see at once. That makes it
**concurrency control**, not UI state.

### The race this design forecloses

The audit on #323 records the concrete failure: an apply passed the live pending
dictionaries straight into an async operation, and the completion handler cleared the
whole store. Edits staged by another surface *during* the in-flight apply were deleted by
a completion that had never seen them. A single-threaded event loop does not help — tasks
interleave at every await.

### Generations

Every change set has a stable `id`, an `origin` (`actor_class` × `surface` × optional
`actor_id`), a `base` `RevisionRef`, timestamps, a lifecycle `state`, and a monotonic
`generation`.

* **`detach()`** takes an immutable snapshot at the current generation. Preflight and
  execution operate on the snapshot, never the live draft — that is what makes "the
  operation applied what it displayed" true.
* **`stage()`** during execution produces a **successor generation** and reopens the draft.
* **`retire(generation)`** succeeds only for the exact generation detached. Otherwise it
  returns `GenerationSuperseded` and **clears nothing**. Later intent belongs to whoever
  staged it.

Retry after a partial application is therefore a **new plan over a changed producer
revision**, not a replay.

### Addressing and ownership

Apply, discard, retry and inspect all target a **named** change set. There is no "the
pending changes" to address, which is what prevents a web page, an MCP client or a device
from flushing a draft it does not own.

`draft_policy.mode` declares `shared` or `actor_scoped`. A shared draft must expose every
`contributor` and every conflict — a user who cannot see another surface's edit will apply
it without knowing. `on_conflict` declares `expose_contributors`,
`merge_non_overlapping` or `last_actor_takeover`.

### Entry validity

`valid`, `stale_base` (carrying `observed_now`, so the user sees the conflict),
`conflicts`, `draft_invalid`, `requires_producer_validation`. If observed or persisted
state moved after the base revision, the whole change set is revalidated and stale or
conflicting fields are reported **before** mutation. Silent rebasing is never permitted:
the user staged a value against what they were shown.

### Retention

`survives_reconnect`, `survives_producer_restart`, `on_epoch_change`
(`expire` / `retain` / `revalidate`), `expires_at`, `apply_requires_authorization`. These
are contract semantics, not frontend behaviour: a draft that evaporates on reconnect, or
lingers forever holding a resource, is a producer property every surface needs to know.

A producer-epoch change **must** publish its held-intent transition to every surface.
Held intent that proves invalid terminates visibly as `rejected` or `expired` with a
reason and a correlation. It is never silently forgotten.

---

## 7. Command outcomes

The governing rule: **a consumer never infers observed state from whether a call returned
or threw.**

`write_attempt` (`not_attempted` / `attempted` / `acknowledged_provisional` /
`confirmed`) is tracked separately from `outcome`, because "we sent it" and "it took
effect" are different facts.

Fifteen outcomes, none reducible to success/failure without losing what a consumer must do
next:

| Outcome | Meaning |
|---|---|
| `applied` | the requested effect is observed |
| `ignored` | accepted, nothing changed (already that value, or inert) |
| `rejected` | refused, with a reason |
| `superseded` | a newer operation or generation replaced it |
| `disconnected` | the transport was down before the write was attempted |
| `timed_out` | no response within budget |
| `held` | stored for a chain or feature that is not loaded |
| `restart_required` | stored, inert until restart |
| **`indeterminate`** | **written, transport dropped — possibly applied** |
| `partially_applied` | some plan steps applied, some did not |
| `compensating` / `compensated` | compensation running / complete |
| `recovery_required` | cannot compensate automatically |
| `expired` | held intent's retention elapsed or it proved invalid |
| `divergent` | readback matched neither the request nor the prior value |

`indeterminate`, `timed_out` and `compensating` are **not state evidence**
(`is_state_evidence()` is false): they describe what happened to the *operation*, not what
is true of the producer.

### The audit trail

`history` is append-only. Reconnect and readback legitimately move an operation from
`indeterminate` to `applied`, `superseded`, `rejected` or `divergent`. The reverse is
forbidden: once an outcome is authoritative it may not decay back into a maybe, or a later
failed poll would erase a known fact. A new operation cannot erase the trail — it is the
only record distinguishing "was never applied" from "was applied, then replaced".

### Multi-revision plans

A plan whose later steps need capabilities its earlier steps create cannot claim
single-revision atomic preflight. Such a plan declares `revision_boundaries`, and every
`PlanStep` carries the revision it observed. `recovery` then states what is needed:
`awaiting_readback`, `awaiting_reconnect`, `compensating`, `replan_required`,
`manual_intervention_required`.

---

## 8. Compatibility policy

### Wire documents

`schema_version` is `major.minor`. Trailing numeric components are tolerated (`"1.4.2"` is
major 1, minor 4); anything else is refused.

| Document vs consumer | Decision |
|---|---|
| same major, same or older minor | **Supported** |
| same major, **newer** minor | **SupportedWithUnknownAdditions** — render it |
| any other major | **Refused** (`UnsupportedMajor`) |
| unparsable `schema_version` | **Refused** (`UnparsableVersion`) |
| missing `schema_version` | **Refused** (`MissingVersion`) |

`admit_document` inspects the version on the **raw value before parsing**. Deserializing
first would make a v2 document surface as a confusing schema error, or partially succeed —
and a consumer could not tell an unsupported generation from corruption.

**A refused document is never partially rendered.** A control plane built from
half-understood data is worse than an explicit "unsupported" message, because the user
cannot tell which controls are missing.

### What "additive" means

Within major 1, a minor release may add fields, vocabulary members and controls. It may
not remove or repurpose them, and it may not change the meaning of an existing field.

* **Unknown fields are ignorable.** They never fail a parse.
* **Unknown fields are *preserved* at container level** — document, producer identity,
  target, lane health, control, lane value, change set, entry, operation — so a
  pass-through projection does not amputate a newer document. Below container level they
  are ignorable but not preserved.
* **Unknown vocabulary members** deserialize to `Unrecognized(String)`, keep their original
  spelling, and round-trip unchanged. They still force consumers to handle the case,
  because the arm exists in the enum.
* **Unknown control kinds** render as much as the consumer can and never hide the observed
  value.
* **Unknown constraint operators** follow §5: visible, not permitted.
* **Deprecated controls** keep rendering and keep applying.
* **Semantic ids are stable forever** and are never backend indices.

### Stored artifacts

Persisted semantic artifacts (drafts, bindings, layouts) carry **their own** schema
version, deliberately independent of the application's release version, so upgrading the
binary does not implicitly migrate data.

| Stamp | Decision |
|---|---|
| same major, same or older minor | `Readable` |
| same major, newer minor | `ReadableWithUnknownAdditions` |
| other major | `Refused` — never migrated silently |
| **absent** (legacy) | `UnstampedAdoptOnWrite` — readable, and stamped on the **next write** |

Adoption on write, not on read: reading a file never rewrites it.

### Fixture stability

`tests/fixtures/adaptive/*.json` are part of the contract for sibling issues. Their paths
are **additive-only**: a fixture may gain fields and new fixtures may be added, but
renaming or deleting one breaks another issue's tests. Canonical (1.0) fixtures must
round-trip exactly and must contain no unrecognized fields — a fixture claiming to
demonstrate held intent while actually demonstrating `None` is worse than no fixture.

### Known gap: no machine-readable schema for non-Rust consumers

The Rust model is the normative source. Firmware (C) and iOS (Swift) consumers cannot read
it, and until a JSON Schema is generated they must transcribe it by hand from the
canonical fixtures — the duplication #312 exists to remove. This is a **deliberate
deferral**, not an oversight: the schema belongs to the first issue that needs it, most
likely #314 (device capability and protocol-version negotiation) or #326. Until then the
fixtures are the reference, which is why their exactness is tested.

---

## 9. Canonical examples

| Fixture | Demonstrates |
|---|---|
| `hqplayer_pipeline_v1.json` | transport actions; dB volume with a verified-pending provisional write; mode enumeration; runtime-authoritative filter list including an option no catalog knows; adaptive output that invalidates exact rate; exact rate unavailable in source-following mode with two machine-readable reasons; restart-required persistent buffer setting; composite fixed volume with a retained dormant level and a `preserves` assertion; chain-scoped control with held dormant intent; grounded-empty matrix profile; all four constraint effects and all eight operators |
| `hqplayer_degraded_lanes.json` | degraded persistence and disconnected telemetry while native control stays fully usable; ungrounded `persisted` with `requires_connection` and `lane_unconfigured`; a setting that becomes `read_only` with a lane-scoped reason |
| `staged_intent_multi_surface.json` | ten change-set lifecycle states across web, MCP, device, HTTP and internal surfaces; human, automation, device and agent actors; a stale base reporting `observed_now`; a conflicting entry; a draft-invalid entry; an entry needing producer validation; retention and expiry; an orphan draft from an earlier epoch |
| `command_outcomes.json` | every outcome, write-attempt state and recovery state; an indeterminate write awaiting readback; a multi-revision plan with declared boundaries and per-step revisions; held intent expiring visibly |
| `forward_compatible_additions.json` | a 1.7 document a 1.0 consumer must render: unknown target role, control kind, value type, availability state, reason code, transport lane, and constraint operators nested inside `all`; unknown fields preserved at container level |
| `unsupported_major.json` | a 2.0 document, shaped nothing like v1, that must be refused before any parsing |

---

## 10. Consumer checklist

A consumer conforms to v1 if it:

1. Calls `admit_document`, surfaces refusals, and never partially renders a refused
   document.
2. Reads only `observed` as evidence of audible reality, and shows divergence rather than
   resolving it.
3. Distinguishes grounded-empty from ungrounded.
4. Never hides a control — including unavailable, deprecated, unknown-kind and
   unknown-constraint cases.
5. Explains unavailability from reason codes, resolving text through the catalog.
6. Evaluates constraints only through `effective_view`, and treats
   `RequiresProducerValidation` as "ask the producer", never as "allowed".
7. Addresses every mutation to a named change set and generation, and never flushes a
   draft it does not own.
8. Treats `indeterminate` as possibly-applied, and never infers state from a call
   returning or throwing.
9. Revalidates whenever staleness is anything but `Current`, and never silently rebases.
10. Drops regressing documents rather than merging them.
11. Preserves unknown fields when projecting a document onward.
12. Persists nothing keyed by a backend index.
