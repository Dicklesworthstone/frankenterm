import Foundation

/// Bounded public attributes on the exact protected credential item. Preserve
/// the published Keychain format: the canonical fingerprint is generic data,
/// while the optional versioned comment contains only the canonical SSH blob.
/// Missing comments identify legacy items; malformed comments never become a
/// reason to fall back to private-key access or to overwrite existing metadata.
struct WorkspaceCredentialMetadata: Equatable, Sendable {
    static let commentPrefix = "ft-ssh-public-v1:"
    static let maximumCommentBytes = commentPrefix.utf8.count + 4 * ((512 + 2) / 3)
    let fingerprint: String
    let identity: SSHPublicIdentity?

    init(identity: SSHPublicIdentity) {
        self.fingerprint = identity.fingerprint; self.identity = identity
    }

    init(fingerprintBytes: Data, publicComment: String?) throws {
        do {
            guard fingerprintBytes.count == 50,
                  let fingerprint = String(data: fingerprintBytes, encoding: .utf8) else {
                throw FTError.storage
            }
            self.fingerprint = try WorkspaceDocument.fingerprint(fingerprint)
            if let publicComment {
                guard publicComment.utf8.count <= Self.maximumCommentBytes,
                      publicComment.hasPrefix(Self.commentPrefix) else { throw FTError.storage }
                let encoded = String(publicComment.dropFirst(Self.commentPrefix.count))
                guard let blob = Data(base64Encoded: encoded), blob.base64EncodedString() == encoded else {
                    throw FTError.storage
                }
                let identity = try SSHPublicIdentity(blob: blob)
                guard identity.fingerprint == fingerprint else { throw FTError.storage }
                self.identity = identity
            } else { identity = nil }
        } catch { throw FTError.storage }
    }

    var fingerprintBytes: Data { Data(fingerprint.utf8) }
    var publicComment: String? { identity.map { Self.commentPrefix + $0.blob.base64EncodedString() } }
}
