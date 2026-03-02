# Dial Configurator Session

Session continuation from command-pattern manifest PoC work.

## Completed This Session

### MCP Schema Fix
- `rust-mcp-sdk` v0.8's `JsonSchema` derive emits `"type":"unknown"` for `serde_json::Value`
  and `"nullable":true` for `Option<T>` — both invalid JSON Schema 2020-12
- Manually implemented `json_schema()` for `HifiControlTool`, `HifiSearchTool`, `HifiPlayItemTool`
- All 14 MCP tools now pass Claude's schema validation

### Build Script
- Created `scripts/build-release.sh` — two-step build (dx WASM + cargo server)
- `--server` flag skips WASM for faster iteration

### License Persistence (Issue #49)
- License was only in-memory — lost on every server restart
- Added `save_license()` / `load_saved_license()` / `delete_saved_license()` to `config/mod.rs`
- POST handler saves to `memex-license.json`, startup loads as fallback
- DELETE handler removes file from disk
- Added to `MIGRATABLE_CONFIG_FILES` for config dir migration

### Device Registration Fix
- `knob_zones_handler` (`GET /zones`) ignored `knob_id` query param — never registered devices
- The real dial firmware at 192.168.1.142 (`d0cf131e1ae0`) calls `/zones?knob_id=...`
- Fixed: now calls `get_or_create` + `update_status` when `knob_id` present
- Device "toothless" (d0cf131e1ae0, fw 2.2.2-dev) now registers correctly

### Configurator Page
- Replaced device type/zone dropdowns with actual device dropdown from `/knob/devices`
- Rewrote DialPreview as pure SVG (no HTML div clipping issues)
- Added `device_display_name()` helper
- Removed zone selector from configurator — zones are runtime conditions, not design-time inputs
- Generate response now feeds preview directly (no zone-based re-fetch)

### Implicit Registration on All Endpoints (DONE)
- `knob_control_handler`: now extracts knob_id from headers, calls `get_or_create` + `update_status`
- `knob_config_handler`: changed from `get()` to `get_or_create()`
- `knob_config_update_handler`: added `get_or_create` before `update_config`

### Stale Device Cleanup (DONE)
- Cleaned knobs.json: removed `test-knob`, `test-knob-123`, `11:22:33:44:55:66`, `AA:BB:CC:DD:EE:FF`
- Added `remove()` method to `KnobStore`
- Added `DELETE /knob/devices/{id}` endpoint

### Rename "Knob" → "Dial" (UI layer DONE)
- Nav link: "Knobs" → "Dials", Route: `/knobs` → `/dials`
- Page component: `Knobs` → `Dials` (new file `dials.rs`)
- Internal rename queued as Task #7 (~277 instances across 16 categories)

---

## Solution Space: Data-Driven Adaptive UI

**Updated:** 2026-03-01

### Problem Statement

The dial currently renders one hardcoded default manifest per zone. Users want to:
1. Create custom layouts (manifest templates) via natural language
2. Save those layouts with names
3. Attach conditions so the right layout renders automatically based on runtime state
4. Preview layouts before deploying them to a device

**Key constraint:** The SHA-based UDP polling mechanism (~2s cycle) already handles manifest change propagation — no firmware protocol changes needed.

**Success looks like:** User creates "Podcast Mode" (mute + 30s skip, no prev/next), attaches condition `genre contains "podcast"`, and the dial automatically switches when a podcast plays.

### Runtime State Available for Conditions

From `Zone` (muse-events/src/zone.rs):
- `zone_id` — e.g. `roon:1601defc98ff...`
- `zone_name` — e.g. "Living Room"
- `source` — `roon`, `lms`, `hqplayer`, `openhome`, `upnp`
- `state` — `playing`, `paused`, `stopped`, etc.

From `NowPlaying`:
- `title`, `artist`, `album`
- `duration` — enables long-content heuristic (>600s = podcast/audiobook)

From `TrackMetadata`:
- `genre` — when available from source
- `format` — FLAC, DSD, MQA, etc.
- `sample_rate`, `bit_depth`

**Note:** Source metadata will expand (e.g., Internet Radio station name, playlist context). Condition matching should be extensible.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|--------|-------|----------|-----------|
| A | Local Optimum | Zone-keyed manifest store with named presets | Doesn't generalize beyond zones |
| B | Reframe | Layout + Condition + Binding as separate entities | More moving parts, but composable |
| C | Redesign | Rule engine with priority-ordered condition stack per device | Full generality, more complexity |
| D | Redesign | CSS-like cascade — conditions as selectors, layouts as rulesets | Elegant but unfamiliar UX |

### Evaluation

**Option A: Zone-Keyed Presets**
- Solves stated problem: Partially (only zone-based conditions)
- Implementation cost: Low (extend existing ManifestStore)
- Maintenance burden: Low
- Second-order: Dead end. "Genre = podcast" can't be expressed. Rebuilds needed later.
- Verdict: **Reject** — too narrow

**Option B: Layout + Condition + Binding (Separate Entities)**
- Solves stated problem: Yes
- Implementation cost: Medium
- Maintenance burden: Medium — three entity types, but each is simple
- Second-order: Clean separation of concerns. Layouts reusable across devices. Conditions reusable across layouts.
- Verdict: **Strong candidate** — matches the user's mental model

**Option C: Priority-Ordered Condition Stack Per Device**
- Solves stated problem: Yes
- Implementation cost: Medium-High
- What it adds over B: The stack IS the binding. Device has an ordered list of (condition → layout) pairs. First match wins.
- Second-order: Natural priority model. "Default" is just the last entry. Easy to reason about.
- Verdict: **Best candidate** — the stack collapses binding into something intuitive

**Option D: CSS-Like Cascade**
- Solves stated problem: Yes
- Implementation cost: High (specificity rules, cascade logic)
- Second-order: Powerful but the UX is hard to explain. "Why is this layout showing?" becomes a debugging problem.
- Verdict: **Reject** — over-engineered for a hardware device UI

### Recommendation

**Selected:** Option C — Priority-Ordered Condition Stack Per Device
**Level:** Redesign

**Rationale:** The condition stack is the simplest model that's fully general. It directly answers "why is this layout showing?" — walk the stack top-to-bottom, first match wins. The user's "layout stack" framing maps directly to this.

**Why not B:** Separate bindings add indirection. The stack collapses layout selection into a single ordered list per device — easier to understand, display, and reorder in the UI.

**Accepted trade-offs:**
- Layouts can't be shared across devices without duplication (but can be cloned)
- Stack reordering is the primary UX for priority — needs drag-and-drop or up/down arrows
- Condition evaluation happens on every manifest request (but conditions are simple field comparisons, negligible cost)

### Data Model

```
Layout {
    id: uuid
    name: String              // "Podcast Mode", "Default", "Late Night"
    screens: Vec<Screen>      // The manifest template (screens + nav + interactions)
    nav: Nav
    interactions: Option<HashMap<String, String>>
    created_at: DateTime
    updated_at: DateTime
}

Condition {
    field: String             // "zone_id", "source", "genre", "artist", "duration_gt", "default"
    op: ConditionOp           // Equals, Contains, GreaterThan, Exists, Always
    value: Option<String>     // Match value (None for Always/Exists)
}

ConditionOp: Equals | Contains | GreaterThan | Exists | Always

DeviceStack {
    device_id: String         // Dial device ID (e.g., "d0cf131e1ae0")
    entries: Vec<StackEntry>  // Ordered top-to-bottom, first match wins
}

StackEntry {
    layout_id: uuid
    conditions: Vec<Condition>  // ALL must match (AND logic)
    enabled: bool               // Can be toggled without removing
}
```

**Evaluation algorithm:**
```
fn resolve_layout(device: &DeviceStack, zone: &Zone) -> Option<&Layout> {
    device.entries.iter()
        .filter(|e| e.enabled)
        .find(|e| e.conditions.iter().all(|c| c.matches(zone)))
        .map(|e| lookup_layout(e.layout_id))
}
```

**Condition matching** against zone state:
- `field: "default"` + `op: Always` → always matches (fallback)
- `field: "zone_id"` + `op: Equals` + `value: "roon:1601..."` → zone match
- `field: "source"` + `op: Equals` + `value: "roon"` → source type
- `field: "genre"` + `op: Contains` + `value: "podcast"` → metadata match
- `field: "artist"` + `op: Contains` + `value: "NPR"` → artist match
- `field: "duration_gt"` + `op: GreaterThan` + `value: "600"` → long content
- `field: "format"` + `op: Equals` + `value: "DSD"` → hi-res content

Extensible: new fields added by expanding the match function. No schema changes needed.

### Persistence

Single file: `layouts.json` in config dir:
```json
{
  "layouts": { "<uuid>": { "name": "...", "screens": [...], ... } },
  "stacks": { "<device_id>": { "entries": [...] } }
}
```

Loaded at startup, saved on mutation. Same pattern as `knobs.json`.

### Integration Points

**ManifestStore changes:**
- `get()` currently takes zone_id, returns pushed manifest
- New: `resolve(device_id, zone)` → evaluate device's condition stack against zone state → return matching layout's screens/nav
- Fallback: if no stack entry matches, use existing `build_default_manifest()`

**`knob_manifest_handler` changes (GET /knob/manifest):**
- Currently: looks up pushed manifest by zone_id
- New: also checks device's condition stack (device_id from header/query)
- Priority: condition-stack layout > Memex push > default

**UDP fast-path (udp.rs):**
- SHA computation already calls `get_pushed_sha()` or `build_default_screens()`
- New: include condition-stack layout SHA in the computation
- If zone state changes and a different condition matches → SHA changes → firmware re-fetches

**Configurator page:**
- Device dropdown (existing)
- LLM chat creates/modifies a layout (existing generate flow, but now saves to LayoutStore)
- New: "Save Layout" button → names and persists the layout
- New: condition stack editor → add conditions, reorder entries
- Preview shows the layout being edited (existing SVG preview)

**LLM generate endpoint:**
- Currently stores manifest in ManifestStore keyed by zone_id
- New: returns the manifest to the client. Client previews it. User saves it as a named layout.
- The generate endpoint becomes stateless — it generates, the client decides whether to save.

### Preview & Test

**Design-time preview:** Already working — generate response feeds SVG preview directly.

**Test against live state:** New "Test" button. Given a device + layout:
1. Fetch current zone state for the device's active zone
2. Merge layout screens with live fast state
3. Show in preview: "This is what it would look like right now"

**Condition preview:** Show which stack entry would match for each zone:
- List zones with a colored indicator showing which layout would activate
- Helps user verify conditions before deploying

### Implementation Order

1. **LayoutStore** — CRUD for layouts, persist to disk
2. **API endpoints** — `GET/POST/DELETE /api/layouts`, `GET/POST /api/devices/{id}/stack`
3. **Condition evaluator** — `matches(condition, zone) -> bool`
4. **ManifestStore integration** — resolve layout from condition stack during manifest serving
5. **Configurator UI** — save layout button, condition stack editor, preview enhancements
6. **UDP SHA integration** — condition-aware SHA computation

### Open Questions

- Should MCP/Memex push still override condition-stack layouts? (Probably yes — Memex is real-time context)
- Should conditions support OR logic? (Start with AND-only per entry; OR is just multiple entries with same layout)
- How to handle zone switching on the device? (Device firmware selects zone → that zone's state feeds condition eval)

---

## Pending Tasks

### Full Knob → Dial Rename (Task #7)
- ~277 instances across 16 categories
- Full breakdown in task description
- Wire protocol stays unchanged

### CodeRabbit Follow-ups
- Review feedback on MCP schema PR
- Review feedback on command-pattern manifest PR
