import Foundation
import Observation
import SwiftUI
import UHCKit

/// The Watch app's single source of truth.
///
/// Holds a `UHCClient` and never a transport directly, so the direct-WiFi vs
/// phone-relay question (#619, `Transport.md`) is settled in exactly one place:
/// `makeClient(for:)`. Nothing else in the app knows how bytes reach UHC.
@MainActor
@Observable
final class WatchController {
    enum Connection {
        case needsServer
        case connecting
        case connected
    }

    private(set) var connection: Connection = .connecting
    private(set) var zones: [Zone] = []
    private(set) var nowPlaying: NowPlaying?
    private(set) var artwork: UIImage?
    private(set) var errorMessage: String?
    /// Shown verbatim on the diagnostics screen. On real hardware this is the
    /// evidence that settles whether watchOS may reach the LAN at all, so it
    /// keeps the underlying error rather than a friendly paraphrase.
    private(set) var lastTransportFailure: String?
    private(set) var serverDescription: String?

    var selectedZoneID: String? {
        didSet {
            guard selectedZoneID != oldValue else { return }
            UserDefaults.standard.set(selectedZoneID, forKey: Self.zoneKey)
            artwork = nil
            loadedArtworkKey = nil
            nowPlaying = nil
        }
    }

    private static let serverKey = "uhc.baseURL"
    private static let zoneKey = "uhc.selectedZoneID"

    private var client: UHCClient?
    private var pollTask: Task<Void, Never>?
    private var loadedArtworkKey: String?
    /// Set while a command is in flight, so optimistic UI does not get stomped
    /// by a poll that started before the command landed.
    private var suppressPollUntil: Date = .distantPast

    // MARK: - Lifecycle

    func startup() async {
        selectedZoneID = UserDefaults.standard.string(forKey: Self.zoneKey)

        if let saved = UserDefaults.standard.string(forKey: Self.serverKey),
           let url = URL(string: saved) {
            await connect(to: url)
            return
        }
        await discover()
    }

    /// Browse for `_uhc._tcp.` and connect to the first server that answers.
    func discover() async {
        connection = .connecting
        errorMessage = nil
        do {
            let url = try await UHCDiscovery.firstServer()
            await connect(to: url)
        } catch {
            lastTransportFailure = (error as? UHCError)?.errorDescription ?? "\(error)"
            errorMessage = "No UHC server found."
            connection = .needsServer
        }
    }

    /// Connect to an explicitly named server (manual entry, or a discovery hit).
    func connect(to url: URL) async {
        connection = .connecting
        errorMessage = nil

        // The single line that would change to move onto the phone relay.
        let client = UHCClient(transport: DirectHTTPTransport(baseURL: url))
        self.client = client
        serverDescription = client.describedEndpoint

        do {
            _ = try await client.status()
            UserDefaults.standard.set(url.absoluteString, forKey: Self.serverKey)
            connection = .connected
            lastTransportFailure = nil
            await refreshZones()
            startPolling()
        } catch {
            recordFailure(error)
            connection = .needsServer
        }
    }

    func forgetServer() {
        pollTask?.cancel()
        pollTask = nil
        client = nil
        zones = []
        nowPlaying = nil
        artwork = nil
        UserDefaults.standard.removeObject(forKey: Self.serverKey)
        connection = .needsServer
    }

    // MARK: - Reads

    func refreshZones() async {
        guard let client else { return }
        do {
            let fetched = try await client.zones()
            zones = fetched
            if selectedZoneID == nil || !fetched.contains(where: { $0.id == selectedZoneID }) {
                // Prefer a zone that is actually playing — on a watch, the
                // thing you reach for is almost always the thing making noise.
                selectedZoneID = fetched.first(where: { $0.state.isPlaying })?.id ?? fetched.first?.id
            }
            errorMessage = nil
        } catch {
            recordFailure(error)
        }
    }

    func refreshNowPlaying() async {
        guard let client, let zoneID = selectedZoneID else { return }
        guard Date() >= suppressPollUntil else { return }
        defer { confirmedVolume = nowPlaying?.volume }
        do {
            let np = try await client.nowPlaying(zoneID: zoneID)
            nowPlaying = np
            // `/now_playing` echoes the zone list, so the picker stays fresh
            // without a second request.
            if !np.zones.isEmpty { zones = np.zones }
            errorMessage = nil
            await refreshArtworkIfNeeded(for: np)
        } catch {
            recordFailure(error)
        }
    }

    private func refreshArtworkIfNeeded(for np: NowPlaying) async {
        guard let client else { return }
        // `image_key` is the server's own cache key: re-fetch only when the art
        // actually changed, not on every poll.
        guard let key = np.imageKey else {
            artwork = nil
            loadedArtworkKey = nil
            return
        }
        guard key != loadedArtworkKey else { return }

        do {
            guard let data = try await client.artwork(zoneID: np.zoneID, width: 200, height: 200),
                  let image = UIImage(data: data.data) else {
                artwork = nil
                loadedArtworkKey = key
                return
            }
            artwork = image
            loadedArtworkKey = key
        } catch {
            // Missing art is not worth surfacing as a connection error.
            artwork = nil
        }
    }

    private func startPolling() {
        pollTask?.cancel()
        pollTask = Task { [weak self] in
            while !Task.isCancelled {
                await self?.refreshNowPlaying()
                // 2s matches the server's own aggregator poll interval; faster
                // buys nothing but battery.
                try? await Task.sleep(for: .seconds(2))
            }
        }
    }

    func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
    }

    // MARK: - Commands

    func playPause() async {
        guard let zoneID = selectedZoneID else { return }
        // Optimistic: the watch should feel instant, and the next poll
        // reconciles against the server's truth.
        if var np = nowPlaying {
            np.isPlaying.toggle()
            nowPlaying = np
        }
        await send { try await $0.playPause(zoneID: zoneID) }
    }

    func next() async {
        guard let zoneID = selectedZoneID else { return }
        await send { try await $0.next(zoneID: zoneID) }
    }

    func previous() async {
        guard let zoneID = selectedZoneID else { return }
        await send { try await $0.previous(zoneID: zoneID) }
    }

    /// Where the crown is pointing right now, before the server has agreed.
    /// While the user is turning, THIS is the truth the readout shows: asking
    /// the server mid-turn returns a value from before the write landed, and
    /// rendering that is what made the number jump backwards.
    private(set) var pendingVolume: Double?
    /// The last value the server confirmed, used to skip writes that would ask
    /// a zone for the volume it already has.
    private var confirmedVolume: Double?

    /// Absolute volume from the Digital Crown, snapped to the zone's own step.
    ///
    /// The crown is a continuous input, so the server is told the destination,
    /// not the journey: the caller debounces, this commits once, and no read
    /// is issued afterwards. The poller reconciles in its own time.
    ///
    /// Returns whether the value actually reached the server, so the view can
    /// confirm it to the wrist.
    @discardableResult
    func commitVolume(_ value: Double) async -> Bool {
        guard let zoneID = selectedZoneID, let client else { return false }
        let control = nowPlaying?.volumeControl
            ?? zones.first(where: { $0.id == zoneID })?.volumeControl
        let target = control?.normalized(value) ?? value

        // Writing the value a zone already holds produces no readback, so the
        // server waits for a change event that never comes and eventually
        // 500s (#621). At the crown that is not an edge case -- a hand that
        // stops on the current value is ordinary -- so never send it.
        if let confirmed = confirmedVolume, confirmed == target {
            pendingVolume = nil
            return true
        }

        pendingVolume = target
        if var np = nowPlaying {
            np.volume = target
            nowPlaying = np
        }
        // Long enough for the write and its readback, which the poller must
        // not overwrite in the meantime.
        suppressPollUntil = Date().addingTimeInterval(1.5)
        do {
            try await client.setVolume(zoneID: zoneID, to: target, within: control)
            confirmedVolume = target
            pendingVolume = nil
            errorMessage = nil
            return true
        } catch {
            pendingVolume = nil
            recordFailure(error)
            // Only now is a read worth making: the local value is wrong and
            // the user needs to see where the zone actually sits.
            await refreshNowPlaying()
            return false
        }
    }

    private func send(_ body: @escaping (UHCClient) async throws -> Void) async {
        guard let client else { return }
        // Hold off the poller briefly so it cannot overwrite the optimistic
        // state with a snapshot taken before the command was applied.
        suppressPollUntil = Date().addingTimeInterval(0.8)
        do {
            try await body(client)
            errorMessage = nil
        } catch {
            recordFailure(error)
        }
        await refreshNowPlaying()
    }

    private func recordFailure(_ error: Error) {
        let uhc = error as? UHCError
        errorMessage = uhc?.errorDescription ?? error.localizedDescription
        if case .unreachable(let underlying) = uhc {
            lastTransportFailure = underlying
        } else if case .timedOut = uhc {
            lastTransportFailure = "timed out"
        }
    }

    // MARK: - Derived

    var selectedZone: Zone? {
        zones.first { $0.id == selectedZoneID }
    }

    var volumeControl: VolumeControl? {
        nowPlaying?.volumeControl ?? selectedZone?.volumeControl
    }
}
