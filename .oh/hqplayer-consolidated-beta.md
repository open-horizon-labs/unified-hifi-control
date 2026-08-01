# HQPlayer direct-control beta: consolidation + QNAP test package

**PR:** #426 · **Base branch:** `v3` · **Head branch:** `feat/hqplayer-direct-control-beta-v3` ·
**Head commit:** `c7c34e41565ab0c313b7cba8872e6a8123bb443d`

Replaces ten stacked, individually-open feature-branch PRs with one squash-merged,
fully-verified unit on current `v3`, plus MCP dispatch wiring ported from #406 into
`v3`'s modular MCP tool structure. No subagents, superego, `ba`, or `wm` were used
for this session. No PR was merged, no GitHub release was published, and no live
HQPlayer host was contacted.

## Included stack (superseded, not closed)

| PR | Issue | Title |
|---|---|---|
| #337 | #322 | executable native-protocol conformance harness and the client defects it caught |
| #364 | #341 | evidence-ranked protocol ledger with a lint that fails when it drifts |
| #366 | #347 | verify live setters and scope enumerations to the loaded chain |
| #370 | #162 | manage the HQPlayer producer lifecycle |
| #371 | #368 | keep HQPlayer semantic operations on one endpoint |
| #373 | #369 | make HQPlayer workers restart-aware and self-healing |
| #376 | #375 | publish HQPlayer native adaptive-control documents |
| #382 | #329 | add revision-fenced HQPlayer immediate commands |
| #391 | #328 | make direct HQPlayer a truthful everyday UHC zone |
| #406 | #401 | validate and fix the MCP surface for the direct HQPlayer zone |

Also relevant to #350 (HQPlayer Beta A: publish an installable direct-control test
build) via the QNAP package below — **not published** toward that issue, evidence only.

None of the ten PRs above were closed or merged by this work; they remain open,
now marked superseded in the PR body.

## What this session did, beyond the already-staged squash

The squash-merge (`git merge --squash feat/issue-401-hqplayer-mcp-beta-validation`)
was already resolved for `AGENTS.md` and `src/mcp/mod.rs` (both kept byte-identical
to `v3`, preserving the modular MCP architecture) when this session started, but the
MCP behavior #406 actually delivers — routing `hqplayer:` zones to a real dispatch
instead of refusing them — had not been ported into that modular structure at all.
This session:

1. Added `handle_hqplayer_control` to `src/mcp/tools/transport.rs`: `hifi_control`
   now dispatches `hqplayer:` zones through
   `crate::knobs::routes::dispatch_hqplayer_action` — the same core the `/knob`
   HTTP surface (#391) uses — supporting play/pause/playpause/stop/next/previous/
   seek/mute/volume_set/volume_up/volume_down. This bypasses the generic
   `TransportRoute`/`VolumeRoute` grid entirely (HQPlayer's action vocabulary,
   decimal-dB volume and zone-state-aware refusals don't fit it); that grid still
   reports `Refused` for `hqplayer:` for every other reason, unchanged.
2. Updated `src/mcp/capabilities.rs`: `hqplayer`/`transport`, `transport_skip` and
   `volume` flip from a gap tracked by #328 to `Supported`. Content operations
   (search, browse, queue, ...) remain an honest gap tracked by #209 — #406 didn't
   touch those. Regenerated `AGENTS.md`'s capability matrix
   (`UPDATE_AGENTS_MATRIX=1 cargo test --test mcp_contract agents_md`) to match —
   the only change to that file beyond the untouched squash content.
3. Confirmed `hifi_search`/`hifi_play` needed **no code change**: `routing.rs`
   already refused `hqplayer:` for library operations before this session (#398),
   which already satisfies #406's "don't hand an hqplayer zone id to Roon" intent
   for search/play through the existing architecture.
4. Fixed a pre-existing compile defect in the already-staged squash:
   `src/mcp/tools/hqplayer.rs::handle_set_pipeline` didn't collapse the
   `Result<SettingOutcome>` the trustworthy-setters work (#347) gave
   `set_mode`/`set_filter_1x`/`set_filter_nx`/`set_shaper`/`set_rate`, unlike
   `src/api/mod.rs`'s equivalent HTTP handler. Fixed with the same
   `.into_applied_result()` call.
5. Updated `tests/mcp_contract.rs` and `src/mcp/capabilities.rs`'s own unit tests
   everywhere the new dispatch genuinely changed behavior: the hqplayer transport/
   volume routing pins (now "unknown instance", not "not wired"), the
   `every_supported_capability_reaches_that_providers_own_adapter` proof count
   (15 → 18 supported cells), and one `not_implemented` example moved from
   `hifi_control` (now dispatched) to `hifi_search` (still gapped, #209).

`tests/mcp_hqplayer_control.rs` (823 lines, #401/#406's own client-facing spec,
driving the real MCP HTTP surface against a wire-level HQPlayer daemon fake) needed
**no changes at all** — it depends only on the crate's public `mcp::create_mcp_extension`
API, not on file layout, and its 45 cases pass unmodified once the dispatch above
was wired in. That is the strongest evidence the port is behaviorally complete.

## API impact

No public API/MCP tool names, schemas, routes, or `api_routes` changed. `hifi_control`'s
*behavior* for `hqplayer:` zones changes from "refused, not_implemented" to
"dispatched" — the substance of #406 — with no change to any tool's declared
parameters. Tool description strings were deliberately left untouched (part of the
pinned `tests/fixtures/mcp_tools.json` snapshot; cosmetic, out of scope here).

## Verification

| Check | Result |
|---|---|
| `cargo test --all-features` | Every suite green — lib (264), `mcp_contract` (94), `mcp_hqplayer_control` (45), `hqplayer_conformance` (295), `hqplayer_direct_zone`, `hqplayer_lifecycle`, `hqplayer_ledger_lint`, `hqplayer_operation_lease`, `hqplayer_pipeline_projection_lint`, `adaptive_*`, `architecture_lint`, `client_harness`, `api_contract`, and every other integration binary — zero failures. |
| `cargo fmt --check` | Clean. |
| `cargo clippy -- -D warnings` (CI's own invocation, `.github/workflows/build.yml:369`) | Clean. `cargo clippy --all-targets --all-features` (stricter than CI) surfaces 406 pre-existing `unwrap`/`expect`/`panic` lint hits confined entirely to `#[cfg(test)]` unit-test blocks in `src/producers/hqplayer.rs` and `hqplayer_command.rs` — not part of CI's actual gate and not introduced by this session. |
| `dx build --release --platform web --features web` | Client (WASM) and server both build successfully. |
| QNAP x86_64 QPKG | Built locally; see below. |

## QNAP x86_64 test package

Built from head commit `c7c34e4` via the repository's own supported cross workflow
(`.github/workflows/build.yml`'s `build-linux-x64` + `build-qnap-x64` jobs, run
locally step-for-step):

1. `dx build --release --platform web --features web` → WASM assets at
   `target/dx/unified-hifi-control/release/web/public/`.
2. `cargo zigbuild --release --target x86_64-unknown-linux-musl` (zig 0.13.0,
   macOS-aarch64 build matching CI's pinned Linux zig version;
   `cargo-zigbuild 0.23.0`), with `UHC_VERSION=0.0.0-dev` and
   `UHC_GIT_SHA=c7c34e4` — `0.0.0-dev` is CI's own fallback version for a build
   that is neither a release nor a numbered PR event.
3. `dist/bin/unified-hifi-linux-x64` — ELF hardening verified: PIE + full RELRO
   (`scripts/check-elf-hardening.sh`).
4. `build/qnap/` staged into `qnap-build/` exactly as `build-qnap-x64` does
   (shared binary + `*.sh` + icons, `qpkg.cfg` with `{{VERSION}}` → `0.0.0-dev`,
   empty `package_routines`).
5. `docker run --rm --platform linux/amd64 -v "$(pwd)/qnap-build:/src" -w /src
   owncloudci/qnap-qpkg-builder@sha256:e342184e415b1df87ef00b8c1df47988ffd6a9d232a21331556067d400f06189
   sh -c '/usr/share/qdk2/QDK/bin/qbuild --build-dir /src/build && cp
   /src/build/*.qpkg /src/ && chmod 666 /src/*.qpkg'` — the exact pinned image
   and command CI uses.

**Artifact:** `dist/installers/unified-hifi-control_0.0.0-dev_x86_64.qpkg`
(not committed — `dist/` is gitignored, matching repository convention)
**Size:** 8,440,033 bytes
**sha256:** `22409b64bbcd6b2f2e4277274f95ba0144b39b941a54638bb1d79a65bd7d852a`
**Architecture:** x86_64 (QNAP QDK2, static-musl binary, shared-data-dir layout)

**Structure/config verified:** the output is a well-formed QNAP self-extracting
installer script. `strings` inspection confirms the embedded package identity
block (`unified-hifi-control0.0.0-dev QNAPQPKG`) and that `qpkg.cfg`'s
`QPKG_DISPLAY_NAME`/summary values were correctly consumed into the installer's
App Center install/error notification strings ("Unified Hi-Fi Control").

**Non-publication status:** this artifact exists only on this machine's local
`dist/installers/` directory. It was not uploaded to any GitHub release, App
Center, or other distribution channel, and no live HQPlayer host was contacted
at any point in this build or in the test suite above (all HQPlayer coverage
runs against `tests/mock_servers/hqplayer`'s wire-level fake).
