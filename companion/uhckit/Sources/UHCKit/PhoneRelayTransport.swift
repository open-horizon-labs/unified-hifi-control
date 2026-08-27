import Foundation

/// The fallback transport: the Watch asks the paired iPhone to make the request
/// and send the bytes back over WatchConnectivity.
///
/// **Deliberately unimplemented in this PR** (#619). It exists as a compiling
/// conformance so that the day `DirectHTTPTransport` is proven unusable on real
/// Watch hardware, the fix is one line at the composition root — see
/// `UHCClient.init(transport:)` — and not a rewrite of the client, the models,
/// or the views. That property is the actual deliverable here; finishing the
/// relay before knowing whether it is needed would be work spent ahead of the
/// evidence.
///
/// Every call throws `UHCError.transportUnavailable`, so a caller that wires
/// this up by mistake fails loudly and immediately rather than hanging.
///
/// ## What implementing it requires
///
/// 1. **Watch side** — `WCSession.default.sendMessageData(_:replyHandler:)`,
///    encoding `UHCRequest` and decoding `UHCResponse`. Both are `Codable`-able
///    value types with no reference members, which is why they are shaped that
///    way. `sendMessageData` needs the counterpart reachable; the
///    `transferUserInfo` queue is the offline path but is far too slow for
///    transport controls and should not be used for them.
/// 2. **Phone side** — a `WCSessionDelegate` implementing
///    `session(_:didReceiveMessageData:replyHandler:)` that hands the decoded
///    request to a `DirectHTTPTransport` and replies with the encoded response.
///    The phone app already holds local-network permission, so its own LAN
///    access is not in question.
/// 3. **Payload limit** — a WatchConnectivity message is capped (on the order
///    of 64 KB). Album art at 240×240 JPEG measured ~27 KB against the live
///    server, so it fits today, but a relay must either request a bounded size
///    via the `width`/`height` parameters or fall back to
///    `transferFile(_:metadata:)` for anything larger.
/// 4. **Reachability** — `WCSession.isReachable` is false when the phone is
///    away or locked, which is precisely when a Watch-only user wants the
///    controller most. A production relay should race both transports rather
///    than replace one with the other.
public final class PhoneRelayTransport: UHCTransport, @unchecked Sendable {
    private static let notImplemented = """
        Relaying UHC requests through the paired iPhone is not implemented yet. \
        Use DirectHTTPTransport, or implement PhoneRelayTransport per its \
        documentation.
        """

    public init() {}

    public var describedEndpoint: String { "iPhone relay (not implemented)" }

    public func perform(_ request: UHCRequest) async throws -> UHCResponse {
        throw UHCError.transportUnavailable(Self.notImplemented)
    }
}
