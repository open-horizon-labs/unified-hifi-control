import Foundation

/// Discovers UHC servers advertising the native companion endpoint on the
/// local network. The resolved service hostname is preferred over TXT: a
/// server may be configured with a loopback or container hostname in its
/// advertised base URL, while NetService has already resolved a LAN-reachable
/// name for this phone.
final class UHCBonjourDiscovery: NSObject, NetServiceBrowserDelegate, NetServiceDelegate {
    var onBaseURL: ((URL) -> Void)?
    var onFailure: ((String) -> Void)?

    private let browser = NetServiceBrowser()
    private var services: [NetService] = []
    private var finished = false
    private var timeoutWork: DispatchWorkItem?

    func start() {
        finished = false
        services.removeAll()
        browser.delegate = self
        browser.searchForServices(ofType: "_uhc._tcp.", inDomain: "local.")
        let timeout = DispatchWorkItem { [weak self] in
            guard let self, !self.finished else { return }
            self.finished = true
            self.stop()
            self.onFailure?("No UHC server was found on the local network.")
        }
        timeoutWork = timeout
        DispatchQueue.main.asyncAfter(deadline: .now() + 8, execute: timeout)
    }

    func stop() {
        timeoutWork?.cancel()
        timeoutWork = nil
        browser.stop()
        services.forEach { $0.stop() }
        services.removeAll()
    }

    func netServiceBrowser(_ browser: NetServiceBrowser, didFind service: NetService, moreComing: Bool) {
        guard !finished else { return }
        services.append(service)
        service.delegate = self
        service.resolve(withTimeout: 5)
    }

    func netServiceDidResolveAddress(_ sender: NetService) {
        guard !finished else { return }
        if let host = sender.hostName?.trimmingCharacters(in: CharacterSet(charactersIn: ".")),
           !host.isEmpty, sender.port > 0,
           let url = URL(string: "http://\(host):\(sender.port)") {
            finish(url)
            return
        }

        // Fall back to the explicit base URL for proxies/tunnels where the
        // service hostname is not usable as an HTTP host.
        let txt = sender.txtRecordData().map(NetService.dictionary(fromTXTRecord:)) ?? [:]
        if let raw = txt["base"].flatMap({ String(data: $0, encoding: .utf8) }),
           let url = URL(string: raw), url.host != nil {
            finish(url)
        }
    }

    func netService(_ sender: NetService, didNotResolve errorDict: [String: NSNumber]) {
        guard !finished, services.allSatisfy({ $0 === sender || $0.port > 0 }) else { return }
        onFailure?("Could not resolve UHC on the local network.")
    }

    func netServiceBrowser(_ browser: NetServiceBrowser, didNotSearch errorDict: [String: NSNumber]) {
        onFailure?("Could not search for UHC on the local network.")
    }

    private func finish(_ url: URL) {
        finished = true
        stop()
        onBaseURL?(url)
    }
}
