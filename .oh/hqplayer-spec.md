# HQPlayer Protocol Specification Session

> ## Historical session record — its protocol claims are retired
>
> **[`docs/hqplayer-evidence-ledger.md`](../docs/hqplayer-evidence-ledger.md) supersedes the protocol
> claims on this page** (issue #341). This file is a February 2026 session record and is kept for the
> reasoning it captures, not for its conclusions.
>
> **One claim here is wrong and was contradicted by `docs/hqplayer-protocol-reference.md` for five
> months:** that `SetMode` expects an enum **VALUE** (−1 / 0 / 1). It expects the **list index**
> (0 / 1 / 2). The client resolves a name to a list index and sends that
> (`src/adapters/hqplayer.rs` `set_mode`), and the live HQPlayer Embedded 6.0.2 run on 2026-07-30
> round-tripped SDM→PCM→SDM with `State.mode=2` for SDM, whose enum ID is 1. See ledger
> **HQP-C-001**.
>
> Wrong claims below are struck through **in place** rather than deleted, so a reader arriving from an
> old link sees the correction instead of a clean file.

## Solution Space Analysis
**Updated:** 2026-02-04

**Problem:** HQPlayer adapter has confusing, contradictory implementation for filter/shaper settings - switching between VALUE and INDEX semantics across commits without a definitive source of truth.

**Key Constraint:** The protocol lacks official documentation beyond the CLI help text. The reference implementation (hqp-control) is a CLI tool that just prints/sends values without revealing the mapping.

### Candidates Considered

| Option | Level | Approach | Trade-off |
|--------|-------|----------|-----------|
| A | Band-Aid | Pick one (VALUE or INDEX) and hope it works | May break when tested with real HQPlayer |
| B | Local Optimum | Empirical testing with real HQPlayer to determine correct semantics | Requires HQPlayer instance; still no documentation |
| C | Reframe | **Create definitive protocol doc first**, then implement against it | Upfront effort, but prevents future confusion |
| D | Redesign | Abstract the semantics - store both VALUE and INDEX, let callers specify which | Over-engineering for a single-purpose integration |

### Evaluation

**Option A: Pick One (Band-Aid)**
- Solves stated problem: No - just kicks the can
- Risk: High - could silently fail with wrong settings
- Second-order: More commits flip-flopping

**Option B: Empirical Testing**
- Solves stated problem: Partially - validates for one HQPlayer version
- Implementation cost: Medium (requires running HQPlayer)
- Second-order: Still no documentation for future maintainers

**Option C: Create Definitive Protocol Doc (Reframe)**
- Solves stated problem: Yes - establishes truth source
- Implementation cost: Medium (analysis + doc writing + implementation)
- Second-order: Future changes have a reference
- Enables: Clear code, clear tests, clear debugging

**Option D: Abstract Both Semantics (Redesign)**
- Solves stated problem: Overkill
- Second-order: Unnecessary complexity

### Recommendation

**Selected:** Option C - Create Definitive Protocol Doc
**Level:** Reframe

**Rationale:** The root problem isn't "which value is correct" - it's "there's no authoritative documentation." The flip-flopping between VALUE and INDEX across commits proves this. By establishing a clean protocol document based on the reference implementation analysis, we:

1. Have a single source of truth
2. Can implement confidently
3. Can debug issues by comparing actual vs expected protocol
4. Future maintainers understand the semantics

### Protocol Semantics (Determined from Reference Implementation)

| Item | State Returns | SetCommand Expects | Notes |
|------|--------------|-------------------|-------|
| ~~Mode~~ | ~~VALUE~~ | ~~VALUE~~ | **[retired #341 — wrong]** ~~mode values: -1=[source], 0=PCM, 1=SDM~~. `State.mode` and `SetMode` speak the **list index** (0/1/2), not the enum ID. Ledger HQP-C-001 |
| Filter | **INDEX** | **INDEX** | State.filter/filter1x/filterNx are indices; CLI confirms `--set-filter <index>` |
| Shaper | **INDEX** | **INDEX** | State.shaper is index; CLI confirms `--set-shaping <index>` |
| Rate | INDEX | INDEX | RateItem has no VALUE field anyway |

**The critical insight:** HQPlayer State response returns INDEX for filter/shaper fields (not VALUE). The CLI confirms this with `--set-filter <index>`. Commit 62cc994 was correct; the protocol audit doc is wrong when it says "send VALUE".

### Evidence from Reference Implementation (hqp-control v5.2.30)

1. **CLI Help** (`Main.cpp:43`): `--set-filter <index> [index1x]` and `--set-shaping <index>`
2. **setFilter signature** (`ControlInterface.cpp:1337`): `void setFilter(int value, int value1x)` - parameter named "value" but CLI calls it "index"
3. **State parsing** (`ControlInterface.cpp:1777`): `xreader->attributes().value("filter").toString().toInt()` - just reads the number
4. **FiltersItem** has both `index` and `value` fields, but State response uses whichever one HQPlayer returns

### Accepted Trade-offs
- Requires rewriting protocol audit doc to fix incorrect VALUE claims
- Need to verify assumption empirically if possible
- Code comments need cleanup to remove contradictions

## Implementation Plan

1. **Create** `/docs/hqplayer-protocol-reference.md` - authoritative protocol semantics document
2. **Update** `/docs/hqplayer-protocol-audit.md` - fix the incorrect VALUE claims, align with reference
3. **Clean up** code comments in `src/adapters/hqplayer.rs` - remove contradictory statements
4. **Verify** `set_filter()` and `set_shaper()` use INDEX consistently
5. **Verify** `get_pipeline_status()` displays INDEX-based lookups correctly

## Execute
**Updated:** 2026-02-04
**Status:** complete

### Changes Made

1. **Created** `docs/hqplayer-protocol-reference.md` - Authoritative 150-line protocol reference with:
   - Clear INDEX vs VALUE semantics table
   - Evidence from reference implementation (line numbers)
   - Quick reference table for all settings
   - Implementation checklist

2. **Updated** `docs/hqplayer-protocol-audit.md` - Fixed incorrect VALUE claims:
   - Summary now correctly states filter/shaper use INDEX
   - State fields table corrected: filter/filter1x/filterNx/shaper are INDEX
   - Testing examples updated to use INDEX
   - Added reference to new protocol-reference.md

3. **Cleaned** `src/adapters/hqplayer.rs` comments:
   - `set_mode()`: Clarified VALUE semantics (only 3 values)
   - `set_filter()`: Removed contradictory "VALUE" claim, now says INDEX
   - `set_shaper()`: Simplified, references CLI help
   - `resolve_filter_index()` / `resolve_shaper_index()`: Removed redundant "not value!" comments

### Verification
- `cargo check` passes
- No code logic changes (commit 62cc994 already correct)
- Documentation now consistent with implementation

---

## Implementation Architecture (Final)
**Updated:** 2026-02-04

After testing with real HQPlayer, we discovered the API was leaking HQPlayer's internal INDEX/VALUE semantics to clients. The fix: **clients use semantic names, adapter handles all conversions**.

### Layer Responsibilities

```
┌────────────┬──────────────┬────────────┬────────────┬────────────┐
│   Layer    │     Mode     │   Filter   │   Shaper   │ Samplerate │
├────────────┼──────────────┼────────────┼────────────┼────────────┤
│ UI options │ NAME ("PCM") │ NAME       │ NAME       │ Hz (48000) │
├────────────┼──────────────┼────────────┼────────────┼────────────┤
│ API/MCP    │ pass NAME    │ pass NAME  │ pass NAME  │ pass Hz    │
├────────────┼──────────────┼────────────┼────────────┼────────────┤
│ Adapter    │ NAME→INDEX*  │ NAME→INDEX │ NAME→INDEX │ Hz→INDEX   │
└────────────┴──────────────┴────────────┴────────────┴────────────┘
```

\* **[retired #341]** this row read `NAME→VALUE` for Mode. It is `NAME→INDEX`: the adapter resolves a mode name to its list position. Ledger HQP-C-001.

### Design Principle

**Clients (UI, API, MCP) only know domain terms:**
- Mode: `"PCM"`, `"DSD"`, `"[source]"`
- Filter: `"poly-sinc-ext2"`, `"IIR"`, etc.
- Shaper: `"ASDM7"`, `"NS5"`, etc.
- Samplerate: `48000`, `96000`, etc. (Hz)

**Adapter handles all HQPlayer-specific weirdness:**
- ~~Mode name → VALUE (-1, 0, 1) via `resolve_mode_value()`~~ **[retired #341]** mode name → **INDEX** (0, 1, 2) via `resolve_mode_index()`; there is no `resolve_mode_value()` in the client. Ledger HQP-C-001
- Filter name → INDEX via `resolve_filter_index()`
- Shaper name → INDEX via `resolve_shaper_index()`
- Rate Hz → INDEX via lookup in rates list

### Key Changes

1. **`set_mode(&str)`** - Now takes name like "PCM", ~~resolves to VALUE~~ **[retired #341]** resolves to the list **INDEX**. Ledger HQP-C-001
2. **UI SelectOptions** - Send NAME in `value` field, not index/value numbers
3. **API handlers** - Pass strings directly to adapter methods
4. **MCP handlers** - Simplified, no numeric parsing for mode/filter/shaper

### Why This Matters

The previous implementation leaked internal details:
- UI sent numeric VALUES for mode (-1, 0, 1)
- UI sent numeric INDICES for filter/shaper
- Off-by-one bugs when semantics got confused

Now clients are insulated from HQPlayer's protocol details. If HQPlayer changes its internal numbering, only the adapter needs updating.
