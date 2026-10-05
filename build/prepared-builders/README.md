# Prepared build environments

`source.yml` retains the reviewed Actions tool setup extracted from the previous
WASM/Linux jobs. `rust-toolchain.toml` supplies the Rust release. Installer
commands and tool versions live in these Actions steps, not in a handwritten
Dockerfile. `profile.json` selects jobs/setup steps and image destinations.

Run the **Prepare build environments** workflow explicitly after updating setup.
It regenerates the tools/runner recipes and workflow variants with the pinned
homelab converter, publishes the hosted tools image to GHCR, and uploads the
prepared-builder recipe and short-lived prepared-tools image artifacts. Existing
content-derived image tags are reused. One publisher builds the fleet derivative and pushes
it to the NAS-backed registry; both fleet members pull/cache that image.

Apply the generated fleet workflow after its images and repository-scoped broker
policy are ready. Hosted containers authenticate with the job token (`packages: read`); grant the
consuming repository access to the GHCR package. Fleet publishers load the tools
artifact locally before building the generated runner derivative. Verify the manifest and
image digests and run the full Build workflow afterward. The preparation workflow
uses `--setup-source` so it can regenerate already converted jobs.

Fleet lint, WASM, and native Linux jobs restore Cargo target state and registry
sources directly from NAS MinIO using `scripts/fleet-build-cache.py`. The key
covers job/target, Cargo.toml/Cargo.lock, pinned toolchain, target flags, and
compiler profile. Ephemeral containers use stable Cargo/Rustup paths so restored
dependency fingerprints remain reusable. Each job keeps independent working
state; S3 publishes complete snapshots atomically.

Snapshots are written once per dependency key and reused across source changes.
This avoids repeated large uploads. Hosted jobs retain Swatinem's cache. NAS
failures are visible but do not block builds. Cargo fingerprints and the existing
main-crate clean still force application rebuilds. Linux jobs report compiler
cache statistics and upload compilation timing reports.

Regenerate with the maintained converter, not by editing Dockerfiles:

```sh
python runner/builder/convert.py /path/to/uhc/.github/workflows/build.yml \
  --setup-source /path/to/uhc/build/prepared-builders/source.yml \
  --config /path/to/uhc/build/prepared-builders/profile.json \
  --output /tmp/uhc-builder
```

The Mac jobs use their existing persistent Tart guest, with one admitted job
at a time. Each architecture's target state lives outside the checkout under
`~/Library/Caches/uhc-build/`, linked into the job as `target/`. Checkout cleans
the symlink without deleting the cache. Hosted Macs retain Swatinem's cache;
fleet Macs avoid target archive downloads/uploads. They are not Linux containers.

The main Build workflow owns formatting, workspace clippy, and workspace tests
(including the HTTP API contract). The former streaming-alpha workflow repeated
those same checks on v4 PRs and has been consolidated into Build.

Untagged validation builds use ThinLTO and 16 codegen units. Tagged releases
retain the Cargo.toml fat-LTO / one-codegen-unit profile. A same-runner x64
comparison measured 230s versus 111s, with the server binary increasing from
29.8 MB to 37.0 MB; hardening and server identity checks passed. The experiment
is [run 37250408869](https://github.com/open-horizon-labs/unified-hifi-control/actions/runs/37250408869).
NAS state keys include the profile overrides, so the two configurations keep
separate dependency snapshots. WASM keeps its existing LTO-disabled profile.

The Test job uses the fleet S3 compiler cache without full target snapshots:
its measured 2.2 GB archive cost more to transfer than the compilation it saved.
Only successful lint/WASM/native builds publish new dependency snapshots.

## Windows MSVC cross-builder

The explicit Prepare build environments workflow also prepares the Windows tools
image from windows-source.yml and windows-profile.json. It includes Rust's actual
project baseline, the Windows MSVC target, cargo-xwin 0.23.1 (checksum verified),
Clang/LLD/LLVM, CMake/Ninja and pre-cached Microsoft SDK/CRT. The SDK is part of
immutable image contents, separate from per-job NAS Cargo state. Microsoft SDK
use is subject to its license (linked in cargo-xwin upstream documentation).

The Linux cross job produces all three executables with the existing version,
assets, NAA features and tag-dependent optimization profile. A blocking native
Windows job verifies executable loading, helper entrypoints, HTTP/version/SHA and
embedded assets, then applies the existing optional signing and publishes the
unchanged binary-windows artifact. Signing/publishing never bypass native checks.

Generate a reviewable image and runner recipe with the homelab converter:

```sh
python3 ../homelab-infra/runner/builder/convert.py .github/workflows/build.yml \
  --setup-source build/prepared-builders/windows-source.yml \
  --config build/prepared-builders/windows-profile.json --output /tmp/windows-builder
```

Build/publish tools once, promote the generated runner digest to the repository-
scoped broker policy and pull it on both fleet hosts before enabling the workflow.
The hosted Ubuntu fallback uses the GHCR tools image and job-token authentication.
No compiler or SDK download is required in the per-build job.
