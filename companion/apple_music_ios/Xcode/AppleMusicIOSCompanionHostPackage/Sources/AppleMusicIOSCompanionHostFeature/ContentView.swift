import AppleMusicIOSCompanion
import SwiftUI

@MainActor
private final class CompanionModel: ObservableObject {
    @Published private(set) var status = "Not authorized"
    @Published var uhcURL = "http://127.0.0.1:18088"
    @Published var bridgeID = ""
    @Published var pairingCode = ""
    @Published private(set) var isPaired = false
    private let host: AppleMusicCompanionHost
    private let player = SystemMusicPlayerCompanion()
    private var pollTask: Task<Void, Never>?

    init() {
        let store = KeychainAppleMusicCompanionInstallationStore()
        let installation = store.load() ?? AppleMusicCompanionInstallation(
            baseURL: URL(string: "http://127.0.0.1:18088")!, companionID: "ios-companion")
        uhcURL = installation.baseURL.absoluteString
        host = AppleMusicCompanionHost(installation: installation, store: store)
        bridgeID = installation.bridgeID ?? ""
        isPaired = installation.accessToken != nil
        status = isPaired ? "Paired; waiting for snapshots" : "Not authorized"
    }

    func authorize() { Task { @MainActor in do { _ = try await host.authorize(); status = "Authorized; enter a UHC pairing code" } catch { status = error.localizedDescription } } }
    func claim() {
        guard !bridgeID.isEmpty, !pairingCode.isEmpty else { status = "Enter the bridge ID and pairing code"; return }
        Task { @MainActor in do { _ = try await host.claim(bridgeID: bridgeID, pairingCode: pairingCode); isPaired = true; status = "Paired; waiting for snapshots"; startPolling() } catch { status = error.localizedDescription } }
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
    func revoke() { Task { @MainActor in do { try await host.revoke(); isPaired = false; stopPolling(); status = "Revoked" } catch { status = error.localizedDescription } } }
}

public struct ContentView: View {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var model = CompanionModel()
    public init() {}
    public var body: some View {
        Form {
            Section("Apple Music") {
                Text("Controls this iPhone's SystemMusicPlayer session. Apple credentials stay on this device.").font(.footnote).foregroundStyle(.secondary)
                Text(model.status).font(.footnote)
                Button("Authorize Apple Music", action: model.authorize)
            }
            Section("Pair with UHC") {
                TextField("UHC server URL", text: $model.uhcURL).textInputAutocapitalization(.never).autocorrectionDisabled()
                TextField("UHC bridge ID", text: $model.bridgeID).textInputAutocapitalization(.never).autocorrectionDisabled()
                TextField("Short-lived pairing code", text: $model.pairingCode).textInputAutocapitalization(.never).autocorrectionDisabled()
                Button("Claim this companion", action: model.claim).disabled(model.bridgeID.isEmpty || model.pairingCode.isEmpty)
                if model.isPaired { Button("Revoke pairing", role: .destructive, action: model.revoke) }
            }
        }
        .onChange(of: scenePhase) { _, phase in if phase == .active { model.startPolling() } else { model.stopPolling() } }
    }
}
