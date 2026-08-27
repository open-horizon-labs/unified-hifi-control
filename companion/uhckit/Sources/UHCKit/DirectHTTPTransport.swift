import Foundation

/// Talks to UHC straight over the LAN with `URLSession`.
///
/// This is the transport whose viability on watchOS is the open question of
/// #619. It is written to make that question *answerable* rather than to guess
/// at the answer: a refused or blocked LAN connection surfaces as
/// `UHCError.unreachable` carrying the underlying `URLError` code, which is the
/// evidence the owner needs to report from real hardware. See `Transport.md`.
public final class DirectHTTPTransport: UHCTransport, @unchecked Sendable {
    private let baseURL: URL
    private let session: URLSession
    private let defaultTimeout: TimeInterval

    public init(baseURL: URL, timeout: TimeInterval = 8, session: URLSession? = nil) {
        self.baseURL = baseURL
        self.defaultTimeout = timeout
        if let session {
            self.session = session
        } else {
            let config = URLSessionConfiguration.ephemeral
            config.timeoutIntervalForRequest = timeout
            config.timeoutIntervalForResource = timeout * 2
            // A controller wants the live state, never a cached one. Artwork is
            // re-fetched only when `image_key` changes, so the cache buys
            // nothing and can show a stale track.
            config.requestCachePolicy = .reloadIgnoringLocalCacheData
            config.waitsForConnectivity = false
            self.session = URLSession(configuration: config)
        }
    }

    public var describedEndpoint: String {
        baseURL.absoluteString
    }

    public func perform(_ request: UHCRequest) async throws -> UHCResponse {
        guard var components = URLComponents(url: baseURL, resolvingAgainstBaseURL: false) else {
            throw UHCError.invalidBaseURL
        }

        // Append rather than replace: a base URL may legitimately carry a path
        // prefix when UHC sits behind a reverse proxy, and dropping it is
        // exactly the bug class `tests/base_path_lint.rs` guards on the Rust
        // side (#611).
        let basePath = components.path.hasSuffix("/")
            ? String(components.path.dropLast())
            : components.path
        components.path = basePath + request.path

        if !request.query.isEmpty {
            // Sorted so a request is reproducible and comparable in logs.
            components.queryItems = request.query
                .sorted { $0.key < $1.key }
                .map { URLQueryItem(name: $0.key, value: $0.value) }
        }

        guard let url = components.url else { throw UHCError.invalidBaseURL }

        var urlRequest = URLRequest(url: url)
        urlRequest.httpMethod = request.method.rawValue
        urlRequest.timeoutInterval = request.timeout ?? defaultTimeout
        if let body = request.body {
            urlRequest.httpBody = body
            urlRequest.setValue("application/json", forHTTPHeaderField: "Content-Type")
        }

        do {
            let (data, response) = try await session.data(for: urlRequest)
            let http = response as? HTTPURLResponse
            return UHCResponse(
                statusCode: http?.statusCode ?? 0,
                body: data,
                contentType: http?.value(forHTTPHeaderField: "Content-Type")
            )
        } catch let error as URLError {
            if error.code == .timedOut { throw UHCError.timedOut }
            // The distinguishing detail for the on-device LAN question. Keep
            // the raw code: on watchOS a policy block and a genuinely absent
            // server look similar to a user but differ here.
            throw UHCError.unreachable(underlying: "\(error.code.rawValue) \(error.localizedDescription)")
        } catch {
            throw UHCError.unreachable(underlying: error.localizedDescription)
        }
    }
}
