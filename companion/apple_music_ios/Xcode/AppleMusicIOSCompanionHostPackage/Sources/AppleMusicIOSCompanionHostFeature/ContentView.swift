import AppleMusicIOSCompanion
import AVKit
import SwiftUI

func newAppleMusicCompanionInstallation() -> AppleMusicCompanionInstallation {
    AppleMusicCompanionInstallation(
        baseURL: URL(string: "http://127.0.0.1:18088")!,
        companionID: "ios-\(UUID().uuidString.lowercased())")
}

@MainActor
private final class CompanionModel: ObservableObject {
    @Published private(set) var status = "Not authorized"
    @Published var uhcURL = "http://127.0.0.1:18088"
    @Published var bridgeID = ""
    @Published var pairingCode = ""
    @Published private(set) var isPaired = false
    private let installationStore: KeychainAppleMusicCompanionInstallationStore
    private let companionID: String
    private var host: AppleMusicCompanionHost
    private let player = SystemMusicPlayerCompanion()
    private var pollTask: Task<Void, Never>?

    init() {
        let store = KeychainAppleMusicCompanionInstallationStore()
        installationStore = store
        let installation: AppleMusicCompanionInstallation
        if let saved = store.load() {
            installation = saved
        } else {
            // The bridge identity is an installation identity, not a product
            // label. Persist a unique value before pairing so two iPhones do
            // not collapse into one owner/zone.
            let fresh = newAppleMusicCompanionInstallation()
            try? store.save(fresh)
            installation = fresh
        }
        companionID = installation.companionID
        uhcURL = installation.baseURL.absoluteString
        host = AppleMusicCompanionHost(installation: installation, store: store)
        bridgeID = installation.bridgeID ?? ""
        isPaired = installation.accessToken != nil
        status = isPaired ? "Paired; waiting for snapshots" : "Not authorized"
    }

    func authorize() { Task { @MainActor in do { _ = try await host.authorize(); status = "Authorized; enter a UHC pairing code" } catch { status = error.localizedDescription } } }
    func claim() {
        guard !bridgeID.isEmpty, !pairingCode.isEmpty else { status = "Enter the bridge ID and pairing code"; return }
        guard let baseURL = Self.validatedURL(uhcURL) else {
            status = "Enter a valid UHC URL (http:// or https://)"
            return
        }
        // The setup field is authoritative. Rebuild the bridge client before
        // claiming so a phone can pair to a LAN/tunnel UHC server instead of
        // silently sending the claim to the localhost default.
        host = AppleMusicCompanionHost(
            bridgeBaseURL: baseURL,
            companionID: companionID,
            store: installationStore
        )
        let claimHost = host
        Task { @MainActor in do { _ = try await claimHost.claim(bridgeID: bridgeID, pairingCode: pairingCode); isPaired = true; status = "Paired; waiting for snapshots"; startPolling() } catch { status = error.localizedDescription } }
    }
    private static func validatedURL(_ raw: String) -> URL? {
        guard let url = URL(string: raw.trimmingCharacters(in: .whitespacesAndNewlines)),
              let scheme = url.scheme?.lowercased(),
              ["http", "https"].contains(scheme),
              url.host != nil else { return nil }
        return url
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
            Section("AirPlay output") {
                Text("Choose an AirPlay speaker or HomePod in Apple's route picker. UHC will control the selected SystemMusicPlayer session; the selected route is owned by iOS and is not reported as a UHC zone until a companion observes it.")
                    .font(.footnote)
                    .foregroundStyle(.secondary)
                AirPlayRoutePicker()
                    .frame(minHeight: 44)
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
        .task { model.startPolling() }
    }
}

/// Native route selection deliberately stays outside the UHC bridge contract.
/// AVRoutePickerView is Apple's supported way to choose HomePod/AirPlay
/// destinations from an iPhone; UHC must not infer or persist route state from
/// the picker alone.
private struct AirPlayRoutePicker: UIViewRepresentable {
    func makeUIView(context: Context) -> AVRoutePickerView {
        let picker = AVRoutePickerView()
        picker.prioritizesVideoDevices = false
        picker.accessibilityLabel = "Choose AirPlay output"
        picker.accessibilityIdentifier = "airPlayRoutePicker"
        return picker
    }

    func updateUIView(_ picker: AVRoutePickerView, context: Context) {}
}
