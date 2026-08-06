# Direct streaming adapters

This document records the provider boundary for the direct streaming-adapter
initiative.  It is intentionally conservative: a provider is not advertised
as supported until its authorization and playback path is available to UHC.

## Amazon Music: access-gated discovery

Amazon publishes a Web API for catalog metadata, search, user libraries, and
playlist operations.  The official documentation currently labels the Web API
as **closed beta** and **preview**.  The playback overview also describes
playback capabilities, but that documentation does not establish that UHC can
obtain production credentials or use the API under terms suitable for a
general-purpose open-source adapter.

Official references:

- [Web API overview](https://developer.amazon.com/docs/music/API_web_overview.html)
- [Web API search](https://developer.amazon.com/docs/music/API_web_search.html)
- [Web API playlists](https://developer.amazon.com/docs/music/API_web_playlist.html)
- [Playback overview](https://developer.amazon.com/docs/music/API_playback_overview.html)
- [Developer program overview](https://developer.amazon.com/docs/music/get_started_program-overview.html)

Until Amazon confirms program enrollment, production endpoint access, OAuth
scopes, supported playback-device semantics, and compatible terms, UHC must
not add an `amazonmusic:` adapter.  In particular, no private API, token
extraction, DRM bypass, or browser automation is an acceptable substitute for
the official access path.

The discovery acceptance signal is one of:

1. Amazon confirms a supported implementation path, credentials, and device
   model; or
2. The work is explicitly deferred with the external access blocker recorded.

## Music Assistant

Music Assistant is an optional peer adapter for users who already run an MA
server.  It is not a prerequisite for direct Apple Music, Spotify, or Amazon
Music support.  MA-backed zones retain the `musicassistant:` adapter scope and
MA remains the authority for those players and queues.

The adapter uses MA's documented authenticated JSON API (`POST /api`) with a
long-lived access token supplied by the user.  Player discovery and transport
control are the initial capabilities; search, queue, and playlist operations
remain separate follow-on capabilities so that MA's own library and queue
semantics are not silently projected onto native provider adapters.  See the
[MA API documentation](https://www.music-assistant.io/api/) for the token and
command contract.

## QNAP x86_64

The QNAP x86_64 package uses the same static
`x86_64-unknown-linux-musl` server binary as the Linux x64 artifact.  A QNAP
host can run direct cloud adapters such as Spotify when their credentials and
device APIs are available.  Apple Music's native MusicKit companion remains a
macOS deployment concern; a QNAP package does not claim to provide MusicKit
playback in-process.
