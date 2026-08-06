# Apple Music MusicKit companion

This macOS 14+ Swift Package is the native side of UHC's `applemusic:`
adapter. It owns an `ApplicationMusicPlayer` session on macOS and must be embedded by a
signed host app (inline with UHC on macOS, or as a paired bridge when UHC runs
elsewhere).

The Rust boundary is in `src/adapters/apple_music.rs`:

- `MusicKitRequest::Snapshot` asks for a `MusicKitSnapshot`.
- `MusicKitRequest::Command` carries playback/volume/mute commands.
- `MusicKitResponse::Error` is a classified bridge failure.

The package intentionally does not invent a token endpoint or put Apple
credentials in the Rust server. The embedding app supplies its developer-token
and Music User Token policy, requests MusicKit authorization, and projects the
authorized player's metadata into the Rust `MusicKitSnapshot` shape.

`Companion.swift` includes the safe, SDK-backed transport operations (`play`,
`pause`, `skipToNextItem`, and `skipToPreviousItem`). Snapshot projection and
queue/library work remain host integration points until the signed macOS app
and entitlements are defined. Linux, QNAP, and other non-macOS builds do not
compile this package.
