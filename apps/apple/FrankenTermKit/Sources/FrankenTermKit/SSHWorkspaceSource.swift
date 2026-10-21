import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

/// Public, exporter-owned identity registry. Keep one registry per selected source
/// namespace; persist it before publishing an export. Names and array offsets are
/// lookup keys only, never the identities sent to another device.
public struct SSHWorkspaceRegistry: Codable, Sendable {
    public let exporterID: String, workspaceID: String, groupID: String, themeID: String
    private var entities: [String: String]

    public init() {
        exporterID = UUID().uuidString
        workspaceID = UUID().uuidString
        groupID = UUID().uuidString
        themeID = UUID().uuidString
        entities = [:]
    }

    fileprivate func validate() throws {
        guard entities.count <= 4096 else { throw FTError.limit("ssh.registry") }
        let ids = [exporterID, workspaceID, groupID, themeID] + Array(entities.values)
        guard Set(ids).count == ids.count else { throw FTError.malformed("ssh.registry_identity") }
        for id in ids { _ = try WorkspaceDocument.identifier(id) }
        for key in entities.keys {
            guard !key.isEmpty, key.utf8.count <= FTLimit.text else { throw FTError.limit("ssh.registry_key") }
        }
    }

    fileprivate mutating func id(_ kind: String, _ source: String) throws -> String {
        let key = kind + ":" + source
        if let existing = entities[key] { return existing }
        guard entities.count < 4096, key.utf8.count <= FTLimit.text else { throw FTError.limit("ssh.registry") }
        let value = UUID().uuidString
        entities[key] = value
        return value
    }
}

public struct SSHWorkspaceExport: Sendable {
    public let workspace: WorkspaceDocument
    public let portability: [AirGapArchive.Portability]
    /// Local-only selection hints. These paths are never encoded into the manifest.
    public let identityReferences: [String: String]
}

/// A bounded, passive export of an explicitly selected SSH configuration file.
/// This is deliberately not `ssh -G`: Match, local commands, environment
/// expansion, DNS and agent queries are never evaluated. The file entry point
/// resolves a bounded set of declarative Include files without executing SSH.
/// Unsupported effective semantics keep the affected route visibly inactive.
public struct SSHWorkspaceSource: Sendable {
    public static let maximumSourceBytes = 2 * 1024 * 1024
    public static let maximumLines = 16384
    public static let maximumAliases = 512
    public static let maximumExpandedJumpHosts = 3
    public static let maximumKnownHostHashComparisons = 131_072
    public static let maximumIncludedFiles = 64
    public static let maximumIncludeDepth = 8
    public static let maximumIncludeDirectoryEntries = 16_384
    public static let maximumKnownHostsFiles = 64
    public static let maximumKnownHostsFilesPerSetting = 16

    /// A frozen public trust inventory. An absent file differs from a file that
    /// could not be read: only the former proves there were no records to apply.
    /// References stay on the exporter; the workspace carries keys and policy.
    public struct KnownHostsSnapshot: Sendable {
        public enum File: Equatable, Sendable {
            case present(Data), missing, unavailable
        }
        public let files: [String: File]

        public init(files: [String: File]) throws {
            guard files.count <= SSHWorkspaceSource.maximumKnownHostsFiles else { throw FTError.limit("ssh.known_hosts.files") }
            var bytes = 0, lines = 0
            for (reference, file) in files {
                try Task.checkCancellation()
                guard SSHWorkspaceSource.knownHostsReference(reference) else { throw FTError.malformed("ssh.known_hosts.reference") }
                if case .present(let data) = file {
                    guard data.count <= SSHWorkspaceSource.maximumSourceBytes - bytes else { throw FTError.limit("ssh.known_hosts.bytes") }
                    bytes += data.count
                    let count = data.reduce(into: 1) { if $1 == 10 { $0 += 1 } }
                    guard count <= SSHWorkspaceSource.maximumLines - lines else { throw FTError.limit("ssh.known_hosts.lines") }
                    lines += count
                }
            }
            self.files = files
        }
    }

    public let aliases: [String]
    private let configuration: Data
    private let directives: [Directive]
    private let globalProblems: [Problem]

    private struct Directive: Sendable {
        let section: Int
        let conditions: [[String]], keyword: String, arguments: [String]
    }
    private struct Problem: Sendable {
        let setting: String, reason: String
        let required: Bool
        init(_ setting: String, _ reason: String, required: Bool = true) {
            self.setting = setting; self.reason = reason; self.required = required
        }
    }
    private struct Resolution {
        let options: [String: [String]]
        let identities: [String]
        var problems: [Problem]
    }
    private enum RouteContext: String {
        case destination = "route", firstHop = "transit_route", followingHop = "transit_via_route"
    }
    private struct RouteKey: Hashable {
        let alias: String, context: RouteContext
    }
    private struct PlannedRoute {
        let key: RouteKey, resolution: Resolution, jumps: [RouteKey]
    }
    private enum JumpFailure: Error {
        case unsupported, unknownAlias, cycle, depth
        var problem: Problem {
            let reason: String
            switch self {
            case .unsupported:
                reason = "ProxyJump requires a comma-separated list of bare configured aliases. User/port overrides, URIs, expansions, and ambiguous forms remain unresolved; no partial hop chain was exported."
            case .unknownAlias:
                reason = "A jump host has no concrete alias in the selected configuration. Its endpoint, authentication, and host policy cannot be reconstructed; no partial hop chain was exported."
            case .cycle:
                reason = "The expanded ProxyJump chain repeats a host alias or returns to its destination. The route remains inactive; no partial hop chain was exported."
            case .depth:
                reason = "The expanded ProxyJump chain exceeds the three-hop limit. The route remains inactive; no partial hop chain was exported."
            }
            return Problem("proxyjump", reason)
        }
    }
    private struct PublicKey {
        let algorithm: String, blob: Data
        func fingerprint<C: AirGapCryptography>(using crypto: C) -> String {
            "SHA256:" + crypto.sha256(blob).base64EncodedString().replacingOccurrences(of: "=", with: "")
        }
    }
    private struct KnownHost {
        enum Names {
            case patterns([String])
            case hashed(salt: Data, digest: Data)
        }
        let names: Names, marker: String, key: PublicKey?
    }
    private struct MatchBudget {
        var remaining = 8_000_000
        var hashComparisons = SSHWorkspaceSource.maximumKnownHostHashComparisons
        mutating func consume() throws {
            guard remaining > 0 else { throw FTError.limit("ssh.pattern_work") }
            remaining -= 1
        }
        mutating func consumeHash() throws {
            guard hashComparisons > 0 else { throw FTError.limit("ssh.known_hosts_hash_work") }
            hashComparisons -= 1
        }
    }

    public init(configuration: Data) throws {
        var parser = ConfigurationParser(homeDirectory: nil)
        try parser.parse(configuration, conditions: [], depth: 0)
        self.init(parser: parser)
    }

    /// The selected file is a USER configuration, including when it lives outside
    /// ~/.ssh. OpenSSH resolves every relative Include against ~/.ssh, not against
    /// the containing file. Only already-local files are admitted. Paths with
    /// tokens, environment variables, named-user tildes or unsupported glob syntax
    /// remain visible required omissions; no executable resolution is attempted.
    public static func loadUserConfiguration(_ url: URL,
        // NSHomeDirectory exists on every platform (homeDirectoryForCurrentUser is
        // unavailable in iOS, where this shared package also compiles).
        homeDirectory: URL = URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)) throws -> SSHWorkspaceSource {
        guard url.isFileURL, homeDirectory.isFileURL else { throw FTError.malformed("ssh.include_path") }
        #if os(macOS) || os(iOS)
        let access = url.startAccessingSecurityScopedResource()
        defer { if access { url.stopAccessingSecurityScopedResource() } }
        #endif
        let home = ConfigurationParser.physicalURL(homeDirectory, isDirectory: true) ?? homeDirectory.standardizedFileURL
        var parser = ConfigurationParser(homeDirectory: home)
        // A missing selected file keeps its literal path and fails as a required omission.
        let root = ConfigurationParser.physicalURL(url, isDirectory: false) ?? url.standardizedFileURL
        try parser.load(root, conditions: [], depth: 0, required: true)
        return SSHWorkspaceSource(parser: parser)
    }

    private init(parser: ConfigurationParser) {
        configuration = parser.snapshot; aliases = parser.aliases
        directives = parser.directives; globalProblems = parser.problems
    }

    private struct ConfigurationParser {
        let homeDirectory: URL?
        var aliases: [String] = [], seen = Set<String>()
        var directives: [Directive] = [], problems: [Problem] = []
        var snapshot = Data("FrankenTerm/SSH-configuration-snapshot/v1\0".utf8)
        var sourceBytes = 0, sourceLines = 0, fileCount = 0, section = 0
        var directoryEntries = 0, includePatterns = 0, globBudget = MatchBudget()
        var activeFiles = Set<FileIdentity>()

        struct FileIdentity: Hashable { let device: UInt64, inode: UInt64 }

        mutating func unresolvedInclude(_ reason: String) {
            if !problems.contains(where: { $0.setting == "include" }) {
                problems.append(Problem("include", reason))
            }
        }

        mutating func parse(_ data: Data, conditions: [[String]], depth: Int) throws {
            try Task.checkCancellation()
            guard sourceBytes <= maximumSourceBytes - data.count else { throw FTError.limit("ssh.include_bytes") }
            sourceBytes += data.count
            let lines = try SSHWorkspaceSource.lines(data, site: "ssh.config")
            guard sourceLines <= maximumLines - lines.count else { throw FTError.limit("ssh.include_lines") }
            sourceLines += lines.count
            snapshot.appendBE(UInt64(data.count), bytes: 4); snapshot += data
            var patterns = ["*"], inMatch = false
            section += 1
            var currentSection = section
            for line in lines {
                try Task.checkCancellation()
                guard let (keyword, arguments) = try SSHWorkspaceSource.directive(line) else { continue }
                switch keyword {
                case "host":
                    guard arguments.count <= 128, arguments.allSatisfy({ $0.utf8.count <= 254 }) else { throw FTError.limit("ssh.host_patterns") }
                    patterns = arguments; inMatch = false; section += 1; currentSection = section
                    for alias in arguments where !alias.hasPrefix("!") && !alias.contains("*") && !alias.contains("?") {
                        guard !alias.isEmpty, alias.utf8.count <= 253,
                              !alias.unicodeScalars.contains(where: { $0.value <= 32 || $0.value == 127 }),
                              !alias.contains("/"), !alias.contains("\\"), !alias.contains("@"),
                              !alias.contains(","), !alias.contains("%"), !alias.contains("$") else {
                            throw FTError.malformed("ssh.host_alias")
                        }
                        var eligible = true
                        for condition in conditions {
                            if try !SSHWorkspaceSource.matches(alias, patterns: condition, budget: &globBudget) { eligible = false; break }
                        }
                        if eligible, seen.insert(alias).inserted { aliases.append(alias) }
                        guard aliases.count <= maximumAliases else { throw FTError.limit("ssh.aliases") }
                    }
                case "match":
                    inMatch = true
                    if !problems.contains(where: { $0.setting == "match" }) {
                        problems.append(Problem("match", "Match conditions and final/canonical passes were not evaluated. Resolve this route from a trusted live snapshot before using it."))
                    }
                case "include":
                    guard homeDirectory != nil, !inMatch else {
                        unresolvedInclude("Included configuration was not resolved in this source context. Its endpoints, identities, and required security policies remain unresolved.")
                        continue
                    }
                    for argument in arguments {
                        guard includePatterns < 256 else { throw FTError.limit("ssh.include_patterns") }
                        includePatterns += 1
                        guard let pattern = includePath(argument) else {
                            unresolvedInclude("An Include path requires unsupported token, environment, named-user, parent-directory, bracket or escape expansion. No command was run and affected routes remain inactive.")
                            continue
                        }
                        for included in try files(matching: pattern) {
                            // OpenSSH restores the containing Host/Match state
                            // after EACH included file. A Host * inside a file
                            // cannot escape the condition on its Include line.
                            try load(included, conditions: conditions + [patterns], depth: depth + 1)
                        }
                    }
                default:
                    if !inMatch {
                        directives.append(Directive(section: currentSection, conditions: conditions + [patterns],
                                                    keyword: keyword, arguments: arguments))
                    }
                }
            }
        }

        func includePath(_ argument: String) -> URL? {
            guard let homeDirectory, !argument.isEmpty, argument.utf8.count <= FTLimit.text,
                  !argument.contains("%"), !argument.contains("$"), !argument.contains("\\"),
                  !argument.contains("["), !argument.contains("]"),
                  !argument.unicodeScalars.contains(where: { $0.value < 32 || $0.value == 127 }),
                  !argument.split(separator: "/").contains("..") else { return nil }
            if argument.hasPrefix("~/") { return homeDirectory.appendingPathComponent(String(argument.dropFirst(2))) }
            if argument.hasPrefix("~") { return nil }
            if argument.hasPrefix("/") { return URL(fileURLWithPath: argument) }
            return homeDirectory.appendingPathComponent(".ssh").appendingPathComponent(argument)
        }

        mutating func files(matching pattern: URL) throws -> [URL] {
            let components = pattern.pathComponents.filter { $0 != "/" && $0 != "." }
            guard !components.isEmpty, components.count <= 32 else { throw FTError.limit("ssh.include_path_depth") }
            var candidates = [URL(fileURLWithPath: "/", isDirectory: true)]
            for component in components {
                try Task.checkCancellation()
                var next: [URL] = []
                for directory in candidates {
                    if component.contains("*") || component.contains("?") {
                        for name in try directoryNames(directory) {
                            if name.hasPrefix("."), !component.hasPrefix(".") { continue }
                            if try SSHWorkspaceSource.glob(name, component, budget: &globBudget) {
                                next.append(directory.appendingPathComponent(name))
                            }
                        }
                    } else { next.append(directory.appendingPathComponent(component)) }
                    guard next.count <= maximumIncludedFiles else { throw FTError.limit("ssh.include_matches") }
                }
                candidates = next
            }
            // POSIX glob's C lexical order, stable regardless of directory order.
            return candidates.sorted { $0.path.utf8.lexicographicallyPrecedes($1.path.utf8) }
        }

        mutating func directoryNames(_ url: URL) throws -> [String] {
            guard let descriptor = try Self.openLocal(url, directory: true) else { return [] }
            guard let directory = fdopendir(descriptor) else { close(descriptor); throw FTError.unsupported("ssh.include_directory") }
            defer { closedir(directory) }
            var result: [String] = []
            while true {
                try Task.checkCancellation()
                errno = 0
                guard let entry = readdir(directory) else {
                    guard errno == 0 else { throw FTError.unsupported("ssh.include_directory") }
                    break
                }
                guard directoryEntries < maximumIncludeDirectoryEntries else { throw FTError.limit("ssh.include_directory_entries") }
                directoryEntries += 1
                let nameCapacity = MemoryLayout.size(ofValue: entry.pointee.d_name)
                let name = withUnsafePointer(to: &entry.pointee.d_name) {
                    $0.withMemoryRebound(to: CChar.self, capacity: nameCapacity) { String(validatingCString: $0) }
                }
                guard let name else { throw FTError.malformed("ssh.include_filename") }
                if name != ".", name != ".." { result.append(name) }
            }
            return result
        }

        static func requireLocal(_ url: URL) throws {
            #if os(macOS) || os(iOS)
            let values = try url.resourceValues(forKeys: [.volumeIsLocalKey, .isUbiquitousItemKey, .ubiquitousItemDownloadingStatusKey])
            guard values.volumeIsLocal == true,
                  values.isUbiquitousItem != true || values.ubiquitousItemDownloadingStatus == .current else {
                throw FTError.unsupported("ssh.include_not_local")
            }
            #endif
        }

        /// The physical path of an existing selected file or home directory.
        /// Foundation's `resolvingSymlinksInPath` strips a leading `/private` on
        /// Darwin, yielding `/var/...` or `/tmp/...` whose first component is
        /// itself a symbolic link, so the O_NOFOLLOW walk below would reject
        /// every include. `realpath(3)` returns the real location instead.
        static func physicalURL(_ url: URL, isDirectory: Bool) -> URL? {
            guard let resolved = realpath(url.path, nil) else { return nil }
            defer { free(resolved) }
            return URL(fileURLWithPath: String(cString: resolved), isDirectory: isDirectory)
        }

        /// Walk from a held directory descriptor; O_NOFOLLOW applies to EVERY
        /// path component, so an included symlink cannot evade cycle/locality
        /// checks by pointing at a different directory or a special file.
        static func openLocal(_ url: URL, directory: Bool) throws -> Int32? {
            let components = url.pathComponents.filter { $0 != "/" && $0 != "." }
            guard !components.contains(".."), components.count <= 32 else { throw FTError.malformed("ssh.include_path") }
            var current = open("/", O_RDONLY | O_DIRECTORY | O_CLOEXEC)
            guard current >= 0 else { throw FTError.unsupported("ssh.include_directory") }
            defer { if current >= 0 { close(current) } }
            var path = URL(fileURLWithPath: "/", isDirectory: true)
            for (index, component) in components.enumerated() {
                try Task.checkCancellation()
                path.appendPathComponent(component)
                do { try requireLocal(path) }
                catch let error as CocoaError where error.code == .fileReadNoSuchFile || error.code == .fileNoSuchFile { return nil }
                let needsDirectory = directory || index < components.count - 1
                let flags = O_RDONLY | O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK | (needsDirectory ? O_DIRECTORY : 0)
                let next = component.withCString { openat(current, $0, flags) }
                if next < 0 {
                    let code = errno
                    if code == ENOENT { return nil }
                    if code == ENOTDIR {
                        var metadata = stat()
                        let status = component.withCString { fstatat(current, $0, &metadata, AT_SYMLINK_NOFOLLOW) }
                        if status == 0, metadata.st_mode & S_IFMT != S_IFLNK { return nil }
                    }
                    throw FTError.unsupported("ssh.include_file")
                }
                close(current); current = next
            }
            let result = current; current = -1
            return result
        }

        mutating func load(_ url: URL, conditions: [[String]], depth: Int, required: Bool = false) throws {
            try Task.checkCancellation()
            guard depth <= maximumIncludeDepth, fileCount < maximumIncludedFiles else { throw FTError.limit("ssh.include_files_or_depth") }
            // A file URL does not authorize network-backed or cloud-placeholder
            // reads. Reject symbolic links rather than silently changing sources.
            guard let descriptor = try Self.openLocal(url, directory: false) else {
                if required { throw FTError.unsupported("ssh.include_file") }
                return
            }
            let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
            defer { try? handle.close() }
            var before = stat()
            guard fstat(descriptor, &before) == 0, before.st_mode & S_IFMT == S_IFREG,
                  before.st_size >= 0, before.st_size <= maximumSourceBytes - sourceBytes else { throw FTError.limit("ssh.include_file") }
            guard (before.st_uid == getuid() || before.st_uid == 0), before.st_mode & 0o022 == 0 else { throw FTError.unsupported("ssh.include_permissions") }
            let identity = FileIdentity(device: UInt64(truncatingIfNeeded: before.st_dev), inode: UInt64(truncatingIfNeeded: before.st_ino))
            guard activeFiles.insert(identity).inserted else { throw FTError.malformed("ssh.include_cycle") }
            defer { activeFiles.remove(identity) }
            fileCount += 1
            var data = Data()
            let remaining = maximumSourceBytes - sourceBytes
            while let chunk = try handle.read(upToCount: min(65_536, remaining + 1 - data.count)), !chunk.isEmpty {
                try Task.checkCancellation()
                guard chunk.count <= remaining - data.count else { throw FTError.limit("ssh.include_bytes") }
                data += chunk
            }
            var after = stat()
            guard fstat(descriptor, &after) == 0, after.st_size == before.st_size, data.count == before.st_size else { throw FTError.stale }
            #if canImport(Darwin)
            guard before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec, before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
                  before.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec, before.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec else { throw FTError.stale }
            #else
            guard before.st_mtim.tv_sec == after.st_mtim.tv_sec, before.st_mtim.tv_nsec == after.st_mtim.tv_nsec,
                  before.st_ctim.tv_sec == after.st_ctim.tv_sec, before.st_ctim.tv_nsec == after.st_ctim.tv_nsec else { throw FTError.stale }
            #endif
            let name = Data(url.path.utf8)
            guard name.count <= FTLimit.text else { throw FTError.limit("ssh.include_path") }
            snapshot.appendBE(UInt64(name.count), bytes: 4); snapshot += name
            try parse(data, conditions: conditions, depth: depth)
        }
    }

    /// Returns references from the selected concrete aliases and their complete
    /// supported jump chains. The caller may read matching .pub files after
    /// explicit source selection; do not enumerate the user's .ssh directory or
    /// treat these strings as shell instructions.
    public func identityFileReferences(for selectedAliases: [String]) throws -> [String] {
        try validateSelection(selectedAliases)
        var seen = Set<String>(), result: [String] = [], budget = MatchBudget()
        for route in try plan(selectedAliases, budget: &budget) {
            for reference in route.resolution.identities where Self.localReference(reference) {
                if seen.insert(reference).inserted { result.append(reference) }
            }
        }
        guard result.count <= 256 else { throw FTError.limit("ssh.identity_references") }
        return result
    }

    /// Effective file lists for just the selected destinations and their jump
    /// graph. First matching assignment wins; none suppresses that whole list.
    /// No path is globbed, no environment/token expression is evaluated, and a
    /// partially supported list is never silently reduced to a usable prefix.
    public func knownHostsFileReferences(for selectedAliases: [String]) throws -> [String] {
        try validateSelection(selectedAliases)
        var seen = Set<String>(), result: [String] = [], budget = MatchBudget()
        for route in try plan(selectedAliases, budget: &budget) {
            var problems: [Problem] = []
            for reference in Self.knownHostsReferences(route.resolution.options, problems: &problems) {
                if seen.insert(reference).inserted { result.append(reference) }
            }
        }
        guard result.count <= Self.maximumKnownHostsFiles else { throw FTError.limit("ssh.known_hosts.files") }
        return result
    }

    /// Read each selected public file reference once. A private key, process, agent,
    /// DNS lookup or network service is never consulted. Unreadable/symlink/unsafe
    /// sources keep their affected routes inactive; unrelated routes can export.
    public func loadKnownHostFiles(for selectedAliases: [String],
        homeDirectory: URL = URL(fileURLWithPath: NSHomeDirectory(), isDirectory: true)) throws -> KnownHostsSnapshot {
        guard homeDirectory.isFileURL else { throw FTError.malformed("ssh.known_hosts.reference") }
        let home = ConfigurationParser.physicalURL(homeDirectory, isDirectory: true) ?? homeDirectory.standardizedFileURL
        var files: [String: KnownHostsSnapshot.File] = [:], bytes = 0
        for reference in try knownHostsFileReferences(for: selectedAliases) {
            try Task.checkCancellation()
            var url = reference.hasPrefix("~/") ? home.appendingPathComponent(String(reference.dropFirst(2)))
                : URL(fileURLWithPath: reference)
            #if os(macOS) || os(iOS)
            // Darwin's OS-owned /etc alias is the documented global default.
            // Map only this fixed prefix; arbitrary source symlinks stay refused.
            if reference.hasPrefix("/etc/") { url = URL(fileURLWithPath: "/private" + reference) }
            let access = url.startAccessingSecurityScopedResource()
            defer { if access { url.stopAccessingSecurityScopedResource() } }
            #endif
            do {
                if let data = try Self.readKnownHostsFile(url, remaining: Self.maximumSourceBytes - bytes) {
                    files[reference] = .present(data); bytes += data.count
                } else { files[reference] = .missing }
            } catch is CancellationError { throw CancellationError() }
            catch let error as FTError {
                if case .limit = error { throw error }
                files[reference] = .unavailable
            } catch { files[reference] = .unavailable }
        }
        return try KnownHostsSnapshot(files: files)
    }

    private static func readKnownHostsFile(_ url: URL, remaining: Int) throws -> Data? {
        guard let descriptor = try ConfigurationParser.openLocal(url, directory: false) else { return nil }
        let handle = FileHandle(fileDescriptor: descriptor, closeOnDealloc: true)
        defer { try? handle.close() }
        var before = stat()
        guard fstat(descriptor, &before) == 0, before.st_mode & S_IFMT == S_IFREG,
              before.st_uid == getuid() || before.st_uid == 0, before.st_mode & 0o022 == 0 else {
            throw FTError.unsupported("ssh.known_hosts.file")
        }
        guard before.st_size >= 0, before.st_size <= remaining else { throw FTError.limit("ssh.known_hosts.bytes") }
        var data = Data()
        while let chunk = try handle.read(upToCount: min(65_536, remaining + 1 - data.count)), !chunk.isEmpty {
            try Task.checkCancellation()
            guard chunk.count <= remaining - data.count else { throw FTError.limit("ssh.known_hosts.bytes") }
            data += chunk
        }
        var after = stat()
        guard fstat(descriptor, &after) == 0, after.st_size == before.st_size, data.count == before.st_size,
              after.st_uid == before.st_uid, after.st_mode == before.st_mode else { throw FTError.stale }
        #if canImport(Darwin)
        guard before.st_mtimespec.tv_sec == after.st_mtimespec.tv_sec, before.st_mtimespec.tv_nsec == after.st_mtimespec.tv_nsec,
              before.st_ctimespec.tv_sec == after.st_ctimespec.tv_sec, before.st_ctimespec.tv_nsec == after.st_ctimespec.tv_nsec else { throw FTError.stale }
        #else
        guard before.st_mtim.tv_sec == after.st_mtim.tv_sec, before.st_mtim.tv_nsec == after.st_mtim.tv_nsec,
              before.st_ctim.tv_sec == after.st_ctim.tv_sec, before.st_ctim.tv_nsec == after.st_ctim.tv_nsec else { throw FTError.stale }
        #endif
        return data
    }

    /// publicKeys maps the exact IdentityFile reference to selected .pub contents
    /// (or a binary SSH public blob). No private material is accepted or emitted.
    /// knownHostFiles applies only each route's actual effective file lists.
    /// The original knownHosts input remains a deliberately partial single
    /// source; it cannot satisfy an explicit User/GlobalKnownHostsFile policy.
    public func export<C: AirGapCryptography>(aliases selectedAliases: [String], workspaceName: String,
                                              defaultUser: String, publicKeys: [String: Data] = [:],
                                              knownHosts: Data? = nil, knownHostFiles: KnownHostsSnapshot? = nil,
                                              registry: inout SSHWorkspaceRegistry,
                                              using crypto: C) throws -> SSHWorkspaceExport {
        try validateSelection(selectedAliases)
        guard !workspaceName.isEmpty, workspaceName.utf8.count <= FTLimit.text,
              Self.validUser(defaultUser) else { throw FTError.malformed("ssh.export_name_or_user") }
        try registry.validate()
        guard publicKeys.count <= 256 else { throw FTError.limit("ssh.public_keys") }
        if let knownHosts, knownHosts.count > Self.maximumSourceBytes { throw FTError.limit("ssh.known_hosts.bytes") }
        guard knownHosts == nil || knownHostFiles == nil else { throw FTError.malformed("ssh.known_hosts.ambiguous_sources") }
        var knownByFile: [String: (entries: [KnownHost], problems: [Problem])] = [:]
        if let knownHostFiles {
            let expected = Set(try knownHostsFileReferences(for: selectedAliases))
            guard Set(knownHostFiles.files.keys).isSubset(of: expected) else { throw FTError.malformed("ssh.known_hosts.unselected_source") }
            for (reference, file) in knownHostFiles.files {
                if case .present(let data) = file { knownByFile[reference] = try Self.knownHosts(data) }
            }
        }
        var sourceBytes = Data("FrankenTerm/SSHWorkspaceSource/v1\0".utf8)
        sourceBytes.appendBE(UInt64(configuration.count), bytes: 4); sourceBytes += configuration
        if let knownHostFiles {
            sourceBytes += Data("file-scoped-known-hosts/v1\0".utf8)
            sourceBytes.appendBE(UInt64(knownHostFiles.files.count), bytes: 4)
            for reference in knownHostFiles.files.keys.sorted() {
                guard let file = knownHostFiles.files[reference] else { throw FTError.stale }
                sourceBytes.appendBE(UInt64(reference.utf8.count), bytes: 4); sourceBytes += Data(reference.utf8)
                switch file {
                case .present(let data):
                    sourceBytes.append(1); sourceBytes.appendBE(UInt64(data.count), bytes: 4); sourceBytes += data
                case .missing: sourceBytes.append(2)
                case .unavailable: sourceBytes.append(3)
                }
            }
        } else {
            sourceBytes.appendBE(UInt64(knownHosts?.count ?? 0), bytes: 4)
            if let knownHosts { sourceBytes += knownHosts }
        }
        for reference in publicKeys.keys.sorted() {
            guard let bytes = publicKeys[reference], reference.utf8.count <= FTLimit.text,
                  bytes.count <= FTLimit.key else { throw FTError.limit("ssh.public_key") }
            sourceBytes.appendBE(UInt64(reference.utf8.count), bytes: 4); sourceBytes += Data(reference.utf8)
            sourceBytes.appendBE(UInt64(bytes.count), bytes: 4); sourceBytes += bytes
            guard sourceBytes.count <= FTLimit.archive else { throw FTError.limit("ssh.source_snapshot") }
        }
        let revision = crypto.sha256(sourceBytes).ftHex.lowercased()
        let legacyKnown = try Self.knownHosts(knownHosts)
        var workingRegistry = registry, budget = MatchBudget()
        let planned = try plan(selectedAliases, budget: &budget)
        var routeIDs: [RouteKey: String] = [:]
        for item in planned { routeIDs[item.key] = try workingRegistry.id(item.key.context.rawValue, item.key.alias) }
        var routes: [JSONValue] = [], machines: [JSONValue] = [], trusts: [JSONValue] = []
        var credentials: [JSONValue] = [], order: [JSONValue] = []
        var emittedCredentials = Set<String>(), emittedTrusts = Set<String>(), references: [String: String] = [:]
        var portability = [AirGapArchive.Portability(entityID: registry.workspaceID, setting: "ssh_config.source_scope", disposition: 0,
            reason: "Created from the selected SSH configuration and supplied public keys/known-hosts data. System configuration, agent inventory, live terminal themes, and implicit certificate files were not captured. Review this explicit route configuration before connecting.")]

        for item in planned {
            let alias = item.key.alias
            guard let routeID = routeIDs[item.key] else { throw FTError.stale }
            let trustID = try workingRegistry.id("trust", alias)
            var resolved = item.resolution
            var known = (entries: [KnownHost](), problems: [Problem]())
            if let knownHostFiles {
                let references = Self.knownHostsReferences(resolved.options, problems: &resolved.problems)
                for reference in references {
                    switch knownHostFiles.files[reference] {
                    case .present:
                        guard let parsed = knownByFile[reference] else { throw FTError.stale }
                        known.entries += parsed.entries; known.problems += parsed.problems
                    case .missing: break
                    case .unavailable, nil:
                        resolved.problems.append(Problem("known_hosts", "An effective known-hosts source could not be read safely or was not supplied. Its positive trust and revocations remain unresolved; this route cannot authenticate."))
                    }
                }
            } else {
                let explicit = ["userknownhostsfile", "globalknownhostsfile"].filter { resolved.options[$0] != nil }
                if explicit.isEmpty { known = legacyKnown }
                for keyword in explicit {
                    resolved.problems.append(Problem(keyword, "This route selects its own known-hosts file policy. Supply a file-scoped trust snapshot; a pooled file cannot establish which keys and revocations apply."))
                }
            }
            let requestedHost = resolved.options["hostname"]?.first ?? alias
            let host: String
            let sourceHost: String
            if let normalized = try? WorkspaceDocument.host(requestedHost) {
                host = normalized
                sourceHost = try Self.sourceLookupHost(requestedHost)
            }
            else {
                resolved.problems.append(Problem("hostname", "The effective endpoint requires unsupported expansion or normalization. The displayed alias is an inactive placeholder."))
                host = (try? WorkspaceDocument.host(alias)) ?? "unresolved.invalid"
                sourceHost = host
            }
            var port: UInt16 = 22
            if let value = resolved.options["port"]?.first {
                if let number = UInt64(value), let valid = try? WorkspaceDocument.port(number) { port = valid }
                else { resolved.problems.append(Problem("port", "The configured port is invalid or unresolved; the route remains inactive.")) }
            }
            var user = resolved.options["user"]?.first ?? defaultUser
            if !Self.validUser(user) || user.contains("%") || user.contains("$") {
                resolved.problems.append(Problem("user", "The account name requires unresolved expansion; the route remains inactive.")); user = defaultUser
            }
            var hostKeyAlias: String?, sourceKeyAlias: String?
            if let values = resolved.options["hostkeyalias"] {
                if values.count == 1, let value = values.first, let normalized = try? WorkspaceDocument.host(value) {
                    hostKeyAlias = normalized
                    sourceKeyAlias = value.lowercased()
                } else {
                    resolved.problems.append(Problem("hostkeyalias", "The host-key lookup alias is unresolved or cannot be represented by the mobile trust policy."))
                }
            }
            let methods = Self.authentication(resolved.options, problems: &resolved.problems)
            var credentialIDs: [JSONValue] = []
            for reference in resolved.identities {
                guard Self.localReference(reference) else {
                    resolved.problems.append(Problem("identityfile", "An identity reference requires unsupported token, environment, relative-path, or home-directory resolution.")); continue
                }
                guard let data = publicKeys[reference] else {
                    resolved.problems.append(Problem("identityfile", "A referenced public identity was not supplied. No filename-based or agent identity was substituted.", required: false)); continue
                }
                guard let key = try? Self.publicKey(data) else {
                    resolved.problems.append(Problem("identityfile", "A supplied public identity is malformed or uses an unsupported key encoding. No partial identity was imported.")); continue
                }
                let fingerprint = key.fingerprint(using: crypto)
                let credentialID = try workingRegistry.id("credential", key.algorithm + ":" + fingerprint)
                if !credentialIDs.contains(.string(credentialID)) { credentialIDs.append(.string(credentialID)) }
                if emittedCredentials.insert(credentialID).inserted {
                    credentials.append(.object(["id": .string(credentialID), "algorithm": .string(key.algorithm),
                        "public_fingerprint": .string(fingerprint), "requirement": .string("matching_private_key"),
                        "private_material": .string("not_included")]))
                    references[credentialID] = reference
                }
                if key.algorithm != "ssh-ed25519" && !SSHECDSACurve.allCases.contains(where: { $0.algorithm == key.algorithm }) {
                    resolved.problems.append(Problem("private_key_format", "This public identity was preserved, but its private-key family is not supported by this build's optical exporter.", required: false))
                }
            }
            if credentialIDs.isEmpty && methods.contains("publickey") {
                resolved.problems.append(Problem("credentials", "No public-key candidate is available for this route. Supply a matching supported identity or deliberately configure an allowed interactive authentication method.", required: false))
            }
            var pins: [JSONValue] = [], revoked: [JSONValue] = []
            // OpenSSH uses an explicit HostKeyAlias verbatim instead of the
            // normal host/port lookup name, including at nonstandard ports.
            let lookup = sourceKeyAlias ?? (port == 22 ? sourceHost : "[\(sourceHost)]:\(port)")
            let canonicalLookup = hostKeyAlias ?? (port == 22 ? host : "[\(host)]:\(port)")
            for problem in known.problems { resolved.problems.append(problem) }
            for entry in known.entries {
                let sourceMatches = try Self.knownHostMatches(lookup, names: entry.names, budget: &budget, using: crypto)
                // Normalizing a hostname for the mobile model cannot erase a
                // source-side denial. Positive pins still need the source lookup
                // to match; do not broaden trust across alternate spellings.
                let canonicalMatches = sourceMatches || lookup == canonicalLookup || entry.marker.isEmpty ? sourceMatches
                    : try Self.knownHostMatches(canonicalLookup, names: entry.names, budget: &budget, using: crypto)
                guard sourceMatches || (!entry.marker.isEmpty && canonicalMatches) else { continue }
                guard let key = entry.key else {
                    resolved.problems.append(Problem("known_hosts", "A matching known-hosts record has an invalid or unsupported public key. Trust cannot be reconstructed completely.")); continue
                }
                let identity = JSONValue.object(["algorithm": .string(key.algorithm), "fingerprint": .string(key.fingerprint(using: crypto))])
                switch entry.marker {
                case "": if !pins.contains(identity) { pins.append(identity) }
                case "@revoked": if !revoked.contains(identity) { revoked.append(identity) }
                case "@cert-authority":
                    resolved.problems.append(Problem("host_ca", "A matching host-certificate authority requires certificate policy support. It was not converted into a direct host-key pin."))
                default:
                    resolved.problems.append(Problem("known_hosts_marker", "A matching known-hosts marker is unsupported; the route remains inactive."))
                }
            }
            if pins.isEmpty {
                resolved.problems.append(Problem("host_trust", "No matching positive host-key pin was supplied. The machine will await explicit host verification before authentication.", required: false))
            }
            let problems = Self.merged(resolved.problems)
            for problem in problems {
                portability.append(AirGapArchive.Portability(entityID: routeID, setting: "ssh_config." + problem.setting,
                    disposition: problem.required ? 1 : 0, reason: problem.reason))
            }
            guard portability.count <= 1024 else { throw FTError.limit("ssh.portability") }
            let jumps = try item.jumps.map { key -> JSONValue in
                guard let id = routeIDs[key] else { throw FTError.stale }
                return .string(id)
            }
            var route: [String: JSONValue] = ["id": .string(routeID), "mode": .string("direct_ssh"),
                "host": .string(host), "port": .number(String(port)), "user": .string(user), "credential_refs": .array(credentialIDs),
                "authentication_methods": .array(methods.map(JSONValue.string)), "host_trust_ref": .string(trustID), "jump_routes": .array(jumps)]
            if item.key.context == .destination { route["session_creation"] = .string("on_select") }
            if problems.contains(where: \.required) { route["required_capabilities"] = .array([.string("ssh_config.unresolved_policy")]) }
            if let hostKeyAlias { route["host_key_alias"] = .string(hostKeyAlias) }
            routes.append(.object(route))
            var trust: [String: JSONValue] = ["id": .string(trustID), "host": .string(host), "port": .number(String(port)),
                "claimed_source": .string("selected_known_hosts"), "pins": .array(pins), "revoked_keys": .array(revoked)]
            if let hostKeyAlias { trust["host_key_alias"] = .string(hostKeyAlias) }
            if emittedTrusts.insert(trustID).inserted { trusts.append(.object(trust)) }
            if item.key.context == .destination {
                let machineID = try workingRegistry.id("machine", alias)
                machines.append(.object(["id": .string(machineID), "name": .string(alias), "theme_ref": .string(registry.themeID),
                    "route_refs": .array([.string(routeID)]), "preferred_route_ref": .string(routeID), "auto_connect": .boolean(false),
                    "pane_selection": .string("last_surviving")]))
                order.append(.string(machineID))
            }
        }
        guard let first = order.first else { throw FTError.malformed("ssh.selection") }
        let root = JSONValue.object(["format": .string("ftworkspace"), "version": .number("1"), "bundle_id": .string(UUID().uuidString),
            "source": .object(["exporter_id": .string(registry.exporterID), "kind": .string("ssh_config"), "configuration_revision": .string(revision)]),
            "required_capabilities": .array([]), "workspace": .object([
                "id": .string(registry.workspaceID), "name": .string(workspaceName),
                "groups": .array([.object(["id": .string(registry.groupID), "name": .string(workspaceName), "machine_order": .array(order), "auto_connect": .boolean(false)])]),
                "machines": .array(machines), "routes": .array(routes), "credentials": .array(credentials), "host_trust": .array(trusts),
                "themes": .array([Self.theme(id: registry.themeID)]),
                "preferences": .object(["initial_group_ref": .string(registry.groupID), "initial_machine_ref": first, "connect_on_open": .boolean(false)])])])
        let workspace = try WorkspaceDocument(data: StrictJSON.encode(root))
        // This also checks reference validity, duplicate findings, and the exact
        // archive metadata profile before the caller receives a usable snapshot.
        _ = try AirGapArchive(workspace: workspace, keys: [], portability: portability, using: crypto)
        registry = workingRegistry
        return SSHWorkspaceExport(workspace: workspace, portability: portability, identityReferences: references)
    }

    private func validateSelection(_ selected: [String]) throws {
        guard !selected.isEmpty, selected.count <= 128, Set(selected).count == selected.count,
              selected.allSatisfy(Set(aliases).contains) else { throw FTError.malformed("ssh.selection") }
    }

    /// Resolve OpenSSH's ordered ProxyJump semantics without invoking ssh or a
    /// shell. In ssh.c, the last listed jump is passed the preceding list with
    /// `-J`, overriding that jump's configured proxy. Only the first listed jump
    /// inherits its own configured ProxyJump, which may add an earlier prefix.
    /// Flatten before publishing so every transit record supplies only endpoint,
    /// authentication and trust, with no shell or independent nested-hop action.
    private func plan(_ selected: [String], budget: inout MatchBudget) throws -> [PlannedRoute] {
        let inventory = Set(aliases)
        var cache: [String: Resolution] = [:]
        func resolved(_ alias: String) throws -> Resolution {
            if let value = cache[alias] { return value }
            let value = try resolve(alias, budget: &budget)
            cache[alias] = value
            return value
        }
        func expand(_ alias: String, path: Set<String>, remaining: Int) throws -> [String] {
            guard !path.contains(alias) else { throw JumpFailure.cycle }
            guard let values = try resolved(alias).options["proxyjump"] else { return [] }
            if values.count == 1, values[0].lowercased() == "none" { return [] }
            guard values.count == 1 else { throw JumpFailure.unsupported }
            let hops = values[0].split(separator: ",", omittingEmptySubsequences: false).map(String.init)
            guard hops.allSatisfy({ name in
                !name.isEmpty && !name.hasPrefix("-") && name.lowercased() != "none" && name.utf8.allSatisfy {
                    (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || [45, 46, 95].contains($0)
                }
            }) else { throw JumpFailure.unsupported }
            guard hops.allSatisfy(inventory.contains) else { throw JumpFailure.unknownAlias }
            guard Set(hops).count == hops.count, !hops.contains(alias), !hops.contains(where: path.contains) else { throw JumpFailure.cycle }
            guard hops.count <= remaining, let first = hops.first else { throw JumpFailure.depth }
            let prefix = try expand(first, path: path.union([alias]), remaining: remaining - hops.count)
            let chain = prefix + hops
            guard Set(chain).count == chain.count else { throw JumpFailure.cycle }
            return chain
        }
        var destinations: [PlannedRoute] = [], transit: [PlannedRoute] = [], retained = Set<RouteKey>()
        for alias in selected {
            var root = try resolved(alias)
            let chain: [String]
            do { chain = try expand(alias, path: [], remaining: Self.maximumExpandedJumpHosts) }
            catch let failure as JumpFailure { root.problems.append(failure.problem); chain = [] }
            var jumps: [RouteKey] = []
            for (index, hop) in chain.enumerated() {
                let key = RouteKey(alias: hop, context: index == 0 ? .firstHop : .followingHop)
                jumps.append(key)
                guard retained.insert(key).inserted else { continue }
                var policy = try resolved(hop)
                if index > 0 {
                    // The explicit earlier prefix replaces this otherwise
                    // configured ProxyCommand; no command is executed.
                    policy.problems.removeAll { $0.setting == "proxycommand" }
                }
                transit.append(PlannedRoute(key: key, resolution: policy, jumps: []))
                guard selected.count + transit.count <= 256 else { throw FTError.limit("ssh.routes") }
            }
            destinations.append(PlannedRoute(key: RouteKey(alias: alias, context: .destination), resolution: root, jumps: jumps))
        }
        return destinations + transit
    }

    private func resolve(_ alias: String, budget: inout MatchBudget) throws -> Resolution {
        var options: [String: [String]] = [:], identities: [String] = [], problems = globalProblems
        var selectedProxy = false, hasIdentityFile = false
        var matchedSections: [Int: Bool] = [:]
        for directive in directives {
            let matched: Bool
            if let cached = matchedSections[directive.section] { matched = cached }
            else {
                var all = true
                for patterns in directive.conditions {
                    if try !Self.matches(alias, patterns: patterns, budget: &budget) { all = false; break }
                }
                matched = all
                matchedSections[directive.section] = matched
            }
            guard matched else { continue }
            let key = directive.keyword, arguments = directive.arguments
            if key == "identityfile" {
                hasIdentityFile = true
                if arguments.count != 1 { problems.append(Problem(key, "IdentityFile requires one bounded reference.")) }
                else if let value = arguments.first, value != "none", !identities.contains(value) { identities.append(value) }
                continue
            }
            if key == "proxycommand" || key == "proxyjump" {
                if selectedProxy { continue }
                selectedProxy = true
            }
            guard options[key] == nil else { continue }
            options[key] = arguments
            let value = arguments.first ?? ""
            switch key {
            case "hostname", "port", "user", "hostkeyalias", "preferredauthentications", "pubkeyauthentication", "passwordauthentication", "kbdinteractiveauthentication", "identitiesonly":
                if arguments.count != 1 { problems.append(Problem(key, "This SSH setting requires exactly one value; its semantics remain unresolved.")) }
            case "userknownhostsfile", "globalknownhostsfile":
                // The whole first matching list is resolved against a frozen
                // file inventory at export, separately for every route and hop.
                break
            case "proxyjump":
                // The complete ordered dependency planner validates this value
                // in its actual destination/transit context before publication.
                break
            case "proxycommand", "identityagent", "pkcs11provider", "securitykeyprovider", "remotecommand":
                if arguments != ["none"] { problems.append(Problem(key, "This route depends on an unsupported proxy, agent, external key provider, or command. No executable value was exported.")) }
            case "canonicalizehostname", "checkhostip", "forwardagent", "forwardx11", "forwardx11trusted", "gssapiauthentication", "hostbasedauthentication", "permitlocalcommand", "batchmode":
                if arguments != ["no"] { problems.append(Problem(key, "This enabled SSH behavior is not represented by the mobile route and must be resolved explicitly.")) }
            case "stricthostkeychecking":
                if arguments.count != 1 || !["yes", "ask"].contains(value) { problems.append(Problem(key, "Automatic or disabled host verification is not a portable trust policy. Review the route's host identity explicitly.")) }
            case "sessiontype":
                if arguments != ["default"] { problems.append(Problem(key, "This non-shell session type is not implemented by the mobile route.")) }
            case "loglevel", "hashknownhosts", "addkeystoagent", "usekeychain", "visualhostkey":
                problems.append(Problem(key, "This desktop presentation or local key-storage preference is not carried to the phone.", required: false))
            default:
                problems.append(Problem(key, "This configured SSH setting has no qualified mobile equivalent. It was not silently replaced with backend defaults."))
            }
        }
        if !hasIdentityFile {
            // Documented OpenSSH default order. Merely returning these references
            // does not authorize unrelated agent identities or load a private key.
            identities = ["~/.ssh/id_rsa", "~/.ssh/id_ecdsa", "~/.ssh/id_ecdsa_sk", "~/.ssh/id_ed25519", "~/.ssh/id_ed25519_sk", "~/.ssh/id_mldsa44_ed25519"]
            problems.append(Problem("default_identities", "No IdentityFile was configured. Standard OpenSSH public-key file candidates were suggested in documented order; only supplied, validated public identities are represented. Agent and implicit certificate inventories were not collected.", required: false))
        }
        if options["pubkeyauthentication"] == ["no"] { identities = [] }
        if identities.count > 8 {
            identities = []
            problems.append(Problem("identityfile", "The route exceeds the eight-candidate identity limit. No partial prefix was exported; select a smaller explicit identity policy."))
        }
        if let values = options["identitiesonly"], ![["yes"], ["no"]].contains(values) {
            problems.append(Problem("identitiesonly", "The identity restriction is invalid or unresolved."))
        }
        return Resolution(options: options, identities: identities, problems: problems)
    }

    private static func authentication(_ options: [String: [String]], problems: inout [Problem]) -> [String] {
        let switches = ["publickey": "pubkeyauthentication", "password": "passwordauthentication", "keyboard-interactive": "kbdinteractiveauthentication"]
        var enabled = Set<String>()
        for (method, key) in switches {
            let value = options[key] ?? ["yes"]
            if value == ["yes"] { enabled.insert(method) }
            else if value != ["no"] { problems.append(Problem(key, "The authentication policy is invalid or unsupported; no backend default was substituted.")) }
        }
        let preferred = (options["preferredauthentications"]?.first ?? "publickey,keyboard-interactive,password").split(separator: ",", omittingEmptySubsequences: false).map(String.init)
        var result: [String] = []
        for method in preferred {
            if switches[method] != nil {
                if enabled.contains(method), !result.contains(method) { result.append(method) }
            } else if method == "gssapi-with-mic", options["gssapiauthentication"] == nil || options["gssapiauthentication"] == ["no"] {
                continue
            } else if method == "hostbased", options["hostbasedauthentication"] == nil || options["hostbasedauthentication"] == ["no"] {
                continue
            } else { problems.append(Problem("preferredauthentications", "The ordered authentication policy contains an unsupported method.")) }
        }
        if result.isEmpty {
            // Structurally represent the machine without granting a usable policy.
            problems.append(Problem("authentication", "No permitted supported authentication method remains. This route is inactive until its policy is deliberately replaced."))
            result = ["publickey"]
        }
        return result
    }

    private static func localReference(_ reference: String) -> Bool {
        (reference.hasPrefix("/") || reference.hasPrefix("~/")) && !reference.contains("%") && !reference.contains("$") &&
        !reference.contains("\\") && !reference.unicodeScalars.contains(where: { $0.value < 32 || $0.value == 127 })
    }
    private static func knownHostsReference(_ reference: String) -> Bool {
        !reference.isEmpty && reference.utf8.count <= FTLimit.text && localReference(reference)
            && !reference.split(separator: "/").contains("..")
    }
    private static func knownHostsReferences(_ options: [String: [String]], problems: inout [Problem]) -> [String] {
        let defaults = [("userknownhostsfile", ["~/.ssh/known_hosts", "~/.ssh/known_hosts2"]),
                        ("globalknownhostsfile", ["/etc/ssh/ssh_known_hosts", "/etc/ssh/ssh_known_hosts2"])]
        var result: [String] = [], seen = Set<String>()
        for (keyword, fallback) in defaults {
            let references = options[keyword] ?? fallback
            if references.count == 1, references[0].lowercased() == "none" { continue }
            guard !references.isEmpty, references.count <= maximumKnownHostsFilesPerSetting,
                  !references.contains(where: { $0.lowercased() == "none" }),
                  references.allSatisfy(knownHostsReference) else {
                problems.append(Problem(keyword, "The complete known-hosts file list needs unsupported relative-path, token, environment, named-user or parent-directory resolution, mixes none with paths, or exceeds the sixteen-file limit. No partial list was substituted."))
                continue
            }
            for reference in references where seen.insert(reference).inserted { result.append(reference) }
        }
        return result
    }
    private static func validUser(_ user: String) -> Bool {
        !user.isEmpty && user.utf8.count <= 256 && !user.unicodeScalars.contains(where: { $0.value <= 32 || $0.value == 127 })
    }
    private static func merged(_ problems: [Problem]) -> [Problem] {
        var order: [String] = [], map: [String: Problem] = [:]
        for problem in problems {
            if let old = map[problem.setting] {
                if problem.required && !old.required { map[problem.setting] = problem }
            } else { order.append(problem.setting); map[problem.setting] = problem }
        }
        return order.compactMap { map[$0] }
    }

    private static func lines(_ data: Data, site: String) throws -> [Substring] {
        guard data.count <= maximumSourceBytes, let text = String(data: data, encoding: .utf8), !text.contains("\0") else {
            throw FTError.limit(site + ".bytes_or_encoding")
        }
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false)
        guard lines.count <= maximumLines else { throw FTError.limit(site + ".lines") }
        return lines
    }
    private static func directive(_ line: Substring) throws -> (String, [String])? {
        guard line.utf8.count <= FTLimit.text else { throw FTError.limit("ssh.config_line") }
        let text = line.trimmingCharacters(in: .whitespacesAndNewlines)
        if text.isEmpty || text.hasPrefix("#") { return nil }
        let bytes = Array(text.utf8)
        var i = 0
        while i < bytes.count, bytes[i] != 32, bytes[i] != 9, bytes[i] != 61 { i += 1 }
        let keywordBytes = bytes[..<i]
        guard !keywordBytes.isEmpty, keywordBytes.allSatisfy({ (65...90).contains($0) || (97...122).contains($0) || (48...57).contains($0) }) else {
            throw FTError.malformed("ssh.keyword")
        }
        var keyword = String(decoding: keywordBytes, as: UTF8.self).lowercased()
        if keyword == "challengeresponseauthentication" { keyword = "kbdinteractiveauthentication" }
        while i < bytes.count, bytes[i] == 32 || bytes[i] == 9 { i += 1 }
        if i < bytes.count, bytes[i] == 61 { i += 1 }
        var result: [String] = [], token: [UInt8] = [], quoted = false, active = false
        while i < bytes.count {
            let byte = bytes[i]; i += 1
            if byte == 34 { quoted.toggle(); active = true; continue }
            if byte == 35 && !quoted { break }
            if !quoted && (byte == 32 || byte == 9) {
                if active { result.append(String(decoding: token, as: UTF8.self)); token = []; active = false }
                continue
            }
            guard byte >= 32 || byte == 9 else { throw FTError.malformed("ssh.control") }
            if byte == 92 && quoted && i < bytes.count && [34, 92].contains(bytes[i]) {
                token.append(bytes[i]); i += 1
            } else { token.append(byte) }
            active = true
        }
        guard !quoted else { throw FTError.malformed("ssh.quote") }
        if active { result.append(String(decoding: token, as: UTF8.self)) }
        guard !result.isEmpty, result.count <= 256 else { throw FTError.malformed("ssh.arguments") }
        return (keyword, result)
    }

    /// Glob matching uses bounded iterative backtracking, not an attacker-built
    /// regular expression or recursion. Both input and pattern have source caps.
    private static func matches(_ value: String, patterns: [String], ignoreCase: Bool = false, budget: inout MatchBudget) throws -> Bool {
        let value = ignoreCase ? value.lowercased() : value
        var positive = false
        for item in patterns {
            try budget.consume()
            let negated = item.hasPrefix("!")
            let raw = negated ? String(item.dropFirst()) : item
            let pattern = ignoreCase ? raw.lowercased() : raw
            if try glob(value, pattern, budget: &budget) {
                if negated { return false }
                positive = true
            }
        }
        return positive
    }
    private static func glob(_ value: String, _ pattern: String, budget: inout MatchBudget) throws -> Bool {
        let text = Array(value.utf8), pattern = Array(pattern.utf8)
        var t = 0, p = 0, star: Int?, retry = 0
        while t < text.count {
            try budget.consume()
            if p < pattern.count && (pattern[p] == 63 || pattern[p] == text[t]) { t += 1; p += 1 }
            else if p < pattern.count && pattern[p] == 42 { star = p; p += 1; retry = t }
            else if let previous = star { retry += 1; t = retry; p = previous + 1 }
            else { return false }
        }
        while p < pattern.count && pattern[p] == 42 { try budget.consume(); p += 1 }
        return p == pattern.count
    }

    private static func publicKey(_ data: Data) throws -> PublicKey {
        guard !data.isEmpty, data.count <= FTLimit.key else { throw FTError.limit("ssh.public_key") }
        let algorithm: String, blob: Data
        if data.first == 0 {
            var reader = ByteCursor(data)
            guard let name = String(data: try reader.sshString(cap: 128), encoding: .utf8) else { throw FTError.malformed("ssh.public_key_type") }
            algorithm = name; blob = data
        } else {
            guard let text = String(data: data, encoding: .utf8) else { throw FTError.malformed("ssh.public_key_text") }
            let lines = text.split(whereSeparator: { $0 == "\n" || $0 == "\r" }).filter { !$0.trimmingCharacters(in: .whitespaces).isEmpty }
            guard lines.count == 1 else { throw FTError.malformed("ssh.public_key_lines") }
            let fields = lines[0].split(whereSeparator: { $0 == " " || $0 == "\t" })
            guard fields.count >= 2, let decoded = Data(base64Encoded: String(fields[1])), decoded.count <= FTLimit.key,
                  decoded.base64EncodedString() == fields[1] else { throw FTError.malformed("ssh.public_key_base64") }
            algorithm = String(fields[0]); blob = decoded
        }
        var reader = ByteCursor(blob)
        guard try reader.sshString(cap: 128) == Data(algorithm.utf8) else { throw FTError.malformed("ssh.public_key_type") }
        switch algorithm {
        case "ssh-ed25519":
            guard try reader.sshString(cap: 32).count == 32 else { throw FTError.malformed("ssh.ed25519_public") }
        case "ssh-rsa":
            let exponent = try reader.sshString(cap: 16), modulus = try reader.sshString(cap: 2049)
            guard positiveMPInt(exponent), positiveMPInt(modulus), modulus.count >= 129 else { throw FTError.malformed("ssh.rsa_public") }
        case "ecdsa-sha2-nistp256", "ecdsa-sha2-nistp384", "ecdsa-sha2-nistp521":
            let curve = String(algorithm.dropFirst("ecdsa-sha2-".count))
            let pointLength = curve == "nistp256" ? 65 : (curve == "nistp384" ? 97 : 133)
            guard try reader.sshString(cap: 16) == Data(curve.utf8) else { throw FTError.malformed("ssh.ecdsa_curve") }
            let point = try reader.sshString(cap: pointLength)
            guard point.count == pointLength, point.first == 4 else { throw FTError.malformed("ssh.ecdsa_point") }
        default: throw FTError.unsupported("ssh.public_key_family")
        }
        guard reader.remaining == 0 else { throw FTError.malformed("ssh.public_key_trailing") }
        return PublicKey(algorithm: algorithm, blob: blob)
    }
    private static func positiveMPInt(_ data: Data) -> Bool {
        guard let first = data.first, first < 128 else { return false }
        if first == 0 { return data.count > 1 && data[data.startIndex + 1] >= 128 }
        return true
    }
    /// OpenSSH canonicalizes numeric addresses even without CanonicalizeHostname,
    /// using the platform's numeric representation. Keep this separate from the
    /// workspace endpoint: IPv4-mapped IPv6 has an IPv6 lookup name in OpenSSH,
    /// while the mobile trust scope intentionally merges it with the IPv4 host.
    /// inet_pton/inet_ntop perform no DNS lookup or network access.
    private static func sourceLookupHost(_ value: String) throws -> String {
        #if canImport(Darwin) || canImport(Glibc)
        var address = in6_addr()
        if value.withCString({ inet_pton(AF_INET6, $0, &address) }) == 1 {
            var output = [CChar](repeating: 0, count: Int(INET6_ADDRSTRLEN))
            let converted = withUnsafePointer(to: &address) { address in
                output.withUnsafeMutableBufferPointer { buffer in
                    inet_ntop(AF_INET6, UnsafeRawPointer(address), buffer.baseAddress, socklen_t(buffer.count)) != nil
                }
            }
            guard converted else { throw FTError.malformed("ssh.numeric_lookup") }
            return String(decoding: output.prefix { $0 != 0 }.map { UInt8(bitPattern: $0) }, as: UTF8.self)
        }
        #endif
        // DNS names and explicit HostKeyAlias values keep their source spelling
        // after OpenSSH's lowercase conversion, including any trailing dot.
        return value.lowercased()
    }
    private static func knownHostMatches<C: AirGapCryptography>(_ lookup: String, names: KnownHost.Names,
                                                               budget: inout MatchBudget, using crypto: C) throws -> Bool {
        switch names {
        case .patterns(let patterns):
            return try matches(lookup, patterns: patterns, ignoreCase: true, budget: &budget)
        case .hashed(let salt, let digest):
            try budget.consumeHash()
            let candidate = try crypto.hmacSHA1(key: salt, message: Data(lookup.utf8))
            guard candidate.count == 20 else { throw FTError.malformed("ssh.known_hosts_hash_result") }
            return candidate == digest
        }
    }
    private static func hashedHost(_ field: String) -> KnownHost.Names? {
        // OpenSSH v1 permits exactly one hashed hostname, without a wildcard,
        // negation, comma list, unknown hash version, or noncanonical base64.
        let parts = field.split(separator: "|", omittingEmptySubsequences: false)
        guard parts.count == 4, parts[0].isEmpty, parts[1] == "1",
              let salt = Data(base64Encoded: String(parts[2])), salt.count == 20,
              let digest = Data(base64Encoded: String(parts[3])), digest.count == 20,
              salt.base64EncodedString() == parts[2], digest.base64EncodedString() == parts[3]
        else { return nil }
        return .hashed(salt: salt, digest: digest)
    }
    private static func knownHosts(_ data: Data?) throws -> (entries: [KnownHost], problems: [Problem]) {
        guard let data else { return ([], []) }
        let lines = try lines(data, site: "ssh.known_hosts")
        var entries: [KnownHost] = [], problems: [Problem] = []
        for line in lines {
            guard line.utf8.count <= 16384 else { throw FTError.limit("ssh.known_hosts_line") }
            let text = line.trimmingCharacters(in: .whitespacesAndNewlines)
            if text.isEmpty || text.hasPrefix("#") { continue }
            var fields = text.split(whereSeparator: { $0 == " " || $0 == "\t" })
            var marker = ""
            if fields.first?.hasPrefix("@") == true { marker = String(fields.removeFirst()) }
            guard fields.count >= 3, fields.first?.hasPrefix("@") != true else {
                problems.append(Problem("known_hosts", "A known-hosts record is malformed; applicable trust or revocation constraints cannot be determined.")); continue
            }
            let patterns = String(fields[0]).split(separator: ",", omittingEmptySubsequences: false).map(String.init)
            guard patterns.count <= 128, patterns.allSatisfy({ !$0.isEmpty && $0.utf8.count <= FTLimit.text }) else {
                throw FTError.limit("ssh.known_hosts_patterns")
            }
            let names: KnownHost.Names
            if patterns.contains(where: { $0.contains("|") }) {
                guard patterns.count == 1, let hashed = hashedHost(patterns[0]) else {
                    problems.append(Problem("hashed_known_hosts", "A hashed known-hosts record is malformed or uses an unsupported format. Its applicable trust or revocation constraints cannot be determined.")); continue
                }
                names = hashed
            } else {
                names = .patterns(patterns)
            }
            let keyData = Data((String(fields[1]) + " " + String(fields[2])).utf8)
            entries.append(KnownHost(names: names, marker: marker, key: try? publicKey(keyData)))
        }
        return (entries, merged(problems))
    }
    private static func theme(id: String) -> JSONValue {
        .object(["id": .string(id), "name": .string("FrankenTerm Cobalt"), "foreground": .string("#EAF2FF"),
            "background": .string("#10223C"), "accent": .string("#3B82F6"), "cursor_foreground": .string("#10223C"),
            "cursor_background": .string("#FFFFFF"), "cursor_border": .string("#FFFFFF"),
            "selection_foreground": .string("#FFFFFF"), "selection_background": .string("#345D91"),
            "ansi": .array(["#14253D", "#FF6B7A", "#7BD88F", "#FFD166", "#6DA9FF", "#C792EA", "#66D9E8", "#CEDAF0",
                "#657B99", "#FF9AA5", "#A4E8B2", "#FFE29A", "#9BC3FF", "#DDB4F5", "#98E8F1", "#FFFFFF"].map(JSONValue.string))])
    }
}
