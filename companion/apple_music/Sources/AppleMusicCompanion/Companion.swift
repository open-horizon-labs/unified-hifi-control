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

    /// State projection is intentionally kept behind the host app's bridge
    /// protocol. MusicKit's item metadata and artwork URL require an active
    /// catalog/user authorization context, which this package cannot obtain
    /// without the embedding app's developer-token policy and entitlements.
    public func snapshot() throws -> Never {
        throw CompanionError.hostIntegrationRequired
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
