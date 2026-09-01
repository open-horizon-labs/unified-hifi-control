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
