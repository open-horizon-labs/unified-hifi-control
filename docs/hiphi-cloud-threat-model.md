# HiPhi Cloud connector threat model and security invariants

This document describes the security boundary between an open-source Unified
Hi-Fi Control (UHC) installation on a private network and the optional HiPhi
Cloud remote-control service. It covers the public connector and relay protocol
implemented in this repository. Cloud account, façade, relay, and controller
implementations must preserve the same invariants even when their source is
maintained separately.

This is a design and review contract, not a security certification. Statements
about what the system prevents assume that the deployed code, keys, TLS
configuration, and operational controls match this document.

“Zero trust” is scoped here to the relay boundary: possession of a relay socket
is not command authority. It does not mean that the cloud service is trustless,
that semantic state is end-to-end encrypted, or that every component is
independently verifiable from this repository.

Related documents:

- [ADR 005: Cloud relay is an outbound, proof-bound zero-trust boundary](adr/005-cloud-relay-zero-trust-boundary.md)
- [HiPhi Cloud issuer-key rotation](hiphi-cloud-issuer-key-rotation.md)
- Relay wire types and bounds: [`src/cloud_connector/protocol.rs`](../src/cloud_connector/protocol.rs)

## Security goals

The connector is intended to provide remote semantic playback control without
turning UHC into a general-purpose path into the home network.

It must:

1. keep provider credentials, provider-native identifiers, LAN addresses, and
   UHC's existing HTTP API behind the local trust boundary;
2. accept remote commands only when a pinned command-signing authority binds a
   short-lived grant to one installation, controller, session epoch, request,
   idempotency key, scope, and exact canonical payload;
3. require proof of the installation's locally held private key before a relay
   session is established;
4. contain replay, stale-session, parser, artwork, and resource-exhaustion
   failures rather than forwarding them into provider adapters;
5. fail closed when cloud authentication or protocol validation fails while
   leaving local playback and LAN control available; and
6. remain secure when the UHC source, protocol fixtures, endpoint names,
   application IDs, and pinned public keys are public.

## Assets to protect

- Provider-native credentials and refresh tokens.
- The installation Ed25519 private key and pairing state.
- The ability to issue playback and volume commands on a listener's system.
- LAN topology, hostnames, addresses, URLs, raw provider/device identifiers,
  cookies, and HTTP headers.
- Owner and controller credentials managed by the cloud service.
- Semantic listening state such as zone names, titles, artists, playback state,
  volume, and artwork.
- Service availability and local playback independence.

## Trust boundaries

### 1. UHC host and private LAN

UHC, its configuration directory, aggregator, adapters, and provider
credentials are trusted to enforce local policy. The aggregator remains the
only authority for client-facing state. The connector may request allowlisted
semantic actions through normal local dispatch; it may not call arbitrary LAN
HTTP endpoints or bypass the aggregator.

The connector opens an **outbound** TLS WebSocket. Pairing and remote access do
not require an inbound firewall rule, port forward, public UHC route, reverse
proxy, or cloud-supplied destination URL.

### 2. Installation identity

Each installation generates its own Ed25519 key pair. The private key remains
on the UHC host with owner-only filesystem permissions. Enrollment exports only
the public key and fingerprint, and requires both a signed-in owner action and
a local owner-only handoff. Installation keys are not system-wide cloud keys.

### 3. Cloud authority and relay

The cloud authority authenticates owners, pairs controllers, issues bounded
session and command grants, and owns entitlement policy. The relay coordinates
the live socket and can observe, delay, reorder, duplicate, or drop semantic
messages. UHC therefore does not treat possession of a relay connection as
command authority.

Session and command signers are separate trust roots. A session-signing key can
authenticate a short-lived socket grant but cannot authorize playback. A
command-signing key can authorize an exact command but cannot prove possession
of an installation key or authenticate the socket. UHC rejects configuration
that reuses a key ID or public key across those roles.

The command-signing authority is nevertheless a trusted, system-wide
component. Its compromise can forge bounded commands for installations that
pin that authority; see [Residual risks and non-goals](#residual-risks-and-non-goals).

### 4. Remote controllers

Browsers, Garmin watches, and future controllers authenticate to the cloud, not
to a public LAN endpoint. A controller is account-bound, installation-scoped,
individually named, expiring, and revocable. UHC does not trust a controller
credential directly; it trusts only a valid command grant from its pinned
command authority.

### 5. Public artwork delivery

Garmin fetches artwork through Garmin infrastructure and cannot attach the
watch bearer to that image request. The public artwork route therefore uses an
opaque, short-lived, installation- and artwork-bound capability. UHC accepts no
URL in the artwork request, applies source-size and concurrency bounds, and
returns bytes through a separate low-priority lane. The cloud deployment must
decode, validate, re-encode, and output-bound those bytes before serving them
publicly.

## Attacker model

The design considers:

- an unauthenticated internet attacker sending malformed, oversized, replayed,
  guessed, or high-volume requests;
- a malicious or compromised controller with a valid but bounded credential;
- a stolen owner browser session attempting to pair or retain a controller;
- a malicious or compromised relay that can observe and manipulate traffic but
  does not possess a command-signing key;
- compromise of either cloud signing role;
- a LAN peer probing UHC's separately configured local APIs;
- dependency, build, package, or update-channel compromise; and
- a compromised UHC host or provider process.

The last two cases can replace trusted code or read local secrets and are not
solved by the relay protocol. They require ordinary host, dependency, signing,
and update security.

## Data that crosses the boundary

The cloud protocol deliberately carries a semantic projection, not a provider
or LAN projection.

| May cross while cloud control is enabled | Must remain local |
|---|---|
| Installation ID, public key/fingerprint, connector version, protocol capabilities | Installation private key |
| Opaque zone handle, owner-visible zone name, transport state, and bounded volume state | Raw Roon/LMS/OpenHome/UPnP/HQPlayer/Spotify/Apple Music identifiers |
| Bounded now-playing title, artist, playing state, and opaque image revision | Provider credentials, refresh tokens, cookies, CSRF tokens, and authorization headers |
| Bounded artwork bytes requested through an opaque capability | Provider artwork URL, arbitrary URL, LAN URL, hostname, or filesystem path |
| Controller ID in signed grants, command/result status, epoch, revision, and timestamps | General LAN HTTP requests or responses and UHC's local API surface |

This means HiPhi Cloud can learn zone names and now-playing metadata while the
connector is active. That is an intentional privacy tradeoff, not encrypted
end-to-end state. In addition, Cloudflare or another TLS/network edge
necessarily sees connection metadata such as the public source IP, timing, and
traffic volume even though the relay protocol does not carry or persist a LAN
address.

## Security invariants

These are release invariants. A change that violates one requires an explicit
security decision and an update to this document and ADR 005.

1. **No inbound cloud path.** UHC initiates the connection. The feature never
   exposes, proxies, or tunnels the existing LAN HTTP service.
2. **Pinned secure destination.** Production configuration accepts an exact
   `wss://` relay endpoint and rejects insecure, credential-bearing, fragment,
   or otherwise non-canonical endpoints. The cloud cannot supply a per-command
   destination.
3. **Local private-key custody.** The installation private key is generated and
   stored locally with owner-only permissions and is never serialized into a
   browser response, handoff file, log, protocol message, image URL, package,
   or repository.
4. **Dual-confirm enrollment.** Pairing requires a signed-in owner capability
   bound to the installation public key plus an owner-only local handoff and
   installation-key proof. A browser bearer is never copied into UHC.
5. **Separate signing roles.** Session and command issuer rings are independently
   pinned, bounded to eight Ed25519 keys, and may not overlap by key ID or
   public key.
6. **Proof-bound sessions.** A short-lived session grant is bound to the exact
   installation, installation public key, connector version, endpoint, and
   generation. Before the socket becomes authoritative, UHC also signs the
   relay's fresh nonce challenge with the installation private key.
7. **Exact command grants.** Every command grant binds issuer, audience,
   installation, controller, request ID, idempotency key, session epoch, scope,
   exact canonical payload hash, expiry, and revocation generation. The maximum
   command-grant lifetime is 15 seconds.
8. **Allowlisted semantics only.** The wire command vocabulary is limited to
   play/pause, next, previous, relative volume, and bounded finite absolute
   volume. Unknown fields, duplicate security fields, non-finite values, and
   unknown actions fail before dispatch.
9. **Fresh epoch and snapshot first.** Reconnect assigns a new monotonically
   increasing epoch. UHC sends a full aggregator-derived snapshot before
   accepting deltas or work, and drops pending work on disconnect.
10. **Replay and uncertain-result safety.** Request IDs are replay checked;
    idempotency keys are bound to payload hashes in a bounded terminal ledger.
    A lost terminal result becomes `unknown_outcome` and is not silently
    executed again.
11. **Bounded parsing and work.** Messages, command payloads, strings, zone
    counts, replay caches, result ledgers, artwork source/output sizes, chunks,
    queue depth, and concurrency have explicit limits. Capacity exhaustion
    rejects or drops excess work instead of evicting authorization state or
    growing without limit.
12. **Opaque handles.** Provider IDs and artwork keys are reduced to
    installation-scoped opaque handles/revisions. The reverse mapping exists
    only in local process memory, is never serialized, and is regenerated after
    a UHC process restart.
13. **Capability-bound artwork.** Artwork capabilities contain no fetch URL,
    are short-lived and single-use except for an identical idempotent retry,
    and cannot select an arbitrary host, port, path, header, or file.
14. **Fail closed, local operation survives.** Invalid configuration, expired or
    forged grants, clock failure, revocation, relay loss, and cloud outage stop
    remote work. They do not disable the aggregator, adapters, or local control.
15. **No secret-by-obscurity dependency.** Publishing UHC as open source must
    reveal no credential that grants runtime authority. Security rests on local
    private keys, protected cloud signer keys, explicit trust roots, freshness,
    exact binding, and bounded parsers—not on hiding source or wire formats.
16. **Sensitive values are not observability fields.** Credentials, grants,
    enrollment secrets, artwork capabilities/paths, provider identifiers,
    command payloads, and artwork bytes must not be placed in application logs,
    analytics, presence records, or error text.

Executable evidence lives primarily in `tests/cloud_connector.rs`,
`tests/fixtures/hiphi-relay-v1/`, `tests/hiphi_pairing_ui_contract.rs`, package
contracts, and the cloud deployment's separate adversarial tests. Documentation
is not a substitute for those checks.

## Residual risks and non-goals

### Command-authority compromise

The current design does not provide controller-to-installation end-to-end
signatures. The system-wide command-signing authority is trusted to enforce
account ownership, controller scope, installation selection, revocation, and
payload intent. If its private key and routing context are compromised, an
attacker may mint otherwise valid short-lived commands across installations
that pin that key. Separate session signing prevents that key from proving an
installation session, but it does not remove this command-forgery blast radius.

Mitigations are protected secret storage, role separation, short grants,
generation revocation, bounded verifier overlap, audit/alerting, and the
documented emergency rotation procedure. A future stronger design could give
each controller an owner-approved public key and require a controller signature
that UHC verifies end to end; that is not implemented today.

### Semantic-state privacy

The cloud sees the semantic projection needed to render remote controls,
including owner-visible zone names and now-playing metadata. Provider
credentials and raw provider identifiers staying local does not make that
projection private from the service. End-to-end encrypted state is not a goal
of protocol version 1.

### Account and controller compromise

A stolen owner session may approve, rename, or revoke resources according to
the cloud account policy. A stolen controller credential may act within its
installation, scope, expiry, and revocation generation. MFA, session security,
device storage, owner-visible controller inventory, expiry, and revocation are
cloud responsibilities.

### Local-host and LAN compromise

An attacker controlling the UHC process or host can read local state, issue
local commands, replace trust roots, or modify connector code. The connector
does not sandbox provider adapters. Existing LAN API authentication is a
separate boundary; enabling HiPhi Cloud must not weaken it, but does not repair
an independently unsafe LAN deployment.

### Availability and traffic analysis

The relay, authority, DNS, TLS PKI, network edge, or internet connection can
deny service. The design intentionally has no durable offline command queue,
because executing old commands after revocation or reconnection is a larger
risk than remote-control availability. Network observers and the TLS edge can
infer connection timing and volume.

### Supply chain and cryptographic assumptions

The model assumes correctly implemented Ed25519, SHA-256, canonical JSON, TLS,
secure randomness, a reasonably synchronized clock, trustworthy release
artifacts, and protected dependency/build/update infrastructure. Failures in
those foundations can invalidate the protocol guarantees.

### Cloud implementation assurance

Publishing UHC and its protocol does not make separately deployed cloud code,
signing operations, account policy, persistence, or image sanitization
auditable. The cloud implementation is not made auditable by publishing UHC;
its deployment needs its own review, tests, secret controls, and incident
response. This public document defines the contract that deployment must meet,
not evidence that every deployment meets it.

## Reviewing changes against this model

Treat a change as security-significant if it adds a command/action, field,
controller type, public route, cloud-persisted datum, artwork behavior, trust
root, pairing path, offline behavior, or log field. Review it by asking:

1. Does it move a provider credential, raw identifier, URL, or LAN authority
   across the boundary?
2. Can a relay, controller, browser session, or stale grant cause work without
   the exact required signer and installation binding?
3. Is freshness, replay behavior, uncertain outcome, revocation, and reconnect
   behavior explicit?
4. Are parsing, storage, concurrency, and response sizes bounded before work is
   dispatched?
5. Does cloud failure remain isolated from local playback?
6. Does publishing the implementation reveal any operational secret? If yes,
   the secret is in the wrong place.

Do not post working credentials, private keys, or live exploit details in a
public issue. The repository should publish a dedicated private security-
reporting channel before the remote-control feature is presented as generally
available.
