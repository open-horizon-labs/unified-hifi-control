# Prepared build environments

`source.yml` retains the reviewed Actions tool setup extracted from the previous
WASM/Linux jobs. `rust-toolchain.toml` supplies the Rust release. Installer
commands and tool versions live in these Actions steps, not in a handwritten
Dockerfile. `profile.json` selects jobs/setup steps and image destinations.

Run the **Prepare build environments** workflow explicitly after updating setup.
It regenerates the tools/runner recipes and workflow variants with the pinned
homelab converter, publishes the hosted tools image to GHCR, and uploads the
prepared-builder artifact. One publisher builds the fleet derivative and pushes
it to the NAS-backed registry; both fleet members pull/cache that image.

Apply the generated fleet workflow after its images and repository-scoped broker
policy are ready. Keep hosted tools public for forks. Verify the manifest and
image digests and run the full Build workflow afterward. The preparation workflow
uses `--setup-source` so it can regenerate already converted jobs.
