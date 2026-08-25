# GitHub Build Workflow

This document explains the build workflow architecture in `.github/workflows/build.yml`.

## Philosophy: Single Source of Truth

We use **one unified workflow** (`build.yml`) instead of separate PR and release workflows. This prevents:

- **Drift**: Separate workflows diverge over time (different cache keys, different build steps)
- **Duplication**: Same job definitions copied between files
- **Testing gaps**: PR builds don't match release builds

The unified workflow uses conditionals to control what runs based on trigger and labels/inputs.

## Configurable Builds

### For PRs: Use Labels

Add labels to your PR to enable optional builds:

| Label | Builds |
|-------|--------|
| `build:lms` | LMS plugin ZIPs (bootstrap + linux-x64 full) |
| `build:lms-macos` | LMS plugin + macOS full ZIP (for testing on Mac) |
| `build:synology` | Synology SPK (x64 + arm64) |
| `build:qnap` | QNAP x64 package |
| `build:qnap-arm` | QNAP arm64 package |
| `build:docker` | Docker x64 image |
| `build:linux-arm` | Linux arm64 + armv7 binaries |
| `build:macos` | macOS universal binary |
| `build:applemusic-macos` | Apple Music companion DMG (macOS arm64 only) |
| `build:windows` | Windows exe |
| `build:linux-packages` | deb/rpm packages |
| `build:all` | Everything |

**Default PR builds** (always run):
- Lint + Tests
- Fullstack build check (validates embedded assets)
- Linux x64 binary
- Smoke test (verifies binary boots, serves HTML with embedded CSS/JS/images)

### For Manual Runs: Use Inputs

`workflow_dispatch` provides checkboxes for each optional build target.

### For Releases: Everything

When triggered by a GitHub release, all builds run automatically.

## The Plan Job: Centralized Decision Logic

Instead of scattering build conditions across every job, we use a **plan job** that runs first (~5 seconds) and computes what needs to be built. All downstream jobs simply check the plan outputs.

```yaml
jobs:
  plan:
    outputs:
      build_linux_arm: ${{ steps.decide.outputs.build_linux_arm }}
      build_synology: ${{ steps.decide.outputs.build_synology }}
      # ... all flags
    steps:
      - id: decide
        run: |
          # Centralized logic - ARM needed if:
          # - release OR build:all label OR
          # - build:linux-arm label OR
          # - any downstream that needs it (synology, qnap-arm, linux-packages)
          BUILD_ARM="false"
          if [[ "$EVENT_NAME" == "release" ]]; then BUILD_ARM="true"; fi
          if [[ "$HAS_LABEL_SYNOLOGY" == "true" ]]; then BUILD_ARM="true"; fi
          # ... etc
          echo "build_linux_arm=$BUILD_ARM" >> $GITHUB_OUTPUT

  build-linux-arm:
    needs: plan
    if: needs.plan.outputs.build_linux_arm == 'true'
    # No scattered conditions - just checks the flag
```

**Benefits:**
- **Single source of truth**: "What triggers ARM build?" is defined in ONE place
- **Implicit dependency triggering**: `build:synology` label automatically enables ARM build
- **Easier debugging**: The plan job summary shows exactly what will build
- **Cleaner job definitions**: Jobs just check `needs.plan.outputs.X == 'true'`

The GitHub Actions UI renders the full dependency DAG, showing `plan` at the root with all builds fanning out from it.

## Parallelization Strategy

Jobs maximize parallelism while respecting dependencies:

```
plan ──► build-wasm ──┬─► build-linux-x64 ──► smoke-test
                      ├─► build-linux-arm (arm64, armv7)
                      ├─► build-macos-x64 ──┬─► build-macos-universal
                      ├─► build-macos-arm64 ┘
                      └─► build-windows
```

- **WASM built once**: Platform-independent, shared via artifact
- **Binary builds run in parallel**: All platform builds start after WASM completes
- **macOS universal**: x64 and arm64 build in parallel, then combined with `lipo`
- **Packaging waits for binaries only**: Docker, Synology, QNAP, LMS jobs (no separate web assets needed - see below)
- **Universal LMS ZIP**: Bundles all platform binaries in one package
- **Optional jobs skip cleanly**: ARM builds skip if not requested

The GitHub Actions UI shows the full dependency DAG.

## PR Artifact Comments

When a PR build completes, a bot comment is automatically posted with links to all artifacts:

```markdown
### 📦 Build Artifacts

| Artifact | Size |
|----------|------|
| linux-x64-binary | 12.3 MB |
| lms-plugin-linux-x64 | 15.1 MB |

[View workflow run](link) to download artifacts.
```

The comment is updated on each push, so you always see the latest artifacts.

## Label Triggers

Labels control builds in two ways:

**`build:*` labels** control WHAT gets built (see table above).

**`build-me` label** controls WHEN to re-trigger builds:
- Adding any `build:*` label does NOT trigger a new workflow run
- Only the `build-me` label triggers builds via the labeled event
- To re-trigger: remove `build-me`, then add it again

**Workflow:**
1. Add `build:lms` label to enable LMS builds
2. Add `build-me` label to trigger the build
3. Build runs, sees `build:lms` label, builds LMS
4. To re-run with same labels: remove `build-me`, add it back

This prevents spurious builds from non-build labels (arch, coderabbit, etc.).

## Caching Strategies

| Build Type | Strategy | Notes |
|------------|----------|-------|
| WASM assets | actions/cache + restore-keys | Content-based key, incremental on partial match |
| Fullstack check | rust-cache only | Validates `dx build --fullstack` works |
| Linux (zigbuild) | rust-cache only | sccache doesn't work with zig wrapper |
| macOS/Windows | sccache + rust-cache | sccache for `.o` files, rust-cache for proc-macros |
| Tools (dx, zigbuild) | actions/cache | Pin version in cache key |
| Docker images | Use GHCR | 10x faster than Docker Hub from Actions |

### WASM Caching

WASM is built once and shared across all platform builds. The cache uses content-based keys with fallback:

```yaml
- uses: actions/cache@v4
  with:
    path: |
      target/dx/
      target/wasm32-unknown-unknown/
    key: wasm-${{ hashFiles('**/Cargo.lock', '**/Cargo.toml', 'Dioxus.toml', 'src/**/*.rs', 'assets/**', 'input.css') }}
    restore-keys: |
      wasm-
```

| Scenario | Cache | Build time |
|----------|-------|------------|
| Exact match (no changes) | hit | ~10s |
| Partial match (small change) | restored | ~1-2 min (incremental) |
| No match (new deps) | miss | ~5 min (full) |

### Key Configurations

**rust-cache** caches `target/` including proc-macro `.dylib` files that sccache can't cache:

```yaml
- uses: Swatinem/rust-cache@v2
  with:
    shared-key: "fullstack-build"
    cache-all-crates: true
    cache-on-failure: true
```

**sccache** caches individual compilation units (`.o` files). Used with rust-cache for native builds:

```yaml
- uses: mozilla-actions/sccache-action@v0.0.11
- run: cargo build --release
  env:
    SCCACHE_GHA_ENABLED: "true"
    RUSTC_WRAPPER: "sccache"
```

**cargo-zigbuild** cross-compiles Linux binaries without Docker containers (unlike `cross`), so rust-cache works normally.

**Tool caching** pins versions in cache keys to ensure invalidation on upgrade:

```yaml
- uses: actions/cache@v4
  with:
    path: ~/.cargo/bin/dx
    key: dx-cli-0.7.10
```

### Embedded Assets (ADR 002)

Web assets (CSS, images) are **embedded directly in the binary** using Rust's `include_str!` and `include_bytes!` macros. This eliminates the need for separate asset distribution.

**How it works:**
- `src/app/embedded_assets.rs` - Contains compile-time asset embedding
- CSS is inlined in the HTML via `<style>` tags
- Images are served as base64 data URLs
- Total embedded: ~65KB (negligible for a 10MB binary)

**Why this works with `cargo zigbuild`:**
- `include_str!`/`include_bytes!` are standard Rust macros
- They work with any Cargo build, not just `dx build`
- The `fullstack-check` job validates the Dioxus SSR configuration is correct
- Release binaries are built with `cargo zigbuild`, which compiles the embedded assets

**Benefits:**
- Single binary distribution (no `public/` folder needed)
- No more `DIOXUS_PUBLIC_PATH` environment variable hacks
- Simplified packaging for all targets (Docker, Synology, QNAP, AUR, deb/rpm)

### NAS Packages

**Synology SPK:** Built by `build/synology/build-spk.sh` using the documented tar
layout (without the 1GB pkgscripts-ng toolkit). An always-on CI contract test
validates DSM-compatible versions, platform families, extracted size, metadata,
and an unprivileged start/status/stop lifecycle. See the
[Synology Developer Guide](https://help.synology.com/developer-guide/synology_package/introduction.html)
for SPK structure.

**QNAP QPKG:** Uses a repository-owned Docker image containing QNAP's official
QDK 2.5.3 release. The image is based on digest-pinned Ubuntu 20.04 and
checksum-pins the official `qdk_2.5.3_amd64.deb` artifact. Both jobs run the
AMD64 builder on the AMD64 GitHub runner; QDK's `qbuild --build-arch` selects
the normal `.qpkg` output architecture (`x86_64` or `arm_64`), so this is not a
package-format migration. The official release also ships an ARM64 QDK
package, but it is not needed for the cross-target build and is not used
speculatively.

To upgrade the builder, choose a published release from
[qnap-dev/QDK](https://github.com/qnap-dev/QDK/releases), verify the release
asset SHA-256 in the Dockerfile, update `QDK_VERSION`, the checksum, and the
base-image digest together, then run the QNAP contract tests and both CI jobs.
The QDK 2.5.3 release assets used here are:

- `qdk_2.5.3_amd64.deb`: `17b3841b7d4590a4ee025844ba583304b5e3c497d9fa8934d5175131d3908022`
- `qdk_2.5.3_arm64.deb`: `4b00c009cb48c0ffa7e4b7b00c5a6a1982a0955d663c0c6ec57020353e68eeb9` (available from the release, not used by CI)

### Apple Music Companion DMG

`build-applemusic-companion-dmg` builds the macOS companion app
(`companion/apple_music/XcodeMac/AppleMusicCompanionMac.xcworkspace`) with
`xcodebuild`, forcing `ARCHS=arm64` — Apple Silicon only, no x86_64 or
universal build, by explicit project decision. The job runs
`companion/apple_music/build-dmg.sh`, which also verifies the built
executable is arm64-only (via `lipo -archs`) before wrapping it with
`hdiutil create` into a `.dmg` containing the `.app` and an `Applications`
symlink.

No code-signing identity is available in CI, so the DMG ships **unsigned**
(ad-hoc signed with `codesign --sign -`). Notarization is tracked as a
follow-up, not a blocker (see issue #535). Because the app is unsigned,
Gatekeeper quarantines it on download; users must either right-click the
`.app` and choose **Open** (confirming the dialog) on first launch, or run:

```sh
xattr -dr com.apple.quarantine "/Applications/AppleMusicCompanionMac.app"
```

This is documented in `companion/apple_music/README.md` and linked from the
release notes' Download Guide.

To reproduce the CI artifact locally on Apple Silicon:

```sh
make companion-apple-music-dmg
# or directly:
VERSION=0.1.0 companion/apple_music/build-dmg.sh
```

The DMG lands in `companion/apple_music/dist/`. Set `CODE_SIGN_IDENTITY` (and
`DEVELOPMENT_TEAM`) to build with a local Developer ID or Apple Development
identity instead of the unsigned default.

## Build Matrix

| Target | Caching | Build Tool | Default | Label |
|--------|---------|------------|---------|-------|
| Fullstack Check | rust-cache | dx build --fullstack | Always | - |
| Linux x86_64-musl | rust-cache | cargo-zigbuild | Always | - |
| Smoke Test | N/A | curl | Always | - |
| Linux aarch64-musl | rust-cache | cargo-zigbuild | Release | `build:linux-arm` |
| Linux armv7-musl | rust-cache | cargo-zigbuild | Release | `build:linux-arm` |
| macOS universal | sccache + rust-cache | cargo + lipo | Release | `build:macos` |
| Apple Music companion DMG (macOS arm64) | N/A | xcodebuild + hdiutil | Release | `build:applemusic-macos` |
| Windows x86_64 | sccache + rust-cache | cargo | Release | `build:windows` |
| Docker x64 | N/A | pre-built binary | Release | `build:docker` |
| Docker multi-arch | N/A | pre-built binaries | Release | - |
| Synology SPK | N/A | tar | Release | `build:synology` |
| QNAP x64 | N/A | qbuild (Docker) | Release | `build:qnap` |
| QNAP arm64 | N/A | qbuild (Docker) | Release | `build:qnap-arm` |
| Linux deb (x64/arm64/armv7) | N/A | fpm | Release | `build:linux-packages` |
| Linux rpm (x64 only) | N/A | fpm | Release | `build:linux-packages` |
| LMS Universal ZIP | N/A | zip + binaries | Release | `build:lms` |

Note: All binaries include embedded web assets (CSS, images) - no separate asset distribution needed.

## Smoke Testing Cross-Compiled Binaries

armv7 binaries are smoke-tested on x86_64 runners using QEMU:

```yaml
- name: Smoke test armv7 binary
  if: matrix.target == 'armv7-unknown-linux-musleabihf'
  run: |
    sudo apt-get update && sudo apt-get install -y qemu-user-static
    qemu-arm-static ./target/${{ matrix.target }}/release/unified-hifi-control --version
```

This adds ~14s but catches ABI issues, missing linkage, and startup crashes before release.

## LMS Plugin

See [lms-plugin.md](lms-plugin.md) for LMS plugin distribution modes (bootstrap vs full ZIPs) and testing instructions.

## Release Signing

Tracked as issue #561, worked cheapest-first. Each platform activates
independently once its GitHub Actions secrets exist - nothing here is
all-or-nothing.

### Status

| Platform | State | Secrets required |
|----------|-------|-------------------|
| SHA256SUMS (all binaries/packages) | **Active** | none - always generated |
| SHA256SUMS.asc (GPG detached signature) | Gated | `GPG_PRIVATE_KEY`, `GPG_PASSPHRASE` |
| Docker images (cosign/sigstore keyless) | **Active** | none - uses GitHub OIDC |
| QNAP QPKG (x64 + arm64) | Prepared, not implemented | `QNAP_SIGNING_CERT`, `QNAP_SIGNING_KEY` (fails loudly if set, since the actual QDK signing call isn't wired up yet) |
| macOS companion DMG (codesign) | Gated | `APPLE_DEVELOPER_ID_CERT_P12`, `APPLE_DEVELOPER_ID_CERT_PASSWORD`, `APPLE_DEVELOPMENT_TEAM_ID` |
| macOS companion DMG (notarize + staple) | Gated (requires codesign above) | `APPLE_NOTARY_KEY_P8`, `APPLE_NOTARY_KEY_ID`, `APPLE_NOTARY_KEY_ISSUER_ID` |
| Windows binary (Authenticode/signtool) | Prepared, disabled | `WINDOWS_SIGNING_CERT_BASE64`, `WINDOWS_SIGNING_CERT_PASSWORD` (deferred until Windows matters commercially) |
| Synology SPK | Not signable | n/a - Synology does not support self-signed packages; DSM 7 shows an "unrecognized publisher" warning by design (see README) |

Every gated step is written to skip cleanly (or, for QNAP, fail with a clear
message) when its secrets are absent, so adding platforms later never
requires touching the workflow's control flow - only adding secrets.

### Free tier (active now)

**SHA256SUMS + GPG signature.** The `upload-release` job in
`.github/workflows/build.yml` runs `sha256sum *` over every file in the
release (binaries, packages, DMG, zips) and writes `SHA256SUMS`. If the
`GPG_PRIVATE_KEY` secret is set, it imports that key and produces a detached,
armored signature at `SHA256SUMS.asc`; if not, the release still ships,
just without the `.asc` file. See
[docs/release-signing/gpg-public-key.asc](release-signing/gpg-public-key.asc)
for exactly how to generate and install that key - it's currently a
placeholder because generating a long-lived signing key isn't something CI
(or an automated change) should do on the owner's behalf.

**cosign (sigstore keyless) for Docker images.** The `docker-manifest` job
signs every tag it publishes (`muness/unified-hifi-control` and the legacy
`muness/roon-extension-knob` alias) with `cosign sign --yes <tag>`. This uses
GitHub Actions' OIDC identity token to get a short-lived certificate from
Sigstore's public-good Fulcio CA and records the signature in the public
Rekor transparency log - no secret, no enrollment, no long-lived key. It's
already active on every tagged release.

### Verifying a release

Given a downloaded release asset (say `unified-hifi-linux-x64`) and the
`SHA256SUMS`/`SHA256SUMS.asc` files from the same release:

```sh
# 1. Checksum: does the file match what CI produced?
sha256sum -c SHA256SUMS --ignore-missing

# 2. Signature: was SHA256SUMS itself signed by the project's key?
#    (one-time) import the project's public key:
gpg --import gpg-public-key.asc   # from docs/release-signing/gpg-public-key.asc
gpg --verify SHA256SUMS.asc SHA256SUMS
```

For Docker images:

```sh
cosign verify muness/unified-hifi-control:<version> \
  --certificate-identity-regexp 'https://github.com/open-horizon-labs/unified-hifi-control/.*' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

If `SHA256SUMS.asc` is missing from a release, GPG signing wasn't active yet
for that build - the checksum file itself is still valid, just unsigned.

### Gated platforms (prepared, waiting on owner-supplied secrets)

**QNAP QPKG.** `build-qnap-x64` and `build-qnap-arm` each have a signing step
gated on `QNAP_SIGNING_CERT`/`QNAP_SIGNING_KEY`. Unlike the other gated
steps, this one is intentionally not a silent no-op if the secrets exist:
QNAP's actual QDK signing call (`codesign_pkg` or equivalent) isn't
implemented yet, so the step fails loudly rather than claim a package is
signed when it isn't. Wire up the real signing call before setting these
secrets in CI.

**macOS companion DMG.** `build-applemusic-companion-dmg` imports a
"Developer ID Application" certificate from `APPLE_DEVELOPER_ID_CERT_P12` /
`APPLE_DEVELOPER_ID_CERT_PASSWORD` into a temporary keychain, builds with
that identity (`APPLE_DEVELOPMENT_TEAM_ID`), and - if the three
`APPLE_NOTARY_KEY_*` secrets (an App Store Connect API key) are also set -
submits the DMG to `notarytool` and staples the ticket. Any subset missing
falls back to today's unsigned/ad-hoc build; once all six secrets exist, the
`companion/apple_music/README.md` "unsigned app" workaround and the matching
note in `.github/RELEASE_TEMPLATE.md`/README should be removed.

**Windows Authenticode.** `build-windows` has a `signtool` step gated on
`WINDOWS_SIGNING_CERT_BASE64`/`WINDOWS_SIGNING_CERT_PASSWORD` (a base64 PFX +
password). Deferred by design until Windows matters commercially per issue
#561. When ready, either populate those two secrets, or replace the step
with `azure/trusted-signing-action` and its `AZURE_TRUSTED_SIGNING_*` secrets
if using Azure Trusted Signing instead of a traditional OV certificate -
both are supported paths, pick whichever the owner buys.

### Secret names, all in one place

| Secret | Used by | Purpose |
|--------|---------|---------|
| `GPG_PRIVATE_KEY` | `upload-release` | ASCII-armored private key, imported at sign time |
| `GPG_PASSPHRASE` | `upload-release` | Passphrase for the key above (may be empty-string key, see placeholder doc) |
| `QNAP_SIGNING_CERT` | `build-qnap-x64`, `build-qnap-arm` | QNAP developer program signing certificate |
| `QNAP_SIGNING_KEY` | `build-qnap-x64`, `build-qnap-arm` | QNAP developer program signing key |
| `APPLE_DEVELOPER_ID_CERT_P12` | `build-applemusic-companion-dmg` | base64 "Developer ID Application" .p12 export |
| `APPLE_DEVELOPER_ID_CERT_PASSWORD` | `build-applemusic-companion-dmg` | Password for the .p12 above |
| `APPLE_DEVELOPMENT_TEAM_ID` | `build-applemusic-companion-dmg` | 10-character Apple Developer Team ID |
| `APPLE_NOTARY_KEY_P8` | `build-applemusic-companion-dmg` | base64 App Store Connect API key (.p8) |
| `APPLE_NOTARY_KEY_ID` | `build-applemusic-companion-dmg` | Key ID for the API key above |
| `APPLE_NOTARY_KEY_ISSUER_ID` | `build-applemusic-companion-dmg` | Issuer ID for the API key above |
| `WINDOWS_SIGNING_CERT_BASE64` | `build-windows` | base64 PFX code-signing certificate (deferred) |
| `WINDOWS_SIGNING_CERT_PASSWORD` | `build-windows` | Password for the PFX above (deferred) |

None of these are committed anywhere; the workflow only ever imports them
from GitHub Actions secrets at build time.
