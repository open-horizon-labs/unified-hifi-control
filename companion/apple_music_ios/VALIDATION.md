# iPhone SystemMusicPlayer validation matrix (#465)

This checklist is evidence-gathering, not a claim that the repository has
passed these tests. Run it from a signed iOS 17+ host using a stable Keychain
companion ID and an eligible Apple Music account. Record the iOS version,
device model, subscription state, app build, UHC commit, and timestamp with
each result.

| Case | Procedure | Record | Capability consequence |
|---|---|---|---|
| Authorization accepted | Install, request MusicKit authorization, claim the UHC code | status, account/subscription state, redacted error state | Enables companion polling only |
| Authorization declined/revoked | Decline, then revoke in Settings and relaunch | exact status and next-action UX | Must classify `authorization_needed`; no stale “connected” |
| No subscription/restricted item | Use a non-member or restricted account/content | provider error and whether playback remains unchanged | Must classify restriction/subscription failure |
| Inactive player | No current playback; publish snapshots | state, current-entry absence, freshness | Reachable-but-inactive, not offline |
| Play/pause/next/previous | Execute each command from UHC and from Music | acknowledgement, observed state, current entry | Promote only individually verified operations |
| Current item metadata | Play catalog and library songs, including artwork | title/artist/album/artwork/position fidelity | Promote metadata fields only when observed |
| Queue replacement | Replace with 2–3 exact songs; manually alter Music queue | current entry, ordering visibility, divergence | Never claim full queue unless observable |
| Play Next | Insert one exact song after current | insertion result and observed transition | Promote `play_next` independently |
| Repeat/shuffle | Toggle each in Music and through host if exposed | state readback and command result | Promote only if both read/write are proven |
| Volume/mute | Test built-in output and an AirPlay route separately | whether MusicKit exposes/control succeeds | Keep unknown and capability 🚧 when unavailable |
| Suspend/resume | Background for 1, 5, and 15 minutes, then foreground | last publish, stale interval, recovery time | Verify stale/reachable transitions |
| Wi-Fi/cellular switch | Switch networks during polling and command | request errors, recovery, duplicate commands | No command replay beyond dedupe contract |
| Pairing recovery | Expire code, reuse code, revoke, re-pair same ID | classified responses and zone identity | Verify owner binding and lost-device recovery |

## Evidence rules

- Capture UHC logs with tokens, Apple IDs, and account identifiers redacted.
- A command acknowledgement is not proof of playback; pair it with a later
  observed snapshot/current-entry change.
- Do not promote a capability based on a single successful play. Repeat the
  normal path after relaunch and after a network transition.
- Keep AirPlay destination observations separate from the iPhone execution
  owner. A route never creates a second UHC zone.
- After the matrix is complete, update the generated capability matrix and
  issue #465 with device-backed evidence. Until then, Apple transport/skip/
  volume remain 🚧 #465.
