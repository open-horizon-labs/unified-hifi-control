# Dive Session

**Intent:** ship
**Started:** 2026-01-16
**Endeavor:** 80222d6d - Unified Hi-Fi Control
**Issue:** https://github.com/open-horizon-labs/unified-hifi-control/issues/62

## Context

### Project
Source-agnostic hi-fi control platform bridging music sources (Roon, UPnP, HQPlayer, LMS) to any control surface (ESP32 knobs, web UI, Home Assistant, Claude MCP).

**Key Architecture Principle:** All backends are optional - any can contribute zones independently.

### Focus
Issue #62: Bridge started from LMS should have LMS features auto-enabled

**Problem Statement:**
- Bridge is started via LMS plugin with env vars: `PORT=8088 LOG_LEVEL=info CONFIG_DIR=... LMS_HOST=127.0.0.1`
- Bridge starts successfully and is accessible on port 8088
- An `lmd-config.json` exists with correct localhost settings
- BUT: LMS integration is NOT automatically enabled
- User must manually enable LMS zone source

**Expected Behavior:**
- When `LMS_HOST` env var is passed, LMS backend should auto-enable
- Config from env vars should take precedence or auto-populate settings

**Related:**
- Possibly duplicate of #54 (env vars ignored)
- Recent commit 1cc6fdf mentions "LMS env var support" - may be incomplete

### Constraints
- From AGENTS.md: Follow TDD - write test first, see it fail, then fix
- Multi-backend architecture: all sources optional
- Must maintain backwards compatibility with existing config files

## Workflow

1. **Understand current behavior**
   - How does LMS backend get enabled currently?
   - Where are env vars read?
   - What does commit 1cc6fdf actually implement?

2. **Write failing test**
   - Test that LMS backend auto-enables when LMS_HOST env var present

3. **Implement fix**
   - Wire env var to auto-enable LMS backend

4. **Verify**
   - Test passes
   - Manual verification if possible

5. **Stage, review, commit**
   - Run `sg review`
   - Clear commit message

## Sources
- Issue: #62 (michaelherger)
- Related: #54 (env vars ignored)
- Commit: 1cc6fdf (recent LMS env var support)
- Local: AGENTS.md (TDD guidelines)
