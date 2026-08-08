import Foundation
import AppleMusicCompanion

/// Discovers the same AirPlay receivers the system picker can present.
/// Selection remains owned by Apple's native picker; this inventory is sent
/// back to UHC so it can expose concurrent output projections.
@MainActor
final class AirPlayOutputDiscovery: NSObject, NetServiceBrowserDelegate, NetServiceDelegate {
    var onOutputs: (([MacMusicKitOutput]) -> Void)?

    private let browser = NetServiceBrowser()
    private var services: [String: NetService] = [:]
    private var outputs: [String: MacMusicKitOutput] = [:]

    func start() {
        browser.delegate = self
        browser.searchForServices(ofType: "_airplay._tcp.", inDomain: "local.")
    }

    func stop() {
        browser.stop()
        services.values.forEach { $0.stop() }
        services.removeAll()
        outputs.removeAll()
        emit()
    }

    nonisolated func netServiceBrowser(
        _ browser: NetServiceBrowser,
        didFind service: NetService,
        moreComing: Bool
    ) {
        let name = service.name
        let domain = service.domain
        Task { @MainActor in
            let key = name + "." + domain
            let resolved = NetService(domain: domain, type: "_airplay._tcp.", name: name)
            self.services[key] = resolved
            resolved.delegate = self
            resolved.resolve(withTimeout: 5)
        }
    }

    nonisolated func netServiceBrowser(
        _ browser: NetServiceBrowser,
        didRemove service: NetService,
        moreComing: Bool
    ) {
        let name = service.name
        let domain = service.domain
        Task { @MainActor in
            let key = name + "." + domain
            self.services.removeValue(forKey: key)
            self.outputs = self.outputs.filter { _, output in
                output.displayName != name
            }
            self.emit()
        }
    }

    nonisolated func netServiceDidResolveAddress(_ sender: NetService) {
        let name = sender.name
        let txtData = sender.txtRecordData()
        Task { @MainActor in
            let txt = txtData.map(NetService.dictionary(fromTXTRecord:)) ?? [:]
            let outputID = txt["deviceid"]
                .flatMap { String(data: $0, encoding: .utf8) }
                ?? name
            self.outputs[outputID] = MacMusicKitOutput(
                outputID: Self.safeID(outputID),
                displayName: name
            )
            self.emit()
        }
    }

    nonisolated func netService(
        _ sender: NetService,
        didNotResolve errorDict: [String: NSNumber]
    ) {}

    nonisolated func netServiceBrowser(
        _ browser: NetServiceBrowser,
        didNotSearch errorDict: [String: NSNumber]
    ) {}

    private func emit() {
        onOutputs?(outputs.values.sorted { $0.displayName < $1.displayName })
    }

    private static func safeID(_ value: String) -> String {
        value.map { character in
            character.isASCII && (character.isLetter || character.isNumber || character == "-" || character == "_")
                ? String(character)
                : "_"
        }.joined()
    }
}
