// Unit tests for public/webmcp-bridge.js (#579).
//
// Covers the pure translation functions (SSE framing, tool policy,
// envelope -> CallToolResult) plus stateless request metadata and CSRF header
// wiring of `Bridge`, driven with a stub `fetch` so no server is needed.
//
// Run with: node --test tests/webmcp_bridge.test.js

const test = require("node:test");
const assert = require("node:assert/strict");
const path = require("node:path");

const bridge = require(path.join("..", "public", "webmcp-bridge.js"));

// ---------------------------------------------------------------------------
// parseSseJson: the transport is SSE-framed (enable_json_response: false in
// src/mcp/mod.rs), so every response arrives as `data: {...}` lines.
// ---------------------------------------------------------------------------

test("parseSseJson extracts the JSON-RPC envelope from an SSE data: line", () => {
  const body = 'event: message\ndata: {"jsonrpc":"2.0","id":1,"result":{"ok":true}}\n\n';
  assert.deepEqual(bridge.parseSseJson(body), { jsonrpc: "2.0", id: 1, result: { ok: true } });
});

test("parseSseJson falls back to plain JSON if the transport is ever switched", () => {
  const body = '{"jsonrpc":"2.0","id":1,"result":{"ok":true}}';
  assert.deepEqual(bridge.parseSseJson(body), { jsonrpc: "2.0", id: 1, result: { ok: true } });
});

test("parseSseJson returns null for unparseable bodies", () => {
  assert.equal(bridge.parseSseJson("not json at all"), null);
  assert.equal(bridge.parseSseJson(""), null);
  assert.equal(bridge.parseSseJson(undefined), null);
});

// ---------------------------------------------------------------------------
// Tool policy: playback/content plane allowlist, deny-by-default for
// anything unrecognized, deny patterns as defense in depth.
// ---------------------------------------------------------------------------

test("isToolAllowed permits every tool in tests/fixtures/mcp_tools.json", () => {
  // Mirrors the canonical tool set pinned by tests/mcp_contract.rs. Kept as
  // a literal list (not a fixture read) so this test does not silently pass
  // if the fixture and the allowlist drift apart -- a new fixture tool name
  // has to be added to both this test and the bridge's allowlist by hand.
  const currentTools = [
    "hifi_apple_music",
    "hifi_capabilities",
    "hifi_collections",
    "hifi_control",
    "hifi_hqplayer_load_profile",
    "hifi_hqplayer_profiles",
    "hifi_hqplayer_set_pipeline",
    "hifi_hqplayer_status",
    "hifi_now_playing",
    "hifi_play",
    "hifi_play_ref",
    "hifi_queue",
    "hifi_search",
    "hifi_spotify",
    "hifi_status",
    "hifi_zone_group",
    "hifi_zones",
  ];
  for (const name of currentTools) {
    assert.equal(bridge.isToolAllowed(name), true, `${name} should be allowed`);
  }
});

test("isToolAllowed denies an unlisted tool name by default", () => {
  assert.equal(bridge.isToolAllowed("hifi_some_future_tool"), false);
});

test("isToolAllowed denies admin/owner/credential-shaped names even if allowlisted by mistake", () => {
  // Simulates #543's hifi_admin landing on the server: even a name that were
  // (incorrectly) added to TOOL_ALLOWLIST must still be refused.
  const allowlist = bridge.TOOL_ALLOWLIST;
  assert.ok(!allowlist.includes("hifi_admin"), "hifi_admin must not be in the allowlist");
  assert.equal(bridge.isToolAllowed("hifi_admin"), false);
  assert.equal(bridge.isToolAllowed("hifi_admin_reset_credentials"), false);
  assert.equal(bridge.isToolAllowed("owner_delete_everything"), false);
});

test("isToolAllowed rejects non-string/empty input", () => {
  assert.equal(bridge.isToolAllowed(""), false);
  assert.equal(bridge.isToolAllowed(undefined), false);
  assert.equal(bridge.isToolAllowed(null), false);
});

// ---------------------------------------------------------------------------
// envelopeToCallToolResult: JSON-RPC envelope -> WebMCP CallToolResult
// ---------------------------------------------------------------------------

test("envelopeToCallToolResult passes a successful result straight through", () => {
  const envelope = {
    jsonrpc: "2.0",
    id: 3,
    result: { content: [{ type: "text", text: "42 zones" }], isError: false },
  };
  assert.deepEqual(bridge.envelopeToCallToolResult(envelope, "hifi_zones"), {
    content: [{ type: "text", text: "42 zones" }],
    isError: false,
  });
});

test("envelopeToCallToolResult turns a JSON-RPC error into an isError result", () => {
  const envelope = { jsonrpc: "2.0", id: 3, error: { code: -32602, message: "invalid params" } };
  const result = bridge.envelopeToCallToolResult(envelope, "hifi_control");
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /hifi_control/);
  assert.match(result.content[0].text, /invalid params/);
});

test("envelopeToCallToolResult handles a null/unparseable envelope", () => {
  const result = bridge.envelopeToCallToolResult(null, "hifi_status");
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /no result/);
});

// ---------------------------------------------------------------------------
// Bridge: stateless tools/list / tools/call and CSRF headers.
// ---------------------------------------------------------------------------

function fakeHeaders(map) {
  return { get: (name) => map[name.toLowerCase()] || null };
}

function sseResponse(jsonBody, headerMap) {
  return {
    ok: true,
    headers: fakeHeaders(headerMap || {}),
    text: async () => `data: ${JSON.stringify(jsonBody)}\n\n`,
  };
}

test("Bridge uses stateless MCP metadata and routing headers for every call", async () => {
  const calls = [];
  const b = new bridge.Bridge(async (_url, init) => {
    const req = JSON.parse(init.body);
    calls.push(req.method);
    assert.equal(init.headers["mcp-session-id"], undefined);
    assert.equal(init.headers["MCP-Protocol-Version"], "2026-07-28");
    assert.equal(init.headers["Mcp-Method"], req.method);
    assert.equal(req.params._meta["io.modelcontextprotocol/protocolVersion"], "2026-07-28");
    assert.deepEqual(req.params._meta["io.modelcontextprotocol/clientCapabilities"], {});
    if (req.method === "tools/list") return sseResponse({jsonrpc:"2.0",id:req.id,result:{tools:[{name:"hifi_zones"},{name:"hifi_admin"}]}});
    assert.equal(init.headers["Mcp-Name"], "hifi_zones");
    return sseResponse({jsonrpc:"2.0",id:req.id,result:{content:[],resultType:"complete"}});
  }, null);
  assert.deepEqual((await b.listAllowedTools()).map(t => t.name), ["hifi_zones"]);
  assert.equal((await b.callTool("hifi_zones", {})).resultType, "complete");
  assert.deepEqual(calls, ["tools/list", "tools/call"]);
});

test("Bridge.callTool refuses a denied tool without making any request", async () => {
  let fetchCalled = false;
  const fetchStub = async () => {
    fetchCalled = true;
    throw new Error("should not be called");
  };
  const b = new bridge.Bridge(fetchStub, null);
  const result = await b.callTool("hifi_admin", {});
  assert.equal(result.isError, true);
  assert.match(result.content[0].text, /not permitted/);
  assert.equal(fetchCalled, false);
});

test("Bridge attaches x-uhc-csrf-token from storage on every request, mirroring src/app/controller_auth.rs", async () => {
  const storage = { getItem: (key) => (key === "uhc_controller_csrf" ? "csrf-token-abc" : null) };
  const seenTokens = [];
  const fetchStub = async (url, init) => {
    seenTokens.push(init.headers["x-uhc-csrf-token"]);
    const req = JSON.parse(init.body);
    if (req.method === "initialize") {
      return sseResponse({ jsonrpc: "2.0", id: req.id, result: {} }, { "mcp-session-id": "sess-xyz" });
    }
    return sseResponse({ jsonrpc: "2.0", id: req.id, result: {} }, {});
  };
  const b = new bridge.Bridge(fetchStub, storage);
  await b.callTool("hifi_status", {});
  assert.ok(seenTokens.every((t) => t === "csrf-token-abc"));
});

test("Bridge never retries a failed tool call and the next request needs no handshake", async () => {
  const calls = [];
  const b = new bridge.Bridge(async (_url, init) => {
    calls.push(JSON.parse(init.body).method);
    throw new Error("network down");
  }, null);
  assert.equal((await b.callTool("hifi_status", {})).isError, true);
  assert.deepEqual(calls, ["tools/call"]);
  b._fetch = async (_url, init) => {
    calls.push(JSON.parse(init.body).method);
    return sseResponse({jsonrpc:"2.0",id:2,result:{content:[],resultType:"complete"}});
  };
  assert.equal((await b.callTool("hifi_status", {})).isError, undefined);
  assert.deepEqual(calls, ["tools/call", "tools/call"]);
});

// ---------------------------------------------------------------------------
// Base-path awareness (#581): behind HA Ingress the server injects a
// uhc-base-path meta tag; the bridge must issue /mcp under that prefix.
// ---------------------------------------------------------------------------

function docWithMeta(content) {
  return {
    querySelector: (sel) =>
      sel === 'meta[name="uhc-base-path"]' && content !== null
        ? { getAttribute: (n) => (n === "content" ? content : null) }
        : null,
  };
}

test("docBasePath reads the injected meta tag and trims trailing slashes", () => {
  assert.equal(bridge.docBasePath(docWithMeta("/api/hassio_ingress/tok")), "/api/hassio_ingress/tok");
  assert.equal(bridge.docBasePath(docWithMeta("/api/hassio_ingress/tok/")), "/api/hassio_ingress/tok");
});

test("docBasePath is empty in direct mode and for degenerate values", () => {
  assert.equal(bridge.docBasePath(docWithMeta(null)), "");
  assert.equal(bridge.docBasePath(docWithMeta("")), "");
  assert.equal(bridge.docBasePath(docWithMeta("/")), "");
  assert.equal(bridge.docBasePath(docWithMeta("not-rooted")), "");
  assert.equal(bridge.docBasePath(undefined), "");
});

test("Bridge posts to the prefixed MCP endpoint when constructed with one", async () => {
  const urls = [];
  const fetchStub = async (url, init) => {
    urls.push(url);
    const req = JSON.parse(init.body);
    if (req.method === "initialize") {
      return sseResponse({ jsonrpc: "2.0", id: req.id, result: {} }, { "mcp-session-id": "sess-1" });
    }
    return sseResponse({ jsonrpc: "2.0", id: req.id, result: { content: [], isError: false } }, {});
  };
  const b = new bridge.Bridge(fetchStub, null, "/api/hassio_ingress/tok/mcp");
  await b.callTool("hifi_status", {});
  assert.ok(urls.length > 0);
  assert.ok(urls.every((u) => u === "/api/hassio_ingress/tok/mcp"));
});

test("Bridge defaults to /mcp when no endpoint is given (direct mode)", async () => {
  const urls = [];
  const fetchStub = async (url, init) => {
    urls.push(url);
    const req = JSON.parse(init.body);
    if (req.method === "initialize") {
      return sseResponse({ jsonrpc: "2.0", id: req.id, result: {} }, { "mcp-session-id": "sess-1" });
    }
    return sseResponse({ jsonrpc: "2.0", id: req.id, result: { content: [], isError: false } }, {});
  };
  const b = new bridge.Bridge(fetchStub, null);
  await b.callTool("hifi_status", {});
  assert.ok(urls.every((u) => u === "/mcp"));
});
