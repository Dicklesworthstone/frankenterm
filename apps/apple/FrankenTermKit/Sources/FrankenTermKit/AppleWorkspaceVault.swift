#if os(iOS)
import Foundation
import Security
import LocalAuthentication
import os

/// A Keychain call that did not succeed, with its status: what a person or a
/// log needs to tell a missing passcode from a locked device or a refused read.
public struct WorkspaceVaultError: LocalizedError, Equatable, Sendable {
    public let operation: String
    public let status: OSStatus
    public var errorDescription: String? {
        switch status {
        case errSecUserCanceled: return "Face ID or the passcode was cancelled, so the key wasn't used."
        case errSecAuthFailed: return "Face ID or the passcode didn't succeed, so the key wasn't used."
        case errSecInteractionNotAllowed: return "The key can't be used while this device is locked."
        default:
            let detail = (SecCopyErrorMessageString(status, nil) as String?) ?? "unknown status"
            return "The Keychain refused to \(operation) a private key (\(status): \(detail))."
        }
    }
    static func check(_ status: OSStatus, _ operation: String) throws {
        guard status != errSecSuccess else { return }
        let error = WorkspaceVaultError(operation: operation, status: status)
        Logger(subsystem: "org.frankenterm.mobile", category: "vault")
            .error("keychain \(operation, privacy: .public) failed: \(status, privacy: .public)")
        throw error
    }
}

/// Only transaction-owned accounts within this app's private services are touched.
/// No wildcard queries, access groups, synchronizable items, or keychain exports.
/// SecItem calls run on WorkspaceStore's actor, never a SwiftUI/MainActor callback.
///
/// Each account has a private key and its public details
/// (fingerprint and comment). Both stay on this device only (never synced or
/// carried by a backup to another device) and are readable only while it is
/// unlocked and has a passcode; removing the passcode deletes them. Signing
/// asks for no Face ID: the operator's call, so a workspace's machines all
/// connect without a prompt each. A key stored earlier behind user presence
/// is asked for once more and then copied to the current private-item generation
/// before its legacy item is retired (storeWithoutPresence).
public struct AppleWorkspaceVault: WorkspaceCredentialVault {
    private static let detailsService = "org.frankenterm.mobile.workspace-key-details.v1"
    // Store mutations and the signing worker share this lock. It is never held
    // across an interactive read: a prompt may outlive removal of its account.
    private static let mutationLock = NSLock()
    public init() {}

    fileprivate func query(account: String, service: String = WorkspacePrivateKeyGeneration.current.service,
                           context: LAContext? = nil) throws -> [String: Any] {
        let parts = account.split(separator: ":", omittingEmptySubsequences: false)
        guard parts.count == 3, parts[0] == "ft-import", let transaction = UUID(uuidString: String(parts[1])),
              parts[1] == Substring(transaction.uuidString), let index = Int(parts[2]), (0..<256).contains(index),
              parts[2] == Substring(String(index)) else { throw FTError.malformed("vault.account") }
        var result: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
                                    kSecAttrService as String: service,
                                    kSecAttrAccount as String: account,
                                    kSecAttrSynchronizable as String: false]
        if let context { result[kSecUseAuthenticationContext as String] = context }
        return result
    }
    private static func withMutation<T>(_ body: (LAContext) throws -> T) rethrows -> T {
        mutationLock.lock(); defer { mutationLock.unlock() }
        let context = LAContext(); context.interactionNotAllowed = true
        defer { context.invalidate() }
        return try body(context)
    }
    private func contains(account: String, service: String, context: LAContext) throws -> Bool {
        var request = try query(account: account, service: service, context: context)
        request[kSecReturnAttributes as String] = true
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(request as CFDictionary, &result)
        if status == errSecItemNotFound { return false }
        try WorkspaceVaultError.check(status, "locate")
        guard result is [String: Any] else { throw FTError.storage }
        return true
    }
    public func create(account: String, privateContainer: Data, fingerprint: String) throws {
        guard !privateContainer.isEmpty, privateContainer.count <= FTLimit.key,
              fingerprint.utf8.count == 50, fingerprint.hasPrefix("SHA256:") else { throw FTError.limit("vault.item") }
        let identity = try SSHUserAuthenticationSigner.publicIdentity(privateContainer: privateContainer)
        guard identity.fingerprint == fingerprint else { throw SSHAuthenticationError.identityMismatch }
        try Self.withMutation { context in
            // One logical account cannot acquire a new identity by choosing a
            // different generation or by replacing an orphaned details item.
            for service in WorkspacePrivateKeyGeneration.allCases.map(\.service) + [Self.detailsService] {
                if try contains(account: account, service: service, context: context) {
                    try WorkspaceVaultError.check(errSecDuplicateItem, "store")
                }
            }
            var item = try query(account: account, context: context)
            item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly
            item[kSecValueData as String] = privateContainer
            // Add only. A duplicate must never overwrite an identity.
            try WorkspaceVaultError.check(SecItemAdd(item as CFDictionary, nil), "store")
            // The key first: a missing details item reads as absent, which only
            // an uncommitted transaction's cleanup acts on.
            let metadata = WorkspaceCredentialMetadata(identity: identity)
            var details = try query(account: account, service: Self.detailsService, context: context)
            details[kSecAttrAccessible as String] = kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly
            details[kSecAttrGeneric as String] = metadata.fingerprintBytes
            if let comment = metadata.publicComment { details[kSecAttrComment as String] = comment }
            details[kSecValueData as String] = Data()
            do { try WorkspaceVaultError.check(SecItemAdd(details as CFDictionary, nil), "store the details of") }
            catch {
                _ = SecItemDelete(try query(account: account, context: context) as CFDictionary)
                throw error
            }
        }
    }
    public func fingerprint(account: String) throws -> String? {
        try metadata(account: account)?.fingerprint
    }
    public func publicIdentity(account: String) throws -> SSHPublicIdentity? {
        try metadata(account: account)?.identity
    }

    /// Upgrade only the public comment during explicitly reviewed reimport.
    /// The exact account and independently stored fingerprint constrain the
    /// update, which touches only the details item: the private key item's
    /// value, accessibility and ACL are never update fields.
    public func enrichPublicIdentity(account: String, identity: SSHPublicIdentity) throws -> Bool {
        try Self.withMutation { context in
            guard let existing = try metadata(account: account, context: context), existing.fingerprint == identity.fingerprint else {
                throw SSHAuthenticationError.identityMismatch
            }
            if let known = existing.identity {
                guard known == identity else { throw SSHAuthenticationError.identityMismatch }
                return true
            }
            guard let comment = WorkspaceCredentialMetadata(identity: identity).publicComment else { throw FTError.storage }
            var request = try query(account: account, service: Self.detailsService, context: context)
            request[kSecAttrGeneric as String] = existing.fingerprintBytes
            let changes = [kSecAttrComment as String: comment]
            try WorkspaceVaultError.check(SecItemUpdate(request as CFDictionary, changes as CFDictionary), "update")
            guard try metadata(account: account, context: context)?.identity == identity else { throw FTError.storage }
            return true
        }
    }

    /// The public details, read without touching the private key item.
    private func metadata(account: String) throws -> WorkspaceCredentialMetadata? {
        let context = LAContext(); context.interactionNotAllowed = true
        defer { context.invalidate() }
        return try metadata(account: account, context: context)
    }
    private func metadata(account: String, context: LAContext) throws -> WorkspaceCredentialMetadata? {
        var request = try query(account: account, service: Self.detailsService, context: context)
        request[kSecReturnAttributes as String] = true
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: CFTypeRef?
        let status = SecItemCopyMatching(request as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        // Locked/unavailable is NOT absent and never authorizes orphan cleanup.
        try WorkspaceVaultError.check(status, "read the details of")
        guard let attributes = result as? [String: Any] else { throw FTError.storage }
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
    /// Current-generation data and accessibility are returned by one exact
    /// query. A different accessibility class or unreadable item cannot cause a
    /// fallback to legacy storage. Legacy reads may use the reviewed prompt.
    fileprivate func readPrivateContainer(account: String, generation: WorkspacePrivateKeyGeneration,
                                           context: LAContext) throws -> (status: OSStatus, container: Data?) {
        var request = try query(account: account, service: generation.service, context: context)
        request[kSecMatchLimit as String] = kSecMatchLimitOne
        request[kSecReturnData as String] = true
        if generation == .current { request[kSecReturnAttributes as String] = true }
        var result: CFTypeRef?
        let status = SecItemCopyMatching(request as CFDictionary, &result)
        guard status == errSecSuccess else { return (status, nil) }
        let container: Data?
        if generation == .current {
            guard let attributes = result as? [String: Any],
                  attributes[kSecAttrAccessible as String] as? String == kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly as String else {
                throw FTError.storage
            }
            container = attributes[kSecValueData as String] as? Data
        } else { container = result as? Data }
        guard let container, !container.isEmpty, container.count <= FTLimit.key else { throw FTError.storage }
        return (status, container)
    }

    /// Publish the independently verified copy before touching the accepted
    /// legacy key. Errors preserve the old item; an interruption after copy
    /// publication is resolved by deterministic current-generation reads.
    fileprivate func storeWithoutPresence(account: String, privateContainer: Data,
                                           identity: SSHPublicIdentity) throws {
        guard try SSHUserAuthenticationSigner.publicIdentity(privateContainer: privateContainer) == identity else {
            throw SSHAuthenticationError.identityMismatch
        }
        try Self.withMutation { context in
            // The signer read this identity before an interactive prompt. Check
            // it again under the mutation lock so removal or replacement during
            // that prompt cannot resurrect an account from an unlocked buffer.
            guard try metadata(account: account, context: context)?.identity == identity else {
                throw SSHAuthenticationError.identityMismatch
            }
            try WorkspacePrivateKeyMigration.migrate(privateContainer, maximumBytes: FTLimit.key,
                storage: MigrationStorage(vault: self, account: account, context: context))
        }
    }

    private struct MigrationStorage: WorkspacePrivateKeyMigrationStorage {
        let vault: AppleWorkspaceVault
        let account: String
        let context: LAContext

        func legacyExists() throws -> Bool {
            try vault.contains(account: account, service: WorkspacePrivateKeyGeneration.legacy.service, context: context)
        }
        func publishCurrentIfAbsent(_ container: Data) throws {
            var item = try vault.query(account: account, context: context)
            item[kSecAttrAccessible as String] = kSecAttrAccessibleWhenPasscodeSetThisDeviceOnly
            item[kSecValueData as String] = container
            let status = SecItemAdd(item as CFDictionary, nil)
            if status != errSecDuplicateItem { try WorkspaceVaultError.check(status, "store") }
        }
        func readCurrent() throws -> Data? {
            let (status, container) = try vault.readPrivateContainer(account: account, generation: .current, context: context)
            if status == errSecItemNotFound { return nil }
            try WorkspaceVaultError.check(status, "verify")
            return container
        }
        func retireLegacy() throws {
            let request = try vault.query(account: account, service: WorkspacePrivateKeyGeneration.legacy.service, context: context)
            let status = SecItemDelete(request as CFDictionary)
            if status != errSecItemNotFound { try WorkspaceVaultError.check(status, "retire the legacy copy of") }
        }
    }

    /// Deleting never prompts. Both private generations go before their details;
    /// migration cannot race this removal and republish an unreferenced key.
    public func remove(account: String) throws {
        try Self.withMutation { context in
            for service in WorkspacePrivateKeyGeneration.allCases.map(\.service) + [Self.detailsService] {
                let status = SecItemDelete(try query(account: account, service: service, context: context) as CFDictionary)
                if status != errSecItemNotFound { try WorkspaceVaultError.check(status, "remove") }
            }
        }
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
        /// The context one Face ID approval authenticated, reused until it
        /// expires or approval ends (see AppleSSHCredentialSigner.endApproval).
        var approval: (context: LAContext, expires: ContinuousClock.Instant)?

        func approvedContext() -> LAContext? {
            lock.lock()
            guard let approval, ContinuousClock.now < approval.expires else {
                let lapsed = approval?.context; approval = nil
                lock.unlock(); lapsed?.invalidate(); return nil
            }
            lock.unlock()
            return approval.context
        }
        /// Keeps this request's freshly approved context for the requests that
        /// follow; finishing the request no longer invalidates it.
        func adopt(_ context: LAContext, for id: UUID, lasting duration: Duration) {
            lock.lock()
            guard current?.id == id, current?.context === context else { lock.unlock(); return }
            current?.context = nil
            let previous = approval?.context
            approval = (context, ContinuousClock.now + duration)
            lock.unlock()
            previous?.invalidate()
        }
        func endApproval() {
            lock.lock(); let context = approval?.context; approval = nil; lock.unlock()
            context?.invalidate()
        }

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

    /// How long one Face ID approval keeps signing: every machine of a
    /// workspace connecting together, and reconnects soon after, ask once.
    public static let approvalDuration: Duration = .seconds(300)
    /// Ends the standing approval at once; the app calls this when it leaves
    /// the foreground, so a later connection asks again.
    public static func endApproval() { worker.endApproval() }

    public func cancel(requestID: UUID) { Self.worker.cancel(requestID) }

    public func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        try await perform(requestID: requestID, authorization: authorization, operation: .publicIdentity)
    }

    public func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization,
                     challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        guard challenge.user.utf8.elementsEqual(authorization.user.utf8), challenge.publicKeyAlgorithm == authorization.algorithm,
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
                        // The public half is in the account's details item,
                        // which never prompts. Only a signature opens the
                        // user-presence key item (see AppleWorkspaceVault).
                        guard let identity = try AppleWorkspaceVault().publicIdentity(account: authorization.account) else {
                            throw SSHAuthenticationError.unavailable
                        }
                        guard identity.algorithm == authorization.algorithm, identity.fingerprint == authorization.fingerprint else {
                            throw SSHAuthenticationError.identityMismatch
                        }
                        guard case .signature(let challenge) = operation else { return identity.blob }
                        guard identity.blob == challenge.subjectPublicBlob,
                              challenge.publicBlob == (authorization.userCertificate?.blob ?? identity.blob) else {
                            throw SSHAuthenticationError.identityMismatch
                        }
                        var selectedGeneration: WorkspacePrivateKeyGeneration?
                        func read(_ context: LAContext) throws -> (status: OSStatus, container: Data?) {
                            func copy(_ generation: WorkspacePrivateKeyGeneration) throws -> (status: OSStatus, container: Data?) {
                                let result = try AppleWorkspaceVault().readPrivateContainer(account: authorization.account,
                                    generation: generation, context: context)
                                try Self.worker.check(requestID)
                                return result
                            }
                            // Once a prompt starts, it remains bound to the
                            // same generation. Removal during the prompt cannot
                            // redirect its approval to a replacement item.
                            if let selectedGeneration { return try copy(selectedGeneration) }
                            let selected = try WorkspacePrivateKeyGeneration.select { generation in
                                let result = try copy(generation)
                                return result.status == errSecItemNotFound ? nil : result
                            }
                            selectedGeneration = selected?.generation
                            return selected?.value ?? (errSecItemNotFound, nil)
                        }
                        // Keys are readable while the device is unlocked, with
                        // no prompt (see AppleWorkspaceVault).
                        let quiet = LAContext(); quiet.interactionNotAllowed = true
                        defer { quiet.invalidate() }
                        var (status, item) = try read(quiet)
                        // A key stored earlier behind user presence: a standing
                        // approval, or one Face ID, then it is stored the
                        // current way and never asks again.
                        let protected = selectedGeneration == .legacy
                            && (status == errSecInteractionNotAllowed || status == errSecAuthFailed)
                        if protected, let approved = Self.worker.approvedContext() {
                            (status, item) = try read(approved)
                            if status != errSecSuccess { Self.worker.endApproval() }
                        }
                        if protected, status != errSecSuccess {
                            let context = LAContext()
                            context.localizedReason = "Unlock your SSH keys once to connect to your machines."
                            try Self.worker.installContext(context, for: requestID)
                            (status, item) = try read(context)
                            if status == errSecSuccess {
                                Self.worker.adopt(context, for: requestID, lasting: Self.approvalDuration)
                            }
                        }
                        try WorkspaceVaultError.check(status, "use")
                        guard var container = item, !container.isEmpty, container.count <= FTLimit.key else {
                            throw FTError.storage
                        }
                        // Foundation and Security may retain other copies. This
                        // clears the owned buffer, not a forensic-erasure promise.
                        defer { container.resetBytes(in: 0..<container.count) }
                        guard try SSHUserAuthenticationSigner.publicIdentity(privateContainer: container) == identity else {
                            throw SSHAuthenticationError.identityMismatch
                        }
                        let value = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: container)
                        try Self.worker.check(requestID)
                        if selectedGeneration == .legacy {
                            // Best-effort migration never deletes the accepted
                            // copy before a verified replacement exists. A
                            // failed migration does not invalidate this signature.
                            try? AppleWorkspaceVault().storeWithoutPresence(account: authorization.account,
                                privateContainer: container, identity: identity)
                        }
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
