# Device family worker brief (#774, alpha)

You are implementing one or more **device family adapters** for Unified HiFi Control (Rust,
`/Users/muness1/src/open-horizon-labs/uhc-device-control`, branch `feat/issue-774-device-control`).
A coordinator integrates your work. Read this whole brief first.

## What a device is

A device is the amplifier/receiver/streamer a zone plays through. It is **not a zone**. Read
these before writing code — they are the contract:

- `src/devices/mod.rs` — `Device`, `DeviceId`, `DeviceVolume` (native scale!), `DeviceInput`,
  `DevicePower`, `ControlChannel`, `DeviceCapabilities`/`DeviceCapability`/`CapabilityEvidence`.
- `src/devices/service.rs` — `DeviceDriver`, `DeviceOperation`. The service resolves, clamps
  and quantises every level **in your device's native scale** before calling you.
- `src/adapters/devices.rs` — `DeviceFamily` (what you implement), `FamilyContext`.
- **Reference implementation: `src/adapters/denonavr/`** (`mod.rs` runtime, `protocol.rs` pure
  wire formats with unit tests). Copy its structure and safety habits.
- `tests/device_safety.rs` + `tests/mock_servers/marantz.rs` — how a family is tested end to end.
- Research: `docs/uhc-native-lan-client-catalog.md` / `.json` — your family's record (discovery,
  ports, protocol, boundaries, reference client). Use the reference client's source and vendor
  docs as the spec (fetch them with WebFetch; cite URLs in code comments). **No hardware exists
  for you** — every claim is `CapabilityEvidence::ImplementationDerived`.

## Your family's contract

1. **Module**: `src/adapters/<family>/mod.rs` (stub exists) + `protocol.rs` (pure functions:
   parse/encode, discovery-description parsing). A public struct `<Name>Adapter` with
   `pub fn new(bus: SharedBus) -> Self`, implementing `DeviceDriver` and `DeviceFamily`
   (`family()` returns the module name, e.g. `"wiim"`).
2. **Identity**: `DeviceId::new(kind, hardware_id)`. Kinds already registered in
   `DEVICE_KINDS`: `wiim yamaha bluos cambridge hegel lyngdorf onkyo sonos anthem arcam trinnov`.
   Hardware id = serial/MAC/UDN-derived; **never** model, name or IP.
3. **Discovery**: per the catalog (SSDP via `ssdp-client`, mDNS via `mdns-sd`, or eISCP UDP).
   Also honour `FamilyContext.manual` (configured host/port/mac). Families with no discovery
   (Anthem, Trinnov) use manual devices only. Validate the device answers the control protocol
   before publishing it ("discovery is not capability detection").
4. **State**: one task per unit owning its single connection; publish
   `BusEvent::DeviceObserved { device: Box::new(..) }` on every readback with a **new, increasing
   `revision`** (copy `next_revision()` from denonavr) and `maturity: DEVICE_CONTROL_MATURITY`.
   Publish `BusEvent::DeviceRemoved` when a unit stops / adapter stops.
5. **Volume in native units**: set `DeviceVolume.scale` honestly (`VolumeScale::Decibel` for dB,
   `Percentage` for 0–100, `Linear` only for 0.0–1.0). `min`, `hardware_max`, `step` in that scale.
   `hardware_max = None` when the device does not report one (the user's ceiling then bounds it).
   Fixed-output devices: `value = None` and `capabilities.volume.supported = false`.
6. **Commands**: implement every `DeviceOperation` the protocol supports (`SetVolume`, `SetMute`,
   `PowerOn`, `PowerStandby`, `SelectInput`). After every write, **read state back** so the service
   sees a fresh revision. Unsupported → return `Err("…not supported by this device…")`.
   Never send a state-changing command on connect or reconnect — queries only.
7. **Capabilities**: `pub fn capabilities_for(model: &str, …) -> DeviceCapabilities`, all
   `ImplementationDerived`, `detail` naming the protocol path and any model/source caveat from
   the catalog's boundaries (e.g. WiiM: no power; Sonos: no power; Arcam: no IP wake on older
   models; StreamMagic: volume only on variable-output models).
8. **Inputs**: `DeviceInput { id: <stable protocol code>, name: <display>, available }`.

## Repo rules your code must pass (they are CI lints)

- Every `tokio::spawn`ed `loop` contains a `tokio::select!` with a cancellation branch.
- Never `let _ = tx.send(..)` — handle or log send results.
- No lock guard held across `.await`.
- No `.unwrap_or(0.0)` / `.unwrap_or(50.0)` on volume values.
- No `unwrap()`/`expect()`/`panic!` in non-test code (clippy denies them); tests may.
- `cargo fmt`; `cargo clippy --features server -- -D warnings` clean.

## Tests you must write

- `protocol.rs` unit tests from **spec examples** (cite the source of each example): parse,
  encode, discovery description, volume encode/decode at min/max/step, malformed input refused.
- `tests/<family>_adapter.rs`: an **in-file mock** of the device protocol on `127.0.0.1:0`
  (shaped from the spec; label it synthetic) and end-to-end tests through `DeviceService`
  exactly like `tests/device_safety.rs` (copy its `harness()`): device observed with correct
  identity and scale; a volume write is clamped by a ceiling and confirmed by readback; **no
  state-changing bytes on connect**; two units of the same model never cross-route; an
  unsupported operation errors rather than succeeding. Add a constructor or `attach_unit`-style
  method so tests can target a mock on ephemeral ports without discovery.

## Boundaries

- **Only create/edit files under** `src/adapters/<your family>/`, `tests/<your family>_adapter.rs`
  and `tests/fixtures/devices/<your family>/`. Do not edit any other file. If you need a change
  elsewhere, say so in your final report.
- Build economically (the disk is nearly full): always `CARGO_INCREMENTAL=0`; run only
  `cargo test --lib adapters::<family>` and `cargo test --test <family>_adapter`; never
  `cargo test --workspace`. Other workers share the target dir; waiting on a cargo lock is normal.
- Onkyo/eISCP: implement from the published ISCP/eISCP protocol documentation, **not** from
  `aioonkyo` source (GPL-3.0). Cite the spec you used.
- Do not commit, push, or touch git.

## Final report (your last message)

Files created; the public API (`<Name>Adapter`, `capabilities_for` signature); discovery method;
volume scale; operations supported/unsupported and why; test names and results; anything you
needed changed outside your directory; spec URLs used.
