# NAA proxy integration kit

This preserves the complete private NAA proxy PoC inside the active UHC v4 source
line and adds a tested headless API/MCP bridge. It is an isolated experiment,
**not yet wired into UHC's HQPlayer adapter, aggregator, UI, or MCP server**.
It is not enabled or packaged by the normal UHC build.

Read [INTEGRATION.md](INTEGRATION.md) for salvage, the selected architecture,
all-feature migration map, API/MCP contract, risk checks and implementation order.
`IMPORT.json` pins every imported source/evidence file. The original checkout
remains intact. No vendor runtime, credentials, private captures, or compiled
binaries are included. The source snapshot does not establish new redistribution
terms; retain the original private integration boundary.

## Build and headless use

From this directory:

```sh
cargo build --release --manifest-path native/naa-router/Cargo.toml
native/naa-router/target/release/naa-router --config ./routes.json
```

That starts a loopback-only PoC. The imported [router README](native/naa-router/README.md)
documents explicit LAN bind, HQPlayer allow-list, discovery interface, and optional
standalone native transport control. There is no automatic household selection.

Every operation is callable without a web UI:

```sh
python3 tools/router_mcp.py --router-url http://127.0.0.1:8787 --call naa_status
python3 tools/router_mcp.py --router-url http://127.0.0.1:8787 --call naa_discover
python3 tools/router_mcp.py --router-url http://127.0.0.1:8787 --call naa_dacs
python3 tools/router_mcp.py --router-url http://127.0.0.1:8787 --call naa_route_add \
  --arguments '{"name":"Chosen NAA","host":"192.0.2.30","device_id":"exact-dac-id"}'
python3 tools/router_mcp.py --router-url http://127.0.0.1:8787 --call naa_select \
  --arguments '{"route_id":"ID-returned-by-add"}'
python3 tools/router_mcp.py --router-url http://127.0.0.1:8787 --call naa_status
python3 tools/router_mcp.py --router-url http://127.0.0.1:8787 --call naa_stop
```

The documentation address is a placeholder, not a test target. A discovered
endpoint's address and port can be passed directly to add; cached DAC IDs can be
passed as `device_id`. CLI calls return the same structured result as MCP.

For an MCP client, configure a stdio command (replace the absolute script path):

```json
{
  "mcpServers": {
    "naa-router-poc": {
      "command": "python3",
      "args": ["/absolute/path/to/experiments/naa-router/tools/router_mcp.py",
               "--router-url", "http://127.0.0.1:8787"]
    }
  }
}
```

This does not install or modify an existing MCP client configuration. Without
`--call`, the process speaks newline-delimited JSON-RPC stdio and advertises all
ten tools. It exposes no HTTP MCP listener. It accepts an explicit loopback
router URL only; a remote deployment needs a separately configured local tunnel.

`naa_select` can return `pending`: poll `naa_status`; do not call Play again just
because the request was accepted. A write timeout returns `indeterminate` and is
never retried automatically. Check actual state before deciding whether to retry.
`naa_stop` has a separate worker so it can overtake a held selection call.
MCP request cancellation does not roll back a mutation; use explicit `naa_stop`.

The bridge follows the MCP [stdio transport](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports)
and [tool result](https://modelcontextprotocol.io/specification/2025-06-18/server/tools)
contracts. It supports tools, initialization and ping, not prompts/resources.
It is a PoC migration tool, not UHC's production MCP implementation.

## Verification

```sh
cargo test --manifest-path native/naa-router/Cargo.toml --no-fail-fast
python3 tools/test_router_mcp.py
python3 tools/test_router_mcp_integration.py --binary native/naa-router/target/debug/naa-router
```

The copied Rust suite invokes the copied Python lab, so it needs no original
checkout. Wire tests exercise the advertised MCP operations against the actual
copied router and software NAA fixtures, including learning a DAC through a
relayed auth session. Another wire test holds selection and proves Stop reaches
the HTTP backend within one second on the same MCP session. No browser is used.
Historical actual Embedded evidence is retained under `docs/naa-router`; it is
not a qualification of a future UHC-managed version.
