---
title: Cloud relay is an outbound, proof-bound zero-trust boundary
status: proposed
date: 2026-08-29
---

# ADR 005: Cloud relay is an outbound, proof-bound zero-trust boundary

## Context

UHC is open source and runs inside a private LAN. HiPhi Cloud is a separate,
authenticated service for paired control surfaces. The cloud must not turn the
LAN service, provider credentials, or provider identifiers into public API
surface. A compromised relay or browser session must not be able to mint a
command accepted by UHC.

## Decision

UHC initiates a TLS WebSocket connection to a versioned HiPhi relay endpoint.
Authentication is established before the upgrade and uses a locally generated
Ed25519 installation key. The private key is persisted with mode `0600` and is
never uploaded. Pairing requires both owner confirmation and local confirmation.
The relay assigns a strictly increasing epoch; reconnects send a full semantic
state snapshot before deltas are accepted, and disconnects discard pending work.

The public wire protocol carries only semantic data: installation-scoped opaque
zone handles, zone state, now-playing metadata, bounded artwork capabilities,
and an allowlisted command vocabulary (`play_pause`, `next`, `previous`, and
absolute volume). It carries no URLs, HTTP headers, cookies, provider
credentials, raw adapter IDs, or executable payloads. The local aggregator
maintains the reverse handle map.

Every command is independently authorized by a compact Ed25519 JWS grant. UHC
pins issuer keys by `key_id` and verifies issuer, audience, installation, node,
request, epoch, scope, exact canonical payload hash, expiry, and grant
generation before dispatch. A bounded result ledger makes `request_id` the
idempotency key: a retry returns the recorded terminal result, while a lost
terminal result is `unknown_outcome` and is never silently replayed.

Artwork is a bounded, low-priority request/response lane. Capabilities are
opaque, short-lived, installation-scoped, and single-use (except an identical
idempotent retry). UHC returns bytes only; the cloud validates and serves the
image, and capability values are never logged or placed in URLs.

## Consequences

- Open-source UHC can be audited without exposing a cloud signing secret.
- HiPhi Cloud can provide account, entitlement, and façade policy without
  gaining LAN access or provider credential authority.
- The connector requires a direct Ed25519 dependency when wired into the
  crate (`ed25519-dalek = { version = "2.2", features = ["rand_core"] }`).
- Reconnection has an observable latency cost, but prevents stale commands and
  offline replay from becoming an authorization bypass.
- Existing LAN HTTP routes remain unchanged and are not used as an internet
  endpoint. Any future façade must authenticate every cloud request and map
  relay result vocabulary faithfully.

## Rejected alternatives

- Exposing the existing unauthenticated LAN API through a proxy: it violates
  the trust boundary and leaks provider-shaped identifiers.
- Forwarding browser cookies, CSRF tokens, or source IP as authentication:
  these are not installation proof and are replayable or forgeable at the
  relay boundary.
- A durable cloud/offline command queue: it makes authorization stale and can
  execute commands after revocation or an epoch replacement.

## Verification

The focused connector tests cover fixture parsing, key-file permissions,
opaque handles, command allowlisting and canonical hashing, wrong audience/key
or installation, expiry, generation revocation, replay, dropped-result
idempotency, artwork bounds, and snapshot-first reconnect behavior.
