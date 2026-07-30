# HQPlayer Control Protocol Reference

> **This document is a reader's guide, not the authority.** Since issue #322 the authority is the
> executable corpus under `tests/fixtures/hqplayer/<version>/`, driven by the conformance suite in
> `tests/hqplayer_conformance.rs`. Where the two disagree, the corpus wins — it carries per-fixture
> provenance and it fails the build. See [ADR 003](adr/003-hqplayer-conformance-boundary.md).
>
> This page was written from `hqp-control` v5.2.30 sources with no live verification, and it was
> wrong by omission in ways that shipped defects: it never mentioned the `result` attribute, so the
> implementation it authorised reported success for commands the daemon had rejected. The
> corrections below are marked **[corrected #322]** and are cross-checked against the 2026-07-29
> comparative salvage reports (`UHC-SALVAGE-UI-DATA-INTEGRATION.md`, `UHC-SALVAGE-BETA-DEV.md`), which
> report an HQPTuner audit of `hqp-control` 6.0.1 with findings verified on a live `hqplayerd` 6.0.4
> (Opal). **The upstream repository was not read directly for this page**; the URL below is the reports'
> own citation, and the live verification it describes is HQPTuner's, not this project's:
> <https://github.com/ohshitgorillas/hqptuner/blob/67557939ae04b157b47cb67bd651b72c3140bcdd/docs/protocol.md>

## Corrections from #322

| Claim here | Correction | Where it is pinned |
|---|---|---|
| No mention of command outcomes | **[corrected #322]** Setters and transport commands echo the request element with `result="OK"` or `result="Error"`, the latter carrying a reason as element text. An **absent** `result` is a third legitimate case: queries never carry one, and `SetAdaptiveVolume` answers a bare element. `<Ok/>` is a shape the daemon never sends. | `an_explicitly_rejected_setter_reports_the_daemon_reason` |
| No mention that `OK` can be a lie | **[corrected #322]** A setter can answer `result="OK"` without applying, and a change can land a poll later. `OK` alone is never proof; confirm by reading `State` back. | `a_setter_accepted_but_not_applied_does_not_report_success`, `a_setter_whose_change_lands_after_a_poll_still_reports_success` |
| `volume` treated as an integer | **[corrected #322]** `State.volume`, `Status.volume` and `VolumeRange.min`/`max`/`step` are **doubles** in dB. `<Volume value="-23.5"/>` is normal. Parsing them as integers silently yielded 0 dB — maximum output. | `a_fractional_negative_db_volume_round_trips`, `a_rounded_volume_is_never_reported_as_zero_db` |
| `State` fields listed without `filter_junk` | **[corrected #322]** The 20 kHz filter is `filter_junk`, an **int index** into `GetJunkFilters`, not a boolean `filter_20k`. The wire element is `SetJunkFilter`. | `the_junk_filter_is_read_as_a_list_index_not_a_boolean` |
| `state` documented as 0/1/2 | **[corrected #322]** There is a fourth value: `3` = stop requested. | `the_stop_requested_playback_state_is_reported_faithfully` |
| Nothing on framing | **[corrected #322]** Documents are newline-*terminated*, not newline-*framed*: a document may contain internal newlines, and containers normally do. Read until a complete document parses. A `Status` document's self-closing `<metadata …/>` child means the document ends at `</Status>`, not at the first `/>`. | `state_read_after_status_with_metadata_child_reports_the_daemon_state` |
| Nothing on attribute escaping | **[corrected #322]** Attribute values arrive entity-escaped and a bare `&` has been observed in the wild, so decoding must be lenient in both directions. Attribute lookups must also be scoped to the root element: a whole-document scan matches the XML declaration's `version="1.0"` in preference to `<GetInfo … version="6"/>`. | `the_matrix_profile_family_round_trips_a_name_containing_an_entity`, `get_info_reports_the_verified_daemon_identity` |
| Nothing on the persistent lane | **[corrected #322]** `hqplayerd.xml` stores enum **IDs** while `State`/`Set*` speak list **indices**; the domains must never be mixed. A persistent write's HTTP 200 proves receipt only — success comes from a readback, never from the POST. | `the_persistent_configuration_lane_stores_enum_ids_not_list_indices`, `the_restore_response_family_carries_no_outcome_signal` |
| Enumerations treated as stable | **[corrected #322]** They are mode-relative: `GetFilters`/`GetShapers`/`GetRates` return the current mode's list only, the lists differ wholesale, and a mode change resets the rate to auto. Re-enumerate after every mode change. | `enumerations_are_mode_relative_and_are_refetched_after_a_mode_change` |

The document's central claim **survives**: `Set*` and `State` speak the **list index**, not the enum
ID. That is now independently confirmed live — `<SetFilter value="6"/>` selects list index 6
(`poly-sinc-lp`) while enum ID 6 is a different filter (`poly-sinc-lp-2s`).

---

Original text follows, unchanged except for the note above.

Authoritative protocol semantics for HQPlayer TCP control interface, derived from analysis of the official `hqp-control` v5.2.30 reference implementation.

## Protocol Overview

- **Port:** 4321
- **Transport:** TCP
- **Format:** XML documents, newline-terminated
- **Example:** `<?xml version="1.0"?><SetMode value="1"/>\n`

## List Items: INDEX vs VALUE

Every list item (modes, filters, shapers) has **two identifiers**:

| Field | Description | Example |
|-------|-------------|---------|
| `index` | Position in list (0, 1, 2, ...) | `index="15"` |
| `value` | HQPlayer internal ID (non-sequential) | `value="53"` |

**Example filter list showing the difference:**

```text
[0] "none" value=0
[1] "IIR" value=1
[2] "IIR2" value=57        <- value != index
[15] "poly-sinc-hb-xs" value=53
[19] "poly-sinc-ext" value=15
```

**Example modes list showing index ≠ value:**

```text
[0] "[source]" value=-1   <- index=0, value=-1
[1] "PCM" value=0         <- index=1, value=0
[2] "SDM" value=1         <- index=2, value=1
```

**Exception:** `RatesItem` has no `value` field - only `index` and `rate` (Hz).

## Command Semantics

### What State Returns

The `<State/>` command returns these fields for settings:

| Field | Returns | Type | Notes |
|-------|---------|------|-------|
| `mode` | INDEX | u32 | Index into modes list (0,1,2) |
| `filter` | INDEX | u32 | General filter (fallback) |
| `filter1x` | INDEX | u32 | 1x filter |
| `filterNx` | INDEX | u32 | Nx filter |
| `shaper` | INDEX | u32 | Noise shaper |
| `rate` | INDEX | u32 | Rate list index (NOT Hz!) |
| `active_mode` | INDEX | u32 | Actually running mode |
| `active_rate` | Hz | u32 | Actually running rate |

### What Set Commands Expect

| Command | Parameter | Expects | Evidence |
|---------|-----------|---------|----------|
| `SetMode` | `value` | INDEX | CLI: `--set-mode <index>` |
| `SetFilter` | `value`, `value1x` | **INDEX** | CLI: `--set-filter <index> [index1x]` |
| `SetShaping` | `value` | **INDEX** | CLI: `--set-shaping <index>` |
| `SetRate` | `value` | INDEX | RateItem has no VALUE field |

### The Critical Rule

**For filter and shaper:**
- State returns INDEX
- SetFilter/SetShaping expect INDEX
- Round-trip: read from State, send back unchanged

**For mode:**
- State returns INDEX
- SetMode expects INDEX
- Same as filter/shaper!

**For rate:**
- State returns INDEX
- SetRate expects INDEX
- Display uses `rate` field from RateItem (Hz value)

## Reference Implementation Evidence

### CLI Help (Main.cpp:43)

```text
--set-mode <index>
--set-filter <index> [index1x]
--set-shaping <index>
--set-rate <index>
```

All commands use INDEX consistently. ModesItem has index (0,1,2) and value (-1,0,1) - these differ!

### setFilter Implementation (ControlInterface.cpp:1337)

```cpp
void clControlInterface::setFilter(int value, int value1x)
{
    xwriter->writeStartElement(QStringLiteral("SetFilter"));
    xwriter->writeAttribute(QStringLiteral("value"), QString::number(value));
    if (value1x >= 0)
        xwriter->writeAttribute(QStringLiteral("value1x"), QString::number(value1x));
}
```

The parameter is named `value` but the CLI passes the `<index>` argument directly.

### State Parsing (ControlInterface.cpp:1774-1790)

```cpp
emit stateResponse(
    xreader->attributes().value("state").toString().toInt(),
    xreader->attributes().value("mode").toString().toInt(),
    xreader->attributes().value("filter").toString().toInt(),
    iFilter1x, iFilterNx,
    xreader->attributes().value("shaper").toString().toInt(),
    xreader->attributes().value("rate").toString().toInt(),
    // ...
);
```

State values are parsed and passed through - the reference doesn't transform them.

### FiltersItem Parsing (ControlInterface.cpp:2084-2091)

```cpp
emit filtersItem(
    xreader->attributes().value("index").toString().toUInt(),
    xreader->attributes().value("name").toString(),
    xreader->attributes().value("value").toString().toInt(),
    xreader->attributes().value("arg").toString().toUInt());
```

Both `index` and `value` are captured from the list response.

## State vs Status

HQPlayer has two query commands with different semantics:

| Aspect | State | Status |
|--------|-------|--------|
| Filter/Shaper | Numeric (INDEX) | String (name) |
| active_mode | Numeric INDEX - **reliable** | String - **unreliable** |
| Use for | Settings UI, actual state | Display names |

**Warning:** Status's `active_mode` may show `"[source]"` even when outputting DSD. Always use State's numeric `active_mode`.

## Implementation Checklist

When implementing HQPlayer control:

- [ ] Parse FiltersItem/ShapersItem/ModesItem storing both `index` and `value`
- [ ] State.filter/filter1x/filterNx/shaper are INDEX - look up by index
- [ ] State.mode is INDEX - look up by index (ModesItem has index≠value!)
- [ ] SetFilter/SetShaping: send INDEX from State unchanged
- [ ] SetMode: send INDEX (CLI help confirms `--set-mode <index>`)
- [ ] SetRate: send INDEX (RateItem has no value field)
- [ ] For display: use Status's string fields (active_filter, active_shaper)
- [ ] For actual mode: use State's active_mode (INDEX), not Status's string

## Quick Reference Table

| Setting | State Field | State Type | Set Command | Set Expects | UI/API Use |
|---------|-------------|------------|-------------|-------------|------------|
| Mode | `mode` | INDEX | SetMode | INDEX | NAME (e.g., "PCM") |
| Filter 1x | `filter1x` | INDEX | SetFilter | INDEX | NAME (e.g., "poly-sinc-ext2") |
| Filter Nx | `filterNx` | INDEX | SetFilter | INDEX | NAME |
| Shaper | `shaper` | INDEX | SetShaping | INDEX | NAME (e.g., "ASDM7") |
| Rate | `rate` | INDEX | SetRate | INDEX | Hz (e.g., 48000) |

## API Design

**Clients (UI, API, MCP) use semantic values:**
- Mode: `"PCM"`, `"DSD"`, `"[source]"`
- Filter: `"poly-sinc-ext2"`, `"IIR"`, etc.
- Shaper: `"ASDM7"`, `"NS5"`, etc.
- Samplerate: `48000`, `96000`, etc. (Hz)

**Adapter handles all HQPlayer-specific conversions:**
- Mode name → INDEX (0, 1, 2)
- Filter name → INDEX
- Shaper name → INDEX
- Rate Hz → INDEX

## Version

- Reference: hqp-control v5.2.30 (2024-03-31)
- This document: 2026-02-05
