# Dive Session

**Intent:** fix
**Started:** 2026-01-23
**Issue:** https://github.com/open-horizon-labs/unified-hifi-control/issues/148

## Context

### Project
Source-agnostic hi-fi control platform bridging music sources (Roon, UPnP, HQPlayer, LMS) to any control surface (ESP32 knobs, web UI, Home Assistant, Claude MCP).

**Key Architecture Principle:** All backends are optional - any can contribute zones independently.

### Focus
Issue #148: Investigate zones_sha emission for dynamic zone detection

**Problem Statement:**
- Roon Knob issue muness/roon-knob#112 reports zones appearing after the Knob starts don't show in the zone picker
- PR muness/roon-knob#80 added `zones_sha` change detection on the Knob side - when the bridge reports a changed `zones_sha` in `/now_playing` responses, the Knob automatically refreshes its zone list
- The bridge needs to emit `zones_sha` in `/now_playing` responses for this to work

**Investigation Needed:**
1. Does the bridge currently emit `zones_sha` in `/now_playing` responses?
2. Does the bridge update `zones_sha` when the zone list changes (zones added/removed)?
3. If not, implement `zones_sha` emission to enable dynamic zone detection

### Current State Analysis

**Test harness expects `zones_sha`:**
- `tests/client_harness.rs` lines 139, 147 define `zones_sha: Option<String>` in response structs
- This indicates the expected API contract includes `zones_sha`

**Bridge does NOT currently emit `zones_sha`:**
- `src/knobs/routes.rs` `NowPlayingResponse` struct (lines 223-244) does NOT include `zones_sha` field
- Grep for `zones_sha` in `src/` returns no matches
- The field is expected by clients but not emitted by the server

**Required Implementation:**
1. Add `zones_sha: Option<String>` to `NowPlayingResponse` struct
2. Compute SHA of zone list (hash of zone IDs + names, or similar)
3. Include `zones_sha` in all `/knob/now_playing` responses
4. Update SHA when zones change (adapters add/remove zones)

### Constraints
- From AGENTS.md: Follow TDD - write test first, see it fail, then fix
- API Stability: Do NOT add/remove/modify API endpoints without explicit user approval
- Note: Adding `zones_sha` to existing response is likely acceptable (additive, backward-compatible)
- Multi-backend architecture: all sources optional
- Must handle zone changes from any adapter (Roon, LMS, OpenHome, UPnP)

## Workflow

1. **Verify the gap**
   - Run existing tests to confirm `zones_sha` expectation
   - Confirm field is missing from response

2. **Write failing test**
   - Test that `/knob/now_playing` includes `zones_sha` field
   - Test that `zones_sha` changes when zone list changes

3. **Implement fix**
   - Add `zones_sha` field to `NowPlayingResponse`
   - Compute hash of zone list
   - Track zone list version/SHA in aggregator or state

4. **Verify**
   - Test passes
   - Manual verification with actual knob if possible

5. **Stage, review, commit**
   - Run `sg review`
   - Clear commit message referencing #148

## Sources
- Issue: #148 (zones_sha emission)
- Related: muness/roon-knob#112 (zones not appearing)
- Related: muness/roon-knob#80 (client-side zones_sha detection)
- Local: `tests/client_harness.rs` (expected response format)
- Local: `src/knobs/routes.rs` (current implementation)
- Local: AGENTS.md (TDD guidelines, API stability)
