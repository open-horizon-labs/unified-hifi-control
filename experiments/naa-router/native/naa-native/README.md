# Private native NAA adapter

`naa-android` is an Android/Bionic Rust executable registered as the optional
`naa-native` endpoint in private `enableHqplayer` builds. It owns public IPv4
discovery/TCP, relays a fresh opaque authentication exchange through official
NAA on a private loopback port, then closes that connection and implements
all subsequent enumeration, streaming, feedback and lifecycle locally.

The existing Java/C++ adapters retain route selection, exclusive grants and
device output. Dedicated generation-scoped v2 Unix-socket IPC carries exact
format requests, grant/refusal, opaque audio, accepted frames, actual rendered
positions, drain completion and abort. Queues are bounded; accepted or dequeued
audio never counts as rendered progress. `server.rs` implements this engine;
`pcm.rs` supplies checked format arithmetic and a queue-only helper whose
delivered count is explicitly not a device clock.

Offers come from the selected output. PCM rates have no 44.1/48 kHz ceiling.
Native DSD uses the same transport and existing output adapters. Its wire byte
count and DSD bit rate map to carrier frames; byte-interleaved stereo is regrouped
losslessly into the existing DSD_U32_BE layout. See
[the measured mapping](../../docs/naa-native/ce007-dsd-wire-mapping.md).
DoP negotiation and unequal PCM valid/container widths remain unmapped and are
not advertised by this adapter yet. Native DSD offers are preserved.
There is no independent PKI implementation, DSP or post-auth vendor fallback.
Vendor bytes and private captures must remain outside git.

`naa-replay` preserves the portable framing/replay projection for
[the sketch](../../docs/naa-native/sketch.md). Its lexical framer emits opaque
control lines; the active server separately uses structural XML parsing. The
crate depends on `libc` and `roxmltree`, with no async framework.

```sh
cargo test --manifest-path native/naa-native/Cargo.toml
python3 -m unittest tests/naa_native_server_test.py
python3 tools/naa_cess_gate.py
```

Build both Android ABIs using `build-android.sh` with `ANDROID_NDK_HOME` and an
external `UE_NAA_NATIVE_LIBS` directory. Packaging stages these only when HQPlayer
is enabled. `tools/naa_trace.py` creates bounded private captures with completeness
manifests; `tools/naa_observe.py` validates those before interpreting bytes.
`naa-replay` does not validate a manifest itself. Interrupted or truncated
captures cannot prove whole-stream equality. See [port evidence](../../docs/naa-native/pcm-port-results.md)
for recording fixtures versus physical hardware qualification.
