import Foundation
import MusicKit

/// The only authorization operation the host app needs from this package.
/// MusicKit keeps the credential/token lifecycle inside the signed app; this
/// package never returns or serializes either to UHC.
@available(iOS 17.0, *)
public enum AppleMusicAuthorization {
    public static var status: MusicAuthorization.Status {
        MusicAuthorization.currentStatus
    }

    @discardableResult
    public static func request() async -> MusicAuthorization.Status {
        await MusicAuthorization.request()
    }
}

public enum BridgeClientError: Error, LocalizedError, Sendable {
    case invalidResponse
    case httpStatus(Int, String)
    case notConfigured

    public var errorDescription: String? {
        switch self {
        case .invalidResponse:
            return "UHC returned an invalid Apple Music bridge response."
        case let .httpStatus(status, detail):
            return "UHC Apple Music bridge returned HTTP (status): (detail)"
        case .notConfigured:
            return "The Apple Music bridge has not been claimed by this companion."
        }
    }
}

public struct PairingResponse: Codable, Sendable {
    public let bridgeID: String
    public let pairingCode: String
    public let expiresAt: UInt64

    enum CodingKeys: String, CodingKey {
        case bridgeID = "bridge_id"
        case pairingCode = "pairing_code"
        case expiresAt = "expires_at"
    }
}

public struct ClaimResponse: Codable, Sendable {
    public let bridgeID: String
    public let accessToken: String

    enum CodingKeys: String, CodingKey {
        case bridgeID = "bridge_id"
        case accessToken = "access_token"
    }
}

public struct BridgeCommand: Codable, Sendable {
    public let commandID: String
    public let command: MusicKitWireCommand
    public let expiresAt: UInt64

    enum CodingKeys: String, CodingKey {
        case commandID = "command_id"
        case command
        case expiresAt = "expires_at"
    }
}

/// Codable command shape used by the existing UHC bridge contract.
public enum MusicKitWireCommand: Codable, Sendable, Equatable {
    case play
    case pause
    case toggle
    case stop
    case next
    case previous
    case setVolume(value: Float)
    case adjustVolume(delta: Float)
    case setMute(muted: Bool)

    private enum CodingKeys: String, CodingKey {
        case command
        case value
        case delta
        case muted
    }

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
        default: throw BridgeClientError.invalidResponse
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
            try values.encode("set_volume", forKey: .command)
            try values.encode(value, forKey: .value)
        case let .adjustVolume(delta):
            try values.encode("adjust_volume", forKey: .command)
            try values.encode(delta, forKey: .delta)
        case let .setMute(muted):
            try values.encode("set_mute", forKey: .command)
            try values.encode(muted, forKey: .muted)
        }
    }
}

/// Minimal authenticated client for the already-approved UHC bridge routes.
/// It stores the bearer only in this process; callers should place it in the
/// host app's Keychain if they need to survive app restarts.
@available(iOS 17.0, *)
public actor AppleMusicBridgeClient {
    private let baseURL: URL
    private let session: URLSession
    private var accessToken: String?

    public init(baseURL: URL, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.session = session
    }

    public func pair(bridgeID: String) async throws -> PairingResponse {
        try await request(
            path: "api/bridges/applemusic/pair",
            method: "POST",
            body: ["bridge_id": bridgeID]
        )
    }

    @discardableResult
    public func claim(bridgeID: String, pairingCode: String) async throws -> ClaimResponse {
        let response: ClaimResponse = try await request(
            path: "api/bridges/applemusic/claim",
            method: "POST",
            body: ["bridge_id": bridgeID, "pairing_code": pairingCode]
        )
        accessToken = response.accessToken
        return response
    }

    public func publish(snapshot: MusicKitSnapshotPayload) async throws {
        try await requestEmpty(path: "api/bridges/applemusic/state", method: "POST", body: snapshot)
    }

    public func pollCommands() async throws -> [BridgeCommand] {
        try await request(path: "api/bridges/applemusic/commands", method: "GET")
    }

    public func acknowledge(commandID: String, ok: Bool, error: String? = nil) async throws {
        try await requestEmpty(
            path: "api/bridges/applemusic/commands/\(commandID)",
            method: "POST",
            body: CommandAcknowledgement(ok: ok, error: error)
        )
    }

    public func revoke() async throws {
        try await requestEmpty(path: "api/bridges/applemusic/revoke", method: "POST")
        accessToken = nil
    }

    private func request<T: Decodable, Body: Encodable>(
        path: String,
        method: String,
        body: Body? = nil
    ) async throws -> T {
        let data = try await perform(path: path, method: method, body: body)
        do { return try JSONDecoder().decode(T.self, from: data) }
        catch { throw BridgeClientError.invalidResponse }
    }

    private func request<T: Decodable>(path: String, method: String) async throws -> T {
        try await request(path: path, method: method, body: Optional<String>.none)
    }

    private func requestEmpty<Body: Encodable>(
        path: String,
        method: String,
        body: Body? = nil
    ) async throws {
        _ = try await perform(path: path, method: method, body: body)
    }

    private func requestEmpty(path: String, method: String) async throws {
        _ = try await perform(path: path, method: method, body: Optional<String>.none)
    }

    private func perform<Body: Encodable>(
        path: String,
        method: String,
        body: Body? = nil
    ) async throws -> Data {
        let url = path.split(separator: "/").reduce(baseURL) { result, component in
            result.appendingPathComponent(String(component))
        }
        var request = URLRequest(url: url)
        request.httpMethod = method
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        if let accessToken { request.setValue("Bearer \(accessToken)", forHTTPHeaderField: "Authorization") }
        if let body {
            request.setValue("application/json", forHTTPHeaderField: "Content-Type")
            request.httpBody = try JSONEncoder().encode(body)
        }
        let (data, response) = try await session.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw BridgeClientError.invalidResponse }
        guard (200..<300).contains(http.statusCode) else {
            let detail = String(data: data, encoding: .utf8) ?? "request failed"
            throw BridgeClientError.httpStatus(http.statusCode, detail)
        }
        return data
    }
}

public struct CommandAcknowledgement: Codable, Sendable {
    public let ok: Bool
    public let error: String?
}

public struct MusicKitSnapshotPayload: Codable, Sendable {
    public let playerID: String
    public let displayName: String
    public let state: String
    public let track: MusicKitTrackPayload?
    public let volume: Float?
    public let isMuted: Bool

    public init(playerID: String, displayName: String, state: String,
                track: MusicKitTrackPayload? = nil, volume: Float? = nil,
                isMuted: Bool = false) {
        self.playerID = playerID
        self.displayName = displayName
        self.state = state
        self.track = track
        self.volume = volume
        self.isMuted = isMuted
    }

    enum CodingKeys: String, CodingKey {
        case playerID = "player_id"
        case displayName = "display_name"
        case state
        case track
        case volume
        case isMuted = "is_muted"
    }
}

public struct MusicKitTrackPayload: Codable, Sendable {
    public let title: String
    public let artist: String
    public let album: String
    public let artworkURL: String?
    public let positionSeconds: Double?
    public let durationSeconds: Double?

    public init(title: String, artist: String, album: String,
                artworkURL: String? = nil, positionSeconds: Double? = nil,
                durationSeconds: Double? = nil) {
        self.title = title
        self.artist = artist
        self.album = album
        self.artworkURL = artworkURL
        self.positionSeconds = positionSeconds
        self.durationSeconds = durationSeconds
    }

    enum CodingKeys: String, CodingKey {
        case title
        case artist
        case album
        case artworkURL = "artwork_url"
        case positionSeconds = "position_seconds"
        case durationSeconds = "duration_seconds"
    }
}

/// Native iPhone execution owner for the UHC Apple Music adapter.
///
/// The host app is responsible for requesting MusicKit authorization and for
/// projecting state into UHC's paired-companion contract. This package only
/// wraps the documented system player transport operations. It never exposes
/// Apple credentials, user tokens, or audio bytes to UHC.
@available(iOS 17.0, *)
public actor SystemMusicPlayerCompanion {
    public static let playerID = "system"

    private let player = SystemMusicPlayer.shared

    public init() {}

    public func play() async throws {
        try await player.play()
    }

    public func pause() {
        player.pause()
    }

    public func skipToNextItem() async throws {
        try await player.skipToNextEntry()
    }

    public func skipToPreviousItem() async throws {
        try await player.skipToPreviousEntry()
    }
}
