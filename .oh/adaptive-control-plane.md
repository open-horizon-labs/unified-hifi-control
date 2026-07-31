# Adaptive control plane: one producer model, surface-appropriate projections (#331)

**OH:** 80222d6d · **Issue:** [#331](https://github.com/open-horizon-labs/unified-hifi-control/issues/331) ·
**Parent epic:** #313 · **Base branch:** `feat/issue-328-direct-hqplayer-zone` at `212f555`

### Inputs read before choosing an approach

* [`.oh/adaptive-interaction-plane.md`](../../unified-hifi-control/.oh/adaptive-interaction-plane.md)
  — the **program session** for epic #313. It is not tracked on this branch; it lives in the main
  checkout and `.oh/adaptive-producer-contract.md:5` names it "read-only for this session". Read
  there. Its Option D (unified adaptive interaction plane) is the direction this issue implements,
  and its implementation note 4 is the sentence #331 is judged against: *"Project all advertised
  capabilities into web, HTTP, device UI, and MCP through one semantic execution path;
  surface-specific subsets are allowed, contradictory semantics are not."*
* [`.oh/adaptive-producer-contract.md`](adaptive-producer-contract.md) (#323) — the v1 document,
  five value lanes, fifteen-plus command outcomes, reason-carrying availability, two revisions.
* [`.oh/adaptive-publication.md`](adaptive-publication.md) (#324) — the admission gate, the
  actor-owned aggregator, `AdaptiveView`, and **the boundary lint that governs this issue's shape**
  (`tests/adaptive_publication_lint.rs`, boundary 2: no file under `src/` other than
  `src/adaptive/`, `src/producers/`, `src/lib.rs` and `src/main.rs` may name `crate::adaptive` or
  `crate::producers`).
* [`.oh/issue-375-hqplayer-adaptive-producer.md`](issue-375-hqplayer-adaptive-producer.md) (#375) —
  the first truthful producer, and its explicit hand-off: *"The generic command preflight, typed
  attempt receipts, and public consumer dispatch remain in #331. That issue must consume the
  registry added here rather than create a second control-to-operation mapping."*
* [`.oh/issue-329-hqplayer-immediate-command.md`](issue-329-hqplayer-immediate-command.md) (#329) —
  `HqpImmediateCommandActor`, whose `Submit` variant carries
  `// The first public consumer lands in #331`.
* [`.oh/hqplayer-direct-zone.md`](hqplayer-direct-zone.md) (#328) — the immediate base, and the
  source of the "no unknown HQPlayer identity falls through to Roon" defect class.
* `AGENTS.md`, `docs/ARCHITECTURE.md`, `tests/architecture_lint.rs`, `tests/api_contract.rs`,
  `tests/fixtures/adaptive/*.json`, and the five #331 issue comments recording the HQPTuner audits.

---

## Aim

A control that exists once in the producer document appears — or is honestly withheld — the same
way everywhere. The web page, an MCP client and a knob read the *same* advertised id, the same
option set, the same constraint, the same reason for unavailability, and the same apply cost; when
one of them acts, all three follow one correlation id to one observed result.

The behaviour change to look for: adding an HQPlayer control stops meaning "and now write it three
more times", and a user stops discovering that the web page offers something the MCP tool denies.

Not the aim: an identical UI on every device, a producer that knows what a knob looks like, or
parity manufactured where the engine has none.

---

## Problem Statement

**Reframed (from the issue):** users need consistent capabilities and outcomes across surfaces, but
bespoke integrations cause each surface to advertise different choices, routing, constraints and
success semantics.

**The shift:** from feature parity by duplicated code to *surface-appropriate projections of one
aggregator-owned producer document and one command execution path*.

### What the code actually does today, read before choosing

| # | Fact | Location |
|---|------|----------|
| 1 | MCP's HQPlayer write tool calls the **adapter** directly — `self.state.hqplayer.set_mode(...)` — bypassing the aggregator, the admitted document, preflight, correlation and outcome recording entirely. | `src/mcp/mod.rs:600-611` |
| 2 | The web HQPlayer page fetches `/hqp/*` JSON and renders hand-wired dropdowns whose option lists come from the legacy `PipelineStatus` projection, not from a producer document. | `src/app/pages/hqplayer.rs`, `src/app/components/hqp_controls.rs` |
| 3 | There is **no device manifest surface at all** on this branch. #326 (matcher-to-manifest composition) is unstarted, so the ESP32 criterion has no composition layer to project into. | absent |
| 4 | `AdaptiveView` — the only legal read path — is *moved* into `HqpImmediateCommandActor::build` at the composition root and is held by nothing else. | `src/main.rs:221-240` |
| 5 | `HqpImmediateCommandHandle::submit` and `HqpCommandServiceRefusal::ServiceClosed` are `#[cfg_attr(not(test), allow(dead_code))]`, waiting for this issue. | `src/producers/hqplayer_command_service.rs:47,148,166` |
| 6 | `hqp_semantic_operation_registry()` is the single 11-entry control-id → semantic-operation bijection, and `preflight()` is a complete pure admission check over a snapshot. | `src/producers/hqplayer.rs:100-160`, `src/producers/hqplayer_command.rs` |

### The constraint that decides this issue's shape

`tests/adaptive_publication_lint.rs::lint_only_the_composition_root_names_the_contract_or_the_publication_layer`
sweeps **all** of `src/` and forbids every file outside `src/adaptive/`, `src/producers/`,
`src/lib.rs` and `src/main.rs` from naming `crate::adaptive` or `crate::producers`. That lint is
#324's mechanical form of the API freeze: `ProducerDocument` derives `Serialize`, so one
`Json(snapshot)` in `src/api/` re-exports the whole v1 contract outside this repository.

The consequence is not a detail, it is the boundary of this issue:

* `src/api/`, `src/mcp/`, `src/app/`, `src/knobs/`, `src/mqtt/` **cannot legally see a projection**
  today.
* The Dioxus web client is SSR + WASM and reads its data over HTTP (`src/app/api.rs:306`), so even
  ignoring the lint, a producer-driven web page needs a route.
* Therefore **every surface hop in #331's acceptance criteria is gated behind exactly one API
  approval**, and the issue's `protocol:additive` label is not that approval.

What remains, and is delivered here, is the whole of the part that does not need it: the one
projection model, the one command execution path, the routing rule, and the contract tests that
falsify drift between the projections the surfaces will consume.

### Constraints treated as real

* **Hard.** No new/changed HTTP route, request or response schema; no MCP tool or schema change;
  `tests/fixtures/api_routes.txt` untouched; `api-change-approved` never self-applied.
* **Hard.** All command execution stays aggregator/application-service owned. No surface reaches an
  adapter. No unknown HQPlayer identity may fall through to Roon or any other adapter.
* **Hard.** Clients use semantic ids and values; no adapter internals, no native indices, no
  backend prose.
* **Hard.** One control-to-operation mapping — #375's registry — never a second.
* **Hard.** Hermetic fixtures only. The live HQPlayer host is not contacted.
* **Soft.** MCP tool granularity, web grouping, which device screens ship first, exact fallback UX.

---

## Solution Space

**Updated:** 2026-07-31

**Problem:** three surfaces need consistent, surface-appropriate views of one producer document and
one command path, while no surface is currently permitted to see the document at all.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|--------|-------|----------|-----------|
| A | Band-Aid | Each surface consumes `ProducerDocument` directly; the contract types are already `pub`. | Zero new model; three renderers reproduce lane resolution, availability, budget and truncation independently — the drift this issue exists to remove — and it is illegal under boundary 2. |
| B | Local Optimum | One projector with a `SurfaceKind { Web, Mcp, Device }` enum and a `match` per behaviour. | One code path, but surface knowledge moves *into* the producer layer; every new client is a new arm in the core, and a device that can render per-choice reasons is indistinguishable from one that cannot. |
| C | Reframe | **Capability-declared `SurfaceProfile` + one pure projector owned by `src/producers/`, emitting a closed `SurfaceProjection`; one `ControlPlaneService` owns routing and command submission.** | One more model layer, and profiles must be honest about what they can render. Surfaces become data, not code. |
| D | Redesign | Server-authored screens/widgets per surface (the v4 manifest approach). | Pixels become the domain model; explicitly excluded by the interaction plane's "what this framing excludes". |
| E | Reframe | Producers emit per-surface document variants. | Pushes surface knowledge into every adapter, N×M, and destroys producer neutrality — the exact coupling #312/#313 exist to remove. |

### Evaluation

**A — surfaces read the document.**
Solves stated problem: no. Cost: low. Burden: high.
Second-order: the failure is not hypothetical. Lane resolution alone has five lanes plus an
editor-effective projection; `EffectiveView` exists in the contract precisely because #323 judged
per-surface resolution unsafe. Truncation for a device, per-choice availability, and "which reason
do I show" would each be re-decided three times. Also disqualified mechanically: boundary 2 fails
the moment `src/mcp/mod.rs` names `crate::adaptive`. Rejected.

**B — surface-kind enum inside the projector.**
Solves stated problem: partially. Cost: low-medium. Burden: medium-high.
Second-order: it works until the second device class. The interaction plane's hard constraint is
*"Device resources and peripherals vary … Negotiate capabilities; do not infer from board names"* —
a `SurfaceKind` enum is a board name. The HQPTuner renderer audit on this issue makes the same point
from the other end: the mismatch it found was a *widget capability* mismatch (segmented controls
cannot express per-choice disabled-with-reason), not a device-class mismatch. Rejected, but its
single-code-path instinct is exactly right and is what C keeps.

**C — capability-declared profiles, one pure projector, one command service.**
Solves stated problem: yes. Cost: medium. Burden: low.
Second-order: the profile is *data*, so a new surface is a new profile literal, and "can this
surface express what the producer advertised?" becomes a checkable question rather than an
assumption. Because the projector is pure over `(ProducerSnapshot, SurfaceProfile)`, cross-surface
drift becomes a property two projections of one fixture either have or do not — i.e. a test rather
than a review comment. Costs: one more layer between document and pixel, and a dishonest profile
produces a dishonest projection (mitigated: capability loss is *declared* in the projection, never
silent). Selected.

**D — server-authored screens.**
Solves stated problem: superficially, then worse. Cost: high. Burden: high.
Second-order: it is the v4 approach the program session already evaluated and rejected — *"Do not
port pixels as the domain contract"*. It also makes every layout change a server release and leaves
MCP, which has no pixels, outside the model. Rejected.

**E — per-surface producer documents.**
Solves stated problem: no. Cost: high. Burden: very high.
Second-order: every adapter would need to know every client class; adding a knob variant would
touch HQPlayer. It also breaks #324's single-admission-gate property, because "the document" stops
being one object. Rejected.

### Recommendation

**Selected:** Option C — capability-declared surface profiles over one aggregator-owned projector
and one command service.
**Level:** Reframe.

**Rationale.** The reframe is *what a surface is*. Read as a class of device, a surface is code and
every one of them is a fork. Read as a **declaration of what it can faithfully render and how much
of it**, a surface is data, and the projector's job becomes a single question asked uniformly: can
this profile express what the producer advertised, and if not, what must be said out loud? That
question is answerable, testable, and — crucially — produces the same answer for the web, MCP and a
knob, which is the acceptance criterion the issue actually turns on.

**Accepted trade-offs.**
* One more model between document and pixel. Justified because the alternative is three.
* Capability loss is handled by *declaration*, not by silent substitution: a control a surface
  cannot render faithfully is withheld with a reason, and a truncated list reports what it dropped.
  Surfaces get less, never something untrue.
* Priority for a bounded device (transport/volume/metadata first) is **profile policy**, expressed
  as a list of group ids the profile ranks. The projector holds no HQPlayer-specific knowledge.
* The delivered layer has **no production consumer** until the API contract below is approved. It is
  exercised by contract tests against canonical fixtures. This is stated as a limitation rather than
  disguised by wiring something into `main.rs` that nothing reads.

### Implementation Notes

1. `src/producers/surface.rs` — pure projection. No tokio, no locks, no clock, no adapter, no
   `crate::api`. `project(&ProducerSnapshot, &SurfaceProfile) -> SurfaceProjection`.
2. `src/producers/control_plane.rs` — the application service over `AdaptiveView`: capability
   routing, projection, and command submission delegated to #329's actor via #375's registry.
3. Both live under `src/producers/` because boundary 2 exempts exactly `src/adaptive/` and
   `src/producers/`. Widening `EXEMPT_DIRS` would weaken the lint that encodes the API freeze; it is
   not touched.
4. Contract tests in `tests/adaptive_control_plane.rs`, driven by the canonical `#323/#324`
   fixtures, comparing web / MCP / device projections of **the same** fixture.
5. No route changes. `tests/fixtures/api_routes.txt` untouched; no label applied.

---

## Execute

**Updated:** 2026-07-31
**Status:** complete

### TDD evidence — RED before GREEN

`tests/adaptive_control_plane.rs` was written first, against a deliberately naive stub
(`src/producers/surface.rs` returning an empty projection and `control_plane.rs` routing by
first-match). The RED transcript is recorded in the Execute PR comment and reproduced in
"Verification" below.

### What landed

| Unit | Behaviour |
|---|---|
| `SurfaceProfile` / `SurfaceCapabilities` / `SurfaceBudget` | A surface declares which control kinds it can render, whether it can show per-choice availability reasons and reason text, the highest risk class it may advertise as invokable, its control/choice budgets and its group priority. Nothing is inferred from a device name. |
| `project()` | One pure function. Same snapshot in, surface-appropriate projection out, one `RevisionRef` stamped on the whole result so no consumer can mix two revisions. |
| Lane separation | `observed` is projected separately from `editor_effective`, `desired`, `held` and `persisted`. The observed lane never carries staged intent. |
| Capability honesty | A control whose kind the profile cannot render, or whose per-choice availability the profile cannot express, is `Withheld` with a `Reason` — never silently flattened. |
| Declared truncation | Budget overflow produces `omitted: Vec<OmittedControl>` carrying the id and the reason. A projection never silently claims completeness. |
| Dependency invalidation | A non-terminal operation on a control whose `apply.invalidates` names `Y` puts `Y`'s choices in `Reloading { invalidated_by, operation }` and **withholds the stale list**. |
| Unknown ≠ empty | `ChoiceProjection::Unknown { reason }` is distinct from an enumerated set that happens to be empty. |
| Operation scoping | Pending markers are per `(control, operation_id)`; one control's completion cannot clear another's. |
| Version fallback | An unsupported major yields `SurfaceProjection::Fallback` carrying the legible identity and the refusal. An untested *product* version yields a notice and keeps every discovered control. A newer minor yields `UnknownAdditions` and renders. |
| `ControlPlaneService::route()` | Resolves a prefixed zone id to a `ProducerKey` from admitted state only. An unknown `hqplayer:` identity returns `RouteRefusal::UnknownProducer`; it can never resolve to another family's key. |
| `ControlPlaneService::submit()` | Surface-neutral semantic command → routed producer → #375's registry → #329's actor. A producer family with no registered executor is refused, not defaulted. |

### Drift check

* Original scope: surfaces consume projections of one producer model over one command path.
* Current work: the projection model, the command path, the routing rule, and their falsification
  tests. No surface wired.
* Gap: the three surface-rendering acceptance criteria and the ESP32 criterion. Cause: the API
  freeze (and, for ESP32, #326 is unstarted). Reported, not worked around.
* Verdict: aligned, and deliberately short of the issue's full acceptance set.

---

## API impact

**None.** `tests/fixtures/api_routes.txt` is byte-identical to `origin/v3`. No `.route(` line added
or removed. `src/api/`, `src/mcp/`, `src/app/`, `src/knobs/` and `src/mqtt/` are untouched. No label
applied to the PR.

### The smallest exact contract that would unblock the surface criteria

Submitted for explicit approval; **not implemented on this branch**.

```
GET  /adaptive/producers                     -> { producers: [ProducerRef] }
GET  /adaptive/projection?producer=<id>&role=<role>&zone=<zone>&surface=<profile-id>
                                             -> SurfaceProjection
POST /adaptive/command                       -> CommandAcknowledgement
       { producer, role, zone?, surface, control, value, lane, correlation_id,
         expected: { epoch, control_plane, state } }
```

Three routes, one new response type (`SurfaceProjection`, already defined and tested here), one new
request type. Notes that matter for the approval decision:

* `GET /adaptive/projection` returns the **projection**, never `ProducerDocument`. The v1 contract
  stays inside the repository; what leaves is the surface-shaped view, which is versioned by
  `SurfaceProjection.schema` independently of the producer contract.
* `POST /adaptive/command` carries `expected` (epoch + both revisions), so #329's revision fence
  applies to every surface identically, and `correlation_id`, so a retried tap is one operation.
* `tests/adaptive_publication_lint.rs::lint_publication_layer_adds_no_routes` currently forbids the
  strings `/adaptive` and `/producer` in `api_routes.txt`. Approving these routes means amending
  that lint deliberately, in the same change, with a rationale — not deleting it.
* Nothing else needs to change: MCP tools would call the same `ControlPlaneService` in-process once
  the boundary-2 exemption is widened to name it, which is a smaller and separately reviewable step.

---

## Limitations

1. **No surface consumes this yet.** Web rendering, MCP discovery/execution and device manifests all
   require the contract above. The layer is complete and tested; the last hop is an approval.
2. **ESP32 manifests are doubly blocked.** Beyond the API freeze, #326 (matcher-to-manifest
   composition) does not exist on this branch. The `device` profile and its bounded, priority-ordered
   projection are delivered and tested, but there is no manifest composer to feed.
3. **One producer family has an executor.** `submit()` routes HQPlayer to #329's actor. Roon, LMS,
   OpenHome and UPnP publish no producer documents yet, so they are refused with
   `NoExecutorForProducer` rather than defaulted — correct, but it means "all surfaces use the same
   command path" is currently true of one family.
4. **Catalog text is keys only.** `label_key` / `description_key` / `display_text_key` pass through
   unresolved. #343 owns the provenance-governed catalog; resolving keys here would have invented
   the provenance rules that issue exists to decide.
5. **Continuous-control interaction rules are not implemented.** Transient drag state, coalesced
   pointer intent and latest-wins settlement are surface-side behaviours; the projector supplies the
   range, step, unit and reset provenance they need, and #342/#329 own the interaction itself.
6. **`SurfaceProjection` is not itself version-negotiated across processes.** It carries a `schema`
   field and a compatibility rule, but with no out-of-process consumer the rule has never been
   exercised against a real mismatch.
7. **No live validation.** Hermetic fixtures only, as instructed. #375 already records that the
   6.0.2 rig is in an About-only Trial state that prevents native/config observation.

---

## Verification

Recorded at the exact head in the Execute PR comment.
