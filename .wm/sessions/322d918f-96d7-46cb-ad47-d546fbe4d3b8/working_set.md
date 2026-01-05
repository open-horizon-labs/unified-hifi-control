# Dive Session

**Intent:** fix
**Started:** 2026-01-05T08:37:00Z
**Endeavor:** 80222d6d - Unified Hi-Fi Control
**Issue:** https://github.com/cloud-atlas-ai/unified-hifi-control/issues/21
**Branch:** fix/roon-zone-late-discovery
**PR:** https://github.com/cloud-atlas-ai/unified-hifi-control/pull/22

## Context

### Project
Source-agnostic hi-fi control platform bridging music sources (Roon, UPnP, HQPlayer) to any control surface (ESP32 knobs, web UI, Home Assistant, Claude MCP).

**Key Architecture Principle:** All backends are optional - any can contribute zones independently.

### Focus
**Bug:** Roon zones not appearing on the knob when powered on after the hub/bridge.

**Symptom:**
- Hub/bridge runs 24/7
- A **Roon zone** is powered on later
- Zone does NOT appear in the knob's zone list
- Cannot select that zone on the knob
- Workaround: Reboot the Hub

**Root cause hypothesis:** Late-joining Roon zones aren't propagating through the chain.

**Diagnostic finding:** Zone missing from both knob AND web UI → Gap is early in chain:
- Roon API → Roon Adapter? (not receiving zones_changed events)
- Roon Adapter → Bus? (not calling onZonesChanged callback)
- (Bus → Knob ruled out since web UI also doesn't see it)

### Constraints
- From .superego/: Must verify hypothesis with instrumentation before implementing fixes
- From OH mission: Multi-backend bus where all sources are optional

### Relevant Knowledge
- Bus calls `refreshZones()` once after all adapters start (bus/index.js:147-158)
- Roon API uses subscription model with zones_changed callback
- Previous UPnP work addressed similar async discovery issues

## Workflow

1. **Locate the gap** (diagnostic first)
   - When a late zone appears, does it show in `/admin` web UI?
     - YES → Gap is Bus → Knob (knob not receiving updates)
     - NO → Gap is earlier in chain
   - Check Roon adapter's zones_changed handler
   - Check if adapter calls bus's onZonesChanged

2. **Understand the code path**
   - Read Roon adapter zone discovery code
   - Trace how zone changes propagate to bus
   - Trace how bus notifies knobs of zone changes

3. **Implement fix**
   - Wire up proper zone change notifications at the identified layer

4. **Test**
   - Power off a Roon zone
   - Start the bridge
   - Power on the zone
   - Verify it appears on the knob without hub reboot

5. **Commit and push**

## Sources
- Local: .superego/, README.md
- Git: master branch
- OH: endeavor 80222d6d (Unified Hi-Fi Control)
