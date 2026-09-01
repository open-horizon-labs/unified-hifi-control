import Foundation

/// The one seam between "what the controller wants" and "how the bytes get
/// there" (#619).
///
/// The Watch may reach UHC two ways — straight over WiFi, or relayed through
/// the paired iPhone via WatchConnectivity — and which of those actually works
/// on real hardware is *unsettled* (see `Transport.md`). So the abstraction is
/// placed where swapping costs nothing: a transport moves one request and
/// returns one response, and knows nothing about zones, artwork or transport
/// controls. `UHCClient` holds all of that and is transport-agnostic.
///
/// The unit is deliberately raw bytes rather than a decoded model. Artwork is a
/// UHC request like any other, so a relay carries images through the same pipe
/// as JSON instead of needing a second, image-shaped hole punched through it.
public protocol UHCTransport: Sendable {
    /// Perform one request. Throws `UHCError` for transport-level failures;
    /// a non-2xx *server* response is returned normally, not thrown, so the
    /// caller can read structured error bodies.
    func perform(_ request: UHCRequest) async throws -> UHCResponse

    /// Human-readable description of where this transport points, for
    /// diagnostics on a screen too small for anything longer.
    var describedEndpoint: String { get }
}

/// A UHC request, expressed independently of any transport.
public struct UHCRequest: Sendable, Hashable {
    public enum Method: String, Sendable, Hashable {
        case get = "GET"
        case post = "POST"
    }

    public var method: Method
    /// Server-absolute, leading slash, no host: "/zones", "/now_playing".
    public var path: String
    public var query: [String: String]
    public var body: Data?
    /// Per-request override; nil means the transport's own default.
    public var timeout: TimeInterval?

    public init(
        method: Method = .get,
        path: String,
        query: [String: String] = [:],
        body: Data? = nil,
        timeout: TimeInterval? = nil
    ) {
        self.method = method
        self.path = path
        self.query = query
        self.body = body
        self.timeout = timeout
    }
}

/// What a transport hands back.
public struct UHCResponse: Sendable, Hashable {
    public var statusCode: Int
    public var body: Data
    public var contentType: String?

    public init(statusCode: Int, body: Data, contentType: String? = nil) {
        self.statusCode = statusCode
        self.body = body
        self.contentType = contentType
    }

    public var isSuccess: Bool { (200..<300).contains(statusCode) }
}

/// Failures a UHCKit call can produce.
public enum UHCError: Error, LocalizedError, Sendable {
    /// The request never reached a server. On watchOS this is the case that
    /// matters most: it is what a blocked LAN connection looks like.
    case unreachable(underlying: String)
    case timedOut
    /// The server answered with a non-2xx status.
    case server(status: Int, message: String?, code: String?)
    case decoding(String)
    /// A transport that exists but has not been implemented yet.
    case transportUnavailable(String)
    case invalidBaseURL

    public var errorDescription: String? {
        switch self {
        case .unreachable(let underlying):
            return "Cannot reach UHC: \(underlying)"
        case .timedOut:
            return "UHC did not respond in time."
        case .server(let status, let message, _):
            return message.map { "UHC error \(status): \($0)" } ?? "UHC error \(status)."
        case .decoding(let detail):
            return "Unexpected response from UHC: \(detail)"
        case .transportUnavailable(let detail):
            return detail
        case .invalidBaseURL:
            return "The UHC address is not a valid URL."
        }
    }
}

/// The structured error body UHC returns for 4xx on the controller endpoints,
/// e.g. `{"error":"zone not found","error_code":"ZONE_NOT_FOUND","zones":[…]}`.
struct UHCErrorBody: Decodable {
    var error: String?
    var errorCode: String?

    enum CodingKeys: String, CodingKey {
        case error
        case errorCode = "error_code"
    }
}
