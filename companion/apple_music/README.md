# Apple Music macOS companion

This macOS 14+ Swift Package is a native execution owner for UHC's
`applemusic:` adapter. It is not the default v1 setup path: the primary
companion is the iOS package in
`companion/apple_music_ios`, using `SystemMusicPlayer` on a physical iPhone.
Mac support is tracked separately in #486 and needs signed-app, lifecycle, and
physical-device validation before it can be enabled.

The implementation wraps an app-private `ApplicationMusicPlayer` session.
That is not the same thing as controlling the user's Music.app session, so
this package does not claim Music.app control or AirPlay control.

The Rust boundary is in `src/adapters/apple_music.rs`:

- `MusicKitRequest::Snapshot` asks for a `MusicKitSnapshot`.
- `MusicKitRequest::Command` carries playback/volume/mute commands.
- `MusicKitResponse::Error` is a classified bridge failure.

The package intentionally does not invent a token endpoint or put Apple
credentials in the Rust server. The embedding app supplies its developer-token
and Music User Token policy, requests MusicKit authorization, and projects the
authorized player's metadata into the Rust `MusicKitSnapshot` shape.

`Companion.swift` includes the safe, SDK-backed transport operations (`play`,
`pause`, `skipToNextItem`, and `skipToPreviousItem`) and a bounded snapshot
projection for its own app-private session. The projection deliberately leaves
volume and route unknown; it does not claim Music.app or AirPlay control.
`Bridge.swift` supplies the existing pairing, snapshot publication, command
polling/acknowledgement, owner-scoped content polling/acknowledgement,
revoke, and bounded command/content deduplication lifecycle for a signed Mac
host. `InstallationStore.swift` persists the bridge bearer,
stable companion ID, bridge ID, and UHC URL in Keychain; inject
`InMemoryAppleMusicCompanionInstallationStore` for previews/tests.
Signed-host lifecycle, Keychain identity, and physical validation remain
required by #486/#487. Linux, QNAP, and other non-macOS builds do not compile
this package.

## Building a distributable DMG (Apple Silicon only)

`build-dmg.sh` builds the Xcode app
(`XcodeMac/AppleMusicCompanionMac.xcworkspace`, scheme
`AppleMusicCompanionMac`) for **arm64 only** — no x86_64 or universal build,
by explicit project decision — and wraps it in a `.dmg` with `hdiutil`. It
verifies the built executable is arm64-only before packaging. Run it via:

```sh
make companion-apple-music-dmg
# or directly, from this directory:
VERSION=0.1.0 ./build-dmg.sh
```

The DMG is written to `companion/apple_music/dist/` (gitignored). This
mirrors the `build-applemusic-companion-dmg` CI job in
`.github/workflows/build.yml`; see `docs/gh-release.md` for how it's wired
into label-gated PR builds and releases.

By default the app is built **unsigned** (ad-hoc, `codesign --sign -`),
since CI has no code-signing identity available. Set `CODE_SIGN_IDENTITY`
(and `DEVELOPMENT_TEAM`) to build with a local Developer ID or Apple
Development identity instead.

### Unsigned app: first-launch workaround

Because the shipped DMG is unsigned, macOS Gatekeeper quarantines the app on
download and blocks a normal double-click launch. After copying
`AppleMusicCompanionMac.app` to `/Applications`, either:

- Right-click (Control-click) the app and choose **Open**, then confirm the
  dialog, or
- Remove the quarantine attribute from Terminal:
  ```sh
  xattr -dr com.apple.quarantine "/Applications/AppleMusicCompanionMac.app"
  ```

Notarization is tracked as a follow-up (not a blocker for distributing this
DMG) — see issue #535.
