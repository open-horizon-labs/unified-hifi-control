---
target: Spotify page and Settings Spotify provider section
total_score: 23
max_score: 40
na_heuristics:
p0_count: 0
p1_count: 3
timestamp: 2026-08-06T20-46-54Z
slug: src-app-pages-spotify-rs
---
Method: dual-agent (A: /root/spotify_ui_critique · B: /root/spotify_ui_evidence)

## Design Health Score

| # | Heuristic | Score | Key issue |
|---|-----------|---:|---|
| 1 | Visibility of System Status | 1/4 | Next worked while metadata stayed stale; Connected status lacked a reliable visual treatment. |
| 2 | Match System / Real World | 3/4 | Spotify Connect language and receiver distinction were clear. |
| 3 | User Control and Freedom | 3/4 | Playback and disconnect controls were present, but duplicate actions were not acknowledged as pending. |
| 4 | Consistency and Standards | 2/4 | Spotify refreshed unlike the selective event behavior used by Zones. |
| 5 | Error Prevention | 2/4 | Repeated Next/Previous presses could race while the previous request was unresolved. |
| 6 | Recognition Rather Than Recall | 2/4 | Configured accounts still saw client setup and tunnel instructions. |
| 7 | Flexibility and Efficiency | 3/4 | Dedicated Spotify navigation and device controls worked, but refresh cost was unnecessarily broad. |
| 8 | Aesthetic and Minimalist Design | 2/4 | A lone Spotify card was constrained to half the Settings width and nested setup columns became narrow. |
| 9 | Error Recovery | 2/4 | Manual refresh was needed to recover current metadata. |
| 10 | Help and Documentation | 3/4 | Setup and tunnel guidance was concrete, though the two-panel framing was misleading after setup. |

**Total: 23/40**

## Design Specificity Verdict

The Spotify page is authored for UHC: it explains that UHC controls existing Spotify Connect devices and is not a receiver, uses the shared zone-card language, and exposes the provider as its own route. The main weakness was operational rather than visual: the page treated a metadata event like a full inventory refresh, so the interaction felt disconnected from the system state.

The required markup detector returned zero findings for the Spotify, Settings, volume, and input CSS targets. A supplemental CSS scan flagged the incumbent Inter font as overused; that is an existing system-level choice, not a Spotify-specific regression.

## What's Working

- Desktop Spotify uses a clear three-column device grid and mobile stacks cleanly with usable tap targets.
- Spotify Connect copy correctly sets the receiver boundary and explains the remote setup path.
- Album art, track title, artist, transport controls, and volume are grouped in a compact device card.

## Priority Issues

- **[P1] Metadata lag after Next/Previous.** The page fetched every device sequentially after any event, so unrelated devices delayed the changed one. Fetch only the event's zone, refresh immediately after successful adapter commands, add a bounded eventual-consistency retry, and show an Updating playback state.
- **[P1] Configured Settings still showed setup.** Connected users saw client ID, secret, redirect URI, and tunnel instructions. Show a compact configured/server-side-token status instead, with an explicit Edit client settings disclosure and account identity retained.
- **[P1] Settings layout was too narrow.** A single provider occupied only half the content width and nested setup columns became cramped. Use full width for one provider and two columns only when Spotify and Apple Music are both enabled.
- **[P2] Album art could remain stale.** The stable proxy URL had no image-key cache bust. Append the track image key.
- **[P2] Status and controls needed stronger affordances.** Add a visible success badge, pending action feedback, volume aria labels, and hide opaque device IDs behind disclosure.

## Persona Red Flags

- **Alex (Power User):** A Next press could succeed while the UI appeared unchanged, encouraging repeated commands and race conditions. Broad refreshes also made the page do unnecessary work.
- **Jordan (First-Timer):** A connected account still presented client setup and tunnel instructions, making the user question whether authorization had actually worked.
- **Morgan (Remote self-hosted user):** Long tunnel prose and opaque device IDs consumed the narrow Settings layout and created avoidable horizontal overflow on mobile.

## Minor Observations

- Empty-state copy now needs to emphasize automatic discovery while retaining a manual fallback.
- Light-theme success text needed stronger contrast for normal-size text.

## Questions to Consider

- Should provider setup be a one-time disclosure with a separate “Edit client settings” mode for all providers?
- Should the Spotify status strip eventually include last-refresh time, or is live/pending state enough?
- Should device IDs have a copy affordance in addition to disclosure?
