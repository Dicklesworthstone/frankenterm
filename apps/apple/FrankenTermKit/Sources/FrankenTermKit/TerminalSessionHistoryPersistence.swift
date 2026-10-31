import Foundation
#if canImport(Security)
import Security
import LocalAuthentication
#endif
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

/// The catalog's identity lives outside its directory, so losing the directory
/// after a process restart cannot silently create an empty effect history.
/// Apple devices keep this small continuity anchor in the device-only Keychain;
/// the portable backend keeps an exclusive, synchronized sibling file.
struct FileTerminalSessionHistoryPersistence: WorkspacePersistence {
    private let files: any WorkspacePersistence
    private let anchor: TerminalSessionHistoryAnchor
    private static let maximumBytes = 512 * 1024

    init(directory: URL, identifier: String) throws {
        try self.init(files: FileWorkspacePersistence(directory: directory),
                      directory: directory, identifier: identifier)
    }

    /// The production path always uses FileWorkspacePersistence. Injection
    /// exercises the atomic write cut points while retaining the real anchor.
    init(files: any WorkspacePersistence, directory: URL, identifier: String) throws {
        guard directory.isFileURL else { throw TerminalSessionJournalError.invalidScope }
        do { _ = try WorkspaceDocument.identifier(identifier) }
        catch { throw TerminalSessionJournalError.invalidScope }
        self.files = files
        anchor = TerminalSessionHistoryAnchor(directory: directory, identifier: identifier)
    }

    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        try files.withExclusiveAccess(body)
    }

    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? {
        guard slot == .catalog else { return try files.read(slot, limit: limit) }
        let continuity = try anchor.read()
        guard let data = try files.read(.catalog, limit: limit) else {
            guard continuity == nil else { throw TerminalSessionJournalError.historyMissing }
            return nil
        }
        let header = try Self.header(data)
        if let continuity {
            guard continuity == header.id else { throw TerminalSessionJournalError.corruptStore }
        } else {
            // Only the pre-anchor format can be migrated. An existing current
            // catalog without its anchor is lost continuity, not first launch.
            guard header.version == 1 else { throw TerminalSessionJournalError.historyMissing }
            // Re-publication must complete before its identity is anchored. No
            // shell permission escapes from this read or from a failed sync.
            try files.write(data, to: .catalog)
            try anchor.create(header.id)
        }
        return data
    }

    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws {
        guard slot == .catalog else { throw TerminalSessionJournalError.wrongNamespace }
        let header = try Self.header(data)
        guard header.version == 3 else { throw TerminalSessionJournalError.corruptStore }
        let continuity = try anchor.read()
        let previous = try files.read(.catalog, limit: Self.maximumBytes)
        if let continuity {
            guard continuity == header.id else { throw TerminalSessionJournalError.corruptStore }
            guard let previous else { throw TerminalSessionJournalError.historyMissing }
            guard try Self.header(previous).id == continuity else {
                throw TerminalSessionJournalError.corruptStore
            }
        } else {
            if let previous {
                let old = try Self.header(previous)
                guard old.version == 1, old.id == header.id else {
                    throw TerminalSessionJournalError.historyMissing
                }
            } else {
                // A durable empty legacy catalog precedes the first anchor.
                // No epoch has escaped yet. A failure before/after this write
                // leaves either nothing or an empty migratable history, so a
                // no-effect bootstrap failure can be retried after recovery.
                let empty = try StrictJSON.encode(.object([
                    "format": .string("frankenterm-terminal-session-history"),
                    "version": .number("1"), "id": .string(header.id.uuidString),
                    "records": .array([])
                ]))
                try files.write(empty, to: .catalog)
            }
            // Continuity is durable before the first nonempty creation marker
            // can issue permission. Every anchored catalog now has a previously
            // synchronized baseline, including if this final write fails.
            try anchor.create(header.id)
        }
        try files.write(data, to: .catalog)
    }

    func removeJournal() throws { throw TerminalSessionJournalError.wrongNamespace }

    private static func header(_ data: Data) throws -> (id: UUID, version: UInt64) {
        guard data.count <= maximumBytes else { throw TerminalSessionJournalError.corruptStore }
        do {
            let root = try StrictJSON.decode(data).object()
            guard root["format"] == .string("frankenterm-terminal-session-history") else {
                throw TerminalSessionJournalError.wrongNamespace
            }
            let text = try root.text("id"), version = try root.required("version").uint()
            guard Set(root.keys) == ["format", "version", "id", "records"],
                  let id = UUID(uuidString: text), id.uuidString == text,
                  (1...3).contains(version) else {
                throw TerminalSessionJournalError.corruptStore
            }
            return (id, version)
        } catch let error as TerminalSessionJournalError { throw error }
        catch { throw TerminalSessionJournalError.corruptStore }
    }
}

private struct TerminalSessionHistoryAnchor: Sendable {
    #if canImport(Security)
    private static let service = "org.frankenterm.mobile.session-history-anchor.v1"
    private let identifier: String
    init(directory: URL, identifier: String) { self.identifier = identifier }

    private var query: [String: Any] {
        var value: [String: Any] = [kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.service,
            kSecAttrAccount as String: identifier,
            kSecAttrSynchronizable as String: false]
        #if os(macOS)
        value[kSecUseDataProtectionKeychain as String] = true
        #endif
        return value
    }

    func read() throws -> UUID? {
        var attributes = query
        attributes[kSecReturnData as String] = true
        attributes[kSecReturnAttributes as String] = true
        attributes[kSecMatchLimit as String] = kSecMatchLimitOne
        let context = LAContext(); context.interactionNotAllowed = true
        defer { context.invalidate() }
        attributes[kSecUseAuthenticationContext as String] = context
        var result: CFTypeRef?
        let status = SecItemCopyMatching(attributes as CFDictionary, &result)
        if status == errSecItemNotFound { return nil }
        guard status == errSecSuccess, let item = result as? [String: Any],
              item[kSecAttrAccessible as String] as? String == kSecAttrAccessibleWhenUnlockedThisDeviceOnly as String,
              let data = item[kSecValueData as String] as? Data else {
            throw TerminalSessionJournalError.recoveryRequired
        }
        return try Self.decode(data)
    }

    func create(_ id: UUID) throws {
        var attributes = query
        attributes[kSecAttrAccessible as String] = kSecAttrAccessibleWhenUnlockedThisDeviceOnly
        attributes[kSecValueData as String] = Data(id.uuidString.utf8)
        let status = SecItemAdd(attributes as CFDictionary, nil)
        if status == errSecDuplicateItem {
            guard try read() == id else { throw TerminalSessionJournalError.corruptStore }
        } else if status != errSecSuccess {
            throw TerminalSessionJournalError.recoveryRequired
        }
    }
    #else
    private let path: URL
    init(directory: URL, identifier: String) {
        let root = directory.standardizedFileURL
        path = root.deletingLastPathComponent().appendingPathComponent(root.lastPathComponent + ".anchor")
    }

    func read() throws -> UUID? {
        let fd = path.path.withCString { open($0, O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK) }
        if fd < 0 {
            if errno == ENOENT { return nil }
            throw TerminalSessionJournalError.recoveryRequired
        }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        defer { try? handle.close() }
        var status = stat()
        guard fstat(fd, &status) == 0, status.st_mode & mode_t(S_IFMT) == mode_t(S_IFREG),
              status.st_size == 36 else { throw TerminalSessionJournalError.corruptStore }
        let data = try handle.read(upToCount: 37) ?? Data()
        return try Self.decode(data)
    }

    func create(_ id: UUID) throws {
        let fd = path.path.withCString {
            open($0, O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0o600)
        }
        if fd < 0 {
            if errno == EEXIST, try read() == id { return }
            throw TerminalSessionJournalError.recoveryRequired
        }
        let handle = FileHandle(fileDescriptor: fd, closeOnDealloc: true)
        defer { try? handle.close() }
        try handle.write(contentsOf: Data(id.uuidString.utf8))
        try handle.synchronize()
        let directory = path.deletingLastPathComponent()
        let directoryFD = directory.path.withCString {
            open($0, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW)
        }
        guard directoryFD >= 0 else { throw TerminalSessionJournalError.recoveryRequired }
        defer { _ = close(directoryFD) }
        guard fsync(directoryFD) == 0 else { throw TerminalSessionJournalError.recoveryRequired }
    }
    #endif

    private static func decode(_ data: Data) throws -> UUID {
        guard data.count == 36, let text = String(data: data, encoding: .utf8),
              let id = UUID(uuidString: text), id.uuidString == text else {
            throw TerminalSessionJournalError.corruptStore
        }
        return id
    }
}
