import XCTest
@testable import UHCKit

/// The Swift half of the UHCKit wire contract (#619).
///
/// Reads the *same* file as `tests/uhckit_contract.rs` — not a copy of it.
/// That is the whole mechanism: the Rust test proves the fixture still matches
/// what the server emits, and this test proves the Swift models still decode
/// the fixture. Neither side can drift without one of the two going red.
///
/// If this test cannot find the fixture, that is a real failure and not a
/// reason to skip: it means the file moved and the Rust test is now guarding
/// something this client never sees.
final class ContractTests: XCTestCase {
    // MARK: - Fixture plumbing

    /// Walks up from this source file to the repository root.
    ///
    /// `Bundle.module` would be the idiomatic route, but that needs the fixture
    /// copied into the package — and a copy is exactly what this design is
    /// avoiding. The path is resolved from `#filePath` so it breaks loudly if
    /// the layout changes.
    static func fixtureURL() throws -> URL {
        let thisFile = URL(fileURLWithPath: #filePath)
        // …/companion/uhckit/Tests/UHCKitTests/ContractTests.swift
        let repoRoot = thisFile
            .deletingLastPathComponent()  // UHCKitTests
            .deletingLastPathComponent()  // Tests
            .deletingLastPathComponent()  // uhckit
            .deletingLastPathComponent()  // companion
            .deletingLastPathComponent()  // <repo root>
        let url = repoRoot
            .appendingPathComponent("tests/fixtures/uhckit_contract.json")

        guard FileManager.default.fileExists(atPath: url.path) else {
            throw XCTSkip(
                """
                Contract fixture not found at \(url.path).
                It is shared with tests/uhckit_contract.rs; if it moved, update both.
                """
            )
        }
        return url
    }

    private func fixture() throws -> [String: Any] {
        let data = try Data(contentsOf: try Self.fixtureURL())
        guard let object = try JSONSerialization.jsonObject(with: data) as? [String: Any] else {
            throw ContractError.malformed("top level is not an object")
        }
        return object
    }

    /// Re-encodes a fixture subtree so it can be fed to `JSONDecoder`.
    private func data(at path: [String], in fixture: [String: Any]) throws -> Data {
        var current: Any = fixture
        for key in path {
            guard let dict = current as? [String: Any], let next = dict[key] else {
                throw ContractError.malformed("missing key path \(path.joined(separator: "."))")
            }
            current = next
        }
        return try JSONSerialization.data(withJSONObject: current)
    }

    private func strings(at path: [String], in fixture: [String: Any]) throws -> [String] {
        var current: Any = fixture
        for key in path {
            guard let dict = current as? [String: Any], let next = dict[key] else {
                throw ContractError.malformed("missing key path \(path.joined(separator: "."))")
            }
            current = next
        }
        guard let array = current as? [String] else {
            throw ContractError.malformed("\(path.joined(separator: ".")) is not an array of strings")
        }
        return array
    }

    enum ContractError: Error { case malformed(String) }

    // MARK: - Zones

    func testFullZoneDecodes() throws {
        let zone = try JSONDecoder().decode(Zone.self, from: try data(at: ["zone", "full"], in: try fixture()))

        XCTAssertEqual(zone.id, "roon:1601deadbeef")
        XCTAssertEqual(zone.name, "Front Family Room")
        XCTAssertEqual(zone.source, "roon")
        XCTAssertEqual(zone.state, .playing)
        XCTAssertTrue(zone.browseSupported)
        XCTAssertEqual(zone.libraryTabs, ["browse", "playlists"])

        let volume = try XCTUnwrap(zone.volumeControl)
        XCTAssertEqual(volume.value, 47.5)
        XCTAssertEqual(volume.min, 0)
        XCTAssertEqual(volume.max, 98)
        XCTAssertEqual(volume.step, 0.5)
        XCTAssertFalse(volume.isMuted)
        XCTAssertEqual(volume.scale, "percentage")
        XCTAssertEqual(volume.outputID, "roon:1701deadbeef")

        XCTAssertEqual(zone.dsp?.type, "hqplayer")
        XCTAssertEqual(zone.dsp?.instance, "embedded")
    }

    /// The case that would break a naive model: the live server publishes a
    /// zone with no `volume_control` key at all.
    func testZoneWithoutVolumeControlDecodes() throws {
        let zone = try JSONDecoder().decode(Zone.self, from: try data(at: ["zone", "minimal"], in: try fixture()))

        XCTAssertEqual(zone.name, "HQPlayer Embedded")
        XCTAssertNil(zone.volumeControl, "a zone with no volume must decode, not throw")
        XCTAssertNil(zone.dsp)
        XCTAssertEqual(zone.state, .stopped)
    }

    /// Every state string the server can emit must map to a case, and an
    /// unrecognised one must degrade rather than throw — a new adapter should
    /// not be able to take down the zone list on an already-installed Watch.
    func testEveryServerStateDecodes() throws {
        let states = try strings(at: ["zone", "states", "values"], in: try fixture())
        XCTAssertFalse(states.isEmpty)

        for raw in states {
            let state = PlaybackState(rawValue: raw)
            XCTAssertEqual(state.rawValue, raw, "state `\(raw)` did not round-trip")
            if case .other = state {
                XCTFail("state `\(raw)` is published by the server but unknown to UHCKit")
            }
        }

        let future = PlaybackState(rawValue: "teleporting")
        XCTAssertEqual(future, .other("teleporting"))
        XCTAssertFalse(future.isPlaying)
    }

    // MARK: - Now playing

    func testNowPlayingDecodes() throws {
        let np = try JSONDecoder().decode(NowPlaying.self, from: try data(at: ["now_playing"], in: try fixture()))

        XCTAssertEqual(np.zoneID, "roon:1601deadbeef")
        XCTAssertEqual(np.title, "Summer (B-Sides)")
        XCTAssertEqual(np.artist, "Moby")
        XCTAssertEqual(np.album, "Play & Play: The B Sides")
        XCTAssertTrue(np.isPlaying)
        XCTAssertEqual(np.volume, 47.5)
        XCTAssertEqual(np.volumeType, "number")
        XCTAssertEqual(np.imageKey, "b7df27c8dd25dd65084d8f2d94f616d0")
        XCTAssertEqual(np.seekPosition, 104)
        XCTAssertEqual(np.length, 358)
        XCTAssertEqual(np.zonesSHA, "4bb54fbe")
        XCTAssertNil(np.configSHA, "config_sha is null for non-knob clients")

        // Capability flags gate the UI's buttons; a mis-mapped pair would show
        // a control that is guaranteed to fail.
        XCTAssertFalse(np.isPlayAllowed)
        XCTAssertTrue(np.isPauseAllowed)
        XCTAssertTrue(np.isNextAllowed)
        XCTAssertTrue(np.isPreviousAllowed)
    }

    /// `/now_playing` carries enough to drive the crown without a second
    /// request to `/zones`.
    func testNowPlayingProjectsAVolumeControl() throws {
        let np = try JSONDecoder().decode(NowPlaying.self, from: try data(at: ["now_playing"], in: try fixture()))
        let volume = try XCTUnwrap(np.volumeControl)

        XCTAssertEqual(volume.value, 47.5)
        XCTAssertEqual(volume.min, 0)
        XCTAssertEqual(volume.max, 98)
        XCTAssertEqual(volume.step, 0.5)
    }

    func testErrorBodyDecodes() throws {
        let body = try JSONDecoder().decode(
            UHCErrorBody.self,
            from: try data(at: ["now_playing_error"], in: try fixture())
        )
        XCTAssertEqual(body.error, "zone not found")
        XCTAssertEqual(body.errorCode, "ZONE_NOT_FOUND")
    }

    // MARK: - Control

    /// Every action the Swift enum can emit must be one the fixture blesses,
    /// and the fixture is in turn checked against the server's match arms by
    /// `tests/uhckit_contract.rs`.
    func testControlActionsAreAllServerSupported() throws {
        let allowed = Set(try strings(at: ["control", "actions"], in: try fixture()))
        let emitted: [ControlAction] = [
            .play, .pause, .playPause, .stop, .next, .previous,
            .volumeUp, .volumeDown, .volumeAbsolute,
        ]

        for action in emitted {
            XCTAssertTrue(
                allowed.contains(action.rawValue),
                "UHCKit can send `\(action.rawValue)`, which the server contract does not list"
            )
        }
    }

    func testControlRequestEncodesToTheServerShape() throws {
        let request = ControlRequest(zoneID: "roon:1601deadbeef", action: .volumeAbsolute, value: 31.5)
        let encoded = try JSONEncoder().encode(request)
        let object = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: encoded) as? [String: Any]
        )

        let expected = try XCTUnwrap(
            try JSONSerialization.jsonObject(with: try data(at: ["control", "request"], in: try fixture()))
                as? [String: Any]
        )

        XCTAssertEqual(Set(object.keys), Set(expected.keys))
        XCTAssertEqual(object["zone_id"] as? String, expected["zone_id"] as? String)
        XCTAssertEqual(object["action"] as? String, expected["action"] as? String)
        XCTAssertEqual(object["value"] as? Double, expected["value"] as? Double)
    }
}
