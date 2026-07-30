# Adaptive Producer Contract v1 (UHC #323)

Session for GitHub issue #323 — *Adaptive control: specify producer document v1 and
compatibility policy*. Parent epic #312. Program session:
`.oh/adaptive-interaction-plane.md` (main checkout, read-only for this session).

## Aim

A generic consumer with no HQPlayer-specific code can read one versioned document and
know: what controls exist, what each one currently *is* (and by whose authority), what
may be changed, what changing it costs, and what happened when it was changed — while
the aggregator remains the only owner of authoritative state.

Out of scope for #323: bus/aggregator publication (#324), HQPlayer mapping (#325),
matcher/manifest composition (#326), persisted bindings (#327), catalog provenance
(#343). No new or changed HTTP routes; `tests/fixtures/api_routes.txt` untouched.

## Solution Space

**Updated:** 2026-07-30

**Problem:** Generic consumers need a stable, versioned description of producer state and
safe operations, but no contract unifies identity, values, choices, ranges, availability,
apply behaviour, pending/error state and revision semantics across integrations.

**Key Constraint:** Stable semantic IDs with no raw backend indices as durable identity;
explicit schema version + document revision; additive evolution with ignorable unknown
fields; unsupported majors fail safely; aggregator keeps ownership of state; public API
routes are frozen without explicit approval.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|--------|-------|----------|-----------|
| A | Band-Aid | Declare the existing HQPlayer `PipelineStatus` JSON to be v1 and bolt on fields | Ships fastest; bakes HQPlayer indices and vocabulary into the contract |
| B | Local Optimum | Hand-authored JSON Schema as the normative artifact; Rust handles `serde_json::Value` | Language-neutral for firmware; no compile-time exhaustiveness, new runtime dep, doc/code drift |
| C | Reframe | Rust domain model is normative; serde defines the wire form; canonical JSON fixtures + a compatibility engine are the contract tests | More types up front; JSON Schema for non-Rust consumers is derived later |
| D | Redesign | Extract a standalone `uhc-adaptive-contract` workspace crate | Correct end state; converting a single-package repo to a workspace is out of #323 scope |
| E | Reframe | Trait/RPC-first typed action registry instead of a document | Devices, MCP and HTTP need data across a wire; traits cannot cross that boundary |

### Evaluation

**Option A: promote `PipelineStatus` to v1**
- Solves stated problem: No. It is a projection of one backend's web UI.
- Implementation cost: Low.
- Maintenance burden: High.
- Second-order effects: `SelectOption.value` in `src/adapters/hqplayer.rs:240` carries
  backend list indices, which the epic explicitly forbids as durable IDs. It has one
  scalar per setting, so observed / persisted / staged / held / editor-effective values
  collapse into a single overloaded field — exactly the failure the HQPTuner audit on
  #323 documented. Every later producer inherits HQPlayer's grammar.

**Option B: JSON Schema as normative artifact**
- Solves stated problem: Partially.
- Implementation cost: Medium.
- Maintenance burden: Medium-high.
- Second-order effects: Good for a future firmware/Swift consumer, but the interesting
  parts of this contract are closed vocabularies (apply lanes, command outcomes,
  availability reasons, constraint operators). Rust `match` exhaustiveness is the cheapest
  possible enforcement of "you handled every outcome"; `Value` access throws that away in
  a crate that denies `unwrap`/`expect`/`panic`. The schema and the code drift by default.

**Option C: Rust model normative, fixtures as contract tests**
- Solves stated problem: Yes.
- Implementation cost: Medium-high.
- Maintenance burden: Low-medium.
- Second-order effects: Compatibility becomes executable — an unsupported major, an
  unknown control kind, an unknown constraint operator and an unknown additive field each
  get a test that pins the required behaviour. Canonical examples become fixtures rather
  than prose, so #325's HQPlayer mapping has something to assert against. Cost: a second
  representation (JSON Schema) is owed to non-Rust consumers eventually.

**Option D: separate contract crate**
- Solves stated problem: Yes, and better for reuse.
- Implementation cost: High *here*.
- Maintenance burden: Low.
- Second-order effects: Touches `Cargo.toml`, `build.rs`, `Dioxus.toml`, `Dockerfile*`
  and CI, and puts the `dx build` fullstack path at risk for a contract-definition issue.
  Deferred, not rejected.

**Option E: trait/RPC-first**
- Solves stated problem: No.
- Implementation cost: Medium.
- Maintenance burden: High.
- Second-order effects: An ESP32 surface, an MCP tool and a browser cannot consume a Rust
  trait. A document would have to be invented anyway, but now derived from adapter code —
  re-coupling surfaces to backends, which is what the epic exists to remove.

### Recommendation

**Selected:** Option C — Rust domain model normative, JSON fixtures as contract tests
**Level:** Reframe

**Rationale:** The binding constraint is not "which JSON shape" but "which truths are
representable". Option C makes the closed vocabularies compiler-enforced, makes the
compatibility policy executable rather than aspirational, and keeps the contract layer
free of adapter/transport dependencies so Option D remains a mechanical extraction when a
second consumer crate actually exists.

**Accepted trade-offs:**
- Non-Rust consumers get canonical JSON fixtures and normative prose now; a generated
  JSON Schema is deferred to the issue that first needs it.
- More types than a minimal HQPlayer payload would need. Justified by the four distinct
  value lanes, eight command outcomes and per-lane health the audits on #323 require.
- The model is *shared* (not `#[cfg(feature = "server")]`) so web/WASM consumers can use
  it. That constrains `src/adaptive/` to serde + std only, enforced by a lint test.

**Rejected for #323, recorded for later:**
- Option D — extraction into `uhc-adaptive-contract`. Kept cheap by forbidding
  adapter/aggregator/axum/dioxus/reqwest references from `src/adaptive/`.

### Implementation Notes

1. `src/adaptive/` — shared module, serde + std only:
   `version` (schema version + compatibility engine), `document` (envelope, identity,
   dual revisions, lane health), `control` (descriptors, kinds, availability, apply
   semantics), `value` (five value lanes + grounding + provenance + divergence),
   `constraint` (bounded pure-data expression vocabulary), `command` (change sets,
   operations, outcomes, correlation).
2. Two revisions, not one: a slow `control_plane` revision for catalog/schema identity and
   a fast `state` revision for observed values, so polling does not churn control identity.
3. Grounding is explicit: `Grounded(value)` vs `Ungrounded(reason)` so an empty/default
   preset identity cannot collapse into `null`.
4. Availability and disablement are reason-carrying data, never a bare boolean.
5. Command outcomes are a closed set of eight terminal/non-terminal states including
   `Indeterminate` (write attempted, transport dropped, possibly applied).
6. Constraints are pure data with a bounded operator vocabulary; unknown operators fail
   *open* for visibility (value still shown, control still escapable) and never hide state.
7. Contract tests live in `tests/adaptive_contract.rs` and assert against canonical
   fixtures in `tests/fixtures/adaptive/`. Fixtures are the examples #325 maps onto.
8. Architecture lint test keeps `src/adaptive/` transport-free and crate-extractable.
9. No route changes. `tests/fixtures/api_routes.txt` is not touched and
   `api-change-approved` is never applied.

## Execute
**Updated:** 2026-07-30
**Status:** complete

Landed in three commits plus a cleanup commit:

- `43d674b` domain model (`src/adaptive/`, 8 modules, shared not server-gated)
- `67e83bb` untrack `.oh/.cache` artifacts a `git add -A` had swept in
- `9449152` six canonical fixtures, 79 contract tests, dependency lint
- docs: normative spec, ADR 003, ARCHITECTURE.md pointer

All five solution-review actions and all five solution-dissent actions landed as tests or
normative spec text rather than prose:

| Action | Where |
|---|---|
| serde/std-only dependency rule, host-runnable | `tests/adaptive_dependency_lint.rs` (verified non-vacuous) |
| JSON Schema debt recorded | spec section 8, ADR consequences, PR body |
| canonical fixtures round-trip exactly | `every_canonical_fixture_round_trips_exactly` |
| no stray fields in our own fixtures | `canonical_fixtures_have_no_stray_fields` |
| every vocabulary member has a worked example | `every_vocabulary_member_has_a_worked_example` |
| two-revision ordering rules + out-of-order test | spec section 3, `mod revisions` |
| visibility open / permission closed, both tested | spec section 5, `mod constraints` |
| change-set lifecycle first-class in v1 | `src/adaptive/change_set.rs`, `mod change_sets` |
| fixture stability policy | spec section 8 |
| expected-ungrounded lanes per producer family | spec section 2 |

**Drift check:** none. No routes touched, `tests/fixtures/api_routes.txt` unchanged,
`api-change-approved` never applied, no user-owned file modified. 390 tests pass.

**Verification gap:** `dx` is not installed in this environment, so the WASM/fullstack
build is proved only by CI's `build-wasm` job (`.github/workflows/build.yml:488`). The
dependency lint is the host-runnable proxy.

## Ship
**Updated:** 2026-07-30
**Status:** staged (draft PR, nothing merged or deployed)

**Delivery path:** draft PR #362 (base `v3`) -> CodeRabbit + human review -> maintainer
approval -> squash merge to `v3` -> `build.yml` on push. Release artifacts only on tag `v*`
or a published release; `docker.yml` is `master`-only, so a `v3` merge publishes no image.

**CI at `a5cae03`** (run 30571330314): Test, Build WASM Assets, Build Linux x64 and Smoke
Test Binary all pass. Lint fails on `src/app/pages/zones.rs:269`
(`clippy::unnecessary_sort_by`) - pre-existing on `v3`, untouched by this PR, fires only on
CI's newer toolchain. Blocks merge; the one-line fix belongs in a separate PR.

**Delivery-path tax:** one red job outside this PR's control. Everything else is green.

**Rollback:** plain revert, no data migration - true only while this is the tip. After #324
publishes and #327 persists bindings, reverting breaks them and the additive-only policy
forbids removal within major 1.

**Verified rather than asserted:** nothing outside `src/adaptive/` references it except
`pub mod adaptive;`; `src/adaptive/` imports nothing from the crate; `api_routes.txt` is
byte-identical to `v3`; `api-guard` does not trigger.

## Review
**Updated:** 2026-07-30
**Verdict:** ALIGNED

Six gate reports on PR #362. Two required in-place correction after later gates caught their
errors - report 3 filed a contract defect as a downstream hazard, report 4 recommended a
v1.1 deletion gate that contradicted the compatibility policy in the same PR. Both fixed at
`a5cae03`. Durable record is ADR 003 plus the specification, not the PR comments.

Raised on #324: C1/C2 publication-time enforcement, and ownership of the pre-publication
reshaping window.
