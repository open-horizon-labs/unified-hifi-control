# ADR 004: Producer-owned adaptive command causality

## Status

Proposed

## Context

The adaptive producer document describes semantic controls, observed values, apply behavior, revisions, and command outcomes. HQPlayer is the first real producer. Web, MCP, and device surfaces need one execution path, but the existing boundaries are deliberately one-way:

- `AdaptiveView` reads atomic aggregator-owned snapshots but cannot reserve or mutate them;
- native adapters translate protocol facts and commands but import no adaptive contract;
- `HqpAdaptivePublisher` owns the producer's revision tracker and active adapter-run lease;
- the aggregator retains only admitted whole documents and refuses different content at equal revisions.

A command crosses time. State may advance after a surface reads a snapshot and before native I/O begins; observations may arrive while the write is in flight; a newer command or adapter run may supersede an older completion. Delivery evidence also differs from application evidence. An absent reply after bytes left is possibly applied, not rejected, and cannot be safely replayed.

The initial proposal used an application service to preflight `AdaptiveView`, execute the adapter, and ask the publisher to append an outcome. Review and dissent showed that this had no atomic reservation, would let the next observation erase operation history, excluded operation changes from state revisions, and could retain a clone of the run lease across native I/O.

## Decision

Adaptive immediate commands use four explicit ownership layers.

1. **Application service — generic advisory preflight and routing.** It reads one `AdaptiveView` snapshot and rejects an unknown producer, non-Live or stale state, mismatched expected revision, unavailable/wrong-lane control, and invalid semantic value. It never mutates a snapshot and never calls an adapter without a producer reservation.
2. **Producer coordinator — liveness, linearization, and document causality.** One per-instance publisher actor owns the non-cloneable `AdapterRun` for the manager lifecycle and alone performs observation, reservation, completion, retirement, and stop publications. It rechecks the exact expected epoch/revisions, deduplicates correlation plus request fingerprint, allocates a dedicated operation generation, appends explicit `CommandOutcome::Pending`, advances state revision, and awaits admission. Only then may execution be queued. It retains an append-only operation ledger and merges it into every later observed document before revision materialization.
3. **Immediate-command actor — accepted-job lifetime and dispatch fencing.** A bounded actor owns accepted jobs independently of the submitting caller. Immediately before native dispatch it asks the producer coordinator to atomically validate the opaque lease and record a coordinator-only executor-begun fence. If the producer run, generation, or conflict-set ownership has changed, it terminalizes the operation as not-attempted Superseded and does not call the adapter. The fence is ordering state, not write-attempt evidence, and is never projected into the producer document. Graceful shutdown closes and drains this actor before adapters are stopped. Caller cancellation cannot abandon a reserved operation: cancellation before dispatch becomes Superseded/NotAttempted; after execution begins, the actor continues through typed completion.
4. **Native executor — typed protocol evidence.** The HQPlayer executor accepts only a validated semantic command and returns a contract-free typed receipt distinguishing no attempt, possible attempt, daemon acknowledgement/rejection, authoritative same-session readback, divergence, and unavailable readback. It never imports adaptive outcome types and never exposes native indexes. The manager holds its lifecycle transaction across exact-instance resolution and native execution so removal or reconfiguration cannot race a dispatched command.

Completion carries an opaque lease containing producer identity, producer epoch, adapter-run ID, conflict set, and operation generation. The publisher accepts a transition only if that lease is still current. A native operation retains the run ID, never the RAII run lease; stop or replacement therefore makes a late completion stale instead of extending producer liveness.

Before the final dispatch fence, `Pending` is explicitly a no-send state. A matching observation in
that interval resolves the operation as `Ignored` with `NotAttempted`; it must never manufacture
`Applied`/`Confirmed`. Once the executor-begun fence is installed, `Pending` means the attempt is
unknown: matching or conflicting observations may update producer state but cannot resolve that
operation. Receipt evidence is the only source of attempted-write truth, and admitting typed
completion clears the fence atomically with its operation transition. Manager stop refuses to drop
the run while a fence remains. Instance removal may arrive after the manager has finished the
native call but before the command actor publishes that receipt, so it records retirement intent
and blocks new work while retaining the coordinator. Retirement occurs only after the receipt's
terminal operation is admitted; the compact retired coordinator then preserves exact correlation
truth for a late duplicate callback.

`CommandOutcome::Pending` is an explicit non-terminal state. It is not evidence of producer state, sets `awaits_convergence`, and may transition to Applied, Ignored, Rejected, Superseded, Disconnected, TimedOut, Indeterminate, or a future recognized terminal outcome. No terminal outcome can regress to Pending.

Operation content participates in canonical state equivalence. Pending, terminal, convergence, and supersession transitions advance the state revision. Volatile lane timing and health refreshes remain revision-neutral. Observations merge the ledger instead of clearing it, so operation history cannot disappear after a poll.

Immediate commands are serialized by declared conflict set, not merely by control ID. The first #329 slice uses one conservative pipeline conflict set for mode, filter 1x, filter Nx, shaper, and rate. Transport and volume stay in #328 until their native APIs expose truthful attempt/evidence receipts.

The supported-operation policy is machine-checkable. Descriptors that #375 publishes as mutable but whose evidence is owned by #328 receive a structured deferred refusal from this service rather than falling through generic dispatch.

Every operation records its control, requested semantic value, lane, correlation, observed base revision, write attempt, outcome, and transition history. Repeating the same correlation and request fingerprint returns the existing operation without another send; reusing a correlation for different intent is rejected.

Terminal publications are a serialization frontier: while one is deferred, observations and new
reservations first flush the immutable terminal candidate rather than publishing a different
document at the same next revision. Documents retain the newest 32 terminal operations; compacted
records retain bounded correlation/fingerprint tombstones (256) so a recent compacted gesture
cannot become a fresh write. Retired producer identities are globally bounded at 64 as well as
bounded within each retained ledger. Correlation identity is therefore a finite replay window,
not perpetual storage; an expired client request must still pass the current epoch/revision
preflight before it can reserve native I/O. Unresolved records are never compacted; admission
applies backpressure at 32 unresolved operations instead of silently forgetting uncertainty.

## Options considered

### Surface-specific adapter dispatch

Rejected. It duplicates validation/outcomes across web, MCP, and devices, leaks backend identity rules, and recreates direct-adapter architecture violations.

### Aggregator actor executes native commands

Rejected. It would make the generic retained-state owner depend on backend executors and hold publication progress behind native timeouts. The aggregator remains the admission/store authority, not the I/O worker.

### Application preflight followed directly by execution

Rejected. A read-only snapshot is not a reservation. It cannot prevent producer advancement between preflight and send or order completion against observations/newer commands.

### Fully bidirectional producer actors

Deferred. A producer actor owning observation, command execution, and publication may be appropriate after several producers validate the pattern, but redesigning #324/#375 is disproportionate for the first immediate command path.

## Consequences

### Positive

- All later surfaces share one revision-fenced semantic command contract.
- Pending state is admitted before native I/O and never replaces authoritative observed values.
- Operation history survives polls, reconnect handling, and concurrent observations.
- Stale completions, duplicate gestures, and ended adapter runs cannot send or overwrite newer truth.
- Caller cancellation and orderly shutdown cannot strand Pending or retain a conflict set forever.
- Native attempt evidence remains typed and testable without adaptive contract leakage into adapters.
- Coupled HQPlayer controls serialize according to actual wire semantics.

### Costs

- The publisher gains a per-instance command coordinator and operation ledger in addition to observation revision tracking.
- The existing #347 setter internals need a typed receipt seam while preserving current public method contracts.
- Operation transitions cause additional producer state revisions and publications.
- The foundation PR adds no visible user surface; #331/#209/#222 depend on it.

### Risks and mitigations

- **Publisher complexity:** require barrier-based tests for observation/pending/completion ordering and stale run replacement.
- **Cancellation and shutdown races:** make accepted jobs actor-owned, drain before adapter shutdown, and test cancellation after reservation, during I/O, and while completion publication is queued.
- **False receipt strength:** define and test an evidence policy per semantic operation; defer operations that cannot meet it.
- **Unbounded ledger growth:** bound terminal ledgers, correlation tombstones, and retired instance identities; never silently discard active or unresolved operations.
- **Future producers need a different model:** keep the command service generic but the coordinator producer-owned; revisit bidirectional actors after a second producer supplies evidence.

## Notes

Generated from solution review and structured dissent for #329 on 2026-07-31. The ADR becomes Accepted only when the implementation PR is approved and merged.
