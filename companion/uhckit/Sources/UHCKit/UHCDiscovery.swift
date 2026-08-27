import Foundation

/// Finds UHC servers advertising `_uhc._tcp.` on the local network.
///
/// Moved here from the iOS feature package (#619) because the Watch needs the
/// identical behaviour and a second copy is exactly the duplication this issue
/// exists to prevent. The iOS app now reaches it through this package; its
/// observable behaviour is unchanged.
///
/// The resolved service hostname is preferred over the TXT record's `base`: a
/// server may advertise a loopback or container hostname in its base URL, while
/// Bonjour has already resolved a name that is reachable from *this* device.
/// TXT is the fallback for proxies and tunnels whose service hostname is not
/// usable as an HTTP host.
///
/// On watchOS this is subject to the same unresolved local-network question as
/// `DirectHTTPTransport` — and more sharply, since Bonjour browsing is gated
/// separately from plain LAN sockets. A Watch build must therefore also accept
/// a manually typed address, which is why `UHCClient` takes a `URL` and never
/// requires discovery to have succeeded. See `Transport.md`.
/// `@unchecked Sendable`: every mutating entry point (`start`, `stop`, the
/// Bonjour delegate callbacks and the timeout) runs on the main queue, and the
/// async wrapper below hops to it explicitly. The compiler cannot see that from
/// an `NSObject` subclass, hence the manual conformance.
public final class UHCDiscovery: NSObject, NetServiceBrowserDelegate, NetServiceDelegate, @unchecked Sendable {
    /// Called on the main queue with the first usable server found.
    public var onBaseURL: ((URL) -> Void)?
    /// Called on the main queue when the search times out or cannot start.
    public var onFailure: ((String) -> Void)?

    private let browser = NetServiceBrowser()
    private let serviceType: String
    private let domain: String
    private let timeoutInterval: TimeInterval
    private var services: [NetService] = []
    private var finished = false
    private var timeoutWork: DispatchWorkItem?

    public init(
        serviceType: String = "_uhc._tcp.",
        domain: String = "local.",
        timeout: TimeInterval = 8
    ) {
        self.serviceType = serviceType
        self.domain = domain
        self.timeoutInterval = timeout
        super.init()
    }

    public func start() {
        finished = false
        services.removeAll()
        browser.delegate = self
        browser.searchForServices(ofType: serviceType, inDomain: domain)
        let timeout = DispatchWorkItem { [weak self] in
            guard let self, !self.finished else { return }
            self.finished = true
            self.stop()
            self.onFailure?("No UHC server was found on the local network.")
        }
        timeoutWork = timeout
        DispatchQueue.main.asyncAfter(deadline: .now() + timeoutInterval, execute: timeout)
    }

    public func stop() {
        timeoutWork?.cancel()
        timeoutWork = nil
        browser.stop()
        services.forEach { $0.stop() }
        services.removeAll()
    }

    /// One-shot async wrapper, for call sites that would rather await than wire
    /// up two callbacks. Cancels the browse if the surrounding task is
    /// cancelled.
    public static func firstServer(
        serviceType: String = "_uhc._tcp.",
        domain: String = "local.",
        timeout: TimeInterval = 8
    ) async throws -> URL {
        let discovery = UHCDiscovery(serviceType: serviceType, domain: domain, timeout: timeout)
        return try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                // Bonjour can fire a resolve and the timeout in quick
                // succession; a continuation resumed twice is a crash, so guard
                // it rather than trusting the ordering.
                let resumed = ResumeGuard()
                discovery.onBaseURL = { url in
                    if resumed.claim() { continuation.resume(returning: url) }
                }
                discovery.onFailure = { message in
                    if resumed.claim() {
                        continuation.resume(throwing: UHCError.unreachable(underlying: message))
                    }
                }
                DispatchQueue.main.async { discovery.start() }
            }
        } onCancel: {
            DispatchQueue.main.async { discovery.stop() }
        }
    }

    // MARK: - NetServiceBrowserDelegate

    public func netServiceBrowser(
        _ browser: NetServiceBrowser,
        didFind service: NetService,
        moreComing: Bool
    ) {
        guard !finished else { return }
        services.append(service)
        service.delegate = self
        service.resolve(withTimeout: 5)
    }

    public func netServiceBrowser(
        _ browser: NetServiceBrowser,
        didNotSearch errorDict: [String: NSNumber]
    ) {
        onFailure?("Could not search for UHC on the local network.")
    }

    // MARK: - NetServiceDelegate

    public func netServiceDidResolveAddress(_ sender: NetService) {
        guard !finished else { return }
        if let host = sender.hostName?.trimmingCharacters(in: CharacterSet(charactersIn: ".")),
           !host.isEmpty, sender.port > 0,
           let url = URL(string: "http://\(host):\(sender.port)") {
            finish(url)
            return
        }

        let txt = sender.txtRecordData().map(NetService.dictionary(fromTXTRecord:)) ?? [:]
        if let raw = txt["base"].flatMap({ String(data: $0, encoding: .utf8) }),
           let url = URL(string: raw), url.host != nil {
            finish(url)
        }
    }

    public func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        guard !finished, services.allSatisfy({ $0 === sender || $0.port > 0 }) else { return }
        onFailure?("Could not resolve UHC on the local network.")
    }

    private func finish(_ url: URL) {
        finished = true
        stop()
        onBaseURL?(url)
    }
}

/// Single-claim latch, so a continuation is resumed exactly once.
private final class ResumeGuard: @unchecked Sendable {
    private let lock = NSLock()
    private var claimed = false

    func claim() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        if claimed { return false }
        claimed = true
        return true
    }
}
