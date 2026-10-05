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

Fleet Linux jobs restore/save Cargo target state and registry sources directly
from NAS MinIO using `scripts/fleet-build-cache.py`. The key covers the job/target,
Cargo.lock, pinned toolchain, and target flags. Jobs keep independent working
directories; S3 publishes snapshots atomically. This avoids uploading large target
archives through GitHub. Hosted jobs retain Swatinem's cache. NAS failures are
visible but do not block builds. Source changes reuse dependency state; Cargo
fingerprints and the existing main-crate clean still force application rebuilds.
Linux compile timings and sccache statistics are uploaded/reported in each run.

Regenerate with the maintained converter, not by editing Dockerfiles:

```sh
python runner/builder/convert.py /path/to/uhc/.github/workflows/build.yml \
  --setup-source /path/to/uhc/build/prepared-builders/source.yml \
  --config /path/to/uhc/build/prepared-builders/profile.json \
  --output /tmp/uhc-builder
```

The Mac jobs continue using their existing persistent Tart guest and Actions
compiler cache. They are not Linux containers.
