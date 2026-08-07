import AppleMusicIOSCompanion
import SwiftUI

/// Minimal signed-host model. The host app supplies a stable companion ID
/// from Keychain and the HTTPS UHC base URL from its setup screen.
@MainActor
final class CompanionModel: ObservableObject {
    @Published private(set) var status = "Not authorized"
    @Published private(set) var pairingCode: String?
    @Published var bridgeID = ""
    @Published var pairingCodeInput = ""
    @Published private(set) var isPaired = false

    private let host: AppleMusicCompanionHost
    private let player = SystemMusicPlayerCompanion()
    private var pollTask: Task<Void, Never>?

    init(baseURL: URL, companionID: String) {
        host = AppleMusicCompanionHost(bridgeBaseURL: baseURL, companionID: companionID)
    }

    func authorize() {
        Task { @MainActor in
            do {
                _ = try await host.authorize()
                status = "Authorized; enter a UHC pairing code"
            } catch { status = error.localizedDescription }
        }
    }

    func claim(bridgeID: String, pairingCode: String) {
        Task { @MainActor in
            do {
                _ = try await host.claim(bridgeID: bridgeID, pairingCode: pairingCode)
                isPaired = true
                status = "Paired; waiting for snapshots"
                startPolling()
            } catch { status = error.localizedDescription }
        }
    }

    func beginPairing(bridgeID: String) {
        Task { @MainActor in
            do {
                let pairing = try await host.bridge.pair(bridgeID: bridgeID)
                pairingCode = pairing.pairingCode
                status = "Pairing code expires at \(pairing.expiresAt)"
            } catch { status = error.localizedDescription }
        }
    }

    func publish(snapshot: MusicKitSnapshotPayload) {
        Task { @MainActor in
            do { try await host.publish(snapshot: snapshot) }
            catch { status = error.localizedDescription }
        }
    }

    func startPolling() {
        guard isPaired else {
            status = "Claim this companion before polling"
            return
        }
        pollTask?.cancel()
        pollTask = Task { [host, player] in
            while !Task.isCancelled {
                do {
                    try await host.pollAndHandle { command in
                        try await player.execute(command)
                    }
                } catch {
                    await MainActor.run { self.status = error.localizedDescription }
                }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
    }

    func revoke() {
        Task { @MainActor in
            do { try await host.revoke(); isPaired = false; stopPolling(); status = "Revoked" }
            catch { status = error.localizedDescription }
        }
    }
}
