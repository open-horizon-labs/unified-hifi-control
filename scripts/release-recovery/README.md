# Alpha8 release recovery

This directory documents the one-off recovery for `v4.0.0-alpha.8`. The
recovery branch is intentionally never merged into `v4`; it preserves the
reproducible assembly procedure while generated artifacts remain outside Git.

The release source is exact commit `6db91eb56f207ae4c3a1452fd52c1f1187b17c42`
(tag `v4.0.0-alpha.8`). The original release workflow run was `33965185308`.
Its GitHub-hosted macOS jobs remained queued, so macOS artifacts were produced
by fallback run `33966821484` on the trusted self-hosted macOS runner.

The validation workspace layout is:

- `artifacts/`: read-only downloads from run `33965185308`
- `fallback33966821484/`: macOS universal and companion artifacts from the fallback run
- `release/`: generated final assets, never committed

Download the artifacts with `gh run download` into the workspace above, then
run:

```sh
ALPHA8_RECOVERY_ROOT=/srv/agent-data/work/unified-hifi-control/alpha8-recovery \
  python3 scripts/release-recovery/assemble_alpha8_release.py
```

`ALPHA8_RECOVERY_ROOT` is configurable for another validated workspace.
`ALPHA8_MAC_ARTIFACT_ROOT` can point to a different fallback artifact tree.
The script validates the exact source tag, five bridge/pair payloads, the
macOS universal slices, the LMS archive preservation rules, the alpha8
`install.xml`, the known Linux x86_64 SHA256, and the 17-asset final inventory.
It generates the external LMS beta feed and SHA256SUMS, but performs no GitHub
upload or release mutation.
