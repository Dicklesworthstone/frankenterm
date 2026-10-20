import Foundation

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

/// Retains unknown optional data without reserializing or executing it.
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
    public struct Group: Identifiable, Sendable {
        public let id: String, name: String
        public let machineOrder: [String]
    }
    public struct Machine: Identifiable, Sendable {
        public let id: String, name: String, themeID: String, preferredRouteID: String
        public let routeIDs: [String]
        public let autoConnect: Bool
    }
    public struct Route: Identifiable, Sendable {
        public let id: String, mode: String, host: String, user: String
        public let port: UInt16
        public let credentialID: String, trustID: String
        public let jumpRoutes: [String]
    }
    public struct Credential: Identifiable, Sendable {
        public let id: String, algorithm: String, fingerprint: String
    }
    public struct Theme: Identifiable, Sendable {
        public let id: String, name: String, foreground: String, background: String, accent: String
        public let ansi: [String]
    }
    public let raw: Data
    public let exporterID: String, id: String, bundleID: String, name: String
    public let groups: [Group], machines: [Machine], routes: [Route], credentials: [Credential], themes: [Theme]
    public let initialMachineID: String

    public init(data: Data) throws {
        let root = try StrictJSON.decode(data).object()
        guard try root.text("format") == "ftworkspace", try root.required("version").uint() == 1 else {
            throw FTError.unsupported("workspace.version")
        }
        let required = try root.required("required_capabilities").array()
        guard required.isEmpty else { throw FTError.unsupported("workspace.required_capabilities") }
        let source = try root.required("source").object()
        exporterID = try Self.identifier(source.text("exporter_id"))
        bundleID = try Self.identifier(root.text("bundle_id"))
        let workspace = try root.required("workspace").object()
        id = try Self.identifier(workspace.text("id"))
        name = try workspace.text("name")
        func records(_ key: String, cap: Int) throws -> [[String: JSONValue]] {
            let values = try workspace.required(key).array()
            guard values.count <= cap else { throw FTError.limit("manifest.records") }
            let objects = try values.map { try $0.object() }
            let ids = try objects.map { try Self.identifier($0.text("id")) }
            guard Set(ids).count == ids.count else { throw FTError.malformed("manifest.duplicate_id") }
            return objects
        }
        credentials = try records("credentials", cap: 256).map {
            let fp = try $0.text("public_fingerprint")
            guard fp.hasPrefix("SHA256:"), fp.utf8.count == 50,
                  let digest = Data(base64Encoded: String(fp.dropFirst(7)) + "="), digest.count == 32,
                  "SHA256:" + digest.base64EncodedString().replacingOccurrences(of: "=", with: "") == fp
            else { throw FTError.malformed("credential.fingerprint") }
            guard try $0.text("private_material") == "not_included" else { throw FTError.malformed("workspace.private_material") }
            return Credential(id: try $0.text("id"), algorithm: try $0.text("algorithm"), fingerprint: fp)
        }
        themes = try records("themes", cap: 128).map {
            let ansi = try $0.required("ansi").array().map { try Self.color($0.string()) }
            guard ansi.count == 16 else { throw FTError.malformed("theme.ansi") }
            for key in ["cursor_foreground", "cursor_background", "cursor_border", "selection_foreground", "selection_background"] {
                _ = try Self.color($0.text(key))
            }
            return Theme(id: try $0.text("id"), name: try $0.text("name"), foreground: try Self.color($0.text("foreground")),
                         background: try Self.color($0.text("background")), accent: try Self.color($0.text("accent")), ansi: ansi)
        }
        let trusts = try records("host_trust", cap: 512)
        var pinCount = 0
        for trust in trusts {
            _ = try Self.host(trust.text("host"))
            _ = try Self.port(trust.required("port").uint())
            let pins = try trust.required("pins").array()
            pinCount += pins.count
            guard pinCount <= 512, !pins.isEmpty else { throw FTError.limit("trust.pins") }
            for pin in pins {
                let fields = try pin.object()
                _ = try fields.text("algorithm")
                let value = try fields.text("fingerprint")
                guard value.hasPrefix("SHA256:"), value.utf8.count == 50,
                      let decoded = Data(base64Encoded: String(value.dropFirst(7)) + "="), decoded.count == 32,
                      "SHA256:" + decoded.base64EncodedString().replacingOccurrences(of: "=", with: "") == value
                else { throw FTError.malformed("trust.fingerprint") }
            }
        }
        routes = try records("routes", cap: 256).map {
            let mode = try $0.text("mode")
            // Via Mac has a different authentication schema; never reinterpret it as SSH.
            guard ["direct_ssh", "frankenterm_mux"].contains(mode) else { throw FTError.unsupported("route.mode") }
            let jumps = try $0.required("jump_routes").array().map { try Self.identifier($0.string()) }
            guard Set(jumps).count == jumps.count, jumps.count <= 3 else { throw FTError.malformed("route.jumps") }
            return Route(id: try $0.text("id"), mode: mode, host: try Self.host($0.text("host")), user: try $0.text("user"),
                         port: try Self.port($0.required("port").uint()), credentialID: try $0.text("credential_ref"),
                         trustID: try $0.text("host_trust_ref"), jumpRoutes: jumps)
        }
        let routeMap = Dictionary(uniqueKeysWithValues: routes.map { ($0.id, $0) })
        let credentialIDs = Set(credentials.map(\.id))
        let trustMap = try Dictionary(uniqueKeysWithValues: trusts.map { (try $0.text("id"), $0) })
        for route in routes {
            guard credentialIDs.contains(route.credentialID), let trust = trustMap[route.trustID],
                  route.host == (try trust.text("host")), UInt64(route.port) == (try trust.required("port").uint())
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
            let themeID = try $0.text("theme_ref"), preferred = try $0.text("preferred_route_ref")
            let ids = try $0.required("route_refs").array().map { try $0.string() }
            guard !ids.isEmpty, ids.count <= 256, Set(ids).count == ids.count, ids.contains(preferred),
                  ids.allSatisfy({ routeMap[$0] != nil }), themeIDs.contains(themeID) else { throw FTError.malformed("machine.references") }
            return Machine(id: try $0.text("id"), name: try $0.text("name"), themeID: themeID,
                           preferredRouteID: preferred, routeIDs: ids, autoConnect: try $0.flag("auto_connect"))
        }
        let machineIDs = Set(machines.map(\.id))
        groups = try records("groups", cap: 64).map {
            let ids = try $0.required("machine_order").array().map { try $0.string() }
            guard ids.count <= 128, Set(ids).count == ids.count, ids.allSatisfy(machineIDs.contains) else { throw FTError.malformed("group.references") }
            return Group(id: try $0.text("id"), name: try $0.text("name"), machineOrder: ids)
        }
        let preferences = try workspace.required("preferences").object()
        initialMachineID = try preferences.text("initial_machine_ref")
        let initialGroupID = try preferences.text("initial_group_ref")
        guard let group = groups.first(where: { $0.id == initialGroupID }), group.machineOrder.contains(initialMachineID) else {
            throw FTError.malformed("workspace.initial_selection")
        }
        raw = data
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
        return value
    }
    static func port(_ value: UInt64) throws -> UInt16 {
        guard value > 0, value <= 65535 else { throw FTError.malformed("route.port") }
        return UInt16(value)
    }
    static func host(_ value: String) throws -> String {
        guard !value.isEmpty, value.utf8.count <= 253, !value.unicodeScalars.contains(where: { $0.value <= 32 || $0.value == 127 }),
              !value.contains("/"), !value.contains("\\"), !value.contains("@") else { throw FTError.malformed("route.host") }
        return value
    }
}
