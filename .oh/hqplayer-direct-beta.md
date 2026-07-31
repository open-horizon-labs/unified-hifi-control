# Direct-control beta qualification: MCP surface validation (#401) + Beta A handoff (#350)

**OH:** 80222d6d · **Issue:** #401 (MCP surface validation) · **Program:** #350 (Beta A) ·
**Parent epics:** #392 (MCP), #313 (HQPlayer) · **Base branch:** `feat/issue-328-direct-hqplayer-zone`
(PR #391), itself stacked on `feat/issue-329-hqplayer-immediate-command` (PR #382).

### Scope reset (read before the rest of this document)

The user explicitly narrowed this program's scope after #401 and #350 were filed:

* **No adaptive/dynamic declarative public control plane.** HQPlayer must behave like the existing
  Roon/OpenHome/LMS integrations: a normal UHC zone with now-playing, transport, seek and safe
  volume on web/knob; MCP exposes those same controls plus the existing explicit HQPlayer
  pipeline/profile operations, over the same reliable HQPlayer command core, without a new adaptive
  UI/API.
* #401's own acceptance criteria (offline resource/capability/browse/queue suite, `mcp-smoke.sh`,
  CAPABILITIES-derived doc lint) are blocked on #397–#400, none of which have landed on `v3`
  (verified: no commits, no `src/*resource*`/`*capabilit*` modules). Re-implementing that epic here
  would silently expand scope past what was authorized. **This session addresses only the
  HQPlayer-relevant slice of #401**: the MCP surface's integration/coverage gaps against the direct
  HQPlayer zone #328 shipped, and the doc convergence #401 asks for, scoped to HQPlayer. The general
  Roon/LMS resource/capability/browse/queue suite is out of scope and untouched.
* Do not use subagents, superego, `ba`, or `wm` this session. #329/#328 (PRs #382/#391) are audited
  read-only; nothing on those branches is rewritten or merged.

---

## Aim

An operator (or an AI assistant through MCP) picks a direct `hqplayer:` zone and every surface tells
the same truth about it: the web UI, the knob, and MCP route transport/volume to the same daemon,
none of them silently hands an HQPlayer command to Roon, and MCP's HQPlayer-specific tools
(status/profiles/pipeline) are documented for what they actually do. AGENTS.md's MCP section stops
being silent about HQPlayer's existence as a zone.

The behavior to look for: an MCP client (Claude, etc.) asked to "pause the HQPlayer zone" gets a
paused HQPlayer, not a paused (or errored) Roon zone; asked to raise its volume, gets a decibel
change on the daemon it named.

Not the aim: a new MCP tool, resource, or schema; multi-instance HQPlayer selection through MCP
(the shipped tools have no instance/zone parameter — adding one is a schema change requiring
approval, out of scope); anything that reopens #328's already-reviewed direct-zone projection.

---

## Problem Statement

**Current framing (#401):** validate the MCP surface end to end and converge its self-description.

**Reframed, after auditing #329/#328 against the MCP surface (`src/mcp/mod.rs`) rather than assuming
it inherited their fixes:** #391 closed the *knob* surface's "every HQPlayer command reaches Roon"
defect in `knob_control_handler` (`src/knobs/routes.rs`). It did not touch `src/mcp/mod.rs` — by its
own accounting ("Deliberately not touched: … `src/mcp/mod.rs`"). MCP has an **independent, structurally
identical** zone-routing switch inside `HifiTools::HifiControlTool`, and it has the same defect,
un-fixed, because it was never in #391's diff.

### What the code actually does (read before choosing an approach)

| # | Defect / gap | Location |
|---|--------------|----------|
| 1 | **Every HQPlayer transport command sent through MCP's `hifi_control` tool reaches Roon.** The zone-id dispatch checks `lms:`, `openhome:`, `upnp:` and falls through to `self.state.roon.control(...)` for everything else, including `hqplayer:` — the exact defect #391 fixed in the knob handler, unfixed here because it is a separate switch in a separate file. | `src/mcp/mod.rs:370-389` |
| 2 | **MCP volume control (`volume_set`/`volume_up`/`volume_down`) has no HQPlayer arm at all.** The `set_volume` helper recognizes only `lms:` and `roon:`/unprefixed; a `hqplayer:` zone id hits the explicit `else` branch and is refused with "Volume control not supported for this zone type" even though #328 gave the zone a real decimal-dB control. | `src/mcp/mod.rs:256-269` |
| 3 | **`seek`, `stop`, and `mute` are not recognized MCP control actions.** `HifiControlTool.action` is a free-text string (no schema change needed to add values), but the implementation's action match has no arms for them — they fall into the generic `other => other` pass-through and are sent verbatim as an unknown action to whichever backend `.control()` was called, which HQPlayer's routing (once fixed) would reject as `UNKNOWN_ACTION`. The direct zone's own contract (#328 AC) requires exactly these verbs. | `src/mcp/mod.rs:347-368` |
| 4 | **Zero test coverage of the MCP handler.** No test file exercises `HifiMcpHandler`, `HifiControlTool`, or any `hifi_hqplayer_*` tool; the only files matching "mcp" in `tests/` reference it in comments. The routing defect above has been reachable and silently wrong through an entire release cycle with no test able to catch it. | `tests/` (absence) |
| 5 | **Doc drift.** AGENTS.md's MCP capability matrix has no HQPlayer column (it predates #328's direct zone); `hifi_zones`/`hifi_control`/`hifi_now_playing` tool descriptions and the server's `instructions` string say "Roon, LMS, OpenHome, UPnP" and never mention HQPlayer, though the zone is real and, once (1)–(3) are fixed, fully controllable through these same tools. | `AGENTS.md:189-224`, `src/mcp/mod.rs:35,56,90,816-823` |

### What is already right and is not re-solved here

* `hifi_zones` and `hifi_now_playing` already read `self.state.aggregator.get_zone(s)`, so a direct
  HQPlayer zone's truthful now-playing/capabilities (#328) already surface correctly through MCP —
  the aggregator is the only state source consulted, matching architecture. Verified by reading, and
  pinned by a coverage test rather than left as an assumption.
* MCP's `hifi_hqplayer_status`/`_profiles`/`_load_profile`/`_set_pipeline` tools call
  `self.state.hqplayer` (the manager's default instance) directly, exactly like the pre-existing
  legacy `/hqplayer/status`, `/hqplayer/control`, `/hqp/pipeline` HTTP handlers in `src/api/mod.rs` —
  this is the established, accepted pattern for HQPlayer *device-management* data (profiles,
  pipeline mode/filter lists) that has no equivalent shape in the `ZoneAggregator` (which holds
  playback zones, not daemon configuration). Not a defect; not touched.
* The HQPlayer command core itself — revision-fenced writes, typed receipts, readback verification —
  lives in `HqpAdapter`'s methods (`set_mode`, `set_volume_db`, `play`, …) and is shared by every
  caller of those methods already, MCP included. #329/#382's contribution is inside the adapter, not
  duplicated per-surface. Confirmed by reading `execute_mode_receipt_under_operation` and siblings.

### Constraints treated as real

* No public HTTP route, MCP tool name, or MCP tool schema changes without explicit user approval;
  `tests/fixtures/api_routes.txt` stays byte-identical. `HifiControlTool.action`/`.value` are already
  untyped enough (`String`/`Option<f64>`) to carry new recognized values with zero schema change.
* No adapter-direct MCP shortcut: MCP must resolve zone truth from `ZoneAggregator`, and instance
  resolution goes through `HqpInstanceManager`, exactly as the knob path does.
* Live validation is authorized on the user's HQPlayer 6.0.2 Embedded rig, non-mutating first, and
  every mutating check must have a proven restore path or must not run.
* #329/#328 branches are read-only inputs. Any shared logic they own is *called*, not copied and not
  edited on their branches.

---

## Solution Space

### Candidate A — Band-aid: add an `hqplayer:` arm directly inside `src/mcp/mod.rs`

Re-implement the routing/capability-check/dispatch logic that `control_hqplayer` already has,
natively inside `HifiTools::HifiControlTool`'s match arm, using `CallToolResult`/`CallToolError`
instead of `(StatusCode, Json)`.

* Solves the stated problem: yes, for routing and volume, if written carefully.
* Cost: low up front. Second-order cost is the one #401 exists to prevent: the safety-critical
  logic (capability checks against the *published* zone, dB clamping, the mute-is-floor rule, seek
  clamped to observed duration) now exists in two independent places. A future change to one of
  those rules — which is exactly the kind of change #329's still-open beta/dev criteria (ambiguous
  outcome recovery, re-enumeration after mode changes) will keep making — has to be applied twice or
  the surfaces silently diverge again. That is the same "three descriptions of itself disagree"
  failure mode #401's problem statement names, recreated one layer down.

### Candidate B — Local optimum: extract the existing dispatch into one shared, transport-neutral function

`control_hqplayer` in `src/knobs/routes.rs` already contains exactly the logic MCP needs: resolve
the instance from `HqpInstanceManager`, resolve the zone from `ZoneAggregator`, check the published
capability flags, clamp/quantise volume, dispatch to the adapter. Extract its body into a
transport-neutral function (`dispatch_hqplayer_action`, returning a small neutral error enum
instead of `(StatusCode, Json<Value>)`); make the existing HTTP `control_hqplayer` a thin wrapper
that converts the neutral result to its current HTTP shape (behavior-preserving refactor — same
inputs, same outputs, same status codes and `error_code` strings); call the same function from
`src/mcp/mod.rs` for `hqplayer:` zone ids, converting the neutral error to MCP's text-error shape.

* Solves the stated problem: yes, for routing, volume, seek/stop/mute, and it is the one place a
  future #329 semantics change (e.g., ambiguous-outcome handling) has to land to reach every surface.
* Cost: medium — touches `src/knobs/routes.rs` (an extraction, not a behavior change; the existing
  `tests/hqplayer_direct_zone.rs` suite is the regression guard that a pure refactor did not change
  anything) and adds one call site plus action-name translation in `src/mcp/mod.rs`.
* Second-order: none identified that Candidate A does not also have, and B removes the duplication
  cost instead of accepting it.

### Candidate C — Reframe: move the shared function into a new `src/services/hqplayer_control.rs`

Same extraction as B, but into a new module neither `knobs` nor `mcp` "owns", so neither surface
module appears to depend on the other's internals.

* Solves the stated problem: yes, identically to B.
* Cost: higher for no behavioral gain — a new top-level module, new `pub mod` wiring in `lib.rs`,
  and import-path churn in two files instead of one, to relocate logic that #328 already put in
  `knobs::routes` and that MCP can import from there (`crate::knobs::routes::dispatch_hqplayer_action`)
  exactly as cheaply. This is a real architectural improvement to bank, not a defect to fix now —
  recorded here so #331 (where the adaptive surface, and likely a real application-service layer,
  arrives) can pick it up rather than re-discover it.
* Rejected as premature for a "focused compatibility fix": more files touched, same result.

### Candidate D — Redesign: route all HQPlayer control (web, knob, and MCP) through the #329/#382 producer-coordinator path

Submit transport and volume as commands to `HqpImmediateCommandService` for every surface,
unifying knob, web, and MCP at the revision-fenced command layer instead of at `HqpAdapter`'s public
methods.

* Solves the stated problem: yes, and is the direction #331's adaptive surface eventually takes.
* Cost: highest. The #329 vocabulary (`Mode`/`Filter1x`/`FilterNx`/`Shaper`/`Rate`) has no transport
  verbs; adding them is a producer-document/contract change under
  `.oh/adaptive-producer-contract.md`'s compatibility policy, not a "focused MCP fix." It also
  duplicates #328's own Candidate D rejection for the identical reason.
* **Explicitly excluded by the user's scope reset** ("NO adaptive/dynamic declarative public control
  plane"). Rejected as out of scope, not merely as sequencing.

### Chosen: **B**

One command core (`HqpAdapter`'s methods, revision-fenced by #329/#382), one dispatch/capability-check
function (`dispatch_hqplayer_action`) that both the HTTP knob path and MCP call, two thin
transport-specific wrappers translating to `(StatusCode, Json)` and `CallToolResult` respectively.
This is the "normal ZoneAggregator/application service path, no adapter-direct MCP shortcut" the
task was scoped to, and it is exactly the "one semantic execution path" principle #328's own
inputs-read section names from `.oh/adaptive-interaction-plane.md`.

---

## Execute

**Status:** in progress · **Updated:** 2026-07-31

Red-first throughout: `tests/mcp_hqplayer_control.rs` drives the real `/mcp` HTTP surface with
`rust-mcp-sdk`'s own client runtime (dev-dependency, streamable-http transport) against a bound
ephemeral port — the same "test from the client's expectation" discipline AGENTS.md requires, with
the MCP client itself standing in for the AI-assistant client this surface exists to serve, the same
role the ESP32/iOS harness plays for the knob/web surfaces.

Filled in as the gates run; full transcript in the Execute-checkpoint PR comment.

| Gate | Command | Result |
|------|---------|--------|
| RED, targeted | `cargo test --test mcp_hqplayer_control` | _pending_ |
| GREEN, targeted | `cargo test --test mcp_hqplayer_control` | _pending_ |
| Direct-zone regression | `cargo test --test hqplayer_direct_zone` | _pending_ |
| Whole suite | `cargo test --all-features` | _pending_ |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | _pending_ |
| Format | `cargo fmt --all --check` | _pending_ |
| API contract | `cargo test --test api_contract` | _pending_ |
| Architecture/adaptive lints | `cargo test --test architecture_lint --test adaptive_dependency_lint --test adaptive_publication_lint` | _pending_ |
| Release UI | `dx build --release --platform web --features web` | _pending_ |
| Live rig | see Ship section | _pending_ |

---

## Review

_Pending — filled in after Execute is green._

---

## Dissent

_Pending._

---

## Ship / Beta A (#350)

_Pending. Will record: source SHA, included issues/PRs, toolchain, target, build date, checksum;
live-rig checklist with hardware-dependent rows explicitly labeled pending until run; rollback path;
artifact channel._
