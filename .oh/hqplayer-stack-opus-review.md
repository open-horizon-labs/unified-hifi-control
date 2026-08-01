# Opus stacked review: the HQPlayer direct-control stack (#382 → #391 → #406)

**OH:** 80222d6d · **Date:** 2026-07-31 · **Reviewer:** independent adversarial pass, no subagents,
no `superego`/`ba`/`wm`, no live host contact.

This document lives on `feat/issue-401-hqplayer-mcp-beta-validation` only. The lower branches carry
code and tests from this review but no review prose, so their diffs stay about their own issues.

## What was reviewed

| PR | Branch | Declared base | Head reviewed | Head after review |
|----|--------|---------------|---------------|-------------------|
| #382 | `feat/issue-329-hqplayer-immediate-command` | `feat/issue-375-hqplayer-adaptive-producer` @ `69114f4` | `a178b76` | `a178b76` (unchanged) |
| #391 | `feat/issue-328-direct-hqplayer-zone` | `feat/issue-329-hqplayer-immediate-command` @ `a178b76` | `212f555` | `71c02a6` |
| #406 | `feat/issue-401-hqplayer-mcp-beta-validation` | `feat/issue-328-direct-hqplayer-zone` @ `212f555` → `71c02a6` | `f438fd3` | see PR comment |

Each PR was read only against its own declared base (`git diff base...head`), bottom-up, and the
composed top of stack was then re-tested as a whole after the rebase.

## Method

The review was run as falsification, not as a read-through. The axes were fixed in advance from the
program's own risk list: operation correlation/fencing/retirement under races; parser bounds and
metadata clearing; published capability versus command authorization; withdrawn, restarted and
reconfigured instances; volume range, rounding, mute and restore; prefix and instance ambiguity;
MCP JSON-RPC, session and error behaviour and every control action; cross-PR stack correctness; and
unsafe beta assumptions.

For every material finding the order was: write the regression test first, run it and record the
actual failure output, apply the smallest correct fix on the branch that introduced the defect,
commit there, then rebase the dependents bottom-up and re-run the full composed suite.

## Findings

### #382 — no material defect

The command core was probed rather than accepted. The specific things checked and found sound:

* `fingerprint` is length-delimited with a per-variant tag on every sum, so distinct requests cannot
  alias through separators or field boundaries. `ControlValue::Composite` is a `BTreeMap`, so the
  canonical form is order-stable.
* The `preflight` → `reserve` → `begin_dispatch` → `complete` sequence is owned by a single-consumer
  actor that awaits each stage, so there is no intra-actor interleaving to race; the coordinator
  repeats the identity, revision and actionability checks while reserving.
* Retention is bounded on all four axes (terminal history, correlation tombstones, retired
  instances, unresolved operations) and each bound has a dedicated test that drives past it.
* The `engine_value` match is exhaustive over `HqpSemanticOperation`, so a newly registered
  operation cannot silently fall through to a native write — it fails to compile.

Non-material observations, recorded rather than changed:

* `MAX_CORRELATION_ID_BYTES` is defined twice (`producers/hqplayer_command.rs`,
  `producers/hqplayer.rs`), both `256`. A drift hazard, not a defect.
* `lookup_correlation` consults `correlation_tombstones` on the live instance but not on a retired
  one. The outcome is the same refusal either way (a retired producer is not `Live`, so `preflight`
  rejects), only the refusal code differs.
* `DeferredToIssue328` is now misnamed: #328 landed and deliberately did **not** adopt the adaptive
  command path for transport/volume — it uses the direct adapter path. The refusal is still correct;
  its name points at a decision that was made differently.
* The actor serialises native I/O, so one slow command delays the queue behind it. That is the
  bounded-actor design, not a regression.

### #391 — one material defect, fixed

**An unorderable `VolumeRange` panicked the polling worker.** `min_db` and `max_db` are both
`parse_attr_f64(...).unwrap_or_default()`, so nothing guarantees they are ordered or finite. A
daemon that reports `max` and omits `min` yields `min_db = 0.0` against a negative `max_db`, and
`"nan".parse::<f64>()` is a parse *success*, so `min="nan"` arrives as NaN. `f64::clamp` is
specified to panic on either shape, via `assert!`, so in release builds too.

#391 added `status.volume_db.clamp(vol_range.min_db, vol_range.max_db)` to `hqp_status_to_zone`,
which runs on the handshake and on every status poll, plus two more clamps on the command path. Such
a daemon therefore panicked the managed worker before it ever published: no zone, no error, nothing
for a surface to show.

Fixed as a capability question rather than a safer clamp. A range that cannot bound a level is not a
volume control, exactly as `enabled="0"` is not. A zero-width range is refused for the adjacent
reason: nothing can move within it, and `is_muted` (`value <= min + tolerance`) would be
unconditionally true, so the zone would report itself permanently muted.
`require_volume_control` re-checks the same precondition because it is the only gate in front of the
two command-path clamps, and a panic in an HTTP or MCP handler is a worse failure than a 400.

Commit: `71c02a6` on `feat/issue-328-direct-hqplayer-zone`.

RED evidence, before the fix:

```
thread '...' panicked at library/core/src/num/f64.rs:1430:9:
min > max, or either was NaN. min = 0.0, max = -10.0
   (repeated on every poll)
thread '...' panicked at tests/hqplayer_direct_zone.rs:206:9:
zone never satisfied the condition inside the bounded budget; zones=[]
```

Tests added to `tests/hqplayer_direct_zone.rs`:
`an_unorderable_volume_range_is_refused_rather_than_panicking_the_poll` (end to end through the wire
fake and `POST /control`) and `a_nan_or_zero_width_volume_range_publishes_no_control` (the NaN,
infinite and zero-width shapes the fake cannot express, through the `project_direct_zone` seam).

Accepted, not fixed — `quantise_db` can round a clamped level up to 0.005 dB past the published
`max`. With `max = -20.004` an absolute write clamps to the f32 bound and then quantises to `-20.00`,
which is above it. Real, but 0.005 dB is far below audibility and below the finest step any observed
daemon reports, and the alternatives (quantise toward the interior, or clamp after quantising to a
non-grid bound) each trade a clean decimal on the wire for nothing a user can perceive. Recorded so
the next reader does not have to rediscover the arithmetic.

### #406 — two material defects, fixed

**1. `hifi_play` and `hifi_search` still handed an `hqplayer:` zone id to Roon.** This is the same
recognise-one-prefix-then-fall-through-to-Roon shape #391 closed in `knob_control_handler` and #406
itself closed in `hifi_control` — left intact in two sibling switches in the same file.
`args.zone_id` goes verbatim into `roon.search_and_play(query, zone_id, …)`, so a `hqplayer:` id an
assistant took from `hifi_zones` became a foreign `zone_or_output_id` offered to a live Roon core.
Both now refuse before dispatch and name the limitation, so the assistant can pick a zone that can
serve the request instead of reading a Roon error it cannot act on.

**2. `hifi_control` presented a pre-command observation as the zone's current state.** A direct
HQPlayer zone is refreshed only by its producer's status poll (2 s by default), and the adapter's
transport commands are fire-and-forget — `adapter.pause()` sends `Pause` and returns. So the
`aggregator.get_zone` immediately afterwards returns the poll from *before* the command, every time,
and it was labelled `Current state:`. The tool told the assistant its own pause had not taken effect;
the observable consequence is a reported failure or a second pause. The surface may not read the
adapter to close the gap (`docs/ARCHITECTURE.md`, `tests/architecture_lint.rs`), so the claim is
dropped rather than a fresher observation invented.

RED evidence, before the fixes:

```
hifi_play(zone_id="hqplayer:rig") ->
  Error: Play error: Browse service not available - not connected to Roon

hifi_control(zone_id="hqplayer:rig", action="pause") ->
  Action 'pause' executed.
  Current state:
  { "state": "playing", ... }
  (while Pause was in the daemon's request log and its playback was 1)
```

Tests added to `tests/mcp_hqplayer_control.rs`, both driven over the real `/mcp` streamable-HTTP
surface with `rust-mcp-sdk`'s client runtime:
`mcp_play_and_search_do_not_hand_an_hqplayer_zone_id_to_roon` and
`mcp_control_does_not_report_a_pre_command_observation_as_current`.

The first assertion was tightened once during the review: the initial version keyed on the absence
of the word "Roon", which the fix's own "pick an lms:/roon: zone" hint legitimately contains. It now
keys on the failure *shape* (`Play error:` / `Search error:` / `not connected`), which only a call
that was actually dispatched to a backend can produce. RED was re-confirmed against the tightened
assertion by reverting `src/mcp/mod.rs` and re-running.

## API impact

None, on any of the three branches. No route added, removed or changed; `tests/fixtures/api_routes.txt`
untouched and `cargo test --test api_contract` green. No MCP tool name, parameter, or schema changed
— the #406 fixes change refusal behaviour and one result string only. AGENTS.md already records
search and play-by-query as unsupported for a direct HQPlayer zone, so no documented capability
changed either.

No adaptive control plane was added, extended, or wired to a surface.

## Live status

No live HQPlayer host was contacted at any point in this review. Every result above is hermetic,
from the stateful wire/model fake in `tests/mock_servers/hqplayer`. The 192.168.1.61 rig's two
known faults and #337's prior live PASS are unchanged by this pass and were not re-exercised.

## Beta A (#350)

The Beta A artifact recorded earlier in `.oh/hqplayer-direct-beta.md` is **provenance-stale**: its
source SHA (`96b5eb6`) and base SHA (`212f555`) both predate this review's commits and the rebase, so
its SHA-256 no longer identifies the code on this branch. The artifact was never published, which is
why this is a rebuild rather than a withdrawal. Beta A cannot be re-qualified from the existing row.

## Residual risks

* **Unfixed, adjacent, out of scope.** `hifi_play` and `hifi_search` still fall through to Roon for
  `openhome:` and `upnp:` zone ids. Same line, same class, but outside this review's HQPlayer scope;
  the established fix is the prefix-aware refusal #391 put in `knob_control_handler`.
* `Self::error_result` returns a plain text result prefixed `Error:` and does not set the MCP
  `isError` flag, so a tool failure is only distinguishable by prose. Pre-existing and shared by
  every tool; setting the flag is arguably a result-schema change and was left for explicit approval.
* The capability flags a command is judged against can be up to one poll interval (2 s) stale. Known
  and recorded in `.oh/hqplayer-direct-zone.md`; closing it needs either a forbidden adapter read on
  the command path or daemon-side compare-and-set.
* Mute has a protocol-inherent false positive (a level deliberately turned to the floor is
  indistinguishable from mute) and no unmute. Recorded in `.oh/hqplayer-direct-zone.md`.
* `hqp_status_to_zone` falls back to `hqplayer:<host>` when an adapter has no instance name, and that
  zone id would not resolve back to an instance on the command path. Unreachable today — every
  adapter the manager can hand out is named by `load_from_config` or `get_or_create_locked` — but the
  fallback is a latent unroutable-zone hazard if a bare `HqpAdapter` is ever wired to publish.
* `TrackMetadata.sample_rate` falls back to the root `samplerate` attribute on evidence from a single
  corpus document where root and child agree. Falsifiable with upsampled material on a live rig;
  already recorded in `.oh/hqplayer-direct-zone.md` and left to #332.

## Test results after the review

Full composed suite on the rebased top of stack, `cargo test --all-features`: 33 binaries, 1292
tests, 0 failures. `cargo fmt --check` clean, `cargo clippy --lib --all-features -- -D warnings`
clean, `cargo check --release --features server` clean. Per-PR figures are in each PR's
`Opus stacked review` comment.
