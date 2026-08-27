import XCTest
@testable import UHCKit

/// Client behaviour, exercised through a stub transport.
///
/// The transport seam exists so the Watch can swap direct-WiFi for a phone
/// relay without touching the client; the same seam makes the client testable
/// without a server, which is a good sign the seam is in the right place.
final class UHCClientTests: XCTestCase {
    /// Records what the client asked for and replays canned responses.
    final class StubTransport: UHCTransport, @unchecked Sendable {
        var requests: [UHCRequest] = []
        var responses: [UHCResponse]
        var error: UHCError?

        init(responses: [UHCResponse] = [], error: UHCError? = nil) {
            self.responses = responses
            self.error = error
        }

        var describedEndpoint: String { "stub" }

        func perform(_ request: UHCRequest) async throws -> UHCResponse {
            requests.append(request)
            if let error { throw error }
            guard !responses.isEmpty else {
                return UHCResponse(statusCode: 200, body: Data("{}".utf8), contentType: "application/json")
            }
            return responses.removeFirst()
        }
    }

    private func json(_ raw: String, status: Int = 200) -> UHCResponse {
        UHCResponse(statusCode: status, body: Data(raw.utf8), contentType: "application/json")
    }

    // MARK: - Requests

    func testNowPlayingSendsZoneIDAsAQueryParameter() async throws {
        let transport = StubTransport(responses: [json(#"{"zone_id":"roon:1","zones":[]}"#)])
        let client = UHCClient(transport: transport)

        _ = try await client.nowPlaying(zoneID: "roon:1601abc")

        let request = try XCTUnwrap(transport.requests.first)
        XCTAssertEqual(request.method, .get)
        XCTAssertEqual(request.path, "/now_playing")
        XCTAssertEqual(request.query["zone_id"], "roon:1601abc")
    }

    /// `width`/`height` — not `size`. Getting this wrong yields a correctly
    /// sized-looking request that the server silently ignores.
    func testArtworkRequestsWidthAndHeight() async throws {
        let transport = StubTransport(responses: [
            UHCResponse(statusCode: 200, body: Data([0xFF, 0xD8, 0xFF]), contentType: "image/jpeg")
        ])
        let client = UHCClient(transport: transport)

        let art = try await client.artwork(zoneID: "roon:1", width: 200, height: 120)

        let request = try XCTUnwrap(transport.requests.first)
        XCTAssertEqual(request.path, "/now_playing/image")
        XCTAssertEqual(request.query["width"], "200")
        XCTAssertEqual(request.query["height"], "120")
        XCTAssertNil(request.query["size"], "the server has no `size` parameter")
        XCTAssertEqual(art?.data.count, 3)
    }

    /// The server answers 200 with an SVG placeholder when it has no art.
    /// `UIImage` cannot decode SVG, so the client must report "no artwork"
    /// rather than handing the UI bytes it will fail to render.
    func testSVGPlaceholderIsReportedAsNoArtwork() async throws {
        let transport = StubTransport(responses: [
            UHCResponse(
                statusCode: 200,
                body: Data("<svg xmlns=\"http://www.w3.org/2000/svg\"/>".utf8),
                contentType: "image/svg+xml"
            )
        ])
        let client = UHCClient(transport: transport)

        let art = try await client.artwork(zoneID: "roon:1")
        XCTAssertNil(art, "an SVG placeholder is not artwork the platform can draw")
    }

    func testControlPostsTheCanonicalBody() async throws {
        let transport = StubTransport(responses: [json(#"{"ok":true}"#)])
        let client = UHCClient(transport: transport)

        try await client.control(zoneID: "roon:1601abc", action: .volumeAbsolute, value: 31.5)

        let request = try XCTUnwrap(transport.requests.first)
        XCTAssertEqual(request.method, .post)
        XCTAssertEqual(request.path, "/control")

        let body = try XCTUnwrap(request.body)
        let object = try XCTUnwrap(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        XCTAssertEqual(object["zone_id"] as? String, "roon:1601abc")
        XCTAssertEqual(object["action"] as? String, "vol_abs")
        XCTAssertEqual(object["value"] as? Double, 31.5)
    }

    // MARK: - Errors

    func testStructuredServerErrorsSurfaceTheirCode() async throws {
        let transport = StubTransport(responses: [
            json(#"{"error":"zone not found","error_code":"ZONE_NOT_FOUND"}"#, status: 404)
        ])
        let client = UHCClient(transport: transport)

        do {
            _ = try await client.nowPlaying(zoneID: "roon:nope")
            XCTFail("expected a server error")
        } catch let error as UHCError {
            guard case .server(let status, let message, let code) = error else {
                return XCTFail("expected .server, got \(error)")
            }
            XCTAssertEqual(status, 404)
            XCTAssertEqual(message, "zone not found")
            XCTAssertEqual(code, "ZONE_NOT_FOUND")
        }
    }

    /// A non-2xx must not be reported as a decoding failure — on a watch the
    /// difference decides whether the user is told "can't reach UHC" or
    /// "that zone is gone".
    func testTransportFailuresPropagate() async throws {
        let transport = StubTransport(error: .unreachable(underlying: "-1004 could not connect"))
        let client = UHCClient(transport: transport)

        do {
            _ = try await client.zones()
            XCTFail("expected a transport error")
        } catch let error as UHCError {
            guard case .unreachable(let underlying) = error else {
                return XCTFail("expected .unreachable, got \(error)")
            }
            XCTAssertTrue(underlying.contains("-1004"))
        }
    }

    // MARK: - Volume

    /// Zones publish genuinely different ranges and steps (0…98 by 0.5 and
    /// 0…42 by 1 live side by side), so the crown's raw value must be snapped
    /// to the zone it is driving.
    func testVolumeIsClampedAndSnappedToTheZoneRange() {
        let control = VolumeControl(value: 40, min: 0, max: 98, step: 0.5)

        XCTAssertEqual(control.normalized(47.4), 47.5)
        XCTAssertEqual(control.normalized(47.6), 47.5)
        XCTAssertEqual(control.normalized(-10), 0, "below range clamps to min")
        XCTAssertEqual(control.normalized(1000), 98, "above range clamps to max")

        let coarse = VolumeControl(value: 5, min: 0, max: 42, step: 1)
        XCTAssertEqual(coarse.normalized(5.4), 5)
        XCTAssertEqual(coarse.normalized(5.6), 6)
        XCTAssertEqual(coarse.normalized(41.9), 42)
    }

    func testSetVolumeSendsTheSnappedValue() async throws {
        let transport = StubTransport(responses: [json(#"{"ok":true}"#)])
        let client = UHCClient(transport: transport)
        let control = VolumeControl(value: 40, min: 0, max: 98, step: 0.5)

        try await client.setVolume(zoneID: "roon:1", to: 47.42, within: control)

        let body = try XCTUnwrap(transport.requests.first?.body)
        let object = try XCTUnwrap(try JSONSerialization.jsonObject(with: body) as? [String: Any])
        XCTAssertEqual(object["value"] as? Double, 47.5)
    }

    // MARK: - Transports

    /// The relay is a documented placeholder. It must fail loudly and
    /// immediately rather than hang, so wiring it up by mistake is obvious.
    func testPhoneRelayTransportFailsLoudly() async {
        let relay = PhoneRelayTransport()
        do {
            _ = try await relay.perform(UHCRequest(path: "/zones"))
            XCTFail("the relay is not implemented and must not appear to work")
        } catch let error as UHCError {
            guard case .transportUnavailable = error else {
                return XCTFail("expected .transportUnavailable, got \(error)")
            }
        } catch {
            XCTFail("expected UHCError, got \(error)")
        }
    }
}
