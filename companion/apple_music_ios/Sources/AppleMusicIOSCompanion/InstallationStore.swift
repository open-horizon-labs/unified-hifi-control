import Foundation

#if canImport(Security)
import Security
#endif

/// The small set of installation state that may survive an app restart. The
/// bearer is a bridge credential, never an Apple Music credential, and must be
/// kept in the platform secure store by a real host app.
@available(iOS 17.0, *)
public struct AppleMusicCompanionInstallation: Codable, Sendable, Equatable {
    public let baseURL: URL
    public let companionID: String
    public var bridgeID: String?
    public var accessToken: String?
    /// Bounded command outcomes survive a host restart so at-least-once bridge
    /// redelivery does not repeat a non-idempotent transport command.
    public var commandOutcomes: [String: Bool]
    /// Bounded content outcomes survive a host restart. Content delivery is
    /// at-least-once, so a playlist or queue mutation must be acknowledged
    /// from the persisted result rather than executed again after redelivery.
    public var contentOutcomes: [String: MusicKitContentResult]

    public init(baseURL: URL, companionID: String, bridgeID: String? = nil, accessToken: String? = nil, commandOutcomes: [String: Bool] = [:], contentOutcomes: [String: MusicKitContentResult] = [:]) {
        self.baseURL = baseURL
        self.companionID = companionID
        self.bridgeID = bridgeID
        self.accessToken = accessToken
        self.commandOutcomes = commandOutcomes
        self.contentOutcomes = contentOutcomes
    }

    private enum CodingKeys: String, CodingKey {
        case baseURL, companionID, bridgeID, accessToken, commandOutcomes, contentOutcomes
    }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        baseURL = try values.decode(URL.self, forKey: .baseURL)
        companionID = try values.decode(String.self, forKey: .companionID)
        bridgeID = try values.decodeIfPresent(String.self, forKey: .bridgeID)
        accessToken = try values.decodeIfPresent(String.self, forKey: .accessToken)
        commandOutcomes = try values.decodeIfPresent([String: Bool].self, forKey: .commandOutcomes) ?? [:]
        contentOutcomes = try values.decodeIfPresent([String: MusicKitContentResult].self, forKey: .contentOutcomes) ?? [:]
    }
}

/// Injectable persistence boundary for signed hosts. The in-memory store is
/// useful for previews/tests; production apps should use KeychainStore.
@available(iOS 17.0, *)
public protocol AppleMusicCompanionInstallationStore: Sendable {
    func load() -> AppleMusicCompanionInstallation?
    func save(_ installation: AppleMusicCompanionInstallation) throws
    func clear() throws
}

@available(iOS 17.0, *)
public final class InMemoryAppleMusicCompanionInstallationStore: @unchecked Sendable, AppleMusicCompanionInstallationStore {
    private let lock = NSLock()
    private var value: AppleMusicCompanionInstallation?

    public init(_ value: AppleMusicCompanionInstallation? = nil) { self.value = value }

    public func load() -> AppleMusicCompanionInstallation? { lock.withLock { value } }
    public func save(_ installation: AppleMusicCompanionInstallation) throws { lock.withLock { value = installation } }
    public func clear() throws { lock.withLock { value = nil } }
}

@available(iOS 17.0, *)
public enum AppleMusicCompanionInstallationStoreError: Error, LocalizedError, Sendable {
    case unavailable
    case invalidData

    public var errorDescription: String? {
        switch self {
        case .unavailable: "The platform secure store is unavailable."
        case .invalidData: "The saved companion installation is invalid."
        }
    }
}

/// Keychain-backed persistence for the signed iOS host. Access tokens are
/// stored as generic-password data and never appear in UserDefaults/logs.
@available(iOS 17.0, *)
public final class KeychainAppleMusicCompanionInstallationStore: @unchecked Sendable, AppleMusicCompanionInstallationStore {
    private let service: String
    private let account: String

    public init(service: String = "com.openhorizon.uhc.apple-music", account: String = "installation") {
        self.service = service
        self.account = account
    }

    public func load() -> AppleMusicCompanionInstallation? {
        #if canImport(Security)
        var query = baseQuery
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
              let data = result as? Data else { return nil }
        return try? JSONDecoder().decode(AppleMusicCompanionInstallation.self, from: data)
        #else
        return nil
        #endif
    }

    public func save(_ installation: AppleMusicCompanionInstallation) throws {
        #if canImport(Security)
        let data = try JSONEncoder().encode(installation)
        let update: [String: Any] = [kSecValueData as String: data]
        let status = SecItemUpdate(baseQuery as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var item = baseQuery
            item[kSecValueData as String] = data
            item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
            guard SecItemAdd(item as CFDictionary, nil) == errSecSuccess else { throw AppleMusicCompanionInstallationStoreError.unavailable }
        } else if status != errSecSuccess {
            throw AppleMusicCompanionInstallationStoreError.unavailable
        }
        #else
        throw AppleMusicCompanionInstallationStoreError.unavailable
        #endif
    }

    public func clear() throws {
        #if canImport(Security)
        let status = SecItemDelete(baseQuery as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw AppleMusicCompanionInstallationStoreError.unavailable }
        #else
        throw AppleMusicCompanionInstallationStoreError.unavailable
        #endif
    }

    #if canImport(Security)
    private var baseQuery: [String: Any] {
        [kSecClass as String: kSecClassGenericPassword, kSecAttrService as String: service, kSecAttrAccount as String: account]
    }
    #endif
}

private extension NSLock {
    func withLock<T>(_ body: () -> T) -> T { lock(); defer { unlock() }; return body() }
}
