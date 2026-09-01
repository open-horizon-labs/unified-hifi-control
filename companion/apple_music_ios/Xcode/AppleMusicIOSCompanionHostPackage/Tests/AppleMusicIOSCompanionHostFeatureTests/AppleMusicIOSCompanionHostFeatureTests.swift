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

@Test func playlistTracksPaginationIsBoundedAndOffset() {
    #expect(Array(playlistEntryRange(limit: 50, offset: 3, count: 100)) == Array(3..<53))
    #expect(Array(playlistEntryRange(limit: 500, offset: 98, count: 100)) == Array(98..<100))
    #expect(playlistEntryRange(limit: 0, offset: 0, count: 10).isEmpty)
    #expect(playlistEntryRange(limit: 50, offset: 100, count: 100).isEmpty)
}

@Test func freshCompanionInstallationsHaveUniqueOwnerSafeIDs() {
    let first = newAppleMusicCompanionInstallation()
    let second = newAppleMusicCompanionInstallation()
    #expect(first.companionID.hasPrefix("ios-"))
    #expect(!first.companionID.contains(":"))
    #expect(first.companionID != second.companionID)
}

@Test func usedOrExpiredPairingCodeOffersAPlainLanguageRetry() {
    let error = BridgeClientError.httpStatus(
        400,
        #"{\"error\":\"pairing code is unknown or already used\",\"code\":\"pairing_failed\"}"#
    )

    #expect(pairingRecoveryMessage(for: error) == "That confirmation code has expired or was already used. Find UHC again to get a new code.")
}
