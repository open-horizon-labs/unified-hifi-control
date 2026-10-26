# Artwork deployment backend

UHC uses the existing Zensally UltraFace RFB-320 and MicroSalNet models, with
unchanged pixel preprocessing, face decoding/NMS, saliency interpretation and
crop computation. Only graph loading changes: ONNX is converted offline to
portable, decluttered NNEF. Tract optimizes the graph for the installation CPU
when loading it. No model download, subprocess, service or GPU is required.

The four Rust modules are derived from `crates/zensally-tract/src` at
Imazen Zensally commit `a8888d08c6d4bd1fe0969cdce40da6c01b8032be`.
Keep them aligned when updating Zensally; `tests/artwork_inference.rs` compares
this backend with that exact upstream implementation. Original implementation
remains a dev dependency only. Run `bash scripts/check-artwork-runtime.sh` to
reject an accidental ONNX importer in the normal server dependency graph.

## Reproduce models

Use the pinned checkout already fetched by Cargo:

```sh
cargo run --release --example convert-artwork-models -- /path/to/zensally-checkout
```

The converter checks the compressed ONNX SHA-256 before conversion, fixes
NCHW input shapes to `[1,3,240,320]` (face) and `[1,3,256,256]` (saliency), and
serializes a typed, decluttered graph. **Do not serialize a CPU-optimized graph**:
it may select instructions unavailable on another installation CPU.
Tract is pinned at `9f6e40611e345ff46481388d4d2c591e5e91a278`.
Tar metadata and gzip timestamps are deterministic; regeneration must leave
both checked-in files unchanged.

| Model | Input `.onnx.gz` SHA-256 | Output `.nnef.tar.gz` SHA-256 |
|---|---|---|
| UltraFace | `3421320bdcaaeb83ae9a0583f525194299b6976560cde12cb92f62b347f79ed1` | `a0a5b0918a45cd99c30f01f4a2bb589a93ce0337b3e31adeaa3df807528de44b` |
| MicroSalNet | `0d8a0c14027d613fb7ff096851fc165128cd0228314672d2dd572468aa729a63` | `65b65de37e6998ef5c321a7e7d9a30ddf1979ddeaf9583d00d2b980798e80bec` |

## Behavior checks

```sh
cargo test --test artwork_inference -- --nocapture
# Additional private/local artwork without checking covers into the repository:
UHC_ARTWORK_EQUIVALENCE_IMAGES=/path/portrait.jpg:/path/cover.png \
  cargo test --test artwork_inference -- --nocapture
```

Checks compare finite face outputs and every saliency value (tolerance 1e-5),
then crop geometry at four aspect ratios. Generated RGB/RGBA/BGRA images cover
square, wide, tall and one-pixel input; the existing crown fixture must produce a face detection.

## Licensing

Copied source retains Zensally's `AGPL-3.0-only OR LicenseRef-Imazen-Commercial`
terms; both original license texts are included here. Converted weights retain
their original terms and attribution in `public/licenses/artwork-models.txt`
and `public/licenses/ultraface-mit.txt`. Converting serialization changes no
weights and grants no additional rights.
