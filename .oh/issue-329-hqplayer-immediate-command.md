# Issue 329 — HQPlayer Immediate Adaptive Command Service

**Updated:** 2026-07-31
**Status:** solution space
**Issue:** #329
**Parent:** #313
**Blocked by:** #375 / PR #376, #347
**Blocks:** #331, #209, #222

## Aim

Every UHC surface can request the same semantic HQPlayer control and receive the same truthful, correlated outcome, without knowing native indexes, calling an adapter directly, or inferring success from whether a request returned.

## Problem Statement

**Current framing:** Connect the adaptive control IDs published by #375 to the existing HQPlayer adapter methods.

**Reframed as:** Web, MCP, and control-device users need one mutation path that proves a request was valid against the exact live producer state, records whether a write may have left the process, and converges on authoritative observed state; today each surface either calls an adapter directly or has no adaptive execution path, so validation and ambiguous-delivery semantics would drift.

**The shift:** From a control-ID switch statement to a revision-fenced application service with typed native execution receipts and producer-owned operation history.

### Constraints

- **Hard:** The aggregator remains the only retained producer-state owner; surfaces never reach adapters; the native adapter imports no adaptive contract; one exact producer revision is preflighted and then atomically reserved by the publisher before mutation; native indexes remain private; ambiguous writes are never blindly replayed; operation history is append-only and merged into every observation by the producer publisher; no public route or payload changes in the first PR.
- **Soft:** Concrete service/module names, whether execution is serialized per instance or per conflict set initially, and how long an indeterminate operation remains eligible for convergence.

### What this framing enables

A single internal command contract can later drive web, MCP, and devices; stale requests fail before mutation; pending/terminal state is visible without replacing observed values; HQPlayer-specific execution stays typed and testable behind the generic preflight boundary.

### What this framing excludes

Surface-specific adapter calls, string-parsing `anyhow` errors into outcomes, optimistic UI values, direct mutation of aggregator snapshots, a public endpoint in the foundation PR, and multi-step persistent/staged plans.

## Solution Space Analysis

**Problem:** Turn a retained adaptive descriptor into one safe, observable HQPlayer immediate command without duplicating validation or outcome semantics across surfaces.

**Key constraint:** Producer documents and `OperationRecord` are retained by the actor-owned aggregator, but a read-only `AdaptiveView` cannot reserve a command. The HQPlayer publisher is the only component that owns both the last admitted document/revision and the run lease needed to publish a Pending reservation before native I/O.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-aid | Keep bespoke web/MCP/device switches that call `HqpAdapter` | Fast first demo, guaranteed semantic drift and direct-adapter boundary violations |
| B | Local optimum | Put HQPlayer dispatch directly in the producer aggregator actor | Atomic store access, but mixes generic retention with backend I/O and lets slow native writes block publication |
| C | Reframe | Application service performs generic preflight, then the producer publisher atomically reserves Pending, invokes a typed producer executor, and serializes observation/operation publication | More seams, but preserves ownership, lifecycle fencing, backpressure, and one reusable surface contract |
| D | Redesign | Make every producer a bidirectional actor that owns observation, command execution, and document publication | Strong ownership model, but large #324/#375 redesign before validating the first immediate path |

### Evaluation

**Option A — surface-specific dispatch**

- Solves stated problem: no; it creates three command paths.
- Implementation cost: low initially.
- Maintenance burden: high and permanent.
- Second-order effects: raw identity fallback, inconsistent stale-choice checks, and incompatible outcome language.

**Option B — aggregator executes adapters**

- Solves stated problem: partially.
- Implementation cost: medium.
- Maintenance burden: high because the generic actor gains backend registries and async I/O.
- Second-order effects: native timeouts can stall producer admission; adapter ownership leaks into the state store.

**Option C — advisory generic preflight plus publisher-owned reservation/outcomes**

- Solves stated problem: yes.
- Implementation cost: medium-high.
- Maintenance burden: moderate, with one generic preflight path and one typed executor per producer.
- Second-order effects: the publisher gains a per-instance coordinator/ledger; command and observation commits share that serialization point; operation-only publication advances state revision.
- Future options: web/MCP/device projections, batching, persistent lanes, and additional producers reuse the same request/receipt model.

**Option D — bidirectional producer actors**

- Solves stated problem: yes.
- Implementation cost: high.
- Maintenance burden: potentially low after migration.
- Second-order effects: invalidates reviewed #324/#375 boundaries and expands the first command slice into a runtime redesign.

### Recommendation

**Selected:** Option C — generic preflight followed by publisher-linearized reservation, typed execution, and producer-owned operation publication.

**Level:** Reframe.

**Rationale:** Reading, reservation, and native execution have different owners. The application service can reject obvious invalid input from one atomic aggregator snapshot without becoming a state store. The publisher must recheck that exact revision and reserve Pending under its per-instance coordinator before the executor may run; that is the command linearization point. The HQPlayer executor preserves native delivery evidence without importing adaptive vocabulary, and the same publisher merges operation history into every later observation. This is the smallest design that produces one truthful path for all later surfaces.

**Why not the others:**

- A violates the explicit #331 architecture and recreates the bug class this program exists to remove.
- B distributes backend knowledge into the generic aggregator and couples native latency to publication.
- D may become attractive after several producers, but it is premature before the first typed immediate path validates the need.

**Accepted trade-offs:**

- The foundation PR introduces internal service/executor/outcome seams before any new UI is visible.
- The first slice covers only the five #329 pipeline settings whose #347 path already performs semantic readback: mode, filter 1x, filter Nx, shaper, and rate.
- HQPlayer transport and volume remain in #328 because their current `Result<()>` cannot distinguish not-attempted, provisional acknowledgement, and ambiguous delivery truthfully.
- Operation-only changes become state changes and must advance the producer state revision; volatile health remains revision-neutral.

## Selected execution shape

1. Define an internal immediate command request with explicit producer/instance key, expected epoch/revisions, control ID, Immediate lane, desired semantic value, caller correlation ID, and a deterministic request fingerprint.
2. Perform advisory generic preflight against one `AdaptiveView` snapshot: exact identity/revision, `Live` presence, non-stale connected Native lane, mutable/available Immediate control, correct kind, and an exact current choice ID whose `engine_name` is the only value forwarded.
3. Submit the request to the single HQPlayer publisher coordinator actor. That task owns the non-cloneable `AdapterRun` for the whole manager lifecycle and serializes observations, reservations, completions, retirements, and run stop. It rechecks its last admitted epoch/revisions and actionability, deduplicates `(producer, conflict set, correlation, fingerprint)`, allocates a dedicated operation generation, appends an explicit `CommandOutcome::Pending`, advances state revision, and awaits admission before returning an opaque lease.
4. The application service may enqueue the actor-owned execution job only after reservation succeeds. A private validated command cannot contain a native index or an unvalidated semantic value.
5. A bounded immediate-command actor owns every accepted job independently of the submitting caller. After reservation it sends the correlation/operation acknowledgement, then continues execution even if the caller disconnects or cancels. Graceful shutdown closes and drains this actor before adapter shutdown, so accepted jobs always terminalize. Immediately before dispatch it asks the publisher coordinator to validate the opaque lease again; stale run/generation is terminalized as not-attempted Superseded.
6. Route the five pipeline settings through the one #375 `HqpSemanticOperation` registry to a typed HQPlayer executor resolved by exact instance identity. The manager holds its lifecycle transaction across exact-instance resolution and native execution, so concurrent instance removal/reconfiguration cannot race a dispatched command. The registry also declares one conservative pipeline conflict set for this slice; one-sided filters cannot race and mode cannot interleave with dependent settings.
7. Refactor the #347 setting path to return a contract-free typed native receipt preserving: not attempted, possibly attempted, daemon acknowledged/rejected, same-session readback verified, readback divergent/unavailable. Existing public methods collapse the receipt back to their unchanged `Result<SettingOutcome>` contract; no error-string classification is allowed.
8. Return the receipt with the opaque lease. The publisher coordinator checks producer epoch, actor-owned adapter-run ID, operation generation, and conflict-set ownership before appending a terminal/indeterminate transition. A stale completion cannot modify the retained document.
9. Retain the append-only operation ledger separately from each freshly projected observation, merge it before every `RevisionTracker::materialize`, and include canonical operation content in the state view. Pending, terminal, convergence, and supersession advance state revision; volatile health remains revision-neutral.
10. Store explicit control, desired semantic value, lane, correlation, and observed revision in the operation/plan record. A duplicate correlation plus identical fingerprint returns the existing operation; a reused correlation with different intent is rejected.
11. Extend the shared command vocabulary with explicit `Pending`: it is non-terminal, not producer-state evidence, and awaits convergence. Legal transitions leave Pending for Applied, Ignored, Rejected, Superseded, Disconnected, TimedOut, Indeterminate, or a recognized future outcome; no terminal outcome can regress to Pending.
12. If cancellation or shutdown occurs before dispatch, terminalize Pending as Superseded with `WriteAttempt::NotAttempted`. Once native I/O begins, caller cancellation cannot abort the actor-owned job; its typed receipt determines the terminal/indeterminate result. Tests cover cancellation after reservation, during native I/O, and while completion publication is queued.

## First-PR boundaries

In scope:

- internal service, advisory preflight, publisher reservation/ledger, typed HQPlayer setting receipt, and hermetic tests;
- mode, filter 1x, filter Nx, shaper, and rate through the existing #375 registry;
- explicit machine-checkable registry evidence/conflict/support policy that returns a structured `DeferredToIssue328`-style refusal for transport and volume until #328 supplies truthful receipts.

Out of scope:

- new or modified HTTP, SSE, MCP, Muse, or device routes/payloads;
- web rendering, MCP tool schemas, ESP32 manifest composition;
- staged, held, persistent, restart, batch, preset, matrix, and composite execution;
- live mutation until the configured Embedded target serves operational native/configuration surfaces.

## Required red tests

- stale epoch or revision refuses before executor invocation;
- producer state advancing after advisory preflight but before publisher reservation refuses before executor invocation;
- LastKnown, stale, disconnected, unavailable, read-only, wrong-lane, wrong-kind, out-of-range, off-step, and unknown-choice requests refuse before execution;
- source-mode rate remains uninvokable while mode remains available as the escape control;
- every in-scope pipeline descriptor resolves through exactly one executor arm and one declared conflict/evidence policy; transport/volume are explicitly deferred to #328;
- typed receipts distinguish no-send, provisional, verified/no-op, and possibly-applied delivery without parsing messages;
- `Pending` is serialized distinctly, is not state evidence, awaits convergence, and cannot be entered from a terminal outcome;
- pending does not replace observed value; terminal transition appends history and advances state revision only;
- a native observation after Pending preserves the ledger and advances from the latest state revision rather than erasing or regressing it;
- an older operation completion cannot clear or overwrite a newer per-control operation;
- duplicate correlation with an identical fingerprint sends at most once; a different fingerprint is rejected;
- filter-side commands and mode/dependent settings cannot interleave within the pipeline conflict set;
- adapter-run replacement prevents stale outcome publication;
- caller cancellation after reservation cannot strand Pending or retain the conflict set indefinitely;
- stop/replacement between reservation and dispatch prevents the native call, not merely its later publication.

## Drift stop conditions

- Any proposed public endpoint or payload change pauses for explicit approval.
- If operation history cannot be published without the command service mutating aggregator state directly, return to solution space rather than adding a debug/release bypass.
- If an existing adapter method cannot report whether a write may have left, do not advertise that command through this service until the native receipt is made truthful.

## Operation evidence matrix — first slice

| Control | Native operation | Conflict set | Strongest terminal evidence |
|---|---|---|---|
| `hqplayer.pipeline.mode` | `set_mode` | pipeline | same-session semantic State readback; same-mode is confirmed no-op |
| `hqplayer.pipeline.filter_1x` | `set_filter_1x` | pipeline | same-session 1x readback while preserving authoritative Nx sibling |
| `hqplayer.pipeline.filter_nx` | `set_filter_nx` | pipeline | same-session Nx readback while preserving authoritative 1x sibling |
| `hqplayer.pipeline.shaper` | `set_shaper` | pipeline | same-session semantic State readback |
| `hqplayer.pipeline.rate` | `set_rate` | pipeline | same-session exact-rate readback; source-mode suppression is not attempted |
| transport actions | #328 | transport/library | deferred: at-most-once attempt and observed predicates require explicit policy |
| decimal-dB volume | #328 | volume | deferred: adaptive volume prevents generic exact readback equivalence |

## Solution review

**Verdict:** ADJUST, then continue.

The selected ownership direction is necessary and aligned, but the original draft incorrectly treated `AdaptiveView` preflight as a reservation, assumed operation publication could be bolted onto an observation-only publisher, reused an unspecified generation token, and overclaimed truthful evidence for all 11 controls. The adjusted design adds publisher-owned reservation/ledger serialization, dedicated operation generation and idempotency, explicit desired-value recording, lifecycle fencing, and the #328/#329 evidence split.

## Solution dissent

**Recommendation:** ADJUST / PROCEED with the revised design.

The strongest failure cases were credible:

- Pending could disappear on the next observation because projection currently starts with an empty operation list.
- An operation-only update could be refused as `NotAdvanced` because operations were excluded from revision canonicalization.
- A late completion could publish after a newer command or after manager stop if it retained a clone of the run lease.
- Transport or volume errors could be falsely classified by parsing `anyhow` text.
- Per-control locking would still allow two one-sided `SetFilter` calls or mode/rate to conflict.

The revised actor-owned publisher coordinator, operation overlay, dedicated lease generation, typed native setting receipt, conservative pipeline conflict set, and narrowed evidence matrix directly address those cases. This ownership decision is durable enough to record as an ADR before implementation.

Follow-up review found the direction sound after replacing the implied per-call run ownership with a single publisher coordinator actor that owns the non-cloneable run for the manager lifecycle. Follow-up dissent additionally required explicit `Pending`, actor-owned cancellation/drain behavior, and a second lifecycle check immediately before dispatch; all are now part of the selected execution shape and red-test contract.
