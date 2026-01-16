# Agent Guidelines

## Open Horizon Labs Tools

This project uses three complementary tools:

### ba (Task Tracker)
**When to use:** Tracking work items, managing tasks across sessions
**Protocol:**
- `ba ready` - See claimable tasks
- `ba claim <id> --session $SESSION_ID` - Claim a task
- `ba complete <id>` - Mark task done
- `ba create "description" -t task` - Create new task

### superego (Metacognitive Advisor)
**When to use:** Before commits, when choosing between approaches, when uncertain
**Protocol:**
- Mode: `pull` (reviews on request, not automatically)
- Use `sg review` at decision points
- Handle findings: P1-P3 fix immediately, P4 can discard with reason

### wm (Working Memory)
**When to use:** Automatic - captures learnings from sessions
**Protocol:**
- Runs automatically via hooks
- Extracts tacit knowledge from completed work
- No manual intervention needed

---

## Branch Strategy

**Default branch for Rust development: `v3`**

- `master` = Node.js v2.x (legacy, stable)
- `v3` = Rust v3.x (active development)

When creating PRs or branches for Rust work, base them on `v3`, not `master`.

## Test-Driven Development (TDD)

When fixing bugs or implementing features that affect client behavior:

1. **Write the test FIRST** - Express what the client expects in the test harness
2. **Run the test and see it FAIL** - Verify the test catches the bug
3. **ONLY THEN fix the code** - Implement the fix to make the test pass

**DO NOT:**
- Look at the Rust implementation and change the test harness to match
- Fix the code before seeing the test fail

**DO:**
- Look at what the CLIENT expects (knob C code, iOS Swift, Node.js server)
- Write a test expressing that expectation
- Verify the test fails against the current implementation
- Then fix the implementation

## Client Test Harness

The test harness (`tests/client_harness.rs`) simulates:
- ESP32 Knob client (roon-knob C firmware)
- iOS/Apple Watch client (hifi-control-ios Swift app)

Reference implementations for expected behavior:
- Node.js server: `/Users/muness1/src/unified-hifi-control/`
- Knob firmware: C code defining expected API responses
- iOS app: Swift BridgeClient defining expected API format

## API Compatibility

The Rust server must be a drop-in replacement for the Node.js server. All API responses must match the Node.js format exactly, including:
- Response structure (`{zones: [...]}` not bare arrays)
- Field names (`zone_name` not `display_name`)
- Zone ID prefixes (`roon:`, `openhome:`, `upnp:`, `lms:`)
- Error response formats
