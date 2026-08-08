import AppleMusicCompanion
import AVKit
import MusicKit
import SwiftUI

public enum CompanionStage: Equatable {
    case authorize, findUHC, findingUHC, confirmCode, confirming
    case reconnecting, pairedServerUnavailable, connectedSyncing, connected
    public var title: String {
        switch self {
        case .authorize: "Connect Apple Music"
        case .findUHC, .findingUHC: "Connect to the music-control server"
        case .confirmCode, .confirming: "Confirm pairing"
        case .reconnecting: "Restoring connection"
        case .pairedServerUnavailable: "Server unavailable"
        case .connectedSyncing: "Connected — syncing"
        case .connected: "Connected"
        }
    }

    public var isConnected: Bool {
        self == .connected || self == .connectedSyncing
    }

    public var statusIcon: String {
        switch self {
        case .connected: "checkmark.circle.fill"
        case .connectedSyncing: "arrow.triangle.2.circlepath"
        case .pairedServerUnavailable: "exclamationmark.triangle.fill"
        case .reconnecting, .findingUHC, .confirming: "arrow.triangle.2.circlepath"
        case .authorize, .findUHC, .confirmCode: "music.note"
        }
    }
}

@MainActor
public final class CompanionModel: ObservableObject {
    @Published private(set) var stage: CompanionStage
    @Published private(set) var message: String?
    @Published private(set) var pairingCode = ""
    @Published private(set) var isPaired = false
    @Published private(set) var automaticRetryEnabled = true
    @Published private(set) var retrySecondsRemaining: Int?
    @Published private(set) var recoveryAttemptCount = 0
    @Published private(set) var lastConnectedAt: Date?
    @Published private(set) var connectedHost: String?
    @Published var showRevokeConfirmation = false
    @Published private(set) var outputs: [MacMusicKitOutput] = []
    private let store: KeychainAppleMusicCompanionInstallationStore
    private let companionID: String
    private var host: MacAppleMusicCompanionHost
    private let player: ApplicationMusicPlayerCompanion
    private var pollTask: Task<Void, Never>?
    private var recoveryTask: Task<Void, Never>?
    private var discovery: UHCBonjourDiscovery?
    private let airplayDiscovery = AirPlayOutputDiscovery()

    public init() {
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
        message = installation.accessToken != nil ? "Saved pairing found. Looking for the music-control server…" : nil
        airplayDiscovery.onOutputs = { [weak self] outputs in
            self?.outputs = outputs
        }
        airplayDiscovery.start()
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
        cancelDiscoveryWork()
        pairingCode = ""
        recoveryAttemptCount = 0
        message = nil
        stage = .findingUHC
        let browser = UHCBonjourDiscovery()
        discovery = browser
        browser.onFailure = { [weak self] _ in
            Task { @MainActor in
                self?.stage = .findUHC
                self?.message = "No music-control server was found on this local network. Make sure UHC is running on the same network, then try again."
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
        retrySecondsRemaining = nil
        stage = .reconnecting
        message = "Saved pairing found. Looking for UHC on the local network…"
        let browser = UHCBonjourDiscovery()
        discovery = browser
        browser.onFailure = { [weak self] _ in
            Task { @MainActor in
                guard let self, self.isPaired else { return }
                self.discovery = nil
                self.recoveryAttemptCount += 1
                self.stage = .pairedServerUnavailable
                self.message = "UHC was not found on this network. Your saved pairing is intact."
                if self.automaticRetryEnabled { self.retryReconnect() }
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
            guard let self else { return }
            for remaining in stride(from: 5, through: 1, by: -1) {
                guard !Task.isCancelled, self.isPaired, self.automaticRetryEnabled else { return }
                self.retrySecondsRemaining = remaining
                self.message = "UHC is unavailable. Retrying in \(remaining) second\(remaining == 1 ? "" : "s")…"
                try? await Task.sleep(for: .seconds(1))
            }
            guard !Task.isCancelled, self.isPaired, self.automaticRetryEnabled else { return }
            self.retrySecondsRemaining = nil
            self.stage = .reconnecting
            self.message = "Retrying the connection to UHC…"
            self.reconnect()
        }
    }

    func reconnectNow() {
        guard isPaired else { return }
        automaticRetryEnabled = true
        retrySecondsRemaining = nil
        reconnect()
    }

    func stopAutomaticRetry() {
        automaticRetryEnabled = false
        recoveryTask?.cancel(); recoveryTask = nil
        retrySecondsRemaining = nil
        if isPaired, !stage.isConnected {
            stage = .pairedServerUnavailable
            message = "Automatic retry is paused. UHC pairing is still saved."
        }
    }

    func refreshOutputs() {
        airplayDiscovery.stop()
        airplayDiscovery.start()
    }

    func cancelDiscovery() {
        cancelDiscoveryWork()
        stage = .findUHC
        message = nil
    }

    private func useReconnectedUHC(at baseURL: URL) {
        discovery?.stop(); discovery = nil
        recoveryTask?.cancel(); recoveryTask = nil
        guard var installation = store.load(), installation.accessToken != nil else { return }
        installation.baseURL = baseURL
        try? store.save(installation)
        connectedHost = baseURL.host
        host = MacAppleMusicCompanionHost(installation: installation, store: store)
        startPolling()
    }

    private func requestPairing(at baseURL: URL) {
        discovery?.stop()
        discovery = nil
        connectedHost = baseURL.host
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
                message = "UHC is connected and syncing Apple Music on this Mac."
                lastConnectedAt = Date()
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
        pollTask = Task { @MainActor [host, player] in
            while !Task.isCancelled {
                do {
                    try await host.publishCurrentSnapshot(from: player, outputs: self.outputs)
                    try await host.pollAndHandle { command in try await player.execute(command) }
                    try await host.pollAndHandleContent { request in try await player.executeContent(request) }
                    await MainActor.run {
                        self.stage = .connected
                        self.message = "UHC is connected and syncing Apple Music on this Mac."
                        self.lastConnectedAt = Date()
                        self.recoveryAttemptCount = 0
                    }
                } catch {
                    await MainActor.run {
                        if let bridgeError = error as? MacBridgeError, bridgeError.isAuthorizationFailure {
                            self.handleAuthorizationFailure()
                        } else {
                            self.recoveryAttemptCount += 1
                            self.stage = .pairedServerUnavailable
                            self.message = "UHC is not responding. Your saved pairing is intact."
                            if self.automaticRetryEnabled { self.retryReconnect() }
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
    func requestRevoke() { showRevokeConfirmation = true }
    func revoke() {
        showRevokeConfirmation = false
        Task { @MainActor in
            do {
                try await host.revoke()
                isPaired = false
                stage = .findUHC
                message = "Pairing was removed. Find the music-control server to connect again."
                stopPolling()
            } catch { message = "Pairing could not be removed. Try again." }
        }
    }

    private func cancelDiscoveryWork() {
        recoveryTask?.cancel(); recoveryTask = nil
        retrySecondsRemaining = nil
        discovery?.stop(); discovery = nil
    }
}

public struct ContentView: View {
    @ObservedObject private var model: CompanionModel

    public init(model: CompanionModel) {
        self.model = model
    }
    public var body: some View {
        Form {
            Section {
                Text(model.stage.isConnected ? "Apple Music is ready for UHC control." : "Let UHC control Apple Music on this Mac.")
                    .font(.headline)
                Text("UHC, the music-control server, keeps Apple Music on this Mac and sends playback commands here.")
                    .foregroundStyle(.secondary)
            } header: { Text(model.stage.title) }
            if !model.stage.isConnected {
                SetupProgressView(stage: model.stage)
            }
            if let message = model.message, !model.stage.isConnected, model.stage != .pairedServerUnavailable {
                Section { Label(message, systemImage: messageIcon) }
            }
            switch model.stage {
            case .authorize:
                Section {
                    Text("Step 1 of 4: allow Apple Music access. Your music stays on this Mac; UHC only sends playback commands.").foregroundStyle(.secondary)
                    ContextualHelpView(title: "About Apple Music access", text: "The companion needs permission to read playback state and send commands. UHC does not receive your Apple Music credentials or library.")
                    Button("Allow Apple Music access", action: model.authorize)
                }
            case .findUHC:
                Section {
                    Text("Step 2 of 4: find UHC on the same local network. UHC will show a matching pairing code before this Mac is connected.").foregroundStyle(.secondary)
                    ContextualHelpView(title: "About UHC", text: "UHC is the music-control server running on your network. This companion lets it control Apple Music on this Mac.")
                    Button("Find UHC on this network", action: model.discover)
                }
            case .findingUHC:
                Section {
                    HStack { ProgressView(); Text("Looking for UHC on the local network…") }
                    Button("Stop searching", action: model.cancelDiscovery)
                }
            case .confirmCode:
                Section {
                    Text("Step 3 of 4: in UHC, open the Apple Music companion pairing screen and make sure it shows this same code. This confirms that you trust this Mac.").foregroundStyle(.secondary)
                    Text(model.pairingCode).font(.system(.largeTitle, design: .monospaced).weight(.semibold)).accessibilityLabel("Confirmation code \(model.pairingCode)")
                    ContextualHelpView(title: "Why the code matters", text: "Only confirm when the code matches in both places. This prevents another device on the network from controlling Apple Music here.")
                    Button("Confirm codes match", action: model.confirm)
                    Button("Start over with a new code", action: model.discover)
                }
            case .confirming:
                Section { HStack { ProgressView(); Text("Confirming pairing…") } }
            case .reconnecting:
                Section {
                    HStack { ProgressView(); Text("Restoring the saved pairing…") }
                    Button("Retry now", action: model.reconnectNow)
                    if model.automaticRetryEnabled { Button("Pause automatic retry", action: model.stopAutomaticRetry) }
                }
                connectionDetails
            case .pairedServerUnavailable:
                Section {
                    Text(model.message ?? "UHC is unavailable. Your saved pairing is intact.")
                        .foregroundStyle(.secondary)
                    Button("Retry now", action: model.reconnectNow)
                    if model.automaticRetryEnabled {
                        HStack {
                            ProgressView()
                            Text(model.retrySecondsRemaining.map { "Retrying in \($0) second\($0 == 1 ? "" : "s")…" } ?? "Automatic retry is on…")
                                .foregroundStyle(.secondary)
                        }
                        Button("Pause automatic retry", action: model.stopAutomaticRetry)
                    } else {
                        Text("Automatic retry is paused. Pairing is still saved.")
                            .foregroundStyle(.secondary)
                    }
                    if model.recoveryAttemptCount >= 3 {
                        RecoveryChecklistView()
                    }
                }
                connectionDetails
            case .connectedSyncing:
                Section { HStack { ProgressView(); Text("Syncing Apple Music playback with UHC…") } }
                connectionDetails
            case .connected:
                ConnectedHeroView(host: model.connectedHost, lastConnectedAt: model.lastConnectedAt, outputs: model.outputs)
                connectionDetails
                Section("AirPlay outputs") {
                    Text(model.outputs.isEmpty ? "No AirPlay outputs found. Make sure the speakers are on the same network, then refresh." : "Choose where Apple Music plays with the native AirPlay picker. macOS manages the active route; the list below is only an inventory of available outputs.")
                        .foregroundStyle(.secondary)
                    MacAirPlayRoutePicker()
                        .frame(minHeight: 44)
                    if model.outputs.isEmpty {
                        Button("Refresh outputs", action: model.refreshOutputs)
                    } else {
                        Text("Available AirPlay outputs (\(model.outputs.count))")
                            .font(.headline)
                        ForEach(model.outputs, id: \.outputID) { output in
                            Label(output.displayName, systemImage: "hifispeaker")
                                .foregroundStyle(.secondary)
                        }
                    }
                }
                Section { Button("Remove pairing…", role: .destructive, action: model.requestRevoke) }
            }
        }
        .formStyle(.grouped)
        .padding(24)
        .frame(minWidth: 460, minHeight: 420)
        .confirmationDialog("Remove saved pairing?", isPresented: $model.showRevokeConfirmation) {
            Button("Remove pairing", role: .destructive, action: model.revoke)
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("UHC will no longer be able to control Apple Music on this Mac until you pair again.")
        }
    }

    private var messageIcon: String {
        if model.stage.isConnected { return "checkmark.circle.fill" }
        if model.stage == .pairedServerUnavailable { return "exclamationmark.triangle.fill" }
        return "info.circle"
    }

    @ViewBuilder
    private var connectionDetails: some View {
        DisclosureGroup("Connection details") {
            if let host = model.connectedHost {
                Label("UHC server: \(host)", systemImage: "network")
            }
            if let date = model.lastConnectedAt {
                Text("Last connected: \(date, style: .relative)")
                    .foregroundStyle(.secondary)
            }
        }
    }
}

private struct ConnectedHeroView: View {
    let host: String?
    let lastConnectedAt: Date?
    let outputs: [MacMusicKitOutput]

    var body: some View {
        Section {
            HStack(alignment: .top, spacing: 12) {
                Image(systemName: "checkmark.circle.fill")
                    .font(.title2)
                    .foregroundStyle(.green)
                VStack(alignment: .leading, spacing: 4) {
                    Text("Apple Music is ready")
                        .font(.headline)
                    Text("UHC can send playback commands to Apple Music on this Mac.")
                        .foregroundStyle(.secondary)
                    if let host {
                        Text("Connected to \(host)")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            Label(
                outputs.isEmpty ? "No AirPlay outputs detected" : "AirPlay outputs available: \(outputs.count)",
                systemImage: outputs.isEmpty ? "hifispeaker.slash" : "hifispeaker"
            )
            .foregroundStyle(outputs.isEmpty ? .secondary : .primary)
            if let lastConnectedAt {
                Text("Last connected \(lastConnectedAt, style: .relative)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        } header: {
            Text("Ready")
        }
    }
}

private struct ContextualHelpView: View {
    let title: String
    let text: String

    var body: some View {
        DisclosureGroup(title) {
            Text(text)
                .font(.caption)
                .foregroundStyle(.secondary)
                .padding(.top, 2)
        }
        .font(.caption)
    }
}

private struct RecoveryChecklistView: View {
    var body: some View {
        DisclosureGroup("What to check") {
            VStack(alignment: .leading, spacing: 6) {
                Label("UHC is running and visible on the same network.", systemImage: "1.circle")
                Label("This Mac and UHC are not blocked by a firewall.", systemImage: "2.circle")
                Label("The saved pairing still appears in UHC.", systemImage: "3.circle")
            }
            .font(.caption)
            .foregroundStyle(.secondary)
        }
    }
}

private struct SetupProgressView: View {
    let stage: CompanionStage

    private let steps = ["Apple Music", "Find UHC", "Pair", "Ready"]

    private var currentStep: Int {
        switch stage {
        case .authorize: 1
        case .findUHC, .findingUHC: 2
        case .confirmCode, .confirming: 3
        case .reconnecting, .pairedServerUnavailable, .connectedSyncing, .connected: 4
        }
    }

    var body: some View {
        Section("Setup") {
            HStack(spacing: 8) {
                ForEach(Array(steps.enumerated()), id: \.offset) { index, step in
                    if index > 0 {
                        Rectangle()
                            .fill(index < currentStep ? Color.accentColor : Color.secondary.opacity(0.25))
                            .frame(height: 1)
                    }
                    VStack(spacing: 4) {
                        Image(systemName: index + 1 < currentStep ? "checkmark.circle.fill" : index + 1 == currentStep ? "circle.fill" : "circle")
                            .foregroundStyle(index + 1 <= currentStep ? Color.accentColor : Color.secondary)
                        Text(step)
                            .font(.caption2)
                            .foregroundStyle(index + 1 == currentStep ? .primary : .secondary)
                            .lineLimit(1)
                    }
                    .frame(maxWidth: .infinity)
                }
            }
            .accessibilityElement(children: .ignore)
            .accessibilityLabel("Setup step \(currentStep) of \(steps.count): \(steps[currentStep - 1])")
        }
    }
}

public struct MenuBarContentView: View {
    @ObservedObject private var model: CompanionModel
    @Environment(\.openWindow) private var openWindow

    public init(model: CompanionModel) {
        self.model = model
    }

    public var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Label(model.stage.isConnected ? "Apple Music ready" : model.stage.title, systemImage: model.stage.statusIcon)
                .font(.headline)

            if let message = model.message, !model.stage.isConnected {
                Text(message)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(3)
            }

            if model.stage.isConnected {
                Label(
                    model.outputs.isEmpty ? "AirPlay: no outputs found" : "AirPlay: route managed by macOS",
                    systemImage: model.outputs.isEmpty ? "hifispeaker.slash" : "hifispeaker"
                )
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                MacAirPlayRoutePicker()
                    .frame(height: 30)
            }

            Divider()

            Button(openWindowTitle) {
                openWindow(id: "main")
            }

            if model.stage == .pairedServerUnavailable || model.stage == .reconnecting {
                Button("Retry now") {
                    model.reconnectNow()
                }
                if model.automaticRetryEnabled {
                    Button("Pause automatic retry") { model.stopAutomaticRetry() }
                }
            }

            Button("Quit Apple Music Companion") {
                NSApplication.shared.terminate(nil)
            }
        }
        .padding(12)
        .frame(width: 260)
    }

    private var openWindowTitle: String {
        switch model.stage {
        case .authorize: "Open to allow Apple Music access"
        case .findUHC, .findingUHC: "Open to find UHC"
        case .confirmCode, .confirming: "Open to confirm pairing"
        case .reconnecting, .pairedServerUnavailable: "Open recovery"
        case .connectedSyncing, .connected: "Open Companion…"
        }
    }
}

private struct MacAirPlayRoutePicker: NSViewRepresentable {
    func makeNSView(context: Context) -> AVRoutePickerView {
        let picker = AVRoutePickerView()
        picker.isRoutePickerButtonBordered = true
        picker.player = AVPlayer()
        picker.setAccessibilityLabel("Choose AirPlay output")
        return picker
    }

    func updateNSView(_ picker: AVRoutePickerView, context: Context) {}
}
