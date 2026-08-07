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

@Test @available(iOS 17.0, *) func systemPlayerQueueReadIsTruthfullyUnsupported() async throws {
    let request = try JSONDecoder().decode(
        MusicKitContentCommand.self,
        from: Data("""
        {"request_id":"queue-read","owner_id":"ios-companion","operation":"queue_read","params":{},"confirm":false,"expires_at":0}
        """.utf8)
    )
    let result = try await SystemMusicPlayerCompanion().executeContent(request)
    #expect(result.outcome == "unsupported")
    #expect(result.error?.code == "queue_visibility_unavailable")
}
