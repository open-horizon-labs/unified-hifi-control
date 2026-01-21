# LMS Plugin Distribution

The LMS plugin supports two distribution modes.

## Bootstrap ZIP (Default for Releases)

**File:** `lms-unified-hifi-control-VERSION.zip`

- Contains only Perl code and plugin metadata (~50KB)
- On first run, downloads binary + web assets from GitHub releases
- Works on any platform (downloads correct binary for detected architecture)
- Requires network access on first run

## Full ZIPs (For PR Testing and Offline)

**Files:** `lms-unified-hifi-control-VERSION-PLATFORM.zip`

| Platform | Description |
|----------|-------------|
| `linux-x64` | Intel/AMD Linux servers |
| `linux-arm64` | ARM64 Linux (Raspberry Pi 4, etc.) |
| `linux-armv7` | ARMv7 Linux (Raspberry Pi 2/3, etc.) |
| `macos` | macOS Universal (Intel + Apple Silicon) |
| `windows` | Windows 64-bit |

Contains bundled binary in `Bin/` and web assets in `public/` (~15-25MB). Works immediately without network access.

## Binary Lookup Priority

When the LMS plugin starts, `Helper.pm` looks for binaries in this order:

1. **Bundled binary** (`$pluginDir/Bin/unified-hifi-control`)
2. **Cached binary** (`$cacheDir/UnifiedHiFi/Bin/$binaryName`)
3. **Download** (from GitHub releases matching plugin version)

## Testing LMS Changes in PRs

1. Add `build:lms` label to your PR
2. Wait for CI to complete
3. Download `lms-plugin-linux-x64` artifact (or appropriate platform)
4. Install via LMS Settings > Plugins > Install Plugin from File

For macOS: use `build:lms-macos` label instead (triggers macOS binary build).
