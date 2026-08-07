# UHC controller authentication boundary (#488)

This is the proposed installation-auth contract for exposing UHC through a
temporary tunnel or a future hosted UI. It is deliberately a contract only;
route and payload changes require explicit approval and the repository's
`api-change-approved` gate.

## Three separate authorities

UHC must never treat one credential as another audience's credential:

1. **Browser controller session** — an installation-bound, opaque, persisted
   session in an `HttpOnly`, `SameSite` cookie. State-changing cookie requests
   require an exact same-origin `Origin`/`Referer` check and a CSRF token.
2. **MCP/controller bearer** — a scoped, expiring token for native or hosted
   clients. It has explicit `read`, `control`, and `configure` scopes and is
   revocable. `MCP-Session-ID` remains protocol correlation, not authority.
3. **Apple bridge bearer** — the existing short-lived companion credential,
   bound to one bridge installation and one execution-owner player. It may
   publish state, poll commands, acknowledge commands, and revoke itself; it
   cannot configure UHC, read Spotify identity, or mint another bridge.

All comparisons are constant-time where applicable. Tokens and provider
credentials are never logged or returned in errors. Unauthorized and
forbidden responses are generic and rate-limited per installation/source.

## Local/tunnel bootstrap

On first start, UHC creates an installation identity and a one-time bootstrap
secret in the owner-only encrypted config directory. An operator may instead
provide `UHC_BOOTSTRAP_TOKEN`. The secret is shown once by the local CLI/package
or a local-only setup surface; it is never placed in a tunnel URL.

The local install flow is:

1. Start UHC and expose its configured port through a temporary HTTPS tunnel.
2. Open the tunnel URL and enter the one-time bootstrap secret.
3. UHC invalidates the secret and issues the browser controller session plus
   CSRF token.
4. The authenticated Settings UI configures Spotify and starts OAuth. The
   exact HTTPS callback URI remains registered with Spotify.
5. The UI creates a scoped controller/MCP token only when requested.
6. Stop the tunnel after setup unless the operator has deliberately configured
   a persistent authenticated reverse proxy.

Recovery uses a new one-time bootstrap secret from the local console/package,
or an operator-provided `UHC_BOOTSTRAP_TOKEN`; it does not reuse a browser
cookie or Apple bridge bearer.

## Proposed route boundary

The following additive routes are candidates for the approved implementation:

- `POST /api/controller/bootstrap`
- `POST /api/controller/session` (optional re-login/recovery)
- `GET /api/controller/status`
- `POST /api/controller/tokens`
- `DELETE /api/controller/tokens/{token_id}`

The controller middleware then protects provider configuration/OAuth/revoke/
account, Apple pairing/status/revoke, `/api/settings`, MCP, and all mutating
legacy playback/configuration routes. `/status`, static assets, and the UI shell
may remain public. Apple `claim` remains pairing-code bootstrap for the native
companion; it is not a generic controller login.

Hosted UI later exchanges its hosted identity for a short-lived,
installation-scoped controller token. It does not receive or replay the local
browser cookie. Exact CORS origins remain opt-in, and forwarded host/scheme
headers are trusted only from a configured proxy.

Until this contract is approved and implemented, UHC must remain LAN-only or
sit behind an authenticated tunnel/reverse proxy such as Cloudflare Access or
Tailscale identity-aware access.
