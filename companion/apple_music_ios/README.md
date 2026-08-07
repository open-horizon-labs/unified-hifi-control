# Apple Music iPhone companion

This iOS 17+ Swift Package is the native execution-owner side of UHC's
`applemusic:` adapter. A signed host app embeds it, requests the documented
MusicKit authorization, and pairs that app instance with UHC.

`SystemMusicPlayer` is used because the v1 product goal is to control the
Apple Music session the listener is using on the iPhone. The package wraps
the documented transport calls (`play`, `pause`, next, and previous), provides
the typed bridge client (pair/claim, snapshot publication, command polling,
acknowledgement, and revoke), and exposes the MusicKit authorization request.
It does not claim queue, route, artwork, volume, or now-playing support until
those surfaces are demonstrated on a physical device (#465).

The host app owns authorization, lifecycle, snapshot projection, and the
bridge transport. Apple credentials and tokens stay in the app's secure
storage; UHC receives only the explicitly paired snapshot/command contract.
UHC never receives or relays audio.

Build this package from an iOS/Xcode host. Linux, QNAP, and other non-iOS
builds do not compile it.
