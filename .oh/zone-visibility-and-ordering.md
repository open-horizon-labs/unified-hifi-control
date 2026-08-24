# Zone visibility and ordering

**Branch:** `feat/zone-visibility-sorting`
**Base:** `v3` @ `18dd880`
**Stage:** Aim (no source change yet)
**Origin:** User report — Docker install, Roon-only: "it should respect private zones and exclude
them from the zones list. Or, at least, it should be configurable somehow." Plus standing requests
for grouping/sorting.

---

## Aim

**Aim:** A listener with zones they don't want to control from UHC removes them once, in settings,
and never sees them again — and every zone list they encounter comes back in the same order every
time.

**Why it matters:** The zone list is the front door of UHC. A Roon user whose phone, laptop, and
System Output show up alongside their three real endpoints is doing visual filtering on every single
interaction. And because the list order is currently drawn fresh from a `HashMap` on each request,
"the third one down" is never the same zone twice — which makes muscle memory impossible and makes
physical knob bindings unreliable. Neither is a missing feature; both are a tax paid per use.

**Current State:**
- Every zone any adapter reports is shown. Personal endpoints (Roon-on-phone, laptop System Output,
  a stale Squeezelite instance) sit in the list next to real listening zones, and there is no way to
  remove one.
- `/zones` returns `HashMap::values()` ([aggregator.rs:272](../src/aggregator.rs), surfaced via
  [`get_all_zones_internal`](../src/knobs/routes.rs)) — order is nondeterministic **per request**.
  Knobs and any API consumer inherit that.
- The web zones page is the one consumer that already sorts
  ([zones.rs:256](../src/app/pages/zones.rs)), and it sorts case-sensitively by byte value, so
  `kitchen` lands after `Zone`.
- **MCP bypasses the shared chokepoint entirely.** Three paths read the aggregator raw —
  [`zones_payload`](../src/mcp/tools/zones.rs) (backing `hifi_zones` and `hifi://zones`),
  [resource enumeration](../src/mcp/resources.rs), and
  [`hifi_capabilities`](../src/mcp/tools/capabilities.rs). Today this costs nothing, because
  adapter settings are enforced *upstream* of the aggregator, not by the chokepoint's filter:
  [`register_from_settings`](../src/main.rs) gates startup, and #429 made the settings endpoint
  flush a disabled adapter's zones via `stop_adapter_and_flush` →
  [`AdapterStopping`](../src/aggregator.rs). So disabled adapters' zones are absent from
  `aggregator.get_zones()` outright. **It costs everything the moment a zone-list policy lives at
  the chokepoint** — which is exactly what this aim adds.

  *(Checked and found false:* an earlier draft claimed MCP ignores adapter toggles. It does not.
  The only residual divergence is an `app-settings.json` edited on disk without a restart —
  `get_all_zones_internal` re-reads settings per call, the aggregator does not — which is narrow,
  self-healing on restart, and closes for free once MCP shares the chokepoint.)*

**Desired State:**
- Zones the user has hidden are absent from every zone list — web UI, knobs, API, and MCP
  (`hifi_zones`, resources, `hifi_capabilities`) — until they unhide them.
- Every zone list is ordered by name, ascending, case-insensitively, identically on every request.

---

### Mechanism

**Change:**
1. Deterministic ordering applied at the shared chokepoint,
   [`get_all_zones_internal`](../src/knobs/routes.rs) — the one place that already does
   adapter-level filtering for all consumers. Default on, ascending, case-insensitive, with
   `zone_id` as tiebreaker so equal names never swap.
2. Per-zone visibility: a persisted set of hidden zone IDs in `app-settings.json` (alongside the
   existing `hide_knobs_page` / `hide_hqp_page` flags), filtered at the same chokepoint, with a
   settings-page control to hide/unhide. Default: nothing hidden.
3. Route MCP's three zone-list paths through the shared chokepoint, so hiding and ordering apply
   there. This is not a bug fix — MCP's output is correct today. It is what keeps MCP correct once
   (2) makes zone-list membership a policy decision rather than a fact about the aggregator.
   `tests/mcp_contract.rs::zones_resource_agrees_with_hifi_zones_tool` already asserts two of the
   three agree — extend it to cover `hifi_capabilities`, so the three cannot drift apart again.

**Hypothesis:** The reporter asked for "respect private zones," but the behavior they want is
"zones I don't care about aren't in my way." Per-zone visibility delivers that behavior without
depending on a signal Roon may not give us — and because the filter keys on the already-prefixed
`zone_id` rather than on any adapter-specific field, it is provider-agnostic by construction: Roon,
LMS, OpenHome, UPnP, and HQPlayer all get it from the same code with no per-adapter work.

**Assumptions:**
- **The Roon Core exposes no privacy flag to extensions.** The `Zone` struct we deserialize
  (`roon-api` `transport.rs:94`) carries `zone_id, display_name, outputs, state, is_*_allowed,
  queue_*, now_playing, settings`; `Output` carries `output_id, zone_id,
  can_group_with_output_ids, display_name, volume, source_controls`. Neither has a privacy field.
  Roon's Private Zone is a *remote-visibility* concept, and the reporter seeing private zones in UHC
  is evidence the Core does not filter them for extensions. **Not yet verified against a live
  Core** — the crate drops unknown JSON fields silently, so a field could exist that we never see.
  Cheap probe: log the raw zone payload once against a real Core. If a flag turns up, default-on
  private-zone hiding becomes possible and this aim gets revisited.
- Hiding a zone is a *display* decision, not a *control* decision — a hidden zone that is already
  bound to a knob or addressed directly by ID keeps working. Hiding removes it from lists, it does
  not deauthorize it.
- Zone IDs are stable enough across Core restarts to persist against. (Roon zone IDs are stable;
  worth confirming for LMS/UPnP before the setting is advertised as cross-provider.)
- Users have few enough zones that an explicit hide list is less work than the filtering they do
  today. Fails at large zone counts, where the ask would become allow-list-by-default.

**Misunderstanding Signal:** Someone ships a `hide_private_zones: true` toggle in settings that
keys off a name heuristic or an output type guess, and calls the reporter's request satisfied. That
delivers a checkbox with the reporter's words on it while guessing wrong about which zones are
personal — worse than the honest answer, because the user can no longer tell whether a missing zone
was their choice or ours.

---

### Feedback

**Signal:**
- The reporter (Docker, Roon-only) confirms their personal endpoints stay gone across a container
  restart, and that they did not have to hand-edit JSON to do it.
- `/zones` returns byte-identical ordering across repeated requests on an unchanged zone set —
  assertable in a test, not just observable.
- Hiding a zone removes it from `hifi_zones` as well as the web UI, in the same edit.
- No follow-up reports of "my knob started controlling the wrong zone," or of an assistant losing
  the ability to control a zone it could reach before.

**Timeframe:** Ordering is verifiable in-repo the day it lands. Visibility needs one round-trip with
the reporter — call it a week.

---

### Guardrails

- **Guardrail:** Hidden zones must not be a control-plane filter — commands addressed to a hidden
  zone by ID still execute, on every surface including MCP. `hifi_play` against a hidden zone's ID
  works; the zone simply is not *offered* by `hifi_zones`.
  **Reason:** Knob bindings, MCP calls, and HQPlayer links reference zones by ID. Making "hidden"
  mean "unreachable" would silently break working setups on upgrade. Hiding is decluttering, not
  authorization.
  **Trigger:** If someone asks to keep an assistant out of a room specifically — a genuine boundary
  rather than tidiness — that is a separate access-control concept and needs its own aim, not an
  overload of this setting.

- **Guardrail:** Ordering and filtering land at `get_all_zones_internal`, not per-consumer. No zone
  list may call `aggregator.get_zones()` directly.
  **Reason:** The ordering defect exists precisely because zone-list policy was treated as a
  per-consumer concern — the web page fixed it locally and everyone else inherited the `HashMap`.
  MCP has been fine reading raw only because zone-list membership was, until now, a fact about the
  aggregator rather than a policy. This aim changes that, and every raw reader becomes a place the
  policy can silently not apply.
  **Trigger:** If a consumer legitimately needs a different order (e.g. recently-played-first),
  that's a parameter on the shared path, not a bypass of it.

- **Guardrail:** Defaults must be honest. Ordering defaults on because there is no argument for
  random. Visibility defaults to *nothing hidden*, because we have no trustworthy signal for which
  zones are private.
  **Reason:** The reporter asked for default-on hiding; we can't deliver that without guessing, and
  a wrong guess hides a zone the user wanted with no clue why.
  **Trigger:** A verified privacy field from a live Core flips this — then default-on hiding of
  Roon-flagged private zones is correct and should ship.

- **Guardrail:** Grouping is out of scope for this aim.
  **Reason:** Group-by-source already exists on the web zones page; what "grouping" means to the
  people asking (Roon transport grouping vs. user-defined room grouping) is unresolved, and the two
  are wildly different in size.
  **Trigger:** Its own aim, once we know which one people want.

---

## Decisions Taken

| Question | Decision |
|---|---|
| What does "grouping" mean? | Deferred. Sorting + visibility only in this aim. |
| Fallback if Roon exposes no privacy flag? | Per-zone hide list, default all visible. Tell the reporter plainly that Roon gives extensions no private-zone signal. |
| Does hiding apply to all adapters? | Yes — provider-agnostic by construction, no per-adapter work. |
| Does hiding apply to MCP? | Yes, one shared list, **list-only**. Hidden zones vanish from `hifi_zones` / resources / `hifi_capabilities`; direct control by zone ID still works. Per-surface toggles and AI access control are explicitly not this aim. |
| Is there an MCP adapter-toggle bug to fix? | **No.** Claimed in an earlier draft, checked, found false — #429 and startup gating already keep disabled adapters' zones out of the aggregator. MCP still gets routed through the chokepoint, but as prevention, not repair. |
