import Foundation

/// Two fixed app-owned locations for one logical credential account. The
/// current item is authoritative when present; only definite absence permits
/// looking in the legacy location. In particular, a locked or damaged current
/// item must not silently select a different private item.
enum WorkspacePrivateKeyGeneration: CaseIterable, Sendable {
    case current, legacy

    var service: String {
        switch self {
        case .current: "org.frankenterm.mobile.workspace-keys.v2"
        case .legacy: "org.frankenterm.mobile.workspace-keys.v1"
        }
    }

    static func select<Value>(read: (Self) throws -> Value?) rethrows -> (generation: Self, value: Value)? {
        for generation in [Self.current, .legacy] {
            if let value = try read(generation) { return (generation, value) }
        }
        return nil
    }
}

enum WorkspacePrivateKeyMigrationError: Error, Equatable, Sendable {
    case invalidContainer, missingSource, replacementMismatch
}

/// The native adapter binds these operations to one exact validated account
/// under its mutation lock, after checking the original public identity again.
/// All reads are noninteractive. A replacement read must check its configured
/// device-only accessibility before returning private bytes.
protocol WorkspacePrivateKeyMigrationStorage {
    func legacyExists() throws -> Bool
    /// Add only. An explicit duplicate may proceed to verification; any other
    /// error must throw, including an error after the insertion took effect.
    func publishCurrentIfAbsent(_ container: Data) throws
    func readCurrent() throws -> Data?
    func retireLegacy() throws
}

/// Publishing a replacement never removes the accepted source first. If the
/// process stops at any boundary, the legacy item or the verified current item
/// remains discoverable by `select`. No journal contains private bytes, and a
/// retry never overwrites an item merely because publication was ambiguous.
enum WorkspacePrivateKeyMigration {
    static func migrate(_ container: Data, maximumBytes: Int,
                        storage: some WorkspacePrivateKeyMigrationStorage) throws {
        guard maximumBytes > 0, !container.isEmpty, container.count <= maximumBytes else {
            throw WorkspacePrivateKeyMigrationError.invalidContainer
        }
        guard try storage.legacyExists() else {
            // A concurrent removal cannot be reversed by a worker holding an
            // older unlocked copy. A completed migration is harmless to retry.
            guard var current = try storage.readCurrent() else {
                throw WorkspacePrivateKeyMigrationError.missingSource
            }
            defer { current.resetBytes(in: 0..<current.count) }
            guard current == container else { throw WorkspacePrivateKeyMigrationError.replacementMismatch }
            return
        }
        try storage.publishCurrentIfAbsent(container)
        guard var verified = try storage.readCurrent() else {
            throw WorkspacePrivateKeyMigrationError.replacementMismatch
        }
        defer { verified.resetBytes(in: 0..<verified.count) }
        guard verified == container else { throw WorkspacePrivateKeyMigrationError.replacementMismatch }
        // Both acknowledged publication and exact, policy-checked readback
        // precede removal. A failed cleanup leaves two bounded generations.
        try storage.retireLegacy()
    }
}
