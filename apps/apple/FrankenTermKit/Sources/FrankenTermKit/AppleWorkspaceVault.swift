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

    fileprivate func query(account: String) throws -> [String: Any] {
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
        let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: privateContainer)
        guard identity.fingerprint == fingerprint else { throw SSHAuthenticationError.identityMismatch }
        var error: Unmanaged<CFError>?
        guard let access = SecAccessControlCreateWithFlags(nil, kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly,
                                                          .userPresence, &error) else {
            _ = error?.takeRetainedValue(); throw FTError.storage
        }
        var item = try query(account: account)
        item[kSecAttrAccessControl as String] = access
        let metadata = WorkspaceCredentialMetadata(identity: identity)
        item[kSecAttrGeneric as String] = metadata.fingerprintBytes
        // Public metadata avoids prompting merely to offer a key. It is checked
        // against the independently stored fingerprint and never grants signing.
        if let comment = metadata.publicComment { item[kSecAttrComment as String] = comment }
        item[kSecValueData as String] = privateContainer
        // Add only. A duplicate must never overwrite an identity or relax its ACL.
        guard SecItemAdd(item as CFDictionary, nil) == errSecSuccess else { throw FTError.storage }
    }
    public func fingerprint(account: String) throws -> String? {
        try metadata(account: account)?.fingerprint
    }
    public func publicIdentity(account: String) throws -> SSHPublicIdentity? {
        try metadata(account: account)?.identity
    }

    /// Upgrade only the public comment during explicitly reviewed reimport.
    /// The exact account and independently stored fingerprint constrain the
    /// update. The protected value, accessibility and ACL are never update fields.
    /// A platform requiring authentication for metadata updates fails without UI.
    public func enrichPublicIdentity(account: String, identity: SSHPublicIdentity) throws -> Bool {
        guard let existing = try metadata(account: account), existing.fingerprint == identity.fingerprint else {
            throw SSHAuthenticationError.identityMismatch
        }
        if let known = existing.identity {
            guard known == identity else { throw SSHAuthenticationError.identityMismatch }
            return true
        }
        guard let comment = WorkspaceCredentialMetadata(identity: identity).publicComment else { throw FTError.storage }
        var request = try query(account: account)
        request[kSecAttrGeneric as String] = existing.fingerprintBytes
        let context = LAContext(); context.interactionNotAllowed = true
        defer { context.invalidate() }
        request[kSecUseAuthenticationContext as String] = context
        let changes = [kSecAttrComment as String: comment]
        guard SecItemUpdate(request as CFDictionary, changes as CFDictionary) == errSecSuccess else { throw FTError.storage }
        guard try publicIdentity(account: account) == identity else { throw FTError.storage }
        return true
    }

    private func metadata(account: String) throws -> WorkspaceCredentialMetadata? {
        var request = try query(account: account)
        request[kSecReturnAttributes as String] = true
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        let context = LAContext()
        context.interactionNotAllowed = true
        defer { context.invalidate() }
        request[kSecUseAuthenticationContext as String] = context
        var result: CFTypeRef?
        let status = SecItemCopyMatching(request as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        // Locked/unavailable is NOT absent and never authorizes orphan cleanup.
        guard status == errSecSuccess, let attributes = result as? [String: Any] else { throw FTError.storage }
        return try Self.metadata(attributes: attributes)
    }

    fileprivate static func metadata(attributes: [String: Any]) throws -> WorkspaceCredentialMetadata {
        guard let fingerprint = attributes[kSecAttrGeneric as String] as? Data else { throw FTError.storage }
        let comment: String?
        if let value = attributes[kSecAttrComment as String] {
            guard let text = value as? String else { throw FTError.storage }
            comment = text
        } else { comment = nil }
        return try WorkspaceCredentialMetadata(fingerprintBytes: fingerprint, publicComment: comment)
    }
    public func remove(account: String) throws {
        var request = try query(account: account)
        let context = LAContext(); context.interactionNotAllowed = true
        request[kSecUseAuthenticationContext as String] = context
        let status = SecItemDelete(request as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else { throw FTError.storage }
    }
}

/// One process-wide, serial authentication worker. SecItem's synchronous user
/// presence operation runs on this dedicated queue, never MainActor, the store
/// actor, or a cooperative Swift executor. Its only result is a bounded signature.
/// Mutable request/context ownership is protected by the lock; no private key is
/// retained in shared state. The broker still rechecks durable policy afterward.
public final class AppleSSHCredentialSigner: WorkspaceSSHSigning, @unchecked Sendable {
    private enum Operation: Sendable { case publicIdentity, signature(SSHUserAuthenticationChallenge) }
    private final class Worker: @unchecked Sendable {
        struct Request {
            let id: UUID
            var cancelled = false
            var context: LAContext?
            var timer: DispatchSourceTimer?
        }
        let queue = DispatchQueue(label: "org.frankenterm.ssh.credential-worker", qos: .userInitiated)
        let lock = NSLock()
        var current: Request?

        func admit(_ id: UUID) throws {
            lock.lock(); defer { lock.unlock() }
            guard current == nil else { throw SSHAuthenticationError.busy }
            current = Request(id: id)
        }
        func installContext(_ context: LAContext, for id: UUID) throws {
            lock.lock(); defer { lock.unlock() }
            guard current?.id == id, current?.cancelled == false else { throw CancellationError() }
            current?.context = context
            let timer = DispatchSource.makeTimerSource(queue: .global(qos: .userInitiated))
            timer.schedule(deadline: .now() + 60)
            timer.setEventHandler { [weak self] in self?.cancel(id) }
            current?.timer = timer
            timer.resume()
        }
        func check(_ id: UUID) throws {
            lock.lock(); defer { lock.unlock() }
            guard current?.id == id, current?.cancelled == false else { throw CancellationError() }
        }
        func cancel(_ id: UUID) {
            lock.lock()
            guard current?.id == id else { lock.unlock(); return }
            current?.cancelled = true
            let context = current?.context
            lock.unlock()
            context?.invalidate()
        }
        func finish(_ id: UUID, result: Result<Data, Error>) -> Result<Data, Error> {
            lock.lock()
            guard let request = current, request.id == id else { lock.unlock(); return .failure(CancellationError()) }
            current = nil
            lock.unlock()
            request.timer?.cancel()
            request.context?.invalidate()
            return request.cancelled ? .failure(CancellationError()) : result
        }
    }
    private static let worker = Worker()
    public init() {}

    public func cancel(requestID: UUID) { Self.worker.cancel(requestID) }

    public func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try await perform(requestID: requestID, authorization: authorization, operation: .publicIdentity)
    }

    public func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
                     challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        guard challenge.user == authorization.user, challenge.algorithm == authorization.algorithm,
              challenge.fingerprint == authorization.fingerprint else { throw SSHAuthenticationError.identityMismatch }
        return try await perform(requestID: requestID, authorization: authorization, operation: .signature(challenge))
    }

    private func perform(requestID: UUID, authorization: WorkspaceSSHAuthorization, operation: Operation) async throws -> Data {
        try Task.checkCancellation()
        let result: Data = try await withTaskCancellationHandler {
            try await withCheckedThrowingContinuation { continuation in
                do {
                    try Self.worker.admit(requestID)
                    // Covers cancellation that arrived before admission, when
                    // the cancellation handler could not yet find this request.
                    if Task.isCancelled { Self.worker.cancel(requestID) }
                } catch { continuation.resume(throwing: error); return }
                Self.worker.queue.async {
                    let result = Result<Data, Error> {
                        try Self.worker.check(requestID)
                        if case .publicIdentity = operation {
                            let metadataContext = LAContext(); metadataContext.interactionNotAllowed = true
                            defer { metadataContext.invalidate() }
                            var query = try AppleWorkspaceVault().query(account: authorization.account)
                            query[kSecMatchLimit as String] = kSecMatchLimitOne
                            query[kSecReturnAttributes as String] = true
                            query[kSecUseAuthenticationContext as String] = metadataContext
                            var item: CFTypeRef?
                            let status = SecItemCopyMatching(query as CFDictionary, &item)
                            try Self.worker.check(requestID)
                            guard status == errSecSuccess, let attributes = item as? [String: Any] else {
                                throw SSHAuthenticationError.denied
                            }
                            let metadata = try AppleWorkspaceVault.metadata(attributes: attributes)
                            guard metadata.fingerprint == authorization.fingerprint else { throw SSHAuthenticationError.identityMismatch }
                            if let identity = metadata.identity {
                                guard identity.algorithm == authorization.algorithm, identity.fingerprint == authorization.fingerprint else {
                                    throw SSHAuthenticationError.identityMismatch
                                }
                                return identity.blob
                            }
                            // An identity imported before public metadata existed
                            // stays usable. Obtain its public half on the same
                            // bounded user-presence worker; never export the key.
                        }
                        let context = LAContext()
                        context.localizedReason = "Use your SSH identity for \(authorization.user)@\(authorization.host):\(authorization.port)."
                        context.touchIDAuthenticationAllowableReuseDuration = 0
                        defer { context.invalidate() }
                        try Self.worker.installContext(context, for: requestID)
                        var query = try AppleWorkspaceVault().query(account: authorization.account)
                        query[kSecMatchLimit as String] = kSecMatchLimitOne
                        query[kSecReturnData as String] = true
                        query[kSecReturnAttributes as String] = true
                        query[kSecUseAuthenticationContext as String] = context
                        var item: CFTypeRef?
                        let status = SecItemCopyMatching(query as CFDictionary, &item)
                        try Self.worker.check(requestID)
                        guard status == errSecSuccess, let attributes = item as? [String: Any],
                              var container = attributes[kSecValueData as String] as? Data,
                              !container.isEmpty, container.count <= FTLimit.key else {
                            throw SSHAuthenticationError.denied
                        }
                        // Foundation and Security may retain other copies. This
                        // clears the owned buffer, not a forensic-erasure promise.
                        defer { container.resetBytes(in: 0..<container.count) }
                        let metadata = try AppleWorkspaceVault.metadata(attributes: attributes)
                        guard metadata.fingerprint == authorization.fingerprint else { throw SSHAuthenticationError.identityMismatch }
                        let value: Data
                        switch operation {
                        case .publicIdentity:
                            let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container)
                            guard identity.algorithm == authorization.algorithm, identity.fingerprint == authorization.fingerprint else {
                                throw SSHAuthenticationError.identityMismatch
                            }
                            guard metadata.identity.map({ $0 == identity }) ?? true else { throw SSHAuthenticationError.identityMismatch }
                            value = identity.blob
                        case .signature(let challenge):
                            guard metadata.identity.map({ $0.blob == challenge.publicBlob }) ?? true else {
                                throw SSHAuthenticationError.identityMismatch
                            }
                            value = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
                        }
                        try Self.worker.check(requestID)
                        return value
                    }
                    continuation.resume(with: Self.worker.finish(requestID, result: result))
                }
            }
        } onCancel: { Self.worker.cancel(requestID) }
        try Task.checkCancellation()
        return result
    }
}
#endif
