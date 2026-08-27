# Unified Hi-Fi Control — Garmin Connect IQ remote

A wrist remote for UHC zones: pick a zone, play/pause, skip, change volume.

## Why it is shaped this way

**It adds nothing to the server.** UHC already exposes a controller API built
for hardware knobs, and a watch is that same class of client:

| | |
|---|---|
| `GET /zones` | ~3.2 KB for ten zones: id, name, source, state, volume |
| `POST /control` | `{zone_id, action}` — `play_pause`, `next`, `previous`, `volume_up`, `volume_down` |

Both were verified against `v3` and against a running 3.7.0-alpha build, so
this app drives an unmodified server.

**It fetches on open and after actions, never on a timer.** The device has
768 KB of app memory, so a 3.2 KB response is trivially affordable — the
reason to avoid polling is Bluetooth latency and watch battery.

**The address is a setting, not a discovery.** Connect IQ has no mDNS and no
way to ask the phone to resolve one, and the settings screen in Garmin
Connect is a declarative form, not code. So the base URL is typed once. Give
the server a stable name (a DHCP reservation, or a public hostname) so the
setting does not rot.

## Networking constraints that shaped this

- Requests are made **by the phone**, not the watch; the watch has no
  independent route to your LAN (its own WiFi is not usable for Connect IQ).
- Garmin restricts plain HTTP to private addresses, so the server needs
  **HTTPS with a valid certificate**.
- **Album art is different**: `makeImageRequest` is fetched and transcoded by
  *Garmin's servers*, so an art URL must be reachable from the public
  internet — a valid certificate on a private address is not enough.

## Target

`fenix8pro47mm`, which the SDK reports as covering *fēnix 8 Pro 47mm / 51mm /
MicroLED / quatix 8 Pro* — one target, 454×454 round AMOLED, touch plus five
buttons, Connect IQ 6.0.2, 768 KB watch-app memory.

Widening to other devices is a layout pass per device family, not a line in
the manifest: a non-touch or lower-resolution watch needs its own design.

## Building

Requires the Connect IQ SDK (via the SDK Manager) and a JRE.

```
monkeyc -f monkey.jungle -d fenix8pro47mm -o bin/uhc.prg -y <developer_key.der> -l 2
```

`-l 2` turns on informative type checking; it has already caught real bugs
here and should stay on.
