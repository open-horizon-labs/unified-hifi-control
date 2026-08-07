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
    public static let playerID = "application"

    private let player = ApplicationMusicPlayer.shared
    public let companionID: String

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

    public func play(song: Song) async throws {
        player.queue = MusicPlayer.Queue(for: [song], startingAt: song)
        try await player.play()
    }

    public func replaceQueue(with songs: [Song]) async throws {
        guard let first = songs.first else { return }
        player.queue = MusicPlayer.Queue(for: songs, startingAt: first)
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
        case .seekingForward, .seekingBackward: "seeking"
        @unknown default: "unknown"
        }
    }
}

@available(macOS 14.0, *)
public struct MacMusicKitSnapshot: Sendable, Equatable {
    public let playerID: String
    public let displayName: String
    public let state: String
    public let track: MacMusicKitTrack?
    public let volume: Float?
    public let isMuted: Bool

    public init(playerID: String, displayName: String, state: String, track: MacMusicKitTrack? = nil, volume: Float? = nil, isMuted: Bool = false) {
        self.playerID = playerID
        self.displayName = displayName
        self.state = state
        self.track = track
        self.volume = volume
        self.isMuted = isMuted
    }
}

@available(macOS 14.0, *)
public struct MacMusicKitTrack: Sendable, Equatable {
    public let title: String
    public let artist: String
    public let album: String
    public let artworkURL: String?
    public let positionSeconds: Double?
    public let durationSeconds: Double?

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

public enum CompanionError: Error, LocalizedError {
    case hostIntegrationRequired

    public var errorDescription: String? {
        switch self {
        case .hostIntegrationRequired:
            "The embedding macOS app must provide MusicKit authorization and state projection."
        }
    }
}
