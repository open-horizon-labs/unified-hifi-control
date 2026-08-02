# HQPlayer aggregator consolidation

## Aim

Make the existing `ZoneAggregator` the sole owner of HQPlayer state consumed by web, knobs, HTTP,
and MCP. Preserve the reliable native/browser protocol implementation while removing the private,
unconsumed adaptive shadow runtime.

## Problem Statement

PR #428 currently publishes a rich but narrow HQPlayer document into a second private aggregator.
No public surface can read it, its command ingress has no production submitter, and ordinary clients
continue to query or dispatch through separate paths. The product therefore has two state models and
the more elaborate one provides no user-visible behavior.

## Solution Space

Selected redesign: publish a typed, coherent HQPlayer snapshot into the existing aggregator. HTTP,
web, knob, and MCP readers project from that snapshot; commands execute through the shared HQPlayer
dispatcher/instance manager and verified readback republishes state. Remove the adaptive runtime,
publisher, and idle command actor. Retain only useful reliability concepts such as session epochs,
lane health/freshness, coherent observations, and verified receipts.

## Execute

**Updated:** 2026-08-02
**Status:** in progress — extending the consolidated HQPlayer branch to the bus-only adapter boundary

### Bus/aggregator boundary extension

- Aim: keep this as one HQPlayer beta branch while making the in-app bus the only adapter I/O
  boundary and the existing aggregator the only readable state authority.
- Pre-flight: preserve every public route/payload, retain verified HQPlayer readback, preserve LMS's
  proven independent CLI/poll retry behavior, and do not merge without approval.
- First guardrail: the syntax-based architecture lint from #436 is now incorporated into this
  branch. Its exact combined baseline contains 117 existing dependencies; every migration must
  reduce it and no new dependency may be added.
- Runtime prerequisite: #440 must separate reliable critical delivery and projection commits from
  lossy post-commit notification before generic and specialized call sites migrate.

### UI expansion pre-flight

- Aim: every currently wired HQPlayer control is visible and operable from HQPlayer zones, with
  advanced native state discoverable before a write.
- Scope: junk filter, convolution, adaptive volume, repeat/random, stop/seek/mute, and the fuller
  native status/options readout on the existing HQPlayer page and zone cards.
- Constraints: preserve existing routes and incumbent visual language; additive payload fields only;
  aggregator remains the sole state owner; mobile retains all controls with touch-sized targets.
- Out of scope: playlist/library management, generic adaptive UI, merging without approval.
- Success: contract tests fail before implementation and pass after; desktop and mobile browser
  passes operate the live controls and verify readback without leaving the daemon changed.

### Pre-flight

- Aim is clear: one aggregator-owned HQPlayer truth across every current surface.
- Constraints: no public route or payload contract changes without separate explicit approval;
  preserve direct-zone and QNAP behavior; test before merge; do not merge without user approval.
- Scope: aggregator state, HQPlayer publication, existing surface readers/dispatch, missing web
  controls that can be expressed through existing contracts, removal of the private adaptive path.
- Out of scope: generic adaptive UI/schema publication, playlist/library implementation, merging.
- Success: relevant contract, adapter, aggregator, UI/MCP, recovery, and packaging tests pass; live
  read-only and safely reversible checks confirm the installed behavior before a new package handoff.

### Implemented

- `ZoneAggregator` retains typed per-instance HQPlayer observations, advanced choices, profile data,
  freshness, session epoch, and revision.
- Production constructs the HQPlayer manager with the existing aggregator as its observation sink;
  the private adaptive runtime and unused command actor are no longer started.
- Pipeline/status/profile/matrix HTTP reads and all HQPlayer MCP reads publish through and project
  from aggregator-owned state.
- Knob, web, HTTP, and MCP mutations share one dispatcher for transport/volume, and successful
  native writes perform coherent readback before reporting success. Pipeline, profile, and matrix
  mutations likewise republish verified state.
- Fixed the profile option value mismatch and matrix-current response model in the Dioxus client.
- Fixed live HQPlayer 6.0.4 matrix lists that omit `index`: compatibility indices are now derived
  from list order, making the second and later matrix profiles selectable.
- HQPlayer direct zones now render now-playing, previous/play-pause/next/stop, verified mute,
  volume, and seek controls. Playback and volume observations refresh from aggregator events.
- The DSP card now renders live engine state, matrix/profile selectors, core pipeline settings, junk
  filter, repeat, convolution, adaptive volume, and random controls from native choices/readback.
- Browser POST helpers now reject non-2xx responses and show the daemon's error instead of silently
  treating a refused command as success.
- HQPlayer cards and DSP controls collapse without loss at mobile widths; every small button has a
  44 by 44 pixel minimum target.

### Verification

- `cargo test --all-features --quiet`: passed.
- `cargo clippy -- -D warnings`: passed.
- API route contract, client harness, HQPlayer direct-zone (65), MCP HQPlayer (48), lifecycle, and
  native conformance (300) suites passed.
- Live UHC process connected to `192.168.1.61:4321` and published `hqplayer:default` through the
  aggregator. HTTP and MCP status returned the same native state and complete live enumerations.
- Live mode changed SDM -> PCM -> SDM with verified readback and restored the starting mode.
- Live matrix changed Default -> Mch-to-Stereo mixdown -> Default with verified readback and restored
  the starting profile. The live no-index matrix defect reproduced before the fix and passed after.
- Chrome exercised the live UI against `192.168.1.61`: junk filter none -> 20k -> none, repeat off ->
  current -> off, adaptive volume off -> on -> off, random off -> on -> off, stop while stopped, and
  mute -3 dB -> -60 dB -> -3 dB. Each mutation matched native state and the daemon was restored.
- The live daemon accepted `SetConvolution` but continued to report convolution off. UHC correctly
  refused to claim success and the browser displayed the verified HTTP 500 explanation.
- Play against the daemon's empty transport correctly surfaced its native rejection; seek remained
  disabled with a visible explanation because the daemon reported no track duration.
- Browser checks passed at 1250 px and 375 px content widths with no horizontal overflow. Mobile
  transport/volume targets measured 44 by 44 pixels; the Impeccable detector returned no findings.
- `cargo test --test client_harness` (83), `hqplayer_direct_zone` (65),
  `mcp_hqplayer_control` (48), and `api_contract` (2) passed; all-feature clippy passed with warnings
  denied and `git diff --check` passed.
- Live tier-1 capture completed every native family within 129 ms, but correctly failed the checked-in
  corpus diff: the abbreviated `hqpd-6.0.4-opal` fixtures differ from the real daemon in 142 entries,
  indices, or enum IDs. Artifact: `/tmp/uhc-hqp-tier1-aggregator.json`. This is a corpus provenance
  issue, not a transport or aggregator failure.

### Residual

- The generic adaptive contract/source remains compiled for compatibility and historical tests, but
  no production runtime, publisher, reader, or command actor uses it. Removing that dormant public
  Rust module and its 26k-line contract/test corpus is a separate deletion, not required to prevent
  parallel runtime state.
- Named HQPlayer browser profiles remain absent on the live rig (`/hqplayer/profiles` returns `[]`),
  so the named-profile selector correctly has nothing to show. Native matrix profiles now work.

## Review Summary

**Aim:** Make `ZoneAggregator` the sole production owner of HQPlayer state used by every existing
surface, with one managed command/readback path.

**Status:** Continue

### Alignment Check

- Necessary: Yes — the private adaptive runtime had no public reader and duplicated retained state.
- Aligned: Yes — production publication, reads, and post-command convergence now meet at the existing
  aggregator.
- Sufficient: Yes — existing HTTP payloads and route contracts are preserved; no generic schema or
  new endpoint was added.
- Mechanism clear: Yes — manager gathers coherent native/browser observations, sink publishes them,
  surfaces project aggregator snapshots, and writes republish verified readback.
- Changes complete: Yes for the production swap and existing controls. The live corpus mismatch is
  recorded separately and named browser profiles require profiles to exist on the daemon.

### Drift Detected

- Scope drift: live verification exposed missing-index matrix profiles. This was accepted because it
  directly prevented the requested profile switching on the target daemon; it received a focused
  regression test and live restore verification.
- Alignment drift caught and corrected: the first implementation retained advanced/profile data in
  the aggregator but let GET handlers initiate adapter reads. Refresh ownership moved into
  `HqpInstanceManager`, leaving surfaces with manager-refresh plus aggregator-read only.

### Decision

Proceed to commit, PR publication, CI, and x64 QNAP artifact. Do not merge without user approval.
