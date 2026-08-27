import Foundation

/// The UHC control client: zones, now-playing, transport, volume, artwork.
///
/// Holds a `UHCTransport` and nothing else about how bytes travel, so the same
/// client serves the Watch over WiFi today and over a phone relay later without
/// changing a line here or in any view.
public final class UHCClient: @unchecked Sendable {
    public let transport: UHCTransport
    private let decoder = JSONDecoder()
    private let encoder = JSONEncoder()

    public init(transport: UHCTransport) {
        self.transport = transport
    }

    /// Convenience for the common case: direct LAN HTTP to `baseURL`.
    public convenience init(baseURL: URL, timeout: TimeInterval = 8) {
        self.init(transport: DirectHTTPTransport(baseURL: baseURL, timeout: timeout))
    }

    public var describedEndpoint: String { transport.describedEndpoint }

    // MARK: - Reads

    /// `GET /zones`
    public func zones() async throws -> [Zone] {
        let response = try await transport.perform(UHCRequest(path: "/zones"))
        return try decodeJSON(ZoneListResponse.self, from: response).zones
    }

    /// `GET /now_playing?zone_id=…`
    ///
    /// The response also carries the full zone list, so a polling controller can
    /// refresh both the picker and the current track in one round trip — which
    /// on a watch is the difference between one radio wake and two.
    public func nowPlaying(zoneID: String) async throws -> NowPlaying {
        let response = try await transport.perform(
            UHCRequest(path: "/now_playing", query: ["zone_id": zoneID])
        )
        return try decodeJSON(NowPlaying.self, from: response)
    }

    /// `GET /status` — a cheap liveness probe that touches no backend.
    /// Used by the Watch's connection check because it cannot be confused with
    /// a zone-level failure.
    @discardableResult
    public func status() async throws -> ServerStatus {
        let response = try await transport.perform(UHCRequest(path: "/status", timeout: 5))
        return try decodeJSON(ServerStatus.self, from: response)
    }

    // MARK: - Artwork

    /// Fetch album art for a zone.
    ///
    /// The server never 404s here: when it has no art it returns an
    /// `image/svg+xml` placeholder with a 200. `UIImage`/`WKInterfaceImage`
    /// cannot decode SVG, so a caller that trusted the status code alone would
    /// render an empty box and have no idea why. This method reports that case
    /// as `nil` and lets the UI draw its own placeholder.
    ///
    /// `width`/`height` are the real parameter names — there is no `size`.
    public func artwork(zoneID: String, width: Int = 240, height: Int = 240) async throws -> ArtworkData? {
        let response = try await transport.perform(
            UHCRequest(
                path: "/now_playing/image",
                query: [
                    "zone_id": zoneID,
                    "width": String(width),
                    "height": String(height),
                ],
                timeout: 15
            )
        )

        guard response.isSuccess else {
            throw serverError(from: response)
        }

        let type = response.contentType?.lowercased() ?? ""
        if type.contains("svg") {
            return nil  // placeholder, not real art
        }
        guard !response.body.isEmpty else { return nil }
        return ArtworkData(data: response.body, contentType: response.contentType)
    }

    // MARK: - Writes

    /// `POST /control`
    ///
    /// Read-modify-write is not used anywhere here: every action is a discrete
    /// command against a zone id, which is what makes the Watch safe to use
    /// while another surface is driving the same zone.
    public func control(zoneID: String, action: ControlAction, value: Double? = nil) async throws {
        let body = try encoder.encode(ControlRequest(zoneID: zoneID, action: action, value: value))
        let response = try await transport.perform(
            UHCRequest(method: .post, path: "/control", body: body)
        )
        guard response.isSuccess else { throw serverError(from: response) }
    }

    public func play(zoneID: String) async throws { try await control(zoneID: zoneID, action: .play) }
    public func pause(zoneID: String) async throws { try await control(zoneID: zoneID, action: .pause) }
    public func playPause(zoneID: String) async throws { try await control(zoneID: zoneID, action: .playPause) }
    public func next(zoneID: String) async throws { try await control(zoneID: zoneID, action: .next) }
    public func previous(zoneID: String) async throws { try await control(zoneID: zoneID, action: .previous) }

    /// Set absolute volume, clamped and step-snapped to the zone's own range.
    ///
    /// The crown produces a continuous value; zones publish steps of 0.5 and 1
    /// and ceilings of 42, 98 and 100 side by side on the live server. Snapping
    /// here means no caller has to remember to.
    public func setVolume(zoneID: String, to raw: Double, within control: VolumeControl?) async throws {
        let value = control?.normalized(raw) ?? raw
        try await self.control(zoneID: zoneID, action: .volumeAbsolute, value: value)
    }

    public func volumeUp(zoneID: String) async throws { try await control(zoneID: zoneID, action: .volumeUp) }
    public func volumeDown(zoneID: String) async throws { try await control(zoneID: zoneID, action: .volumeDown) }

    // MARK: - Plumbing

    private func decodeJSON<T: Decodable>(_ type: T.Type, from response: UHCResponse) throws -> T {
        guard response.isSuccess else { throw serverError(from: response) }
        do {
            return try decoder.decode(T.self, from: response.body)
        } catch {
            throw UHCError.decoding("\(T.self): \(error)")
        }
    }

    private func serverError(from response: UHCResponse) -> UHCError {
        let body = try? decoder.decode(UHCErrorBody.self, from: response.body)
        return .server(status: response.statusCode, message: body?.error, code: body?.errorCode)
    }
}

/// `GET /status`.
public struct ServerStatus: Codable, Sendable {
    public var service: String
    public var version: String
    public var roonConnected: Bool?

    enum CodingKeys: String, CodingKey {
        case service, version
        case roonConnected = "roon_connected"
    }
}

/// Raw artwork bytes plus the type the server labelled them with, so the
/// platform layer decides how to turn them into an image.
public struct ArtworkData: Sendable, Hashable {
    public var data: Data
    public var contentType: String?

    public init(data: Data, contentType: String? = nil) {
        self.data = data
        self.contentType = contentType
    }
}
