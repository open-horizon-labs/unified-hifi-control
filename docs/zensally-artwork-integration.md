# Cached artwork composition (#772)

The goal is artwork that fills Frame and Android displays while retaining the subject, including portrait layouts. The approved approach combines Zensally's embedded UltraFace and MicroSalNet models with zenlayout's default Minimal crop solver. It replaces the default crop placement while retaining explicit fit, existing routes and payloads, and the libvips/centered fallback chain.

## Execution and dissent

The strongest reason to choose this pipeline is that it combines face regions with neural saliency and crop geometry, rather than assuming face detection alone solves composition. The owner approved trying Zensally after seeing actual offline outputs for the reported cover. The integration pins Zensally commit a8888d08c6d4bd1fe0969cdce40da6c01b8032be and zenlayout 0.2.2, matching that evaluation.

The contrary evidence is that a good single-cover result does not establish corpus-wide quality, small model files do not establish Pi 3 latency, and rectangular cover crops cannot preserve every widely separated subject. Functional failure would be repeated inference or an invalid crop; adoption failure would be a slow Pi display; opportunity cost would be introducing a larger inference dependency for covers that already look good. The adjusted implementation serializes inference, loads models lazily, shares 32 cached analyses by content rather than output size, caps the RGBA probe at 640 pixels per side, and keeps the existing fallback path. Cached 128×128 float heatmaps occupy approximately 2 MiB at capacity, excluding model and inference working memory. Cache eviction and restarts can repeat analysis. Tiny strip/display layout is unaffected.

Checks: reproduce the original lost-face failure before implementation; retain the detected face in landscape and portrait; reuse the same analysis for both orientations and repeated requests; invalidate on changed content; maintain bounded cache and crop bounds; verify native production lint and browser dependency isolation; produce a QNAP candidate for real hardware acceptance. Pi 3 performance and broad subjective crop quality require hardware/user evaluation and are not inferred from Mac timings.

## Licensing and model provenance

UHC retains its PolyForm Noncommercial license. Zensally and zenlayout offer AGPL/commercial licensing; the owner approved the commercial license path. Obtain and retain the applicable Imazen license separately; approval to use this dependency does not constitute a purchase receipt. Do not place license keys in source or build logs. The server embeds third-party model weights, whose upstream terms must accompany redistribution. Zensally's upstream model provenance and license files are authoritative.

Sources: [Zensally pipeline](https://github.com/imazen/zensally), [commercial license](https://github.com/imazen/zensally/blob/main/LICENSE-COMMERCIAL), [libvips maintainer on attention versus face detection](https://github.com/libvips/libvips/discussions/3767). Face detection is not itself a composition guarantee; the default solver permits partial focus visibility when geometry requires it.

The small Crown image in tests/fixtures/artwork/crown.png is the user-supplied reproduction of the crop failure, used only for regression testing. Its artwork is not licensed as UHC source code or bundled into the runtime.

## Local verification and review

The original landscape regression failed against libvips attention and passed with Zensally. Artwork tests passed for the default crop, browser PNG, RGB565 and ACeP6 Frame packing, and existing fit/fallback geometry. Cache tests passed for portrait/landscape reuse, changed content, eviction/capacity, and concurrent duplicate requests. Production clippy passed with both the pinned and latest stable toolchains. The Dioxus release client build passed after supplying the local Rust toolchain's LLVM library path to the strip tool; no repository workaround was needed. The web dependency tree excludes Zensally, tract and zenlayout, and the model notices are copied into embedded public assets.

Review verdict: aligned with the approved artwork-first aim, with no API/schema change or display-layout change. The crop composition remains a subjective hardware acceptance item; ARM compilation and QNAP packaging are CI gates, and Pi 3 runtime performance remains unmeasured. Small model files are not evidence of Pi latency.
