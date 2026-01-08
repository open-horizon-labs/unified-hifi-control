# Agent Development Guidelines

This document defines hard constraints and best practices for AI agents working on this codebase.

## Hard Constraints

### 1. Safety-Critical Code MUST Have Regression Tests

**Constraint:** Any code that can cause physical harm or equipment damage MUST have explicit regression tests in CI before shipping.

**Scope:**
- Volume control (can send dangerous levels)
- Zone routing (can send commands to wrong device)
- Command execution (can trigger unintended actions)
- Firmware updates (can brick devices)
- Audio pipeline control (can cause distortion/clipping)

**Requirements:**
1. **Test the exact failure mode** - Don't just test happy path
2. **Make danger explicit** - Test names and comments must explain why it's critical
3. **Run in CI** - Tests must gate merges, not be optional
4. **Verify fix prevents original bug** - If code reverts, test must fail

**Example - Volume Safety:**
```javascript
test('CRITICAL: -12 dB stays -12 dB (not clamped to 0)', () => {
  // This prevents sending maximum volume (0 dB) when user wants -12 dB
  // which could damage equipment or hearing
  const { min, max } = getVolumeRange(dbOutput);
  expect(clamp(-12, min, max)).toBe(-12);  // Must be -12, NOT 0
});
```

**Historical Context:**
- **2026-01-08:** Volume safety bug shipped - `vol_abs` clamped dB values to 0-100, sending maximum volume (0 dB) instead of safe levels like -12 dB. Fixed with regression tests in v2.5.0.

### 2. Multi-Scale Assumptions Require Explicit Handling

**Constraint:** Never assume a single scale for values that can have multiple representations.

**Scope:**
- Volume (dB vs percentage vs fixed)
- Seek position (seconds vs samples vs percentage)
- Image dimensions (pixels vs aspect ratio)

**Requirements:**
1. Read zone/device metadata for actual scale
2. Use metadata to inform clamping/conversion
3. Test multiple scales explicitly

### 3. Backward Compatibility for Persisted Data

**Constraint:** Changes to persisted data formats (config files, settings) MUST support migration from previous versions.

**Requirements:**
1. Detect old format
2. Convert to new format automatically
3. Log migration for debugging
4. Test old→new conversion path

**Example:**
```javascript
// Support both old single-instance and new multi-instance format
if (Array.isArray(data)) {
  configs = data;  // New format
} else if (data.host) {
  // Old format: migrate to array
  configs = [{ name: data.name || data.host, ...data }];
  log.info('Migrated config from single to multi-instance format');
}
```

## Best Practices

### Test Design
- Keep tests simple - test one thing clearly
- Prefer extracted functions over complex mocks
- 60 lines of clear tests > 200 lines of mock setup

### Code Comments
- Add safety comments at decision points
- Reference test files for regression protection
- Explain "why" not "what"

### Documentation
- Update docs/ when architecture changes
- Keep CHANGELOG.md current
- Document breaking changes prominently

## CI Pipeline

All PRs must pass:
1. **Lint** - ESLint checks code quality
2. **Test** - Jest runs all safety and unit tests
3. **Build** - Docker builds for amd64/arm64/arm

Tests run before build - no shipping broken safety checks.
