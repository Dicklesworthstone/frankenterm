import Foundation
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// Normalized negative trust. Serial intervals remain compact, including a
/// range covering almost the entire UInt64 space. Keys and certificate IDs
/// are compared as bytes, never as Unicode-equivalent strings.
public struct WorkspaceHostRevocationPolicy: Equatable, Sendable, Codable {
    public static let maximumEntries = 8192
    public static let maximumAuthorities = 256
    public static let maximumKeyIDBytes = 1024

    public struct SerialRange: Equatable, Sendable, Codable {
        public let first: UInt64, last: UInt64
        public init(first: UInt64, last: UInt64) throws {
            guard first > 0, last >= first else { throw FTError.malformed("revocation.serial_range") }
            self.first = first; self.last = last
        }
        private enum CodingKeys: CodingKey { case first, last }
        public init(from decoder: any Decoder) throws {
            let fields = try decoder.container(keyedBy: CodingKeys.self)
            try self.init(first: fields.decode(UInt64.self, forKey: .first), last: fields.decode(UInt64.self, forKey: .last))
        }
    }

    public struct CertificateRule: Equatable, Sendable, Codable {
        /// Nil is OpenSSH's explicit wildcard CA, not an unresolved issuer.
        public let authoritySHA256: String?
        public let serialRanges: [SerialRange], keyIDs: [Data]
        private let keyIDSet: Set<Data>

        public init(authoritySHA256: String? = nil, serialRanges: [SerialRange] = [], keyIDs: [Data] = []) throws {
            guard serialRanges.count <= maximumEntries, keyIDs.count <= maximumEntries,
                  serialRanges.count + keyIDs.count <= maximumEntries else { throw FTError.limit("revocation.entries") }
            if let authoritySHA256 { _ = try WorkspaceDocument.fingerprint(authoritySHA256) }
            for keyID in keyIDs {
                guard keyID.count <= maximumKeyIDBytes else { throw FTError.limit("revocation.key_id") }
                guard !keyID.contains(0) else { throw FTError.malformed("revocation.key_id") }
            }
            self.authoritySHA256 = authoritySHA256
            self.serialRanges = Self.normalized(serialRanges)
            keyIDSet = Set(keyIDs); self.keyIDs = keyIDSet.sorted { $0.lexicographicallyPrecedes($1) }
        }

        private enum CodingKeys: CodingKey { case authoritySHA256, serialRanges, keyIDs }
        public init(from decoder: any Decoder) throws {
            let fields = try decoder.container(keyedBy: CodingKeys.self)
            try self.init(authoritySHA256: fields.decodeIfPresent(String.self, forKey: .authoritySHA256),
                serialRanges: fields.decode([SerialRange].self, forKey: .serialRanges),
                keyIDs: fields.decode([Data].self, forKey: .keyIDs))
        }

        fileprivate static func normalized(_ ranges: [SerialRange]) -> [SerialRange] {
            let sorted = ranges.sorted { $0.first == $1.first ? $0.last < $1.last : $0.first < $1.first }
            var result: [SerialRange] = []
            for range in sorted {
                if let previous = result.last,
                   previous.last == UInt64.max || range.first <= previous.last + 1 {
                    result[result.count - 1] = SerialRange(validatedFirst: previous.first, last: max(previous.last, range.last))
                } else { result.append(range) }
            }
            return result
        }

        fileprivate func unioning(_ other: Self) throws -> Self {
            guard authoritySHA256 == other.authoritySHA256 else { throw FTError.malformed("revocation.authority") }
            return try .init(authoritySHA256: authoritySHA256,
                serialRanges: Self.normalized(serialRanges + other.serialRanges),
                keyIDs: Array(keyIDSet.union(other.keyIDSet)))
        }

        fileprivate func denies(serial: UInt64, keyID: Data) -> Bool {
            if keyIDSet.contains(keyID) { return true }
            guard serial > 0 else { return false }
            var low = 0, high = serialRanges.count
            while low < high {
                let middle = low + (high - low) / 2
                if serialRanges[middle].first <= serial { low = middle + 1 }
                else { high = middle }
            }
            return low > 0 && serialRanges[low - 1].last >= serial
        }
    }

    public let revokedSHA256: [Data], revokedSHA1: [Data], certificates: [CertificateRule]
    private let sha256Set: Set<Data>, sha1Set: Set<Data>
    private let rules: [String: CertificateRule]
    public var isEmpty: Bool { revokedSHA256.isEmpty && revokedSHA1.isEmpty && certificates.isEmpty }
    /// A SHA256-only pin cannot prove absence from a SHA1 revocation set.
    /// Verification must obtain the actual canonical raw identity in that case.
    public var requiresRawIdentity: Bool { !revokedSHA1.isEmpty }

    public init(revokedSHA256: [Data] = [], revokedSHA1: [Data] = [], certificates: [CertificateRule] = []) throws {
        guard certificates.count <= Self.maximumAuthorities,
              revokedSHA256.count <= Self.maximumEntries, revokedSHA1.count <= Self.maximumEntries else {
            throw FTError.limit("revocation.entries")
        }
        var entries = revokedSHA256.count + revokedSHA1.count
        for rule in certificates {
            entries += rule.serialRanges.count + rule.keyIDs.count
            guard entries <= Self.maximumEntries else { throw FTError.limit("revocation.entries") }
        }
        guard entries <= Self.maximumEntries else { throw FTError.limit("revocation.entries") }
        guard revokedSHA256.allSatisfy({ $0.count == 32 }), revokedSHA1.allSatisfy({ $0.count == 20 }) else {
            throw FTError.malformed("revocation.digest")
        }
        var rules: [String: CertificateRule] = [:]
        for rule in certificates where !rule.serialRanges.isEmpty || !rule.keyIDs.isEmpty {
            let key = rule.authoritySHA256 ?? ""
            rules[key] = try rules[key].map { try $0.unioning(rule) } ?? rule
        }
        sha256Set = Set(revokedSHA256); sha1Set = Set(revokedSHA1)
        self.revokedSHA256 = sha256Set.sorted { $0.lexicographicallyPrecedes($1) }
        self.revokedSHA1 = sha1Set.sorted { $0.lexicographicallyPrecedes($1) }
        self.rules = rules; self.certificates = rules.keys.sorted().compactMap { rules[$0] }
    }

    private init(empty: Void) {
        revokedSHA256 = []; revokedSHA1 = []; certificates = []; sha256Set = []; sha1Set = []; rules = [:]
    }
    public static let empty = Self(empty: ())

    private enum CodingKeys: CodingKey { case revokedSHA256, revokedSHA1, certificates }
    public init(from decoder: any Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(revokedSHA256: fields.decode([Data].self, forKey: .revokedSHA256),
            revokedSHA1: fields.decode([Data].self, forKey: .revokedSHA1),
            certificates: fields.decode([CertificateRule].self, forKey: .certificates))
    }

    public func unioning(_ other: Self) throws -> Self {
        if isEmpty { return other }
        if other.isEmpty || self == other { return self }
        var combined = rules
        for (authority, rule) in other.rules {
            combined[authority] = try combined[authority].map { try $0.unioning(rule) } ?? rule
        }
        return try .init(revokedSHA256: Array(sha256Set.union(other.sha256Set)),
            revokedSHA1: Array(sha1Set.union(other.sha1Set)), certificates: Array(combined.values))
    }

    /// This answers only negative trust. The caller separately binds the exact
    /// raw identity/certificate to the exchanged host key and refuses a missing
    /// raw witness when requiresRawIdentity is true. Certificate hashes in an
    /// OpenSSH KRL refer to the raw subject, not the complete certificate blob.
    public func denies(presented: WorkspaceDocument.HostKey, rawIdentity: SSHPublicIdentity? = nil,
                       certificate: SSHHostCertificate? = nil) -> Bool {
        if let certificate {
            if deniesRaw(certificate.subject) || deniesRaw(certificate.authority) { return true }
            return rules[certificate.authority.fingerprint]?.denies(serial: certificate.serial, keyID: certificate.keyID) == true
                || rules[""]?.denies(serial: certificate.serial, keyID: certificate.keyID) == true
        }
        if let rawIdentity { return deniesRaw(rawIdentity) }
        guard !presented.algorithm.contains("-cert-"), let digest = Self.fingerprintBytes(presented.fingerprint) else { return false }
        return sha256Set.contains(digest)
    }

    private func deniesRaw(_ identity: SSHPublicIdentity) -> Bool {
        if let digest = Self.fingerprintBytes(identity.fingerprint), sha256Set.contains(digest) { return true }
        return !sha1Set.isEmpty && sha1Set.contains(Data(Insecure.SHA1.hash(data: identity.blob)))
    }

    fileprivate static func fingerprintBytes(_ fingerprint: String) -> Data? {
        guard fingerprint.hasPrefix("SHA256:") else { return nil }
        return Data(base64Encoded: String(fingerprint.dropFirst(7)) + "=")
    }
}

extension WorkspaceHostRevocationPolicy.SerialRange {
    fileprivate init(validatedFirst: UInt64, last: UInt64) { first = validatedFirst; self.last = last }
}

/// Provenance is descriptive, not a freshness or signature authority. The
/// authenticated workspace binds these normalized denials and the claimed
/// source digest together. Same-provenance conflicting contents must be refused
/// by the resource pool, not silently substituted during a later import.
public struct WorkspaceHostRevocationSource: Equatable, Sendable, Codable {
    public let id: String, sourceKind: String, sourceSHA256: String
    public let krlVersion: UInt64?, generatedAt: UInt64?
    public let policy: WorkspaceHostRevocationPolicy

    public init(id: String, sourceKind: String, sourceSHA256: String,
                krlVersion: UInt64? = nil, generatedAt: UInt64? = nil,
                policy: WorkspaceHostRevocationPolicy) throws {
        self.id = try WorkspaceDocument.identifier(id)
        self.sourceSHA256 = try WorkspaceDocument.fingerprint(sourceSHA256)
        switch sourceKind {
        case "openssh_key_list_v1":
            guard krlVersion == nil, generatedAt == nil, policy.revokedSHA1.isEmpty, policy.certificates.isEmpty else {
                throw FTError.malformed("revocation.source_kind")
            }
        case "openssh_krl_v1":
            guard krlVersion != nil, generatedAt != nil else { throw FTError.malformed("revocation.provenance") }
        default: throw FTError.unsupported("revocation.source_kind")
        }
        self.sourceKind = sourceKind; self.krlVersion = krlVersion; self.generatedAt = generatedAt; self.policy = policy
    }

    private enum CodingKeys: CodingKey { case id, sourceKind, sourceSHA256, krlVersion, generatedAt, policy }
    public init(from decoder: any Decoder) throws {
        let fields = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(id: fields.decode(String.self, forKey: .id), sourceKind: fields.decode(String.self, forKey: .sourceKind),
            sourceSHA256: fields.decode(String.self, forKey: .sourceSHA256),
            krlVersion: fields.decodeIfPresent(UInt64.self, forKey: .krlVersion),
            generatedAt: fields.decodeIfPresent(UInt64.self, forKey: .generatedAt),
            policy: fields.decode(WorkspaceHostRevocationPolicy.self, forKey: .policy))
    }

    public init(json: JSONValue) throws {
        let fields = try json.object()
        guard Set(fields.keys).isSubset(of: ["id", "source_kind", "source_sha256", "krl_version", "generated_at",
            "revoked_sha256", "revoked_sha1", "certificates"]) else { throw FTError.malformed("revocation.source_field") }
        func digests(_ name: String, length: Int) throws -> [Data] {
            let values = try fields.required(name).array()
            guard values.count <= WorkspaceHostRevocationPolicy.maximumEntries else { throw FTError.limit("revocation.entries") }
            return try values.map { try Self.bytes($0, maximum: length, exact: length) }
        }
        let certificateValues = try fields.required("certificates").array()
        guard certificateValues.count <= WorkspaceHostRevocationPolicy.maximumAuthorities else { throw FTError.limit("revocation.authorities") }
        let certificates = try certificateValues.map { value -> WorkspaceHostRevocationPolicy.CertificateRule in
            let rule = try value.object()
            guard Set(rule.keys).isSubset(of: ["authority_sha256", "serial_ranges", "key_ids"]) else {
                throw FTError.malformed("revocation.certificate_field")
            }
            let ranges = try rule.required("serial_ranges").array(), ids = try rule.required("key_ids").array()
            guard ranges.count + ids.count <= WorkspaceHostRevocationPolicy.maximumEntries else { throw FTError.limit("revocation.entries") }
            let serials = try ranges.map { value -> WorkspaceHostRevocationPolicy.SerialRange in
                let range = try value.object()
                guard Set(range.keys) == ["first", "last"] else { throw FTError.malformed("revocation.serial_range") }
                return try .init(first: range.required("first").uint(), last: range.required("last").uint())
            }
            return try .init(authoritySHA256: rule["authority_sha256"].map { try $0.string() }, serialRanges: serials,
                keyIDs: ids.map { try Self.bytes($0, maximum: WorkspaceHostRevocationPolicy.maximumKeyIDBytes) })
        }
        try self.init(id: fields.text("id"), sourceKind: fields.text("source_kind"), sourceSHA256: fields.text("source_sha256"),
            krlVersion: fields["krl_version"].map { try $0.uint() }, generatedAt: fields["generated_at"].map { try $0.uint() },
            policy: .init(revokedSHA256: digests("revoked_sha256", length: 32), revokedSHA1: digests("revoked_sha1", length: 20),
                certificates: certificates))
    }

    private static func bytes(_ value: JSONValue, maximum: Int, exact: Int? = nil) throws -> Data {
        let text = try value.string()
        guard text.utf8.count <= 4 * ((maximum + 2) / 3), let data = Data(base64Encoded: text),
              data.count <= maximum, exact == nil || data.count == exact,
              data.base64EncodedString() == text else { throw FTError.malformed("revocation.base64") }
        return data
    }

    public var json: JSONValue {
        var fields: [String: JSONValue] = ["id": .string(id), "source_kind": .string(sourceKind), "source_sha256": .string(sourceSHA256),
            "revoked_sha256": .array(policy.revokedSHA256.map { .string($0.base64EncodedString()) }),
            "revoked_sha1": .array(policy.revokedSHA1.map { .string($0.base64EncodedString()) }),
            "certificates": .array(policy.certificates.map { certificate in
                var rule: [String: JSONValue] = ["serial_ranges": .array(certificate.serialRanges.map {
                    .object(["first": .number(String($0.first)), "last": .number(String($0.last))])
                }), "key_ids": .array(certificate.keyIDs.map { .string($0.base64EncodedString()) })]
                if let authority = certificate.authoritySHA256 { rule["authority_sha256"] = .string(authority) }
                return .object(rule)
            })]
        if let krlVersion { fields["krl_version"] = .number(String(krlVersion)) }
        if let generatedAt { fields["generated_at"] = .number(String(generatedAt)) }
        return .object(fields)
    }
}

/// Passive, bounded OpenSSH RevokedHostKeys decoding. No file, command, network
/// or clock is consulted. Unknown mandatory semantics fail as a whole, so an
/// accepted prefix can never turn an unreadable revocation policy into trust.
public enum SSHRevokedHostKeys {
    public static let maximumSourceBytes = 256 * 1024
    public static let maximumAggregateSourceBytes = 2 * 1024 * 1024
    public static let maximumSources = 64
    public static let maximumReferences = 16
    private static let magic = Data([0x53, 0x53, 0x48, 0x4b, 0x52, 0x4c, 0x0a, 0])
    private typealias Policy = WorkspaceHostRevocationPolicy

    public static func parse(data: Data, id: String? = nil) throws -> WorkspaceHostRevocationSource {
        try Task.checkCancellation()
        guard data.count <= maximumSourceBytes else { throw FTError.limit("revocation.source_bytes") }
        let digest = Data(SHA256.hash(data: data))
        let fingerprint = "SHA256:" + digest.base64EncodedString().replacingOccurrences(of: "=", with: "")
        let identifier = id ?? "revocation-" + digest.map { String(format: "%02x", $0) }.joined()
        if data.starts(with: magic) {
            let decoded = try parseKRL(data)
            return try .init(id: identifier, sourceKind: "openssh_krl_v1", sourceSHA256: fingerprint,
                krlVersion: decoded.version, generatedAt: decoded.generated, policy: decoded.policy)
        }
        return try .init(id: identifier, sourceKind: "openssh_key_list_v1", sourceSHA256: fingerprint,
            policy: parseKeyList(data))
    }

    private static func parseKeyList(_ data: Data) throws -> Policy {
        var hashes: [Data] = [], lines = 0
        for rawLine in data.split(separator: 10, omittingEmptySubsequences: false) {
            try Task.checkCancellation()
            lines += 1
            guard lines <= 16_384 else { throw FTError.limit("revocation.lines") }
            let line = rawLine.drop(while: { $0 == 32 || $0 == 9 })
            if line.isEmpty || line.first == 35 || line.elementsEqual([13]) { continue }
            let parts = line.split(maxSplits: 2, omittingEmptySubsequences: true, whereSeparator: { $0 == 32 || $0 == 9 || $0 == 13 })
            guard parts.count >= 2, let name = String(data: Data(parts[0]), encoding: .ascii),
                  let base64 = String(data: Data(parts[1]), encoding: .ascii),
                  base64.utf8.count <= 4 * ((SSHCertificateFields.maximumBlobBytes + 2) / 3),
                  let blob = Data(base64Encoded: base64) else { throw FTError.malformed("revocation.key_list") }
            let identity = try rawIdentity(blob)
            let blobName = try SSHCertificateFields.algorithmName(in: blob)
            guard name == blobName else { throw FTError.malformed("revocation.key_type") }
            hashes.append(Data(SHA256.hash(data: identity.blob)))
            guard hashes.count <= Policy.maximumEntries else { throw FTError.limit("revocation.entries") }
        }
        return try .init(revokedSHA256: hashes)
    }

    private static func rawIdentity(_ blob: Data) throws -> SSHPublicIdentity {
        let algorithm = try SSHCertificateFields.algorithmName(in: blob)
        guard algorithm.contains("-cert-v01@") else { return try SSHPublicIdentity(blob: blob) }
        // A certificate on a flat key list denies its raw public subject,
        // regardless of serial, validity or signed permissions.
        do { return try SSHCertificateFields(blob: blob, kind: .host).subject }
        catch { return try SSHCertificateFields(blob: blob, kind: .user).subject }
    }

    private struct Accumulator {
        var sha256: [Data] = [], sha1: [Data] = [], certificates: [Policy.CertificateRule] = []
        var entries = 0, sections = 0
        mutating func count(_ added: Int = 1) throws {
            guard added >= 0, added <= Policy.maximumEntries - entries else { throw FTError.limit("revocation.entries") }
            entries += added
        }
        mutating func section() throws {
            sections += 1
            guard sections <= Policy.maximumEntries else { throw FTError.limit("revocation.sections") }
            try Task.checkCancellation()
        }
    }

    private static func parseKRL(_ data: Data) throws -> (version: UInt64, generated: UInt64, policy: Policy) {
        var input = ByteCursor(data)
        _ = try input.take(magic.count)
        guard try input.uint(4) == 1 else { throw FTError.unsupported("revocation.krl_version") }
        let version = try input.uint(8), generated = try input.uint(8)
        guard try input.uint(8) == 0 else { throw FTError.unsupported("revocation.flags") }
        _ = try input.sshString(cap: maximumSourceBytes) // ignored reserved field
        let comment = try input.sshString(cap: maximumSourceBytes)
        guard !comment.contains(0) else { throw FTError.malformed("revocation.comment") }
        var result = Accumulator()
        while input.remaining > 0 {
            try result.section()
            let type = try input.uint(1)
            var section = ByteCursor(try input.sshString(cap: maximumSourceBytes))
            switch type {
            case 1: try certificateSection(&section, result: &result)
            case 2:
                while section.remaining > 0 {
                    try result.count()
                    // OpenSSH's explicit-key section compares exact plain key
                    // blobs. Hashing preserves that equality without exporting
                    // opaque blob contents or admitting a key for authentication.
                    let blob = try section.sshString(cap: maximumSourceBytes)
                    result.sha256.append(Data(SHA256.hash(data: blob)))
                }
            case 3, 5:
                while section.remaining > 0 {
                    try result.count()
                    let length = type == 3 ? 20 : 32, digest = try section.sshString(cap: length)
                    guard digest.count == length else { throw FTError.malformed("revocation.digest") }
                    if type == 3 { result.sha1.append(digest) } else { result.sha256.append(digest) }
                }
            case 255: try optionalExtension(&section)
            case 4: throw FTError.unsupported("revocation.signature_section")
            default: throw FTError.unsupported("revocation.section")
            }
            guard section.remaining == 0 else { throw FTError.malformed("revocation.trailing_data") }
        }
        return (version, generated, try .init(revokedSHA256: result.sha256, revokedSHA1: result.sha1,
            certificates: result.certificates))
    }

    private static func certificateSection(_ input: inout ByteCursor, result: inout Accumulator) throws {
        let authorityBlob = try input.sshString(cap: SSHPublicIdentity.maximumBlobBytes)
        let authority = try authorityBlob.isEmpty ? nil : SSHPublicIdentity(blob: authorityBlob).fingerprint
        _ = try input.sshString(cap: maximumSourceBytes) // ignored certificate-section reserved field
        var ranges: [Policy.SerialRange] = [], ids: [Data] = []
        while input.remaining > 0 {
            try result.section()
            let type = try input.uint(1)
            var section = ByteCursor(try input.sshString(cap: maximumSourceBytes))
            switch type {
            case 0x20:
                while section.remaining > 0 {
                    try result.count()
                    let serial = try section.uint(8)
                    ranges.append(try .init(first: serial, last: serial))
                }
            case 0x21:
                try result.count()
                ranges.append(try .init(first: section.uint(8), last: section.uint(8)))
            case 0x22:
                let offset = try section.uint(8), bitmap = try section.sshString(cap: maximumSourceBytes)
                try bitmapRanges(bitmap, offset: offset, ranges: &ranges, result: &result)
            case 0x23:
                while section.remaining > 0 {
                    try result.count()
                    let keyID = try section.sshString(cap: Policy.maximumKeyIDBytes)
                    guard !keyID.contains(0) else { throw FTError.malformed("revocation.key_id") }
                    ids.append(keyID)
                }
            case 0x39: try optionalExtension(&section)
            default: throw FTError.unsupported("revocation.certificate_section")
            }
            guard section.remaining == 0 else { throw FTError.malformed("revocation.trailing_data") }
        }
        guard result.certificates.count < Policy.maximumAuthorities else { throw FTError.limit("revocation.authorities") }
        result.certificates.append(try .init(authoritySHA256: authority, serialRanges: ranges, keyIDs: ids))
    }

    private static func bitmapRanges(_ encoded: Data, offset: UInt64, ranges: inout [Policy.SerialRange],
                                     result: inout Accumulator) throws {
        if encoded.isEmpty { return } // canonical zero mpint
        guard let first = encoded.first, first & 0x80 == 0 else { throw FTError.malformed("revocation.bitmap_sign") }
        var bytes = encoded
        if first == 0 {
            guard encoded.count > 1, encoded[encoded.startIndex + 1] & 0x80 != 0 else {
                throw FTError.malformed("revocation.bitmap_encoding")
            }
            bytes = Data(encoded.dropFirst())
        }
        guard let leading = bytes.first else { return }
        let bitCount = (bytes.count - 1) * 8 + (8 - leading.leadingZeroBitCount)
        let (_, overflow) = offset.addingReportingOverflow(UInt64(bitCount - 1))
        guard !overflow else { throw FTError.malformed("revocation.bitmap_overflow") }
        var run: UInt64?
        for bit in 0..<bitCount {
            if bit & 0x3fff == 0 { try Task.checkCancellation() }
            let set = bytes[bytes.count - 1 - bit / 8] & (UInt8(1) << (bit % 8)) != 0
            if set {
                if run == nil { run = offset + UInt64(bit) }
            } else if let first = run {
                try result.count(); ranges.append(try .init(first: first, last: offset + UInt64(bit) - 1)); run = nil
            }
        }
        if let first = run {
            try result.count(); ranges.append(try .init(first: first, last: offset + UInt64(bitCount - 1)))
        }
    }

    private static func optionalExtension(_ input: inout ByteCursor) throws {
        let name = try input.sshString(cap: FTLimit.text), critical = try input.uint(1)
        _ = try input.sshString(cap: maximumSourceBytes)
        guard !name.contains(0), input.remaining == 0 else { throw FTError.malformed("revocation.extension") }
        guard critical == 0 else { throw FTError.unsupported("revocation.critical_extension") }
    }
}
