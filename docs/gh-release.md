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
| `build:windows` | Windows exe |
| `build:linux-packages` | deb/rpm packages |
| `build:all` | Everything |

**Default PR builds** (always run):
- Lint + Tests
- Web assets (WASM)
- Linux x64 binary

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

Jobs are structured to maximize parallelism while respecting dependencies:

```
                         ┌─────────────────┐
                         │  build-web-     │
                         │  assets         │
                         └────────┬────────┘
                                  │
        ┌─────────────────────────┼─────────────────────────┐
        │                         │                         │
        ▼                         ▼                         ▼
┌───────────────┐    ┌────────────────────────┐    ┌───────────────┐
│ build-linux   │    │      build-macos       │    │ build-windows │
│ (x64 + ARM)   │    │  ┌─────┐    ┌─────┐   │    │               │
└───────┬───────┘    │  │ x64 │    │arm64│   │    └───────┬───────┘
        │            │  └──┬──┘    └──┬──┘   │            │
        │            │     └────┬─────┘      │            │
        │            │          ▼            │            │
        │            │    ┌──────────┐       │            │
        │            │    │ universal│       │            │
        │            │    │  (lipo)  │       │            │
        │            │    └──────────┘       │            │
        │            └────────────────────────┘            │
        │                         │                        │
        └─────────────────────────┼────────────────────────┘
                                  │
                                  ▼
                     ┌───────────────────────┐
                     │  build-docker         │
                     │  build-linux-packages │
                     │  build-synology       │
                     │  build-qnap           │
                     │  build-lms-full       │
                     └───────────────────────┘
```

- **Independent jobs run in parallel**: All binary builds start simultaneously
- **macOS x64 and arm64 build in parallel**: Combined with `lipo` in a separate quick job
- **Dependent jobs wait**: Packaging jobs wait for binaries + web assets
- **Dynamic matrix for LMS**: Only builds platform variants whose binaries are enabled
- **Optional jobs skip cleanly**: ARM builds skip if not requested, dependent jobs handle missing artifacts

## PR Artifact Comments

When a PR build completes, a bot comment is automatically posted with links to all artifacts:

```markdown
### 📦 Build Artifacts

| Artifact | Size |
|----------|------|
| linux-x64-binary | 12.3 MB |
| lms-plugin-linux-x64 | 15.1 MB |
| web-assets | 2.4 MB |

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

### 1. rust-cache with Shared Keys

```yaml
- name: Cache Rust
  uses: Swatinem/rust-cache@v2
  with:
    shared-key: "wasm-build"        # Same key across all triggers
    cache-all-crates: true          # Cache all dependencies, not just workspace
    cache-on-failure: true          # Save cache even if build fails
    cache-directories: target/dx    # Include dioxus build artifacts
```

**Options explained:**
- `shared-key`: Overrides job-based cache key to share across triggers
- `cache-all-crates`: Caches all crates, not just workspace members (important for proc-macros)
- `cache-on-failure`: Saves partial cache if build fails (speeds up retry)
- `cache-directories`: Additional directories to cache (e.g., `target/dx` for dioxus)

### 2. sccache for Compilation Units

**Used by:** Native builds (Web Assets, macOS, Windows)

```yaml
- name: Setup sccache
  uses: mozilla-actions/sccache-action@v0.0.9

- name: Build
  env:
    SCCACHE_GHA_ENABLED: "true"
    RUSTC_WRAPPER: "sccache"
  run: cargo build --release
```

**Why both sccache AND rust-cache?** They cache different things:
- **sccache**: Caches individual compilation units (`.o` files) keyed by source hash
- **rust-cache**: Caches `target/` directory including proc-macro `.dylib` files

Proc-macros (serde_derive, dioxus, thiserror) can't be cached by sccache due to "crate-type" limitations. rust-cache preserves compiled proc-macro binaries.

**Note:** sccache doesn't support zig's compiler wrapper, so zigbuild jobs use rust-cache only.

### 3. cargo-zigbuild for Linux Cross-Compilation

**Used by:** Linux musl builds (x86_64, aarch64, armv7)

**Why zigbuild instead of cross:** The [cross](https://github.com/cross-rs/cross) tool runs cargo inside Docker containers, which breaks caching - container paths (`/project/`) don't match host paths, invalidating Cargo's fingerprints.

[cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild) uses Zig as a cross-linker without containers:
- rust-cache works normally (no container path issues)
- No Docker image pulls (~15s saved per build)
- Produces static musl binaries

```yaml
- name: Install zig
  run: |
    curl -L https://ziglang.org/download/0.13.0/zig-linux-x86_64-0.13.0.tar.xz | tar -xJ
    sudo mv zig-linux-x86_64-0.13.0 /opt/zig
    echo "/opt/zig" >> $GITHUB_PATH

- name: Build
  run: cargo zigbuild --release --target ${{ matrix.target }}
```

**Per-target cache keys:**
```yaml
- name: Cache Rust
  uses: Swatinem/rust-cache@v2
  with:
    shared-key: zigbuild-${{ matrix.target }}  # Separate cache per target
```

### 4. macOS Universal Binary (Parallel builds + lipo)

**Why not zigbuild?** zigbuild can't find macOS system frameworks. Use native cargo for each arch, then combine with `lipo`.

**Parallel job structure:**
- `build-macos-x64`: Builds x86_64 binary (~1.5 min)
- `build-macos-arm64`: Builds aarch64 binary (~1.5 min) - runs in parallel
- `build-macos-universal`: Downloads both, combines with `lipo` (seconds)

This cuts macOS build time from ~3 min (serial) to ~1.5 min (parallel).

```yaml
# In build-macos-universal job:
- name: Create universal binary
  run: |
    lipo -create \
      x64/unified-hifi-control \
      arm64/unified-hifi-control \
      -output unified-hifi-macos-universal
```

### 5. Tool Binary Caching

```yaml
- name: Cache Dioxus CLI
  id: cache-dx
  uses: actions/cache@v4
  with:
    path: ~/.cargo/bin/dx
    key: dx-cli-0.7.3  # Version in key ensures cache invalidation on upgrade

- name: Install Dioxus CLI
  if: steps.cache-dx.outputs.cache-hit != 'true'
  run: cargo install dioxus-cli@0.7.3 --locked
```

**Why:** Tools take 2-3 minutes to compile. Caching binaries saves this on every run.

### 6. GHCR Base Images

```dockerfile
FROM ghcr.io/linuxcontainers/alpine:3.20
```

**Why GHCR?**
- GitHub Actions runners are co-located with GHCR (~10x faster pulls)
- Docker Hub has rate limits (200 pulls/6 hours) that can block CI

### 7. Web Assets Artifact Sharing

Web assets (WASM + JS + CSS) are identical across all platforms. Build once, share via artifacts:

**Build steps:**
1. `make css` - Compiles Tailwind CSS (required before dx build)
2. `dx build --release --platform web --features web` - Compiles Rust to WASM

```yaml
# Build job uploads:
- uses: actions/upload-artifact@v4
  with:
    name: web-assets
    path: target/dx/unified-hifi-control/release/web/public/

# Platform jobs download:
- uses: actions/download-artifact@v4
  with:
    name: web-assets
    path: public/
```

### 8. NAS Package Building (Synology SPK, QNAP QPKG)

NAS packages reuse pre-built Linux binaries and web assets - no compilation needed.

**Synology SPK:** Built directly with `tar`, not the full Synology toolkit.

```yaml
- name: Build Synology SPK
  run: |
    # Create package.tgz with binary + web assets
    tar -czf package.tgz -C package .

    # Build SPK archive (tar format per Synology spec)
    tar -cf "UnifiedHifiControl-${ARCH}-${VERSION}.spk" \
      INFO PACKAGE_ICON.PNG PACKAGE_ICON_256.PNG \
      package.tgz scripts conf WIZARD_UIFILES
```

**Why not use Synology's pkgscripts-ng toolkit?**
- Toolkit downloads ~1GB chroot environment
- Creates both debug and release SPKs (no way to skip debug)
- Takes 5+ minutes vs seconds for direct tar
- We already have cross-compiled binaries - no need for their cross-compiler

**SPK structure** (per [Synology Developer Guide](https://help.synology.com/developer-guide/synology_package/introduction.html)):
```
spk/
├── INFO                    # Package metadata
├── package.tgz             # Binary + web assets
├── scripts/                # start-stop-status, postinst, preuninst
├── conf/                   # privilege, resource
├── PACKAGE_ICON.PNG        # 72x72 icon
├── PACKAGE_ICON_256.PNG    # 256x256 icon
└── WIZARD_UIFILES/         # Install/uninstall UI (optional)
```

**QNAP QPKG:** Uses the official qbuild tool via Docker:

```yaml
- name: Build QPKG with Docker
  run: |
    docker run --rm --platform linux/amd64 \
      -v "$(pwd)/qnap-build:/src" \
      owncloudci/qnap-qpkg-builder \
      sh -c '/usr/share/qdk2/QDK/bin/qbuild --build-dir /src/build'
```

## Build Matrix

| Target | Caching | Build Tool | Default | Label |
|--------|---------|------------|---------|-------|
| Web Assets (WASM) | sccache + rust-cache | dx | Always | - |
| Linux x86_64-musl | rust-cache | cargo-zigbuild | Always | - |
| Linux aarch64-musl | rust-cache | cargo-zigbuild | Release | `build:linux-arm` |
| Linux armv7-musl | rust-cache | cargo-zigbuild | Release | `build:linux-arm` |
| macOS universal | sccache + rust-cache | cargo + lipo | Release | `build:macos` |
| Windows x86_64 | sccache + rust-cache | cargo | Release | `build:windows` |
| Docker x64 | N/A | pre-built binary | Release | `build:docker` |
| Docker multi-arch | N/A | pre-built binaries | Release | - |
| Synology SPK | N/A | tar | Release | `build:synology` |
| QNAP x64 | N/A | qbuild (Docker) | Release | `build:qnap` |
| QNAP arm64 | N/A | qbuild (Docker) | Release | `build:qnap-arm` |
| Linux deb (x64/arm64/armv7) | N/A | fpm | Release | `build:linux-packages` |
| Linux rpm (x64 only) | N/A | fpm | Release | `build:linux-packages` |
| LMS Bootstrap ZIP | N/A | zip | Release | `build:lms` |
| LMS Full ZIPs | N/A | zip + binary | Release | `build:lms` |

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

## Lessons Learned

1. **Single workflow, conditional jobs**: One `build.yml` prevents drift between PR and release builds. Use `if:` conditions to control what runs.

2. **Labels for PR customization**: Instead of separate "full build" workflows, use labels like `build:all` to enable extra builds when testing specific platforms.

3. **sccache + rust-cache**: Use both for native builds. sccache caches `.o` files, rust-cache caches proc-macro dylibs. zigbuild jobs can only use rust-cache (sccache incompatible with zig wrapper).

4. **Avoid containerized cross-compilation**: `cross` runs cargo in Docker containers, breaking Cargo's fingerprint caching. `cargo-zigbuild` cross-compiles without containers.

5. **Universal macOS via lipo**: Build each arch with native cargo, combine with `lipo`. zigbuild can't find macOS system frameworks.

6. **QEMU for cross-arch testing**: Smoke test armv7 binaries on x86_64 runners. Catches real issues.

7. **Registry locality matters**: GHCR from GitHub Actions is ~10x faster than Docker Hub.

8. **Pin tool versions in cache keys**: `dx-cli-0.7.3` ensures cache invalidation when upgrading tools.

9. **Direct zig download**: Downloading zig directly is faster than package managers.

10. **Build NAS packages directly**: Synology's toolkit downloads 1GB+ and creates unwanted debug packages. Build SPKs directly with `tar`. QNAP's qbuild is lightweight enough to use via Docker.

11. **Conditional builds with `hashFiles`**: For jobs that conditionally build artifacts (like ARM packages when ARM binaries are available), use `if: hashFiles('path/to/file') != ''` to check if a file exists at step runtime. Combined with `always()` at job level and `merge-multiple: true` in artifact downloads, this allows graceful handling of optional dependencies.

## LMS Plugin Binary Bundling

The LMS plugin supports two distribution modes:

### Bootstrap ZIP (Default for Releases)

**File:** `lms-unified-hifi-control-VERSION.zip`

- Contains only Perl code and plugin metadata
- Small download (~50KB)
- On first run, downloads binary + web assets from GitHub releases
- Works on any platform (downloads correct binary for detected architecture)
- Requires network access on first run
- Best for: end users who want small downloads and automatic updates

### Full ZIPs (For PR Testing and Offline)

**Files:** `lms-unified-hifi-control-VERSION-PLATFORM.zip`

Available platforms:
- `linux-x64` - Intel/AMD Linux servers
- `linux-arm64` - ARM64 Linux (Raspberry Pi 4, etc.)
- `linux-armv7` - ARMv7 Linux (Raspberry Pi 2/3, etc.)
- `macos` - macOS (Universal binary: Intel + Apple Silicon)
- `windows` - Windows 64-bit

Characteristics:
- Contains bundled binary in `Bin/` directory
- Contains bundled web assets in `public/` directory
- Larger download (~15-25MB depending on platform)
- Works immediately without network access
- Best for: PR testing, offline installations, air-gapped systems

### Binary Lookup Priority

When the LMS plugin starts, `Helper.pm` looks for binaries in this order:

1. **Bundled binary** (`$pluginDir/Bin/unified-hifi-control`)
2. **Cached binary** (`$cacheDir/UnifiedHiFi/Bin/$binaryName`)
3. **Download** (from GitHub releases matching plugin version)

If a bundled binary exists, no download is attempted. The cached location is used for bootstrap ZIPs that download on first run.

### Testing LMS Changes in PRs

1. Add the `build:lms` label to your PR
2. Wait for CI to complete
3. Download `lms-plugin-linux-x64` artifact (or appropriate platform)
4. Install the ZIP in LMS via Settings > Plugins > Install Plugin from File

For macOS testing:
1. Add the `build:lms-macos` label instead (also triggers macOS binary build)
2. Download `lms-plugin-macos` artifact
