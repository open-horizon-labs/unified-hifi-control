# GitHub Release Workflow Caching Strategy

This document explains the caching and artifact reuse strategies in `.github/workflows/release.yml` and why each is needed.

## Overview

The release workflow builds for 6 targets across 3 platforms, plus web assets, Docker images, and platform-specific packages. Without caching, each release would take 30+ minutes and compile identical dependencies repeatedly.

## Caching Strategies

### 1. sccache for Native Builds

**Used by:** Web Assets (WASM), macOS, Windows

**Why:** [sccache](https://github.com/mozilla/sccache) caches individual compilation units, not just dependencies. This means incremental changes compile much faster than with directory-based caching.

**How:**
```yaml
- name: Setup sccache
  uses: mozilla-actions/sccache-action@v0.0.9

- name: Build
  env:
    SCCACHE_GHA_ENABLED: "true"
    RUSTC_WRAPPER: "sccache"
  run: cargo build --release
```

**Why not rust-cache?** rust-cache caches the `target/` directory but cleans up "anything that is not a dependency" on save. This removes WASM build artifacts that take significant time to regenerate.

### 2. rust-cache for Cross Builds

**Used by:** Linux musl builds (x86_64, aarch64, armv7)

**Why:** Cross-compiled builds use the [cross](https://github.com/cross-rs/cross) tool which runs inside Docker containers. The host's sccache is not accessible from inside these containers.

**How:**
```yaml
- name: Cache Rust (cross builds)
  uses: Swatinem/rust-cache@v2
  with:
    cache-directories: target/${{ matrix.target }}
```

**Why this works:** Cross mounts the host's `target/` directory into the container, so rust-cache's directory-based caching is effective here.

### 3. Tool Binary Caching

**Dioxus CLI:**
```yaml
- name: Cache Dioxus CLI
  uses: actions/cache@v4
  with:
    path: ~/.cargo/bin/dx
    key: dx-cli-0.7.3

- name: Install Dioxus CLI
  if: steps.cache-dx.outputs.cache-hit != 'true'
  run: cargo install dioxus-cli@0.7.3 --locked
```

**Cross:**
```yaml
- name: Cache cross
  uses: actions/cache@v4
  with:
    path: ${{ github.workspace }}/.cargo/bin/cross
    key: cross-0.2.5
```

**Why:** These tools take 2-3 minutes to compile. Caching the binaries saves this time on every run.

### 4. GHCR Base Images

**Used by:** Dockerfile.ci, Dockerfile.release

**Why:** GitHub Actions runners are in the same datacenter as GHCR (GitHub Container Registry). Pulling base images from GHCR is significantly faster than Docker Hub, AWS ECR, or other registries.

**Also:** Docker Hub has rate limits (200 pulls/6 hours for free accounts) which can block CI runs.

**How:**
```dockerfile
FROM ghcr.io/linuxserver/baseimage-alpine:3.20
```

### 5. Web Assets Artifact Sharing

**Why:** Web assets (WASM + JS) are identical across all platforms. Building once and sharing via artifacts avoids 3x redundant WASM compilation.

**How:**
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

**Tarball for LMS Plugin:**
```yaml
- name: Create web assets tarball
  run: |
    cd target/dx/unified-hifi-control/release/web
    tar -czvf web-assets.tar.gz public/
```

The LMS plugin downloads this tarball at runtime since it can't bundle large binary assets.

## Build Matrix

| Target | Caching | Base Image |
|--------|---------|------------|
| Web Assets (WASM) | sccache | N/A |
| macOS x86_64 | sccache | N/A |
| macOS aarch64 | sccache | N/A |
| Windows x86_64 | sccache | N/A |
| Linux x86_64-musl | rust-cache | GHCR |
| Linux aarch64-musl | rust-cache | GHCR |
| Linux armv7-musl | rust-cache | GHCR |
| Docker multi-arch | N/A | GHCR |

## Lessons Learned

1. **sccache vs rust-cache:** Use sccache for native builds, rust-cache for containerized builds (cross).

2. **CARGO_HOME placement:** For cross builds, set `CARGO_HOME` inside the workspace so the container can mount it:
   ```yaml
   env:
     CARGO_HOME: ${{ github.workspace }}/.cargo
   ```

3. **Cross binary path:** Use full path `$CARGO_HOME/bin/cross` since the modified `CARGO_HOME` isn't in `$PATH`.

4. **Registry choice matters:** GHCR from GitHub Actions is ~10x faster than external registries due to network locality.

5. **Tool version pinning:** Pin tool versions in cache keys (`dx-cli-0.7.3`, `cross-0.2.5`) to ensure cache invalidation on upgrades.
