# Installer footprint: evidence and decisions (#895)

All sizes below are decimal bytes, not MiB. Measurements are artifact inspection
or controlled local builds, not estimates of QPKG download savings.

## Accuracy report

| Claim | Status | Evidence | Correction / scope |
|---|---|---|---|
| The main historical jump occurred in beta.4. | Verified | Released QPKGs: beta.3 13,857,169; beta.4 30,139,810. Corresponding Linux executables: 29,891,024 and 54,890,056. | Distinguish compressed installer from executable. |
| Native executable code grew 19.6 MB in beta.4. | Verified | ELF `.text`: 20,876,717 to 40,472,162 (+19,595,445). Total executable grew 24,999,032. | `.text` is not the whole file or installer. |
| The two embedded ONNX models total 1.94 MB. | Verified | Pinned Zensally UltraFace gzip 1,064,589 + MicroSalNet gzip 878,253 = 1,942,842. | Weight files alone do not explain the code increase. |
| All beta.4 growth was inference. | Unsupported | That release changed more than inference. | Do not attribute the entire historical delta without full-source ablation. |
| PR builds have different optimization from tags. | Verified | Previous `build.yml` overrides: tag fat LTO / 1 codegen unit; other events ThinLTO / 16. Identical beta.10 source trees produced executables 60,422,896 (tag) and 70,839,848 (PR). | Workflow optimization is a demonstrated difference; the artifact comparison is not an isolated linker experiment. |
| Native debug symbols caused the main increase. | Incorrect | Examined beta.3, beta.4, beta.10 tag/PR and beta.11 ELF executables were already stripped, with no native DWARF. | Native stripping was already enabled. |
| Browser debug data ships inside the native executable. | Verified | Extracted beta.10 WASM: 5,614,975 total, 1,508,386 debug bytes. beta.11: 5,931,673 total, 1,520,680 debug bytes. | Separate browser DWARF from native ELF DWARF. |
| NNEF reduces the inference runtime. | Verified with edit | Same pinned Tract revision, host, fat LTO, 1 codegen unit: standalone ONNX executable 16,922,736; NNEF 12,859,168. gzip -9: 6,632,034 vs 4,957,829. | Component experiment saves 4,063,568 raw / 1,674,205 compressed bytes. These are not full-UHC or QPKG deltas. |
| The inference change reduces a full UHC executable by about 4.13 MB on the measured host. | Verified with edit | ARM64 glibc host comparison: 49,260,720 to 45,131,648 bytes, same final browser bytes and profile. | Host output; small git-stamp difference disclosed below. QPKG savings remain unmeasured. |
| Preconversion preserves model results. | Verified with edit | Controlled deterministic tensor experiment: UltraFace 26,520 floats and MicroSalNet 16,384 floats bit-identical. Full pipeline test compares face outputs, every saliency value and crops. | Evidence applies to the tested host/corpus; platform CI and device testing remain necessary. |

Primary evidence: released executables/packages from
[beta.4 / #773](https://github.com/open-horizon-labs/unified-hifi-control/pull/773),
beta.10 tag and [PR build 40207148320](https://github.com/open-horizon-labs/unified-hifi-control/actions/runs/40207148320),
and [beta.11 build 40350402926](https://github.com/open-horizon-labs/unified-hifi-control/actions/runs/40350402926).
The beta.10 tag source commit `589649c7` and PR #878 head `1cf9c8ff` had identical source trees.

Source inspection: pinned Zensally
[`a8888d08`](https://github.com/imazen/zensally/tree/a8888d08c6d4bd1fe0969cdce40da6c01b8032be/crates/zensally-tract),
pinned Tract [`9f6e4061`](https://github.com/sonos/tract/tree/9f6e40611e345ff46481388d4d2c591e5e91a278),
and Dioxus CLI 0.7.10 bundling source. Tract's own
[introduction](https://github.com/sonos/tract/blob/main/doc/intro.md) and
[pipeline](https://github.com/sonos/tract/blob/main/doc/pipeline.md) explain
portable typed/decluttered graphs and deployment through NNEF.

## Solution space

**Problem:** Shipping installers carry unnecessary runtime code and browser debug
data, and differ in optimization depending on their triggering event.
**Key constraint:** Preserve offline artwork behavior and single executable installs
across NAS, Linux, macOS and Windows; no public API changes.
**Working story:** Build divergence and generic ONNX import machinery are removable
costs; model weights are a smaller, necessary cost.
**Success signal:** One shipping profile, zero browser debug data, no ONNX
importer in normal server dependencies, equivalent analysis/crops and smaller builds.
**Decision criteria:** Total installed/downloaded bytes, behavior, portability,
installation complexity and ongoing maintenance.
**Critical assumptions:** Decluttered NNEF retains model semantics, and CPU-specific
optimization can remain on the installation CPU.

| Option | Level | Approach | Main trade-off |
|---|---|---|---|
| A | Local optimum | Unify profiles and remove browser debug only | Lowest risk; leaves the import runtime. Selected as a necessary first layer. |
| B | Reframe | Convert models offline; ship Tract core/NNEF instead of ONNX import | Preserves the models; maintains four small upstream-derived processing modules and serialized graph files. Selected. |
| C | Redesign | Separate inference service, optional plugin or dynamic library | Moves bytes outside the executable without necessarily reducing total install size; adds NAS deployment and availability complexity. Rejected for this task. |
| D | Redesign | Replace/quantize models, use handcrafted crops, or hand-implement kernels | Potential further savings, but changes quality/numerics or greatly expands inference maintenance. Deferred pending a separate quality corpus and size target. |

**Interpretive variety:** A assumes compiler settings are the problem; B tests the
runtime dependency explanation; C changes deployment boundaries; D questions the
inference approach itself. If B failed equivalence or had no material savings,
retain A and revisit D with explicit quality criteria. Do not hide that failure by
silently dropping face or saliency inference.

## Risk retirement

| Risk / assumption | Disposition | Tempting patch the check must fail | Evidence / rationale | Stop or pivot if |
|---|---|---|---|---|
| Smaller output changes crops or skips a detector | Retired by evidence for tested corpus | Empty face results / constant saliency / changed normalization | Reference-backend comparison including positive face detection in the committed crown fixture; all saliency values and four crop ratios | Any finite-value, tolerance or geometry mismatch |
| Importer remains linked through a transitive dependency | Retired by evidence | Merely add an NNEF facade while retaining `zensally-tract` | `scripts/check-artwork-runtime.sh` checks normal server dependency graph, excluding dev dependencies | ONNX/HIR/transformers/pulse import stack appears |
| Model serialized with host CPU kernels | Retired by source inspection and portable graph loading | Serialize `.into_optimized()` output | Converter uses typed/decluttered only; loader optimizes on target | Cross-target model loading fails |
| Browser strip corrupts bindgen metadata or asset hashes | Retired by build/audit | Strip intermediate WASM or rewrite a final hashed asset | Dioxus strips after bindgen, before hashing; audit is read-only and tests preserve runtime metadata | Bindgen/build/validation or hydration fails |
| Restored cache contains browser DWARF | Retired by evidence | Audit only on cache misses | Audit runs unconditionally before upload; poisoned synthetic modules fail | Any forbidden section or source map survives |
| Smaller main executable merely relocates bytes | Accepted with rationale | Sidecar/plugin size comparison excluding sidecar | No new executable, service or model download introduced | Additional runtime install becomes necessary |
| Cross-CPU numerical drift / user-perceived crop quality | Accepted pending target CI/device testing | Claim universal equivalence from one host | Same weights and image processing; portable graph; local reference tests; platform build matrix | Target equivalence or real artwork regression |
| Faster/lower-memory inference | Unverified; no claim | Treat one warm timing as a benchmark | Serialization changes do not prove speed or memory improvement | A measured regression affects artwork responsiveness |

## Inference runtime findings

The old `tract_onnx::onnx()` calls `ops::register_all_ops`, retaining a general
ONNX importer and translation machinery even though UHC runs two fixed graphs.
Its normal dependency graph includes `tract-hir`, `tract-onnx-opl`,
`tract-transformers`, `tract-pulse`, `tract-pulse-opl` and `tract-extra` in addition
to core/NNEF/data/linalg. The deployment backend needs only the latter four Tract
crates. This removes generic model import; it does not remove neural inference.
Both detectors and their weights remain embedded and offline.

NNEF still registers a general core operation vocabulary and retains the linear
algebra kernels. Further reduction would require specializing that runtime,
quantizing/changing models, or implementing a small fixed-graph executor. None is
an automatic linker flag: each adds numerical/quality or maintenance risk. The
selected change removes an unnecessary format boundary while retaining the
existing inference implementation. A sidecar would not establish a reduction in
total installer bytes, and avoiding inference altogether would change the feature.

## Implementation and verification

`Cargo.toml` is the shipping optimization authority for every event: fat LTO,
one codegen unit, debug 0, stripped native output, abort-on-panic. Browser release
inherits it but retains bindgen metadata until processed. Dioxus explicitly strips
both names/debug and DWARF before final hashing. Its asset pass retains an empty
seven-byte name envelope with no symbols; the audit accepts only this empty
envelope and rejects every nonempty name section. An unconditional final audit also
rejects source maps and runs on restored caches. It never edits built assets.

Artwork models are preconverted into deterministic NNEF with fixed source hashes.
`crates/artwork-inference/README.md` records provenance, conversion and output
hashes. Upstream preprocessing, face decoding/NMS and saliency interpretation are
retained. The existing aggregator, artwork execution/cache ownership and HTTP API
are unchanged. The ONNX backend remains test/developer-only. The source Docker build uses the
same client-first audit and native profile; its obsolete Tailwind inputs are
replaced with the current source/public inputs.

Reproduce the isolated runtime experiment with `build/inference-footprint`:

```sh
cargo build --release --locked --manifest-path build/inference-footprint/Cargo.toml --features onnx
# Save resulting executable before building the other feature.
cargo build --release --locked --manifest-path build/inference-footprint/Cargo.toml --features nnef
# Run either executable: MODE MODEL_PATH HEIGHT WIDTH OUTPUT_F32_LE
# ONNX uses uncompressed source .onnx; NNEF accepts .nnef.tar.gz.
# UltraFace H/W = 240/320, MicroSalNet = 256/256.
```

Its input tensor is deterministic; compare output files byte for byte. The
`convert` feature is developer-only and can reproduce NNEF files from decompressed
ONNX models. Do not enable `convert` when measuring a deployment executable.

### Completed local validation

- `cargo test --workspace`: **3,503 passed, 0 failed, 37 ignored** across 112
  test binaries/doc-test groups. Workflow and hardening contracts were rerun after
  the final workflow edits: 17 + 6 passed.
- `cargo clippy --workspace -- -D warnings` and
  `cargo clippy -p uhc-artwork-inference -- -D warnings`: passed. An additional
  `--all-targets` attempt encountered pre-existing warnings in test fixtures; this
  does not change the repository's standard blocking lint command.
- Browser release built with Dioxus CLI 0.7.10. Final WASM **4,424,464 bytes**,
  validated by Node's WebAssembly validator; final-asset audit passed. Only
  `target_features` and an empty name envelope remain. Six poisoned-artifact/audit
  tests, six release-hardening tests, and the existing tool/cache tests pass.
- Full pipeline ONNX/NNEF comparison passed for generated input, crown fixture
  and a local upstream portrait with **one detected face**. The committed crown
  fixture also asserts positive face detection, so CI cannot pass by dropping
  face inference. Every saliency value
  and all four crop geometries matched within the test's stated tolerance.
- Offline converter regenerated both model files byte for byte. Normal dependency
  graph check passed; backend source was diffed against the pinned upstream to
  verify preprocessing/output logic was preserved.
- Native executable builds and `--help` loading smoke passed on
  **aarch64-unknown-linux-gnu**, Rust 1.99.1, fat LTO / one codegen unit, default
  `server,naa-proxy` features. Both executables contain the exact same finalized
  4,424,464-byte browser module. Native output is stripped, without DWARF.

| Full UHC host build | Executable | gzip -9 |
|---|---:|---:|
| ONNX reference backend | 49,260,720 | 21,694,543 |
| NNEF deployment backend | 45,131,648 | 19,979,896 |
| Measured reduction | **4,129,072 (8.38%)** | **1,714,647 (7.90%)** |

This is a controlled current-source backend comparison, using a temporary source
copy with only the backend dependency/import restored to ONNX. The baseline copy
has git stamp `unknown`; the working-tree build has the base commit stamp. These
small metadata differences remain a measurement caveat. Both use the same
browser bytes, profile, compiler, host and features. This is **glibc ARM64 host
output, not the x86_64-musl QNAP package**. It demonstrates a full-UHC reduction,
but does not establish a QPKG reduction of the same number of bytes.

Executable SHA-256:

- ONNX: `77572536b96b9ca07d0f8fed539f511b94bcf7c85d34045bd3dbe7b7b53f6407`
- NNEF: `54a735ca92bba371dd3c669172e753c28f5deb4c2b9a3619cc6c9f1a677a48b7`

Cross-platform CI/package builds and a fresh source Docker image build are still
required; no local Docker daemon was available. No package-size or universal
cross-CPU equivalence claim is made from the host experiment.

### Remaining evidence boundaries

No new QPKG savings are claimed until the same-source package is actually built.
The component probe includes the runtime, loading and tensor execution, with
models read externally; it excludes the entire UHC link graph. Converted embedded
models total 1,937,327 bytes, versus 1,942,842 for ONNX gzip (5,515 fewer).
The full-UHC experiment isolates the current backend choice, not the historical
beta.4 source change. Historical artifacts establish the jump, not exact causal attribution to every
crate. Host inference latency, peak memory, target CPU equivalence and broader
real-artwork quality need separate measurements before making performance claims.
No domain-expert factual claim is left unresolved; the remaining gaps are build
and device measurements, not licensing or API approval requests.

## Execute: Pi workload follow-up on #896

**Aim:** Avoid artwork inference and duplicate conversion work when neither is
needed, while preserving crop geometry, pixel processing, API payloads and cache
budgets. PR #896 now targets `integration/v4-beta.11`; that base was merged into
the feature branch without rewriting either branch. The size measurements above
were made before this beta integration/Pi follow-up and are not measurements of
the new combined head.

**Delivered:**

- Exact matching aspect ratios bypass model loading, image hashing and inference;
  the existing full-image resampler handles the resize. Equality uses u64 cross
  products, preserving distinct aspect ratios without floating-point tolerances.
- Concurrent requests for a cacheable output share a per-key conversion lock and
  recheck the output cache before decoding/resampling. Different output keys can
  still run concurrently. The existing global two-worker bound remains.
- Cancellation transfers neither a running worker's slot nor its output-key lock
  to another request. Both are owned by the blocking task until conversion ends.
- Failures remain untagged/uncached and retryable. Oversized outputs retain the
  existing serve-without-caching policy; queued requests may reconvert these
  rather than violating the RAM budget. Completed key locks are weak references,
  pruned on subsequent acquisition, and do not retain a growing cover history.
- `artwork-inference-bench` reports model-load time, first inference and warm
  p50/p95 for 20 uncached runs, using the server's bounded probe dimensions.

### Adversarial verification / risk retirement

| Risk | Disposition | Tempting patch this must fail | Evidence |
|---|---|---|---|
| Same-aspect resize still initializes inference | Retired | Cache the inference result but still run once unnecessarily | New test failed with one analysis before fix; now asserts zero analyses and no loaded analyzer for square, wide and tall resize cases, with exact resampler output equality |
| Optimization disables useful subject-aware crops | Retired | Always bypass inference | Existing crown landscape test requires the face-preserving top crop across display/RGB565 encoding; analysis/crop reference tests remain |
| Duplicate decoding/resampling survives inference coalescing | Retired | Keep only the analysis mutex or omit the post-lock output-cache check | Eight-request test failed with eight conversions before fix, now requires one conversion and shared result identity |
| Cancelled caller releases a still-running conversion | Retired | Keep key lock on the async waiter | Gated blocking conversion + aborted waiter + duplicate request test; duplicate must reuse the first result with only one converter call |
| New global lock serializes unrelated output sizes | Retired | One mutex for every conversion | A blocked output must not prevent a different-size output from completing |
| Cache failures or leak output identity history | Retired | Retain failed results or strong permanent key locks | Failure/retry and 40-key weak-lock reclamation test |
| Claim Pi performance from host measurements | Accepted with rationale | Label a desktop timing as Pi timing | No Pi hardware is identified/connected in this session. Hardware latency and peak RSS remain unmeasured; benchmark is a measurement tool, not proof of a Pi budget |

### Pi measurement procedure

Build the example in release mode on a suitable build host, then run the executable
on the actual Pi. For an ARM64 musl cross build using the repository's existing
Zig tooling:

```sh
cargo zigbuild --release --target aarch64-unknown-linux-musl --example artwork-inference-bench
# Copy target/aarch64-unknown-linux-musl/release/examples/artwork-inference-bench to the Pi.
# On the Pi, record its model, RAM, OS/architecture and thermal/throttling state:
/usr/bin/time -v ./artwork-inference-bench
# Optional provider/local cover:
/usr/bin/time -v ./artwork-inference-bench /path/to/cover.jpg
```

Building natively uses `cargo build --release --example artwork-inference-bench`.
Run `time` on the built executable, not on Cargo: compiler memory is unrelated to
inference memory. This tool intentionally bypasses the caches to measure the cost
of a genuine miss. Its peak RSS covers this isolated process, not total running
UHC memory. Measure HTTP cache-hit/miss latency separately when setting a Pi budget.

**Follow-up checks:** Both new behavior tests were observed failing before the
fixes (one unnecessary analysis; eight duplicate conversions). The 11 targeted
artwork tests now pass, including cancellation, independent keys, failure retry,
lock reclamation and existing crop/cache tests. Standard workspace clippy passes.
The refreshed beta.11 browser release is 4,742,014 bytes, passes WebAssembly
validation and contains no debug data under the final-asset audit. Full-workspace
results and platform/package results are tracked on PR #896.
