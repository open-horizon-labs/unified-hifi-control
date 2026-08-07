# Apple Music iPhone companion

This iOS 17+ Swift Package is the native execution-owner side of UHC's
`applemusic:` adapter. A signed host app embeds it, requests the documented
MusicKit authorization, and pairs that app instance with UHC.

`SystemMusicPlayer` is used because the v1 product goal is to control the
Apple Music session the listener is using on the iPhone. The package wraps
the documented transport calls (`play`, `pause`, next, and previous), provides
the typed bridge client (pair/claim, snapshot publication, command polling,
acknowledgement, and revoke), exposes the MusicKit authorization request, and
contains native catalog/library search plus exact-play and queue primitives.
The content/queue methods are companion-local building blocks: they are not
yet exposed through UHC's bridge transport, and their queue, route, artwork,
volume, and now-playing behavior remains unclaimed until demonstrated on a
physical device (#465).

The host app owns authorization, lifecycle, snapshot projection, and the
bridge transport. Apple credentials and tokens stay in the app's secure
storage; UHC receives only the explicitly paired snapshot/command contract.
UHC never receives or relays audio.

`AppleMusicCompanionHost` provides the host lifecycle: request authorization,
claim a pairing code, publish validated snapshots, poll/execute/acknowledge
commands, and revoke. The host supplies the snapshot projection and maps each
`MusicKitWireCommand` to the validated `SystemMusicPlayer` operation. This is
deliberate: the exact observation surface is the physical-device acceptance
work in #465, not an assumption made by the package.

Build this package from an iOS/Xcode host. Linux, QNAP, and other non-iOS
builds do not compile it.
