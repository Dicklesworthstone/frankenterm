#if os(iOS)
import Foundation
import Security
import LocalAuthentication

/// Only transaction-owned accounts within this app's private service are touched.
/// No wildcard queries, access groups, synchronizable items, or keychain exports.
/// SecItem calls run on WorkspaceStore's actor, never a SwiftUI/MainActor callback.
public struct AppleWorkspaceVault: WorkspaceCredentialVault {
    private static let service = "org.frankenterm.mobile.workspace-keys.v1"
    public init() {}

    private func query(account: String) throws -> [String: Any] {
        let parts = account.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "ft-import", let transaction = UUID(uuidString: String(parts[1])),
              parts[1] == Substring(transaction.uuidString), let index = Int(parts[2]), (0..<256).contains(index),
              parts[2] == Substring(String(index)) else { throw FTError.malformed("vault.account") }
        return [kSecClass as String: kSecClassGenericPassword,
                kSecAttrService as String: Self.service,
                kSecAttrAccount as String: account,
                kSecAttrSynchronizable as String: false]
    }
    public func create(account: String, privateContainer: Data, fingerprint: String) throws {
        guard !privateContainer.isEmpty, privateContainer.count <= FTLimit.key,
              fingerprint.utf8.count == 50, fingerprint.hasPrefix("SHA256:") else { throw FTError.limit("vault.item") }
        var error: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly,
                                                          .userPresence, &error) else {
            _ = error?.takeRetainedValue(); throw FTError.storage
        }
        var item = try query(account: account)
        item[kSecAttrAccessControl as String] = access
        item[kSecAttrGeneric as String] = Data(fingerprint.utf8)
        item[kSecValueData as String] = privateContainer
        // Add only. A duplicate must never overwrite an identity or relax its ACL.
        guard SecItemAdd(item as CFDictionary, nil) == errSecSuccess else { throw FTError.storage }
    }
    public func fingerprint(account: String) throws -> String? {
        var request = try query(account: account)
        request[kSecReturnAttributes as String] = true
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        let context = LAContext()
        context.interactionNotAllowed = true
        request[kSecUseAuthenticationContext as String] = context
        var result: CFTypeRef?
        let status = SecItemCopyMatching(request as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        // Locked/unavailable is NOT absent and never authorizes orphan cleanup.
        guard status == errSecSuccess, let attributes = result as? [String: Any],
              let bytes = attributes[kSecAttrGeneric as String] as? Data,
              let value = String(data: bytes, encoding: .utf8), value.utf8.count == 50,
              value.hasPrefix("SHA256:") else { throw FTError.storage }
        return value
    }
    public func remove(account: String) throws {
        var request = try query(account: account)
        let context = LAContext(); context.interactionNotAllowed = true
        request[kSecUseAuthenticationContext as String] = context
        let status = SecItemDelete(request as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw FTError.storage }
    }
}
#endif
