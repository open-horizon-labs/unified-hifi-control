# Direct streaming adapters

This document records the provider boundary for the direct streaming-adapter
initiative.  It is intentionally conservative: a provider is not advertised
as supported until its authorization and playback path is available to UHC.

**Status: Alpha.** Every provider documented here that UHC currently ships —
Spotify, Apple Music, and Music Assistant — is labeled Alpha in the Settings
UI (`badge badge-secondary "Alpha"`). They are fully wired end to end
(settings → enable → working zone), not feature-gated or hidden, but their
authorization flows and capability coverage are new enough to expect rough
edges and occasional breaking changes between releases.

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
long-lived access token supplied by the user. It currently supports player
discovery; transport, skip, volume, mute, repeat, and shuffle; catalog search;
exact play and queue-add from MA-owned references; and a bounded active-queue
read. All queue-scoped operations resolve MA's active queue for the selected
player rather than assuming that a grouped player's queue ID equals its player
ID. Browse, saved playlists, favorites, queue mutation, and MA grouping remain
separate follow-on capabilities so MA-specific semantics are not silently
projected onto native provider adapters. The local MA wire fixture covers the
authenticated request envelope and ensures peer error bodies are never exposed
through UHC errors. See the [MA API documentation](https://www.music-assistant.io/api/)
for the token and command contract. HTTPS is required by default. For a deliberately trusted
local-development-only MA instance, set `MUSIC_ASSISTANT_INSECURE_HTTP=1`
alongside `MUSIC_ASSISTANT_TLS=false`; never use that override across an
untrusted LAN or tunnel because the bearer token is sent on the wire.

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
authorization. The Spotify card's "Client settings" pane now walks through the
whole flow as numbered steps: create/open an app in the Spotify Developer
Dashboard, copy the exact callback URL shown there (with a one-click copy
button) into the app's Redirect URIs, paste the Client ID (and Client Secret,
if not using PKCE) into the fields below, save, then Connect. Leave the client
secret blank to use Authorization Code with PKCE.

If the browser is not on the UHC host, Spotify accepts plain HTTP only for
explicit loopback callbacks (`127.0.0.1` or `::1`); anything else — a remote
QNAP, a different machine on the LAN — needs HTTPS. The "Using UHC from
another device or a NAS?" panel below the callback URL has a **Get an HTTPS
address** button for this: it asks UHC's server to open a temporary tunnel to
itself and shows the resulting `https://…` callback URL with a copy button,
so a first-time user never has to install or run anything. The Redirect URI
field is pre-filled with that URL's callback path; paste the same URL into
the Spotify dashboard's Redirect URIs, save client settings, then Connect.
The tunnel closes itself automatically once authorization completes (success
or failure), after 15 minutes, or via its own "Stop tunnel" button — while it
is open, this UHC server is briefly reachable from the public internet at
that address, though the callback endpoint behind it only ever accepts the
single in-flight OAuth `state` token, so no extra trust is extended to
traffic that arrives through it (see `oauth_callback_json` in
`src/api/provider_auth.rs`).

Under the hood this is a plain `ssh -R` reverse tunnel to
[pinggy.io](https://pinggy.io)'s free tier — no account and no bundled binary
beyond the `ssh` client already present on essentially every Linux, macOS, or
NAS install — falling back automatically to
[localhost.run](https://localhost.run) if pinggy.io cannot be reached. Both
providers mint a fresh random subdomain on every connection, so a new tunnel
always means a newly registered callback URL; that expectation and the retry
logic live in `SpotifyTunnelManager` (`src/api/spotify_tunnel.rs`), which is
tested against a scripted fake process rather than a live tunnel provider
(the pinggy URL-parsing test fixture is an exact capture of a live anonymous
tunnel's stdout, though, since a first live-smoke pass found the original
`pinggy.link`-only pattern never matched pinggy's real anonymous-tier hosts —
`pinggy-free.link` and `free.pinggy.net`). Pinggy's free tier also caps an
anonymous tunnel at 60 minutes before it expires on its own; UHC's own
15-minute cap closes it well before that regardless. If `ssh` is missing,
both providers are unreachable, or outbound `ssh` traffic is blocked, the
panel reports why in the same beginner-readable style as the rest of
Settings, and the collapsed **Advanced: bring your own HTTPS** note
underneath still covers running your own tunnel by hand (for example
`cloudflared tunnel --url http://127.0.0.1:8088` or Tailscale Funnel) and
pasting its callback URL into the Redirect URI field yourself. Reauthorizing
later always needs a new tunnel (built-in or your own) and a newly registered
callback URL either way. This is the first-run onboarding path tracked in
#469, #534, and #538.

New endpoints backing the built-in tunnel — `POST
/api/providers/spotify/tunnel/start`, `GET
/api/providers/spotify/tunnel/status`, and `POST
/api/providers/spotify/tunnel/stop` — are controller-authenticated the same
as the rest of `/api/providers/*` (see `requires_controller_auth` in
`src/api/controller_auth.rs`) and registered in
`tests/fixtures/api_routes.txt`.

If authorization does not complete, Settings shows an actionable message
instead of a generic failure: an expired or already-used sign-in link, a
declined consent screen, a callback URL mismatch between the Spotify dashboard
and the Redirect URI configured here, or a server-side storage/adapter-start
problem are each called out with what to fix. See
`spotify_oauth_error_message` in `src/app/pages/settings.rs`, which maps every
`code` returned by `oauth_callback_json` in `src/api/provider_auth.rs` to one
of these messages.

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
- `GET /api/bridges/applemusic/status` reports the legacy freshest-companion
  fields plus a `companions` collection containing every live companion's id,
  snapshot state, and last-seen time. Multiple distinct companion ids may be
  paired concurrently; re-pairing the same id replaces that installation.
  `POST /api/bridges/applemusic/revoke` invalidates one companion token.

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
