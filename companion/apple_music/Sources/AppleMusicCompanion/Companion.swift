import Foundation
import MusicKit

/// Native side of the UHC Apple Music boundary.
///
/// This package is intentionally a library rather than an extra UHC server.
/// A signed macOS host app can embed it for inline mode, while a paired bridge
/// executable can use the same actor for off-host mode. Credentials and
/// MusicKit authorization stay inside that host app.
@available(macOS 14.0, *)
public actor ApplicationMusicPlayerCompanion {
    private let player = ApplicationMusicPlayer.shared
    public let companionID: String
    private var handles: [String: Song] = [:]
    private var handleOrder: [String] = []
    private var playlistHandles: [String: Playlist] = [:]
    private var playlistHandleOrder: [String] = []
    private static let maxHandles = 256

    public init(companionID: String) {
        precondition(!companionID.isEmpty && !companionID.contains(":"), "companionID must be a non-empty owner-safe identifier")
        self.companionID = companionID
    }

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

    public func execute(_ command: MacMusicKitCommand) async throws {
        switch command {
        case .play: try await play()
        case .pause: pause()
        case .next: try await skipToNextItem()
        case .previous: try await skipToPreviousItem()
        case .setRepeat(mode: let mode): player.state.repeatMode = mode
        case .setShuffle(enabled: let enabled): player.state.shuffleMode = enabled ? .songs : .off
        case .toggle, .stop, .setVolume, .adjustVolume, .setMute:
            throw CompanionError.commandNotValidated
        }
    }

    /// Catalog and library access remains inside the signed host app. These
    /// methods mirror the iPhone companion's documented MusicKit primitives;
    /// the macOS session is still app-private until #486 validates it.
    public func searchCatalog(term: String, limit: Int = 25) async throws -> [MacAppleMusicItem] {
        var request = MusicCatalogSearchRequest(term: term, types: [Song.self])
        request.limit = min(max(limit, 1), 50)
        let response = try await request.response()
        return response.songs.map(MacAppleMusicItem.init)
    }

    public func librarySongs(limit: Int = 25, offset: Int = 0) async throws -> [MacAppleMusicItem] {
        var request = MusicLibraryRequest<Song>()
        request.limit = min(max(limit, 1), 50)
        request.offset = max(offset, 0)
        let response = try await request.response()
        return response.items.map(MacAppleMusicItem.init)
    }

    /// Execute the owner-scoped content operations used by the approved UHC
    /// bridge. Apple identifiers remain in this actor; only companion-local
    /// opaque handles cross the bridge.
    public func executeContent(_ request: MacMusicKitContentCommand) async throws -> MacMusicKitContentResult {
        switch request.operation {
        case "catalog_search":
            var search = MusicCatalogSearchRequest(term: stringParam(request.params, "query") ?? "", types: [Song.self])
            search.limit = min(max(intParam(request.params, "limit") ?? 25, 1), 50)
            let response = try await search.response()
            let items = response.songs.map { song in
                MacAppleMusicBridgeItem(title: song.title, subtitle: song.artistName, uri: mintHandle(for: song))
            }
            return MacMusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "library":
            var library = MusicLibraryRequest<Song>()
            library.limit = min(max(intParam(request.params, "limit") ?? 25, 1), 50)
            library.offset = max(intParam(request.params, "offset") ?? 0, 0)
            let response = try await library.response()
            let items = response.items.map { song in
                MacAppleMusicBridgeItem(title: song.title, subtitle: song.artistName, uri: mintHandle(for: song))
            }
            return MacMusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "recent":
            var recent = MusicRecentlyPlayedRequest<Song>()
            recent.limit = min(max(intParam(request.params, "limit") ?? 25, 1), 50)
            recent.offset = max(intParam(request.params, "offset") ?? 0, 0)
            let response = try await recent.response()
            let items = response.items.map { song in
                MacAppleMusicBridgeItem(title: song.title, subtitle: song.artistName, uri: mintHandle(for: song))
            }
            return MacMusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "recommendations":
            var recommendations = MusicPersonalRecommendationsRequest()
            recommendations.limit = min(max(intParam(request.params, "limit") ?? 10, 1), 25)
            recommendations.offset = max(intParam(request.params, "offset") ?? 0, 0)
            let response = try await recommendations.response()
            let items = response.recommendations.map { recommendation in
                MacAppleMusicBridgeRecommendation(
                    ref: "apple_mac_recommendation_\(UUID().uuidString.lowercased())",
                    title: recommendation.title ?? "Apple Music recommendation",
                    reason: recommendation.reason,
                    nextRefreshAt: recommendation.nextRefreshDate?.timeIntervalSince1970
                )
            }
            return MacMusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "playlists":
            var playlists = MusicLibraryRequest<Playlist>()
            playlists.limit = min(max(intParam(request.params, "limit") ?? 25, 1), 50)
            playlists.offset = max(intParam(request.params, "offset") ?? 0, 0)
            let response = try await playlists.response()
            let items = response.items.map { playlist in
                MacAppleMusicBridgePlaylistSummary(ref: mintPlaylistHandle(for: playlist), title: playlist.name)
            }
            return MacMusicKitContentResult(outcome: "success", data: try jsonValue(items))
        case "playlist_tracks":
            guard let reference = stringParam(request.params, "id") ?? stringParam(request.params, "uri"),
                  let playlist = playlistHandles[reference] else {
                return MacMusicKitContentResult(outcome: "not_found", error: MacMusicKitContentError(code: "unknown_ref", message: "Playlist handle is unknown or expired.", retryable: false))
            }
            let loaded = try await playlist.with([.entries], preferredSource: .library)
            let entries = (loaded.entries ?? []).map { entry in
                let song = entry.item.flatMap { item -> Song? in
                    if case let .song(song) = item { return song }
                    return nil
                }
                return MacAppleMusicBridgePlaylistEntry(
                    title: entry.title,
                    subtitle: entry.artistName,
                    uri: song.map { mintHandle(for: $0) },
                    position: entry.position,
                    isPlayable: song != nil
                )
            }
            return MacMusicKitContentResult(outcome: "success", data: try jsonValue(entries))
        case "playlist_create", "playlist_update", "playlist_add":
            // MusicLibrary playlist mutations are unavailable in the macOS
            // MusicKit SDK. Keep the operation explicit rather than falling
            // back to Music.app automation or claiming it succeeded.
            return MacMusicKitContentResult(
                outcome: "unsupported",
                error: MacMusicKitContentError(
                    code: "macos_music_library_mutation_unavailable",
                    message: "Apple Music playlist mutations require the iPhone companion.",
                    retryable: false
                )
            )
        case "play_uri", "queue_uri":
            guard let uri = stringParam(request.params, "uri"), let song = handles[uri] else {
                return MacMusicKitContentResult(outcome: "not_found", error: MacMusicKitContentError(code: "unknown_ref", message: "Apple Music handle is unknown or expired.", retryable: false))
            }
            if request.operation == "play_uri" { try await play(song: song) }
            else { try await playNext(song: song) }
            return MacMusicKitContentResult(outcome: "success", data: .object(["uri": .string(uri)]))
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
                return MacMusicKitContentResult(outcome: "not_found", error: MacMusicKitContentError(code: "unknown_ref", message: "One or more song handles are unknown or expired.", retryable: false))
            }
            try await replaceQueue(with: songs)
            return MacMusicKitContentResult(outcome: "success", data: .object(["queued": .number(Double(songs.count))]))
        default:
            return MacMusicKitContentResult(outcome: "unsupported", error: MacMusicKitContentError(code: "unsupported", message: "This operation is not enabled on the macOS companion.", retryable: false))
        }
    }

    private func mintHandle(for song: Song) -> String {
        let handle = "apple_mac_handle_\(UUID().uuidString.lowercased())"
        handles[handle] = song
        handleOrder.removeAll { $0 == handle }
        handleOrder.append(handle)
        while handleOrder.count > Self.maxHandles { handles.removeValue(forKey: handleOrder.removeFirst()) }
        return handle
    }

    private func mintPlaylistHandle(for playlist: Playlist) -> String {
        let handle = "apple_mac_playlist_\(UUID().uuidString.lowercased())"
        playlistHandles[handle] = playlist
        playlistHandleOrder.removeAll { $0 == handle }
        playlistHandleOrder.append(handle)
        while playlistHandleOrder.count > Self.maxHandles { playlistHandles.removeValue(forKey: playlistHandleOrder.removeFirst()) }
        return handle
    }

    private func stringParam(_ params: [String: MacMusicKitJSONValue], _ key: String) -> String? {
        guard case let .string(value) = params[key] else { return nil }
        return value
    }

    private func intParam(_ params: [String: MacMusicKitJSONValue], _ key: String) -> Int? {
        guard case let .number(value) = params[key] else { return nil }
        return Int(value)
    }

    private func jsonValue<T: Encodable>(_ value: T) throws -> MacMusicKitJSONValue {
        try JSONDecoder().decode(MacMusicKitJSONValue.self, from: JSONEncoder().encode(value))
    }

    private func invalidContentResult(_ message: String) -> MacMusicKitContentResult {
        MacMusicKitContentResult(outcome: "invalid", error: MacMusicKitContentError(code: "invalid", message: message, retryable: false))
    }

    public func play(song: Song) async throws {
        player.queue = ApplicationMusicPlayer.Queue(for: [song], startingAt: song)
        try await player.play()
    }

    public func replaceQueue(with songs: [Song]) async throws {
        guard let first = songs.first else { return }
        player.queue = ApplicationMusicPlayer.Queue(for: songs, startingAt: first)
        try await player.play()
    }

    public func playNext(song: Song) async throws {
        try await player.queue.insert(song, position: .afterCurrentEntry)
    }

    /// Project the app-private player state. This proves only the
    /// ApplicationMusicPlayer session owned by this host; it does not claim
    /// control of Music.app or any AirPlay destination.
    public func snapshot() -> MacMusicKitSnapshot {
        let entry = player.queue.currentEntry
        let track: MacMusicKitTrack?
        if let entry, case let .song(song) = entry.item {
            track = MacMusicKitTrack(
                title: song.title,
                artist: song.artistName,
                album: song.albumTitle ?? "",
                artworkURL: song.artwork?.url(width: 512, height: 512)?.absoluteString,
                positionSeconds: finite(player.playbackTime),
                durationSeconds: song.duration
            )
        } else if let entry {
            track = MacMusicKitTrack(title: entry.title, artist: entry.subtitle ?? "", album: "", positionSeconds: finite(player.playbackTime))
        } else {
            track = nil
        }
        return MacMusicKitSnapshot(
            playerID: companionID,
            displayName: "Mac Apple Music",
            state: playbackState(player.state.playbackStatus),
            track: track,
            volume: nil,
            isMuted: false,
            repeatMode: repeatWireValue(player.state.repeatMode),
            shuffle: shuffleWireValue(player.state.shuffleMode)
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
        case .seekingForward, .seekingBackward: "seeking"
        @unknown default: "unknown"
        }
    }

    private func repeatWireValue(_ mode: MusicPlayer.RepeatMode?) -> String? {
        guard let mode else { return nil }
        switch mode {
        case .none: return "off"
        case .one: return "one"
        case .all: return "all"
        @unknown default: return nil
        }
    }

    private func shuffleWireValue(_ mode: MusicPlayer.ShuffleMode?) -> Bool? {
        guard let mode else { return nil }
        switch mode {
        case .off: return false
        case .songs: return true
        @unknown default: return nil
        }
    }
}

@available(macOS 14.0, *)
public struct MacMusicKitSnapshot: Codable, Sendable, Equatable {
    public let playerID: String
    public let displayName: String
    public let state: String
    public let track: MacMusicKitTrack?
    public let volume: Float?
    public let isMuted: Bool
    public let repeatMode: String?
    public let shuffle: Bool?

    enum CodingKeys: String, CodingKey {
        case playerID = "player_id", displayName = "display_name", state, track, volume, isMuted = "is_muted", repeatMode = "repeat_mode", shuffle
    }

    public init(playerID: String, displayName: String, state: String, track: MacMusicKitTrack? = nil, volume: Float? = nil, isMuted: Bool = false, repeatMode: String? = nil, shuffle: Bool? = nil) {
        self.playerID = playerID
        self.displayName = displayName
        self.state = state
        self.track = track
        self.volume = volume
        self.isMuted = isMuted
        self.repeatMode = repeatMode
        self.shuffle = shuffle
    }
}

@available(macOS 14.0, *)
public struct MacMusicKitTrack: Codable, Sendable, Equatable {
    public let title: String
    public let artist: String
    public let album: String
    public let artworkURL: String?
    public let positionSeconds: Double?
    public let durationSeconds: Double?

    enum CodingKeys: String, CodingKey {
        case title, artist, album, artworkURL = "artwork_url", positionSeconds = "position_seconds", durationSeconds = "duration_seconds"
    }

    public init(title: String, artist: String, album: String, artworkURL: String? = nil, positionSeconds: Double? = nil, durationSeconds: Double? = nil) {
        self.title = title
        self.artist = artist
        self.album = album
        self.artworkURL = artworkURL
        self.positionSeconds = positionSeconds
        self.durationSeconds = durationSeconds
    }
}

@available(macOS 14.0, *)
public struct MacAppleMusicItem: Sendable, Equatable {
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

@available(macOS 14.0, *)
public struct MacAppleMusicBridgeItem: Codable, Sendable, Equatable {
    public let title: String
    public let subtitle: String?
    public let uri: String
    public init(title: String, subtitle: String?, uri: String) {
        self.title = title; self.subtitle = subtitle; self.uri = uri
    }
}

@available(macOS 14.0, *)
public struct MacAppleMusicBridgePlaylistSummary: Codable, Sendable, Equatable {
    public let ref: String
    public let title: String
    public init(ref: String, title: String) { self.ref = ref; self.title = title }
}

@available(macOS 14.0, *)
public struct MacAppleMusicBridgePlaylistEntry: Codable, Sendable, Equatable {
    public let title: String
    public let subtitle: String
    public let uri: String?
    public let position: Int
    public let isPlayable: Bool
    enum CodingKeys: String, CodingKey { case title, subtitle, uri, position; case isPlayable = "is_playable" }
    public init(title: String, subtitle: String, uri: String?, position: Int, isPlayable: Bool) {
        self.title = title; self.subtitle = subtitle; self.uri = uri; self.position = position; self.isPlayable = isPlayable
    }
}

@available(macOS 14.0, *)
public struct MacAppleMusicBridgeRecommendation: Codable, Sendable, Equatable {
    public let ref: String
    public let title: String
    public let reason: String?
    public let nextRefreshAt: Double?
    public init(ref: String, title: String, reason: String?, nextRefreshAt: Double?) {
        self.ref = ref; self.title = title; self.reason = reason; self.nextRefreshAt = nextRefreshAt
    }
}

public enum CompanionError: Error, LocalizedError {
    case hostIntegrationRequired
    case commandNotValidated

    public var errorDescription: String? {
        switch self {
        case .hostIntegrationRequired:
            "The embedding macOS app must provide MusicKit authorization and state projection."
        case .commandNotValidated:
            "This Apple Music command is not validated for the macOS companion."
        }
    }
}
