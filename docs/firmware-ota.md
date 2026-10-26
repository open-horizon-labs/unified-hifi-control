# Local controller OTA

UHC serves controller application firmware over the same local HTTP connection used for playback. USB browser flashing is hosted at [firmware.hiphi.audio](https://firmware.hiphi.audio); UHC does not need HTTPS or host a browser installer.

## Approved request contract

`GET /firmware/version` and `GET /firmware/download` accept optional `device_type` and `channel` query parameters. Device identity can also be supplied through `X-Device-Type`. When both query and header identify hardware they must agree after alias normalization. Unknown or conflicting targets are rejected; missing artifacts never fall back to another device or channel.

Examples:

```text
http://bridge:8088/firmware/version?device_type=frame
http://bridge:8088/firmware/download?device_type=frame
http://bridge:8088/firmware/version?device_type=m5dial&channel=alpha
http://bridge:8088/firmware/download?device_type=m5dial&channel=alpha
```

Use the same hardware and channel for the version check and download. The version payload retains `version`, `size` and `file`; download remains an application/octet-stream application image. Stable is the default; beta and alpha require explicit selection. Availability depends on a published matching release asset. Support in the server catalog does not imply that every family currently has stable firmware.

Requests with no hardware selector or header retain the legacy `knob` target (`roon_knob.bin`). Modern `dial` is a distinct release artifact (`hiphi_dial.bin`). Accepted canonical target values are `knob`, `dial`, `frame`, `halo`, `tough`, `joy`, `m5dial`, `rlcd`, `stackchan`, `sticks3` and `stopwatch`. Published `hiphi-*` identities are normalized through the shared catalog, including historical M5 beta names; `hiphi-dial-beta` means M5 Dial, not the primary Dial.

Regular controller requests with `X-Device-Type` persist canonical device identity. `GET /knob/devices` includes `device_type` in each existing device record. Older stored records default to `knob`; an omitted or invalid identity header does not erase previously known hardware. Identity discovery does not rewrite the user's configuration.

`GET /manifest-s3.json` remains unavailable. No `image` selector or merged-image download is added to UHC.

## Installing from the Controllers page

Every controller type in the catalog, HALO included, can be updated over the air, provided its firmware carries the OTA partition layout (a one-time browser reflash that keeps settings when erase is declined; see the firmware repository's OTA guide).

The Controllers page asks `/firmware/version` for each controller's own type and channel and shows **Update to X** only when that image is newer than the version the controller reported. Pressing it sends `{"firmware_update": "install"}` to `POST /knob/config` (or `PUT /config/{knob_id}`). The Bridge refuses with `409` unless a newer image exists on the controller's channel, and `400` for an unknown action.

A pending request travels to the controller inside the existing config payload (`GET /config/{knob_id}`):

```json
{ "config": { "firmware_update": { "action": "install", "from_version": "2.8.0-alpha.4", "requested_at": "…" } } }
```

The key exists only while a request is pending and is part of `config_sha`, so the controller refetches its config, sees it, and runs a forced check followed by the install. It ignores a request whose `from_version` is not the version it is running. The Bridge clears the request when the controller reports a different version, or after 30 minutes without one, so a failed install is not retried on every reboot.

Rollback needs the bootloader's rollback setting, which only a USB reflash can change; a controller updated only over the air keeps whatever bootloader it already had. The firmware confirms a new image after 45 seconds of uptime.

## Device settings

Beyond the shared display and power settings, a controller can have settings of its own. The table in `src/firmware/settings.rs` is the single source of truth: the server rejects a setting a controller does not have (`400`, nothing stored) and the Controllers page renders only the controls its firmware applies.

| Controller | Setting | Config key | Values |
|---|---|---|---|
| Frame | Show IP address | `show_ip` | bool |
| Dial, HALO | Brightness | `brightness` | 5–100 (%) |
| Kizz | Body language, Sounds | `body_enabled`, `sound_enabled` | bool |
| Kizz | Sound volume | `sound_volume` | 0 Low, 1 Medium, 2 High |

A key is omitted from the config payload until the owner sets it, so a controller without these settings serializes and hashes exactly as before and an old firmware simply ignores them. Frame and HALO store the shared power fields but do not apply them, so the page hides those; Slate applies only the album-art timeout.

Wi-Fi networks and passwords, the Hi-Fi Control address, Bluetooth pairing and factory reset are deliberately not exposed through the Bridge. Wi-Fi passwords must not travel over an unauthenticated LAN API, and the bridge address cannot safely be changed by the bridge itself. They remain on the controller's own web page (port 80, shown as a link from the Controllers page).

## Validation boundary

Caches separate hardware and channels. The legacy stable cache retains its existing directory. Images are published immutably before metadata switches; unsuccessful fetches preserve the prior bundle. Version and download are separate HTTP requests, so an update between them can change the advertised version. This contract does not add a version-pinning selector.

Firmware from the v4 beta line selects its own channel: it sends `channel=` derived from its running version (an alpha build asks for `alpha`, a beta build for `beta`, a release for `stable`), so a prerelease is only offered newer builds of the same channel. Older Dial firmware sends no channel and keeps getting Stable; the Bridge never silently moves it to a prerelease.

Physical OTA needs validation with the exact firmware build and controller. Some firmware families use different OTA implementations, and Dial's current firmware performs its own image identity checks. Server tests cannot establish successful flashing, boot recovery, or power-loss safety on hardware.
