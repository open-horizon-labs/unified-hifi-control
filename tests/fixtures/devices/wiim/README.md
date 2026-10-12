# WiiM hardware captures

Captured 2026-10-12 from six WiiM units on the maintainer's network (five models), read-only
`GET /httpapi.asp?command=getStatusEx|getPlayerStatus`. Network fields (SSID, BSSID, router MAC,
IP/Wi-Fi diagnostics, clock) are redacted.

| File prefix | Model (`project`) | Firmware |
|---|---|---|
| `pro` | WiiM_Pro_with_gc4a | Linkplay.4.8.827634 |
| `pro-nomfi` | WiiM_Pro_No_MFI | Linkplay.4.8.713134 |
| `pro-plus` | WiiM_Pro_Plus | Linkplay.4.8.827634 |
| `amp-pro` | WiiM_Amp_Pro | Linkplay.5.2.830031 |
| `ultra` | WiiM_Ultra | Linkplay.5.2.829661 |

Facts established:

- `uuid` is unique per unit (two Pro Plus units differ) and is the hardware id.
- `max_volume` (the device's own limit setting) is reported; the adapter uses it as the maximum.
- `volume_control` is `0` on every unit — including one at volume 100 feeding an amplifier — so it
  does not reveal fixed output; fixed-output detection remains unavailable over this API.
- `getPlayerStatus` `mode` `-1` means no active source (no current input).

Live write path (`tests/wiim_live.rs`, through `DeviceService`) on the WiiM Ultra: volume set
confirmed in 0.18 s; no-op write confirmed (1.7 s, no hang); restore 2.1 s; mute on/off 1.9 s each;
power refused (no LinkPlay power command). Unit restored to its starting state.
