# ADR 002: Single Binary Asset Distribution

## Status
**Accepted**

## Problem Statement

**Users cannot reliably run the binary after installation.**

The current architecture requires:
1. Binary installed to `/usr/bin/unified-hifi-control`
2. Web assets installed to `/usr/share/unified-hifi-control/public/`
3. `DIOXUS_PUBLIC_PATH` environment variable set in service files
4. Dioxus runtime creating symlinks or finding assets

This fails when:
- Package scripts misconfigure paths (AUR, deb, rpm all had bugs)
- `ProtectSystem=strict` blocks runtime symlink creation
- Users run binary directly without env vars set
- Asset folder gets deleted or moved

**Root cause:** Dioxus fullstack hardcodes looking for `public/` relative to the executable, requiring workarounds in every deployment target.

## Context

The current build process produces:
1. A server binary (`cargo build --release`)
2. Separate web assets (`dx build --platform web` → `public/` folder with WASM, JS, HTML, CSS)

This requires:
- Setting `DIOXUS_PUBLIC_PATH` environment variable in all deployment scripts
- Shipping/installing assets alongside the binary
- Complex package manifests for AUR, deb, rpm, Synology, QNAP, Windows, macOS, LMS

Users report issues when assets aren't found (#134), and the packaging complexity leads to bugs like `ProtectSystem=strict` blocking runtime symlinks.

## Research Findings

### Fullstack Build Discovery

`dx build --fullstack --release` produces a significantly simpler output:
- **SSR (Server-Side Rendering)** with hydration data embedded in HTML
- **No WASM bundle** - the server renders HTML directly
- **Only 23KB of CSS assets** instead of ~2MB WASM + JS + HTML

The fullstack binary starts and serves pages without the `public/` folder, but CSS returns 404.

### Asset Breakdown

| Asset Type | Current (web build) | Fullstack Build |
|------------|---------------------|-----------------|
| WASM bundle | ~1.5MB | Not needed (SSR) |
| JavaScript | ~50KB | Not needed (SSR) |
| index.html | ~2KB | Not needed (SSR) |
| tailwind.css | 20KB | 20KB |
| dx-theme.css | 3KB | 3KB |
| Component CSS | 8KB | 8KB |
| favicon.ico | 4KB | 4KB |
| apple-touch-icon.png | 25KB | 25KB |
| hifi-logo.png | 5KB | 5KB |
| **Total** | **~1.6MB** | **65KB** |

## Options

### Option A: DIOXUS_PUBLIC_PATH (Current + Fix)
Keep separate assets, use Dioxus's native `DIOXUS_PUBLIC_PATH` env var.

**Pros:**
- Minimal code changes
- Standard Dioxus deployment

**Cons:**
- Still requires shipping assets folder
- All deployment scripts need env var
- 31KB of external files

### Option B: Inline CSS with include_str!
Embed CSS at compile time using `include_str!` and `document::Style`.

```rust
const TAILWIND_CSS: &str = include_str!("../../../public/tailwind.css");
document::Style { {TAILWIND_CSS} }
```

**Pros:**
- True single binary
- No external files or env vars needed
- Simple deployment: copy binary, run

**Cons:**
- 31KB added to binary size (negligible)
- CSS changes require recompile
- Need to inline ~6 CSS files

### Option C: rust-embed
Use `rust-embed` crate to embed assets and serve from memory.

**Pros:**
- Standard pattern for embedded assets
- Works with any file type (images, fonts)

**Cons:**
- New dependency
- Need custom axum middleware before Dioxus
- More complex than inline CSS

### Option D: Data URLs for CSS
Encode CSS as base64 data URLs in link href.

**Pros:**
- No code changes to serving logic

**Cons:**
- 33% larger (base64 overhead)
- Ugly URLs in HTML source

## Decision

**Recommended: Option B (Inline CSS)**

Rationale:
1. Simplest implementation - just change `document::Link` to `document::Style`
2. True single binary - copy and run, no configuration
3. Eliminates entire category of deployment bugs
4. 31KB binary size increase is negligible (~0.3% of 10MB binary)

## Implementation Plan

1. Switch CI from `cargo build` + `dx build --platform web` to `dx build --fullstack`
2. Replace `document::Link { href: asset!(...) }` with `document::Style { {include_str!(...)} }`
3. Handle favicon/icons (either inline as data URLs or accept they need external serving)
4. Remove `DIOXUS_PUBLIC_PATH` from all package scripts
5. Remove `web-assets` CI artifact and tarball

## Files Requiring CSS Inlining

- `src/app/components/layout.rs` - tailwind.css, dx-components-theme.css
- `src/components/navbar/component.rs` - navbar/style.css (3.7KB)
- `src/components/tabs/component.rs` - tabs/style.css (1.9KB)
- `src/components/button/component.rs` - button/style.css (1.4KB)
- `src/components/collapsible/component.rs` - collapsible/style.css (0.8KB)

## Image Handling

Images (65KB total) can be embedded as base64 data URLs:

```rust
// In layout.rs
const FAVICON: &str = concat!("data:image/x-icon;base64,", include_str!("../../../public/favicon.ico.b64"));
document::Link { rel: "icon", href: FAVICON }

// In nav.rs
const LOGO: &str = concat!("data:image/png;base64,", include_str!("../../../public/hifi-logo.png.b64"));
img { src: LOGO, alt: "Hi-Fi Control" }
```

Build step needed: `base64 < public/favicon.ico > public/favicon.ico.b64`

Alternative: Use `include_bytes!` + runtime base64 encoding (adds ~1KB code, avoids build step).

## Open Questions

1. **Build complexity**: Pre-encode base64 in build, or runtime encode?
2. **Cross-compilation**: Does `dx build --fullstack` work with cross-compilation targets?
3. **Binary size**: 65KB embedded is ~0.6% of 10MB binary - acceptable?

## Consequences

**Positive:**
- Single binary distribution
- No deployment configuration required
- Eliminates PUBLIC_DIR/DIOXUS_PUBLIC_PATH complexity
- Simpler CI (one build step instead of two)

**Negative:**
- CSS changes require full recompile
- Slightly larger binary (~31KB)
- May need fallback for favicon/images

## Implementation Notes

### Avoiding the `public/` Directory Requirement

Dioxus's `serve_dioxus_application()` tries to serve static assets from a `public/` directory next to the executable. Since we embed all assets, we use `serve_api_application()` instead, which provides the same SSR functionality without the static asset serving:

```rust
// Instead of: .serve_dioxus_application(ServeConfig::new(), app::App)
.serve_api_application(dioxus::server::ServeConfig::new(), app::App)
```

This achieves true single-binary distribution - no external files or directories needed
