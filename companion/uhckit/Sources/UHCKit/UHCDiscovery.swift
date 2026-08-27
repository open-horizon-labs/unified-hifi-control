import Foundation

#if os(watchOS)
import Network
#endif

/// Finds UHC servers advertising `_uhc._tcp.` on the local network.
///
/// Moved here from the iOS feature package (#619) because the Watch needs the
/// same capability and a second copy is exactly the duplication this issue
/// exists to prevent.
///
/// ## Why there are two implementations
///
/// **`NetService` and `NetServiceBrowser` do not exist on watchOS.** They are
/// marked unavailable, so this is a compile error rather than a runtime
/// failure — one of the few hard facts available about watchOS networking
/// without a device in hand (see `Transport.md`). The iOS path therefore keeps
/// the exact `NetService` code that ships today, byte for byte, and watchOS
/// gets an `NWBrowser` implementation.
///
/// The two differ in how they arrive at a URL, and it is worth knowing which
/// you are relying on:
///
/// - **iOS/macOS** resolves the service and prefers the *resolved hostname*,
///   falling back to the TXT record's `base` key.
/// - **watchOS** can only read the TXT record: `NWBrowser` reports services and
///   their metadata but resolving one to a host requires opening an
///   `NWConnection`, and the TXT `base` key the server already publishes
///   (`base=http://NAS2:8088`) makes that round trip unnecessary. A server that
///   advertises no `base` key is therefore undiscoverable from the Watch, which
///   is why the Watch UI always offers manual address entry too.
public final class UHCDiscovery: NSObject, @unchecked Sendable {
    /// Called on the main queue with the first usable server found.
    public var onBaseURL: ((URL) -> Void)?
    /// Called on the main queue when the search times out or cannot start.
    public var onFailure: ((String) -> Void)?

    private let serviceType: String
    private let domain: String
    private let timeoutInterval: TimeInterval
    private var finished = false
    private var timeoutWork: DispatchWorkItem?

    #if os(watchOS)
    private var browser: NWBrowser?
    #else
    private let browser = NetServiceBrowser()
    private var services: [NetService] = []
    #endif

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
        let timeout = DispatchWorkItem { [weak self] in
            guard let self, !self.finished else { return }
            self.finished = true
            self.stop()
            self.onFailure?("No UHC server was found on the local network.")
        }
        timeoutWork = timeout
        DispatchQueue.main.asyncAfter(deadline: .now() + timeoutInterval, execute: timeout)
        startBrowsing()
    }

    public func stop() {
        timeoutWork?.cancel()
        timeoutWork = nil
        stopBrowsing()
    }

    private func finish(_ url: URL) {
        guard !finished else { return }
        finished = true
        stop()
        onBaseURL?(url)
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

    /// The first usable numeric host in a Bonjour address list, IPv4 first.
    /// Link-local IPv6 is skipped: it needs a scope id that does not survive
    /// being written into a URL string.
    static func numericHost(from addresses: [Data]) -> String? {
        var fallbackV6: String?
        for data in addresses {
            let host: String? = data.withUnsafeBytes { raw -> String? in
                guard let base = raw.baseAddress, raw.count >= MemoryLayout<sockaddr>.size else {
                    return nil
                }
                let sa = base.assumingMemoryBound(to: sockaddr.self)
                var buffer = [CChar](repeating: 0, count: Int(NI_MAXHOST))
                guard getnameinfo(sa, socklen_t(raw.count), &buffer, socklen_t(buffer.count),
                                  nil, 0, NI_NUMERICHOST) == 0 else { return nil }
                let text = String(cString: buffer)
                if sa.pointee.sa_family == sa_family_t(AF_INET) { return text }
                if sa.pointee.sa_family == sa_family_t(AF_INET6) {
                    if text.hasPrefix("fe80") || text.contains("%") { return nil }
                    return "[\(text)]"
                }
                return nil
            }
            guard let host else { continue }
            if host.hasPrefix("[") { fallbackV6 = fallbackV6 ?? host } else { return host }
        }
        return fallbackV6
    }

    /// Reads a usable base URL out of a Bonjour TXT record. Shared by both
    /// implementations so the fallback rule cannot drift between platforms.
    static func baseURL(fromTXT txt: [String: String]) -> URL? {
        guard let raw = txt["base"], let url = URL(string: raw), url.host != nil else {
            return nil
        }
        return url
    }
}

// MARK: - watchOS: NWBrowser

#if os(watchOS)
extension UHCDiscovery {
    private func startBrowsing() {
        let parameters = NWParameters()
        parameters.includePeerToPeer = false

        // NWBrowser wants the type without the trailing dot NetService uses.
        let type = serviceType.hasSuffix(".") ? String(serviceType.dropLast()) : serviceType
        let trimmedDomain = domain.hasSuffix(".") ? String(domain.dropLast()) : domain

        let browser = NWBrowser(
            for: .bonjourWithTXTRecord(type: type, domain: trimmedDomain),
            using: parameters
        )
        self.browser = browser

        browser.stateUpdateHandler = { [weak self] state in
            guard let self else { return }
            if case .failed(let error) = state {
                DispatchQueue.main.async {
                    guard !self.finished else { return }
                    // On watchOS this is the signal that matters: a denial here
                    // means local-network access is refused, not that the
                    // server is absent.
                    self.onFailure?("Bonjour browsing failed: \(error.localizedDescription)")
                }
            }
        }

        browser.browseResultsChangedHandler = { [weak self] results, _ in
            guard let self else { return }
            for result in results {
                guard case .bonjour(let record) = result.metadata else { continue }
                guard let url = UHCDiscovery.baseURL(fromTXT: record.dictionary) else { continue }
                DispatchQueue.main.async { self.finish(url) }
                return
            }
        }

        browser.start(queue: .main)
    }

    private func stopBrowsing() {
        browser?.cancel()
        browser = nil
    }
}
#endif

// MARK: - iOS/macOS: NetService
//
// Unchanged from the implementation that ships in the iOS companion today.

#if !os(watchOS)
extension UHCDiscovery: NetServiceBrowserDelegate, NetServiceDelegate {
    private func startBrowsing() {
        services.removeAll()
        browser.delegate = self
        browser.searchForServices(ofType: serviceType, inDomain: domain)
    }

    private func stopBrowsing() {
        browser.stop()
        services.forEach { $0.stop() }
        services.removeAll()
    }

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

    public func netServiceDidResolveAddress(_ sender: NetService) {
        guard !finished else { return }
        // Prefer the numeric address Bonjour has ALREADY resolved. Using the
        // `.local` hostname instead makes every subsequent request re-run an
        // mDNS lookup, and those lose the race often enough to matter -- on
        // the owner's network that surfaced as sporadic `-1001` timeouts and
        // a `did not receive all answers in time for nas2.local:8088` in the
        // device log, on a server that was up the whole time.
        //
        // Trade-off, deliberate: an address can go stale if DHCP moves the
        // server, where a hostname would have followed it. Re-running
        // discovery fixes that, and a lookup that intermittently fails is
        // worse than one that fails predictably and can be retried.
        if let address = UHCDiscovery.numericHost(from: sender.addresses ?? []),
           sender.port > 0,
           let url = URL(string: "http://\(address):\(sender.port)") {
            finish(url)
            return
        }

        // No usable address: fall back to the resolved hostname.
        if let host = sender.hostName?.trimmingCharacters(in: CharacterSet(charactersIn: ".")),
           !host.isEmpty, sender.port > 0,
           let url = URL(string: "http://\(host):\(sender.port)") {
            finish(url)
            return
        }

        // Fall back to the explicit base URL for proxies/tunnels whose service
        // hostname is not usable as an HTTP host.
        let raw = sender.txtRecordData().map(NetService.dictionary(fromTXTRecord:)) ?? [:]
        let txt = raw.compactMapValues { String(data: $0, encoding: .utf8) }
        if let url = UHCDiscovery.baseURL(fromTXT: txt) {
            finish(url)
        }
    }

    public func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        // Only give up once every other candidate has also failed: the live
        // network advertises a stale second instance that never resolves, and
        // failing on it would abandon a browse that is about to succeed.
        guard !finished, services.allSatisfy({ $0 === sender || $0.port > 0 }) else { return }
        onFailure?("Could not resolve UHC on the local network.")
    }
}
#endif

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
