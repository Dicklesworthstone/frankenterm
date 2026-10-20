import Foundation
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif

/// Site labels are constants, never remote text, paths, key bytes, or passwords.
public enum FTError: Error, Equatable, Sendable, LocalizedError {
    case malformed(String), unsupported(String), limit(String)
    case authentication, checksum, stale, cancelled, storage
    public var errorDescription: String? {
        switch self {
        case .malformed(let site): return "Invalid data (\(site))."
        case .unsupported(let site): return "Not supported by this build (\(site))."
        case .limit(let site): return "Resource limit reached (\(site))."
        case .authentication: return "The code is incorrect or the encrypted package was altered."
        case .checksum: return "The data checksum does not match."
        case .stale: return "This operation belongs to an earlier source or session."
        case .cancelled: return "The operation was cancelled."
        case .storage: return "Protected storage could not complete the operation."
        }
    }
}

public enum FTLimit {
    public static let manifest = 2 * 1024 * 1024
    public static let archive = 8 * 1024 * 1024
    public static let envelope = 4 * 1024 * 1024
    public static let key = 64 * 1024
    public static let text = 4096
    public static let page = 32768
    public static let qrText = 2048
}

/// Bounded JSON syntax tree. Unknown extension payloads never enter a saved workspace.
public indirect enum JSONValue: Equatable, Sendable {
    case object([String: JSONValue]), array([JSONValue]), string(String)
    case number(String), boolean(Bool), null
    public func object() throws -> [String: JSONValue] {
        guard case .object(let value) = self else { throw FTError.malformed("json.object") }
        return value
    }
    public func array() throws -> [JSONValue] {
        guard case .array(let value) = self else { throw FTError.malformed("json.array") }
        return value
    }
    public func string() throws -> String {
        guard case .string(let value) = self else { throw FTError.malformed("json.string") }
        return value
    }
    public func uint() throws -> UInt64 {
        guard case .number(let value) = self, !value.isEmpty,
              value.utf8.allSatisfy({ (48...57).contains($0) }), let n = UInt64(value)
        else { throw FTError.malformed("json.unsigned_integer") }
        return n
    }

    /// Used only for an explicit public-data transformation. Unchanged manifests
    /// retain their original bytes; numbers are never round-tripped through Double.
    func encoded() throws -> Data { try StrictJSON.encode(self) }
}

/// Validates duplicate keys *before* building a Foundation dictionary.
/// Node, depth, string, and input caps bound small-token and escape attacks.
public struct StrictJSON {
    private let bytes: [UInt8]
    private var offset = 0
    private var nodes = 0
    public static func decode(_ data: Data) throws -> JSONValue {
        guard !data.isEmpty, data.count <= FTLimit.manifest else { throw FTError.limit("json.bytes") }
        var parser = StrictJSON(bytes: Array(data))
        let value = try parser.value(depth: 0)
        parser.whitespace()
        guard parser.offset == parser.bytes.count else { throw FTError.malformed("json.trailing") }
        return value
    }
    /// Sorted object keys and exact numeric tokens make selections reproducible.
    /// The same decoder validates the result, including callers' number tokens.
    public static func encode(_ value: JSONValue) throws -> Data {
        var output = Data(), nodes = 0
        let encoder = JSONEncoder()
        func append(_ data: Data) throws {
            guard data.count <= FTLimit.manifest - output.count else { throw FTError.limit("json.bytes") }
            output += data
        }
        func write(_ value: JSONValue, depth: Int) throws {
            nodes += 1
            guard nodes <= 32768, depth <= 12 else { throw FTError.limit("json.complexity") }
            switch value {
            case .object(let fields):
                try append(Data([123]))
                for (index, key) in fields.keys.sorted().enumerated() {
                    if index > 0 { try append(Data([44])) }
                    guard key.utf8.count <= FTLimit.text else { throw FTError.limit("json.string") }
                    try append(encoder.encode(key)); try append(Data([58]))
                    if let child = fields[key] { try write(child, depth: depth + 1) }
                }
                try append(Data([125]))
            case .array(let items):
                try append(Data([91]))
                for (index, item) in items.enumerated() {
                    if index > 0 { try append(Data([44])) }
                    try write(item, depth: depth + 1)
                }
                try append(Data([93]))
            case .string(let text):
                guard text.utf8.count <= FTLimit.text else { throw FTError.limit("json.string") }
                try append(encoder.encode(text))
            case .number(let number):
                guard number.utf8.count <= 64,
                      case .number = try decode(Data(number.utf8)) else { throw FTError.malformed("json.number") }
                try append(Data(number.utf8))
            case .boolean(let value): try append(Data((value ? "true" : "false").utf8))
            case .null: try append(Data("null".utf8))
            }
        }
        try write(value, depth: 0)
        _ = try decode(output)
        return output
    }
    private mutating func whitespace() {
        while offset < bytes.count, [9, 10, 13, 32].contains(bytes[offset]) { offset += 1 }
    }
    private mutating func take(_ byte: UInt8) -> Bool {
        guard offset < bytes.count, bytes[offset] == byte else { return false }
        offset += 1
        return true
    }
    private mutating func text() throws -> String {
        let start = offset
        guard take(34) else { throw FTError.malformed("json.quote") }
        while offset < bytes.count {
            guard offset - start <= FTLimit.text * 6 + 2 else { throw FTError.limit("json.string") }
            let byte = bytes[offset]
            offset += 1
            if byte == 34 {
                let data = Data(bytes[start..<offset])
                guard let value = try? JSONDecoder().decode(String.self, from: data),
                      value.utf8.count <= FTLimit.text else { throw FTError.malformed("json.string") }
                return value
            }
            guard byte >= 32 else { throw FTError.malformed("json.control") }
            if byte == 92 {
                guard offset < bytes.count else { throw FTError.malformed("json.escape") }
                let escaped = bytes[offset]
                offset += 1
                if escaped == 117 {
                    guard bytes.count - offset >= 4,
                          bytes[offset..<offset + 4].allSatisfy({
                              (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0)
                          }) else { throw FTError.malformed("json.unicode") }
                    offset += 4
                } else if ![34, 47, 92, 98, 102, 110, 114, 116].contains(escaped) {
                    throw FTError.malformed("json.escape")
                }
            }
        }
        throw FTError.malformed("json.unterminated_string")
    }
    private mutating func value(depth: Int) throws -> JSONValue {
        nodes += 1
        guard nodes <= 32768, depth <= 12 else { throw FTError.limit("json.complexity") }
        whitespace()
        guard offset < bytes.count else { throw FTError.malformed("json.eof") }
        if bytes[offset] == 34 { return .string(try text()) }
        if take(123) {
            var result: [String: JSONValue] = [:]
            whitespace()
            if take(125) { return .object(result) }
            while true {
                whitespace()
                let key = try text()
                guard result[key] == nil else { throw FTError.malformed("json.duplicate_key") }
                whitespace()
                guard take(58) else { throw FTError.malformed("json.colon") }
                result[key] = try value(depth: depth + 1)
                whitespace()
                if take(125) { return .object(result) }
                guard take(44) else { throw FTError.malformed("json.object_separator") }
            }
        }
        if take(91) {
            var result: [JSONValue] = []
            whitespace()
            if take(93) { return .array(result) }
            while true {
                result.append(try value(depth: depth + 1))
                whitespace()
                if take(93) { return .array(result) }
                guard take(44) else { throw FTError.malformed("json.array_separator") }
            }
        }
        for (literal, result) in [("true", JSONValue.boolean(true)), ("false", .boolean(false)), ("null", .null)] {
            let token = Array(literal.utf8)
            if bytes[offset...].starts(with: token) { offset += token.count; return result }
        }
        let start = offset
        _ = take(45)
        if !take(48) {
            guard offset < bytes.count, (49...57).contains(bytes[offset]) else { throw FTError.malformed("json.number") }
            while offset < bytes.count, (48...57).contains(bytes[offset]) { offset += 1 }
        }
        if take(46) { try digits() }
        if take(101) || take(69) {
            if !take(43) { _ = take(45) }
            try digits()
        }
        guard offset - start <= 64 else { throw FTError.limit("json.number") }
        return .number(String(decoding: bytes[start..<offset], as: UTF8.self))
    }
    private mutating func digits() throws {
        let start = offset
        while offset < bytes.count, (48...57).contains(bytes[offset]) { offset += 1 }
        guard start != offset else { throw FTError.malformed("json.digits") }
    }
}

extension Dictionary where Key == String, Value == JSONValue {
    func allowing(_ keys: Set<String>) throws {
        guard Set(self.keys).isSubset(of: keys.union(["extensions"])) else {
            throw FTError.malformed("manifest.unexpected_field")
        }
    }
    func required(_ key: String) throws -> JSONValue {
        guard let value = self[key] else { throw FTError.malformed("manifest.missing_field") }
        return value
    }
    func text(_ key: String) throws -> String { try required(key).string() }
    func flag(_ key: String, default fallback: Bool = false) throws -> Bool {
        guard let field = self[key] else { return fallback }
        guard case .boolean(let result) = field else { throw FTError.malformed("manifest.boolean") }
        return result
    }
}

public struct WorkspaceDocument: Sendable {
    public enum AuthenticationMethod: String, CaseIterable, Hashable, Sendable {
        case publicKey = "publickey", keyboardInteractive = "keyboard-interactive", password
    }
    public struct Diagnostic: Equatable, Sendable {
        public let code: String
        public let count: Int
    }
    public struct Group: Identifiable, Sendable {
        public let id: String, name: String
        public let machineOrder: [String]
        public let autoConnect: Bool
    }
    public struct Machine: Identifiable, Sendable {
        public let id: String, name: String, themeID: String, preferredRouteID: String
        public let shortLabel: String?, paneSelection: String?
        public let routeIDs: [String]
        public let autoConnect: Bool
        public let tags: [String]
    }
    public struct Route: Identifiable, Sendable {
        public let id: String, mode: String, host: String, user: String
        public let displayHost: String
        public let port: UInt16
        public let credentialIDs: [String], trustID: String
        public let authenticationMethods: [AuthenticationMethod]
        public let jumpRoutes: [String]
        public let sessionCreation: String?, attachPolicy: String?, hostKeyAlias: String?
        public let requiredCapabilities: [String], requiredExtensions: [String]
    }
    public struct Credential: Identifiable, Sendable {
        public let id: String, algorithm: String, fingerprint: String
        public let requiredCapabilities: [String], requiredExtensions: [String]
    }
    public struct Font: Sendable {
        public let family: String, face: String?
        public let pointSize: Double?
        public let fallbacks: [String], variations: [String: Double]
    }
    public struct Theme: Identifiable, Sendable {
        public let id: String, name: String, foreground: String, background: String, accent: String
        public let cursorForeground: String, cursorBackground: String, cursorBorder: String
        public let selectionForeground: String, selectionBackground: String
        public let ansi: [String]
        /// Entries 0...15 belong to ansi; unspecified 16...255 use the v1
        /// xterm 6×6×6 color cube followed by 24 grayscale entries.
        public let indexed: [UInt8: String]
        public let font: Font?
        public func indexedColor(_ index: UInt8) -> String {
            if index < 16 { return ansi[Int(index)] }
            if let custom = indexed[index] { return custom }
            if index >= 232 {
                let component = Int(index - 232) * 10 + 8
                return String(format: "#%02X%02X%02X", component, component, component)
            }
            let value = Int(index - 16), levels = [0, 95, 135, 175, 215, 255]
            return String(format: "#%02X%02X%02X", levels[value / 36], levels[(value / 6) % 6], levels[value % 6])
        }
    }
    public struct HostKey: Hashable, Codable, Sendable {
        public let algorithm: String, fingerprint: String
        public init(algorithm: String, fingerprint: String) throws {
            self.algorithm = try WorkspaceDocument.algorithm(algorithm)
            self.fingerprint = try WorkspaceDocument.fingerprint(fingerprint)
        }
        private enum CodingKeys: CodingKey { case algorithm, fingerprint }
        public init(from decoder: Decoder) throws {
            let fields = try decoder.container(keyedBy: CodingKeys.self)
            try self.init(algorithm: fields.decode(String.self, forKey: .algorithm),
                          fingerprint: fields.decode(String.self, forKey: .fingerprint))
        }
    }
    public enum HostTrustDecision: Equatable, Sendable {
        case trusted, reviewRequired, unconfigured, mismatch, revoked, requiredPolicyUnavailable
    }
    public struct HostTrust: Identifiable, Sendable {
        public let id: String, host: String, displayHost: String
        public let port: UInt16
        public let hostKeyAlias: String?, claimedSource: String?
        public let pins: [HostKey], revokedKeys: [HostKey]
        public let requiredCapabilities: [String], requiredExtensions: [String]
        /// Reserved markers preserve mandatory unknown extensions across import.
        /// No v1 implementation may satisfy them merely by listing a capability.
        public var requiredPolicyRequirements: Set<String> {
            Set(requiredCapabilities).union(requiredExtensions.map { "extension:" + $0 })
        }
        /// OpenSSH HostKeyAlias replaces the hostname for known-host matching.
        /// The length framing avoids collisions with ports and IPv6 colons.
        public var canonicalTrustScope: String {
            let identity = hostKeyAlias ?? host
            return "\(port):\(identity.utf8.count):\(identity)"
        }
        public func evaluate(_ presented: HostKey, locallyAccepted: Bool,
                             additionalRevocations: Set<HostKey> = [],
                             supportedCapabilities: Set<String> = []) -> HostTrustDecision {
            if revokedKeys.contains(where: { $0.fingerprint == presented.fingerprint })
                || additionalRevocations.contains(where: { $0.fingerprint == presented.fingerprint }) { return .revoked }
            if !WorkspaceDocument.supports(requiredPolicyRequirements, using: supportedCapabilities) { return .requiredPolicyUnavailable }
            if pins.isEmpty { return .unconfigured }
            if !pins.contains(presented) { return .mismatch }
            return locallyAccepted ? .trusted : .reviewRequired
        }
    }
    public enum ReadinessCode: String, Hashable, Sendable {
        case requiredCapabilityUnavailable, hostTrustReviewRequired, hostTrustMissing
        case hostIdentityRevoked, hostKeyAlgorithmUnsupported, authenticationMethodUnsupported
        case credentialRequired, credentialAlgorithmUnsupported
    }
    public struct ReadinessIssue: Hashable, Sendable {
        public let routeID: String
        public let code: ReadinessCode
    }
    public struct RouteReadinessDetail: Sendable {
        public let routeID: String
        public let usableCredentialIDs: [String]
        public let authenticationMethods: [AuthenticationMethod]
        public let issues: [ReadinessIssue]
        /// Only configuration eligibility. A fresh connection must still verify
        /// the actual host and complete authentication under this method policy.
        public var canAttemptConnection: Bool { issues.isEmpty }
    }
    /// A transport integration supplies its actually qualified profile. Parsing
    /// a private container alone never claims native SSH or certificate support.
    public struct RouteSupport: Sendable {
        public let routeModes: Set<String>, authenticationMethods: Set<AuthenticationMethod>
        public let credentialAlgorithms: Set<String>, hostKeyAlgorithms: Set<String>, capabilities: Set<String>
        public init(routeModes: Set<String>, authenticationMethods: Set<AuthenticationMethod>,
                    credentialAlgorithms: Set<String>, hostKeyAlgorithms: Set<String>, capabilities: Set<String> = []) {
            self.routeModes = routeModes; self.authenticationMethods = authenticationMethods
            self.credentialAlgorithms = credentialAlgorithms; self.hostKeyAlgorithms = hostKeyAlgorithms; self.capabilities = capabilities
        }
        public static let offlineOnly = RouteSupport(routeModes: [], authenticationMethods: [], credentialAlgorithms: [], hostKeyAlgorithms: [])
    }
    public enum RouteReadinessIssue: Equatable, Sendable {
        case transportUnavailable, unsupportedRequirement, authenticationUnavailable
        case credentialUnavailable, credentialAlgorithmUnavailable, hostTrustMissing, hostTrustNeedsReview, hostIdentityRevoked
        case hostAlgorithmUnavailable, locallyRejectedTrust, sessionPolicyMissing, blockedJumpRoute(String)
        public var message: String {
            switch self {
            case .transportUnavailable: "The terminal transport for this route is not integrated in this build."
            case .unsupportedRequirement: "A required security policy or extension is not supported by this build."
            case .authenticationUnavailable: "None of the permitted authentication methods is supported by this build."
            case .credentialUnavailable: "No permitted matching credential is available for this route."
            case .credentialAlgorithmUnavailable: "The available credential needs an SSH key algorithm not supported by this build."
            case .hostTrustMissing: "This route needs a verified host identity."
            case .hostTrustNeedsReview: "Review the imported host identity before authentication."
            case .hostIdentityRevoked: "All imported host identities for this route are revoked."
            case .hostAlgorithmUnavailable: "No accepted host-key algorithm is supported by this build."
            case .locallyRejectedTrust: "A local host-trust rejection blocks this route."
            case .sessionPolicyMissing: "This route needs an explicit session-creation or existing-mux attachment policy."
            case .blockedJumpRoute: "A required jump route is not locally ready."
            }
        }
    }
    public struct RouteReadiness: Sendable {
        public let routeID: String, usableCredentialIDs: [String], issues: [RouteReadinessIssue]
        /// Local eligibility never proves reachability or successful authentication.
        public var isLocallyReady: Bool { issues.isEmpty }
    }
    public enum HostVerificationDecision: Equatable, Sendable {
        case trustedPin, revoked, locallyRejected, reviewRequired, pinMismatch
        case unsupportedPolicy, unsupportedAlgorithm, unsupportedCertificate
    }
    /// Public, validated configuration bytes. Unknown optional payloads are not
    /// persisted or re-exported. Fully understood manifests retain exact bytes.
    public let raw: Data
    public let exporterID: String, id: String, bundleID: String, name: String
    public let groups: [Group], machines: [Machine], routes: [Route], credentials: [Credential], themes: [Theme]
    public let hostTrust: [HostTrust]
    public let diagnostics: [Diagnostic]
    public let initialMachineID: String
    public let initialGroupID: String
    public let connectOnOpen: Bool
    public let sourceKind: String?, configurationRevision: String?, exportSequence: UInt64?
    public let discardedExtensionCount: Int

    public var entityIDs: Set<String> {
        Set([id] + groups.map(\.id) + machines.map(\.id) + routes.map(\.id)
            + credentials.map(\.id) + themes.map(\.id) + hostTrust.map(\.id))
    }

    /// A newly prepared immutable export gets a fresh bundle identity, without
    /// changing workspace or entity identity or executing any imported policy.
    public func reissuingBundle(id: String) throws -> WorkspaceDocument {
        var root = try StrictJSON.decode(raw).object()
        root["bundle_id"] = .string(try Self.identifier(id))
        return try WorkspaceDocument(data: JSONValue.object(root).encoded())
    }

    /// Resolve all records for the effective trust scope before evaluating a
    /// live key. A second route/trust alias cannot undo a same-scope revocation.
    public func evaluateHostKey(_ presented: HostKey, trustID: String, locallyAccepted: Bool,
                                revokedHostKeysByTrustScope: [String: Set<HostKey>] = [:],
                                supportedCapabilities: Set<String> = [],
                                requiredHostCapabilitiesByTrustScope: [String: Set<String>] = [:]) throws -> HostTrustDecision {
        guard let selected = hostTrust.first(where: { $0.id == trustID }) else { throw FTError.malformed("trust.reference") }
        let related = hostTrust.filter { $0.canonicalTrustScope == selected.canonicalTrustScope }
        let revoked = Set(related.flatMap(\.revokedKeys)).union(revokedHostKeysByTrustScope[selected.canonicalTrustScope] ?? [])
        if revoked.contains(where: { $0.fingerprint == presented.fingerprint }) { return .revoked }
        let required = Set(related.flatMap(\.requiredPolicyRequirements))
            .union(requiredHostCapabilitiesByTrustScope[selected.canonicalTrustScope] ?? [])
        if !Self.supports(required, using: supportedCapabilities) { return .requiredPolicyUnavailable }
        return selected.evaluate(presented, locallyAccepted: locallyAccepted, additionalRevocations: revoked,
                                 supportedCapabilities: supportedCapabilities)
    }

    public init(data: Data) throws {
        var discarded = 0
        let sanitized = try Self.discardExtensions(StrictJSON.decode(data), discarded: &discarded)
        let root = try sanitized.object()
        try root.allowing(["format", "version", "bundle_id", "source", "required_capabilities", "workspace"])
        guard try root.text("format") == "ftworkspace", try root.required("version").uint() == 1 else {
            throw FTError.unsupported("workspace.version")
        }
        let required = try Self.identifiers(root.required("required_capabilities"), cap: 64)
        guard required.isEmpty else { throw FTError.unsupported("workspace.required_capabilities") }
        let source = try root.required("source").object()
        try source.allowing(["exporter_id", "kind", "configuration_revision", "export_sequence"])
        exporterID = try Self.identifier(source.text("exporter_id"))
        sourceKind = try source["kind"].map { try Self.identifier($0.string()) }
        configurationRevision = try source["configuration_revision"].map { try Self.displayText($0.string(), cap: 256) }
        exportSequence = try source["export_sequence"]?.uint()
        bundleID = try Self.identifier(root.text("bundle_id"))
        let workspace = try root.required("workspace").object()
        try workspace.allowing(["id", "name", "groups", "machines", "routes", "credentials", "host_trust", "themes", "preferences"])
        id = try Self.identifier(workspace.text("id"))
        name = try Self.displayText(workspace.text("name"), cap: 256)
        func records(_ key: String, cap: Int) throws -> [[String: JSONValue]] {
            let values = try workspace.required(key).array()
            guard values.count <= cap else { throw FTError.limit("manifest.records") }
            let objects = try values.map { try $0.object() }
            let ids = try objects.map { try Self.identifier($0.text("id")) }
            guard Set(ids).count == ids.count else { throw FTError.malformed("manifest.duplicate_id") }
            return objects
        }
        credentials = try records("credentials", cap: 256).map {
            try $0.allowing(["id", "algorithm", "public_fingerprint", "requirement", "private_material", "required_capabilities"])
            let fp = try Self.fingerprint($0.text("public_fingerprint"))
            guard try $0.text("private_material") == "not_included" else { throw FTError.malformed("workspace.private_material") }
            if let requirement = $0["requirement"], try requirement.string() != "matching_private_key" {
                throw FTError.unsupported("credential.requirement")
            }
            return Credential(id: try $0.text("id"), algorithm: try Self.algorithm($0.text("algorithm")), fingerprint: fp,
                              requiredCapabilities: try Self.capabilities($0), requiredExtensions: try Self.requiredExtensions($0))
        }
        themes = try records("themes", cap: 128).map {
            try $0.allowing(["id", "name", "foreground", "background", "accent", "ansi", "cursor_foreground", "cursor_background",
                             "cursor_border", "selection_foreground", "selection_background", "indexed", "font"])
            let ansi = try $0.required("ansi").array().map { try Self.color($0.string()) }
            guard ansi.count == 16 else { throw FTError.malformed("theme.ansi") }
            var overrides: [UInt8: String] = [:]
            if let value = $0["indexed"] {
                let fields = try value.object()
                guard fields.count <= 240 else { throw FTError.limit("theme.indexed") }
                for (key, value) in fields {
                    guard let index = UInt8(key), index >= 16, String(index) == key else { throw FTError.malformed("theme.index") }
                    overrides[index] = try Self.color(value.string())
                }
            }
            return Theme(id: try $0.text("id"), name: try Self.displayText($0.text("name"), cap: 256), foreground: try Self.color($0.text("foreground")),
                         background: try Self.color($0.text("background")), accent: try Self.color($0.text("accent")),
                         cursorForeground: try Self.color($0.text("cursor_foreground")), cursorBackground: try Self.color($0.text("cursor_background")),
                         cursorBorder: try Self.color($0.text("cursor_border")), selectionForeground: try Self.color($0.text("selection_foreground")),
                         selectionBackground: try Self.color($0.text("selection_background")), ansi: ansi, indexed: overrides,
                         font: try $0["font"].map(Self.font))
        }
        var pinCount = 0
        hostTrust = try records("host_trust", cap: 512).map { trust in
            try trust.allowing(["id", "host", "port", "host_key_alias", "claimed_source", "pins", "revoked_keys", "required_capabilities"])
            func keys(_ field: String, optional: Bool = false) throws -> [HostKey] {
                let value: JSONValue
                if let present = trust[field] { value = present }
                else if optional { value = .array([]) }
                else { throw FTError.malformed("manifest.missing_field") }
                let values = try value.array()
                pinCount += values.count
                guard pinCount <= 512 else { throw FTError.limit("trust.keys") }
                let result = try values.map { value -> HostKey in
                    let fields = try value.object()
                    try fields.allowing(["algorithm", "fingerprint"])
                    return try HostKey(algorithm: fields.text("algorithm"), fingerprint: fields.text("fingerprint"))
                }
                guard Set(result).count == result.count else { throw FTError.malformed("trust.duplicate_key") }
                return result
            }
            let originalHost = try trust.text("host")
            return HostTrust(id: try trust.text("id"), host: try Self.host(originalHost), displayHost: originalHost,
                             port: try Self.port(trust.required("port").uint()),
                             hostKeyAlias: try trust["host_key_alias"].map { try Self.host($0.string()) },
                             claimedSource: try trust["claimed_source"].map { try Self.displayText($0.string(), cap: 128) },
                             pins: try keys("pins"), revokedKeys: try keys("revoked_keys", optional: true),
                             requiredCapabilities: try Self.capabilities(trust), requiredExtensions: try Self.requiredExtensions(trust))
        }
        routes = try records("routes", cap: 256).map {
            try $0.allowing(["id", "mode", "host", "port", "user", "credential_refs", "authentication_methods", "host_trust_ref",
                             "jump_routes", "session_creation", "attach_policy", "host_key_alias", "required_capabilities"])
            let mode = try $0.text("mode")
            // Via Mac has a different authentication schema; never reinterpret it as SSH.
            guard ["direct_ssh", "frankenterm_mux"].contains(mode) else { throw FTError.unsupported("route.mode") }
            let jumps = try Self.identifiers($0.required("jump_routes"), cap: 3)
            let candidates = try Self.identifiers($0.required("credential_refs"), cap: 8)
            let methods = try Self.identifiers($0.required("authentication_methods"), cap: 3).map { value -> AuthenticationMethod in
                guard let method = AuthenticationMethod(rawValue: value) else { throw FTError.unsupported("route.authentication_method") }
                return method
            }
            guard !methods.isEmpty else { throw FTError.malformed("route.authentication_policy") }
            let creation = try $0["session_creation"]?.string(), attach = try $0["attach_policy"]?.string()
            if mode == "direct_ssh" {
                guard attach == nil, creation.map({ ["on_select", "manual"].contains($0) }) ?? true else { throw FTError.unsupported("route.session_creation") }
            } else {
                guard creation == nil, attach == nil || attach == "existing_only" else { throw FTError.unsupported("route.attach_policy") }
            }
            let originalHost = try $0.text("host")
            let user = try Self.displayText($0.text("user"), cap: 256)
            guard !user.unicodeScalars.contains(where: { CharacterSet.whitespacesAndNewlines.contains($0) }) else { throw FTError.malformed("route.user") }
            return Route(id: try $0.text("id"), mode: mode, host: try Self.host(originalHost), user: user,
                         displayHost: originalHost, port: try Self.port($0.required("port").uint()), credentialIDs: candidates,
                         trustID: try Self.identifier($0.text("host_trust_ref")), authenticationMethods: methods, jumpRoutes: jumps,
                         sessionCreation: creation, attachPolicy: attach,
                         hostKeyAlias: try $0["host_key_alias"].map { try Self.host($0.string()) },
                         requiredCapabilities: try Self.capabilities($0), requiredExtensions: try Self.requiredExtensions($0))
        }
        let routeMap = Dictionary(uniqueKeysWithValues: routes.map { ($0.id, $0) })
        let credentialIDs = Set(credentials.map(\.id))
        let trustMap = Dictionary(uniqueKeysWithValues: hostTrust.map { ($0.id, $0) })
        for route in routes {
            guard route.credentialIDs.allSatisfy(credentialIDs.contains), let trust = trustMap[route.trustID],
                  route.host == trust.host, route.port == trust.port, route.hostKeyAlias == trust.hostKeyAlias
            else { throw FTError.malformed("route.credential_or_trust") }
            var expanded = 0
            func walk(_ candidate: Route, path: Set<String>) throws {
                for next in candidate.jumpRoutes {
                    expanded += 1
                    guard expanded <= 3, !path.contains(next), let hop = routeMap[next] else { throw FTError.malformed("route.jump_graph") }
                    try walk(hop, path: path.union([next]))
                }
            }
            try walk(route, path: [route.id])
        }
        let themeIDs = Set(themes.map(\.id))
        machines = try records("machines", cap: 128).map {
            try $0.allowing(["id", "name", "short_label", "theme_ref", "preferred_route_ref", "route_refs", "auto_connect", "pane_selection", "tags"])
            let themeID = try $0.text("theme_ref"), preferred = try $0.text("preferred_route_ref")
            let ids = try Self.identifiers($0.required("route_refs"), cap: 256)
            guard !ids.isEmpty, ids.contains(preferred),
                  ids.allSatisfy({ routeMap[$0] != nil }), themeIDs.contains(themeID) else { throw FTError.malformed("machine.references") }
            if let selection = $0["pane_selection"], try selection.string() != "last_surviving" { throw FTError.unsupported("machine.pane_selection") }
            let tags = try ($0["tags"] ?? .array([])).array()
            guard tags.count <= 64 else { throw FTError.limit("machine.tags") }
            return Machine(id: try $0.text("id"), name: try Self.displayText($0.text("name"), cap: 256), themeID: themeID,
                           preferredRouteID: preferred, shortLabel: try $0["short_label"].map { try Self.displayText($0.string(), cap: 32) },
                           paneSelection: try $0["pane_selection"]?.string(),
                           routeIDs: ids, autoConnect: try $0.flag("auto_connect"), tags: try tags.map { try Self.displayText($0.string(), cap: 128) })
        }
        let machineIDs = Set(machines.map(\.id))
        groups = try records("groups", cap: 64).map {
            try $0.allowing(["id", "name", "machine_order", "auto_connect"])
            let ids = try Self.identifiers($0.required("machine_order"), cap: 128)
            guard ids.allSatisfy(machineIDs.contains) else { throw FTError.malformed("group.references") }
            return Group(id: try $0.text("id"), name: try Self.displayText($0.text("name"), cap: 256), machineOrder: ids, autoConnect: try $0.flag("auto_connect"))
        }
        let preferences = try workspace.required("preferences").object()
        try preferences.allowing(["initial_machine_ref", "initial_group_ref", "connect_on_open"])
        initialMachineID = try preferences.text("initial_machine_ref")
        let selectedGroupID = try preferences.text("initial_group_ref")
        guard let group = groups.first(where: { $0.id == selectedGroupID }), group.machineOrder.contains(initialMachineID) else {
            throw FTError.malformed("workspace.initial_selection")
        }
        initialGroupID = selectedGroupID
        connectOnOpen = try preferences.flag("connect_on_open")
        let entityIDs = [id] + groups.map(\.id) + machines.map(\.id) + routes.map(\.id)
            + credentials.map(\.id) + themes.map(\.id) + hostTrust.map(\.id)
        guard Set(entityIDs).count == entityIDs.count else { throw FTError.malformed("manifest.duplicate_id") }
        var algorithmsByFingerprint: [String: String] = [:]
        // Positive identities must describe the same public key consistently.
        // A negative fingerprint remains a denial even when its self-asserted
        // algorithm label differs from that of the positive key it forbids.
        let publicIdentities = credentials.map { ($0.algorithm, $0.fingerprint) }
            + hostTrust.flatMap(\.pins).map { ($0.algorithm, $0.fingerprint) }
        for (algorithm, fingerprint) in publicIdentities {
            if let previous = algorithmsByFingerprint[fingerprint], previous != algorithm {
                throw FTError.malformed("identity.algorithm_conflict")
            }
            algorithmsByFingerprint[fingerprint] = algorithm
        }
        diagnostics = discarded == 0 ? [] : [.init(code: "workspace.unknown_optional_extensions_discarded", count: discarded)]
        discardedExtensionCount = discarded
        raw = discarded == 0 ? data : try sanitized.encoded()
    }

    public func readiness(forRouteID id: String, availableCredentialIDs: Set<String>, reviewedTrustIDs: Set<String> = [],
                          revokedCredentialFingerprints: Set<String> = [], locallyRejectedTrustIDs: Set<String> = [],
                          locallyRevokedHostKeys: [String: Set<HostKey>] = [:],
                          support: RouteSupport = .offlineOnly,
                          revokedHostKeysByTrustScope: [String: Set<HostKey>] = [:],
                          requiredHostCapabilitiesByTrustScope: [String: Set<String>] = [:]) throws -> RouteReadiness {
        let routeMap = Dictionary(uniqueKeysWithValues: routes.map { ($0.id, $0) })
        let trustMap = Dictionary(uniqueKeysWithValues: hostTrust.map { ($0.id, $0) })
        let credentialMap = Dictionary(uniqueKeysWithValues: credentials.map { ($0.id, $0) })
        var revocations = revokedHostKeysByTrustScope, requirements = requiredHostCapabilitiesByTrustScope
        var rejectedScopes = Set<String>()
        for trust in hostTrust {
            revocations[trust.canonicalTrustScope, default: []].formUnion(trust.revokedKeys)
            revocations[trust.canonicalTrustScope, default: []].formUnion(locallyRevokedHostKeys[trust.id] ?? [])
            requirements[trust.canonicalTrustScope, default: []].formUnion(trust.requiredPolicyRequirements)
            if locallyRejectedTrustIDs.contains(trust.id) { rejectedScopes.insert(trust.canonicalTrustScope) }
        }
        func evaluate(_ routeID: String, transit: Bool) throws -> RouteReadiness {
            guard let route = routeMap[routeID], let trust = trustMap[route.trustID] else { throw FTError.stale }
            var issues: [RouteReadinessIssue] = []
            func add(_ issue: RouteReadinessIssue) { if !issues.contains(issue) { issues.append(issue) } }
            // Transit supplies only SSH, never the hop's shell or mux preference.
            if !support.routeModes.contains(transit ? "direct_ssh" : route.mode) { add(.transportUnavailable) }
            let required = Set(route.requiredCapabilities).union(requirements[trust.canonicalTrustScope] ?? [])
            if !Self.supports(required, using: support.capabilities) || !route.requiredExtensions.isEmpty { add(.unsupportedRequirement) }
            if !transit, (route.mode == "direct_ssh" && route.sessionCreation == nil)
                || (route.mode == "frankenterm_mux" && route.attachPolicy == nil) { add(.sessionPolicyMissing) }
            let methods = Set(route.authenticationMethods).intersection(support.authenticationMethods)
            if methods.isEmpty { add(.authenticationUnavailable) }
            let candidates = route.credentialIDs.compactMap { credentialMap[$0] }.filter {
                availableCredentialIDs.contains($0.id) && !revokedCredentialFingerprints.contains($0.fingerprint)
            }
            let policyCandidates = candidates.filter {
                Self.supports(Set($0.requiredCapabilities), using: support.capabilities) && $0.requiredExtensions.isEmpty
            }
            let usable = policyCandidates.filter { support.credentialAlgorithms.contains($0.algorithm) }.map(\.id)
            let alternateMethod = methods.contains(.password) || methods.contains(.keyboardInteractive)
            if route.authenticationMethods.contains(.publicKey), !alternateMethod,
               (methods.contains(.publicKey) || route.authenticationMethods == [.publicKey]) {
                if candidates.isEmpty { add(.credentialUnavailable) }
                else if policyCandidates.isEmpty { add(.unsupportedRequirement) }
                else if usable.isEmpty, !methods.isEmpty { add(.credentialAlgorithmUnavailable) }
            }
            if rejectedScopes.contains(trust.canonicalTrustScope) { add(.locallyRejectedTrust) }
            let revoked = Set((revocations[trust.canonicalTrustScope] ?? []).map(\.fingerprint))
            let acceptablePins = trust.pins.filter { !revoked.contains($0.fingerprint) }
            if trust.pins.isEmpty { add(.hostTrustMissing) }
            else if acceptablePins.isEmpty { add(.hostIdentityRevoked) }
            else {
                if !reviewedTrustIDs.contains(trust.id) { add(.hostTrustNeedsReview) }
                if !acceptablePins.contains(where: { support.hostKeyAlgorithms.contains($0.algorithm) }) { add(.hostAlgorithmUnavailable) }
            }
            for hop in route.jumpRoutes where !(try evaluate(hop, transit: true)).isLocallyReady { add(.blockedJumpRoute(hop)) }
            return .init(routeID: routeID, usableCredentialIDs: methods.contains(.publicKey) ? usable : [], issues: issues)
        }
        return try evaluate(id, transit: false)
    }

    /// Each actual SSH peer and jump host must be verified before credentials
    /// are offered. Negative trust wins even for an unsupported certificate.
    public func verifyHostKey(forRouteID id: String, presented: HostKey, certificateAuthority: HostKey? = nil,
                              reviewedTrustIDs: Set<String>, localRevokedFingerprints: Set<String> = [],
                              locallyRejectedTrustIDs: Set<String> = [],
                              locallyRevokedHostKeys: [String: Set<HostKey>] = [:], support: RouteSupport = .offlineOnly,
                              revokedHostKeysByTrustScope: [String: Set<HostKey>] = [:],
                              requiredHostCapabilitiesByTrustScope: [String: Set<String>] = [:]) throws -> HostVerificationDecision {
        guard let route = routes.first(where: { $0.id == id }), let trust = hostTrust.first(where: { $0.id == route.trustID }) else { throw FTError.stale }
        let related = hostTrust.filter { $0.canonicalTrustScope == trust.canonicalTrustScope }
        let revoked = localRevokedFingerprints.union(related.flatMap(\.revokedKeys).map(\.fingerprint))
            .union((revokedHostKeysByTrustScope[trust.canonicalTrustScope] ?? []).map(\.fingerprint))
            .union(related.flatMap { locallyRevokedHostKeys[$0.id] ?? [] }.map(\.fingerprint))
        if revoked.contains(presented.fingerprint) || certificateAuthority.map({ revoked.contains($0.fingerprint) }) == true { return .revoked }
        if related.contains(where: { locallyRejectedTrustIDs.contains($0.id) }) { return .locallyRejected }
        if certificateAuthority != nil || presented.algorithm.contains("-cert-v01@") { return .unsupportedCertificate }
        let required = Set(route.requiredCapabilities).union(related.flatMap(\.requiredPolicyRequirements))
            .union(requiredHostCapabilitiesByTrustScope[trust.canonicalTrustScope] ?? [])
        if !Self.supports(required, using: support.capabilities) || !route.requiredExtensions.isEmpty { return .unsupportedPolicy }
        if !support.hostKeyAlgorithms.contains(presented.algorithm) { return .unsupportedAlgorithm }
        if !reviewedTrustIDs.contains(trust.id) { return .reviewRequired }
        return trust.pins.contains(presented) ? .trustedPin : .pinMismatch
    }

    /// Evaluate the selected route and every expanded jump under explicit local
    /// qualification and review. Importing configuration never supplies these
    /// local authorizations. A transit hop's session-creation preference is inert.
    public func readiness(for routeID: String, availableCredentialIDs: Set<String>,
                          acceptedTrustIDs: Set<String>,
                          supportedAuthenticationMethods: Set<AuthenticationMethod>,
                          supportedCredentialAlgorithms: Set<String>,
                          supportedHostKeyAlgorithms: Set<String>,
                          supportedCapabilities: Set<String> = [],
                          revokedCredentialFingerprints: Set<String> = [],
                          revokedHostKeysByTrustScope: [String: Set<HostKey>] = [:],
                          requiredHostCapabilitiesByTrustScope: [String: Set<String>] = [:]) throws -> RouteReadinessDetail {
        let routeMap = Dictionary(uniqueKeysWithValues: routes.map { ($0.id, $0) })
        let trustMap = Dictionary(uniqueKeysWithValues: hostTrust.map { ($0.id, $0) })
        let credentialMap = Dictionary(uniqueKeysWithValues: credentials.map { ($0.id, $0) })
        var revocations = revokedHostKeysByTrustScope, trustRequirements = requiredHostCapabilitiesByTrustScope
        for trust in hostTrust {
            revocations[trust.canonicalTrustScope, default: []].formUnion(trust.revokedKeys)
            trustRequirements[trust.canonicalTrustScope, default: []].formUnion(trust.requiredPolicyRequirements)
        }
        guard let selected = routeMap[routeID] else { throw FTError.malformed("route.reference") }
        var issues: [ReadinessIssue] = [], visited = Set<String>()
        var selectedKeys: [String] = [], selectedMethods: [AuthenticationMethod] = []
        func inspect(_ route: Route) {
            guard visited.insert(route.id).inserted else { return }
            func issue(_ code: ReadinessCode) { issues.append(.init(routeID: route.id, code: code)) }
            if !Self.supports(Set(route.requiredCapabilities), using: supportedCapabilities) || !route.requiredExtensions.isEmpty { issue(.requiredCapabilityUnavailable) }
            if let trust = trustMap[route.trustID] {
                if !Self.supports(trustRequirements[trust.canonicalTrustScope] ?? [], using: supportedCapabilities) { issue(.requiredCapabilityUnavailable) }
                if !acceptedTrustIDs.contains(trust.id) { issue(.hostTrustReviewRequired) }
                let revoked = Set((revocations[trust.canonicalTrustScope] ?? []).map(\.fingerprint))
                let allowed = trust.pins.filter { !revoked.contains($0.fingerprint) }
                if trust.pins.isEmpty { issue(.hostTrustMissing) }
                else if allowed.isEmpty { issue(.hostIdentityRevoked) }
                else if !allowed.contains(where: { supportedHostKeyAlgorithms.contains($0.algorithm) }) { issue(.hostKeyAlgorithmUnsupported) }
            } else { issue(.hostTrustMissing) }

            let keys = route.credentialIDs.filter { id in
                guard availableCredentialIDs.contains(id), let key = credentialMap[id] else { return false }
                return supportedCredentialAlgorithms.contains(key.algorithm) && !revokedCredentialFingerprints.contains(key.fingerprint)
                    && Self.supports(Set(key.requiredCapabilities), using: supportedCapabilities) && key.requiredExtensions.isEmpty
            }
            let qualifiedMethods = route.authenticationMethods.filter(supportedAuthenticationMethods.contains)
            let usableMethods = qualifiedMethods.filter { $0 != .publicKey || !keys.isEmpty }
            if qualifiedMethods.isEmpty { issue(.authenticationMethodUnsupported) }
            else if usableMethods.isEmpty {
                let existing = route.credentialIDs.compactMap { id -> Credential? in
                    guard availableCredentialIDs.contains(id) else { return nil }
                    return credentialMap[id]
                }
                if !existing.isEmpty, existing.allSatisfy({ !supportedCredentialAlgorithms.contains($0.algorithm) }) {
                    issue(.credentialAlgorithmUnsupported)
                } else { issue(.credentialRequired) }
            }
            if route.id == routeID { selectedKeys = keys; selectedMethods = usableMethods }
            for hopID in route.jumpRoutes { if let hop = routeMap[hopID] { inspect(hop) } }
        }
        inspect(selected)
        return RouteReadinessDetail(routeID: routeID, usableCredentialIDs: selectedKeys,
                              authenticationMethods: selectedMethods, issues: issues)
    }

    private static func supports(_ requirements: Set<String>, using supported: Set<String>) -> Bool {
        !requirements.contains(where: { $0.hasPrefix("extension:") }) && requirements.isSubset(of: supported)
    }
    private static func capabilities(_ fields: [String: JSONValue]) throws -> [String] {
        try identifiers(fields["required_capabilities"] ?? .array([]), cap: 64)
    }
    private static func requiredExtensions(_ fields: [String: JSONValue]) throws -> [String] {
        guard let value = fields["extensions"] else { return [] }
        return try value.object().keys.sorted()
    }

    /// V1 registers no extension evaluator. Optional payloads are discarded;
    /// a required marker survives only on a route, trust, or credential record
    /// and keeps it unavailable. Required extensions elsewhere reject safely.
    private static func discardExtensions(_ value: JSONValue, path: [String] = [], discarded: inout Int) throws -> JSONValue {
        switch value {
        case .object(var fields):
            if let value = fields["extensions"] {
                let extensions = try value.object()
                guard extensions.count <= 64 else { throw FTError.limit("workspace.extensions") }
                var markers: [String: JSONValue] = [:]
                for (name, payload) in extensions {
                    _ = try identifier(name)
                    let segments = name.split(omittingEmptySubsequences: false, whereSeparator: { $0 == "." || $0 == ":" })
                    guard segments.count >= 2, segments.allSatisfy({ !$0.isEmpty }) else { throw FTError.malformed("workspace.extension_namespace") }
                    let required: Bool
                    if case .object(let attributes) = payload { required = try attributes.flag("required") } else { required = false }
                    if required {
                        guard path.count == 3, path[0] == "workspace", path[2] == "[]",
                              ["routes", "host_trust", "credentials"].contains(path[1]) else { throw FTError.unsupported("workspace.required_extension") }
                        markers[name] = .object(["required": .boolean(true)])
                    }
                    discarded += 1
                    guard discarded <= 256 else { throw FTError.limit("workspace.extensions") }
                }
                fields["extensions"] = markers.isEmpty ? nil : .object(markers)
            }
            for (key, value) in fields where key != "extensions" {
                fields[key] = try discardExtensions(value, path: path + [key], discarded: &discarded)
            }
            return .object(fields)
        case .array(let values): return .array(try values.map { try discardExtensions($0, path: path + ["[]"], discarded: &discarded) })
        default: return value
        }
    }

    private static func font(_ value: JSONValue) throws -> Font {
        let fields = try value.object()
        try fields.allowing(["family", "face", "point_size", "fallbacks", "variations"])
        func number(_ value: JSONValue) throws -> Double {
            guard case .number(let raw) = value, let result = Double(raw), result.isFinite else { throw FTError.malformed("font.number") }
            return result
        }
        let size = try fields["point_size"].map(number)
        if let size { guard (6...96).contains(size) else { throw FTError.limit("font.point_size") } }
        let fallbackValues = try fields["fallbacks"]?.array() ?? []
        guard fallbackValues.count <= 16 else { throw FTError.limit("font.fallbacks") }
        let fallbacks = try fallbackValues.map { try displayText($0.string(), cap: 256) }
        guard Set(fallbacks).count == fallbacks.count else { throw FTError.malformed("font.fallbacks") }
        let variationValues = try fields["variations"]?.object() ?? [:]
        guard variationValues.count <= 16 else { throw FTError.limit("font.variations") }
        var variations: [String: Double] = [:]
        for (tag, value) in variationValues {
            guard tag.utf8.count == 4, tag.utf8.allSatisfy({ (32...126).contains($0) }) else { throw FTError.malformed("font.axis") }
            variations[tag] = try number(value)
        }
        return Font(family: try displayText(fields.text("family"), cap: 256),
                    face: try fields["face"].map { try displayText($0.string(), cap: 256) },
                    pointSize: size, fallbacks: fallbacks, variations: variations)
    }

    private static func identifiers(_ value: JSONValue, cap: Int) throws -> [String] {
        let values = try value.array()
        guard values.count <= cap else { throw FTError.limit("manifest.references") }
        let result = try values.map { try identifier($0.string()) }
        guard Set(result).count == result.count else { throw FTError.malformed("manifest.duplicate_reference") }
        return result
    }

    private static func displayText(_ value: String, cap: Int) throws -> String {
        guard !value.isEmpty, value.utf8.count <= cap,
              !value.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) })
        else { throw FTError.malformed("manifest.display_text") }
        return value
    }

    static func fingerprint(_ value: String) throws -> String {
        guard value.hasPrefix("SHA256:"), value.utf8.count == 50,
              let bytes = Data(base64Encoded: String(value.dropFirst(7)) + "="), bytes.count == 32,
              "SHA256:" + bytes.base64EncodedString().replacingOccurrences(of: "=", with: "") == value
        else { throw FTError.malformed("identity.fingerprint") }
        return value
    }

    static func algorithm(_ value: String) throws -> String {
        guard !value.isEmpty, value.utf8.count <= 128, value.utf8.allSatisfy({
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || [43, 45, 46, 64, 95].contains($0)
        }) else { throw FTError.malformed("identity.algorithm") }
        return value
    }

    static func identifier(_ value: String) throws -> String {
        guard !value.isEmpty, value.utf8.count <= 128, value.utf8.allSatisfy({
            (48...57).contains($0) || (65...90).contains($0) || (97...122).contains($0) || [45, 46, 58, 95].contains($0)
        }) else { throw FTError.malformed("manifest.identifier") }
        return value
    }
    static func color(_ value: String) throws -> String {
        guard [7, 9].contains(value.utf8.count), value.first == "#", value.utf8.dropFirst().allSatisfy({
            (48...57).contains($0) || (65...70).contains($0) || (97...102).contains($0)
        }) else { throw FTError.malformed("theme.color") }
        return value.uppercased()
    }
    static func port(_ value: UInt64) throws -> UInt16 {
        guard value > 0, value <= 65535 else { throw FTError.malformed("route.port") }
        return UInt16(value)
    }
    static func host(_ value: String) throws -> String {
        // This parser performs no name resolution. Original display/export
        // spelling stays in raw; trust comparisons and network targets use the
        // same canonical endpoint, including persistent negative trust.
        guard !value.isEmpty, value.utf8.count <= 1024,
              !value.unicodeScalars.contains(where: { $0.properties.isWhitespace || $0.value < 32 || (127...159).contains($0.value) }),
              value.rangeOfCharacter(from: CharacterSet(charactersIn: "/\\@?#%")) == nil
        else { throw FTError.malformed("route.host") }
        var input = value
        if input.contains("[") || input.contains("]") {
            guard input.first == "[", input.last == "]" else { throw FTError.malformed("route.host") }
            input = String(input.dropFirst().dropLast())
            guard input.contains(":"), !input.contains("["), !input.contains("]") else { throw FTError.malformed("route.host") }
        }
        if input.contains(":") {
            #if canImport(Darwin) || canImport(Glibc)
            // The C string and destination address live only for this call.
            // No pointer escapes these synchronous closures.
            var address = in6_addr()
            guard input.withCString({ inet_pton(AF_INET6, $0, &address) }) == 1 else { throw FTError.malformed("route.ipv6") }
            let bytes = withUnsafeBytes(of: &address) { Array($0) }
            guard bytes.count == 16 else { throw FTError.malformed("route.ipv6") }
            // IPv4-mapped IPv6 reaches the same IPv4 endpoint. It cannot become
            // another identity that bypasses that endpoint's rejected key.
            if bytes.prefix(10).allSatisfy({ $0 == 0 }), bytes[10] == 255, bytes[11] == 255 {
                return bytes.suffix(4).map(String.init).joined(separator: ".")
            }
            // Deterministic RFC 5952 spelling, including first-run tie breaking.
            // Do not depend on platform-specific inet_ntop display conventions.
            let words = stride(from: 0, to: 16, by: 2).map { (UInt16(bytes[$0]) << 8) | UInt16(bytes[$0 + 1]) }
            var bestStart = 0, bestLength = 0, index = 0
            while index < words.count {
                guard words[index] == 0 else { index += 1; continue }
                let start = index
                while index < words.count, words[index] == 0 { index += 1 }
                if index - start > bestLength { bestStart = start; bestLength = index - start }
            }
            let text = words.map { String($0, radix: 16) }
            guard bestLength >= 2 else { return text.joined(separator: ":") }
            return text[..<bestStart].joined(separator: ":") + "::" + text[(bestStart + bestLength)...].joined(separator: ":")
            #else
            throw FTError.unsupported("route.ipv6")
            #endif
        }
        // Foundation's documented IDNA conversion supports internationalized
        // names. Check the whole authority and then the ASCII result; percent
        // fallback, userinfo, ports and path interpretation are never endpoints.
        guard let parsed = URLComponents(string: "https://" + input, encodingInvalidCharacters: true),
              parsed.scheme == "https", parsed.user == nil, parsed.password == nil, parsed.port == nil,
              parsed.path.isEmpty, parsed.query == nil, parsed.fragment == nil,
              var canonical = parsed.encodedHost, !canonical.isEmpty
        else { throw FTError.malformed("route.host") }
        canonical = canonical.lowercased()
        if canonical.last == "." { canonical.removeLast() }
        guard !canonical.isEmpty, canonical.utf8.count <= 253,
              canonical.utf8.allSatisfy({ (48...57).contains($0) || (97...122).contains($0) || $0 == 45 || $0 == 46 })
        else { throw FTError.malformed("route.host") }
        let labels = canonical.split(separator: ".", omittingEmptySubsequences: false)
        guard labels.allSatisfy({ !$0.isEmpty && $0.utf8.count <= 63 && $0.first != "-" && $0.last != "-" }) else {
            throw FTError.malformed("route.host")
        }
        let numeric = labels.allSatisfy { label in
            if label.utf8.allSatisfy({ (48...57).contains($0) }) { return true }
            return label.hasPrefix("0x") && label.count > 2 && label.dropFirst(2).utf8.allSatisfy {
                (48...57).contains($0) || (97...102).contains($0)
            }
        }
        if numeric {
            // Legacy octal, hexadecimal, shortened and integer IPv4 spellings
            // are accepted by some resolvers. Refuse them before networking.
            guard labels.count == 4, labels.allSatisfy({
                $0.utf8.allSatisfy({ (48...57).contains($0) }) && ($0.count == 1 || $0.first != "0") && UInt8($0) != nil
            }) else { throw FTError.malformed("route.ipv4") }
            #if canImport(Darwin) || canImport(Glibc)
            var address = in_addr()
            guard canonical.withCString({ inet_pton(AF_INET, $0, &address) }) == 1 else { throw FTError.malformed("route.ipv4") }
            #endif
        }
        return canonical
    }
}
