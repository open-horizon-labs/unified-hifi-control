# Issue #347 — HQPlayer: verify live setters and scope enumerations to the loaded chain

OH: 80222d6d · epic #311 · stacked on #341 (`feat/issue-341-hqplayer-evidence-ledger`, head
`6d688bdbcb3fa607ca87a1593dd692e7ba00133a`)

## Aim

A client issues a semantic setting change during source-following playback, and UHC either proves the
exact setting applied or returns a bounded, truthful non-applied outcome without corrupting a sibling
setting.

## Problem Statement (confirmed from #347)

**The problem:** `result="OK"` and a mode-keyed list cache are both treated as proof. The daemon can
acknowledge a setter without applying it; the loaded PCM/SDM chain can move while configured mode
stays `[source]`; and the one-sided `SetFilter` helpers guess the sibling from a different `State`
field.

**Key constraint:** no public API route/request/response contract changes, no live appliance, and the
#322 executable corpus — not current Rust behaviour — is the protocol authority.

**Success:** every supported live setter reports `applied`, `already-set`, `ignored`, `suppressed` or
`ambiguous` from an authoritative `State` readback; enumerations are scoped to the loaded chain and
invalidated when it moves; no raw integer reaches a control path.

## Solution Space

### Axis 1 — how a setter outcome is represented

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-Aid | Keep `Result<()>`, put the distinction in error prose | "Ignored" is indistinguishable from "rejected" to any caller; no type stops a future call site from treating OK as applied |
| B | Local Optimum | `Result<()>` plus a side-channel counter/log | Observable only out-of-band; a boundary cannot map outcomes |
| C | Reframe | Adapter setters return `Result<SettingOutcome>`, `#[must_use]`, with `Applied / AlreadySet / Ignored / Suppressed / Ambiguous`; HTTP+MCP boundaries collapse it to today's `{ok:true}` / `{error}` shapes | Every call site must be updated; five variants to keep honest |
| D | Redesign | Move live-setting ownership to a producer/aggregator contract with an outcome event stream | #347 explicitly defers this to #325/#329; would balloon scope |

**Selected: C.** The issue names four outcomes and requires them "observably … using existing
internal contracts (no public API change)". A type is the only representation a later call site
cannot silently discard, and `Result<SettingOutcome>` keeps transport/rejection failures where they
already are. `AlreadySet` is the fifth because *skipping* a write is a distinct fact from *applying*
one — the no-op mode skip exists precisely so nothing is sent.

### Axis 2 — how the loaded chain is resolved

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-Aid | `refresh_lists()` before every setter | 4 extra round trips per write; never *detects* a change, so nothing else can react |
| B | Local Optimum | Keep the mode-keyed cache, add `State.active_mode` as a second key | Settles by fiat exactly what HQP-C-024 records as **unmeasured**. Rejected on evidence grounds |
| C | Reframe | **The loaded chain is the enumeration the daemon currently serves.** Per-family fingerprints; a fresh fetch whose fingerprint differs from the cached one *is* a chain transition and invalidates every chain-scoped family | Needs one fresh fetch per control operation; no PCM/SDM *label* falls out |
| D | Redesign | A chain-identity event on the bus that producers subscribe to | #325/#329 own the producer; premature here |

**Selected: C.** It is independent of configured mode and of rate-pin availability *by construction*,
it invents no threshold and no unmeasured field reading, and HQP-C-008 (`E0-uhc-live`) already
settles that enumerations are chain-scoped. The deliberate consequence: UHC identifies *which* chain
is loaded, not what to *call* it. Nothing in #347 needs the label, and inventing a PCM/SDM classifier
from rate spans would be the exact failure mode this epic exists to end.

### Axis 3 — the legacy numeric HTTP contract

| Option | Level | Approach | Trade-off |
|---|---|---|---|
| A | Band-Aid | Keep the adapter's `parse::<u32>()` fallback | The acceptance criterion forbids it; a stale index silently selects a different setting |
| B | Reframe | Resolve the number to a **name** at the HTTP boundary against the fresh enumeration, then call the semantic setter | Boundary gains a lookup and a failure mode; request/response contracts unchanged |

**Selected: B.** It is what #347 and HQP-C-063 jointly prescribe: keep the number at the
compatibility boundary, never publish or persist it as identity.

### Accepted trade-offs

- One fresh enumeration fetch per control operation. Correctness over round trips; the alternative is
  a cache that can name a different filter than the user picked.
- No PCM/SDM label for the loaded chain. `shaper_label` keeps its existing configured-mode heuristic;
  changing a response *value* has no acceptance criterion and would be an unbudgeted risk.
- `Ambiguous` is reachable only after a write was attempted and its reply lost — the client then does
  a bounded readback rather than assuming failure (HQP-C-029).

## Execute

Status: complete. See the PR body for RED/GREEN evidence.

## Review / Dissent

Recorded as PR comments 1–6.
