# WebMCP (alpha)

**Status: alpha.** [WebMCP](https://blog.cloudflare.com/webmcp/) is an emerging
browser standard (`document.modelContext`), shipping *experimentally* behind a
flag/origin trial in Chrome 146 at the time this was written. It is not yet
available in a shipping, unflagged browser. Nothing here changes UHC's MCP
server or its wire protocol -- this is an additional, optional in-page bridge.

## What it does

`public/webmcp-bridge.js` is loaded on every page of UHC's web UI. On page
load it:

1. Feature-detects `document.modelContext`. If the browser doesn't have it,
   the script does nothing else -- zero behavior change, no console noise.
2. Opens a session against UHC's own `/mcp` endpoint (the same MCP server any
   external client like Claude Code talks to -- see the [MCP Server
   section](../README.md#mcp-server-claude-integration) of the README) and
   fetches `tools/list`.
3. Registers each tool the [tool policy](#tool-policy) allows with
   `document.modelContext.registerTool(...)`, using the name, description,
   and `inputSchema` UHC's MCP server already advertises -- verbatim, no
   second copy of any tool's shape.
4. Each registered tool's `execute()` posts a `tools/call` to `/mcp` and
   passes the `CallToolResult` straight through.

So a WebMCP-capable browser agent visiting UHC's web UI can search, play, and
control zones through the exact same tools an MCP client gets over HTTP --
without any MCP client configuration.

## Tool policy

UHC's MCP surface today is entirely playback/content tools (`hifi_zones`,
`hifi_control`, `hifi_search`, ...). [#543](https://github.com/open-horizon-labs/unified-hifi-control/issues/543)
plans an owner/admin tool (`hifi_admin`) for credential and
provider-configuration operations.

A WebMCP tool runs with the *page's* ambient authority -- there is no
separate consent step per tool the way an external MCP client integration
gets. A visiting browser agent must not automatically inherit owner/admin
operations just because the browser happens to hold an owner-bootstrapped
controller session. So the bridge is **deny-by-default**, in
`public/webmcp-bridge.js`:

- `TOOL_ALLOWLIST` is the explicit, curated list of tool names the bridge
  will ever register. A tool the server starts advertising is **not**
  exposed here until a person adds it to this list on purpose.
- `TOOL_DENY_PATTERNS` is defense in depth on top of that: even if a future
  admin/owner tool were mistakenly added to the allowlist, a name that looks
  like admin/owner/credential surface (`/^hifi_admin/i`, `/\bowner\b/i`,
  `/\badmin\b/i`, `/\bcredential/i`) is still refused.

When `hifi_admin` (or any future owner-only tool) lands, it must **not** be
added to `TOOL_ALLOWLIST`.

## Controller-auth

`/mcp` is a protected path in `src/api/controller_auth.rs`. By default
(`UHC_REQUIRE_CONTROLLER_AUTH` unset) that gate is a no-op for LAN
compatibility. When an operator opts in, every non-GET/HEAD request needs
the session cookie (sent automatically by the browser, same-origin) plus the
`x-uhc-csrf-token` double-submit header. The bridge reads that token from the
exact `localStorage` key the Dioxus app writes it to
(`uhc_controller_csrf`, see `CSRF_STORAGE_KEY` in
`src/app/controller_auth.rs`) -- it authenticates exactly as the rest of the
UI does, with no separate credential and no new attack surface.

## Session lifecycle

UHC's `/mcp` endpoint uses the MCP Streamable HTTP transport with
`enable_json_response: false` (see `src/mcp/mod.rs`), so every response --
even a single request/response exchange -- is SSE-framed
(`data: {...}\n\n`), and every call after `initialize` must carry the
`mcp-session-id` header the server returned. The bridge opens exactly one
session per page load, lazily, the first time a tool is registered or
called, and drops it (forcing a fresh `initialize` on the next call) if a
request fails outright.

## Verification

- `node --test tests/webmcp_bridge.test.js` unit-tests the pure
  SSE-envelope-to-`CallToolResult` translation and the tool policy against a
  stubbed `fetch`, with no server required.
- `cargo test --test mcp_contract` continues to pin the underlying `/mcp`
  wire shapes this bridge is a thin client of.
- Manual runtime verification (no shipping browser exposes
  `document.modelContext` yet) is done by stubbing it in via the browser
  console against a real `make web-run` server and driving one
  `registerTool`/`execute` cycle by hand -- see the PR description for a
  transcript.
