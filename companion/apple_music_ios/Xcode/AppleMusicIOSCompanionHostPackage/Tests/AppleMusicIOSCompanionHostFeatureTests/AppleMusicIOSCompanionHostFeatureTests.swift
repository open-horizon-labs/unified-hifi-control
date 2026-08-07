import Testing
import Foundation
import AppleMusicIOSCompanion
@testable import AppleMusicIOSCompanionHostFeature

@Test @available(iOS 17.0, *) func pairingInstallationPersistsOnlyBridgeState() throws {
    let store = InMemoryAppleMusicCompanionInstallationStore()
    var installation = AppleMusicCompanionInstallation(
        baseURL: URL(string: "https://uhc.example.test")!,
        companionID: "ios-companion"
    )
    installation.bridgeID = "bridge-1"
    installation.accessToken = "opaque-bridge-bearer"
    try store.save(installation)

    #expect(store.load() == installation)
    #expect(store.load()?.baseURL.absoluteString == "https://uhc.example.test")
    #expect(store.load()?.companionID == "ios-companion")
}
