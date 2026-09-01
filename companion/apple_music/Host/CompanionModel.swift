import AppleMusicCompanion
import MusicKit
import SwiftUI

/// Minimal signed-host model for the macOS companion scaffold.
///
/// The model deliberately drives only ApplicationMusicPlayer. It does not
/// claim to automate Music.app or select AirPlay destinations.
@MainActor
final class CompanionModel: ObservableObject {
    @Published private(set) var status = "Not authorized"
    @Published var bridgeID = ""
    @Published var pairingCodeInput = ""
    @Published private(set) var isPaired = false

    private let host: MacAppleMusicCompanionHost
    private let player: ApplicationMusicPlayerCompanion
    private var pollTask: Task<Void, Never>?

    init(
        baseURL: URL,
        companionID: String,
        installationStore: any AppleMusicCompanionInstallationStore = KeychainAppleMusicCompanionInstallationStore()
    ) {
        let installation = installationStore.load()
            ?? AppleMusicCompanionInstallation(baseURL: baseURL, companionID: companionID)
        host = MacAppleMusicCompanionHost(installation: installation, store: installationStore)
        player = ApplicationMusicPlayerCompanion(companionID: installation.companionID)
        bridgeID = installation.bridgeID ?? ""
        isPaired = installation.accessToken != nil
        status = isPaired ? "Paired; waiting for snapshots" : "Not authorized"
        if isPaired {
            startPolling()
        }
    }

    func authorize() {
        Task { @MainActor in
            let result = await host.authorize()
            status = result == .authorized
                ? "Authorized; enter a UHC pairing code"
                : "Apple Music authorization status: \(result)"
        }
    }

    func claim() {
        let bridgeID = bridgeID
        let pairingCode = pairingCodeInput
        guard !bridgeID.isEmpty, !pairingCode.isEmpty else {
            status = "Enter the bridge ID and pairing code"
            return
        }
        Task { @MainActor in
            do {
                try await host.claim(bridgeID: bridgeID, pairingCode: pairingCode)
                isPaired = true
                status = "Paired; waiting for snapshots"
                startPolling()
            } catch {
                status = error.localizedDescription
            }
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
                    try await host.publishCurrentSnapshot(from: player)
                    try await host.pollAndHandle { command in
                        try await player.execute(command)
                    }
                    try await host.pollAndHandleContent { request in
                        try await player.executeContent(request)
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
            do {
                try await host.revoke()
                isPaired = false
                stopPolling()
                status = "Revoked"
            } catch {
                status = error.localizedDescription
            }
        }
    }
}
