## Changes in beta.9

- Sweep direct Rust and npm dependencies against current stable releases, including major API migrations: Rand 0.10, SHA2 0.11, Ed25519 3, Reqwest 0.13, Syn 3 and MCP SDK 2.0. Refresh the transitive graph and npm lockfile.
- Build with Rust 1.99.1, Zig 0.17, cargo-zigbuild 0.24, Tailwind 4.3.3 and Node 26.12.1. Use Alpine 3.24.3 for published runtime containers.
- Preserve OAuth form submission, bounded streamed responses, cryptographic random sources and existing encrypted credential formats while adapting upstream APIs.
- Preserve WASM-bindgen metadata until the browser bundle is processed on LLVM 23. Native release binaries retain their stripping, LTO and hardening settings.
- **Breaking MCP protocol update:** use MCP 2026-07-28 with `server/discover`, per-request protocol/capability metadata and method/name headers. Legacy initialize/session GET/DELETE and automatic stale-session recovery are removed. Resource-list notifications use explicit POST `subscriptions/listen` streams. The browser WebMCP bridge and contract clients are migrated together.
- Update CI actions to their latest stable major lines. Model/evaluation Git pins and the MQTT security fix retain their reviewed provenance.

Pair with **HiPhi firmware v2.8.0-alpha.4**. Exact packaged artifacts, hardware and live providers still require qualification before promotion to stable.

## Changes in beta.8

- Play and queue complete LMS saved playlists from collection rows. Stale playlist handles are checked before the queue is changed.
- Make supported collections reachable to browse-only controllers: retain Roon Playlists at the browse root, add Music Assistant Playlists/Favorites/Radio shortcuts, and expose LMS Favorites and Radio.
- Add LMS Albums A-Z and Artists A-Z menus using the server's own alphabetical index. Letter paging stops at the chosen letter's boundary and respects provider ordering.
- Preserve knob sleep and other settings atomically across bridge restarts, and report persistence failures instead of silently accepting lost settings.
- Accept LMS playlist indices encoded as strings. Refresh OpenHome radio metadata even when the stream URI stays the same.
- Reduce redundant MCP resource-list notifications and return the correct response for expired SSE sessions.
- Update dependencies and their API integrations, including Tokio 1.54; keep server credential encryption out of the browser build. Remove the obsolete Spotify OAuth relay spike.

Pair with **HiPhi firmware v2.8.0-alpha.3** for text library lists, Open/Play/Queue actions, visible volume feedback from artwork mode, and defensive playback/transport handling.

The shared collection contract is exercised against Roon, LMS and Music Assistant adapters. It follows paginated opaque browse locations and checks whole-collection playback targets. Unsupported shared-browser providers remain explicit. Roon Favorites and non-LMS letter menus are not added; nested LMS Favorites folders remain outside this release.

This is a beta candidate. Exact packaged artifacts and live playback still need qualification before promotion to stable.

## Installation

### Docker

```yaml
services:
  unified-hifi-control:
    image: muness/unified-hifi-control:{{VERSION}}
    network_mode: host
    volumes:
      - ./data:/data
    environment:
      - CONFIG_DIR=/data
    restart: unless-stopped
```

```bash
docker compose up -d
# Access http://localhost:8088
```

### QNAP NAS

Download the QPKG package from the assets below:
- `unified-hifi-control_*_x86_64.qpkg` — Intel/AMD x86_64
- `unified-hifi-control_*_arm_64.qpkg` — ARM64

### Roon Extension Manager

Search for "Unified Hi-Fi Control" in Roon Extension Manager and install.

### LMS Plugin

Add this repository URL in LMS Settings → Plugins → Additional Repositories:
```
https://raw.githubusercontent.com/open-horizon-labs/unified-hifi-control/v4/lms-plugin/repo.xml
```
Then install "Unified Hi-Fi Control" from the plugin list.

### Apple Music Companion (macOS, Apple Silicon)

Download `unified-hifi-applemusic-companion-macos-arm64-*.dmg` from the assets
below, open it, and drag **AppleMusicCompanionMac.app** (shown in Finder as
"Apple Music Companion") into **Applications**. Apple Silicon (arm64) Macs
only.

**This build is unsigned.** macOS Gatekeeper will block the first launch.
Either:
- Right-click (or Control-click) the app in Applications and choose **Open**,
  then confirm the dialog, or
- Remove the quarantine attribute from Terminal:
  ```bash
  xattr -dr com.apple.quarantine "/Applications/AppleMusicCompanionMac.app"
  ```

Notarization is tracked as a follow-up and is not required to use the app.
See `companion/apple_music/README.md` for details on what this companion
does and does not control.

---

## Verifying These Artifacts

Every file attached to this release is listed in `SHA256SUMS`. If
`SHA256SUMS.asc` is also attached, it's a detached GPG signature over
`SHA256SUMS`, made with the project's release-signing key
(`docs/release-signing/gpg-public-key.asc` in the repo).

```bash
# Checksum any downloaded file against SHA256SUMS
sha256sum -c SHA256SUMS --ignore-missing

# Verify SHA256SUMS itself was signed by the project (one-time key import)
gpg --import gpg-public-key.asc
gpg --verify SHA256SUMS.asc SHA256SUMS
```

Docker images (`muness/unified-hifi-control:{{VERSION}}`) are signed
keylessly with [cosign](https://docs.sigstore.dev/cosign/overview/) via
GitHub Actions OIDC - no key to import:

```bash
cosign verify muness/unified-hifi-control:{{VERSION}} \
  --certificate-identity-regexp 'https://github.com/open-horizon-labs/unified-hifi-control/.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

Full details, including which platforms are signed today versus pending
owner credentials: [docs/gh-release.md#release-signing](https://github.com/open-horizon-labs/unified-hifi-control/blob/v4/docs/gh-release.md#release-signing).

---

## MCP Server (Claude Integration)

The bridge includes a built-in MCP server. Add to your MCP config (Claude Code, Claude Desktop, etc.):

```json
{
  "mcpServers": {
    "unified-hifi-control": {
      "type": "http",
      "url": "http://<your-bridge-host>:8088/mcp"
    }
  }
}
```

Replace `<your-bridge-host>` with your bridge IP or hostname (e.g., `localhost`, `192.168.1.100`, `nas.local`).

---

## Configuration

Configure all backends (Roon, LMS, HQPlayer, UPnP/OpenHome) via the web UI at `http://<your-bridge-host>:8088`.
