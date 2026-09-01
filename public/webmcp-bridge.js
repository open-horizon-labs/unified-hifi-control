// WebMCP bridge (#579): exposes UHC's own MCP tools on `document.modelContext`
// so a WebMCP-capable browser agent (shipping experimentally in Chrome 146 --
// https://blog.cloudflare.com/webmcp/) can discover and call them without any
// MCP client configuration.
//
// This file is a plain static asset (see `serve_static_file` in
// src/embedded.rs and the `/webmcp-bridge.js` route in src/main.rs) -- it is
// NOT part of the Dioxus/wasm bundle. It is loaded on every page via
// `document::Script` in src/app/components/layout.rs.
//
// # Zero-duplication design
//
// The bridge does not define its own tool catalog. On load it fetches UHC's
// own MCP `tools/list` from the same origin and registers each allowed tool
// verbatim (name/description/inputSchema straight from the server). Every
// `execute()` posts a `tools/call` to the same MCP endpoint and passes the
// `CallToolResult` straight through. See src/mcp/handler.rs and
// tests/mcp_contract.rs for the wire shapes this mirrors.
//
// # Tool policy (playback/content plane only)
//
// UHC's MCP surface today is entirely playback/content tools (hifi_zones,
// hifi_control, hifi_search, ...). issue #543 plans an owner/admin tool
// (`hifi_admin`) for credential and provider-configuration operations. A
// WebMCP-capable visitor to UHC's web UI must not automatically inherit
// those just because the browser session happens to be owner-bootstrapped
// (WebMCP tools run with the *page's* ambient authority, not a distinct
// grant). So this bridge is deny-by-default:
//
//   - `TOOL_ALLOWLIST` is the explicit, curated list of tool names this
//     bridge will ever register. A new tool the server starts advertising is
//     NOT exposed here until someone adds it to this list on purpose.
//   - `TOOL_DENY_PATTERNS` is defense in depth on top of that: even if a
//     future admin/owner tool is (mistakenly) added to the allowlist, a name
//     or description that looks like admin/owner/credential surface is
//     refused.
//
// # Session lifecycle
//
// UHC's `/mcp` endpoint uses the MCP Streamable HTTP transport
// (`enable_json_response: false` in src/mcp/mod.rs), so every response is
// SSE-framed even for a single request/response exchange, and every call
// after `initialize` must carry the `mcp-session-id` header it returned.
// This bridge opens exactly one session per page load, lazily, the first
// time it is needed.
//
// # Controller-auth (#570)
//
// `/mcp` is a protected path in src/api/controller_auth.rs. By default
// (`UHC_REQUIRE_CONTROLLER_AUTH` unset) that gate is a no-op, but when the
// operator opts in, every non-GET/HEAD request needs the session cookie
// (sent automatically by the browser same-origin) plus the `x-uhc-csrf-token`
// double-submit header. This bridge reads that token from the exact
// `localStorage` key the Dioxus app writes it to
// (`CSRF_STORAGE_KEY` in src/app/controller_auth.rs), so it authenticates
// exactly as the UI does -- no separate credential, no new attack surface.
(function (root, factory) {
  if (typeof module === "object" && module.exports) {
    // Node, for unit tests (see tests/webmcp_bridge.test.js).
    module.exports = factory();
  } else {
    factory().install(root);
  }
})(typeof self !== "undefined" ? self : this, function () {
  "use strict";

  var MCP_ENDPOINT = "/mcp";
  // #581: mirrors BASE_PATH_META in src/app/base_path.rs. Behind an ingress
  // /subpath proxy the server injects this meta tag; the bridge must issue
  // its /mcp POSTs under the same prefix or they escape the proxy. Absent
  // tag (direct mode) resolves to "" and the endpoint stays "/mcp".
  var BASE_PATH_META = "uhc-base-path";
  var MCP_SESSION_HEADER = "mcp-session-id";
  var CSRF_HEADER = "x-uhc-csrf-token";
  // Mirrors `CSRF_STORAGE_KEY` in src/app/controller_auth.rs exactly -- this
  // is the one piece of bootstrap state a browser-side script can see (the
  // session cookie itself is HttpOnly).
  var CSRF_STORAGE_KEY = "uhc_controller_csrf";
  var PROTOCOL_VERSION = "2025-11-25";

  // Playback/content plane only. Kept in sync by hand with
  // tests/fixtures/mcp_tools.json; a tool absent from both this list and the
  // deny patterns is simply never registered, which is the safe failure
  // mode for a name this bridge does not recognize.
  var TOOL_ALLOWLIST = [
    "hifi_zones",
    "hifi_now_playing",
    "hifi_control",
    "hifi_search",
    "hifi_play",
    "hifi_play_ref",
    "hifi_queue",
    "hifi_status",
    "hifi_hqplayer_status",
    "hifi_hqplayer_profiles",
    "hifi_hqplayer_load_profile",
    "hifi_hqplayer_set_pipeline",
    "hifi_capabilities",
    "hifi_spotify",
    "hifi_apple_music",
    "hifi_collections",
    "hifi_zone_group",
  ];

  // Defense in depth: refuse anything that looks like owner/admin/credential
  // surface even if it were ever (mistakenly) added to the allowlist above.
  var TOOL_DENY_PATTERNS = [/^hifi_admin/i, /\bowner\b/i, /\badmin\b/i, /\bcredential/i];

  /// Whether `name` may be registered with `document.modelContext`, per the
  /// policy above. Exported for unit testing.
  function isToolAllowed(name) {
    if (typeof name !== "string" || name.length === 0) return false;
    for (var i = 0; i < TOOL_DENY_PATTERNS.length; i++) {
      if (TOOL_DENY_PATTERNS[i].test(name)) return false;
    }
    return TOOL_ALLOWLIST.indexOf(name) !== -1;
  }

  /// Extract the JSON-RPC envelope from an MCP Streamable HTTP response body:
  /// SSE-framed (`data: {...}`) today, or plain JSON if the transport is ever
  /// reconfigured with `enable_json_response: true`. Mirrors `parse_sse_json`
  /// in tests/mcp_contract.rs. Exported for unit testing.
  function parseSseJson(body) {
    if (typeof body !== "string") return null;
    var lines = body.split("\n");
    for (var i = 0; i < lines.length; i++) {
      var line = lines[i];
      if (line.slice(0, 5) === "data:") {
        try {
          return JSON.parse(line.slice(5).trim());
        } catch (e) {
          return null;
        }
      }
    }
    try {
      return JSON.parse(body);
    } catch (e) {
      return null;
    }
  }

  /// Translate a JSON-RPC envelope (already unwrapped from SSE by
  /// `parseSseJson`) into a WebMCP `CallToolResult`. UHC's `tools/call`
  /// result IS already a `CallToolResult` (`content` + `isError`), so a
  /// successful call is a straight passthrough of `envelope.result`; a
  /// JSON-RPC-level error (bad session, transport failure, malformed
  /// request) is translated into an `isError: true` result so a WebMCP
  /// caller never has to special-case JSON-RPC framing. Exported for unit
  /// testing.
  function envelopeToCallToolResult(envelope, toolName) {
    if (envelope && envelope.result) {
      return envelope.result;
    }
    if (envelope && envelope.error) {
      var message = envelope.error.message || "MCP call failed";
      return errorResult(toolName + ": " + message);
    }
    return errorResult(toolName + ": no result from MCP endpoint");
  }

  function errorResult(message) {
    return { content: [{ type: "text", text: String(message) }], isError: true };
  }

  /// The runtime base path the server advertised (see BASE_PATH_META), or
  /// "" when there is none. Exported for unit testing.
  function docBasePath(doc) {
    if (!doc || typeof doc.querySelector !== "function") return "";
    var meta = doc.querySelector('meta[name="' + BASE_PATH_META + '"]');
    var value = (meta && meta.getAttribute("content")) || "";
    value = value.replace(/\/+$/, "");
    // Same shape rule as base_path::normalize: a rooted, non-degenerate path.
    if (value.charAt(0) !== "/" || value.length < 2) return "";
    return value;
  }

  function Bridge(fetchImpl, storage, endpoint) {
    this._fetch = fetchImpl;
    this._storage = storage;
    this._endpoint = endpoint || MCP_ENDPOINT;
    this._sessionId = null;
    this._nextId = 1;
  }

  /// The double-submit CSRF token the Dioxus app stored, if any. Absent
  /// entirely when controller-auth's compatibility mode is on (the default)
  /// or before the owner has bootstrapped -- in both cases the header is
  /// simply omitted, exactly like `post_json` in src/app/api.rs.
  Bridge.prototype._csrfToken = function () {
    if (!this._storage) return null;
    try {
      return this._storage.getItem(CSRF_STORAGE_KEY);
    } catch (e) {
      return null;
    }
  };

  /// POST one JSON-RPC message to `/mcp` and return
  /// `{ sessionId, envelope }`, where `envelope` is the parsed JSON-RPC
  /// response (or null for a fire-and-forget notification / unparseable
  /// body).
  Bridge.prototype._rpc = function (method, params, sessionId) {
    var self = this;
    var headers = {
      "Content-Type": "application/json",
      Accept: "application/json, text/event-stream",
    };
    if (sessionId) headers[MCP_SESSION_HEADER] = sessionId;
    var token = self._csrfToken();
    if (token) headers[CSRF_HEADER] = token;

    var isNotification = method.indexOf("notifications/") === 0;
    var body = { jsonrpc: "2.0", method: method, params: params || {} };
    if (!isNotification) body.id = self._nextId++;

    return self
      ._fetch(self._endpoint, {
        method: "POST",
        headers: headers,
        credentials: "same-origin",
        body: JSON.stringify(body),
      })
      .then(function (res) {
        var newSessionId = res.headers && res.headers.get ? res.headers.get(MCP_SESSION_HEADER) : null;
        return res.text().then(function (text) {
          return { sessionId: newSessionId || sessionId || null, envelope: parseSseJson(text), ok: res.ok };
        });
      });
  };

  Bridge.prototype._ensureSession = function () {
    var self = this;
    if (self._sessionId) return Promise.resolve(self._sessionId);
    return self
      ._rpc(
        "initialize",
        {
          protocolVersion: PROTOCOL_VERSION,
          capabilities: {},
          clientInfo: { name: "uhc-webmcp-bridge", version: "1" },
        },
        null
      )
      .then(function (result) {
        if (!result.sessionId) {
          throw new Error("MCP initialize did not return an mcp-session-id");
        }
        self._sessionId = result.sessionId;
        // Fire-and-forget: the SDK does not reply to notifications.
        return self._rpc("notifications/initialized", {}, self._sessionId).then(function () {
          return self._sessionId;
        });
      });
  };

  /// `tools/list`, filtered through the tool policy. Returns the raw tool
  /// definitions (name/description/inputSchema) UHC advertised.
  Bridge.prototype.listAllowedTools = function () {
    var self = this;
    return self._ensureSession().then(function (sessionId) {
      return self._rpc("tools/list", {}, sessionId).then(function (result) {
        var tools = (result.envelope && result.envelope.result && result.envelope.result.tools) || [];
        return tools.filter(function (tool) {
          return isToolAllowed(tool && tool.name);
        });
      });
    });
  };

  /// Call one MCP tool and return a WebMCP `CallToolResult`. Never rejects:
  /// any failure (network, transport, stale session) comes back as
  /// `{ isError: true }` so a `document.modelContext` `execute()` handler
  /// can return it directly.
  Bridge.prototype.callTool = function (name, args) {
    var self = this;
    if (!isToolAllowed(name)) {
      return Promise.resolve(errorResult(name + ": not permitted by UHC's WebMCP tool policy"));
    }
    return self
      ._ensureSession()
      .then(function (sessionId) {
        return self._rpc("tools/call", { name: name, arguments: args || {} }, sessionId);
      })
      .then(function (result) {
        return envelopeToCallToolResult(result.envelope, name);
      })
      .catch(function (e) {
        // A stale/expired session (server restart, TTL) fails once here;
        // drop it so the *next* call re-initializes, rather than retrying
        // inline and risking a duplicate side-effecting tools/call.
        self._sessionId = null;
        return errorResult(name + ": " + (e && e.message ? e.message : String(e)));
      });
  };

  /// Register every allowed tool with `modelContext`. No-ops (resolves
  /// immediately) if `tools/list` fails or returns nothing allowed.
  Bridge.prototype.registerAll = function (modelContext) {
    var self = this;
    return self.listAllowedTools().then(function (tools) {
      tools.forEach(function (tool) {
        modelContext.registerTool({
          name: tool.name,
          description: tool.description,
          inputSchema: tool.inputSchema,
          execute: function (args) {
            return self.callTool(tool.name, args);
          },
        });
      });
      return tools;
    });
  };

  function install(root) {
    var doc = root && root.document;
    // Feature detection (per the WebMCP proposal): absent
    // `document.modelContext`, this is a complete no-op and the page behaves
    // exactly as it did before this script existed.
    if (!doc || !doc.modelContext || typeof doc.modelContext.registerTool !== "function") {
      return;
    }
    var bridge = new Bridge(
      root.fetch.bind(root),
      safeLocalStorage(root),
      docBasePath(doc) + MCP_ENDPOINT
    );
    bridge.registerAll(doc.modelContext).catch(function (e) {
      if (root.console && root.console.error) {
        root.console.error("WebMCP bridge: failed to register UHC tools", e);
      }
    });
  }

  function safeLocalStorage(root) {
    try {
      return root.localStorage || null;
    } catch (e) {
      return null;
    }
  }

  return {
    install: install,
    Bridge: Bridge,
    docBasePath: docBasePath,
    isToolAllowed: isToolAllowed,
    parseSseJson: parseSseJson,
    envelopeToCallToolResult: envelopeToCallToolResult,
    errorResult: errorResult,
    TOOL_ALLOWLIST: TOOL_ALLOWLIST,
  };
});
