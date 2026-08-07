import Foundation

/// Discovers UHC servers advertising the native companion endpoint on the
/// local network.  The server publishes its reachable base URL in TXT as
/// `base=<url>`; this avoids guessing an address (and, importantly, avoids
/// sending a pairing request to localhost on a phone).
final class UHCBonjourDiscovery: NSObject, NetServiceBrowserDelegate, NetServiceDelegate {
    var onBaseURL: ((URL) -> Void)?
    var onFailure: ((String) -> Void)?

    private let browser = NetServiceBrowser()
    private var services: [NetService] = []
    private var finished = false

    func start() {
        finished = false
        services.removeAll()
        browser.delegate = self
        browser.searchForServices(ofType: "_uhc._tcp.", inDomain: "local.")
    }

    func stop() {
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
        let txt = sender.txtRecordData().map(NetService.dictionary(fromTXTRecord:)) ?? [:]
        if let raw = txt["base"].flatMap({ String(data: $0, encoding: .utf8) }),
           let url = URL(string: raw), url.host != nil {
            finish(url)
            return
        }

        // Older UHC builds may not have the TXT URL.  A resolved service is
        // still unambiguous on the LAN, so derive the normal HTTP endpoint.
        if let host = sender.hostName?.trimmingCharacters(in: CharacterSet(charactersIn: ".")),
           !host.isEmpty, sender.port > 0,
           let url = URL(string: "http://\(host):\(sender.port)") {
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
