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
            return "UHC Apple Music bridge returned HTTP \(status): \(detail)"
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

public enum MusicKitJSONValue: Codable, Sendable, Equatable {
    case null
    case bool(Bool)
    case number(Double)
    case string(String)
    case array([MusicKitJSONValue])
    case object([String: MusicKitJSONValue])

    public init(from decoder: Decoder) throws {
        let container = try decoder.singleValueContainer()
        if container.decodeNil() { self = .null }
        else if let value = try? container.decode(Bool.self) { self = .bool(value) }
        else if let value = try? container.decode(Double.self) { self = .number(value) }
        else if let value = try? container.decode(String.self) { self = .string(value) }
        else if let value = try? container.decode([MusicKitJSONValue].self) { self = .array(value) }
        else { self = .object(try container.decode([String: MusicKitJSONValue].self)) }
    }

    public func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .null: try container.encodeNil()
        case let .bool(value): try container.encode(value)
        case let .number(value): try container.encode(value)
        case let .string(value): try container.encode(value)
        case let .array(value): try container.encode(value)
        case let .object(value): try container.encode(value)
        }
    }
}

public struct MusicKitContentCommand: Codable, Sendable {
    public let requestID: String
    public let ownerID: String
    public let operation: String
    public let params: [String: MusicKitJSONValue]
    public let idempotencyKey: String?
    public let precondition: MusicKitJSONValue?
    public let confirm: Bool
    public let expiresAt: UInt64

    enum CodingKeys: String, CodingKey {
        case requestID = "request_id"
        case ownerID = "owner_id"
        case operation
        case params
        case idempotencyKey = "idempotency_key"
        case precondition
        case confirm
        case expiresAt = "expires_at"
    }
}

public struct MusicKitContentError: Codable, Sendable, Equatable {
    public let code: String
    public let message: String
    public let retryable: Bool

    public init(code: String, message: String, retryable: Bool) {
        self.code = code
        self.message = message
        self.retryable = retryable
    }
}

public struct MusicKitContentResult: Codable, Sendable, Equatable {
    /// The bridge vocabulary is intentionally closed. Provider-specific
    /// errors must be translated before they are persisted or acknowledged.
    public static let allowedOutcomes: Set<String> = [
        "success", "unsupported", "unauthorized", "subscription_required",
        "restricted", "not_found", "offline", "rate_limited", "stale_owner",
        "conflict", "invalid", "failed"
    ]

    public let outcome: String
    public let data: MusicKitJSONValue?
    public let error: MusicKitContentError?

    public init(outcome: String, data: MusicKitJSONValue? = nil, error: MusicKitContentError? = nil) {
        self.outcome = outcome
        self.data = data
        self.error = error
    }

    /// Converts an unrecognized provider outcome into a durable, truthful
    /// bridge error instead of allowing an unknown string onto the wire.
    public static func validated(_ result: MusicKitContentResult) -> MusicKitContentResult {
        guard allowedOutcomes.contains(result.outcome) else {
            return MusicKitContentResult(
                outcome: "invalid",
                error: MusicKitContentError(
                    code: "invalid_outcome",
                    message: "Companion returned an unsupported content outcome.",
                    retryable: false
                )
            )
        }
        return result
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
/// A signed host may restore a paired bearer from its Keychain.
@available(iOS 17.0, *)
public actor AppleMusicBridgeClient {
    private let baseURL: URL
    private let session: URLSession
    private var accessToken: String?

    public init(baseURL: URL, accessToken: String? = nil, session: URLSession = .shared) {
        self.baseURL = baseURL
        self.session = session
        self.accessToken = accessToken
    }

    public func currentAccessToken() -> String? { accessToken }

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

    public func pollContent() async throws -> [MusicKitContentCommand] {
        try await request(path: "api/bridges/applemusic/content", method: "GET")
    }

    public func acknowledge(commandID: String, ok: Bool, error: String? = nil) async throws {
        try await requestEmpty(
            path: "api/bridges/applemusic/commands/\(commandID)",
            method: "POST",
            body: CommandAcknowledgement(ok: ok, error: error)
        )
    }

    public func acknowledgeContent(_ requestID: String, result: MusicKitContentResult) async throws {
        try await requestEmpty(
            path: "api/bridges/applemusic/content/\(requestID)",
            method: "POST",
            body: result
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

/// Host-app lifecycle coordinator for the signed iPhone companion.
///
/// The coordinator publishes a bounded documented player projection while
/// leaving unvalidated volume/route details unknown. It makes authorization,
/// claim, publication, command execution, acknowledgement, and revoke
/// consistent for a real host app.
@available(iOS 17.0, *)
public actor AppleMusicCompanionHost {
    private static let maxRememberedCommandOutcomes = 128
    private static let maxRememberedContentOutcomes = 128
    public let bridge: AppleMusicBridgeClient

    public let companionID: String
    private var installation: AppleMusicCompanionInstallation
    private let installationStore: any AppleMusicCompanionInstallationStore
    /// Outcomes are retained in the bounded installation store. The bridge is
    /// at-least-once, so this prevents lease redelivery from driving a
    /// non-idempotent command twice across normal relaunches. It is still not
    /// exactly-once: a crash during the handler before persistence can retry.
    private var commandOutcomes: [String: Bool]
    private var commandOutcomeOrder: [String] = []
    private var contentOutcomes: [String: MusicKitContentResult]
    private var contentOutcomeOrder: [String] = []

    public init(
        bridgeBaseURL: URL,
        companionID: String,
        store: any AppleMusicCompanionInstallationStore = KeychainAppleMusicCompanionInstallationStore()
    ) {
        precondition(!companionID.isEmpty && !companionID.contains(":"), "companionID must be a non-empty owner-safe identifier")
        self.companionID = companionID
        bridge = AppleMusicBridgeClient(baseURL: bridgeBaseURL)
        installation = AppleMusicCompanionInstallation(baseURL: bridgeBaseURL, companionID: companionID)
        installationStore = store
        commandOutcomes = [:]
        contentOutcomes = [:]
    }

    /// Restore a paired installation from secure host storage. The default
    /// store is Keychain-backed; callers can inject an in-memory store for
    /// previews/tests.
    public init(
        installation: AppleMusicCompanionInstallation,
        store: any AppleMusicCompanionInstallationStore = KeychainAppleMusicCompanionInstallationStore()
    ) {
        precondition(!installation.companionID.isEmpty && !installation.companionID.contains(":"), "companionID must be a non-empty owner-safe identifier")
        self.companionID = installation.companionID
        self.installation = installation
        self.installationStore = store
        bridge = AppleMusicBridgeClient(baseURL: installation.baseURL, accessToken: installation.accessToken)
        commandOutcomes = Array(installation.commandOutcomes.prefix(Self.maxRememberedCommandOutcomes)).reduce(into: [:]) { $0[$1.key] = $1.value }
        commandOutcomeOrder = Array(commandOutcomes.keys)
        contentOutcomes = Array(installation.contentOutcomes.prefix(Self.maxRememberedContentOutcomes)).reduce(into: [:]) { $0[$1.key] = $1.value }
        contentOutcomeOrder = Array(contentOutcomes.keys)
    }

    @discardableResult
    public func authorize() async throws -> MusicAuthorization.Status {
        let status = await AppleMusicAuthorization.request()
        guard status == .authorized else { throw CompanionHostError.authorizationDenied(status) }
        return status
    }

    @discardableResult
    public func claim(bridgeID: String, pairingCode: String) async throws -> ClaimResponse {
        let response = try await bridge.claim(bridgeID: bridgeID, pairingCode: pairingCode)
        var saved = installation
        saved.bridgeID = bridgeID
        saved.accessToken = response.accessToken
        saved.commandOutcomes = [:]
        saved.contentOutcomes = [:]
        commandOutcomes.removeAll(keepingCapacity: true)
        commandOutcomeOrder.removeAll(keepingCapacity: true)
        contentOutcomes.removeAll(keepingCapacity: true)
        contentOutcomeOrder.removeAll(keepingCapacity: true)
        installation = saved
        try installationStore.save(saved)
        return response
    }

    public func publish(snapshot: MusicKitSnapshotPayload) async throws {
        try await bridge.publish(snapshot: snapshot)
    }

    /// Read and publish one bounded projection of the system player. The
    /// projection intentionally leaves volume/route unavailable; those fields
    /// are not part of the documented shared SystemMusicPlayer state and must
    /// not be inferred by the host.
    @available(iOS 17.0, *)
    public func publishCurrentSnapshot(from player: SystemMusicPlayerCompanion) async throws {
        try await publish(snapshot: player.snapshotPayload(companionID: companionID))
    }

    /// Poll once and execute each command at least once from the host's point
    /// of view. The server's acknowledgement removes the command from its
    /// delivery queue; a crash before acknowledgement may cause a redelivery,
    /// so host command handlers must deduplicate by command ID before driving
    /// non-idempotent operations.
    public func pollAndHandle(
        _ handler: @Sendable (MusicKitWireCommand) async throws -> Void
    ) async throws {
        for command in try await bridge.pollCommands() {
            if let priorOutcome = commandOutcomes[command.commandID] {
                try await bridge.acknowledge(commandID: command.commandID, ok: priorOutcome)
                continue
            }
            var outcome = true
            var errorDescription: String?
            do {
                try await handler(command.command)
            } catch {
                outcome = false
                errorDescription = String(describing: error)
            }
            try await rememberCommandOutcome(command.commandID, ok: outcome)
            try await bridge.acknowledge(commandID: command.commandID, ok: outcome, error: errorDescription)
        }
    }

    private func rememberCommandOutcome(_ commandID: String, ok: Bool) async throws {
        commandOutcomes[commandID] = ok
        commandOutcomeOrder.removeAll { $0 == commandID }
        commandOutcomeOrder.append(commandID)
        while commandOutcomeOrder.count > Self.maxRememberedCommandOutcomes {
            commandOutcomes.removeValue(forKey: commandOutcomeOrder.removeFirst())
        }
        installation.commandOutcomes = commandOutcomes
        try installationStore.save(installation)
    }

    public func revoke() async throws {
        try await bridge.revoke()
        try installationStore.clear()
    }

    public func pollAndHandleContent(
        _ handler: @Sendable (MusicKitContentCommand) async throws -> MusicKitContentResult
    ) async throws {
        for request in try await bridge.pollContent() {
            if let priorOutcome = contentOutcomes[request.requestID] {
                try await bridge.acknowledgeContent(request.requestID, result: priorOutcome)
                continue
            }
            let result: MusicKitContentResult
            do {
                result = try await handler(request)
            } catch {
                result = MusicKitContentResult(
                    outcome: "failed",
                    error: MusicKitContentError(
                        code: "companion_failed",
                        message: String(error.localizedDescription.prefix(512)),
                        retryable: true
                    )
                )
            }
            let validatedResult = MusicKitContentResult.validated(result)
            // Persist before acknowledging. If the HTTP acknowledgement is
            // lost, the bridge may redeliver, but the native mutation will not
            // run twice; the cached result is replayed instead.
            try await rememberContentOutcome(request.requestID, result: validatedResult)
            try await bridge.acknowledgeContent(request.requestID, result: validatedResult)
        }
    }

    private func rememberContentOutcome(_ requestID: String, result: MusicKitContentResult) async throws {
        contentOutcomes[requestID] = result
        contentOutcomeOrder.removeAll { $0 == requestID }
        contentOutcomeOrder.append(requestID)
        while contentOutcomeOrder.count > Self.maxRememberedContentOutcomes {
            contentOutcomes.removeValue(forKey: contentOutcomeOrder.removeFirst())
        }
        installation.contentOutcomes = contentOutcomes
        try installationStore.save(installation)
    }
}

public enum CompanionHostError: Error, LocalizedError, Sendable {
    case authorizationDenied(MusicAuthorization.Status)

    public var errorDescription: String? {
        switch self {
        case let .authorizationDenied(status):
            "Apple Music authorization is not available (status: \(status))."
        }
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
    private var handles: [String: Song] = [:]
    private var handleOrder: [String] = []
    private var playlistHandles: [String: Playlist] = [:]
    private var playlistHandleOrder: [String] = []
    private static let maxHandles = 256

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

    public func execute(_ command: MusicKitWireCommand) async throws {
        switch command {
        case .play: try await play()
        case .pause: pause()
        case .toggle:
            if player.state.playbackStatus == .playing {
                pause()
            } else {
                try await play()
            }
        case .next: try await skipToNextItem()
        case .previous: try await skipToPreviousItem()
        case .stop, .setVolume, .adjustVolume, .setMute:
            throw CompanionCommandError.notValidated(command)
        }
    }

    /// Project only the documented/readable system-player state. Current-item
    /// album/artist fields are included when MusicKit resolves a Song; an
    /// unresolved queue entry remains a truthful title-only observation.
    public func snapshotPayload(companionID: String) -> MusicKitSnapshotPayload {
        let entry = player.queue.currentEntry
        let track: MusicKitTrackPayload?
        if let entry, case let .song(song) = entry.item {
            track = MusicKitTrackPayload(
                title: song.title,
                artist: song.artistName,
                album: song.albumTitle ?? "",
                artworkURL: song.artwork?.url(width: 512, height: 512)?.absoluteString,
                positionSeconds: finite(player.playbackTime),
                durationSeconds: song.duration
            )
        } else if let entry {
            track = MusicKitTrackPayload(
                title: entry.title,
                artist: entry.subtitle ?? "",
                album: "",
                positionSeconds: finite(player.playbackTime)
            )
        } else {
            track = nil
        }
        return MusicKitSnapshotPayload(
            playerID: companionID,
            displayName: "iPhone Apple Music",
            state: playbackState(player.state.playbackStatus),
            track: track,
            volume: nil,
            isMuted: false
        )
    }

    private func finite(_ value: TimeInterval) -> Double? {
        value.isFinite && value >= 0 ? value : nil
    }

    private func playbackState(_ status: MusicPlayer.PlaybackStatus) -> String {
        switch status {
        case .playing: "playing"
        case .paused: "paused"
        case .stopped: "stopped"
        case .interrupted: "interrupted"
        // UHC's shared wire enum intentionally has no transient seeking
        // state. Keep the snapshot decodable and truthful while the player
        // settles instead of emitting an undocumented value.
        case .seekingForward, .seekingBackward: "unknown"
        @unknown default: "unknown"
        }
    }

    /// Catalog search stays on the signed companion. The returned projection
    /// contains only fields UHC needs for opaque-ref search results; Apple
    /// identifiers remain inside the host app until #463's content transport
    /// is explicitly extended.
    public func searchCatalog(term: String, limit: Int = 25) async throws -> [AppleMusicSearchItem] {
        let boundedLimit = min(max(limit, 1), 50)
        var request = MusicCatalogSearchRequest(term: term, types: [Song.self])
        request.limit = boundedLimit
        let response = try await request.response()
        return response.songs.map(AppleMusicSearchItem.init)
    }

    /// Read a bounded slice of the listener's library without returning
    /// Apple credentials or raw provider responses to UHC.
    public func librarySongs(limit: Int = 25, offset: Int = 0) async throws -> [AppleMusicSearchItem] {
        var request = MusicLibraryRequest<Song>()
        request.limit = min(max(limit, 1), 50)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.items.map(AppleMusicSearchItem.init)
    }

    /// Read the listener's library playlists locally. Apple identifiers stay
    /// inside the companion until the approved content bridge defines scoped
    /// opaque references.
    public func libraryPlaylists(limit: Int = 25, offset: Int = 0) async throws -> [AppleMusicPlaylistSummary] {
        var request = MusicLibraryRequest<Playlist>()
        request.limit = min(max(limit, 1), 50)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.items.map(AppleMusicPlaylistSummary.init)
    }

    /// Load ordered playlist entries, retaining position and unavailable-entry
    /// information needed for truthful read/mutation preconditions.
    public func playlistEntries(
        _ playlist: Playlist,
        limit: Int = 50,
        offset: Int = 0
    ) async throws -> [AppleMusicPlaylistEntrySummary] {
        let loaded = try await playlist.with([.entries], preferredSource: .library)
        let boundedLimit = min(max(limit, 1), 50)
        let start = max(offset, 0)
        guard let entries = loaded.entries else { return [] }
        return Array(entries.dropFirst(start).prefix(boundedLimit))
            .map(AppleMusicPlaylistEntrySummary.init)
    }

    /// Read a bounded recent-song view. A mixed recent-items request can be
    /// added when the content contract needs albums/playlists/stations too.
    public func recentlyPlayedSongs(limit: Int = 25, offset: Int = 0) async throws -> [AppleMusicSearchItem] {
        var request = MusicRecentlyPlayedRequest<Song>()
        request.limit = min(max(limit, 1), 50)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.items.map(AppleMusicSearchItem.init)
    }

    /// Retrieve Apple's bounded personalized recommendation containers. This
    /// is provider context for a future curator, not an autonomous DJ result.
    public func personalRecommendations(limit: Int = 10, offset: Int = 0) async throws -> [AppleMusicRecommendationSummary] {
        var request = MusicPersonalRecommendationsRequest()
        request.limit = min(max(limit, 1), 25)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.recommendations.map(AppleMusicRecommendationSummary.init)
    }

    /// Create a playlist explicitly requested by the signed host. UHC must
    /// require confirmation and retain the returned owner-scoped reference
    /// before invoking this mutation.
    public func createPlaylist(name: String, description: String? = nil) async throws -> AppleMusicPlaylistSummary {
        let boundedName = String(name.prefix(200))
        guard !boundedName.isEmpty else { throw CompanionContentError.invalidName }
        let playlist = try await MusicLibrary.shared.createPlaylist(
            name: boundedName,
            description: description.map { String($0.prefix(500)) },
            authorDisplayName: nil
        )
        return AppleMusicPlaylistSummary(playlist: playlist)
    }

    /// Append one exact song to a playlist. Apple Music itself rejects
    /// playlists that are not available in the listener's library; arbitrary
    /// remove/reorder/delete operations are intentionally not provided here.
    public func add(song: Song, to playlist: Playlist) async throws -> AppleMusicPlaylistSummary {
        let updated = try await MusicLibrary.shared.add(song, to: playlist)
        return AppleMusicPlaylistSummary(playlist: updated)
    }

    /// Start an exact catalog/library result on the iPhone's system player.
    public func play(song: Song) async throws {
        player.queue = MusicPlayer.Queue(for: [song], startingAt: song)
        try await player.play()
    }

    /// Handle one approved bridge content request. Apple identifiers remain
    /// inside this actor; UHC receives only a companion-local opaque handle.
    public func executeContent(_ request: MusicKitContentCommand) async throws -> MusicKitContentResult {
        switch request.operation {
        case "catalog_search":
            let query = stringParam(request.params, "query") ?? ""
            let limit = intParam(request.params, "limit") ?? 25
            let items = try await searchForBridge(term: query, limit: limit)
            return MusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "library":
            let items = try await libraryForBridge(
                limit: intParam(request.params, "limit") ?? 25,
                offset: intParam(request.params, "offset") ?? 0
            )
            return MusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "recent":
            let items = try await recentForBridge(
                limit: intParam(request.params, "limit") ?? 25,
                offset: intParam(request.params, "offset") ?? 0
            )
            return MusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "recommendations":
            let items = try await recommendationsForBridge(
                limit: intParam(request.params, "limit") ?? 10,
                offset: intParam(request.params, "offset") ?? 0
            )
            return MusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "playlists":
            let items = try await playlistsForBridge(
                limit: intParam(request.params, "limit") ?? 25,
                offset: intParam(request.params, "offset") ?? 0
            )
            return MusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "playlist_tracks":
            guard let reference = stringParam(request.params, "id") ?? stringParam(request.params, "uri"),
                  let playlist = playlistHandles[reference] else {
                return MusicKitContentResult(
                    outcome: "not_found",
                    error: MusicKitContentError(code: "unknown_ref", message: "Playlist handle is unknown or expired.", retryable: false)
                )
            }
            let loaded = try await playlist.with([.entries], preferredSource: .library)
            let entries = (loaded.entries ?? []).map { entry in
                let song = entry.item.flatMap { item -> Song? in
                    if case let .song(song) = item { return song }
                    return nil
                }
                return AppleMusicBridgePlaylistEntry(
                    title: entry.title,
                    subtitle: entry.artistName,
                    uri: song.map { mintHandle(for: $0) },
                    position: entry.position,
                    isPlayable: song != nil
                )
            }
            return MusicKitContentResult(outcome: "success", data: try jsonValue(entries))
        case "playlist_create":
            guard let name = stringParam(request.params, "name"), !name.isEmpty else {
                return invalidContentResult("name is required")
            }
            let playlist = try await MusicLibrary.shared.createPlaylist(
                name: String(name.prefix(200)),
                description: stringParam(request.params, "description").map { String($0.prefix(500)) },
                authorDisplayName: nil
            )
            let handle = mintPlaylistHandle(for: playlist)
            return MusicKitContentResult(
                outcome: "success",
                data: try jsonValue(AppleMusicBridgePlaylistSummary(ref: handle, title: playlist.name))
            )
        case "playlist_update":
            guard let reference = stringParam(request.params, "id") ?? stringParam(request.params, "uri"),
                  let playlist = playlistHandles[reference] else {
                return MusicKitContentResult(
                    outcome: "not_found",
                    error: MusicKitContentError(code: "unknown_ref", message: "Playlist handle is unknown or expired.", retryable: false)
                )
            }
            let updated = try await MusicLibrary.shared.edit(
                playlist,
                name: stringParam(request.params, "name"),
                description: stringParam(request.params, "description"),
                authorDisplayName: nil
            )
            let handle = mintPlaylistHandle(for: updated)
            return MusicKitContentResult(
                outcome: "success",
                data: try jsonValue(AppleMusicBridgePlaylistSummary(ref: handle, title: updated.name))
            )
        case "playlist_add":
            guard let playlistReference = stringParam(request.params, "id") ?? stringParam(request.params, "playlist_ref"),
                  let playlist = playlistHandles[playlistReference] else {
                return MusicKitContentResult(
                    outcome: "not_found",
                    error: MusicKitContentError(code: "unknown_ref", message: "Playlist handle is unknown or expired.", retryable: false)
                )
            }
            guard let songReference = stringParam(request.params, "uri") ?? stringParam(request.params, "item_ref"),
                  let song = handles[songReference] else {
                return MusicKitContentResult(
                    outcome: "not_found",
                    error: MusicKitContentError(code: "unknown_ref", message: "Song handle is unknown or expired.", retryable: false)
                )
            }
            let updated = try await MusicLibrary.shared.add(song, to: playlist)
            let handle = mintPlaylistHandle(for: updated)
            return MusicKitContentResult(
                outcome: "success",
                data: try jsonValue(AppleMusicBridgePlaylistSummary(ref: handle, title: updated.name))
            )
        case "queue_plan":
            guard case let .array(values)? = request.params["items"] else {
                return invalidContentResult("items is required")
            }
            let references = values.compactMap { value -> String? in
                guard case let .string(reference) = value else { return nil }
                return reference
            }
            guard references.count == values.count, !references.isEmpty, references.count <= 200 else {
                return invalidContentResult("items must contain between 1 and 200 song handles")
            }
            let songs = references.compactMap { handles[$0] }
            guard songs.count == references.count else {
                return MusicKitContentResult(
                    outcome: "not_found",
                    error: MusicKitContentError(code: "unknown_ref", message: "One or more song handles are unknown or expired.", retryable: false)
                )
            }
            try await replaceQueue(with: songs)
            return MusicKitContentResult(
                outcome: "success",
                data: .object(["queued": .number(Double(songs.count))])
            )
        case "play_uri", "queue_uri":
            guard let handle = stringParam(request.params, "uri") else {
                return invalidContentResult("uri is required")
            }
            guard let song = handles[handle] else {
                return MusicKitContentResult(
                    outcome: "not_found",
                    error: MusicKitContentError(code: "unknown_ref", message: "Apple Music handle is unknown or expired.", retryable: false)
                )
            }
            if request.operation == "play_uri" {
                try await play(song: song)
            } else {
                try await playNext(song: song)
            }
            return MusicKitContentResult(
                outcome: "success",
                data: .object(["message": .string(request.operation == "play_uri" ? "Apple Music item started" : "Apple Music item queued")])
            )
        default:
            return MusicKitContentResult(
                outcome: "unsupported",
                error: MusicKitContentError(code: "unsupported", message: "This Apple Music content operation is not enabled on the iPhone companion.", retryable: false)
            )
        }
    }

    private func searchForBridge(term: String, limit: Int) async throws -> [AppleMusicBridgeSearchItem] {
        let boundedLimit = min(max(limit, 1), 50)
        var request = MusicCatalogSearchRequest(term: term, types: [Song.self])
        request.limit = boundedLimit
        let response = try await request.response()
        return response.songs.map { song in
            let handle = mintHandle(for: song)
            return AppleMusicBridgeSearchItem(
                title: song.title,
                subtitle: song.artistName,
                uri: handle
            )
        }
    }

    private func libraryForBridge(limit: Int, offset: Int) async throws -> [AppleMusicBridgeSearchItem] {
        var request = MusicLibraryRequest<Song>()
        request.limit = min(max(limit, 1), 50)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.items.map { song in
            AppleMusicBridgeSearchItem(title: song.title, subtitle: song.artistName, uri: mintHandle(for: song))
        }
    }

    private func recentForBridge(limit: Int, offset: Int) async throws -> [AppleMusicBridgeSearchItem] {
        var request = MusicRecentlyPlayedRequest<Song>()
        request.limit = min(max(limit, 1), 50)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.items.map { song in
            AppleMusicBridgeSearchItem(title: song.title, subtitle: song.artistName, uri: mintHandle(for: song))
        }
    }

    private func recommendationsForBridge(limit: Int, offset: Int) async throws -> [AppleMusicBridgeRecommendation] {
        var request = MusicPersonalRecommendationsRequest()
        request.limit = min(max(limit, 1), 25)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.recommendations.map { recommendation in
            AppleMusicBridgeRecommendation(
                ref: "apple_recommendation_\(UUID().uuidString.lowercased())",
                title: recommendation.title ?? "Apple Music recommendation",
                reason: recommendation.reason,
                nextRefreshAt: recommendation.nextRefreshDate?.timeIntervalSince1970
            )
        }
    }

    private func playlistsForBridge(limit: Int, offset: Int) async throws -> [AppleMusicBridgePlaylistSummary] {
        var request = MusicLibraryRequest<Playlist>()
        request.limit = min(max(limit, 1), 50)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.items.map { playlist in
            let handle = mintPlaylistHandle(for: playlist)
            return AppleMusicBridgePlaylistSummary(ref: handle, title: playlist.name)
        }
    }

    private func mintHandle(for song: Song) -> String {
        let handle = "apple_handle_\(UUID().uuidString.lowercased())"
        handles[handle] = song
        handleOrder.removeAll { $0 == handle }
        handleOrder.append(handle)
        while handleOrder.count > Self.maxHandles {
            handles.removeValue(forKey: handleOrder.removeFirst())
        }
        return handle
    }

    private func mintPlaylistHandle(for playlist: Playlist) -> String {
        let handle = "apple_playlist_\(UUID().uuidString.lowercased())"
        playlistHandles[handle] = playlist
        playlistHandleOrder.removeAll { $0 == handle }
        playlistHandleOrder.append(handle)
        while playlistHandleOrder.count > Self.maxHandles {
            playlistHandles.removeValue(forKey: playlistHandleOrder.removeFirst())
        }
        return handle
    }

    private func stringParam(_ params: [String: MusicKitJSONValue], _ key: String) -> String? {
        guard case let .string(value) = params[key] else { return nil }
        return value
    }

    private func intParam(_ params: [String: MusicKitJSONValue], _ key: String) -> Int? {
        guard case let .number(value) = params[key] else { return nil }
        return Int(value)
    }

    private func jsonValue<T: Encodable>(_ value: T) throws -> MusicKitJSONValue {
        try JSONDecoder().decode(MusicKitJSONValue.self, from: JSONEncoder().encode(value))
    }

    private func invalidContentResult(_ message: String) -> MusicKitContentResult {
        MusicKitContentResult(
            outcome: "invalid",
            error: MusicKitContentError(code: "invalid", message: message, retryable: false)
        )
    }

    /// Replace the requested system-player queue. SystemMusicPlayer queue
    /// visibility and persistence must still be proven on physical hardware.
    public func replaceQueue(with songs: [Song]) async throws {
        guard let first = songs.first else { return }
        player.queue = MusicPlayer.Queue(for: songs, startingAt: first)
        try await player.play()
    }

    /// Add one item after the current entry (the documented Play Next
    /// position). This is intentionally not represented as confirmed queue
    /// state until the companion publishes a validated snapshot.
    public func playNext(song: Song) async throws {
        try await player.queue.insert(song, position: .afterCurrentEntry)
    }
}

@available(iOS 17.0, *)
public enum CompanionCommandError: Error, LocalizedError, Sendable {
    case notValidated(MusicKitWireCommand)

    public var errorDescription: String? {
        switch self {
        case let .notValidated(command):
            "Apple Music command is not validated on this companion: \(command)."
        }
    }
}

/// Provider-neutral projection for native search/library results.
@available(iOS 17.0, *)
public struct AppleMusicSearchItem: Sendable, Equatable {
    public let id: String
    public let title: String
    public let artist: String
    public let album: String
    public let artworkURL: URL?

    public init(song: Song) {
        id = song.id.rawValue
        title = song.title
        artist = song.artistName
        album = song.albumTitle ?? ""
        artworkURL = song.artwork?.url(width: 512, height: 512)
    }
}

@available(iOS 17.0, *)
public struct AppleMusicPlaylistSummary: Sendable, Equatable {
    public let id: String
    public let title: String

    public init(playlist: Playlist) {
        id = playlist.id.rawValue
        title = playlist.name
    }
}

@available(iOS 17.0, *)
public struct AppleMusicPlaylistEntrySummary: Sendable, Equatable {
    public let id: String
    public let position: Int
    public let title: String
    public let artist: String
    public let album: String
    public let artworkURL: URL?
    public let isPlayable: Bool

    public init(entry: Playlist.Entry) {
        id = entry.id.rawValue
        position = entry.position
        title = entry.title
        artist = entry.artistName
        album = entry.albumTitle ?? ""
        artworkURL = entry.artwork?.url(width: 512, height: 512)
        isPlayable = entry.item != nil
    }
}

@available(iOS 17.0, *)
public struct AppleMusicRecommendationSummary: Sendable, Equatable {
    public let id: String
    public let title: String
    public let reason: String?
    public let nextRefreshDate: Date?

    public init(recommendation: MusicPersonalRecommendation) {
        id = recommendation.id.rawValue
        title = recommendation.title ?? "Apple Music recommendation"
        reason = recommendation.reason
        nextRefreshDate = recommendation.nextRefreshDate
    }
}

@available(iOS 17.0, *)
public struct AppleMusicBridgeSearchItem: Codable, Sendable, Equatable {
    public let title: String
    public let subtitle: String?
    public let uri: String

    public init(title: String, subtitle: String?, uri: String) {
        self.title = title
        self.subtitle = subtitle
        self.uri = uri
    }
}

@available(iOS 17.0, *)
public struct AppleMusicBridgePlaylistSummary: Codable, Sendable, Equatable {
    public let ref: String
    public let title: String

    public init(ref: String, title: String) {
        self.ref = ref
        self.title = title
    }
}

@available(iOS 17.0, *)
public struct AppleMusicBridgePlaylistEntry: Codable, Sendable, Equatable {
    public let title: String
    public let subtitle: String
    public let uri: String?
    public let position: Int
    public let isPlayable: Bool

    enum CodingKeys: String, CodingKey {
        case title, subtitle, uri, position
        case isPlayable = "is_playable"
    }

    public init(title: String, subtitle: String, uri: String?, position: Int, isPlayable: Bool) {
        self.title = title
        self.subtitle = subtitle
        self.uri = uri
        self.position = position
        self.isPlayable = isPlayable
    }
}

@available(iOS 17.0, *)
public struct AppleMusicBridgeRecommendation: Codable, Sendable, Equatable {
    public let ref: String
    public let title: String
    public let reason: String?
    public let nextRefreshAt: Double?

    public init(ref: String, title: String, reason: String?, nextRefreshAt: Double?) {
        self.ref = ref
        self.title = title
        self.reason = reason
        self.nextRefreshAt = nextRefreshAt
    }
}

@available(iOS 17.0, *)
public enum CompanionContentError: Error, LocalizedError, Sendable {
    case invalidName

    public var errorDescription: String? {
        switch self {
        case .invalidName: "Apple Music playlist name must not be empty."
        }
    }
}
