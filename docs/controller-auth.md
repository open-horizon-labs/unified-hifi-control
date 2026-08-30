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
3. **Apple bridge bearer** — the existing ephemeral, in-memory companion
   credential, liveness-bounded and bound to one bridge installation and one
   execution-owner player. It may
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

1. Start UHC locally and bootstrap the browser controller session on its LAN
   address; never put that session or bootstrap secret in a tunnel URL.
2. The authenticated Settings UI configures Spotify, then opens a temporary
   HTTPS tunnel only to UHC's callback-only loopback listener.
3. The exact HTTPS callback URI remains registered with Spotify. The public
   listener accepts only that callback plus its bounded liveness probe; it
   does not expose the UHC UI, bootstrap, MCP, provider settings, or LAN API.
4. The UI creates a scoped controller/MCP token only when requested.
5. Stop the tunnel after OAuth unless the operator has deliberately configured
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

The controller middleware then protects provider configuration/OAuth-start/
revoke/account, Apple pairing/status/revoke, `/api/settings`, MCP, and all
mutating legacy playback/configuration routes. The Spotify OAuth callback is
the deliberate exception: it is a cross-site redirect, so the browser's
`SameSite=Strict` cookie is absent; its single-use pending state and PKCE
exchange are the authority for that one callback. `/status`, static assets,
and the UI shell may remain public. Apple `claim` remains pairing-code
bootstrap for the native companion; it is not a generic controller login.

Hosted UI later exchanges its hosted identity for a short-lived,
installation-scoped controller token. It does not receive or replay the local
browser cookie. Exact CORS origins remain opt-in, and forwarded host/scheme
headers are trusted only from a configured proxy.

## Migration and enforcement

The controller boundary is implemented behind the explicit
`UHC_REQUIRE_CONTROLLER_AUTH=true` install switch. It is intentionally opt-in
for existing LAN installs while the Settings UI gains its bootstrap screen;
when unset, the historical LAN browser flow remains available. Tunnel or
hosted deployments must set the switch before exposing UHC. With enforcement
enabled, provider configuration/OAuth-start/revoke/account, Apple pairing
management, MCP, and mutating playback/configuration routes require the
browser session and CSRF checks. The Spotify OAuth callback is authenticated
by its pending state/PKCE exchange instead of the browser cookie. Read-only
device/status pages remain available, and native Apple bridge bearer routes
remain independent.

Independent of that switch, `/api/providers/*` and Apple Music pairing are
*always* owner-gated (`requires_controller_auth` in
`src/api/controller_auth.rs`) — a fresh install cannot let any reachable
client replace OAuth credentials or mint a companion pairing before the owner
bootstraps. The Settings UI's client-side counterpart to this contract lives
in `src/app/controller_auth.rs` and `src/app/components/bootstrap_prompt.rs`:
every fetch helper in `src/app/api.rs` routes a `controller_unauthorized`
response into an in-page bootstrap prompt (token → `POST
/api/controller/bootstrap` → CSRF token stored for subsequent requests)
instead of surfacing the raw `HTTP 401`. See "First save on a NAS: the owner
bootstrap prompt" in `docs/streaming-adapters.md` for the full walkthrough.
