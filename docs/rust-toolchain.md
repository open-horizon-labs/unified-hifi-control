# Reviewed Rust baseline and advisory lint

`rust-toolchain.toml` declares the exact Rust release used by contributors and blocking workflow builds. `.github/actions/setup-rust` reads that file, installs the requested targets and components, and selects the same toolchain for subsequent commands. The baseline is currently Rust 1.98.1, which passed the local v4 preparation checks. Compiler installation and the toolchain version are separate: updating the setup action does not advance the reviewed compiler.

The Build workflow's `Lint` job remains a blocking `cargo clippy -- -D warnings` check. The `Latest stable lint (advisory)` job explicitly invokes `cargo +stable clippy -- -D warnings`; its findings stay visible in its logs and job summary, but its failure does not block the reviewed baseline. Both paths build the embedded CSS before compilation. An advisory failure can also indicate a dependency or runner problem; inspect the actual error before calling it a new lint.

Repository maintainers own baseline upgrades. Open a GitHub issue for each actionable advisory finding, linking the failing run and preserving the warning text. Fix the problem without suppressing warnings, then propose an exact channel change in `rust-toolchain.toml` through a reviewed PR. Check formatting, blocking Clippy, the server tests, the WASM build, and the relevant platform/package build jobs before merging that upgrade. Keep the advisory job enabled so later stable releases remain observable.

The Docker builder image has its own explicitly versioned Rust distribution; changing that image is a separate container-build upgrade. This workflow baseline change does not silently replace its base image.

`tests/ci_workflow_contract.rs` checks that workflow setup uses the shared baseline, each lint path builds CSS first and rejects warnings, and the latest-stable path is advisory with an unconditional summary. Actions execution remains the final check for runner-specific behavior.
