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

/// One process-wide, serial authentication worker. SecItem's synchronous user
/// presence operation runs on this dedicated queue, never MainActor, the store
/// actor, or a cooperative Swift executor. Its only result is a bounded signature.
/// Mutable request/context ownership is protected by the lock; no private key is
/// retained in shared state. The broker still rechecks durable policy afterward.
public final class AppleSSHCredentialSigner: WorkspaceSSHSigning, @unchecked Sendable {
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

    public func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
                     challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        try Task.checkCancellation()
        guard challenge.user == authorization.user, challenge.algorithm == authorization.algorithm,
              challenge.fingerprint == authorization.fingerprint else { throw SSHAuthenticationError.identityMismatch }
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
                              let metadata = attributes[kSecAttrGeneric as String] as? Data,
                              metadata == Data(authorization.fingerprint.utf8),
                              var container = attributes[kSecValueData as String] as? Data,
                              !container.isEmpty, container.count <= FTLimit.key else {
                            throw SSHAuthenticationError.denied
                        }
                        // Foundation and Security may retain other copies. This
                        // clears the owned buffer, not a forensic-erasure promise.
                        defer { container.resetBytes(in: 0..<container.count) }
                        let signature = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
                        try Self.worker.check(requestID)
                        return signature
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
