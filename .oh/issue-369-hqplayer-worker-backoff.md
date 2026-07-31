# Issue #369 — HQPlayer restart-aware recovery and worker self-healing

**OH:** 80222d6d
**Issue:** #369
**Parent:** #311
**Program:** #310
**Branch:** `feat/issue-369-hqplayer-worker-backoff`
**Draft PR:** #373 (stacked on #371)

## Aim
**Updated:** 2026-07-31

Keep every configured HQPlayer producer observable and recoverable: fast through a qualified daemon
restart, quiet during a prolonged outage, stable before retry debt is forgiven, and replaced exactly
once after an unexpected worker exit.

## Problem Statement
**Updated:** 2026-07-31

A fixed reconnect delay and retained task-map membership are not lifecycle management. HQPlayer
recovery has a short expected restart phase and a prolonged-outage phase, while a completed task
handle is no longer a live producer. Retry state and worker liveness must therefore be explicit
supervisor state.

## Constraints

- Preserve all existing HTTP, SSE, client, and MCP route/payload schemas.
- The aggregator remains the authority for reachable zone state.
- Runtime add/remove/start/stop operations remain serialized and return only after affected workers
  are joined.
- Restart timing is injectable until measured against a supported HQPlayer build.
- Retry debt resets only after coherent observations remain stable for a threshold.
- Every delay and observation is cancellation-aware.
- The release profile currently uses `panic = "abort"`; in-process panic replacement is impossible
  there. Unexpected exits remain self-healing, unwind-build panics are covered, and release panic
  policy is tracked explicitly rather than overclaimed.

## Solution Space
**Updated:** 2026-07-31

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-Aid | Check `JoinHandle::is_finished` opportunistically | A dead worker remains dead until another API call and races multiply |
| B | Local optimum | Reuse `AdapterHandle::run_with_retry` unchanged | Wrong lifecycle unit; no restart window, per-instance health, or retained-handle repair |
| C | Reframe | One per-instance supervisor around the existing observer, sharing a pure recovery tracker across replacements | Moderate local refactor |
| D | Redesign | Replace the manager with a command-channel actor and `JoinSet` | Strong global ownership, but disproportionate rewrite for this slice |

### Recommendation

Select **C**. Keep the manager's existing lifecycle transaction as the single ordering boundary.
Each map entry becomes a long-lived supervisor handle/token/status. The supervisor owns and joins one
child observer, clears adapter reachability after an unexpected exit, preserves the recovery tracker
across replacements, waits cancellation-safely, and starts exactly one replacement.

Use a pure recovery state machine:

- fixed short delay while elapsed outage time is inside the measured restart window;
- saturating exponential progression after that window, capped at a documented maximum;
- a coherent success marks the child healthy, but retry debt resets only after coherent health spans
  the configured stability threshold.

Expose only an internal Rust lifecycle snapshot with supervisor-enabled plus
`Recovering`/`Healthy`/`Failed`/`Stopped`. Existing serialized status shapes remain frozen.

### Why not the alternatives

- A cannot notice failure autonomously and does not own completion.
- B solves adapter-loop retries but not per-instance supervisor liveness.
- D is a valid later redesign if lifecycle mutations outgrow the existing transaction lock, but
  rewriting every manager command now adds more race surface than it removes.

### Dissent adjustments accepted

- Retry state outlives replaceable children.
- Unexpected exit cleanup invalidates stale reachability before replacement.
- Stop/remove cancel and join the supervisor inside the existing lifecycle transaction.
- Same-name remove/re-add cannot observe an old completion because removal joins before the
  transaction releases.
- Panic recovery is not claimed for release builds while `panic = "abort"` remains.

## Execute
**Updated:** 2026-07-31
**Status:** in-progress

- [x] Aim is clear.
- [x] Frozen external contracts and manager transaction boundaries are known.
- [x] Scope is limited to recovery policy, supervisor ownership, internal status, tests, and measured
  supported-version evidence.
- [x] No UI or MCP payload changes are in this issue.
- [x] RED tests demonstrated policy progression/reset, zero-delay storm prevention, internal status,
  shared retry debt, supervisor exit/panic/cancellation behavior, and the shutdown-vs-panic stale
  cleanup race before their implementations.
- [x] Focused lifecycle (37), lifecycle policy/supervisor unit tests (8), native conformance (271),
  API contract, await-in-lock lint, and workspace/all-features clippy gates pass.
- [x] Full-workspace all-features test passes; the opt-in live qualification remains ignored.
- [x] Final exact-diff review and dissent gates pass with no code findings.
- [ ] Supported HQPlayer restart recovery is measured only after explicit live-test notice.

### Live qualification attempt

The explicit read-only probe was compiled and run from the development host against HQPlayer
Embedded 6.0.2 at the configured native endpoint. A coherent baseline could not be established:
both remote and loopback requests were accepted at TCP and reset by the daemon. HTTP remained
available, redirected the main surface to `/about`, and reported a Trial license. The process had
been running since the prior day.

No restart occurred. `systemctl restart hqplayerd` requested the root credential; the supplied
operator account is not authorized to manage the unit, and no privilege or service configuration
was changed. The supported-version restart-window gate therefore remains pending an
operator-authorized restart while the read-only probe is waiting.

The ignored, opt-in probe now emits `uhc-hqp-lifecycle/v1` JSON containing product, version, engine,
probe interval, connect/response timeouts, last-coherent-to-first-failed-observation time, and
first-failed-observation-to-coherent-recovery time. It also emits explicit recovery-window lower
and upper bounds; policy qualification uses the conservative last-coherent-to-recovery upper bound.

## Review and Dissent
**Updated:** 2026-07-31

- Review: PASS for a draft PR. The shutdown/panic cleanup race, shared replacement debt, probe
  identity, and timing-bound semantics are verified.
- Dissent: PASS with no P1–P4 code findings. No stale reachability, duplicate supervisor, orphan
  worker, reconnect storm, or public-contract drift was found.
- Known gate: keep the PR draft until the live HQPlayer Embedded 6.0.2 measurement qualifies or
  replaces the provisional 12-second restart window.
- Explicit dependency: #372 owns release-mode in-process panic recovery while release builds use
  `panic = "abort"`.

## Ship
**Updated:** 2026-07-31
**Status:** in-progress

- [x] Implementation, hermetic tests, exact-diff review, and dissent are complete.
- [x] Issue #369 states the release panic constraint and dependency on #372.
- [x] Commit and push the focused stacked branch.
- [x] Open draft PR #373 stacked on #371.
- [x] Request CodeRabbit.
- [ ] Post the six exact-head workflow reports and validate their output contracts.
- [ ] Live qualification remains a pre-merge gate; no merge is authorized.
