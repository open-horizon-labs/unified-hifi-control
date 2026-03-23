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

### Merging PRs
**TEST BEFORE MERGING.** Do not merge PRs without testing.

1. **Wait for CI to pass** - Build must succeed
2. **Test the change** - Deploy to test environment or run locally
3. **Get user approval** - Do NOT auto-merge, even after reviews pass
4. **Only merge when explicitly told** - "merge it", "LGTM", "ship it"

**NEVER:**
- Merge a PR just because CI passed
- Merge without verifying the fix works
- Auto-merge in a "merge party" without explicit approval for each PR

### Git Workflow
**DO NOT force push (`git push --force` or `git push -f`)**
- This project uses squash merges, so commit history cleanup is unnecessary
- Force pushing breaks checkouts for anyone tracking the branch
- Force pushing loses SHA references (builds, comments, reviews)
- Just push new commits - they all get squashed on merge anyway

---

## Building & Running Locally

### Single-binary build (production packaging mode)
The server embeds WASM/JS assets so the released binary is self-contained. Two separate steps because `dx` compiles WASM and `cargo` embeds those assets into the server binary:

```bash
# 1. Compile WASM/JS assets into target/dx/ (re-run only when frontend code changes)
dx build --fullstack --release '@client' --no-default-features --features web '@server' --features server

# 2. Build server binary — embeds the assets produced in step 1
SDKROOT=/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk cargo build --release --bin unified-hifi-control --features server --no-default-features

# 3. Run the packaged binary (serves UI + API + mDNS + UDP on port 8088)
./target/release/unified-hifi-control
```

### Development mode (hot reload)
For iterating on the web UI without a full two-step build:

```bash
dx serve
```

`dx serve` compiles both WASM and server together with live reload. Use this for frontend development — **do not use `dx serve` output for deployment** as assets are served from disk, not embedded.

### NEVER use `cargo run` alone
`cargo run --bin unified-hifi-control` builds the server WITHOUT embedded WASM assets. It will crash looking for a `public/` directory. Always use `dx serve` for development or the two-step build above for production.

### Setting the license (for Configurator)
```bash
curl -X POST http://localhost:8088/api/config/license \
  -H "Content-Type: application/json" \
  -d '{"license":"<JWT>"}'
```

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

## API Stability (CRITICAL)

**DO NOT add, remove, or modify API endpoints without explicit user approval.**

This includes:
- Route paths in main.rs
- HTTP methods (GET/POST/PUT/DELETE)
- Response payload structure
- Request body schemas

### Enforcement

1. **Contract file**: `tests/fixtures/api_routes.txt` lists all routes
2. **Test**: `cargo test --test api_contract` fails if routes change
3. **CI**: PRs changing the contract require `api-change-approved` label

### If you think API needs to change

1. **ASK FIRST** - Describe the proposed change and get explicit approval
2. Only after approval: update `api_routes.txt` and implementation
3. User adds `api-change-approved` label to PR

**NEVER:**
- Add the `api-change-approved` label yourself
- Update `api_routes.txt` without explicit approval
- Assume API changes are "minor" or "safe"

## Engineering Philosophy

**Do things the right way, even if "larger" or "harder".**

### Terminology: "Refactor"

**Refactor means restructuring code WITHOUT changing behavior.**

- A refactor is a behavior-preserving transformation
- If you're adding a new type (e.g., `PrefixedZoneId`), wiring it through means updating ALL call sites to use it - not just adding the type definition
- "Refactor to use X" = find every place that should use X and change it
- Don't confuse "adding a type" with "refactoring to use a type"

- Correct architecture now prevents regressions later
- Short-term hacks create long-term maintenance burden
- When you see an architectural inconsistency, fix it - don't work around it
- If a component exists for a purpose (e.g., aggregator as "single source of truth"), USE IT

**Examples:**
- Don't bypass the aggregator to query adapters directly
- Don't cache computed values at the wrong layer
- Don't add settings-filtering at query time if it belongs at the source

---

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the event bus pattern and design principles.

**Key insight:** Complexity should be absorbed by the bus, not distributed across components. When in doubt:
- Adapters are dumb (discover, translate, handle commands)
- Aggregator owns state (merge, hydrate, track last-seen)
- UI talks to aggregator only (never directly to backends)

## MCP Server (AI Assistant Integration)

**Endpoint:** `http://<host>:8088/mcp` (Streamable HTTP transport)

MCP (Model Context Protocol) enables AI assistants (Claude, ChatGPT, BoltAI, etc.) to control music playback.

### Current Capabilities

| Feature | Roon | LMS | OpenHome | UPnP |
|---------|------|-----|----------|------|
| Discovery (zones) | ✅ | ✅ | ✅ | ✅ |
| Transport (play/pause/next) | ✅ | ✅ | ✅ | ✅ |
| Volume control | ✅ | ✅ | ❌ | ❌ |
| Search | ✅ | ✅ | ❌ | ❌ |
| Play by query | ✅ | ✅ | ❌ | ❌ |
| Queue building | ✅ | ✅ | ❌ | ❌ |

### Tool Design Principles

**Every tool must work as documented.** AI models try the "obvious" thing first - if it fails, they give up or confuse users.

- **No broken tools** - If a tool can't reliably do what its description says, remove it
- **No orphaned fields** - Don't return data (like `item_key`) that can't be used by any tool
- **Clear queue pattern** - Use `hifi_play` with `action='queue'` to build playlists; call multiple times for multiple tracks

**Tool descriptions are documentation.** AI models read descriptions to decide which tool to use. Be explicit about capabilities:
- ❌ "Searches and immediately plays" (hides queue capability)
- ✅ "Searches and plays, queues, or starts radio. Use action='queue' to add to queue."

### Current Tools (10)

| Tool | Purpose |
|------|---------|
| `hifi_zones` | List all playback zones |
| `hifi_now_playing` | Get current track info |
| `hifi_control` | Play, pause, next, previous, volume |
| `hifi_search` | Search library/streaming with rich metadata (title, artist, album, year, genre, duration, image_url) and `_play` token |
| `hifi_play_item` | Play a search result via `_play` token from hifi_search |
| `hifi_status` | Bridge connection status |
| `hifi_hqplayer_status` | HQPlayer pipeline status |
| `hifi_hqplayer_profiles` | List HQPlayer configs |
| `hifi_hqplayer_load_profile` | Load HQPlayer config |
| `hifi_hqplayer_set_pipeline` | Change HQPlayer setting |
| `esp_get_manifest` | Get current manifest for a zone's ESP device |
| `esp_configure` | Configure device layout via natural language (license-gated) |
| `esp_get_actions` | List all available element actions |
| `esp_get_icons` | List all firmware-renderable icons |

---

## UI Stack

**Web UI:** Dioxus + Tailwind CSS + DioxusLabs components

- **Dioxus** - Fullstack Rust framework (SSR + WASM hydration)
- **Tailwind CSS v4** - Utility-first styling via `src/input.css` (standalone CLI, no Node.js)
- **DioxusLabs/components** - Accessible primitives (Navbar, Button, Collapsible, etc.)

**DO NOT use:**
- Bootstrap, Pico CSS, or other CSS frameworks
- Hand-rolled navigation with onclick handlers
- Inline styles for common patterns (use Tailwind utilities)

**Build CSS:** `make css` or `make css-watch` (auto-downloads standalone CLI)

### Building & Running

**CRITICAL: Dioxus fullstack requires `dx build`, not `cargo build`**

The web UI uses SSR + WASM hydration. This means:
1. Server renders HTML (SSR)
2. Client loads WASM bundle
3. WASM "hydrates" the DOM (attaches event handlers, starts futures)

Without the WASM bundle, components render but don't work (no navigation, no button clicks, no data loading).

**Correct workflow:**
```bash
# Build both server + WASM client
dx build --release --platform web

# Run from the dx output directory (contains public/wasm/ assets)
./target/dx/unified-hifi-control/release/web/unified-hifi-control
```

**Why `cargo run` doesn't work:**
- Only builds the server, not the WASM client
- Server panics looking for `public/wasm/` which doesn't exist
- Even if it doesn't panic, no hydration = no interactivity

**For development:** Use `dx serve` which handles both builds and hot reload.

**Verify build:** Use `dx build --release --platform web --features web` to verify both server and WASM compile. Do NOT use `cargo check --target wasm32-unknown-unknown` - it lacks correct feature flags. See [README.md Development section](README.md#verify-build-wasm--server) for details.
