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

## Authorization and the Apple Music bridge

The embedded UI is same-origin. Cross-origin browser access is disabled by
default because this server also exposes playback, OAuth, and companion
authority endpoints. If a separately hosted UI is intentionally deployed, set
`UHC_ALLOWED_ORIGINS` to an exact comma-separated origin allowlist (for
example `https://uhc.example.test`). Do not use `*` for an installation that
is reachable through a tunnel; put authentication and access control at the
tunnel/reverse-proxy boundary until the controller-auth contract in #463 is
implemented.

The first authorization contract is now implemented in UHC. Spotify uses the
standard authorization-code flow:

- `GET /api/providers/spotify/oauth/start` creates a single-use, ten-minute
  state and returns the provider authorization URL.
- Spotify redirects to `GET /api/providers/spotify/oauth/callback`; UHC
  exchanges the code server-side and stores the access/refresh token in the
  Spotify adapter and encrypted credential store. Tokens are never returned
  by the endpoint.
- `POST /api/providers/spotify/oauth/revoke` clears the adapter and removes
  the durable credential file.

The credential envelope uses an operator-managed 32-byte key. Set
`UHC_CREDENTIAL_KEY` (hex or base64url) or `UHC_CREDENTIAL_KEY_FILE`; when
neither is set, UHC creates `credential.key` beside the encrypted credential
file with owner-only permissions. `UHC_SPOTIFY_CREDENTIAL_FILE` can point at a
secret-backed volume when the default config directory is not appropriate.
The key and encrypted file must be backed up together if a restart or package
upgrade is expected to preserve the connection.

For a hosted UHC deployment, set `SPOTIFY_CLIENT_ID`,
`SPOTIFY_CLIENT_SECRET`, and optionally
`SPOTIFY_REDIRECT_URI` (and `SPOTIFY_TOKEN_URL` for a local test server) before
starting the flow. The callback endpoint must be registered exactly with the
Spotify application.

For a local or QNAP install, open Settings in the browser you will use for
authorization, enter the Spotify client ID, and leave the client secret blank
to use Authorization Code with PKCE. If the browser is not on the UHC host,
first start a temporary HTTPS tunnel to port 8088 (for example with
`cloudflared` or Tailscale Funnel), open its HTTPS URL in the browser, and
register the exact callback shown by Settings in the Spotify developer
dashboard. Spotify accepts plain HTTP only for explicit loopback callbacks
(`127.0.0.1` or `::1`); a remote QNAP needs HTTPS. Saving the form persists the
client configuration in the encrypted credential envelope, after which
**Connect Spotify** starts the browser authorization flow. Stop the tunnel
after authorization; reauthorization creates a new callback URL. This is the
first-run onboarding path tracked in #469.

Apple Music authorization remains native to the companion. The v1 execution
owner is a signed iPhone app using `SystemMusicPlayer`; the iOS package and its
deliberately narrow transport wrapper live in `companion/apple_music_ios`.
The existing macOS package is deferred until #486 validates a supported Mac
session on physical hardware. A future iPhone or Mac companion can pair
through the same bridge contract:

- `POST /api/bridges/applemusic/pair` creates a five-minute one-time pairing
  code for a companion id.
- `POST /api/bridges/applemusic/claim` exchanges that code for a bearer token.
- The companion publishes `POST /api/bridges/applemusic/state`, polls
  `GET /api/bridges/applemusic/commands`, and acknowledges commands with
  `POST /api/bridges/applemusic/commands/{command_id}`.
- `GET /api/bridges/applemusic/status` reports pairing, snapshot, and
  thirty-second liveness; `POST /api/bridges/applemusic/revoke` invalidates a
  companion token.

Internally, the bridge registry keeps owner-scoped liveness distinct: an owner
can be `unpaired`, `awaiting_snapshot`, `reachable`, or `stale`. The existing
HTTP status remains backward-compatible while adapter/aggregator work can use
the richer classification; a paired token alone is not evidence that playback
is controllable.

Pending OAuth states and bridge tokens are intentionally in-memory in this
first slice, so a restart invalidates those short-lived values. Spotify's
provider token is persisted through the encrypted credential boundary above;
the companion owns MusicKit authorization and UHC receives no Apple token or
audio. Deploy these endpoints behind HTTPS and a trusted identity boundary.

The companion owns the playback session; an AirPlay route is only an output
destination observed by that owner. Routes are not duplicate UHC zones and do
not prove that UHC can select or command the destination. HomePod and Apple TV
remain destination contexts until a supported host/control path is separately
validated under #487.

## QNAP x86_64

The QNAP x86_64 package uses the same static
`x86_64-unknown-linux-musl` server binary as the Linux x64 artifact.  A QNAP
host can run direct cloud adapters such as Spotify when their credentials and
device APIs are available.  Apple Music's native MusicKit companion remains a
macOS deployment concern; a QNAP package does not claim to provide MusicKit
playback in-process. Fresh packages create a private `config` directory and
the service defaults `UHC_CONFIG_DIR` to it, so the encrypted Spotify
credential file and its key survive process restarts and package upgrades.
Operators may override that variable to mount a secret-backed config volume;
the package does not remove credentials during an upgrade or uninstall.

The local release path is reproducible with:

```sh
cargo zigbuild --release --target x86_64-unknown-linux-musl
make qnap-test
```

The resulting server binary is a static PIE ELF suitable for the QNAP x86_64
package. The package contains no Swift/MusicKit runtime; Apple Music remains a
paired native-companion capability.
