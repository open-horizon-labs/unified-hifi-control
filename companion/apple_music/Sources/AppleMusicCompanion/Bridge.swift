import Foundation
import MusicKit

/// Transport command shape shared with the existing UHC Apple bridge routes.
@available(macOS 14.0, *)
public enum MacMusicKitCommand: Codable, Sendable, Equatable {
    case play, pause, toggle, stop, next, previous
    case setVolume(value: Float), adjustVolume(delta: Float), setMute(muted: Bool)

    private enum CodingKeys: String, CodingKey { case command, value, delta, muted }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        switch try values.decode(String.self, forKey: .command) {
        case "play": self = .play
        case "pause": self = .pause
        case "toggle": self = .toggle
        case "stop": self = .stop
        case "next": self = .next
        case "previous": self = .previous
        case "set_volume": self = .setVolume(value: try values.decode(Float.self, forKey: .value))
        case "adjust_volume": self = .adjustVolume(delta: try values.decode(Float.self, forKey: .delta))
        case "set_mute": self = .setMute(muted: try values.decode(Bool.self, forKey: .muted))
        default: throw MacBridgeError.invalidResponse
        }
    }

    public func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .play: try values.encode("play", forKey: .command)
        case .pause: try values.encode("pause", forKey: .command)
        case .toggle: try values.encode("toggle", forKey: .command)
        case .stop: try values.encode("stop", forKey: .command)
        case .next: try values.encode("next", forKey: .command)
        case .previous: try values.encode("previous", forKey: .command)
        case let .setVolume(value):
            try values.encode("set_volume", forKey: .command); try values.encode(value, forKey: .value)
        case let .adjustVolume(delta):
            try values.encode("adjust_volume", forKey: .command); try values.encode(delta, forKey: .delta)
        case let .setMute(muted):
            try values.encode("set_mute", forKey: .command); try values.encode(muted, forKey: .muted)
        }
    }
}

@available(macOS 14.0, *)
public struct MacBridgeCommand: Codable, Sendable {
    public let commandID: String
    public let command: MacMusicKitCommand
    public let expiresAt: UInt64

    enum CodingKeys: String, CodingKey {
        case commandID = "command_id", command, expiresAt = "expires_at"
    }
}

@available(macOS 14.0, *)
public struct MacCommandAcknowledgement: Codable, Sendable {
    public let ok: Bool
    public let error: String?
}

@available(macOS 14.0, *)
public struct MacPairingResponse: Codable, Sendable {
    public let bridgeID: String
    public let pairingCode: String
    public let expiresAt: UInt64

    enum CodingKeys: String, CodingKey {
        case bridgeID = "bridge_id", pairingCode = "pairing_code", expiresAt = "expires_at"
    }
}

@available(macOS 14.0, *)
public struct MacClaimResponse: Codable, Sendable {
    public let bridgeID: String
    public let accessToken: String

    enum CodingKeys: String, CodingKey { case bridgeID = "bridge_id", accessToken = "access_token" }
}

/// Authenticated client for the existing Apple transport bridge. A signed host
/// may restore a paired bearer from its Keychain.
@available(macOS 14.0, *)
public actor MacAppleMusicBridgeClient {
    private let baseURL: URL
    private let session: URLSession
    private var accessToken: String?

    public init(baseURL: URL, accessToken: String? = nil, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.session = session
        self.accessToken = accessToken
    }

    public func currentAccessToken() -> String? { accessToken }

    public func pair(bridgeID: String) async throws -> MacPairingResponse {
        try await request(path: "api/bridges/applemusic/pair", method: "POST", body: ["bridge_id": bridgeID])
    }

    @discardableResult
    public func claim(bridgeID: String, pairingCode: String) async throws -> MacClaimResponse {
        let response: MacClaimResponse = try await request(path: "api/bridges/applemusic/claim", method: "POST", body: ["bridge_id": bridgeID, "pairing_code": pairingCode])
        accessToken = response.accessToken
        return response
    }

    public func publish(snapshot: MacMusicKitSnapshot) async throws {
        try await requestEmpty(path: "api/bridges/applemusic/state", method: "POST", body: snapshot)
    }

    public func pollCommands() async throws -> [MacBridgeCommand] {
        try await request(path: "api/bridges/applemusic/commands", method: "GET")
    }

    public func acknowledge(commandID: String, ok: Bool, error: String? = nil) async throws {
        try await requestEmpty(path: "api/bridges/applemusic/commands/\(commandID)", method: "POST", body: MacCommandAcknowledgement(ok: ok, error: error))
    }

    public func revoke() async throws {
        try await requestEmpty(path: "api/bridges/applemusic/revoke", method: "POST")
        accessToken = nil
    }

    private func request<T: Decodable, Body: Encodable>(path: String, method: String, body: Body? = nil) async throws -> T {
        let data = try await perform(path: path, method: method, body: body)
        do { return try JSONDecoder().decode(T.self, from: data) } catch { throw MacBridgeError.invalidResponse }
    }

    private func request<T: Decodable>(path: String, method: String) async throws -> T {
        try await request(path: path, method: method, body: Optional<String>.none)
    }

    private func requestEmpty<Body: Encodable>(path: String, method: String, body: Body? = nil) async throws {
        _ = try await perform(path: path, method: method, body: body)
    }

    private func perform<Body: Encodable>(path: String, method: String, body: Body? = nil) async throws -> Data {
        let url = path.split(separator: "/").reduce(baseURL) { $0.appendingPathComponent(String($1)) }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let accessToken { request.setValue("Bearer \(accessToken)", forHTTPHeaderField: "Authorization") }
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw MacBridgeError.invalidResponse }
        guard (200..<300).contains(http.statusCode) else {
            throw MacBridgeError.httpStatus(http.statusCode, String(data: data, encoding: .utf8) ?? "request failed")
        }
        return data
    }
}

@available(macOS 14.0, *)
public actor MacAppleMusicCompanionHost {
    private static let maxRememberedCommandOutcomes = 128
    public let companionID: String
    public let bridge: MacAppleMusicBridgeClient
    private let installation: AppleMusicCompanionInstallation
    private let installationStore: any AppleMusicCompanionInstallationStore
    private var commandOutcomes: [String: Bool] = [:]
    private var commandOutcomeOrder: [String] = []

    public init(bridgeBaseURL: URL, companionID: String) {
        precondition(!companionID.isEmpty && !companionID.contains(":"), "companionID must be owner-safe")
        self.companionID = companionID
        bridge = MacAppleMusicBridgeClient(baseURL: bridgeBaseURL)
        installation = AppleMusicCompanionInstallation(baseURL: bridgeBaseURL, companionID: companionID)
        installationStore = InMemoryAppleMusicCompanionInstallationStore()
    }

    /// Restore a paired installation from secure host storage. A host can
    /// inject the in-memory implementation in previews/tests.
    public init(
        installation: AppleMusicCompanionInstallation,
        store: any AppleMusicCompanionInstallationStore = KeychainAppleMusicCompanionInstallationStore()
    ) {
        precondition(!installation.companionID.isEmpty && !installation.companionID.contains(":"), "companionID must be owner-safe")
        self.companionID = installation.companionID
        self.installation = installation
        self.installationStore = store
        bridge = MacAppleMusicBridgeClient(baseURL: installation.baseURL, accessToken: installation.accessToken)
    }

    @discardableResult
    public func authorize() async -> MusicAuthorization.Status {
        await MusicAuthorization.request()
    }

    public func claim(bridgeID: String, pairingCode: String) async throws {
        let response = try await bridge.claim(bridgeID: bridgeID, pairingCode: pairingCode)
        var saved = installation
        saved.bridgeID = bridgeID
        saved.accessToken = response.accessToken
        try installationStore.save(saved)
    }

    public func publishCurrentSnapshot(from player: ApplicationMusicPlayerCompanion) async throws {
        try await bridge.publish(snapshot: player.snapshot())
    }

    /// Commands are at-least-once. The host must deduplicate command IDs before
    /// replaying non-idempotent operations after a process crash.
    public func pollAndHandle(_ handler: @Sendable (MacMusicKitCommand) async throws -> Void) async throws {
        for command in try await bridge.pollCommands() {
            if let priorOutcome = commandOutcomes[command.commandID] {
                try await bridge.acknowledge(commandID: command.commandID, ok: priorOutcome)
                continue
            }
            do {
                try await handler(command.command)
                remember(command.commandID, ok: true)
                try await bridge.acknowledge(commandID: command.commandID, ok: true)
            } catch {
                remember(command.commandID, ok: false)
                try await bridge.acknowledge(commandID: command.commandID, ok: false, error: String(describing: error))
            }
        }
    }

    private func remember(_ commandID: String, ok: Bool) {
        commandOutcomes[commandID] = ok
        commandOutcomeOrder.removeAll { $0 == commandID }
        commandOutcomeOrder.append(commandID)
        while commandOutcomeOrder.count > Self.maxRememberedCommandOutcomes {
            commandOutcomes.removeValue(forKey: commandOutcomeOrder.removeFirst())
        }
    }

    public func revoke() async throws {
        try await bridge.revoke()
        try installationStore.clear()
    }
}

@available(macOS 14.0, *)
public enum MacBridgeError: Error, LocalizedError, Sendable {
    case invalidResponse
    case httpStatus(Int, String)

    public var errorDescription: String? {
        switch self {
        case .invalidResponse: "UHC returned an invalid Mac Apple Music bridge response."
        case let .httpStatus(status, detail): "UHC Mac Apple Music bridge returned HTTP \(status): \(detail)"
        }
    }
}
