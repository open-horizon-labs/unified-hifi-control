# Artwork composition (#770)

## Execute: selected approach and checks

The aim is to fill Frame and Android artwork viewports with proportionate artwork, using libvips attention to choose the crop placement. Scope is the existing UHC image surface and its runtime packaging. Android UI changes, firmware releases, new endpoints, and merging this PR are outside this change. Success means matching browser and packed-image defaults, an explicit complete-cover option, actual attention cropping, bounded failures, preserved original-image access, and a PR against `integration/v4-hiphi-readiness-2026-10-02`.

Architecture dissent: FFI would couple the Rust binary and its cross-compilation matrix to native libvips linking. Running the executable keeps the existing static binaries and isolates native failures, but adds temporary files, subprocess overhead, and a runtime dependency. Choose the executable with bounded workers and a deadline; ship the dependency in container runtimes and Linux packages. A missing or failed executable falls back to centered fill with a warning. Stop or adjust if native attention cannot be exercised, processing blocks the async runtime, the client query still adds margins, or packaging cannot supply the executable.

Before implementation, write and run client tests that fail the existing behavior. The regression checks must reject these tempting patches: changing only RGB565 while browser images bypass composition; honoring the old 10% budget and keeping Frame margins; letting packed-format selection override explicit fit; using `size=force` and stretching subjects; substituting center crop while calling it attention; blocking the async reactor; hanging indefinitely on a native process; and omitting the runtime dependency.

## Client behavior

The existing `/now_playing/image` and `/knob/now_playing/image` endpoints use attention-based fill for requested viewport dimensions, including RGB565 and ACeP6 output. `scale=smart` selects the same default. `scale=fit` retains the complete source on the existing gallery mat and takes precedence over format selection. The legacy `crop_limit` query field remains accepted for older Frame clients, but no longer limits the default smart crop: enforcing a 10% budget would prevent a square sleeve filling a landscape frame. No route or payload fields are added.

Original-image fetches through `AppState::get_image` with no dimensions, output format, or composition policy retain the provider bytes. The hosted bounded-source lane likewise keeps its original-image behavior. The internal legacy bounded-center policy remains available for callers that explicitly request it. Generated placeholders retain their existing rendering.

Attention changes crop placement, not the amount removed. Filling a square source into a 16:9 or 9:16 viewport removes 43.75% of the source area. Both portrait and landscape source/viewport combinations are supported; EXIF orientation is applied before crop selection. It cannot guarantee preservation of typography, the artist's intent, or the subject a listener prefers. Use explicit fit for covers that should remain complete. Clients should request the measured artwork area and invalidate the crop when its aspect changes.

## Runtime dependency

UHC calls `vips thumbnail` with `--crop=attention`, `--size=both`, and the requested width and height. `size=force` must never be used because it breaks aspect ratio. Inputs are decoded, oriented, limited, and normalized to PNG before reaching libvips; untrusted filenames and shell execution are avoided. Source decode is capped at 16,777,216 pixels/128 MiB; each dimension is capped at 8192. Output is capped at 8,388,608 pixels (1,536,000 for e-ink). An extreme aspect ratio requiring over 67,108,864 intermediate cover pixels uses the centered fallback, which crops before resizing to keep its allocations bounded. Processing runs on two blocking workers, with the permit retained across cancellation. Native children have a three-second deadline and are killed and reaped on timeout. Temporary directories are removed on success and failure. SVG placeholders retain their existing path; unsupported/invalid raster composition retains the existing conversion-error behavior.

Container images install `vips-tools` on Alpine and `libvips-tools` on Debian. Linux deb/rpm packages declare `libvips-tools`/`vips-tools`; the Arch package declares `libvips`. For standalone macOS installs, run `brew install vips` and ensure the executable is on the UHC process's PATH. For standalone Debian/Ubuntu installs, install `libvips-tools`. Other standalone/NAS/Windows environments may install the libvips executable separately; without it, artwork still fills using a centered crop. Ensure the service account can find `vips` and create temporary files. Libvips availability and version depend on the runtime, independently of UHC's static binary. Package evidence: [Debian](https://packages.debian.org/bookworm/graphics/libvips-tools), [Fedora](https://packages.fedoraproject.org/pkgs/vips/vips-tools/), [Arch executable](https://archlinux.org/packages/extra/x86_64/libvips/files/).

Primary references: [libvips thumbnail](https://www.libvips.org/API/current/ctor.Image.thumbnail.html), [attention selection](https://www.libvips.org/API/8.17/enum.Interesting.html), and [smartcrop](https://www.libvips.org/API/8.17/method.Image.smartcrop.html). Thumbnail resizes before selecting the crop; smartcrop alone only crops.

## Verification and review

TDD: three newly written client tests failed before implementation: the normal image remained 100×100 instead of 160×90, Frame's existing smart/budget query added white margins, and explicit packed-image fit was overridden by format selection. The completed change passes 19 image tests, 78 direct HQPlayer/client tests, two API-contract tests, and 16 CI-workflow tests. `cargo fmt --all -- --check`, `git diff --check`, and the repository's production `cargo clippy -- -D warnings` pass. An exploratory `cargo clippy --lib --tests` encounters the existing crate's denied test unwrap/expect patterns; the production lint gate is unchanged.

The Dioxus fullstack release-profile verification builds both server and WASM with explicit client/server feature sets (`@client --no-default-features --features web @server --features server,naa-proxy`). LTO is disabled/codegen-units=16 for this local verification, as in CI asset generation. The local toolchain's rust-objcopy required `DYLD_LIBRARY_PATH` pointing to its existing toolchain `lib` directory to find libLLVM during packaging; no toolchain files were changed. Container smoke checks install the native tools and produce an exact 160×90 attention thumbnail using Alpine 3.20/libvips 8.15.2 and Debian bookworm/libvips 8.14.1; macOS tests use libvips 8.18.7.

| Risk / assumption | Disposition | Tempting patch rejected / evidence |
|---|---|---|
| Default must fill on all clients | Retired | Browser bypass and legacy Frame mat rejected by client image tests, including portrait output and headers |
| Attention must choose content | Retired | Off-center circle is absent in center crop but retained by actual vips; default, normal PNG, and RGB565 outputs share that placement |
| Preserve proportions and upscale small art | Retired | Round-subject geometry on tall/wide sources and portrait viewport; PNG/JPEG upscaling checks; no force option |
| Complete-cover opt-out | Retired | Explicit fit wins over packed format and preserves complete source on mat |
| Source geometry/orientation | Retired | EXIF test and inspection of provider dimensions: composed policies request source aspect rather than final provider dimensions |
| Async blocking and cancellation | Retired by inspection | Conversion and native work are inside spawn_blocking; acquired worker permit moves into that closure, outliving a canceled caller |
| Hang, leaking process/temp files | Retired | Fake stalled native process is killed/reaped; its recorded temporary source directory is gone after error |
| Excessive allocations from extreme geometry | Retired | Zero/oversized/skinny targets and oversized source rejected; pathological wide-to-tall fixture rejects native intermediate budget and succeeds through bounded crop-before-resize |
| Missing runtime / platform linking | Retired for shipped containers and package declarations | Executable strategy avoids FFI; dependency/workflow contracts and actual Alpine/Debian thumbnail smoke checks; missing executable exercises centered fallback |
| Original-source consumers and cached Frame art | Retired | Original HQPlayer image bytes unchanged without composition request; packed image identity/policy/geometry cache checks pass |
| Route changes and integration-branch validation | Retired | API contract unchanged; Build/API Guard include integration/v4-* PR targets |
| Artistic crop preference and exact WONNIE behavior | Accepted with rationale | Physical device and human preference boundary; fixtures prove mechanism, not aesthetic success or firmware APK access |

Review disposition: Continue. Necessary, aligned, sufficient, and scoped to the selected artwork mechanism. Portrait client request sizing is a companion Android correction; the large-control UI redesign remains outside this backend PR. No merge/release is performed. Real-cover crop quality and operation on the physical WONNIE frame require human verification: synthetic adversarial fixtures establish the mechanism and geometry, but cannot establish aesthetic preference or APK access.
