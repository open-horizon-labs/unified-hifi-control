# Playback device control: aim and problem weave

Session artifact for the device-control layer (power / volume / mute / input on the
amplifier a zone feeds). Produced 2026-10-12 by `/aim` and `/problem-weave`; read by
`/solution-space` and `/oh-plan`. Research input: `uhc-native-lan-client-catalog.{json,md}`.

## Aim

**Aim:** A UHC user turns the room on, sets volume, and lands on the right amplifier
input from whatever UHC surface they're already holding (ESP32 knob, Apple Watch/iOS,
web, MCP, Home Assistant), instead of reaching for the amplifier's own remote or app.

**Why it matters:** UHC already unifies transport and library across music servers.
The amplifier is not unified: zones feeding an amp the music server doesn't expose
report `volume_control: None`, power is unmodelled, physical input is unmodelled.
Users run two controllers for one room. #304 asks for power on/off from the knob.

**Current state:** Each Marantz SR6008 (the user owns two identical units) is invisible
to UHC. **Desired state:** each is a UHC *device* linked to every zone that feeds it;
linked zones show the amp's real power/volume/mute/input, accept those commands from
every surface, and reflect changes made on the Marantz remote within ~2 s.

### Mechanism (hypothesis)

Devices (native-IP adapters first; HA `media_player` entities as a fallback provider)
with stable hardware identity and one connection owner each. Users *link* zones to
devices: `zone_id → (device, source)`, many zones per device, `source` = the device's
physical input that zone arrives on. The aggregator merges: transport/now-playing from
the music provider, power/volume/mute/input from the linked device. Precedent:
`HqpZoneLinkService` (`src/adapters/hqplayer.rs`).

Hypothesis: the second remote exists because of the amplifier; if the zone owns the
amp's power/volume/input, existing surfaces already have the affordances.

### Priorities (user)

Native adapters first (SR6008 baseline). HA fallback second — most users don't run HA.
MCP and HTTP API first; UI after the data model settles.

### Decisions taken (2026-10-12)

- **O1 — published `max` on a linked zone is `min(device_max, user_ceiling)`.** The
  ceiling (#246) can only lower; unmodified clients cannot request above it. "More max
  settings are always good."
- **D2 — no automatic input switching.** Input changes only on an explicit
  `select_input` from a surface/MCP/API. The per-link `source` is kept as data (it
  tells UHC which linked zone is live on a shared amp) but nothing acts on it
  automatically. Auto-select on play/power-on: maybe later.

### Assumptions kept visible

1. Which amp a zone feeds is not derivable from discovery; the user confirms links.
2. A zone whose transport and volume come from different providers stretches
   `Zone.source` and "one backend = one prefix" — the one-way architectural decision.
3. One UHC-owned session per device must coexist with vendor apps on limited-connection
   models (Marantz Telnet: one client).
4. ~~When input auto-select fires~~ — resolved by D2: never, for now.
5. Volume scale is read from the device (SR6008: relative dB −80…+18 or absolute
   0…98, selectable on the unit; half-dB steps), never assumed.
6. UHC exports its zones to HA as `media_player` entities; an HA inbound provider must
   exclude platform `unified_hifi_control` or a zone links to its own reflection.
7. Two identical SR6008s are told apart by serial/MAC/UDN, never model name.
8. Knob C and iOS Swift decoders tolerate unknown JSON keys (unverified; iOS source
   not on disk).

### Feedback signal

Both SR6008s discovered and distinguishable; every zone feeding them linked with an
input; knob rotation on a linked zone moves the amp with readback; power-on and input
select from knob/watch/MCP; a volume change on the Marantz remote appears in UHC
within ~2 s; linked zones agree on volume; #304 closes. Checkpoints: (1) one SR6008
hardware-verified; (2) second unit proves identity and multi-device ownership;
(3) further families natively, HA fallback.

### Guardrails

- Volume safety first: no level sent on connect/reconnect; `vol_abs` refused until
  range and scale are read; clamp to range ∧ ceiling; CI regression tests per
  `docs/AGENTS.md` hard constraint 1.
- Confirmed ≠ sent: success on observed readback via the existing `CommandStatus`
  lifecycle (`src/bus/runtime.rs`), never on bytes written (#621).
- One connection owner per device inside UHC; frontends never open device sessions.
- Scope: power / volume / mute / input / per-link source. Receiver Zone 2/3, sound
  modes, Audyssey, tone, HEOS transport, grouping, DSP: later.
- API stability: new action values, additive payload fields and new routes all need
  explicit approval; `tests/fixtures/api_routes.txt` changes only after it.
- Claims per model, hardware-verified, three-state (✅ / ⛔ / 🚧). "SR6008 ✅" never
  makes "Denon/Marantz ✅".
- No GPL translation (Onkyo `aioonkyo`). Standalone parity: HA accelerates, never
  required.

---

## Problem Weave

**Current frame:** Add a device-control layer whose devices are linked to existing
zones, many zones per device, so every surface drives the amp without client changes.

**Layers used:** Data model & API contract (Opus); native device protocol; HA fallback
provider; control-surface behaviour; safety & verification (Sonnet). Five parallel
agents, identical contract, no cross-visibility. `/problem-space` was skipped (terrain
mapped during `/aim`). Coordinator fact-checked three contested claims:

- `VolumeChanged` updates only the first zone whose `output_id` matches —
  `src/aggregator.rs:321` has a `break`. Observed. Harmless today only because no two
  zones share an `output_id`; this feature would be the first to make them.
- The "#621 readback verifier" is the `CommandStatus` lifecycle
  (`src/bus/runtime.rs:100-127`; "did not publish a verified readback in time" at
  `src/api/mod.rs:794,937`, `src/knobs/routes.rs:893`). Its call sites are per-provider.
- `HqpZoneLinkService::load_links_sync` warns on parse error and leaves links empty; the
  next `save_links` overwrites the unreadable file (`src/adapters/hqplayer.rs:10157-10175`).
  Format is an unversioned `HashMap<String,String>` keyed by user-editable instance name;
  `auto_correct_links` re-points orphans to the only remaining instance.

**Status:** O1 and D2 decided; ready for `/solution-space` on SG-0.

### Shared core (agreed independently by ≥2 layers)

1. A device is a distinct entity, not a zone. `marantz:` as a `PrefixedZoneId` was
   rejected: transport-less zones would enter `/knob/zones`, the HA export, zone
   order/hide lists; HA would see each amp twice.
2. Device volume flows through the existing `volume_control` so unmodified clients
   improve (knob reads `volume_min/max/step` from it, `src/knobs/routes.rs:394-422`).
3. The merge happens in the aggregator, not the HTTP layer. `ZoneDiscovered` replaces
   the whole zone (`src/aggregator.rs:162`); writing device values into the stored zone
   would be erased on the next provider snapshot. The HQP precedent joins at
   `routes.rs:147-197` and is acceptable only for informational DSP data.
4. Identity is serial/MAC/UDN, never model, name or IP; the HQP store's defects are not
   copied.
5. Range and scale are read before any absolute write; no level on connect.
6. Confirmed ≠ sent runs through `CommandStatus` with device push as the projection.
7. Power is a device field, never a `PlaybackState` variant (an "off" variant would
   corrupt playback history and the knob's `is_playing`).
8. Input switching is explicit-intent only (D2: manual only). The explicit/ambient
   split exists structurally: surface commands enter via `/knob/control` and MCP;
   provider state via bus events.
9. Hardware transcripts precede code; two-unit tests are mandatory; ✅ is gated.
10. Some API change is unavoidable: new action values on `POST /knob/control` (schema),
    additive `ZoneInfo` fields (payload), new device/link routes (`api_routes.txt`).

### Distinct problem statements

1. **Data model:** the maintainer needs a home for device state, a merged zone view and
   a durable link table, because the model assumes one zone = one backend — today
   `ZoneDiscovered` overwrites, `VolumeChanged` stops at the first match, control
   routes by prefix (`src/knobs/routes.rs:594-663`, `src/mcp/routing.rs:80-105`).
2. **Native protocol:** the implementer needs a verified SR6008 contract (identity
   field, HTTP generation, Telnet grammar incl. half-step `MV505`, standby
   reachability, single-client behaviour) because everything is secondhand from
   `denonavr` — blocked by zero captures.
3. **HA fallback:** an HA user needs the amp they already have as an entity to be
   linkable without loops or fabricated scale, because `/api/states` carries no
   `platform`, `supported_features` is a claim, and `volume_level` is `(dB+80)/100`.
4. **Control surface:** the person holding a knob needs one-room semantics with no
   firmware change, because the verb set is closed (`src/mcp/routing.rs:284`), `None`
   collapses to `volume_type:"fixed"` (`routes.rs:395-415`), nothing marks intent.
5. **Safety:** the user needs claims made only when verified, because
   `tests/volume_safety.rs` and the capability matrix are provider-shaped.

### Interwoven S&T steps

| ID | Parent | Depth | Layer | Strategy | Tactic | Disposition | Necessity | Parallel assumption | Owner | Review trigger | Evidence | Provenance |
|---|---|---|---|---|---|---|---|---|---|---|---|---|
| W-0 | — | 0 | all | Zones can be *fed by* devices; surfaces drive the amp through fields they already read | — | — | — | — | user | — | — | root |
| W-1 | W-0 | 1 | DM | Devices have their own identity & state | `Device` entity in `AggregateState` (precedent: `hqplayer_snapshots`, `aggregator.rs:65`) | candidate | many-to-one needs one copy of truth | same lock/revision domain already holds a non-zone entity | maintainer | 2nd family needs state it can't hold | observed | DM-1, NP-9, HA-9 |
| W-1.1 | W-1 | 2 | DM/NP/SV | Identity survives IP change, tells twins apart | `DeviceId` ≠ `PrefixedZoneId`; key = serial/MAC/UDN; IP/name/model mutable; provenance recorded; no auto-correct | candidate | links persist against it | HQP keys by name + auto-corrects | maintainer | capture 1 shows no serial | observed/assumed | DM-1.1, NP-1, HA-9, SV-2 |
| W-1.2 | W-1 | 2 | DM/NP/HA/CS | Power & input are device facts | power ∈ {on, standby, unknown}; connection state separate; input = current source id + catalog | candidate | provider "playing" into standby amp must be representable | `PlaybackState` feeds history & `is_playing` | maintainer | knob can only render power via `state` | observed | DM-1.2, NP-7, HA-4, CS-2, SV-9 |
| W-1.3 | W-1 | 2 | DM/NP/HA/SV/CS | Volume carries real scale, step, ceiling | new `VolumeScale` for absolute 0–98; step 0.5; ceiling (#246, unimplemented) stored separately; **published `max = min(hw_max, ceiling)` (O1)** | candidate | HA export maps 100%→`max`; MCP default step 5.0 | `step: f32` allows 0.5 | maintainer + safety | scale changes at runtime | observed | DM-8, NP-6, HA-5, SV-3, CS-1a |
| W-2 | W-0 | 1 | DM/CS | One merged zone view every surface reads | aggregator owns links; composes provider `Zone` + linked `Device` on read; raw zones untouched | candidate | stored-zone writes are erased by `ZoneDiscovered` | aggregator owns state (ARCHITECTURE) | maintainer | merge-on-read too slow | observed | DM-2, CS-1 |
| W-2.1 | W-2 | 2 | DM/CS | Linked zones agree on volume | device readback updates Device once; replace first-match `break` with device-keyed fan-out; `output_id` device-scoped | candidate | feedback signal; current code guarantees disagreement | lint enforces prefixed output ids | maintainer | none (latent defect) | observed | DM-2.2, CS-4 |
| W-2.2 | W-2 | 2 | CS | Non-live linked zones don't pretend to be audible | device's current input vs link `source` decides live zone; flag others via existing fields | candidate | shared volume confusing otherwise | only existing flags reach unmodified clients | maintainer | client mishandles flag | assumed | CS-3a |
| W-3 | W-0 | 1 | DM/SV/CS | Links persist, tolerate absence, never auto-drop | versioned file in config dir; orphan-not-delete; parse failure refuses linking and never overwrites; `source_id` = stable input code | candidate | assumption 1 | HQP loader defect | maintainer | unify with HQP links | observed | DM-3, SV-10, CS-7 |
| W-4 | W-0 | 1 | DM/CS | Surface commands reach the device via the link | consult link table before prefix routing in `knob_control_handler`; new actions `power_on/off/toggle`, `select_input` (body schema → approval) | candidate | all surfaces use `/knob/control` | route exists | user approves | — | observed | DM-4, CS-2, CS-5 |
| W-4.1 | W-4 | 2 | DM/CS | Devices & links manageable without UI | new routes: device list/status, link list/link/unlink (`api_routes.txt` → approval); MCP: extend `hifi_control` vs new tool(s) | candidate | MCP/API first | `/hqp/zones/link` shape accepted before | user approves | — | observed | DM-4.1, CS-5, CS-7 |
| W-4.2 | W-4 | 2 | DM/CS | Read surfaces show power/input additively | optional `power`, `input`, `inputs`, `device` on `ZoneInfo` with `skip_serializing_if` (payload → approval) | candidate | clients can't offer what they can't see | `compute_zones_sha` hashes id+name only | user approves | HA strict shape check | observed/assumed | DM-5, CS-9 |
| W-5 | W-0 | 1 | DM/SV/CS/NP | "Success" means device readback | `ReadFrom::Device` in envelope; device push = projection; no-op write confirms promptly; Indeterminate never success; queue expires stale relative steps; mute as set-state | candidate | #621 class | lifecycle exists, call sites per-provider | maintainer + safety | SR6008 doesn't echo no-op (capture 4) | observed | DM-7, SV-4, SV-5, CS-6, NP-3 |
| W-6 | W-0 | 1 | CS/HA/SV | Input changes only when meant | **D2: manual `select_input` only.** Intent matrix (explicit play → switch; zone-pick/autoplay → no; conflict → surface it; power-on → link source) kept as a later option | deferred | — | explicit/ambient paths structurally distinct | user | user asks for auto-select | — | CS-3, CS-10, HA-8, SV-11 |
| W-7 | W-0 | 1 | NP/SV | Verified SR6008 contract before code | capture-first on both units: description/identity → Deviceinfo → Telnet frames on & standby (Network Control on/off) → range/scale both modes → volume/mute/power/input readback → Marantz-app contention → cable pull → reboot; replay via `tests/mock_servers/` | candidate | every ✅ and identity design depend on it | mock-server pattern exists ×5 | user + implementer | firmware differs between units | observed/assumed | NP-2, NP-10, SV-6, SV-7 |
| W-7.1 | W-7 | 2 | NP | Telnet push primary; HTTP read-only secondary | tokio TCP (LMS pattern); queries only on connect; parse PW/ZM/MV/MVMAX/MU/SI; Z2/Z3 parsed & ignored; HTTP/XML for input display names & scale mode | candidate | ~2 s needs push; names need HTTP | `ssdp-client`, `quick-xml` in Cargo.toml | implementer | no unsolicited events | inferred (denonavr `const.py`) | NP-3, NP-4, NP-5, NP-12 |
| W-7.2 | W-7 | 2 | NP | Coexist with Marantz app on one-client channel | two experiments then choose: own Telnet / HTTP-poll only / yield | deferred | users hit it otherwise | app may not use Telnet | implementer | capture 8 | assumed | NP-11 |
| W-8 | W-0 | 1 | HA | HA entities linkable, honest, loop-free | fail-closed platform exclusion via WS entity registry; per-entity three-state earned by readback; `Linear` scale, step unknown, `vol_abs` refused until ceiling set; WS `state_changed`; reuse `core_api_credentials()` | deferred (after native) | loop is silent | `tokio-tungstenite` present | HA lane | Supervisor token denied registry | observed/inferred | HA-1..8, CS-8, SV-11 |
| W-9 | W-0 | 1 | SV/DM | Safety-class modes have named CI tests; claims can't drift | `device_safety` family (wrong-amp w/ twin mocks, scale, connect, replay, mute, power-on level, input yank, ceiling bypass, stale, orphan); matrix per model w/ evidence label; lint: ✅ needs hardware fixture; ✅ gate checklist | candidate | AGENTS.md constraint 1 | `volume_safety.rs` + generated matrix | implementer; user signs off | new mode in captures | observed | SV-1, SV-3, SV-8, SV-12, DM-6 |
| W-10 | W-0 | 1 | CS/DM | Explicitly deferred | Zone2/3 outputs; LMS player-power vs amp-power precedence (#304); HQP↔device link unification; two-gain chain; power-off → pause provider? | deferred | visible, not expanded | — | user | first request | — | CS-2a, DM-1, DM-3, DM-2.1 |

### Sufficiency groups

| Group | Parent | Children | Mode | Coverage claim | Gap |
|---|---|---|---|---|---|
| SG-0 | W-0 | W-1, W-2, W-3, W-4, W-5, W-7, W-9 required; W-8 conditional on HA; W-6 deferred | all required + conditional | state home, merge, links, write path, confirmation, hardware contract, safety | `GET /events` and MQTT projection of device state unframed |
| SG-1 | W-1 | W-1.1, W-1.2, W-1.3 | all required | identity, power/input, volume units | Zone2/3 as device outputs |
| SG-2 | W-2 | W-2.1, W-2.2 | all required | shared volume, live-zone presentation | surface wording |
| SG-4 | W-4 | W-4.1, W-4.2 | all required | link CRUD, read fields | MCP tool shape open |
| SG-7 | W-7 | W-7.1, W-7.2 | 7.1 required; 7.2 by capture 8 | one owned session, push + metadata | HTTP may become primary |

### Opposition and uncertainty

- **O1 (decided):** `max = min(device_max, ceiling)`.
- **O3 — power vocabulary.** DM on/standby/unknown; NP adds unreachable and `PWON` vs
  `ZMON`; HA off/standby/on/unknown; SV-9 connection state separate. Resolution carried
  into W-1.2: power and connection orthogonal; whole-unit vs main-zone deferred with
  Zone2/3.
- **U1 — confirmation oracle is circular.** Device push is the only observer. SV wants
  a distinct query frame; NP assumes echo. Evidence: capture 4 (no-op `MV` write).
- **U2 — Telnet vs HTTP primary.** Capture 8.
- **U3 — Supervisor token can list the entity registry.** One WS call under the add-on.
- **D2 (decided):** manual input select only.
- **Resolved contradictions:** "#621 verifier not found" → `CommandStatus`. "#246 not
  in repo" → correct, unimplemented; W-1.3 creates it.

### Recommended working frame

A zone can be **fed by** a device. Devices are first-class aggregator entities with
hardware identity; links are user-confirmed, versioned, orphan-tolerant facts; the
aggregator composes a zone's effective control surface at read time so unmodified
clients receive device volume through the fields they already read; every device write
runs the existing command lifecycle with device push as its projection; nothing is
claimed ✅ without two-unit hardware transcripts. HA entities are a second kind of
device under the same contract, weaker and later.

### Alternate frames kept

- **Device-as-zone** — rejected on evidence; kept for the record.
- **Observe-only first slice** — discover, identify, link, display; no writes. De-risks
  W-1.1, W-2, W-5 before any safety-class byte. A sequencing option.
- **Per-link intent settings** — the later form of W-6 if auto-select is wanted.
- **HA-first** — user-rejected; remains the path for amps with no native adapter.

### Evidence and invalidation

- **Learn (hardware, in order):** identity fields on both units; HTTP generation;
  Telnet in standby with Network Control on/off; scale-mode readability; no-op echo;
  app contention; idle timeout.
- **Learn (clients):** knob C and iOS Swift tolerate unknown JSON keys.
- **Learn (HA, later):** Supervisor token's registry permission.
- **Synthesis is wrong if:** no stable identifier before writing (identity collapses to
  user labels); Telnet evicted by the Marantz app (push model wrong); clients reject
  additive fields (versioned payloads needed); deployments link one zone per device
  (fields-on-Zone would suffice).

### Wiring follow-up

| ID | Type | Statement | Step | Disposition | Owner |
|---|---|---|---|---|---|
| WF-1 | outcome | User drives room power/volume/input from any UHC surface without the amp's remote | W-0 | candidate | user |
| WF-2 | capability | Devices as aggregator entities with hardware identity | W-1 | candidate | maintainer |
| WF-3 | capability | User-confirmed, versioned, orphan-tolerant zone→(device, source) links | W-3 | candidate | maintainer |
| WF-4 | capability | Merged zone view; device volume through existing `volume_control` | W-2 | candidate | maintainer |
| WF-5 | capability | Device writes confirmed by device readback via `CommandStatus` | W-5 | candidate | maintainer |
| WF-6 | capability | Hardware-verified SR6008 adapter | W-7 | candidate | user + implementer |
| WF-7 | signal | Remote-originated volume change visible in UHC ≤ 2 s (p95) | W-5/W-7.1 | candidate | implementer |
| WF-8 | signal | Zones linked to one device report identical volume | W-2.1 | candidate | implementer |
| WF-9 | guardrail | No absolute write before range/scale read; no level on connect | W-1.3/W-9 | candidate | safety |
| WF-10 | guardrail | No ✅ for a device model without two-unit hardware fixtures | W-9 | candidate | user |
| WF-11 | guardrail | HA provider never offers UHC's own exported entities (fail closed) | W-8 | deferred | HA lane |
| WF-12 | metis | `VolumeChanged` fan-out stops at first matching zone (`aggregator.rs:321`) | W-2.1 | observed | — |
| WF-13 | metis | HQP link store: name-keyed, auto-corrects orphans, overwrites on parse error | W-3 | observed | — |

Plan gate: `/oh-plan` only on W-ids marked `selected` by `/solution-space`.

---

## Solution Space

**Problem:** Zones can't carry the amplifier they feed, so every surface stops at the
music server. **Key constraint:** unmodified clients must get device volume through
fields they already read; every device write is range-gated and readback-confirmed.
**Working story:** a zone is *fed by* a device; the aggregator holds devices and links
and composes the effective zone on read; device readback is the projection that
confirms writes. **Success signal:** a provider zone reporting `None` shows the
SR6008's dB volume on the knob, the ring moves the amp with readback, remote changes
appear ≤ 2 s, linked zones agree; #304 closes.

**Decision criteria:** (1) unmodified clients improve — eliminator; (2) one source of
truth — eliminator; (3) one chokepoint for range/ceiling; (4) minimal approval
surface; (5) time to first hardware-verified checkpoint; (6) no foreclosure of HA
fallback or a second family.

### Candidates

| Option | Level | Approach | Main trade-off |
|---|---|---|---|
| A | Band-aid | HQP-style enrichment at the HTTP layer; link service outside the aggregator | Stored `Zone` keeps `volume_control: None`; SSE `/events` consumers (the HA integration applies `VolumeChanged` directly) never see device volume; MCP and `/knob` need separate joins and gates. Fails criteria 2, 3. |
| B | Local optimum | Device as a zone (`marantz:` prefix) | The linked Roon/LMS zone the user holds stays `None`; transport-less zones in every list; HA sees each amp twice. Fails criterion 1. **Rejected.** |
| C | Redesign | `Device` + link table in `AggregateState`; merge-on-read; `volume_control.output_id = marantz:<serial>:main` so existing `VolumeChanged` is the readback path; link-aware routing; `CommandStatus` with device projection | Aggregator changes; fan-out fix; new routes; HQP links become the odd one out. |
| D | Reframe (sequencing) | C, observe-only first slice; writes after captures 1–10 | One slice of delay; retires identity and oracle-circularity risks before any safety-class byte. |

**Interpretive variety:** A and B share the "one zone, one backend, enrich at the edge"
premise. C changes the frame. D changes altitude on risk. A's failure is checkable
now (HA consumes SSE, not `/knob` responses).

### Risk retirement plan

| Risk | Disposition | Tempting patch this must fail | Evidence / rationale | Pivot if |
|---|---|---|---|---|
| Stable per-unit id (W-1.1) | Retired by evidence | key by IP / friendly name | capture 1 on both units: a field differs and survives reboot | none differs → user-bound identity |
| Merge keeps clients working (W-2) | Retired by evidence | write device volume into stored `Zone` | harness: `None` + linked mock → `volume_type:"db"`; **re-publish provider snapshot, volume survives** | a client field the merge can't populate → `/problem-statement` on client contract |
| Lifecycle generalises; no-op confirms (W-5) | Retired by evidence | short-circuit on cache == requested | cache X, device Y, write X → confirm against device; no echo → Indeterminate, never success | needs cache to confirm → redesign confirmation |
| SR6008 echoes no-op `MV` (U1) | Retired by evidence | assume echo | capture 4 | no echo → explicit `MV?` after write |
| Telnet survives Marantz app (U2) | Retired by evidence | hold and hope | capture 8, both orders | evicted → HTTP primary |
| One ceiling/range chokepoint (W-1.3, W-9) | Retired by evidence | clamp only in knob handler | above-ceiling via `/knob/control` *and* MCP → both clamped; zero bytes above on mock socket | second unclamped path → gate into adapter entry |
| Fan-out `break` (W-2.1) | Retired by evidence | adapter emits one event per zone | two linked zones, one event → both updated | — |
| Decoders ignore unknown keys (W-4.2) | Retired (knob, HA); Accepted (iOS: `JSONDecoder` default) | "it's JSON" | harness with unknown fields vs knob decoder and HA `api.py` | reject → versioned payload |
| Standby wake needs Network Control (W-7) | Retired by evidence | `PWON` exists → ✅ | capture 3 with setting on/off | unreachable → `power_on` ⛔ / WOL |
| Scale mode readable (W-1.3) | Retired by evidence | infer from `MV` magnitude | capture 3 | not readable → relative-only |
| Many-to-one needed (W-1) | Accepted | — | two amps, several providers; map shape is free | — |
| HQP link unification (W-10) | Accepted | — | out of scope | — |
| API approval (W-4, 4.1, 4.2) | Triggered (human) | — | user approves before implementation | not approved → observe-only via existing routes |
| C is a one-way door | Triggered | — | `/dissent` before `/execute` | — |

### Recommendation

**Selected: C, executed in D's order.** Level: redesign.

Rationale: A and B each fail an eliminator on checkable facts. C satisfies both with
the fewest new concepts — a device's main output is an `output_id` on the existing
`VolumeControl`, so readback rides `VolumeChanged` and `CommandStatus` unchanged.
D's ordering retires the two paper-unretirable risks first.

Accepted trade-offs: HQP links stay separate; `Zone.source` remains the transport
provider with device provenance beside it; slice 1 displays without control; no
power/input bus event yet (named gap).

### S&T selection

| Step | Disposition | Note |
|---|---|---|
| W-1, W-1.1, W-1.2, W-1.3 | selected | 1.1 gated by capture 1; 1.3 carries O1 |
| W-2, W-2.1 | selected | 2.1 also a standalone bug |
| W-2.2 | deferred | data ships in W-4.2; flagging decided after slice 1 |
| W-3 | selected | versioned, orphan-tolerant, never-overwrite |
| W-4 | selected, slice 2, approval | new action values |
| W-4.1 | selected, approval | device list/status; link CRUD. MCP recommendation: extend `hifi_control` for power/input + one read tool for devices/links |
| W-4.2 | selected, approval | additive `ZoneInfo` fields; **child gap:** bus event for power/input to `GET /events` |
| W-5 | selected, slice 2 | `ReadFrom::Device`; device push as projection |
| W-6 | deferred | D2 |
| W-7, W-7.1 | selected, slice 0 | captures start now, no approval needed |
| W-7.2 | deferred (decision) | experiments inside W-7; choice after capture 8 |
| W-8 | deferred | HA after native |
| W-9 | selected | scaffold slice 0, full slice 2 |
| W-10 | deferred | — |
| (B) device-as-zone | rejected | — |

**Sufficiency:** SG-0 covered for the native path. Named gap: no bus event projects
device power/input changes to SSE; slice 1 is safe without it (polling fallbacks),
slice 2 is not done without it.

### Slices

- **Slice 0 (now, no approval):** captures 1–10 on both SR6008s; twin-mock
  `device_safety` scaffold; fix `aggregator.rs:321` as a standalone bug.
- **Slice 1 (approval: read routes + fields):** `Device` entity, `DeviceId`, link
  store, merge-on-read, display of power/volume/input, Telnet/HTTP read path.
- **Slice 2 (approval: action values):** writes, lifecycle with device projection,
  full safety suite, power/input bus event, ✅ gate, on-device sign-off.

### Execution handoff

- **Preserve:** clients read device volume through `volume_control`; aggregator is the
  only link resolver; `max = min(hw_max, ceiling)`; manual input select; no ✅ without
  two-unit fixtures.
- **Verify via:** the harness tests in the risk table; on hardware, remote → UHC ≤ 2 s.
- **Invalidated if:** no distinguishing id; merge can't satisfy a client field;
  lifecycle needs cache to confirm.
- **Stop/pivot:** Telnet evicted → HTTP primary; unreachable in standby → ⛔;
  scale unreadable → relative-only; decoder rejects → versioned payload.
- **Needs human:** API approvals (W-4, 4.1, 4.2); MCP tool shape; on-device sign-off.
- **Next:** `/dissent` (one-way door), then `/oh-plan` on selected steps.

---

## Plan
**Updated:** 2026-10-12

Epic: #774 — native playback device control (power/volume/mute/input) linked to zones.
API routes and MCP tool additions are scoped to this epic; the user applies
`api-change-approved` to #782 and #786. `/dissent` on Option C was offered and not
yet run; the one-way-door steps (#778, #780, #782) are where it would bite.

| S&T Step | Disposition | Issue/Epic | Parent Step | Depends On | Slice |
|---|---|---|---|---|---|
| W-0 | selected | #774 (epic) | — | — | — |
| W-2.1 | selected | #775 | W-2 | none | 0 |
| W-7 | selected | #776 | W-0 | none | 0 |
| W-9 (scaffold) / W-7.1 (fixtures) | selected | #777 | W-0 / W-7 | #776 (soft) | 0 |
| W-1, W-1.1, W-1.2, W-1.3 | selected | #778 | W-0 | none | 1 |
| W-3 | selected | #779 | W-0 | #778 | 1 |
| W-2 | selected | #780 | W-0 | #775, #778, #779 | 1 |
| W-7.1 (read) | selected | #781 | W-7 | #776, #777, #778 | 1 |
| W-4.1 (read/CRUD), W-4.2 | selected (approval) | #782 | W-4 | #778, #779, #780 | 1 |
| W-4.1 (MCP), DM-6 | selected | #783 | W-4 | #782 | 1 |
| W-5, W-7.1 (write) | selected | #784 | W-0 / W-7 | #780, #781 | 2 |
| W-4 (volume/mute) | selected | #785 | W-0 | #784 | 2 |
| W-4 (new actions), W-4.1 (MCP extend) | selected (approval) | #786 | W-0 / W-4 | #785 | 2 |
| W-4.2 child gap (SSE) | selected | #787 | W-4.2 | #780 | 2 |
| W-9 (full) | selected | #788 | W-0 | #786, #787 | 2 |
| W-2.2 | deferred | — | W-2 | — | — |
| W-6 | deferred (D2) | — | W-0 | — | — |
| W-7.2 | deferred (capture 8) | — | W-7 | — | — |
| W-8 | deferred | — | W-0 | — | — |
| W-10 | deferred | — | W-0 | — | — |
| (B) device-as-zone | rejected | — | — | — | — |

Sufficiency check: SG-0, SG-1, SG-2, SG-4, SG-7 each have every required selected
child planned. The SG-4 named gap (SSE projection) is #787.

---

## Execute

**Branch:** `feat/issue-774-device-control` (off `v4`). One PR for the epic.

### Decisions taken during execution

- **Hardware first, read-only.** Captures 1–3 ran against both SR6008s before code
  (`tests/fixtures/devices/marantz/sr6008/`). They settled identity (`serialNumber` = MAC, unique
  per unit), volume encoding (`MV = dB + 80`, half step as a third digit), the unit's own
  maximum (`MVMAX 70` = -10 dB, Telnet-only), the interleaved `@`-protocol, and that a unit in
  standby answers HTTP but refuses Telnet. No write was sent to hardware.
- **Volume is published in dB.** The "absolute 0–98" front-panel scale is display-only
  (`DeviceVolume.display`); no new `VolumeScale` variant was needed, so clients that already
  render dB (HQPlayer zones) render the amplifier unchanged.
- **Confirmation is a device-service lifecycle, not `CommandGateway`.** `CommandGateway` targets
  `PrefixedZoneId`s and commits zone projections; devices are deliberately not zones (option B
  rejected), so routing them through it would reintroduce device-as-zone ids. The same semantics
  are implemented in `DeviceService::control`: subscribe before dispatch, require a readback
  *newer than dispatch*, report `confirmed` / `confirmed_different` / error `DEVICE_UNCONFIRMED`.
  The driver always re-queries after a write, so a no-op write is confirmed against the device.
- **Envelope vocabulary unchanged.** Device results ride `data` (`confirmation`, `observed`,
  `maturity`) instead of a new `Outcome::Verified`/`ReadFrom::Device`, per `envelope.rs`'s rule
  that downstream issues attach payloads to `data`.
- **MCP power/input live in `hifi_devices`, not `hifi_control`.** The user scoped the MCP surface
  to this epic's tool; `hifi_control` keeps its closed transport vocabulary and gains only one
  hook: volume actions on a *linked* zone go to the device (with the device step, not 5).
- **Telnet primary with real HTTP fallback.** Capture 8 (app contention) was not run, but the
  bedroom unit refusing Telnet in standby made the fallback mandatory anyway: HTTP polling for
  state, HTTP `goform` commands for power/mute/input/volume, and volume refused while `MVMAX` is
  unknown.
- **Alpha everywhere.** `maturity: "alpha"` on every device payload, the Settings toggle wears the
  Alpha badge, `hifi_devices`' description starts "Alpha capability", HA entities expose
  `linked_device_maturity`.

### Risk retirement

| Risk | Status | Tempting patch the check fails | Evidence |
|---|---|---|---|
| Stable per-unit id (W-1.1) | Retired | key by IP/name | captures: two distinct serials; `captured_descriptions_give_two_identities_for_two_identical_models` |
| Merge keeps clients working (W-2) | Retired | write device volume into stored zone | `linked_zone_publishes_device_volume_and_survives_a_provider_resnapshot` (mutation-tested) |
| No-op confirms against device (W-5, #621) | Retired | short-circuit on cache | `critical_noop_write_confirms_without_timeout` (mutation-tested) |
| Unconfirmed ≠ success | Retired | treat send as success | `unacknowledged_write_is_reported_unconfirmed_not_success` |
| One ceiling chokepoint (W-1.3) | Retired | clamp only in knob path | `critical_ceiling_enforced_via_knob_and_mcp` (mutation-tested) |
| No level on connect | Retired | restore cached level on reconnect | `critical_no_level_sent_on_connect_or_reconnect` (mutation-tested) |
| Wrong-amp routing | Retired | "the only Marantz" | `critical_zone_a_volume_never_reaches_amp_b` (twin mocks, identical model) |
| Fan-out `break` (#775) | Retired | per-zone events from adapter | `volume_changed_updates_every_zone_sharing_the_output` (failed first, then fixed) |
| Clients tolerate additive fields | Retired | "it's JSON" | Rust `uhckit_contract` + Swift `ContractTests` (8/8) decode the fixture with `device` |
| Scale readable (W-1.3) | Retired | infer from `MV` magnitude | XML carries dB + `VolumeDisplay`; encoder round-trips the captured value |
| Standby wake (capture 6) | **Accepted, pending hardware** | — | HTTP `formiPhoneAppPower` path implemented; not sent to a unit |
| Telnet vs Marantz app (U2, capture 8) | **Accepted, pending hardware** | — | fallback implemented; contention not measured |
| Write path on hardware | **Accepted, pending hardware** | — | every write claim is 🚧 in the generated AGENTS.md table until a write capture lands |
| HA linked device | Retired | — | HA suite 64/64 incl. linked-device tests |
| Claims drift | Retired | hand-edited ✅ | `tests/device_claims.rs` generates the table and fails a verified claim without a capture |

### Course correction: the full adapter set (2026-10-12)

The first pass built only the hardware the maintainer owns. The scope is the catalog: eleven more
families, built to spec and labelled alpha / implementation-derived. What changed:

- **Volume is native-scale.** `DeviceVolume` carries `scale` (`decibel`, `percentage`, `linear`,
  new `steps` for device ranges like MusicCast 0–161). The service clamps and quantises in the
  device's own units; nothing converts between scales. The knob renders `steps` as a number.
- **Protocol full scale is the hardware maximum** (100 for percentage devices, Arcam 99, the
  documented dB max for Trinnov/Lyngdorf/Anthem). Only when a maximum genuinely cannot be known
  (Denon over HTTP, Home Assistant's 0–1) does the user ceiling become the bound; with neither,
  every level is refused.
- **One adapter set.** `DeviceAdapterSet` (Startable `devices`) owns every `DeviceFamily`; one
  Alpha Settings toggle. Families without discovery (Anthem, Trinnov) use `manual_devices`.
- **Families:** Denon/Marantz, WiiM, Sonos, MusicCast, BluOS, StreamMagic, Trinnov, eISCP
  (from the published ISCP v1.15 spec, not the GPL client), Arcam, Hegel, Lyngdorf, Anthem, and
  Home Assistant `media_player` entities (#790). Built by five Sonnet workers against
  `docs/device-family-brief.md`, integrated and gated here.
- **Push vs polling.** Lyngdorf and Anthem poll (5 s): their push modes are enabled by writing a
  persistent setup option, which the queries-only-on-connect rule excludes.
- **Sources.** Each worker reported which references it actually fetched; citations not fetched
  are marked in code comments. Every non-SR6008 claim is `implementation-derived`, and
  `tests/device_claims.rs` fails any other evidence label.
