import AppleMusicCompanion
import MusicKit
import SwiftUI

@MainActor
private final class CompanionModel: ObservableObject {
    @Published private(set) var status = "Not authorized"
    @Published var bridgeID = ""
    @Published var pairingCodeInput = ""
    @Published private(set) var isPaired = false
    private let host: MacAppleMusicCompanionHost
    private let player: ApplicationMusicPlayerCompanion
    private var pollTask: Task<Void, Never>?

    init() {
        let store = KeychainAppleMusicCompanionInstallationStore()
        let installation = store.load() ?? AppleMusicCompanionInstallation(
            baseURL: URL(string: "https://uhc.example.invalid")!, companionID: "replace-from-keychain")
        host = MacAppleMusicCompanionHost(installation: installation, store: store)
        player = ApplicationMusicPlayerCompanion(companionID: installation.companionID)
        bridgeID = installation.bridgeID ?? ""
        isPaired = installation.accessToken != nil
        status = isPaired ? "Paired; waiting for snapshots" : "Not authorized"
    }

    func authorize() {
        Task { @MainActor in
            let result = await host.authorize()
            status = result == .authorized ? "Authorized; enter a UHC pairing code" : "Apple Music authorization status: \(result)"
        }
    }

    func claim() {
        guard !bridgeID.isEmpty, !pairingCodeInput.isEmpty else { status = "Enter the bridge ID and pairing code"; return }
        let bridgeID = bridgeID, pairingCode = pairingCodeInput
        Task { @MainActor in
            do {
                try await host.claim(bridgeID: bridgeID, pairingCode: pairingCode)
                isPaired = true; status = "Paired; waiting for snapshots"; startPolling()
            } catch { status = error.localizedDescription }
        }
    }

    func startPolling() {
        guard isPaired else { return }
        pollTask?.cancel()
        pollTask = Task { [host, player] in
            while !Task.isCancelled {
                do {
                    try await host.publishCurrentSnapshot(from: player)
                    try await host.pollAndHandle { command in try await player.execute(command) }
                    try await host.pollAndHandleContent { request in try await player.executeContent(request) }
                } catch { await MainActor.run { self.status = error.localizedDescription } }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    func stopPolling() { pollTask?.cancel(); pollTask = nil }
    func revoke() {
        Task { @MainActor in
            do { try await host.revoke(); isPaired = false; stopPolling(); status = "Revoked" }
            catch { status = error.localizedDescription }
        }
    }
}

public struct ContentView: View {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var model = CompanionModel()
    public init() {}
    public var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Apple Music Companion").font(.title2)
            Text("Controls this Mac's app-private Apple Music session; it does not automate Music.app.").font(.callout).foregroundStyle(.secondary)
            Text(model.status).font(.footnote)
            Button("Authorize Apple Music", action: model.authorize)
            TextField("UHC bridge ID", text: $model.bridgeID)
            TextField("Short-lived pairing code", text: $model.pairingCodeInput)
            Button("Claim this companion", action: model.claim).disabled(model.bridgeID.isEmpty || model.pairingCodeInput.isEmpty)
            if model.isPaired { Button("Revoke pairing", role: .destructive, action: model.revoke) }
        }
        .textFieldStyle(.roundedBorder).padding(24).frame(minWidth: 460)
        .onChange(of: scenePhase) { phase in if phase == .active { model.startPolling() } else { model.stopPolling() } }
    }
}
