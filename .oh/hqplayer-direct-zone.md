# HQPlayer direct zone: now-playing, transport, seek, safe volume (#328)

**OH:** 80222d6d · **Issue:** [#328](https://github.com/open-horizon-labs/unified-hifi-control/issues/328) ·
**Parent epic:** #313 · **Base branch:** `feat/issue-329-hqplayer-immediate-command` (PR #382)

### Inputs read before choosing an approach

`.oh/adaptive-interaction-plane.md` is the **program session** for epic #313 and is *not* tracked on
this branch — it lives in the main checkout and `.oh/adaptive-producer-contract.md:5` names it
"read-only for this session". It was read there. Recorded because a document that is absent from the
branch it governs is easy to skip, and its constraints are the ones this work is judged against:

* *"Aggregator owns authoritative state"* and *"Devices never call adapters directly"* — hard.
* *"Existing API changes require explicit approval"* — hard: *"New protocols need approved additive
  routes/contracts."*
* *"Provider capabilities differ … Do not manufacture parity"* — hard, and its success criterion is
  the sentence this issue turns on: **"unsupported operations are never advertised."**
* *"Project all advertised capabilities into web, HTTP, device UI, and MCP through one semantic
  execution path; surface-specific subsets are allowed, contradictory semantics are not."*
* Its chosen Option D (unified adaptive interaction plane) sequences *"HQPlayer proving producer"*
  ahead of the client-facing plane. #328 is that proving step for the **legacy zone projection**;
  #331 is where the adaptive surface arrives. This is why Candidate D below is rejected as
  sequencing rather than direction.

Also read and unedited: [`.oh/adaptive-producer-contract.md`](adaptive-producer-contract.md) (#323
producer document v1 + compatibility policy), [`.oh/adaptive-publication.md`](adaptive-publication.md)
(#324 bus + aggregator publication), [`.oh/hqplayer-evidence-ledger.md`](hqplayer-evidence-ledger.md)
(#341 protocol evidence authority), `docs/ARCHITECTURE.md`, `AGENTS.md`.

---

## Aim

A direct HQPlayer user picks a `hqplayer:` zone on a knob, in the web UI, or through MCP and it
behaves like every other everyday zone: it says what is playing, it says what it can do, transport
and volume land on *that* daemon, and what it reports keeps up with the daemon even when somebody
else is driving.

The behaviour change to look for: a user stops keeping a Roon or LMS zone selected "because the
HQPlayer one doesn't really work", and stops reaching for the relay PC to change the level.

Not the aim: making HQPlayer a library browser, and not making a zone *look* complete when the
daemon has not said enough to make it complete.

---

## Problem Statement

**Reframed (from the issue):** direct HQPlayer users need a complete everyday playback zone on UHC
surfaces. Today the zone exists but is not truthful, and the untruths are individually small and
jointly disqualifying.

What the code actually does, read before choosing an approach:

| # | Defect | Location |
|---|--------|----------|
| 1 | **Every HQPlayer control command is executed against Roon.** `knob_control_handler` dispatches `lms:`, `openhome:`, `upnp:` explicitly and then *falls through* to `control_roon` for everything else — including `hqplayer:`. The Roon adapter is handed `hqplayer:Living Room` with the prefix stripped, and either fails or, worse, matches an unrelated Roon zone id. | `src/knobs/routes.rs:533-554` |
| 2 | **The HQPlayer adapter toggle does not filter HQPlayer zones.** The zone filter tests the prefix `hqp:`; zones are published as `hqplayer:`. So the test never matches, HQPlayer zones land in the `_ => true` arm, and disabling the adapter in settings leaves its zones listed. | `src/knobs/routes.rs:165` |
| 3 | **Transport capability is asserted, not observed.** `is_seekable`, `is_next_allowed`, `is_previous_allowed` are hard-coded `true` for every HQPlayer zone in every state, including stopped with nothing loaded. | `src/adapters/hqplayer.rs:7174-7179` |
| 4 | **Output-domain facts are published as track metadata.** `TrackMetadata.bit_depth` is filled from `Status.active_bits` (the DAC's output depth) and `format` from `Status.active_mode` (the output mode, which under `[source]` reads back the literal string `[source]`). The source-domain depth the daemon *does* supply, on the `metadata` child, is dropped. | `src/adapters/hqplayer.rs:7146-7155` |
| 5 | **A lost `VolumeRange` reply erases the volume capability.** `ensure_volume_range` answers a transient failure with `VolumeRange::default()`, whose `enabled` is `false`; `hqp_status_to_zone` maps `!enabled` to `volume_control: None`. One dropped reply and a zone that has a working −60…0 dB control reports as fixed-volume. | `src/adapters/hqplayer.rs:3255-3258`, `7120-7133` |
| 6 | **Mute is published as a constant.** `is_muted: false`, with the comment "HQPlayer doesn't report mute separately". It does not report a *flag*; `VolumeMute` is a verified absolute move to the range floor (HQP, #322 live), so mute is observable — as the level sitting at `min_db`. | `src/adapters/hqplayer.rs:7127` |
| 7 | **A direct zone can be linked to itself as a DSP enrichment target.** `link_zone` accepts any `zone_id` string, so `hqplayer:A` → instance `A` is storable, after which the zone carries a `dsp` block pointing at its own control path: two routes, one daemon, no disambiguation. | `src/adapters/hqplayer.rs:7783` |
| 8 | **A missing volume value defaults to 50.** The `vol_abs` arm ends `.unwrap_or(50.0)`. For a dB zone +50 dB is above every possible maximum, and the existing safety lint does not see it — that lint scans `src/adapters` only, and matches the pattern `.value.unwrap_or(50.0)`, not `.as_f64()).unwrap_or(50.0)`. | `src/knobs/routes.rs:610`, `tests/architecture_lint.rs:787-829` |

What is **already** right and must not be re-solved: the 2 s coherent-observation poll loop
(#162/#369) republishes the zone continuously, so "changes made by another controller" is a
*verification* obligation here, not an implementation one; `knob_now_playing_handler` already reads
zone state from `ZoneAggregator` and no surface reads adapter state; `HqpStatus.volume_db` and
`VolumeRange.{min,max,step}_db` already carry exact decimal dB internally (#322/#347).

### Constraints treated as real

* **No public HTTP/MCP endpoint, request schema, or response schema may change** without explicit
  user approval, and `tests/fixtures/api_routes.txt` may not be edited. This is the binding
  constraint on the design and it is what forces the `serde(skip)` internal-field approach below.
* **No surface may query an adapter for state** (`docs/ARCHITECTURE.md`,
  `tests/architecture_lint.rs`). Commands to adapters from `_control_handler` are the established
  and allowed pattern; state reads are not.
* **No unevidenced protocol claim.** `.oh/hqplayer-evidence-ledger.md` is the authority, and a
  grep of the whole repository finds **no** evidence for `composer`, `performer`, `albumartist`,
  `genre`, `date` or `uri` as `Status/metadata` attribute *spellings*. Only `artist`, `album`,
  `song`, `samplerate`, `bits`, `channels`, `bitrate` are in the corpus.
* **Hermetic only.** No connection to a live HQPlayer host from this branch.

---

## Solution Space

### Candidate A — Band-aid: add a `hqplayer:` arm to the router

Add the missing `else if req.zone_id.starts_with("hqplayer:")` branch, mapping actions onto the
adapter methods that already exist.

* Solves the stated problem: **the routing half only** (defect 1). Every other defect is in the
  *projection* — the zone would still advertise seek on a stopped zone, still publish the DAC's bit
  depth as the track's, still lose its volume control on a dropped reply.
* Cost: very low. Second-order: the worst outcome available here, because commands start landing on
  the right daemon while the capability flags stay false. A knob that now really does send `Seek` to
  HQPlayer, told by the same server that seek is always allowed, is a *regression* in user-visible
  correctness even though every individual change is an improvement.

### Candidate B — Local optimum: fix the router and the projection together

Add the `hqplayer:` arm **and** make `hqp_status_to_zone` derive every capability, level and
metadata field from the observation instead of asserting it: capability from observed state,
source-domain metadata from the `metadata` child, mute from the floor comparison, volume capability
retained across a transient read failure.

* Solves the stated problem: **yes**, for every acceptance criterion whose data has somewhere to go
  in the existing schemas.
* Cost: medium, concentrated in two files plus a link-service guard.
* Second-order: the router and the projection have to agree about what "allowed" means, and nothing
  structural makes them agree — they are two functions in two modules. Mitigated by deriving the
  router's refusals from the *aggregator's published zone*, so the flag the client was given is
  literally the flag the command is checked against. That is not a new abstraction; it is the
  aggregator already being the single source of truth.

### Candidate C — Reframe: a typed direct-zone projection module

Extract an `hqplayer::direct_zone` module owning one function
`observation -> (Zone, allowed_actions)`, with `AllowedActions` a type the router consumes, so
"advertised" and "permitted" are one value by construction rather than by agreement.

* Solves the stated problem: yes, and closes B's residual structurally.
* Cost: high. `AllowedActions` has to cross from `src/adapters` to `src/knobs`, and the only way to
  carry it is on the `Zone` the aggregator stores — which is `BusEvent::ZoneDiscovered`'s payload,
  serialised verbatim into `GET /events`. **So the reframe requires a public response-schema change
  and is therefore not available without approval.** A parallel side-channel keyed by zone id would
  avoid the schema change and reintroduce exactly the drift the reframe exists to remove, on a path
  the aggregator does not own.
* Rejected on the constraint, not on the merits. Recorded as the shape to revisit if #331 opens the
  adaptive surface, where a typed capability set has a legitimate home.

### Candidate D — Redesign: route direct HQPlayer transport through the #329 immediate-command actor

Model transport and volume as adaptive commands and submit them to
`HqpImmediateCommandService`, gaining revision fencing, correlation-id dedup, operation records and
supersession for free.

* Solves the stated problem: yes, and it is where this ends up eventually.
* Cost: highest. The #329 vocabulary is `Mode`/`Filter1x`/`FilterNx`/`Shaper`/`Rate` — declarative
  *settings* with a `desired`/`observed` lane and a readback. Transport has no desired lane (there
  is nothing to stage), and its "readback" is a state transition, not a value comparison. Adding
  transport to that vocabulary means extending the producer document's control vocabulary, which is
  the #323 v1 contract under `.oh/adaptive-producer-contract.md` and its compatibility policy.
* Second-order: it also inherits #329's *pending* work. Doing it here couples a user-visible
  everyday-zone fix to an unlanded contract extension on a stacked branch. **Rejected as
  sequencing, not as direction** — #331 is where the adaptive surface arrives, and this is the note
  to read then.

### Chosen: **B**, with C's invariant enforced by test rather than by type

Level: **local optimum**. The reframe is the better design and the constraint that blocks it is a
real one, so the honest move is to take B and pay for C's guarantee in verification: the router
resolves the zone from `ZoneAggregator` and refuses on the *published* flag, and a test asserts the
two cannot disagree by driving both through the same daemon observation.

Consequences accepted:

1. `Status/metadata` is parsed **generically** — every attribute of the child is read into a map,
   and known keys are mapped onto typed fields. This makes no claim that any given spelling exists:
   an attribute is published when the daemon supplies it and absent otherwise. It is the only
   parser shape that satisfies "parse composer/genre where present" without adding an unevidenced
   protocol claim to a repository whose ledger lint exists to prevent exactly that.
2. New metadata reaches surfaces only through fields that **already exist** on `NowPlaying` /
   `TrackMetadata` (`composer`, `genre`, `sample_rate`, `bit_depth`, `bitrate`). New internal fields
   on `HqpStatus` are `#[serde(skip)]`, following the precedent already set for `title`/`artist`/
   `album` on that struct. No response payload changes shape.
3. **URI cannot be published.** No existing field on any published type can carry it. See Known
   limitations for the exact approved change it would need.

---

## Execute

**Status:** complete (draft PR open, not merged) · **Updated:** 2026-07-31

Test-first throughout: every behavioural change below has a client-expectation test that was run
and observed to fail against the unmodified tree before the implementation existed. RED evidence is
in [Verification evidence](#verification-evidence).

### Client-first failing tests (new file `tests/hqplayer_direct_zone.rs`)

The file is organised by the client whose expectation it encodes, not by the module it exercises.

| Group | Client expectation |
|-------|--------------------|
| `routing` | A knob that posts `{zone_id: "hqplayer:…", action: …}` reaches *that* HQPlayer daemon and nothing else — asserted on the mock daemon's received-request log, and by a Roon adapter that would have to have been called and was not. An unknown *prefixed* zone id is refused rather than silently sent to Roon; a legacy unprefixed id still goes to Roon. |
| `capability` | The flags the client is given match the state the daemon is in: no seek without a duration, no next/previous with nothing loaded, no pause when not playing. |
| `metadata` | Title/artist/album/composer/genre appear when the daemon supplies them; source-domain rate and depth come from the source, not from the DAC; a malformed `metadata` child loses the metadata and keeps the transport state. |
| `stale_state` | Stopping clears now-playing, seekability and next/previous; an observed fixed-volume transition clears the volume control; a *transient* read failure does not; zone identity survives a reconnect. |
| `volume` | Decimal dB survives end to end; a missing or unparseable level is refused rather than defaulted; out-of-range levels clamp to the observed range; the daemon is never sent a volume command when it has no volume control. |
| `reconciliation` | A change made by another controller is republished to the aggregator without any UHC command being issued. |
| `dsp_boundary` | A direct zone carries no `dsp` block; linking a `hqplayer:` zone is refused; a linked Roon zone keeps its enrichment. |

### Implementation boundary

Four files. Nothing else is touched.

* `src/adapters/hqplayer.rs` — generic `metadata`-child attribute parse; `HqpStatus` internal
  fields; `hqp_status_to_zone` capability/metadata/volume derivation; `ensure_volume_range`
  retains the last observed capability; `HqpZoneLinkService::link_zone` refuses `hqplayer:` ids.
* `src/knobs/routes.rs` — `hqplayer:` routing arm; prefix-fallthrough closed; `hqp:` → `hqplayer:`
  filter fix; no `dsp` block on direct zones.
* `tests/hqplayer_direct_zone.rs` — new.
* `tests/volume_safety.rs` — lint extension covering the direct-zone volume path.

Deliberately **not** touched: `tests/fixtures/api_routes.txt`, `src/api/mod.rs`, `src/mcp/mod.rs`,
`src/bus/events.rs`, `src/adaptive/**`, `src/producers/**`.

---

## Known limitations

1. **URI is not published (AC1, partially not met).** Parsed from the `metadata` child when
   supplied and used as a track-loaded signal for capability, but no published type has a field
   for it. **Exact approved change required:** add
   `#[serde(skip_serializing_if = "Option::is_none")] pub uri: Option<String>` to
   `crate::bus::NowPlaying` (`src/bus/events.rs`). Reach: `ZoneDiscovered` / `ZoneUpdated` payloads
   on the `GET /events` SSE stream only — no HTTP response body gains a field, and with
   `skip_serializing_if` no payload changes at all unless HQPlayer supplies a URI. Not made.
2. **Performer, album artist and date are not published (AC1 addendum, not met).** Same cause,
   same shape of change; they would need `TrackMetadata` fields. Not made, and not parsed either,
   because parsing a field nothing can publish is dead weight.
3. **Album art is explicitly represented unavailable (AC9, met by the "explicitly unavailable"
   arm).** `image_key` stays `None` for HQPlayer zones and `/knob/now_playing/image` returns its
   placeholder. Native `LibraryPicture` needs binary framing on a connection this client treats as
   XML-only, plus caps, auth and cache policy — the issue's own status-stream boundary says not to
   issue picture commands before those are proven. Nothing here claims art is coming.
4. **`play_pause` resolves against aggregator state, which can be up to one poll interval stale
   (2 s default).** A `play_pause` issued inside that window can resolve to the direction the user
   did not want. Bounded, and the alternative — a synchronous pre-read of daemon state on the
   command path — is a surface reading adapter state, which the architecture forbids. Recorded
   rather than mitigated.
5. **`vol_up`/`vol_down` are absolute writes computed from the aggregator's last observed level**,
   for the same staleness reason, so a rapid knob spin can compress. HQPlayer's own
   `VolumeUp`/`VolumeDown` would avoid it but use the daemon's step, which the verified sample does
   not even send — and it would ignore the user's `volume_step_override`. Traded deliberately.
6. **Mute is one-way.** `VolumeMute` is a verified absolute move to the range floor with no mute
   flag and no daemon-side unmute; `is_muted` is therefore derived as *level at floor*. Unmute is a
   plain level write, so an "unmute" that restores a remembered level is not advertised, because
   its result would not be observable as unmute. UHC stores no pre-mute level.
7. **Adaptive-volume mode is reported but not modelled as a mode.** `VolumeRange.adaptive` is
   observed and reaches the internal snapshot; the direct zone does not distinguish an
   adaptive-mode level from a user-set one, so a level the daemon moved on its own reads as a
   user's level. Making that distinction visible needs a field. Recorded against the issue's
   "adaptive-volume mode may be stored as a mode, not a level" criterion, which is **not met**.
8. **No live verification.** Hermetic wire/model fixtures only. The 6.0.2 rig
   (`.oh/hqplayer-evidence-ledger.md`, and the rig-side faults recorded against #337) was not
   touched from this branch, per the task constraint. Every claim below is a hermetic claim.
9. **Mode-switch qualification during playback (issue's beta/dev criterion) is not addressed.**
   It is a #347/#375 setter concern, not a direct-zone projection concern, and nothing here
   changes it.

---

## Verification evidence

Filled in as each gate runs; see the PR's Execute-checkpoint and Review comments for the transcript
excerpts.

| Gate | Command | Result |
|------|---------|--------|
| **RED, targeted** | `cargo test --test hqplayer_direct_zone` @ `74b24d6` | **33 passed, 19 failed** — tests + seams only, no behaviour |
| GREEN, targeted | `cargo test --test hqplayer_direct_zone` | **56 passed, 0 failed** |
| Conformance | `cargo test --test hqplayer_conformance` | **290 passed, 0 failed** |
| Volume safety | `cargo test --test volume_safety` | **16 passed** (2 new, both proven non-vacuous by mutation) |
| API contract | `cargo test --test api_contract` | **2 passed**; `api_routes.txt` byte-identical |
| Whole suite | `cargo test --all-features` | **1,250 passed, 0 failed, 13 ignored** across 32 targets |
| Clippy | `cargo clippy --all-targets --all-features -- -D warnings` | **Not green, and not made green.** 397 pre-existing errors at base `a178b76` (the #338 backlog). One new finding introduced and fixed. Findings in `src/knobs/routes.rs`: **0**. In `src/adapters/hqplayer.rs`: **3**, all pre-existing in its `#[cfg(test)]` module, none on an edited line. |
| Format | `cargo fmt --all --check` | **clean** |
| Release UI | `dx build --release --platform web --features web` | **Client + Server build completed successfully** |
| Live | — | **Not run.** Hermetic fixtures only, per constraint. |

### Environment faults hit on the way (not code defects)

* `public/tailwind.css` is a gitignored build artifact and absent from a fresh worktree; nothing
  compiles until `make css` generates it.
* `dx build` first failed with `Missing rust target wasm32-unknown-unknown`. Two causes stacked:
  `sccache` (set as `rustc-wrapper` in `~/.cargo/config.toml`) corrupts `dx`'s rustc probe, and
  `/opt/homebrew/bin/rustc` 1.93.1 — which has no wasm std — shadows the rustup `stable` toolchain
  that does. Green with `RUSTC_WRAPPER=` and the rustup `stable` bin ahead on `PATH`. Same family as
  the rig-side faults recorded against #337: the tree was fine and the host was not.

---

## Review pass — what falsification actually found

Four defects, all in code written earlier in this same session. Each is now pinned by a named test.

1. **The volume-step safety floor coarsened a finer reported step.** The floor was applied as
   `.max(MIN_PUBLISHED_VOLUME_STEP_DB)` *over the reported value*, so a daemon offering 0.1 dB was
   published as offering 0.5 dB. A fabrication in the opposite direction from the one the floor
   exists to prevent, and the most embarrassing of the four: the fix for "never publish an unusable
   step" quietly became "never publish an accurate one". Now the floor applies only when nothing
   usable was reported. → `a_finer_reported_step_is_not_coarsened_by_the_safety_floor`
2. **Float representation error reached the wire.** The published zone carries level and step as
   `f32`; widening them back to `f64` to add turned `-23.5 + 0.1` into `-23.399999998509884`, and
   `set_volume_db` formats with `{}`. Fixed by quantising to 0.01 dB — below audibility and below the
   finest observed step, so it removes noise without quantising away a real distinction. →
   `a_computed_level_reaches_the_wire_without_float_noise`
3. **A raw `>` in an attribute value truncated the metadata scan.** XML forbids `<` and `&` in
   attribute values but *permits a bare `>`*, so `song="a/>b"` is well-formed; the plain search for
   the child's `/>` cut inside the value and silently lost the title, the album *and* the source bit
   depth. The terminator search is now quote-aware. Note the direction of the miss: this was
   introduced by the very bound added to stop a malformed child swallowing `</Status>`. →
   `a_raw_terminator_inside_an_attribute_value_does_not_truncate_the_metadata_scan`
4. **Transport was accepted for a withdrawn zone.** `play`/`pause`/`stop` did not consult the
   published zone at all, so they answered `{"ok":true}` for a zone the aggregator had withdrawn,
   purely because the instance remained in the manager's map. Now every arm requires the published
   zone. → `transport_is_refused_for_a_zone_the_aggregator_has_withdrawn`

Two hypotheses the pass **failed** to confirm, recorded because a review that only reports hits is
not evidence of much:

* That `trim_start_matches("hqplayer:")` mishandles an instance name containing a colon. It does
  not — `hqplayer:a:b` yields `a:b`, which is the correct key. (It *would* strip a repeated prefix,
  but an instance literally named `hqplayer:x` is not a reachable configuration.)
* That an attribute value containing an encoded `&gt;` truncates the scan. It does not: the raw
  document contains `&gt;`, not `>`. Only the *unencoded* form was a hazard, which is what sharpened
  the probe into finding 3.

### What the review did not close

* The one-poll-interval staleness on `play_pause` and relative volume (Known limitations 4 and 5).
  Confirmed reachable; not fixed, because both available fixes are forbidden — a surface reading
  adapter state, or a daemon-side compare-and-set the protocol lacks.
* Whether any real daemon populates `composer`/`genre`/`uri`. Unfalsifiable hermetically, and
  deliberately not claimed either way: the parser reads what arrives.
