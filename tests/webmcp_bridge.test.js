// Unit tests for public/webmcp-bridge.js (#579).
//
// Covers the pure translation functions (SSE framing, tool policy,
// envelope -> CallToolResult) plus the session lifecycle and CSRF header
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
// Bridge: session lifecycle (initialize -> notifications/initialized ->
// tools/list / tools/call) and CSRF header attachment, against a stub fetch.
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

test("Bridge opens exactly one session across tools/list and a tools/call", async () => {
  const calls = [];
  let nextResultId = 1;
  const fetchStub = async (url, init) => {
    const req = JSON.parse(init.body);
    calls.push({ method: req.method, headers: init.headers, sessionHeaderSent: init.headers["mcp-session-id"] });
    if (req.method === "initialize") {
      return sseResponse({ jsonrpc: "2.0", id: req.id, result: { protocolVersion: "2025-11-25" } }, {
        "mcp-session-id": "sess-123",
      });
    }
    if (req.method === "notifications/initialized") {
      return sseResponse({}, {});
    }
    if (req.method === "tools/list") {
      return sseResponse({
        jsonrpc: "2.0",
        id: req.id,
        result: {
          tools: [
            { name: "hifi_zones", description: "list zones", inputSchema: { type: "object" } },
            { name: "hifi_admin", description: "danger", inputSchema: { type: "object" } },
          ],
        },
      });
    }
    if (req.method === "tools/call") {
      return sseResponse({
        jsonrpc: "2.0",
        id: req.id,
        result: { content: [{ type: "text", text: "ok" }], isError: false },
      });
    }
    throw new Error("unexpected method " + req.method);
  };

  const b = new bridge.Bridge(fetchStub, null);
  const tools = await b.listAllowedTools();
  // hifi_admin must never reach the caller, even though the (stubbed) server
  // advertised it -- this is the tool-policy filter, not the server's job.
  assert.deepEqual(tools.map((t) => t.name), ["hifi_zones"]);

  const result = await b.callTool("hifi_zones", {});
  assert.deepEqual(result, { content: [{ type: "text", text: "ok" }], isError: false });

  const methodsCalled = calls.map((c) => c.method);
  assert.deepEqual(methodsCalled, [
    "initialize",
    "notifications/initialized",
    "tools/list",
    "tools/call",
  ]);
  // Every call after initialize carries the session id the server minted.
  assert.equal(calls[1].sessionHeaderSent, "sess-123");
  assert.equal(calls[2].sessionHeaderSent, "sess-123");
  assert.equal(calls[3].sessionHeaderSent, "sess-123");
  // initialize itself has no prior session to send.
  assert.equal(calls[0].sessionHeaderSent, undefined);
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

test("Bridge.callTool recovers from a transport failure as an isError result and re-initializes next time", async () => {
  let initializeCalls = 0;
  const fetchStub = async (url, init) => {
    const req = JSON.parse(init.body);
    if (req.method === "initialize") {
      initializeCalls++;
      return sseResponse({ jsonrpc: "2.0", id: req.id, result: {} }, { "mcp-session-id": "sess-" + initializeCalls });
    }
    if (req.method === "notifications/initialized") {
      return sseResponse({}, {});
    }
    if (req.method === "tools/call") {
      throw new Error("network down");
    }
    throw new Error("unexpected " + req.method);
  };
  const b = new bridge.Bridge(fetchStub, null);
  const first = await b.callTool("hifi_status", {});
  assert.equal(first.isError, true);
  assert.match(first.content[0].text, /network down/);
  assert.equal(initializeCalls, 1);

  // The dropped session forces a fresh initialize on the next call.
  const fetchStub2Calls = [];
  b._fetch = async (url, init) => {
    const req = JSON.parse(init.body);
    fetchStub2Calls.push(req.method);
    if (req.method === "initialize") {
      return sseResponse({ jsonrpc: "2.0", id: req.id, result: {} }, { "mcp-session-id": "sess-2" });
    }
    return sseResponse({ jsonrpc: "2.0", id: req.id, result: { content: [], isError: false } }, {});
  };
  await b.callTool("hifi_status", {});
  assert.ok(fetchStub2Calls.includes("initialize"), "a dropped session must be re-initialized");
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
