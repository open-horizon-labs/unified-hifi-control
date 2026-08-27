import Foundation

// Wire models for UHC's source-agnostic controller protocol.
//
// Every field below was read off the running production server
// (192.168.1.209:8088, UHC 3.7.0-alpha.9) rather than inferred from the Rust
// types, and the Rust types were then read to find which fields are *omitted*
// rather than null — a distinction a single live sample cannot show. The
// endpoints are the ones the knob firmware already speaks:
//
//   GET  /zones                       -> ZoneListResponse
//   GET  /now_playing?zone_id=...     -> NowPlaying   (also carries `zones`)
//   GET  /now_playing/image?zone_id=  -> image/jpeg bytes
//   POST /control                     <- ControlRequest
//
// Optionality here is load-bearing, not defensive. `volume_control` really is
// absent for zones with no volume (the live server's "HQPlayer Embedded" zone
// is one), so making it non-optional would fail to decode the real zone list.

/// Transport state of a zone, as UHC's aggregator publishes it.
///
/// Unknown cases decode rather than throw: a new backend adapter teaching the
/// server a state string this client has never heard of must not take down the
/// zone list. The contract test is what catches that drift; runtime just copes.
public enum PlaybackState: RawRepresentable, Codable, Hashable, Sendable {
    case playing
    case paused
    case stopped
    case loading
    case buffering
    case unknown
    case other(String)

    public init(rawValue: String) {
        switch rawValue {
        case "playing": self = .playing
        case "paused": self = .paused
        case "stopped": self = .stopped
        case "loading": self = .loading
        case "buffering": self = .buffering
        case "unknown": self = .unknown
        default: self = .other(rawValue)
        }
    }

    public var rawValue: String {
        switch self {
        case .playing: return "playing"
        case .paused: return "paused"
        case .stopped: return "stopped"
        case .loading: return "loading"
        case .buffering: return "buffering"
        case .unknown: return "unknown"
        case .other(let raw): return raw
        }
    }

    public var isPlaying: Bool { self == .playing }
}

/// A zone's volume capability and current position within it.
///
/// `min`/`max`/`step` are per-zone and genuinely vary on real hardware — the
/// live server publishes ranges of 0…98 step 0.5 and 0…42 step 1 side by side —
/// so the Digital Crown must be bound to *this* range, never to a hardcoded
/// 0…100.
public struct VolumeControl: Codable, Hashable, Sendable {
    public var value: Double
    public var min: Double
    public var max: Double
    public var step: Double
    public var isMuted: Bool
    /// "percentage", "db", "number" — advisory, for display only.
    public var scale: String?
    /// The Roon *output* id, distinct from the zone id. Present for zones whose
    /// volume is addressed per-output.
    public var outputID: String?

    public init(
        value: Double,
        min: Double,
        max: Double,
        step: Double,
        isMuted: Bool = false,
        scale: String? = nil,
        outputID: String? = nil
    ) {
        self.value = value
        self.min = min
        self.max = max
        self.step = step
        self.isMuted = isMuted
        self.scale = scale
        self.outputID = outputID
    }

    enum CodingKeys: String, CodingKey {
        case value, min, max, step
        case isMuted = "is_muted"
        case scale
        case outputID = "output_id"
    }

    /// `value` clamped into `min...max` and snapped to `step`, which is what a
    /// crown-driven control must send: an unsnapped absolute volume is either
    /// rejected or silently rounded by the backend, and the two backends
    /// disagree about which.
    public func normalized(_ raw: Double) -> Double {
        guard step > 0, max > min else { return Swift.min(Swift.max(raw, min), max) }
        let clamped = Swift.min(Swift.max(raw, min), max)
        let snapped = (clamped / step).rounded() * step
        return Swift.min(Swift.max(snapped, min), max)
    }
}

/// One entry of `GET /zones`.
public struct Zone: Codable, Hashable, Identifiable, Sendable {
    /// Prefixed and globally unique: "roon:1601…", "lms:…", "hqplayer:…".
    /// The prefix is how `POST /control` routes to a backend, so it must be
    /// carried through verbatim and never stripped for display.
    public var id: String
    public var name: String
    /// "roon", "lms", "openhome", "upnp", "hqplayer", …
    public var source: String
    public var state: PlaybackState
    /// Absent for zones with no volume control at all.
    public var volumeControl: VolumeControl?
    /// Present only for zones fronted by an HQPlayer DSP pipeline. The Watch
    /// does not act on it; it is decoded so the type stays a faithful mirror of
    /// the wire and the contract test can see the field.
    public var dsp: DSPInfo?
    public var browseSupported: Bool
    public var libraryTabs: [String]

    public init(
        id: String,
        name: String,
        source: String,
        state: PlaybackState,
        volumeControl: VolumeControl? = nil,
        dsp: DSPInfo? = nil,
        browseSupported: Bool = false,
        libraryTabs: [String] = []
    ) {
        self.id = id
        self.name = name
        self.source = source
        self.state = state
        self.volumeControl = volumeControl
        self.dsp = dsp
        self.browseSupported = browseSupported
        self.libraryTabs = libraryTabs
    }

    enum CodingKeys: String, CodingKey {
        case id = "zone_id"
        case name = "zone_name"
        case source, state
        case volumeControl = "volume_control"
        case dsp
        case browseSupported = "browse_supported"
        case libraryTabs = "library_tabs"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        name = try c.decode(String.self, forKey: .name)
        source = try c.decode(String.self, forKey: .source)
        state = try c.decode(PlaybackState.self, forKey: .state)
        volumeControl = try c.decodeIfPresent(VolumeControl.self, forKey: .volumeControl)
        dsp = try c.decodeIfPresent(DSPInfo.self, forKey: .dsp)
        browseSupported = try c.decodeIfPresent(Bool.self, forKey: .browseSupported) ?? false
        libraryTabs = try c.decodeIfPresent([String].self, forKey: .libraryTabs) ?? []
    }
}

/// The `dsp` object a zone carries when an HQPlayer pipeline fronts it.
public struct DSPInfo: Codable, Hashable, Sendable {
    public var type: String
    public var instance: String?
    public var pipeline: String?
    public var profiles: String?

    public init(type: String, instance: String? = nil, pipeline: String? = nil, profiles: String? = nil) {
        self.type = type
        self.instance = instance
        self.pipeline = pipeline
        self.profiles = profiles
    }
}

/// `GET /zones`.
public struct ZoneListResponse: Codable, Sendable {
    public var zones: [Zone]

    public init(zones: [Zone]) { self.zones = zones }
}

/// `GET /now_playing?zone_id=…`.
///
/// The server names the text fields `line1`/`line2`/`line3` because the knob
/// firmware renders three lines without caring what they mean. They are in
/// practice title / artist / album, and this type exposes both spellings: the
/// positional names stay faithful to the wire, the semantic ones are what a
/// SwiftUI view should bind to.
public struct NowPlaying: Codable, Hashable, Sendable {
    public var zoneID: String
    public var line1: String?
    public var line2: String?
    public var line3: String?
    public var isPlaying: Bool
    public var volume: Double?
    /// "number" or "db" — how `volume` should be formatted.
    public var volumeType: String?
    public var volumeMin: Double?
    public var volumeMax: Double?
    public var volumeStep: Double?
    /// Server-built, server-relative artwork path. Never construct this
    /// client-side: #611 was four surfaces independently building artwork URLs
    /// and all four breaking at once, which is why `tests/base_path_lint.rs`
    /// exists. Resolve it against the base URL and otherwise leave it alone.
    public var imageURLPath: String?
    /// Changes when the art changes; the correct cache key.
    public var imageKey: String?
    public var seekPosition: Double?
    public var length: Double?
    public var isPlayAllowed: Bool
    public var isPauseAllowed: Bool
    public var isNextAllowed: Bool
    public var isPreviousAllowed: Bool
    /// The full zone list, echoed so a client can render a picker from one
    /// round trip instead of two.
    public var zones: [Zone]
    /// Short hash of the zone list. Cheap change detection for a polling
    /// client: unchanged sha means the picker need not be rebuilt.
    public var zonesSHA: String?
    /// Non-null only for knob clients that identify themselves; always null
    /// here. Decoded for wire fidelity.
    public var configSHA: String?

    public var title: String? { line1 }
    public var artist: String? { line2 }
    public var album: String? { line3 }

    /// Volume as a `VolumeControl`, so the crown binds to one type whether the
    /// value came from `/zones` or `/now_playing`.
    public var volumeControl: VolumeControl? {
        guard let volume else { return nil }
        return VolumeControl(
            value: volume,
            min: volumeMin ?? 0,
            max: volumeMax ?? 100,
            step: volumeStep ?? 1,
            isMuted: false,
            scale: volumeType
        )
    }

    enum CodingKeys: String, CodingKey {
        case zoneID = "zone_id"
        case line1, line2, line3
        case isPlaying = "is_playing"
        case volume
        case volumeType = "volume_type"
        case volumeMin = "volume_min"
        case volumeMax = "volume_max"
        case volumeStep = "volume_step"
        case imageURLPath = "image_url"
        case imageKey = "image_key"
        case seekPosition = "seek_position"
        case length
        case isPlayAllowed = "is_play_allowed"
        case isPauseAllowed = "is_pause_allowed"
        case isNextAllowed = "is_next_allowed"
        case isPreviousAllowed = "is_previous_allowed"
        case zones
        case zonesSHA = "zones_sha"
        case configSHA = "config_sha"
    }

    public init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        zoneID = try c.decode(String.self, forKey: .zoneID)
        line1 = try c.decodeIfPresent(String.self, forKey: .line1)
        line2 = try c.decodeIfPresent(String.self, forKey: .line2)
        line3 = try c.decodeIfPresent(String.self, forKey: .line3)
        isPlaying = try c.decodeIfPresent(Bool.self, forKey: .isPlaying) ?? false
        volume = try c.decodeIfPresent(Double.self, forKey: .volume)
        volumeType = try c.decodeIfPresent(String.self, forKey: .volumeType)
        volumeMin = try c.decodeIfPresent(Double.self, forKey: .volumeMin)
        volumeMax = try c.decodeIfPresent(Double.self, forKey: .volumeMax)
        volumeStep = try c.decodeIfPresent(Double.self, forKey: .volumeStep)
        imageURLPath = try c.decodeIfPresent(String.self, forKey: .imageURLPath)
        imageKey = try c.decodeIfPresent(String.self, forKey: .imageKey)
        seekPosition = try c.decodeIfPresent(Double.self, forKey: .seekPosition)
        length = try c.decodeIfPresent(Double.self, forKey: .length)
        isPlayAllowed = try c.decodeIfPresent(Bool.self, forKey: .isPlayAllowed) ?? false
        isPauseAllowed = try c.decodeIfPresent(Bool.self, forKey: .isPauseAllowed) ?? false
        isNextAllowed = try c.decodeIfPresent(Bool.self, forKey: .isNextAllowed) ?? false
        isPreviousAllowed = try c.decodeIfPresent(Bool.self, forKey: .isPreviousAllowed) ?? false
        zones = try c.decodeIfPresent([Zone].self, forKey: .zones) ?? []
        zonesSHA = try c.decodeIfPresent(String.self, forKey: .zonesSHA)
        configSHA = try c.decodeIfPresent(String.self, forKey: .configSHA)
    }

    public init(
        zoneID: String,
        line1: String? = nil,
        line2: String? = nil,
        line3: String? = nil,
        isPlaying: Bool = false,
        volume: Double? = nil,
        volumeType: String? = nil,
        volumeMin: Double? = nil,
        volumeMax: Double? = nil,
        volumeStep: Double? = nil,
        imageURLPath: String? = nil,
        imageKey: String? = nil,
        seekPosition: Double? = nil,
        length: Double? = nil,
        isPlayAllowed: Bool = false,
        isPauseAllowed: Bool = false,
        isNextAllowed: Bool = false,
        isPreviousAllowed: Bool = false,
        zones: [Zone] = [],
        zonesSHA: String? = nil,
        configSHA: String? = nil
    ) {
        self.zoneID = zoneID
        self.line1 = line1
        self.line2 = line2
        self.line3 = line3
        self.isPlaying = isPlaying
        self.volume = volume
        self.volumeType = volumeType
        self.volumeMin = volumeMin
        self.volumeMax = volumeMax
        self.volumeStep = volumeStep
        self.imageURLPath = imageURLPath
        self.imageKey = imageKey
        self.seekPosition = seekPosition
        self.length = length
        self.isPlayAllowed = isPlayAllowed
        self.isPauseAllowed = isPauseAllowed
        self.isNextAllowed = isNextAllowed
        self.isPreviousAllowed = isPreviousAllowed
        self.zones = zones
        self.zonesSHA = zonesSHA
        self.configSHA = configSHA
    }
}

/// The `action` strings `POST /control` accepts.
///
/// Spelled out rather than left as free strings so a typo is a compile error.
/// The server also accepts synonyms ("playpause", "prev", "volume_up",
/// "volume"); this client sends one canonical spelling per action.
public enum ControlAction: String, Codable, Sendable {
    case play
    case pause
    case playPause = "play_pause"
    case stop
    case next
    case previous
    case volumeUp = "vol_up"
    case volumeDown = "vol_down"
    /// Absolute volume. Requires `value`.
    case volumeAbsolute = "vol_abs"
}

/// The `POST /control` body.
public struct ControlRequest: Codable, Sendable {
    public var zoneID: String
    public var action: ControlAction
    /// Only meaningful for `.volumeAbsolute`; the server ignores it otherwise.
    public var value: Double?

    public init(zoneID: String, action: ControlAction, value: Double? = nil) {
        self.zoneID = zoneID
        self.action = action
        self.value = value
    }

    enum CodingKeys: String, CodingKey {
        case zoneID = "zone_id"
        case action, value
    }
}
