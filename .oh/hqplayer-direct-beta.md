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

**Status:** complete (draft PR open, not merged) · **Updated:** 2026-07-31

Red-first throughout: `tests/mcp_hqplayer_control.rs` drives the real `/mcp` HTTP surface with
`rust-mcp-sdk`'s own client runtime (dev-dependency, streamable-http transport) against a bound
ephemeral port — the same "test from the client's expectation" discipline AGENTS.md requires, with
the MCP client itself standing in for the AI-assistant client this surface exists to serve, the same
role the ESP32/iOS harness plays for the knob/web surfaces.

Implementation: extracted `control_hqplayer`'s body in `src/knobs/routes.rs` into transport-neutral
`dispatch_hqplayer_action` (`Result<(), HqpDispatchError>`); `control_hqplayer` became a thin wrapper
converting that back to its existing `(StatusCode, Json)` shape. `src/mcp/mod.rs`'s
`HifiControlTool` arm now calls the same function for `hqplayer:` zone ids before falling into its
existing Roon/LMS/OpenHome/UPnP dispatch. Tool descriptions and the server `instructions` string
updated to state HQPlayer's zone support, its dB volume unit, and the `hifi_hqplayer_*`
default-instance-only scope.

| Gate | Command | Result |
|------|---------|--------|
| RED, targeted | `cargo test --test mcp_hqplayer_control` @ `f7542f9` | **32 passed, 5 failed** — the routing/volume/action gaps the audit found, nothing else |
| GREEN, targeted | `cargo test --test mcp_hqplayer_control` | **38 passed, 0 failed** (8 behavioral + mock-server infra tests) |
| Direct-zone regression | `cargo test --test hqplayer_direct_zone` | **56 passed, 0 failed** — unchanged, confirms the extraction is behavior-preserving |
| Volume-safety source lint | `cargo test --test volume_safety` | Initially failed (scanned the now-empty `control_hqplayer` body) — non-vacuity assertion caught it; updated to scan `dispatch_hqplayer_action`; **16 passed** |
| Whole suite | `cargo test --all-features` | **1287+ passed, 0 failed, 33 targets** |
| Clippy (lib) | `cargo clippy --lib --all-features -- -D warnings` | **clean** |
| Clippy (all targets) | `cargo clippy --all-targets --all-features` | **395 pre-existing errors** (#338 backlog); **0** in any touched file |
| Format | `cargo fmt --all --check` | **clean** |
| API contract | `cargo test --test api_contract` | **2 passed**; `api_routes.txt` byte-identical |
| Architecture/adaptive lints | `architecture_lint` / `adaptive_dependency_lint` / `adaptive_publication_lint` | **9 / 7 / 57 passed** |
| Release UI | `dx build --release --platform web --features web` | **client + server build completed successfully** |
| Live rig | see "Live-rig validation attempt" section | **blocked**: rig-side `GetInfo` crash, unchanged since #382; stopped before mutation |

---

## Review

**Status:** complete · **Updated:** 2026-07-31

Read the whole diff against the base branch looking for what falsification would find, not just
re-confirming the design. One real gap found and closed; nothing else survived scrutiny.

1. **Coverage gap: `volume_up`/`volume_down` were never exercised through MCP.** The Execute
   checkpoint's tests proved `volume_set` reaches the HQPlayer instance but not the other two verbs
   that shared the same broken `set_volume` helper before the fix (and the same
   `dispatch_hqplayer_action` arm after it). Closed by
   `mcp_volume_up_and_down_route_to_the_hqplayer_instance`, which asserts the daemon received two
   distinct `Volume` writes moving in opposite directions from the last observed level.

Checked and **not** found to be defects:

* **MCP zone-level tools (`hifi_zones`/`hifi_now_playing`/`hifi_control`) do not honor the
  `adapters.hqplayer` (or any other adapter) settings toggle**, unlike the knob surface's
  `get_all_zones_internal`. Read `src/adapters/hqplayer.rs` and `HqpInstanceManager` for any
  toggle-awareness: none exists below the knob-routes filtering layer, and MCP's `hifi_zones` calls
  `state.aggregator.get_zones()` directly, bypassing that filter — for every backend, not only
  HQPlayer. Pre-existing behavior, not introduced or worsened by this branch; recorded as a known
  limitation rather than fixed, since closing it is an MCP-wide settings-filtering change well
  outside "focused HQPlayer compatibility fix."
* **Error-message duplication between the HTTP and MCP surfaces on `Backend` errors.** MCP's
  `Self::error_result` already prefixes `"Error: "`; the non-HQPlayer arm additionally prefixes
  `"Control error: "` before calling it, so its errors read `"Error: Control error: …"` while the new
  HQPlayer arm reads plain `"Error: …"`. Cosmetic only — the HQPlayer form is arguably cleaner — and
  not part of any tested contract.
* **Three near-identical `McpNowPlaying`-building blocks now exist in `src/mcp/mod.rs`** (the
  `hifi_now_playing` tool, the pre-existing non-HQPlayer control success path, and the new HQPlayer
  arm). A `fn mcp_now_playing(zone) -> McpNowPlaying` helper would remove the duplication, but two of
  the three copies predate this branch and extracting only for the new one is inconsistent scope
  creep for a "focused" fix. Recorded as a follow-up, not made.
* **`trim_start_matches("hqplayer:")` on an instance name containing a colon** — same non-finding
  #328's own review recorded for the knob path: it would strip a repeated literal prefix, but an
  instance named `hqplayer:x` is not a reachable configuration, and the behavior is identical to the
  code this branch reused, not new.

---

## Dissent

**Status:** complete · **Updated:** 2026-07-31 · **Verdict:** PROCEED

Actively argued against the chosen approach and the Review pass's own conclusions before locking in.

1. **The shared function widened its own accepted vocabulary — reopening reviewed code, not just
   reusing it.** `dispatch_hqplayer_action` initially accepted `"volume_set"` as a synonym for
   `"volume"`, a spelling only MCP uses. That is a behavior change to the exact function #328/#391
   already reviewed and dissented on, for the sake of a caller that hadn't been written yet — the
   opposite of "audit #329/#328, do not rewrite." **Closed:** moved the translation to the MCP call
   site (commit `c4e9a1c`); `dispatch_hqplayer_action`'s accepted actions are now byte-for-byte what
   `control_hqplayer` recognized before this branch touched it.
2. **Is Candidate B's dependency direction (`src/mcp` → `src/knobs::routes`) actually sound, or is
   it avoidance of Candidate C's larger diff?** Checked `docs/ARCHITECTURE.md` and
   `tests/architecture_lint.rs` for any rule treating `knobs`, `api`, and `mcp` as anything but peer
   surfaces (`architecture_lint.rs:282` lists all three together as surfaces that must route through
   the aggregator) — none found. The dependency is real but not circular, and Rust's module system
   does not encode "knobs is ESP32-only" as a constraint; nothing enforces it. Accepted as sound, not
   merely convenient; Candidate C remains banked for #331 if a true application-service layer
   emerges there.
3. **A future edit to the shared function for one surface's benefit could silently change the
   other's behavior**, since both now call one function. This is the design's whole point (one
   command core cannot diverge by construction) and also its main risk. Mitigated only by the
   function's own doc comment stating both callers, and by both surfaces' test suites running in the
   same `cargo test --all-features` pass — not by anything structural. Recorded as an accepted
   trade-off, not a defect: the alternative (Candidate A) has the identical risk in the opposite
   direction — silent divergence instead of silent coupling — and divergence is the worse failure
   mode for a safety-critical volume/transport path.
4. **Is the real MCP client (rust-mcp-sdk, dev-dependency) a meaningfully more faithful test than
   calling `handle_call_tool_request` directly, or extra machinery for its own sake?** Considered a
   hand-rolled `Arc<dyn McpServer>` stub with ~15 `unimplemented!()` methods to avoid the
   dev-dependency. Rejected: the stub only proves the handler's Rust-level logic, not the JSON-RPC
   framing/session/tool-call encoding a real client depends on — and getting 15 trait method
   signatures exactly right against a pinned SDK version is itself a source of drift no test would
   catch. The real client is more faithful and, once written, no larger in the test file.
5. **Did "checked and not a defect" in Review understate anything?** Re-examined the settings-toggle
   gap: confirmed by reading `src/adapters/hqplayer.rs` and `HqpInstanceManager` end to end that no
   toggle-awareness exists below the knob-routes filtering layer for *any* backend — this is not an
   HQPlayer-specific finding dressed up as general, it is general.

No change to the chosen approach (Candidate B) or its scope survived this pass except item 1's
narrowing. Final verdict: **PROCEED to live-rig validation.**

---

## Live-rig validation attempt (192.168.1.61)

**Status:** stopped before mutation, per constraint · **2026-07-31**

Non-mutating checks first, as required. Findings, in order:

1. **Host reachable.** `ping` succeeds (round trip ~0.4 ms — same LAN). No credentials were needed
   or used for any step below; the native HQPlayer protocol on port 4321 is unauthenticated.
2. **TCP connect to port 4321 succeeds and stays open if nothing is sent.** A bare connect, held
   idle for several seconds with no data written, closes cleanly with no reset and no unsolicited
   data. The daemon process is listening and accepting connections.
3. **Sending the exact `GetInfo` request `HqpAdapter::build_request` constructs
   (`<?xml version="1.0"?><GetInfo/>`) resets the connection every time** — three consecutive
   attempts, all `ECONNRESET`, with no partial reply. This isolates the fault to request parsing,
   not connection setup: the daemon accepts the TCP handshake but crashes/resets on receiving its
   first real request.

This exactly reproduces the rig-side fault already recorded against this host by PR #382 ("its
native 4321 connection currently resets during the initial GetInfo exchange, including from the
HQPlayer host itself") and by prior-session memory (libmicrohttpd 1.0.6 crash). **It has not been
fixed since**, as of this session.

**Stopping here, per the task's own rule:** live validation is authorized to proceed only past
non-mutating checks, and even the first non-mutating check — `GetInfo` — cannot complete. There is
therefore no live state to baseline, nothing to restore, and no basis for attempting pause/seek/
volume tests: a restoration guarantee cannot be evaluated against a daemon that resets before
answering a read. No mutation of any kind was attempted against this host. Fixing the daemon-side
crash requires host access (SSH/package-level) this session was not given credentials for and which
is outside this issue's scope regardless — #401/#350 validate UHC's client behavior, not HQPlayer
Embedded's own service health.

**What this means for #350's live-rig checklist:** every hardware-dependent row (metadata, transport,
seek, decimal volume, external-controller convergence, reconnect, linked-DSP coexistence) is
**pending, not passed** — labeled that way explicitly per #350's own constraint ("Label every
hardware-dependent row pending until run; do not describe it as passed"). Hermetic coverage
(`tests/mcp_hqplayer_control.rs`, `tests/hqplayer_direct_zone.rs`) is what stands behind this PR;
live verification needs either this rig's daemon-side fault fixed, or a different reachable HQPlayer
6.0.2 Embedded instance.

---

## Ship / Beta A (#350)

**Status:** artifact built and smoke-tested; **not published** — awaiting explicit maintainer
approval per #350's own hard constraint · **Updated:** 2026-07-31

> **Provenance below is stale — the artifact must be rebuilt before Beta A can be re-qualified.**
> The Opus stacked review (see [`hqplayer-stack-opus-review.md`](hqplayer-stack-opus-review.md))
> landed a fix on the base branch (`71c02a6`, PR #391) and two on this one, and this branch was then
> rebased. Both the source SHA and the base SHA in the table are pre-review, so the recorded SHA-256
> no longer identifies the code here. Nothing was published, so this is a rebuild rather than a
> withdrawal: re-run the build and the smoke test, then replace the rows below. The install/run and
> rollback instructions further down are unaffected.

### Provenance

| Field | Value |
|-------|-------|
| Source SHA | `96b5eb6698085612dd8792688bfb4c4068827dd5` (this branch, `feat/issue-401-hqplayer-mcp-beta-validation`) |
| Base SHA | `212f5551da3b2f83d6c433734b2613816c6ff6d0` (`feat/issue-328-direct-hqplayer-zone`, PR #391) |
| Included issues/PRs | #401 (this), stacked on #328/PR #391, stacked on #329/PR #382. All three still **draft, unmerged** — this artifact is not a release build off `v3`. |
| Toolchain | rustc 1.97.1 (rustup `stable-aarch64-apple-darwin`, 2026-07-14), Dioxus CLI 0.7.10 (57d6794) |
| Build command | `make css && dx build --release --platform web --features web` |
| Target | `aarch64-apple-darwin` (arm64 macOS) — the only target built this session; no cross-compiled Linux/musl artifact was produced |
| Build date | 2026-07-31 |
| Artifact | `target/dx/unified-hifi-control/release/web/server` (14 MB Mach-O 64-bit executable, plus its sibling `public/` directory of static/WASM assets — both are required; the binary alone will panic looking for `public/wasm/`, per README) |
| SHA-256 | `ab01c789c90045638885f523ab6372d307c7d607ee7f52ecb5f3da2786c93b15` |
| Config/schema version | Unchanged from `v3` — `AppSettings`/`app-settings.json` shape is untouched by this branch |

### Smoke test (no live hardware, no LAN activity)

Run with all four network-facing adapters (`roon`, `upnp`, `openhome`, `lms`) and `hqplayer`
disabled via an isolated `UHC_CONFIG_DIR`, specifically so the default-enabled Roon extension
discovery does not touch the real network during this check — the task's "do not scan the LAN"
constraint applies to this smoke test exactly as it does to live-rig validation.

| Check | Result |
|-------|--------|
| Process starts, binds port 8088, stays alive | ✅ |
| `GET /` (SSR/web hydration shell) | ✅ HTTP 200 |
| `GET /status` | ✅ valid JSON, all adapters correctly report disconnected |
| `GET /zones` (existing non-HQPlayer path, API contract) | ✅ HTTP 200, `{"zones":[]}` — correct with everything disabled |
| `POST /mcp` initialize without required `Accept` headers | ✅ correctly rejected with a JSON-RPC error, not a crash |

One earlier run this session (before the artifact above was rebuilt) hit a startup panic —
`Invalid route "/favicon.ico": Insertion failed due to conflict with previously registered route`
— when `has_embedded_assets()` evaluated false and the server fell back to
`serve_dioxus_application`'s own asset-route registration, colliding with the explicit
`/favicon.ico` route in `main.rs`. Rebuilding cleanly (after touching `src/embedded.rs` to rule out
stale incremental-compilation state, which briefly surfaced a different, unrelated `rust-embed`
`.iter()` compile error that also did not reproduce on a clean rebuild) made it disappear, and the
artifact above — built fresh, smoke-tested three times — starts cleanly every time. **Recorded as a
suspected build-environment/incremental-compilation fragility in the general Dioxus embedded-assets
pipeline** (`src/embedded.rs`, `main.rs`'s asset routes), **not reproduced on a clean build, not
touched by any commit on this branch** (this branch never edits `src/embedded.rs` or `main.rs`), and
out of scope to chase further here — flagged for whoever next touches the embedded-assets pipeline,
not fixed.

### Live-rig checklist (#350's required focused checklist)

Every row is hardware-dependent and is labeled **pending**, not passed, per #350's explicit
constraint. See "Live-rig validation attempt" above for why: the rig's daemon crashes on the first
non-mutating `GetInfo` request, so nothing below could be attempted.

| Row | Status |
|-----|--------|
| Metadata (title/artist/album/etc. from a real playing track) | pending — rig `GetInfo` fault |
| Transport (play/pause/next/previous) | pending — rig `GetInfo` fault |
| Seek | pending — rig `GetInfo` fault |
| Decimal volume (set, up/down, clamp) | pending — rig `GetInfo` fault |
| External-controller convergence (another client changes state, UHC observes it) | pending — rig `GetInfo` fault |
| Reconnect (daemon restart/network drop recovery) | pending — rig `GetInfo` fault |
| Linked-DSP coexistence | pending — rig `GetInfo` fault |

### Install / run (Beta A test build — not a stable-channel release)

This is a source-built test artifact for reviewers/testers with a Rust+Dioxus toolchain, not a
packaged release; #350's "distinct beta channel" and "installable" artifact requirements are for the
*next* step (an actual tagged pre-release build) once this PR merges and a maintainer approves
publishing one. Until then:

1. **Back up existing configuration**, if testing on a machine with a real UHC install:
   `cp -r "$UHC_CONFIG_DIR" "$UHC_CONFIG_DIR.bak"` (default config dir per `src/config`;
   override with `UHC_CONFIG_DIR` to test in complete isolation instead, as the smoke test above
   does — recommended for anyone who has a working stable install already).
2. **Build:** `git checkout feat/issue-401-hqplayer-mcp-beta-validation && make css && dx build
   --release --platform web --features web`.
3. **Run:** `cd target/dx/unified-hifi-control/release/web && PORT=8088 UHC_CONFIG_DIR=/path/to/test-config ./server`.
4. **Exercise:** enable the HQPlayer adapter and point an instance at the tester's HQPlayer 6.0.2
   Embedded host through the existing web UI/config exactly as today; drive it from the web UI, a
   knob, or an MCP client (Claude, etc.) via `http://<host>:8088/mcp`.
5. **Log capture:** the process logs to stdout/stderr (`tracing`); redirect to a file
   (`... ./server > uhc-beta.log 2>&1`) for issue reports. No credentials are ever logged by this
   path — the HQPlayer native protocol this branch touches is unauthenticated.
6. **Roll back:** stop the test binary; if a real config dir was used (not isolated), restore the
   backup from step 1; resume the tester's existing stable install/binary. Nothing this branch does
   is persisted outside the config dir it was pointed at.

**Explicit maintainer approval is required, and has not been requested or given, before any artifact
from this branch is published to any channel — beta or otherwise.**
