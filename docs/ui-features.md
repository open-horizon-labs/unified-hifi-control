# UI Features Documentation

This document catalogs the UI features across both the v2 (Node.js / master branch) and v3 (Rust / current branch) implementations.

## Pages Overview

| Page | v2 (Node.js) | v3 (Rust/Dioxus) |
|------|--------------|------------------|
| Control / Zones | `/control` | `/zones` |
| Zone (focused) | `/zone` | `/zone` |
| Knobs | `/knobs` | `/knobs` |
| HQPlayer | `/hqp` | `/hqplayer` |
| Settings | `/settings` | `/settings` |
| Dashboard | - | `/dashboard` |
| LMS | - | `/lms` |

---

## Control / Zones Page

The main zones overview showing all available zones with playback controls.

### Features

| Feature | v2 (Node.js) | v3 (Rust) | Notes |
|---------|--------------|-----------|-------|
| Zone cards with album art | ✅ 120x120 | ✅ 80x80 | v3 uses w-20 (80px) |
| Transport controls (prev/play/next) | ✅ | ✅ | |
| Volume controls (+/-) | ✅ | ✅ | |
| HQP badge on linked zones | ✅ | ✅ | |
| Source badge (Roon/LMS/etc) | ✅ | ✅ | |
| Zone grouping by protocol | ✅ | ❌ | v2 groups by roon:/lms:/openhome: prefix |
| HQP profile dropdown in card | ✅ | ✅ | Via HqpControlsCompact |
| HQP matrix dropdown in card | ❌ | ✅ | v3 added matrix to compact controls |
| Now playing (track/artist/album) | ✅ | ✅ (track/artist only) | v3 shows less detail |
| Volume display with unit | ✅ | ✅ | Handles dB vs numeric |
| Placeholder for no album art | ✅ "No Art" box | ✅ "♪" icon | Different styling |
| Real-time SSE updates | ✅ via polling | ✅ via SSE | |

---

## Zone Page (Single Zone Focus)

Focused view for a single zone with full DSP controls.

### Features

| Feature | v2 (Node.js) | v3 (Rust) | Notes |
|---------|--------------|-----------|-------|
| Zone selector dropdown | ✅ | ✅ | |
| Persist selected zone (localStorage) | ✅ | ✅ | |
| Large album art | ✅ 120x120 | ✅ 200x200 | v3 larger |
| Transport controls | ✅ | ✅ | |
| Volume display and controls | ✅ | ✅ | |
| Track/artist/album display | ✅ | ✅ | |
| HQP badge | ✅ | ❌ | Missing in v3 zone display |
| Device name display | ✅ | ❌ | v2 shows "(Device Name)" |
| HQP DSP Section (when HQP zone) | ✅ | ✅ | |
| HQP Status display | ✅ | ❌ | v2 shows "Connected/Disconnected" |
| HQP Profile selector | ✅ | ✅ | |
| HQP Matrix selector | ✅ | ✅ | |
| HQP Mode selector | ✅ | ✅ | |
| HQP Sample Rate selector | ✅ | ✅ | |
| HQP Filter 1x selector | ✅ | ✅ | |
| HQP Filter Nx selector | ✅ | ✅ | |
| HQP Shaper/Dither selector | ✅ | ✅ | |
| HQP Shaper label changes based on mode | ✅ | ❌ | v2: "Modulator" for SDM/DSD |
| Pipeline controls wired | ✅ | ✅ | |
| Loading state during HQP changes | ✅ | ❌ | v2 disables controls during updates |

---

## Knobs Page

Knob device management and firmware.

### Features

| Feature | v2 (Node.js) | v3 (Rust) | Notes |
|---------|--------------|-----------|-------|
| Knobs table (ID, name, version, etc) | ✅ | ✅ | |
| Battery level with charging indicator | ✅ | ✅ | |
| Zone assignment display | ✅ | ✅ | |
| Last seen timestamp | ✅ | ✅ | |
| Config button per knob | ✅ | ✅ | |
| Firmware version display | ✅ | ✅ | |
| Fetch firmware from GitHub | ✅ | ✅ | |
| Flash knob link | ✅ | ✅ | |
| Community thread link | ✅ | ✅ | |

### Config Modal Features

| Feature | v2 (Node.js) | v3 (Rust) | Notes |
|---------|--------------|-----------|-------|
| Knob name | ✅ | ✅ | |
| Display rotation (charging) | ✅ 0°/180° | ✅ 0°/180° | |
| Display rotation (battery) | ✅ 0°/180° | ✅ 0°/180° | |
| Side-by-side Charging/Battery layout | ✅ | ✅ | |
| Rotation in power columns | ✅ | ✅ | |
| Art Mode timeout (charging) | ✅ | ✅ | |
| Art Mode timeout (battery) | ✅ | ✅ | |
| Dim timeout (charging) | ✅ | ✅ | |
| Dim timeout (battery) | ✅ | ✅ | |
| Sleep timeout (charging) | ✅ | ✅ | |
| Sleep timeout (battery) | ✅ | ✅ | |
| Deep Sleep timeout (charging) | ✅ | ✅ | |
| Deep Sleep timeout (battery) | ✅ | ✅ | |
| Enabled checkbox per power mode | ✅ | ❌ | v3 uses "0 = disabled" |
| WiFi Power Save toggle | ✅ | ✅ | |
| CPU Frequency Scaling toggle | ✅ | ✅ | |
| Sleep Poll Interval | ✅ | ✅ | |
| Power state explanatory text | ✅ | ✅ | v3 has "Set 0 to disable" |

---

## HQPlayer Page

HQPlayer service configuration and zone linking.

### Features

| Feature | v2 (Node.js) | v3 (Rust) | Notes |
|---------|--------------|-----------|-------|
| Add Instance form | ✅ | ✅ | |
| Host input with Test button | ✅ | ✅ | |
| Auto-detect HQP type (Embedded/Desktop) | ✅ | ✅ | |
| Web UI credentials (Embedded) | ✅ | ✅ | |
| Instances table | ✅ | ✅ | |
| Delete instance button | ✅ | ✅ | |
| Zone Links section | ✅ | ✅ | |
| Link zone dropdown | ✅ | ✅ | |
| Unlink button | ✅ | ✅ | |
| Credentials persistence | ✅ | ✅ | Fixed in v3 |
| "Saved credentials" indicator | ❌ | ✅ | v3 shows placeholder text |

---

## Settings Page

Application settings and discovery status.

### Features

| Feature | v2 (Node.js) | v3 (Rust) | Notes |
|---------|--------------|-----------|-------|
| LMS Configuration section | ✅ | ❌ | v3 has separate /lms page |
| Audio Backends toggles | ✅ | ✅ | |
| - Roon toggle | ✅ | ✅ | |
| - LMS toggle | ✅ | ✅ | |
| - OpenHome toggle | ✅ | ✅ | |
| - UPnP toggle | ✅ | ✅ | |
| - HQPlayer toggle | ❌ | ✅ | v3 added |
| Hide Knobs page toggle | ✅ | ✅ | |
| Hide HQPlayer page toggle | ✅ | ✅ | |
| Hide LMS page toggle | ❌ | ✅ | v3 added |
| Status/Discovery table | ✅ | ✅ | |
| - Roon status row | ✅ | ✅ | |
| - OpenHome status row | ❌ | ✅ | v3 added |
| - UPnP status row | ❌ | ✅ | v3 added |
| - LMS status row | ❌ | ✅ | v3 added |
| - HQPlayer status row | ❌ | ✅ | v3 added |
| Debug info (bus activity) | ✅ | ❌ | Hidden in collapsible |
| Theme toggle | ✅ (in nav) | ✅ (dedicated section) | v3 has 4 themes |
| Theme: Light | ✅ | ✅ | |
| Theme: Dark | ✅ | ✅ | |
| Theme: Black/OLED | ✅ | ✅ | |
| Theme: System | ❌ | ✅ | v3 added |

---

## Navigation

| Feature | v2 (Node.js) | v3 (Rust) | Notes |
|---------|--------------|-----------|-------|
| Brand/logo link | ✅ | ✅ | |
| Desktop nav links | ✅ | ✅ | |
| Mobile hamburger menu | ❌ | ✅ | v3 added |
| Tab hiding based on settings | ✅ | ✅ | |
| Theme toggle in nav | ✅ | ❌ | v3 moved to Settings |
| Version display in nav | ✅ | ❌ | |

---

## Gap Analysis Summary

### v3 Missing Features (from v2)

1. **Zone grouping by protocol** - v2 groups zones under headers (Roon, LMS, OpenHome, UPnP)
2. **HQP badge on Zone detail page** - Zone page doesn't show HQP badge
3. **Device name in Zone detail** - v2 shows "(Device Name)" after zone name
4. **HQP status indicator** - "Connected/Disconnected" status in Zone page HQP section
5. **Dynamic Shaper label** - v2 changes "Shaper" to "Modulator" for SDM/DSD modes
6. **Loading state during HQP changes** - v2 disables all HQP controls during updates
7. **Power mode enabled checkboxes** - v2 has separate checkbox + number input per power mode
8. **Debug info section** - v2 has collapsible bus activity debug info
9. **Version display in nav** - v2 shows app version in navigation

### v3 Improvements over v2

1. **Mobile responsive navigation** - Hamburger menu for mobile devices
2. **Separate LMS page** - Dedicated configuration page
3. **HQPlayer adapter toggle** - Can enable/disable HQPlayer in settings
4. **Hide LMS page option** - Additional navigation customization
5. **System theme option** - Respects OS color scheme preference
6. **Full discovery status table** - Shows all adapters with status
7. **HQP Matrix in compact controls** - Available in zone cards
8. **Larger album art on Zone page** - 200x200 vs 120x120
9. **Saved credentials indicator** - Shows when HQP credentials are saved
10. **SSE-based real-time updates** - More efficient than polling

### Parity Status

- **Knob Config Modal**: ✅ Full parity (side-by-side layout, all power modes, rotation, advanced settings)
- **Settings Page**: ✅ Full parity + improvements (more adapters, more hide options, better status)
- **Zones Page**: ✅ Mostly at parity (missing protocol grouping)
- **Zone Page**: ⚠️ Minor gaps (missing HQP badge, device name, status indicator, loading state)
- **HQPlayer Page**: ✅ Full parity + improvements (credentials indicator)
- **Navigation**: ✅ Full parity + improvements (mobile menu)
