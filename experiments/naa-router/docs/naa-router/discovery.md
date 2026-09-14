# Destination discovery — execution and review

## Aim and pre-flight

Select a named NAA without entering an address, while preserving the stable
HQPlayer-facing device and existing authenticated DAC enumeration. Selected
approach: native UDP multicast discovery, saved routes, and manual fallback.
The scanner uses an explicitly configured interface and never opens an NAA TCP
connection. Success means names appear, selection reuses the existing route path,
and scanning cannot change playback. Cross-network multicast reachability is an
accepted limitation; automatic format changes and standalone authentication are
outside this change. Pivot if discovering hosts requires authentication or scanning
interferes with an active session.

## Delivered

`--discovery-interface` now also enables the querying side. Opening the picker
queries both existing NAA multicast groups on UDP 43210 using an independent,
interface-bound ephemeral socket. The two-second scan is bounded to 256 unique
addresses and 4096-byte replies. Concurrent scans are rejected. No results are
persisted implicitly. The UI automatically scans on opening, offers Refresh,
expires results after 30 seconds, and preserves saved routes on empty/failed scans.

Selecting a result saves its name/address and enters the existing route selection
flow. Existing saved routes are reused by address and port. If several saved DAC
routes share an endpoint, the UI directs the user to those explicit choices.
Discovery names use text nodes. The scan does not authenticate or enumerate DACs;
that remains part of the selected HQPlayer-to-NAA relayed session.

HTTP: GET `/api/discovery` reports whether enabled; POST `/api/discover` with JSON
returns the latest scan. Both retain the existing Host/origin protections. The
configured discovery responder port does not change the destination query port:
standard destination discovery always queries 43210.

## Risk retirement and review

| Risk | Disposition | Tempting shortcut rejected | Evidence |
|---|---|---|---|
| Self-routing/loops | Retired by evidence | List every reply | Parser tests exclude our listener and HiPhi Router announcements, including other router instances. |
| Duplicate names | Retired by evidence | Key by display name | UDP tests retain separate same-name addresses and deduplicate repeated replies; browser showed separate same-name fixture choices. |
| Stale availability | Retired by evidence | Accumulate scan results forever | Empty subsequent UDP collection returns no cached entries; browser expiry removed discovered entries and retained the saved selected route. |
| Scan changes playback | Retired by evidence | Reuse NAA/auth connections to discover | Independent socket/API fixture scan during streaming preserved generation, route, increasing audio bytes and exactly one auth exchange. |
| Malformed replies/name injection | Retired by evidence | Trust arbitrary XML or HTML | Parser rejects requests, declarations, oversized replies/names; browser displayed `Fixture <NAA>` literally. |
| Discovery requires auth | Retired by evidence for hosts | Connect to every NAA to enumerate | Real LAN query returned `audiolinuxrpi4` and `cm4nano`, protocol 6, without TCP/authentication. DAC enumeration continues through the selected relayed session. |
| Multicast blocked between networks | Accepted with rationale | Delete saved routes after an empty scan | Explicit-interface LAN scope; manual destinations remain available. Network topology beyond the tested LAN is not qualified. |

Review: aligned; the scanner has no Router reference and cannot call transport or
route mutation methods. Selection remains an explicit UI action. Tests use software
fixtures for audio and browser selection; no physical DAC playback was performed.
The existing live Embedded qualification belongs to the earlier frozen artifact,
not a new full live audio qualification of this discovery build.

## Verification

- Cargo test: six unit tests, five additional Rust integration cases, and the
  black-box wrapper passed; the Python suite contains 64 cases (55 router and
  nine reference-fixture cases).
- Clippy with warnings denied passed; release build passed.
- Actual LAN discovery found two official NAA hosts by name and protocol.
- Browser verified automatic discovery, distinct duplicate-name rows, saving and
  selecting a loopback fixture, and expiry preserving the saved destination.
- The discovery release manifest records source and binary hashes separately from
  the historical live-qualified build in `software-gate.json`.

Temporary discovery/browser test processes were stopped. HQPlayer configuration
and playback were not changed during this task. Physical DAC behavior and other
network topologies remain human/environment verification boundaries.

The scoped commit attempt was cancelled after signing did not complete within
20 seconds. Signing was not bypassed. Only this task’s paths were unstaged;
concurrent metadata staging was preserved. The source and frozen release remain
available locally.

## Read-through DAC follow-up

Aim: choose physical DACs from information already passing through the proxy.
The observation cache is keyed by configured host and port, kept in memory, and
bounded to 256 endpoints. Successful getdevices replies replace the full list
before virtual-device rewriting. The existing session-ID guard rejects late
observations from superseded workers. The browser exposes timestamped cached
DACs on saved and discovered endpoints and saves/reuses the exact selected ID.

Risk retirement: an independent fixture test learned two DACs, removed one,
verified replacement rather than accumulation, switched to another endpoint,
and confirmed both endpoint records survived Stop with one auth exchange each.
The browser showed both physical names, selected USB DAC, and API readback
confirmed exact device ID `usb:two`. No standalone auth or background DAC queries
were introduced. Cached availability is explicitly last-seen; host aliases are
not assumed equivalent. Cache persistence across process restart is outside this
increment. Physical attachment/playback is not inferred from a cached reply.

The extended release passes 65 Python checks (56 router, nine reference-fixture),
11 Rust checks and Clippy. Full Embedded audio qualification remains associated
with the historical frozen build; this increment uses independent fixture and
browser evidence. See `dac-gate.json` for the source/build snapshot.
