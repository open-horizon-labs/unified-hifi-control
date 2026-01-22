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
- uses: mozilla-actions/sccache-action@v0.0.9
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
    key: dx-cli-0.7.3
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

**Synology SPK:** Built directly with `tar` (not the 1GB pkgscripts-ng toolkit). See [Synology Developer Guide](https://help.synology.com/developer-guide/synology_package/introduction.html) for SPK structure.

**QNAP QPKG:** Uses `qbuild` via Docker.

## Build Matrix

| Target | Caching | Build Tool | Default | Label |
|--------|---------|------------|---------|-------|
| Fullstack Check | rust-cache | dx build --fullstack | Always | - |
| Linux x86_64-musl | rust-cache | cargo-zigbuild | Always | - |
| Smoke Test | N/A | curl | Always | - |
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
