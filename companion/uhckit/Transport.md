# Reaching UHC from a Watch

Status: **unsettled**. This document records what is known, what was measured,
and exactly what still needs a physical Apple Watch to decide (#619).

## The question

Can a watchOS app open an HTTP connection to a device on the local network —
UHC on `192.168.1.209:8088`, or `NAS2.local:8088` — or must the request be
relayed through the paired iPhone?

The evidence in the issue conflicts:

- Apple DTS states a plain `URLSession` call to a LAN IP **fails** on watchOS
  ([forum thread 785846](https://developer.apple.com/forums/thread/785846)).
- rooWatch ships as a standalone Watch Roon controller that connects **directly
  over WiFi**, which the first claim would forbid.
- A third thread suggests the deciding factor is **an iOS app in the same bundle
  holding local-network permission**
  ([thread 759321](https://developer.apple.com/forums/thread/759321)).

This project satisfies that third condition: the iOS companion already declares
`NSLocalNetworkUsageDescription` and `NSBonjourServices` for `_uhc._tcp`, and
`UHCWatchApp` is embedded in the iOS app bundle via an *Embed Watch Content*
build phase. So if the third explanation is the correct one, this build should
work.

## What is actually settled

**`NetService` and `NetServiceBrowser` are unavailable on watchOS.** This is not
a runtime guess — it is a compile error:

```
error: 'NetServiceBrowser' is unavailable in watchOS
```

That is the one hard fact obtainable without a device. It means classic Bonjour
discovery cannot be shared with iOS, and `UHCDiscovery` therefore carries an
`NWBrowser` implementation for watchOS. The watchOS path reads the service's TXT
record and uses its `base` key (`base=http://NAS2:8088`), because resolving a
`NWBrowser` result to a host otherwise requires opening an `NWConnection`.

A consequence worth noting: Bonjour browsing is gated *separately* from plain
LAN sockets. Discovery could fail while direct HTTP to a typed-in address still
works, or vice versa. The Watch UI therefore always offers manual address entry
and never requires discovery to have succeeded.

## What the simulator proves — and does not

The watchOS simulator runs on macOS and uses the Mac's networking stack. It will
reach the LAN happily whatever the real OS policy is. **A green simulator run is
not evidence about real hardware.**

What the simulator run did legitimately establish (watchOS 26.5, Apple Watch
Series 11 46mm) is that the client and UI are correct: the app discovered the
live server over Bonjour, listed all 10 real zones, rendered now-playing with
decoded JPEG artwork, and honoured the server's per-command capability flags.
Those would be equally true on a device *if* the transport works.

## How to settle it

Build and install `UHCWatchApp` on a real Apple Watch, on the same WiFi network
as UHC, then:

1. **Launch the app.** It browses for `_uhc._tcp`.
   - Zones appear → direct LAN access works, including Bonjour. Done.
   - "No UHC server found" → continue.
2. **Open `Server` → `Manual address`** and enter the server's IP and port
   (`192.168.1.209:8088`). Tap **Connect**. This separates the two gates: if
   this works, plain LAN sockets are allowed and only Bonjour browsing is
   blocked.
3. **If it still fails, read the `Diagnostics` section** on that same screen. It
   shows the raw underlying error rather than a friendly paraphrase, which is
   the detail that distinguishes the cases:
   - `-1004` (`cannotConnectToHost`) — nothing listening, or the request was
     refused. Check the address first.
   - `-1009` (`notConnectedToInternet`) — the OS considers the app offline; on a
     Watch that is the signature of a *policy* block rather than a routing
     failure.
   - `-1001` (`timedOut`) — silently dropped, the classic look of a firewall or
     entitlement block.
4. **Also try with the iPhone switched off or out of range.** If it works only
   while the phone is nearby, the traffic is being proxied through the phone by
   the OS and is not true direct access — which changes the answer even though
   both look identical from the app.

Please report the exact diagnostics string. That is what turns this from a
guess into a fact.

## If direct access turns out to be blocked

Nothing above the transport changes. `UHCClient` is constructed with a
`UHCTransport`, and the Watch builds one in exactly one place —
`WatchController.connect(to:)`:

```swift
let client = UHCClient(transport: DirectHTTPTransport(baseURL: url))
```

Swapping that for `PhoneRelayTransport()` is the entire migration. The models,
the client, and every view are untouched, because a transport moves one
`UHCRequest` and returns one `UHCResponse` and knows nothing about zones,
artwork, or transport controls.

`PhoneRelayTransport` is a deliberate, documented placeholder in this PR: it
compiles, conforms, and throws `UHCError.transportUnavailable` on every call so
that wiring it up by mistake fails loudly instead of hanging. Its source
documents what implementing it requires — `sendMessageData` on the Watch side, a
`WCSessionDelegate` on the phone side, the ~64 KB message cap (measured artwork
is ~15–27 KB at 200–240 px, so it fits today), and the fact that
`WCSession.isReachable` is false exactly when a Watch-only user wants the
controller most.

It was left unimplemented on purpose. Building a relay before knowing whether it
is needed is work spent ahead of the evidence, and the evidence costs one
install.
