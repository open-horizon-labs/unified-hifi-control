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

Requests with no hardware selector or header retain the legacy `knob` target (`roon_knob.bin`). Modern `dial` is a distinct release artifact (`hiphi_dial.bin`). Accepted canonical target values are `knob`, `dial`, `frame`, `tough`, `joy`, `m5dial`, `rlcd`, `stackchan`, `sticks3` and `stopwatch`. Published `hiphi-*` identities are normalized through the shared catalog, including historical M5 beta names; `hiphi-dial-beta` means M5 Dial, not the primary Dial.

Regular controller requests with `X-Device-Type` persist canonical device identity. `GET /knob/devices` includes `device_type` in each existing device record. Older stored records default to `knob`; an omitted or invalid identity header does not erase previously known hardware. Identity discovery does not rewrite the user's configuration.

`GET /manifest-s3.json` remains unavailable. No `image` selector or merged-image download is added to UHC.

## Validation boundary

Caches separate hardware and channels. The legacy stable cache retains its existing directory. Images are published immutably before metadata switches; unsuccessful fetches preserve the prior bundle. Version and download are separate HTTP requests, so an update between them can change the advertised version. This contract does not add a version-pinning selector.

The inspected Dial firmware sends `X-Device-Type: hiphi-dial` but does not select a channel. It therefore requests modern Dial stable firmware; with today’s legacy-only stable release, the expected result is unavailable (`404`). Serving alpha requires a firmware client that explicitly selects `channel=alpha`, or a future modern stable release. The Bridge does not silently move that client to alpha.

Physical OTA needs validation with the exact firmware build and controller. Some firmware families use different OTA implementations, and Dial's current firmware performs its own image identity checks. Server tests cannot establish successful flashing, boot recovery, or power-loss safety on hardware.
