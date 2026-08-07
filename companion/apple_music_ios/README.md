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
The content/queue methods are exposed through UHC's approved bridge transport.
Their queue, route, artwork, volume, and now-playing behavior remains subject
to physical-device acceptance (#465). Apple identifiers stay inside the
companion; results cross the bridge only as owner-scoped opaque references.
SystemMusicPlayer exposes `currentEntry` and supports replacing/inserting queue
items, but does not expose a readable entries collection. Accordingly,
`queue_read` returns an explicit `unsupported/queue_visibility_unavailable`
outcome rather than inventing provider queue state; UHC's listening plan remains
separate and truthful.

The package also exposes narrowly scoped native mutations for confirmed
playlist creation, metadata editing, and appending an exact song. It
deliberately does not offer generic playlist deletion/reorder/removal or
unfavorite operations; those remain refused until Apple documents a safe
operation and UHC has an ownership/conflict model (#484).

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
