import Foundation

#if canImport(Security)
import Security
#endif

/// State that a signed macOS host may restore after an app restart. The
/// bearer is only the paired UHC bridge credential, never an Apple token.
@available(macOS 14.0, *)
public struct AppleMusicCompanionInstallation: Codable, Sendable, Equatable {
    public let baseURL: URL
    public let companionID: String
    public var bridgeID: String?
    public var accessToken: String?
    /// Bounded command outcomes survive a host restart so at-least-once bridge
    /// redelivery does not repeat a non-idempotent transport command.
    public var commandOutcomes: [String: Bool]

    public init(baseURL: URL, companionID: String, bridgeID: String? = nil, accessToken: String? = nil, commandOutcomes: [String: Bool] = [:]) {
        self.baseURL = baseURL
        self.companionID = companionID
        self.bridgeID = bridgeID
        self.accessToken = accessToken
        self.commandOutcomes = commandOutcomes
    }

    private enum CodingKeys: String, CodingKey {
        case baseURL, companionID, bridgeID, accessToken, commandOutcomes
    }

    public init(from decoder: Decoder) throws {
        let values = try decoder.container(keyedBy: CodingKeys.self)
        baseURL = try values.decode(URL.self, forKey: .baseURL)
        companionID = try values.decode(String.self, forKey: .companionID)
        bridgeID = try values.decodeIfPresent(String.self, forKey: .bridgeID)
        accessToken = try values.decodeIfPresent(String.self, forKey: .accessToken)
        commandOutcomes = try values.decodeIfPresent([String: Bool].self, forKey: .commandOutcomes) ?? [:]
    }
}

@available(macOS 14.0, *)
public protocol AppleMusicCompanionInstallationStore: Sendable {
    func load() -> AppleMusicCompanionInstallation?
    func save(_ installation: AppleMusicCompanionInstallation) throws
    func clear() throws
}

@available(macOS 14.0, *)
public final class InMemoryAppleMusicCompanionInstallationStore: @unchecked Sendable, AppleMusicCompanionInstallationStore {
    private let lock = NSLock()
    private var value: AppleMusicCompanionInstallation?

    public init(_ value: AppleMusicCompanionInstallation? = nil) { self.value = value }
    public func load() -> AppleMusicCompanionInstallation? { lock.withLock { value } }
    public func save(_ installation: AppleMusicCompanionInstallation) throws { lock.withLock { value = installation } }
    public func clear() throws { lock.withLock { value = nil } }
}

@available(macOS 14.0, *)
public enum AppleMusicCompanionInstallationStoreError: Error, LocalizedError, Sendable {
    case unavailable

    public var errorDescription: String? { "The platform secure store is unavailable." }
}

/// Keychain-backed persistence for the signed macOS host.
@available(macOS 14.0, *)
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
        let status = SecItemUpdate(baseQuery as CFDictionary, [kSecValueData as String: data] as CFDictionary)
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
