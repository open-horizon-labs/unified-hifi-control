> **STATUS: DO NOT MERGE.** Working prototype, verified on real hardware
> (fēnix 8 51mm AMOLED) driving a live Unified Hi-Fi Control server. It is
> parked on its own branch deliberately:
>
> - The Connect IQ Store upload **burns its appID** — publishing needs a
>   fresh UUID in `manifest.xml` (see below).
> - It depends on a **publicly reachable HTTPS endpoint**. Connect IQ refuses
>   plain HTTP, so there is no LAN fallback, and remote access is an open
>   product question rather than a settled one.
> - Nothing in the main repo depends on it, and it adds no server code.

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

## Why there is no custom-drawn control screen

Garmin's music playback screen — icons down the right edge, the highlighted
one named on the left — cannot be had by an app like this, and three
attempts to imitate it all looked wrong for the same reason.

That screen belongs to the **native media player**. An app reaches it by
being an `audio-content-provider-app`, which means the app supplies songs
for the WATCH to play. Garmin's own sample for it,
`garmin/connectiq-apps/audio-provider/monkeymusic`, draws no playback
controls at all — the system player renders them. A remote for a hi-fi in
another room has no audio on the watch, so it cannot be one.

Two further checks point the same way: the personality library's icon set is
about / check / cancel / discard / question / revert / save / search /
warning — **no transport icons** — and `WatchUi.ActionMenu`, the nearest
system widget, explicitly does not support iconography.

So the interface is built from system widgets instead: `Menu2` for the zone
list and for the per-zone actions. Garmin draws them, which means the
device's own typography, spacing, highlight, scrolling and back behaviour,
and full operation from UP / DOWN / START / BACK with no touchscreen
required. That is what "native" can actually mean here.

Volume is two ordinary rows rather than a picker or a mode: resting on
"Volume up" and pressing START repeatedly changes the level without the
selection moving and without needing to read the screen, which is the
eyes-free gesture the app exists for.

## HTTPS is mandatory — measured, not assumed

The Connect IQ runtime refuses plain HTTP. Verified in the simulator against
this app: every request returns

```
responseCode = -1001
```

which the SDK documents as `SECURE_CONNECTION_REQUIRED` — *"Indicates an https
connection is required for the request."* This held for a LAN address **and
for `127.0.0.1`**; there is no localhost exemption in SDK 9.2.0.

**The dangerous part:** the request still reaches the server. A local fixture
server logged the `POST /control` bodies arriving and answering `200`, while
the app reported failure. So over HTTP a command *executes* and the watch
tells you it did not — the worst possible split. This is a reason to fail
closed on a bad configuration rather than retry.

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

## Distribution, and the appID trap

Sideloading over USB is a dead end on macOS for this watch: the fēnix 8 Pro
presents as MTP (091e:51b8), macOS will not mount it, and libmtp cannot open
a session. Switching the watch to "Garmin" USB mode makes it worse — it
enumerates as 091e:0003, Garmin's proprietary protocol, which is not mass
storage either.

The working route is the Connect IQ Store's test upload: publish the `.iq`
as a test app and install it to your OWN watch over Bluetooth through
Garmin Connect. Only the developer can install a test build, which is
exactly the case here.

**The trap**, in Garmin's own words on the upload page:

> Only you will be able to download and test the app. If you want to publish
> your app after testing, you will need to upload it again and use another
> appID in the app's manifest.xml.

So a test upload BURNS its appID. The id currently in `manifest.xml` is a
hand-written placeholder and is fine to spend on testing. Before any public
release, mint a fresh UUID for `<iq:application id="...">` — and keep the
signing key (`developer_key.der`), because a published app can only be
updated by the key that first signed it.

Build the test package with:

```
monkeyc -f monkey.jungle -o uhc-hifi-control.iq -y <developer_key.der> -e -r
```

## Building

Requires the Connect IQ SDK (via the SDK Manager) and a JRE.

```
monkeyc -f monkey.jungle -d fenix8pro47mm -o bin/uhc.prg -y <developer_key.der> -l 2
```

`-l 2` turns on informative type checking; it has already caught real bugs
here and should stay on.
