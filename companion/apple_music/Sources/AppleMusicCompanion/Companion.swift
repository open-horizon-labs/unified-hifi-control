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

    /// State projection is intentionally kept behind the host app's bridge
    /// protocol. MusicKit's item metadata and artwork URL require an active
    /// catalog/user authorization context, which this package cannot obtain
    /// without the embedding app's developer-token policy and entitlements.
    public func snapshot() throws -> Never {
        throw CompanionError.hostIntegrationRequired
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
