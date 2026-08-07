# Mac companion and AirPlay validation matrix (#486/#487)

This checklist applies to the signed macOS 14+ `ApplicationMusicPlayer`
companion. It does not test or imply Music.app automation. Record macOS
version, Mac model, host build, UHC commit, and timestamp.

| Case | Procedure | Record | Capability consequence |
|---|---|---|---|
| Authorization | Authorize the signed host and claim a UHC code | status and redacted failure | Mac owner becomes eligible, not automatically supported |
| App-private session | Play while Music.app is independently playing | which session changes | Confirms no Music.app control claim |
| Lifecycle | Hide, background, terminate, relaunch | snapshot cadence and stale/recovery timing | Defines reachability semantics |
| Transport | Play/pause/next/previous through UHC | acknowledgement plus observed state | Promote only individually proven actions |
| Metadata | Catalog/library playback and current-entry projection | metadata/artwork fidelity | Record unavailable fields honestly |
| Built-in output | Play to the Mac's local output | output observation | Does not prove route control |
| AirPlay/HomePod/Apple TV | Select route outside UHC, then change/loss/recovery | route visibility, owner state, stale behavior | Route remains destination-only unless documented API proves more |
| Companion recovery | Revoke, re-pair, network loss, restart | owner identity and stale classification | Verify no duplicate zones or stale route leakage |

No capability is promoted from this matrix without physical evidence. The Mac
package intentionally publishes no route and no volume claim today.
