# ADR 003: Adaptive-control producer document v1

## Status

Accepted (specification only — no producer publishes it yet)

**Date:** 2026-07-30
**Issue:** [#323](https://github.com/open-horizon-labs/unified-hifi-control/issues/323)
**Epic:** #312 · **Program:** #353
**Specification:** [docs/architecture/adaptive-producer-contract-v1.md](../architecture/adaptive-producer-contract-v1.md)

## Context

UHC needs one versioned description of a producer's controllable state so that web, MCP,
device and HTTP surfaces stop each encoding backend knowledge. Today those facts are
duplicated across `src/adapters/hqplayer.rs`, the web UI, MCP tool descriptions and
firmware-facing manifests.

This is a one-way door. Once #324 publishes documents on the bus, #326 matches layouts
against them and #327 persists bindings to their semantic ids, the shape is expensive to
change. It is cheap to change now.

Two things made the decision harder than "pick a JSON shape":

1. **A control has more than one legitimate value.** Direct audits recorded on #323 found
   that observed runtime state, persisted configuration, staged intent, intent held for an
   unloaded chain, and the editor's effective projection are all real, and that a model
   with one scalar per setting destroys data. The worked case: a disabled fixed-volume
   feature retains its level in a commented configuration element, and a verifier that
   re-reads only the field it wrote reports success after deleting it.
2. **Staged intent is concurrency control, not UI state.** An audit found a lost-update
   race where an apply passed live pending dictionaries into an async operation and the
   completion cleared the whole store, deleting edits another surface staged mid-flight.

## Decision

Define v1 as a **Rust domain model that is normative**, with serde defining the wire form,
and hand-authored canonical JSON fixtures plus a compatibility engine acting as the
contract tests.

Load-bearing choices, each with the alternative it displaced:

| Decision | Alternative rejected | Why |
|---|---|---|
| Five value lanes with explicit grounding and provenance | one `value` per control | one scalar cannot express a retained dormant value, and writing it destroys one |
| Divergence is published data | producer reconciles and publishes a winner | hiding the conflict makes it undiagnosable from any surface |
| `grounded + empty` distinct from `ungrounded` | `null` for both | a default selection would read as unreadable, and a write-back would overwrite it |
| Two revisions (`control_plane`, `state`) | one monotonic revision | a polling producer would churn control identity and make every draft look stale |
| `Staleness` precedence epoch ▸ control plane ▸ state | consumers compare revisions | with two counters, a consumer comparing the wrong pair acts on a stale catalog |
| Constraints as bounded pure data (8 deep, 128 nodes) | serialized predicate functions | shipping producer code to a browser, an ESP32 and an MCP client is unauditable and unbounded |
| Visibility fails open, permission fails closed | one degradation rule | fail-open is right for what the user sees and wrong for what the system accepts |
| Change-set generations; retire only what you detached | clear pending on success | reproduces the recorded lost-update race |
| Fifteen command outcomes incl. `indeterminate` | success/failure | a dropped transport after a write is possibly-applied, which is neither |
| Append-only outcome history | current outcome only | otherwise "never applied" and "applied then replaced" are indistinguishable |
| Vocabularies as enums with an `Unrecognized` arm | plain string enums, or open strings | exhaustive `match` *and* forward compatibility; open strings lose the compiler's help |
| Presentation keys, never prose | inline labels and help text | keeps a wording change from being a contract change, and keeps catalog licensing under #343 |
| Shared module, `serde`/`serde_json`/`std` only | `#[cfg(feature = "server")]` | WASM and MCP consumers need the same types; #312 exists to remove duplication |

## Options considered

**A — Promote HQPlayer's existing `PipelineStatus` JSON to v1.** Fastest. Rejected:
`SelectOption.value` carries backend list indices, which the epic forbids as durable
identity, and its one-scalar-per-setting shape is exactly the model the audits ruled out.

**B — JSON Schema as the normative artifact, `serde_json::Value` in Rust.** Good for
non-Rust consumers. Rejected: the risky parts of this contract are closed vocabularies
(outcomes, apply lanes, availability reasons, constraint operators) where `match`
exhaustiveness is free enforcement in a crate that already denies `unwrap`, `expect` and
`panic`. Schema and code would drift by default.

**C — Rust model normative, fixtures as contract tests. (Selected.)** Compatibility becomes
executable: unsupported majors, unknown minors, unknown kinds, unknown operators and
unknown additive fields each have a test pinning the required behaviour.

**D — Extract a standalone `uhc-adaptive-contract` crate.** The right end state, and
deferred rather than rejected. Converting this single-package repository into a workspace
touches `Cargo.toml`, `build.rs`, `Dioxus.toml`, the Dockerfiles and CI, and puts the
`dx build` fullstack path at risk during a contract-definition issue. Kept mechanical by
`tests/adaptive_dependency_lint.rs`, which forbids transport and state dependencies in
`src/adaptive/`.

**E — Trait/RPC-first typed action registry.** Rejected: an ESP32, an MCP client and a
browser cannot consume a Rust trait. A document would be invented anyway, derived from
adapter code — re-coupling surfaces to backends, which is what the epic removes.

## Consequences

### Accepted

* **Size, stated plainly.** `src/adaptive/` is 3262 lines, of which 2022 are non-blank
  non-comment — so about 38% is documentation of *why*. Tests add 2034 lines and fixtures
  1960. For scale, `src/adapters/hqplayer.rs` is 2622 lines for one backend's live protocol.

  The acceptance criteria mandate the *concepts*; this implementation chose the fullest
  representation of each, and that was a judgment call rather than a forced move.
  `constraint.rs` is the clearest case: at 625 lines it is a fifth of the module, and a
  three-operator vocabulary with the same degradation rules would have satisfied every
  criterion in roughly 250. The extra operators exist because they were cheap to add while
  the serializer was being written, which is a reason to write code, not to ship it.

  `every_vocabulary_member_has_a_worked_example` proves representability, not necessity: it
  can always be satisfied by extending a fixture, so it bounds *reachability*, not size.

* **#325 is the deletion gate.** When HQPlayer's mapping lands, any construct it cannot
  populate from a real engine is a candidate for removal in v1.1 rather than permanent
  surface every future producer must still handle. Removal within major 1 is otherwise
  forbidden by the compatibility policy, so this is an explicit, time-boxed exception: it
  applies only to constructs no producer has ever published, and it closes when #324 ships.
  Current best guess at the exposed set: the `compensating` / `compensated` /
  `recovery_required` / `divergent` outcomes, multi-revision plan boundaries, the
  `in_range` / `is_grounded` / `not_eq` / `any` operators, and `held` values on producers
  with no settings interface.
* Non-Rust consumers get canonical fixtures and normative prose now; a generated JSON
  Schema is owed to the first issue that needs it (#314 or #326).
* Consumers must handle an `Unrecognized` arm for every vocabulary. That is the cost of
  additive evolution being real rather than promised.
* Two revisions are more for a consumer to track than one. Mitigated by `Staleness`
  computing precedence so no consumer compares revisions itself.

### Risks

* **Generality is inferred from one backend's failure modes.** Every lane and outcome
  traces to an HQPlayer/HQPTuner audit. Roon has no persisted-configuration lane at all;
  Home Assistant has availability but no restart-required class. The model may be
  simultaneously over-specified for HQPlayer and under-specified for the next producer.
  Falsifiable only by #325. Mitigated by documenting which lanes are expected to be
  ungrounded per producer family, so absence is not read as breakage.
* **Adoption.** The web UI already reads `/hqp/pipeline` directly and it works. If #324
  exposes the document additively without a migration plan, no surface moves and the
  document is written but never read. Owned by #324; #323 cannot decide it, because
  deprecating a route needs explicit API approval.
* **Additive-only evolution may not survive the first real producer.** Mitigated by making
  the refusal path a tested, day-one path rather than a promise.

### Not consequences

No public API surface changes. No routes added, removed or modified;
`tests/fixtures/api_routes.txt` is unchanged and `api-change-approved` was neither
requested nor applied. Nothing is wired into a route, adapter or the aggregator, so a
deployed binary's runtime behaviour is unchanged and rollback is a plain revert with no
data migration.

## Notes

Generated from `/solution-space` → `/review` → `/dissent` on 2026-07-30. Gate reports are
recorded on PR #362. Session: `.oh/adaptive-producer-contract.md`.
