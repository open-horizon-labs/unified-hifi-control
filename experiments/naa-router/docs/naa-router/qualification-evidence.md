# Qualification evidence checked on 2026-09-16

This is a review of saved September 14 evidence, not a new live test.
The implementation was the standalone Rust `native/naa-router` controller and
its bundled `web/index.html` selector, not the later UHC HQPlayer page.
The run's `binary.json` identifies `naa-router-af3f14ca20ac`, SHA-256
`af3f14ca20ac870ec735e00b4822087e58b0f0dce7ce85fb650921d8bfa54c0c`.

Saved observations:

- `integrated-one-click.json`: A → B → A completed in 5.333 and 5.376 seconds.
  Both final controller projections report `position_restored: true`, native
  transport state `2`, and a started, forwarding downstream session with audio.
- `integrated-browser-switch.json`: the standalone selector's recorded browser
  switch ends on route B, with `position_restored: true`, native state `2`, and
  1,806,336 forwarded audio bytes. The controller saved position 38.533 seconds;
  subsequent native Status reads 41.733 seconds on track 1.
- `browser/report.json`: both recording sinks completed without fixture errors;
  A accepted 3,650,304 bytes and B accepted 16,068,864 bytes. Each accepted payload
  exactly matched its simulated rendered payload.

These records support the later integrated-controller qualification summary.
They do not establish physical listening, a DAC output measurement, seamless
handover, or qualification of the current UHC build. The earlier cutoff and
blocked-audit entries describe the period before renewed authorization and
must not be read as the final result.

Private evidence stays outside git. Content hashes below identify the records
reviewed without publishing native captures or household configuration:

| Record | SHA-256 |
|---|---|
| `binary.json` | `607630feacfbd204714adfac8a2f2bffd5e4e429ac749974c4de8caa7e0e9088` |
| `integrated-one-click.json` | `03bb696a286aff0bacd3fadacc2d37552b851c6ba9c782cf5729144b4e6cd6cc` |
| `integrated-browser-switch.json` | `4412e7de0051f3ab5659227544b60e557fbe670530556c85f4e0eea6e3b96aa7` |
| `report.json` | `b2c518499cd09f71e0dc20e34409ca7fc428c82a0608fae8c638fec0faddd56a` |
