import Foundation
import Security
import UIKit

/// Persists this device's identity (PKCS8 DER bytes) in the real iOS
/// Keychain. Unlike the desktop shells (which call into `continuity-crypto`
/// directly and hit real cross-binary Keychain ACL friction when a second
/// unsigned dev binary touches the same item — see docs/protocol.md), this
/// works straightforwardly because Swift *is* the first-party app talking
/// to its own Keychain item, not a second process reading someone else's.
enum SecureIdentity {
    private static let service = "app.continuity.identity"
    private static let account = "device-signing-key"

    enum IdentityError: Error {
        case notFound
        case unexpectedStatus(OSStatus)
    }

    /// Loads the stored identity, generating and persisting a new one on
    /// first run.
    static func loadOrCreateIdentityDer() throws -> Data {
        if let existing = try? load() {
            return existing
        }
        let fresh = try generateIdentityDer()
        try save(fresh)
        return fresh
    }

    private static func load() throws -> Data {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
        ]
        var result: AnyObject?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        guard status == errSecSuccess, let data = result as? Data else {
            throw IdentityError.notFound
        }
        return data
    }

    private static func save(_ data: Data) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
        ]
        // Clear any stale entry first — SecItemAdd fails if one already exists.
        SecItemDelete(query as CFDictionary)

        var attributes = query
        attributes[kSecValueData as String] = data
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock

        let status = SecItemAdd(attributes as CFDictionary, nil)
        guard status == errSecSuccess else {
            throw IdentityError.unexpectedStatus(status)
        }
    }

    static func deviceName() -> String {
        let key = "app.continuity.deviceName"
        if let stored = UserDefaults.standard.string(forKey: key) {
            return stored
        }
        let name = UIDevice.current.name
        UserDefaults.standard.set(name, forKey: key)
        return name
    }
}
