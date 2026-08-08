import AppleMusicIOSCompanion
import AVKit
import SwiftUI

func newAppleMusicCompanionInstallation() -> AppleMusicCompanionInstallation {
    AppleMusicCompanionInstallation(
        baseURL: URL(string: "http://127.0.0.1:18088")!,
        companionID: "ios-\(UUID().uuidString.lowercased())")
}

/// The bridge deliberately keeps its diagnostic payloads detailed for logs and
/// support. The companion turns the only expected pairing failure into a
/// recovery action instead of making someone decipher HTTP or JSON.
func pairingRecoveryMessage(for error: Error) -> String? {
    guard case let BridgeClientError.httpStatus(status, detail) = error,
          status == 400,
          detail.contains("pairing_failed") || detail.localizedCaseInsensitiveContains("pairing code")
    else { return nil }

    return "That confirmation code has expired or was already used. Find UHC again to get a new code."
}

private enum CompanionStage: Equatable {
    case authorize
    case findUHC
    case findingUHC
    case confirmCode
    case confirming
    case connected

    var title: String {
        switch self {
        case .authorize: "Connect Apple Music"
        case .findUHC, .findingUHC: "Find UHC"
        case .confirmCode, .confirming: "Confirm pairing"
        case .connected: "Connected"
        }
    }
}

@MainActor
private final class CompanionModel: ObservableObject {
    @Published private(set) var stage: CompanionStage
    @Published private(set) var message: String?
    private let bridgeID: String
    @Published private(set) var pairingCode = ""
    @Published private(set) var isPaired = false
    @Published private(set) var isLive = false
    @Published private(set) var outputLabel: String?
    private let installationStore: KeychainAppleMusicCompanionInstallationStore
    private let companionID: String
    private var host: AppleMusicCompanionHost
    private let player = SystemMusicPlayerCompanion()
    private var heartbeatTask: Task<Void, Never>?
    private var commandTask: Task<Void, Never>?
    private var discovery: UHCBonjourDiscovery?

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
        host = AppleMusicCompanionHost(installation: installation, store: store)
        bridgeID = installation.bridgeID ?? companionID
        let alreadyPaired = installation.accessToken != nil
        isPaired = alreadyPaired
        stage = alreadyPaired ? .connected : .authorize
        message = alreadyPaired ? "Paired. Open this companion to connect to UHC." : nil
        if alreadyPaired { reconnect() }
    }

    func authorize() {
        message = nil
        Task { @MainActor in
            do {
                let authorization = try await host.authorize()
                guard authorization == .authorized else {
                    stage = .authorize
                    message = "Apple Music access was not granted. Allow access, then try again."
                    return
                }
                stage = .findUHC
            } catch {
                stage = .authorize
                message = "Apple Music could not be authorized. Try again."
            }
        }
    }

    func discover() {
        guard !bridgeID.isEmpty else {
            message = "This companion needs to be restarted before it can pair."
            return
        }
        pairingCode = ""
        message = nil
        stage = .findingUHC
        let browser = UHCBonjourDiscovery()
        discovery = browser
        browser.onFailure = { [weak self] message in
            Task { @MainActor in
                self?.stage = .findUHC
                self?.message = "UHC was not found on this local network. Make sure it is running, then try again."
            }
        }
        browser.onBaseURL = { [weak self] baseURL in
            Task { @MainActor in self?.requestPairing(at: baseURL) }
        }
        browser.start()
    }

    /// Restore the Keychain bearer, then use Bonjour to find UHC's current
    /// address after a server restart or network/OS change.
    private func reconnect() {
        let browser = UHCBonjourDiscovery()
        discovery = browser
        browser.onFailure = { [weak self] _ in
            Task { @MainActor in
                self?.isLive = false
                self?.message = "Paired, but UHC was not found on this local network. It will reconnect when available."
            }
        }
        browser.onBaseURL = { [weak self] baseURL in
            Task { @MainActor in self?.useReconnectedUHC(at: baseURL) }
        }
        browser.start()
    }

    private func useReconnectedUHC(at baseURL: URL) {
        discovery?.stop(); discovery = nil
        guard var installation = installationStore.load(), installation.accessToken != nil else { return }
        installation.baseURL = baseURL
        try? installationStore.save(installation)
        host = AppleMusicCompanionHost(installation: installation, store: installationStore)
        startPolling()
    }
    private func requestPairing(at baseURL: URL) {
        discovery?.stop(); discovery = nil
        host = AppleMusicCompanionHost(bridgeBaseURL: baseURL, companionID: companionID, store: installationStore)
        let discoverHost = host
        stage = .findingUHC
        Task { @MainActor in
            do {
                let pairing = try await discoverHost.discoverPairing(bridgeID: bridgeID)
                pairingCode = pairing.pairingCode
                stage = .confirmCode
            } catch {
                pairingCode = ""
                stage = .findUHC
                message = pairingRecoveryMessage(for: error) ?? "UHC was found, but it could not start pairing. Try again."
            }
        }
    }
    func confirm() {
        guard !bridgeID.isEmpty, !pairingCode.isEmpty else {
            stage = .findUHC
            message = "Find UHC again to get a confirmation code."
            return
        }
        message = nil
        stage = .confirming
        let claimHost = host
        Task { @MainActor in
            do {
                _ = try await claimHost.claim(bridgeID: bridgeID, pairingCode: pairingCode)
                isPaired = true
                isLive = false
                pairingCode = ""
                stage = .connected
                message = "Paired. Connecting to UHC…"
                startPolling()
            } catch {
                pairingCode = ""
                stage = .findUHC
                message = pairingRecoveryMessage(for: error) ?? "Pairing could not be completed. Find UHC again to retry."
            }
        }
    }
    func startPolling() {
        guard isPaired else { return }
        stopPolling()
        let host = host
        let player = player
        heartbeatTask = Task { [weak self, host, player] in
            while !Task.isCancelled {
                do {
                    try await host.publishCurrentSnapshot(from: player)
                    let outputLabel = await player.currentOutputLabel()
                    await MainActor.run {
                        self?.isLive = true
                        self?.outputLabel = outputLabel
                        self?.message = "UHC is connected and controlling this companion."
                    }
                } catch {
                    if !Task.isCancelled {
                        await MainActor.run {
                            if let bridgeError = error as? BridgeClientError, bridgeError.isAuthorizationFailure {
                                self?.isPaired = false
                                self?.stopPolling()
                                self?.isLive = false
                                self?.outputLabel = nil
                                self?.message = "UHC removed this pairing. Find UHC to pair again."
                                Task { try? await host.forgetAuthorization() }
                            } else {
                                self?.isLive = false
                                self?.outputLabel = nil
                                self?.message = "Paired, but UHC is temporarily unavailable. It will keep trying while this app is open."
                            }
                        }
                    }
                }
                try? await Task.sleep(for: .seconds(5))
            }
        }
        commandTask = Task { [host, player] in
            while !Task.isCancelled {
                do {
                    try await host.pollAndHandle { command in try await player.execute(command) }
                    try await host.pollAndHandleContent { request in try await player.executeContent(request) }
                } catch {
                    try? await Task.sleep(for: .seconds(5))
                }
            }
        }
    }
    func stopPolling() {
        heartbeatTask?.cancel()
        heartbeatTask = nil
        commandTask?.cancel()
        commandTask = nil
        isLive = false
        outputLabel = nil
    }
    func revoke() {
        Task { @MainActor in
            do {
                try await host.revoke()
                isPaired = false
                pairingCode = ""
                stopPolling()
                stage = .findUHC
                message = "Pairing was removed. Find UHC to connect again."
            } catch {
                message = "Pairing could not be removed. Try again."
            }
        }
    }
}

public struct ContentView: View {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var model = CompanionModel()
    public init() {}
    public var body: some View {
        NavigationStack {
            Form {
                Section {
                    Text("Use Apple Music here, then let UHC control this companion from the rest of your system.")
                        .foregroundStyle(.secondary)
                } header: {
                    Text(model.stage.title)
                }

                if let message = model.message {
                    Section {
                        Label(message, systemImage: model.isLive ? "checkmark.circle.fill" : "info.circle")
                            .foregroundStyle(model.isLive ? .green : .secondary)
                    }
                }

                switch model.stage {
                case .authorize:
                    Section {
                        Text("Apple Music stays authorized on this device.")
                            .foregroundStyle(.secondary)
                        Button("Authorize Apple Music", action: model.authorize)
                    }
                case .findUHC:
                    Section {
                        Text("Find the UHC server on this local network.")
                            .foregroundStyle(.secondary)
                        Button("Find UHC", action: model.discover)
                    }
                case .findingUHC:
                    Section {
                        HStack(spacing: 12) {
                            ProgressView()
                            Text("Looking for UHC on the local network…")
                        }
                    }
                case .confirmCode:
                    Section {
                        Text("Make sure UHC shows this same code.")
                            .foregroundStyle(.secondary)
                        Text(model.pairingCode)
                            .font(.system(.largeTitle, design: .monospaced).weight(.semibold))
                            .accessibilityLabel("Confirmation code \(model.pairingCode)")
                        Button("Confirm codes match", action: model.confirm)
                    }
                case .confirming:
                    Section {
                        HStack(spacing: 12) {
                            ProgressView()
                            Text("Confirming pairing…")
                        }
                    }
                case .connected:
                    Section("Apple Music") {
                        Text("Apple Music is ready on this device. UHC controls this companion’s playback session.")
                            .foregroundStyle(.secondary)
                        Label(
                            model.isLive ? "UHC live" : "Paired · waiting for UHC",
                            systemImage: model.isLive ? "dot.radiowaves.left.and.right" : "clock"
                        )
                        .foregroundStyle(model.isLive ? .green : .secondary)
                        if let outputLabel = model.outputLabel {
                            Label("Output: \(outputLabel)", systemImage: "hifispeaker.fill")
                        } else {
                            Label("Output: unavailable", systemImage: "hifispeaker.slash")
                                .foregroundStyle(.secondary)
                        }
                    }
                    Section("AirPlay output") {
                        Text("Choose an AirPlay speaker or HomePod in Apple’s route picker.")
                            .foregroundStyle(.secondary)
                        AirPlayRoutePicker()
                            .frame(minHeight: 44)
                    }
                    Section {
                        Button("Disconnect from UHC", role: .destructive, action: model.revoke)
                    }
                }
            }
            .navigationTitle("Apple Music")
        }
        .onChange(of: scenePhase) { _, phase in
            if phase == .active {
                model.startPolling()
            } else if phase == .background {
                model.stopPolling()
            }
        }
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
