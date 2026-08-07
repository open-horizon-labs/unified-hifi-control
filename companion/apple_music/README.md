# Apple Music macOS companion (deferred)

This macOS 14+ Swift Package is a deferred execution-owner experiment for
UHC's `applemusic:` adapter. It must not be presented as the current Apple
Music setup path: the v1 companion is the iOS package in
`companion/apple_music_ios`, using `SystemMusicPlayer` on a physical iPhone.
Mac support is tracked separately in #486 and needs signed-app, lifecycle, and
physical-device validation before it can be enabled.

The current implementation wraps an app-private `ApplicationMusicPlayer`
session. That is not the same thing as controlling the user's Music.app
session, so this package does not claim Music.app control or AirPlay control.

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
Signed-host lifecycle, Keychain identity, and physical validation remain
required by #486/#487. Linux, QNAP, and other non-macOS builds do not compile
this package.
