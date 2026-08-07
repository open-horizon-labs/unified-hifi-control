import Foundation
import MusicKit

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
