import Foundation

/// Content-free source-file errors are distinct from an FTAG transfer-code error.
public enum OpenSSHSourceKeyError: Error, Equatable, Sendable, LocalizedError {
    case passphraseRequired, incorrectPassphraseOrDamagedKey, identityMismatch, attemptsExhausted
    public var errorDescription: String? {
        switch self {
        case .passphraseRequired: return "Enter the passphrase for this selected OpenSSH key file."
        case .incorrectPassphraseOrDamagedKey: return "The passphrase is incorrect or the selected key file is damaged."
        case .identityMismatch: return "The selected key does not match the approved workspace identity."
        case .attemptsExhausted: return "This key file reached the unlock-attempt limit. Choose the file again to start a new attempt."
        }
    }
}

/// A bounded, explicitly selected source file. Preflight exposes only its public
/// identity and protection profile; private material remains opaque until unlock.
/// This type never reads a path, runs a process, or writes a decrypted file.
public struct OpenSSHSourceKey: Sendable, CustomStringConvertible, CustomDebugStringConvertible {
    public static let maximumBcryptRounds: UInt32 = 64
    public static let maximumPassphraseBytes = 1024
    public let algorithm: String
    public let publicBlob: Data
    public let bcryptRounds: UInt32?
    public var isEncrypted: Bool { cipher != .none }
    public var description: String { "OpenSSHSourceKey([redacted])" }
    public var debugDescription: String { description }

    private enum Cipher: String, Sendable {
        case none, aes128CTR = "aes128-ctr", aes256CTR = "aes256-ctr"
        var keyBytes: Int { self == .aes128CTR ? 16 : 32 }
        var blockBytes: Int { self == .none ? 8 : 16 }
    }
    private let cipher: Cipher
    private let container: Data
    private let privatePart: Data
    private let salt: Data

    public init(pem: Data) throws {
        guard !pem.isEmpty, pem.count <= FTLimit.key, let text = String(data: pem, encoding: .utf8) else {
            throw FTError.limit("source_key.pem")
        }
        let lines = text.trimmingCharacters(in: .whitespacesAndNewlines)
            .components(separatedBy: .newlines).filter { !$0.isEmpty }
        guard lines.count >= 3, lines.first == "-----BEGIN OPENSSH PRIVATE KEY-----",
              lines.last == "-----END OPENSSH PRIVATE KEY-----",
              let decoded = Data(base64Encoded: lines.dropFirst().dropLast().joined()), decoded.count <= FTLimit.key else {
            throw FTError.malformed("source_key.pem")
        }
        container = decoded
        var reader = ByteCursor(decoded)
        guard try reader.take(15) == Data("openssh-key-v1\0".utf8) else { throw FTError.malformed("source_key.magic") }
        let cipherBytes = try reader.sshString(cap: 64)
        guard let cipherName = String(data: cipherBytes, encoding: .ascii), let cipher = Cipher(rawValue: cipherName) else {
            throw FTError.unsupported("source_key.cipher")
        }
        self.cipher = cipher
        let kdf = try reader.sshString(cap: 64)
        let options = try reader.sshString(cap: 72)
        if cipher == .none {
            guard kdf == Data("none".utf8), options.isEmpty else { throw FTError.malformed("source_key.kdf_combination") }
            salt = Data(); bcryptRounds = nil
        } else {
            guard kdf == Data("bcrypt".utf8) else { throw FTError.unsupported("source_key.kdf") }
            var parameters = ByteCursor(options)
            let salt = try parameters.sshString(cap: 64)
            let rounds = try parameters.uint(4)
            guard parameters.remaining == 0, (16...64).contains(salt.count),
                  (1...UInt64(Self.maximumBcryptRounds)).contains(rounds) else {
                throw FTError.limit("source_key.kdf_parameters")
            }
            self.salt = salt; bcryptRounds = UInt32(rounds)
        }
        guard try reader.uint(4) == 1 else { throw FTError.unsupported("source_key.key_count") }
        publicBlob = try reader.sshString(cap: 2048)
        privatePart = try reader.sshString()
        guard reader.remaining == 0, !privatePart.isEmpty, privatePart.count.isMultiple(of: cipher.blockBytes) else {
            throw FTError.malformed("source_key.extent")
        }
        // Reject unsupported families and malformed public profiles before a
        // passphrase prompt or KDF. Full private binding is checked after unlock.
        var publicReader = ByteCursor(publicBlob)
        let name = try publicReader.sshString(cap: 64)
        guard let algorithm = String(data: name, encoding: .ascii) else { throw FTError.malformed("source_key.algorithm") }
        if algorithm == "ssh-ed25519" {
            guard try publicReader.sshString(cap: 32).count == 32 else { throw FTError.malformed("source_key.public") }
        } else if let curve = SSHECDSACurve.allCases.first(where: { $0.algorithm == algorithm }) {
            guard try publicReader.sshString(cap: 16) == Data(curve.rawValue.utf8) else { throw FTError.malformed("source_key.curve") }
            let point = try publicReader.sshString(cap: curve.publicByteCount)
            guard point.count == curve.publicByteCount, point.first == 4 else { throw FTError.malformed("source_key.public") }
        } else { throw FTError.unsupported("source_key.family") }
        guard publicReader.remaining == 0 else { throw FTError.malformed("source_key.public_extent") }
        self.algorithm = algorithm
    }

    public func fingerprint<C: AirGapCryptography>(using crypto: C) -> String {
        "SHA256:" + crypto.sha256(publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: "")
    }

    /// Run off MainActor on one bounded worker. The admitted native KDF is
    /// synchronous and cannot be preempted; cancellation is checked around it,
    /// and callers must retire stale attempts before accepting the result.
    public func unlock<C: AirGapCryptography>(passphrase: Data? = nil, credentialID: String,
                                             using crypto: C) throws -> PrivateKeyRecord {
        _ = try WorkspaceDocument.identifier(credentialID)
        try Task.checkCancellation()
        if cipher == .none {
            guard passphrase == nil || passphrase?.isEmpty == true else { throw FTError.malformed("source_key.unexpected_passphrase") }
            let blob = try PrivateKeyRecord.validateContainer(container, using: crypto)
            try Task.checkCancellation()
            return .init(credentialID: credentialID, container: container, publicBlob: blob)
        }
        guard let passphrase else { throw OpenSSHSourceKeyError.passphraseRequired }
        guard (1...Self.maximumPassphraseBytes).contains(passphrase.count) else { throw FTError.limit("source_key.passphrase") }
        guard let rounds = bcryptRounds else { throw FTError.malformed("source_key.kdf") }
        var material = try crypto.bcryptPBKDF(passphrase: passphrase, salt: salt, rounds: rounds,
                                             outputByteCount: cipher.keyBytes + 16)
        defer { material.resetBytes(in: 0..<material.count) }
        guard material.count == cipher.keyBytes + 16 else { throw FTError.malformed("source_key.kdf_output") }
        try Task.checkCancellation()
        var clear = try crypto.decryptAESCTR(privatePart, key: Data(material.prefix(cipher.keyBytes)), iv: Data(material.suffix(16)))
        defer { clear.resetBytes(in: 0..<clear.count) }
        try Task.checkCancellation()
        do {
            let key = try PrivateKeyRecord.fromDecryptedOpenSSH(publicBlob: publicBlob, privatePart: clear,
                                                               credentialID: credentialID, using: crypto)
            try Task.checkCancellation()
            return key
        } catch is CancellationError { throw CancellationError() }
        catch { throw OpenSSHSourceKeyError.incorrectPassphraseOrDamagedKey }
    }
}

/// One export owner's sequential source-key admission state. The native actor
/// keeps this value isolated; only content-free requests cross to the UI.
public struct OpenSSHSourceKeyUnlocker<C: AirGapCryptography>: Sendable {
    public struct Request: Sendable, Identifiable, Equatable {
        public let id: UUID
        public let credentialID: String
        public let algorithm: String
        public let fingerprint: String
        public let bcryptRounds: UInt32
        public let attemptsRemaining: Int
    }
    public enum Selection: Sendable, CustomDebugStringConvertible {
        case ready(PrivateKeyRecord), passphraseRequired(Request)
        public var debugDescription: String { "OpenSSHSourceKeyUnlocker.Selection([redacted])" }
    }
    private struct Pending: Sendable {
        let id: UUID
        let source: OpenSSHSourceKey
        let reference: WorkspaceDocument.Credential
        var attempts = 0
    }
    private let crypto: C
    private var pending: Pending?
    public static var maximumAttempts: Int { 5 }
    public init(using crypto: C) { self.crypto = crypto }

    public var request: Request? {
        guard let pending, let rounds = pending.source.bcryptRounds else { return nil }
        return .init(id: pending.id, credentialID: pending.reference.id, algorithm: pending.reference.algorithm,
                     fingerprint: pending.reference.fingerprint, bcryptRounds: rounds,
                     attemptsRemaining: Self.maximumAttempts - pending.attempts)
    }

    /// The untrusted public header must match the explicit approved reference
    /// before any password prompt; the private-derived identity is still checked
    /// after decryption. Selecting another file retires the previous request.
    public mutating func select(pem: Data, reference: WorkspaceDocument.Credential) throws -> Selection {
        pending = nil
        try Task.checkCancellation()
        let source = try OpenSSHSourceKey(pem: pem)
        guard source.algorithm == reference.algorithm, source.fingerprint(using: crypto) == reference.fingerprint else {
            throw OpenSSHSourceKeyError.identityMismatch
        }
        if !source.isEncrypted {
            return .ready(try source.unlock(credentialID: reference.id, using: crypto))
        }
        try Task.checkCancellation()
        pending = .init(id: UUID(), source: source, reference: reference)
        guard let request else { throw FTError.stale }
        return .passphraseRequired(request)
    }

    /// A prompt is bound to one exact selected file and reference. There is no
    /// global passphrase cache, unbounded retry loop, or key enumeration.
    public mutating func unlock(requestID: UUID, passphrase: Data) throws -> PrivateKeyRecord {
        guard var selected = pending, selected.id == requestID else { throw FTError.stale }
        guard (1...OpenSSHSourceKey.maximumPassphraseBytes).contains(passphrase.count) else {
            throw FTError.limit("source_key.passphrase")
        }
        guard selected.attempts < Self.maximumAttempts else {
            pending = nil; throw OpenSSHSourceKeyError.attemptsExhausted
        }
        selected.attempts += 1; pending = selected
        do {
            let key = try selected.source.unlock(passphrase: passphrase, credentialID: selected.reference.id, using: crypto)
            try Task.checkCancellation()
            guard "SHA256:" + crypto.sha256(key.publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: "")
                    == selected.reference.fingerprint else { throw OpenSSHSourceKeyError.identityMismatch }
            pending = nil
            return key
        } catch is CancellationError {
            pending = nil
            throw CancellationError()
        } catch {
            if selected.attempts >= Self.maximumAttempts {
                pending = nil
                throw OpenSSHSourceKeyError.attemptsExhausted
            }
            throw error
        }
    }

    /// A dismissal from a retired sheet cannot cancel a newer selected file.
    public mutating func cancel(requestID: UUID? = nil) throws {
        if let requestID, pending?.id != requestID { throw FTError.stale }
        pending = nil
    }
}
