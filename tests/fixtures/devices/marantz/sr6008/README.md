# Marantz SR6008 hardware captures

Captured 2026-10-12 from the two SR6008 units on the maintainer's network, from a Mac on the same
subnet, for #776. **Read-only**: only HTTP GETs of status documents and Telnet state *queries*
(`PW?`, `ZM?`, `MV?`, `MU?`, `SI?`) were sent. No command that changes device state was sent.

Evidence label: **hardware-verified (read path only)**. The write path (volume, mute, power, input)
is implementation-derived until a write capture is added here.

| Serial (= MAC, = UDN suffix) | Friendly name | State at capture | Telnet :23 |
|---|---|---|---|
| `0006781E0AB8` | `front` | main zone ON, input `MPLAY` (renamed "WiiM"), -39.5 dB, display Absolute, MVMAX 70 | accepted |
| `0006781E2AB9` | `Bedroom` | STANDBY, -59.0 dB | **refused** (connection refused while in standby) |

## Files per unit

- `description.xml` — UPnP description from the SSDP `LOCATION` (`http://<ip>:8080/description.xml`).
  `serialNumber` and the UDN suffix both carry the unit's MAC; the two units differ only there.
- `deviceinfo.xml` — `GET /goform/Deviceinfo.xml` (port 80). Contains raw non-UTF-8 bytes in
  `BrandCode`/`ProductCategory`; read it as bytes.
- `main_zone.xml` — `GET /goform/formMainZone_MainZoneXml.xml`.
- `main_zone_status.xml` — `GET /goform/formMainZone_MainZoneXmlStatus.xml`: power, `MasterVolume`
  (dB), mute, current input as an `SI` code, `VolumeDisplay`, and the input catalog
  (`InputFuncList` / `RenameSource` / `SourceDelete`).
- `telnet_state_queries.{tx,rx}` (front only) — exact bytes sent and received.

## Facts these captures establish

1. **Identity:** `serialNumber` is unique per unit and equals the MAC (`00:06:78` = D&M Holdings).
2. **Volume encoding:** Telnet `MV405` with `MasterVolume=-39.5` ⇒ `MV = dB + 80`, third digit `5`
   is the half step. `MVMAX 70` is the unit's own limit (-10 dB). The XML always carries dB; the
   front-panel "Absolute" display is a convention only.
3. **Two protocols interleave on :23:** every Telnet reply is followed by an `@`-prefixed line
   (`@VOL:-395`, `@SRC:33`). Parsers must ignore them.
4. **Standby:** HTTP answers in standby (network standby is on) but Telnet refuses connections, and
   most `RenameSource` entries are blank. Power state must be read over HTTP in standby, power-on
   must be sent over HTTP, and the input catalog must be re-read when the unit wakes.
5. **`MVMAX` is Telnet-only:** a unit reached only over HTTP has no known maximum, so UHC refuses
   every volume write to it.
