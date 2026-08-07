import AppleMusicCompanion
import MusicKit
import SwiftUI

private enum CompanionStage: Equatable {
    case authorize, findUHC, findingUHC, confirmCode, confirming, reconnecting, connected
    var title: String {
        switch self {
        case .authorize: "Connect Apple Music"
        case .findUHC, .findingUHC, .reconnecting: "Reconnect to UHC"
        case .confirmCode, .confirming: "Confirm pairing"
        case .connected: "Connected"
        }
    }
}

@MainActor
private final class CompanionModel: ObservableObject {
    @Published private(set) var stage: CompanionStage
    @Published private(set) var message: String?
    @Published private(set) var pairingCode = ""
    @Published private(set) var isPaired = false
    private let store: KeychainAppleMusicCompanionInstallationStore
    private let companionID: String
    private var host: MacAppleMusicCompanionHost
    private let player: ApplicationMusicPlayerCompanion
    private var pollTask: Task<Void, Never>?
    private var recoveryTask: Task<Void, Never>?
    private var discovery: UHCBonjourDiscovery?

    init() {
        store = KeychainAppleMusicCompanionInstallationStore()
        let installation = store.load() ?? AppleMusicCompanionInstallation(
            baseURL: URL(string: "http://127.0.0.1:18088")!,
            companionID: "macos-\(UUID().uuidString.lowercased())")
        if store.load() == nil { try? store.save(installation) }
        companionID = installation.companionID
        host = MacAppleMusicCompanionHost(installation: installation, store: store)
        player = ApplicationMusicPlayerCompanion(companionID: installation.companionID)
        isPaired = installation.accessToken != nil
        stage = installation.accessToken != nil ? .reconnecting : .authorize
        message = installation.accessToken != nil ? "Paired; reconnecting to UHC…" : nil
        if installation.accessToken != nil { reconnect() }
    }

    func authorize() {
        message = nil
        Task { @MainActor in
            guard await host.authorize() == .authorized else {
                message = "Apple Music access was not granted. Allow access, then try again."
                return
            }
            stage = .findUHC
        }
    }

    func discover() {
        recoveryTask?.cancel(); recoveryTask = nil
        pairingCode = ""
        message = nil
        stage = .findingUHC
        let browser = UHCBonjourDiscovery()
        discovery = browser
        browser.onFailure = { [weak self] _ in
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

    /// A persisted bearer is sufficient to reconnect; Bonjour supplies the
    /// current UHC address after a server restart or network/OS change. A
    /// failed lookup is transient, so keep retrying without touching Keychain.
    private func reconnect() {
        guard isPaired else { return }
        recoveryTask?.cancel(); recoveryTask = nil
        stage = .reconnecting
        message = "Paired; looking for UHC on the local network…"
        let browser = UHCBonjourDiscovery()
        discovery = browser
        browser.onFailure = { [weak self] _ in
            Task { @MainActor in
                guard let self, self.isPaired else { return }
                self.discovery = nil
                self.message = "Paired; UHC is unavailable. Retrying automatically…"
                self.retryReconnect()
            }
        }
        browser.onBaseURL = { [weak self] baseURL in
            Task { @MainActor in self?.useReconnectedUHC(at: baseURL) }
        }
        browser.start()
    }

    private func retryReconnect() {
        recoveryTask?.cancel()
        recoveryTask = Task { @MainActor [weak self] in
            try? await Task.sleep(for: .seconds(5))
            guard let self, !Task.isCancelled, self.isPaired else { return }
            self.reconnect()
        }
    }

    func reconnectNow() {
        guard isPaired else { return }
        reconnect()
    }

    private func useReconnectedUHC(at baseURL: URL) {
        discovery?.stop(); discovery = nil
        recoveryTask?.cancel(); recoveryTask = nil
        guard var installation = store.load(), installation.accessToken != nil else { return }
        installation.baseURL = baseURL
        try? store.save(installation)
        host = MacAppleMusicCompanionHost(installation: installation, store: store)
        startPolling()
    }

    private func requestPairing(at baseURL: URL) {
        discovery?.stop()
        discovery = nil
        host = MacAppleMusicCompanionHost(bridgeBaseURL: baseURL, companionID: companionID, store: store)
        let pairingHost = host
        Task { @MainActor in
            do {
                let response = try await pairingHost.bridge.discoverPairing(bridgeID: companionID)
                pairingCode = response.pairingCode
                stage = .confirmCode
            } catch {
                stage = .findUHC
                message = "UHC was found, but it could not start pairing. Try again."
            }
        }
    }

    func confirm() {
        guard !pairingCode.isEmpty else { return }
        stage = .confirming
        let pairingHost = host
        let code = pairingCode
        Task { @MainActor in
            do {
                try await pairingHost.claim(bridgeID: companionID, pairingCode: code)
                isPaired = true
                pairingCode = ""
                stage = .connected
                message = "This companion is connected to UHC."
                startPolling()
            } catch {
                stage = .findUHC
                message = "Pairing could not be completed. Find UHC again to retry."
            }
        }
    }

    func startPolling() {
        guard isPaired else { return }
        recoveryTask?.cancel(); recoveryTask = nil
        pollTask?.cancel()
        pollTask = Task { [host, player] in
            while !Task.isCancelled {
                do {
                    try await host.publishCurrentSnapshot(from: player)
                    try await host.pollAndHandle { command in try await player.execute(command) }
                    try await host.pollAndHandleContent { request in try await player.executeContent(request) }
                    await MainActor.run {
                        self.stage = .connected
                        self.message = "This companion is connected to UHC."
                    }
                } catch {
                    await MainActor.run {
                        if let bridgeError = error as? MacBridgeError, bridgeError.isAuthorizationFailure {
                            self.handleAuthorizationFailure()
                        } else {
                            self.stage = .reconnecting
                            self.message = "Paired; UHC is temporarily unavailable. Reconnecting…"
                            self.retryReconnect()
                        }
                    }
                    if !(error is MacBridgeError && (error as? MacBridgeError)?.isAuthorizationFailure == true) { return }
                }
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }

    private func handleAuthorizationFailure() {
        stopPolling()
        recoveryTask?.cancel(); recoveryTask = nil
        let host = self.host
        isPaired = false
        stage = .findUHC
        message = "UHC no longer recognizes this pairing. Find UHC to pair again."
        Task { try? await host.forgetAuthorization() }
    }

    func stopPolling() { pollTask?.cancel(); pollTask = nil }
    func revoke() {
        Task { @MainActor in
            do {
                try await host.revoke()
                isPaired = false
                stage = .findUHC
                message = "Pairing was removed. Find UHC to connect again."
                stopPolling()
            } catch { message = "Pairing could not be removed. Try again." }
        }
    }
}

public struct ContentView: View {
    @Environment(\.scenePhase) private var scenePhase
    @StateObject private var model = CompanionModel()
    public init() {}
    public var body: some View {
        Form {
            Section { Text("Use Apple Music on this Mac, then let UHC control this companion from the rest of your system.").foregroundStyle(.secondary) } header: { Text(model.stage.title) }
            if let message = model.message {
                Section { Label(message, systemImage: model.stage == .connected ? "checkmark.circle.fill" : "info.circle") }
            }
            switch model.stage {
            case .authorize:
                Section { Text("Apple Music stays authorized on this Mac.").foregroundStyle(.secondary); Button("Authorize Apple Music", action: model.authorize) }
            case .findUHC:
                Section { Text("Find the UHC server on this local network.").foregroundStyle(.secondary); Button("Find UHC", action: model.discover) }
            case .findingUHC:
                Section { HStack { ProgressView(); Text("Looking for UHC on the local network…") } }
            case .confirmCode:
                Section {
                    Text("Make sure UHC shows this same code.").foregroundStyle(.secondary)
                    Text(model.pairingCode).font(.system(.largeTitle, design: .monospaced).weight(.semibold)).accessibilityLabel("Confirmation code \(model.pairingCode)")
                    Button("Confirm codes match", action: model.confirm)
                }
            case .confirming:
                Section { HStack { ProgressView(); Text("Confirming pairing…") } }
            case .reconnecting:
                Section {
                    HStack { ProgressView(); Text("Restoring the saved pairing…") }
                    Button("Check again", action: model.reconnectNow)
                }
            case .connected:
                Section("Apple Music") { Text("Apple Music is ready on this Mac. UHC controls this companion’s playback session.").foregroundStyle(.secondary) }
                Section { Button("Disconnect from UHC", role: .destructive, action: model.revoke) }
            }
        }
        .formStyle(.grouped)
        .padding(24)
        .frame(minWidth: 460)
        .onChange(of: scenePhase) { _, phase in if phase == .active { model.startPolling() } else { model.stopPolling() } }
        .task { model.startPolling() }
    }
}
