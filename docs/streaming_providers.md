# Streaming provider support matrix

UHC's job is to control the listener's intended playback **device**.  A
provider having a catalogue API, or even an SDK that can play audio in a new
application, does not by itself make it a UHC adapter candidate.  It must be
possible to control an existing provider/device playback path without UHC
receiving, decrypting, relaying, mixing, or rendering the audio.

This document records the current evidence for direct streaming-service work.
It does not change the capability report: a provider becomes supported there
only when a tested adapter and routed MCP path exist.

## Decision matrix

| Provider | Direct device control | UHC disposition | Why |
|---|---|---|---|
| Spotify | Yes, through Spotify Connect | **Direct adapter** | The Web API discovers Connect devices and can control a selected device. OAuth and Spotify Premium are required for playback control. |
| Apple Music | Only on the device running MusicKit | **Direct adapter with an iPhone companion (Wave 1)** | Apple has no cloud API for remote control of arbitrary Apple Music sessions. A signed iPhone companion owns authorization and the `SystemMusicPlayer` session. A native Mac companion and truthful AirPlay destination model are separate Wave 2 validation work. |
| TIDAL | No public general-purpose remote-device control | **Device/protocol adapter, or obtain written vendor approval** | The public API supplies metadata and the Player SDK supplies playback, but playback must remain in TIDAL's official Player module. TIDAL Connect integrations are device-partner only. UHC must not become a TIDAL player or bypass that SDK. |
| Qobuz | No public self-service remote-device control | **Partner-gated** | Qobuz has a third-party API programme, but its integration material directs prospective integrations to Qobuz. Existing device/server integrations remain controllable through their own UHC adapters. |
| Amazon Music | Not established | **Access-gated discovery** | Amazon documents search, library, playlist, and playback APIs, but labels the Web Playback API closed beta and preview. Confirm production access and device semantics before an adapter is proposed. |
| Pandora | No public remote-device control | **Partner-gated source, not a device controller** | Pandora's GraphQL API can search and drive playback for a supplied device UUID, but expects the integrator to run the playback client and report progress. It is not a remote controller for the listener's existing Pandora devices. |
| SoundCloud | No remote-device control | **Optional source only** | Its public API can search and stream playable SoundCloud tracks to an application-controlled player. This is useful only if UHC intentionally owns a playback target. |
| YouTube Music | No supported third-party music playback/control API | **Not a direct adapter candidate** | Do not treat unofficial clients, token extraction, or scraping as an integration path. |
| Deezer | No supported public playback path for new third-party applications | **Not a direct adapter candidate** | Metadata access is not enough for the UHC outcome, and no supported device-control path has been identified. |

## What "direct" means

### Direct device-controller adapters

These preserve UHC's architecture: the adapter discovers or is configured with
real playback zones, publishes their state to the event bus, and sends commands
to the provider/device that owns audio playback.

- `spotify:` can represent Spotify Connect devices.
- `applemusic:` represents a paired native companion's MusicKit session. Wave
  1 is an iPhone `SystemMusicPlayer` companion; Wave 2 separately validates a
  Mac execution owner and AirPlay destination observations. A route or speaker
  is never published as a second Apple Music zone merely because it is where
  the companion sends audio.

The Apple companion is a control bridge, not an audio proxy.  Apple credentials
and authorization remain on the companion; audio never travels through UHC.

### Device and server integrations remain first-class

A source that cannot be controlled directly is not necessarily unavailable to a
listener.  A good playback device may already expose a UHC-supported control
surface while it plays TIDAL, Qobuz, or another integrated service:

- Roon and LMS own their corresponding server/player sessions.
- OpenHome and UPnP expose renderer transport and volume where the device
  provides them.
- Music Assistant is an optional peer adapter for an existing MA installation;
  it is never required for native Apple Music, Spotify, or Amazon work.

The zone stays scoped to its actual owner (`roon:`, `lms:`, `openhome:`,
`upnp:`, or `musicassistant:`).  UHC must not relabel a device-backed zone as
the streaming source merely because that source is playing there.

## Non-negotiable guardrails

- Never use private APIs, scraping, token extraction, DRM bypass, or browser
  automation as a substitute for a provider's supported path.
- Never make UHC an audio relay, cache, mixer, or renderer solely to reach a
  source service.  That changes the product from device control into a playback
  client and often violates provider terms.
- Treat provider terms as capability constraints.  TIDAL's current guidelines,
  for example, require playback through its unmodified Player SDK and prohibit
  AI/machine-intelligence uses without written approval.  Do not expose TIDAL
  playback through UHC's MCP/AI path without that approval.
- Keep authentication, availability, device restrictions, and provider-specific
  actions explicit in the adapter's state and capability report.
- A new OAuth callback or bridge HTTP endpoint is an API contract change.  It
  needs a specified contract and the user-applied `api-change-approved` label
  before implementation.

## Primary references

- [Spotify Connect device discovery](https://developer.spotify.com/documentation/web-api/reference/get-a-users-available-devices)
- [Spotify playback control](https://developer.spotify.com/documentation/web-api/reference/start-a-users-playback)
- [Apple Music API authentication](https://developer.apple.com/documentation/applemusicapi/user-authentication-for-musickit)
- [TIDAL developer platform](https://developer.tidal.com/documentation/overview)
- [TIDAL developer guidelines](https://developer.tidal.com/documentation/guidelines/guidelines-developer-guidelines)
- [TIDAL Connect status](https://developer.tidal.com/documentation/connect)
- [Qobuz API terms](https://static.qobuz.com/apps/api/QobuzAPI-TermsofUse.pdf)
- [Qobuz third-party integration guidelines](https://static.qobuz.com/apps/api/Qobuz-AppsGuidelines-V1.0.pdf)
- [Amazon Music playback API status](https://developer.amazon.com/docs/music/API_playback_overview.html)
- [Pandora developer API features](https://developer.pandora.com/docs/overview/support-and-api-features/)
- [SoundCloud API and streaming](https://developers.soundcloud.com/docs/api/)
