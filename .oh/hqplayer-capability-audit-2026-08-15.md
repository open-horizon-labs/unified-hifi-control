# HQPlayer live-control audit — 2026-08-15

## Scope

This audit covers the user-approved surface only:

1. native configuration-profile inventory, optional active getter, and verified changer;
2. all immediate DSP getters and selectors;
3. PCM/SDM re-enumeration and readback;
4. adapter → aggregator → web UI/MCP consistency;
5. existing matrix-profile list and switch.

Restart-backed configuration editing, user-owned presets, system/hardware tuning, and matrix CRUD or
pipeline editing are explicitly excluded.

The reference is HQPTuner 1.7.0 at commit `f996561`. Live comparisons used installed UHC 3.5.1 at
`192.168.1.2:8088`, HQPTuner at `217.myqnapcloud.com:8090`, and HQPlayer Embedded 6.0.4 at
`192.168.1.61:4321`.

## Verdict

The installed 3.5.1 build is wrong and must not be promoted as the fix. The worktree now fixes the
identified code paths, but it has not been deployed to the live host yet.

| Capability | Installed 3.5.1 | Current worktree | Evidence |
|---|---|---|---|
| Named native profile list | Correct | Correct | Same four names; unnamed base omitted |
| Optional active native profile | Missing from aggregator/UI/MCP | Implemented | Native `ConfigurationGet`; empty becomes `None` / `(no preset)` |
| Verified native profile changer | Implemented | Implemented, canonical result required | Live `Zen` load followed by direct native `ConfigurationGet value="Zen"`; hermetic restart/readback tests cover failure paths |
| Immediate selectors | Present | Present | Mode, rate, 1x/Nx filter, dither/modulator, junk, matrix, convolution, adaptive, repeat, random, volume |
| PCM/SDM re-enumeration | Correct inventories | Correct inventories | Exact live comparison in both chains |
| Live mode/rate display | Wrong | Fixed | Installed says PCM while configured SDM; stopped worktree suppresses stale mode/rate |
| Mutation projection | Can return bare `{"ok":true}` | Fixed | Canonical aggregator payload or explicit failure |
| Matrix unnamed default | Blank | `[Default]` | Empty native name remains distinct from saved `Default` |
| Selector UX | Opaque raw names/Hz | Improved | Friendly rates/mode, native filter guidance, search, rate-gated modulators |

## Live HQPTuner cross-check

### Exact inventory and selected-value parity

The comparison used both applications' live APIs while pointed at the same daemon. Values below are
semantic names or exact Hz, never native list indexes.

| Chain | Modes | Filters | Dither/modulators | Rates | Selected values |
|---|---:|---:|---:|---:|---|
| SDM | exact order match | 77, exact order match | 36, exact order match | 6, exact order match | mode, 1x, Nx, modulator, and rate all matched |
| PCM | exact order match | 67, exact order match | 10, exact order match | 13, exact order match | mode, 1x, Nx, dither, and rate all matched |

The initial and restored SDM/DSD256 selections were:

```text
mode      SDM (DSD)
filter1x  poly-sinc-gauss-long
filterNx  poly-sinc-gauss-hires-lp
modulator ASDM7EC-fast
rate      11289600
```

The targeted PCM pass changed every dependent selector. Both applications independently reported:

```text
mode      PCM
filter1x  poly-sinc-gauss-short
filterNx  poly-sinc-gauss-hires-mp
dither    TPDF
rate      705600
```

The targeted SDM pass then exercised DSD128 with `poly-sinc-gauss-medium`,
`poly-sinc-gauss-hires-ip`, and `ASDM7EC-fast`, followed by DSD256 with
`poly-sinc-gauss-long`, `poly-sinc-gauss-hires-lp`, and both `DSD7 256+fs` and
`ASDM7EC-fast`. No rate above DSD256 was selected. The audit restored the exact original Zen
SDM/DSD256 mode, rate, filters, modulator, volume, matrix, and advanced-control state.

The eight junk-filter names and ordering also matched exactly.

### Matrix identity

The daemon reports an empty current matrix name. HQPTuner renders that as `[Default]`, followed by
saved profiles `Default` and `Mch-to-Stereo mixdown`. Installed UHC renders no selection.

The worktree now applies the same user-facing rule:

- `[Default]` serializes to the daemon's empty `MatrixSetProfile.value`;
- saved `Default` remains the literal non-empty name `Default`;
- an unknown non-empty current name is retained instead of being mislabeled as `[Default]`.

### Native configuration profiles

The daemon's named inventory is `Cen.Grand`, `GaN`, `Zen`, and `Zen1`. The audit loaded `Zen`
through UHC and then queried the daemon's TCP control protocol independently; it returned
`ConfigurationGet result="OK" value="Zen"`. An empty value is HQPlayer's unnamed base
configuration, not a profile choice.

The worktree therefore:

- never puts the unnamed base in the profile list;
- preserves a genuinely saved native profile named `Default`; only the exact `[default]` base
  sentinel (or an empty value) is omitted;
- exposes the active name as `Option<String>` in the Rust client and aggregator;
- renders no active name as `(no preset)`;
- adds the optional active name to MCP status;
- marks exactly the matching named profile active after a list or load refresh.

HQPTuner's header presets are its own snapshot store in 1.7.0, not a trustworthy implementation
source for native profile loading. Its `/api/config.active` deliberately reports that store rather
than native `ConfigurationGet`; the source says its restore-only workflow leaves the daemon on
`[default]`. Its neutral `(no preset)` presentation is still the correct UX reference for an absent
named native selection, but direct protocol readback is the authority for UHC's getter/changer.

### Live immediate controls

The audit also exercised and independently read back the non-selector controls:

- junk filter `20k` and restore to `none`;
- random and adaptive volume on/off, restored off;
- convolution-on rejection when the daemon did not apply it, with convolution left off;
- repeat-one refusal/readback on an empty playlist, with repeat left off;
- HQPlayer digital volume from -3 dB to -4 dB and back to -3 dB.

The Zen's external endpoint may be fixed-volume, but this daemon advertises an enabled -60..0 dB
digital range and both UHC and HQPTuner observed the -4 dB write. UHC must follow the native
`VolumeRange`, not infer fixed volume from the downstream endpoint.

## Defects found and fixed

### 1. Wrong live PCM/SDM authority

Installed UHC derives the displayed active mode from `State.active_mode`. On this live 6.0.4 daemon,
configured SDM reported a stale PCM state index while `Status.active_mode` correctly reported
`SDM (DSD)`.

The worktree now uses:

- `State.mode` for the configured selector;
- `Status.active_mode` for an explicit running chain;
- `Status.active_rate` to derive the loaded family under source-following mode;
- `Status.active_filter`, `active_shaper`, and `active_rate` for live readouts;
- no live mode/output claim while stopped or paused.

MCP now carries live `pipeline.mode` separately from configured `options.mode.current`.

### 2. Active profile was verification-only state

The adapter already queried `ConfigurationGet` to verify a profile load but discarded it everywhere
else. It is now a public optional Rust getter and part of the coherent native observation retained
by the aggregator and projected to UI/MCP.

The first implementation placed this reconnect-capable query after the setting-list coherence
boundary. The full suite caught that it could mix a new session's profile identity with an old
session's lists. It now runs before the boundary; a reconnect invalidates and refills the complete
chain-relative set.

### 3. Filter metadata was discarded

HQPlayer's live `FiltersItem.description` contains compact selector guidance such as quality, focus,
and rate-ratio suitability. HQPTuner preserves it; UHC parsed only index/name/value/arg.

The Rust client now retains the optional daemon-authored description. UI filter labels and search
use it while the raw semantic filter name remains the wire value. Unknown future filters still
render and remain selectable.

### 4. Rate and modulator UX was needlessly opaque

The worktree presents exact values with familiar tiers, for example:

```text
352800   → 8× · 352.8 kHz
5644800  → DSD128 · 5.6448 MHz
11289600 → DSD256 · 11.2896 MHz
```

`[source]` is labeled `Auto (follow source)`, PCM calls the shaper `Dither`, and SDM calls it
`Modulator`. Filter and shaper controls are searchable. Rate-specific `256+fs`, `512+fs`, and AHM
modulators remain visible but are disabled below their HQPTuner-validated minimum rate with an
explanation.

### 5. Verified writes could still lie to callers

The reliable dispatch path already performed the native write, same-session verification, and
canonical publication. The HTTP handler then performed a redundant second full native refresh. If
that extra read failed, it returned legacy `{"ok":true}` instead of the state the reliable path had
already published.

The live exhaustive run made the bug concrete:

```text
391 selector/control rows exercised across PCM and SDM
103 returned canonical projected state
288 filter rows returned HTTP 200 without projected settings
exact original configuration restored
```

Those 288 rows are exactly the two 1x/Nx filter passes across the 67-option PCM and 77-option SDM
inventories. The worktree removes the redundant refresh and returns the already-published aggregator
snapshot. HTTP and MCP now fail explicitly if canonical readback is unavailable; neither can claim a
successful mutation without data.

### 6. Matrix default identity was collapsed

The UI now always offers `[Default]` after a successful matrix inventory read, even when there are
no saved named profiles. The same empty-name normalization is used by the modern HTTP control path,
Rust client, and MCP.

## Immediate-control inventory

| Setting | Read authority | Semantic write | Web | MCP | Verification |
|---|---|---|---|---|---|
| Mode | State + modes | `SetMode` | Yes | Yes | Re-enumerate + readback |
| Active mode | Status/rate | N/A | Yes | Yes | Running-only projection |
| Rate | State + rates | `SetRate` | Yes | Yes | Fresh list + readback |
| Filter 1x/Nx | State + filters | paired `SetFilter` | Yes | Yes | Sibling preserved + readback |
| Dither/modulator | State + shapers | `SetShaping` | Yes | Yes | Fresh list + readback |
| Junk filter | State + list | `SetJunkFilter` | Yes | Yes | Readback |
| Matrix profile | State + list | `MatrixSetProfile` | Yes | Yes | Empty/named semantic readback |
| Convolution | State | `SetConvolution` | Yes | Yes | Readback |
| Adaptive volume | State | `SetAdaptiveVolume` | Yes | Yes | Readback |
| Repeat | State | `SetRepeat` | Yes | Yes | Readback |
| Random | State | `SetRandom` | Yes | Yes | Readback |
| Volume | State + range | absolute/relative | Yes | General MCP | Exact dB readback |

`State.invert` is readable but no supported immediate native setter was established in either UHC
or HQPTuner's live lane, so it is not counted as a missing selector.

## Qualification boundary

Hermetic tests cover active profile identity, profile-load verification, all immediate setter
readbacks, source/PCM/SDM chain changes, stale-index rejection, matrix empty-name semantics,
stopped-state projection, canonical mutation responses, UI filtering/rate guidance, and MCP shape.

The live daemon audit now covers named profile load and direct native readback, semantic PCM and SDM
selector changes, exact inventories, representative advanced controls, volume, and restoration. The
patched worktree is not installed on `192.168.1.2`, so its corrected rendered UI and MCP projection
still require one packaged-beta pass; this is a deployment qualification boundary, not an untested
native-client path.

Playback did not sustain during this pass: native `Play` initially reported an empty transport, and
the Roon `HQPlayer dsp` source later supplied an HTTP URL that returned 404. DSP writes and readbacks
were therefore qualified while stopped; transport/source failure was kept separate and is not
misreported as a successful playback test.

Separate dormant PCM/SDM user intent and user-owned live presets are HQPTuner product features. They
are not native daemon state and remain outside this explicitly narrowed effort.
