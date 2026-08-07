# Apple Music iPhone companion

This iOS 17+ Swift Package is the native execution-owner side of UHC's
`applemusic:` adapter. A signed host app embeds it, requests the documented
MusicKit authorization, and pairs that app instance with UHC.

`SystemMusicPlayer` is used because the v1 product goal is to control the
Apple Music session the listener is using on the iPhone. The package wraps
the documented transport calls (`play`, `pause`, next, and previous), provides
the typed bridge client (pair/claim, snapshot publication, command polling,
acknowledgement, and revoke), exposes the MusicKit authorization request, and
contains native catalog/library search, playlist/recent/recommendation
retrieval, exact-play and queue primitives, and can project a bounded
current-player snapshot.

The iPhone command path synthesizes `toggle` from the documented
`SystemMusicPlayer` playback status (`pause` while playing, otherwise `play`).
Commands whose semantics are not exposed by the validated system-player
surface—stop, volume, mute, and repeat/shuffle—remain explicit refusals rather
than silently mapping to a different operation.
The content/queue methods are companion-local building blocks: they are not
yet exposed through UHC's bridge transport, and their queue, route, artwork,
volume, and now-playing behavior remains unclaimed until demonstrated on a
physical device (#465). Their Apple identifiers must become owner-scoped
opaque references before any result crosses the bridge (#463).

The package also exposes narrowly scoped native mutations for confirmed
playlist creation and appending an exact song. It deliberately does not offer
generic playlist deletion/reorder/removal; those operations need a separate
approved content contract and ownership/conflict checks (#484).

The host app owns authorization, lifecycle, snapshot publication, and the
bridge transport. `KeychainAppleMusicCompanionInstallationStore` persists the
stable installation identity and paired bridge bearer; Apple credentials and
tokens stay in the app's secure storage. UHC receives only the explicitly
paired snapshot/command contract.
UHC never receives or relays audio.

`AppleMusicCompanionHost` provides the host lifecycle: request authorization,
claim a pairing code, publish a bounded snapshot, poll/execute/acknowledge
commands, and revoke. The snapshot deliberately leaves volume and route
unknown; the exact observation surface still requires the physical-device
acceptance work in #465.

Build this package from an iOS/Xcode host. Linux, QNAP, and other non-iOS
builds do not compile it.
