# Agent Guidelines

## Task Management

Use GitHub for all task tracking:

### GitHub Issues
**Purpose:** Describe problems and desired outcomes
- Create issues for bugs, features, and improvements
- Focus on the *what* and *why*, not implementation details
- Reference related issues with `#123` syntax
- Add OH endeavor ID for cross-reference: `OH: 80222d6d` (enables agent linkage to Open Horizons context)

### Branches
**Purpose:** Isolate work in progress
- Create a branch for each issue: `fix/issue-123-description` or `feat/issue-123-description`
- Base Rust work on `v3`, not `master`
- Keep branches focused on a single issue

### Pull Requests
**Purpose:** Propose implementations for review
- Link to the issue being addressed: `Fixes #123`
- Describe what changed and how to test
- Request review from coderabbit and superego

---

## Code Review

This project uses two complementary review tools:

### superego (Metacognitive Advisor)
**When to use:** Before commits, when choosing between approaches, when uncertain
**Protocol:**
- Mode: `pull` (reviews on request, not automatically)
- Use `sg review` at decision points during development
- Post superego reviews to PRs for visibility
- Handle findings: P1-P3 fix immediately, P4 can discard with reason

### coderabbit (Automated Code Review)
**When to use:** Automatically runs on all PRs
**Protocol:**
- Reviews code style, potential bugs, and best practices
- Address feedback before merging
- Use `@coderabbit` in PR comments to ask questions

### wm (Working Memory)
**When to use:** Automatic - captures learnings from sessions
**Protocol:**
- Runs automatically via hooks
- Extracts tacit knowledge from completed work
- No manual intervention needed

---

## Branch Strategy

- `master` = Node.js v2.x (legacy, stable)
- `v3` = Rust v3.x (active development)

**Default branch for Rust development: `v3`**

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

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the event bus pattern and design principles.

**Key insight:** Complexity should be absorbed by the bus, not distributed across components. When in doubt:
- Adapters are dumb (discover, translate, handle commands)
- Aggregator owns state (merge, hydrate, track last-seen)
- UI talks to aggregator only (never directly to backends)
