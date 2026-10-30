import Foundation
#if canImport(CryptoKit)
import CryptoKit
#elseif canImport(Crypto)
import Crypto
#endif

/// A public request to authorize one independently generated phone identity.
/// Opening or transferring it does not authorize a connection or remote edit.
/// The Mac must show the fingerprint and obtain a separate target review.
public struct WorkspaceKeyEnrollmentRequest: Equatable, Sendable, Identifiable {
    public static let maximumBytes = 16_384
    public let id: String
    public let key: WorkspaceDeviceKey

    public init(key: WorkspaceDeviceKey, requestID: UUID = UUID()) throws {
        try key.validate()
        self.id = requestID.uuidString.lowercased()
        self.key = key
    }

    public init(data: Data) throws {
        guard !data.isEmpty, data.count <= Self.maximumBytes else {
            throw FTError.limit("enrollment.request_bytes")
        }
        let root = try StrictJSON.decode(data).object()
        try Self.exactFields(root, ["format", "version", "request_id", "key"])
        guard try root.required("format").string() == "ft-phone-key-enrollment",
              try root.required("version").uint() == 1 else {
            throw FTError.unsupported("enrollment.request_version")
        }
        let id = try root.required("request_id").string()
        guard let requestID = UUID(uuidString: id), requestID.uuidString.lowercased() == id else {
            throw FTError.malformed("enrollment.request_id")
        }
        let record = try root.required("key").object()
        try Self.exactFields(record, ["id", "algorithm", "public_blob", "public_fingerprint"])
        guard try record.required("algorithm").string() == "ssh-ed25519" else {
            throw FTError.unsupported("enrollment.key_algorithm")
        }
        let encoded = try record.required("public_blob").string()
        guard encoded.utf8.count <= 128, let blob = Data(base64Encoded: encoded),
              blob.base64EncodedString() == encoded else {
            throw FTError.malformed("enrollment.public_blob")
        }
        let key = WorkspaceDeviceKey(id: try record.required("id").string(),
            fingerprint: try record.required("public_fingerprint").string(), publicBlob: blob)
        try self.init(key: key, requestID: requestID)
    }

    public func encoded() throws -> Data {
        let data = try StrictJSON.encode(.object([
            "format": .string("ft-phone-key-enrollment"), "version": .number("1"),
            "request_id": .string(id), "key": .object([
                "id": .string(key.id), "algorithm": .string(key.algorithm),
                "public_blob": .string(key.publicBlob.base64EncodedString()),
                "public_fingerprint": .string(key.fingerprint)
            ])
        ]))
        guard data.count <= Self.maximumBytes else { throw FTError.limit("enrollment.request_bytes") }
        return data
    }

    /// Stable across retries and restored jobs, but distinct for another request,
    /// identity or account. All fields exclude NUL, so separators are unambiguous.
    /// This is provenance/idempotency metadata, not authentication or authority.
    public func operationID(for target: WorkspaceKeyEnrollmentTarget) -> String {
        enrollmentDigest("frankenterm-phone-key-enrollment-operation-v1\0" + id + "\0" +
            key.id + "\0" + key.fingerprint + "\0" + target.id)
    }

    private static func exactFields(_ object: [String: JSONValue], _ expected: Set<String>) throws {
        guard Set(object.keys) == expected else { throw FTError.malformed("enrollment.request_fields") }
    }
}

/// The exact remote account reviewed for enrollment. Route selection and SSH
/// host authentication remain the transport's responsibility. This endpoint
/// profile deliberately rejects shell syntax, option-like names and scoped IPs.
public struct WorkspaceKeyEnrollmentTarget: Codable, Equatable, Hashable, Identifiable, Sendable {
    public let host: String
    public let port: UInt16
    public let user: String
    public var id: String {
        enrollmentDigest("frankenterm-phone-key-enrollment-target-v1\0" + host + "\0" +
            String(port) + "\0" + user)
    }

    public init(host: String, port: UInt16 = 22, user: String) throws {
        guard !host.isEmpty, host.utf8.allSatisfy({ $0 < 128 }), host.first != "-", port > 0 else {
            throw FTError.malformed("enrollment.target")
        }
        let canonical = try WorkspaceDocument.host(host)
        let bytes = Array(user.utf8)
        func alphaNumeric(_ byte: UInt8) -> Bool {
            (48...57).contains(byte) || (65...90).contains(byte) || (97...122).contains(byte)
        }
        guard (1...128).contains(bytes.count), let first = bytes.first,
              alphaNumeric(first) || first == 95,
              bytes.allSatisfy({ alphaNumeric($0) || [45, 46, 95].contains($0) }) else {
            throw FTError.malformed("enrollment.account")
        }
        self.host = canonical; self.port = port; self.user = user
    }

    private enum CodingKeys: String, CodingKey { case host, port, user }

    public init(from decoder: any Decoder) throws {
        try enrollmentExactCodingFields(decoder, ["host", "port", "user"])
        let values = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(host: values.decode(String.self, forKey: .host),
                      port: values.decode(UInt16.self, forKey: .port),
                      user: values.decode(String.self, forKey: .user))
    }
}

public enum WorkspaceKeyEnrollmentAction: String, Codable, Sendable {
    /// Inspect only; an absent entry remains absent until another user decision.
    case inspect
    /// Inspect first, add only if absent, then read back the exact owned entry.
    case install
}

/// File-level observations, never a claim that the phone successfully signed in.
/// Restricted preexisting entries are not duplicated with unrestricted entries.
public enum WorkspaceKeyEnrollmentState: String, Codable, CaseIterable, Sendable {
    case absent, installed, alreadyPresent, restricted, blocked, uncertain
}

/// One account's outcome. Remote diagnostic text must not be used as detail;
/// the transport maps its bounded response codes to locally supplied prose.
public struct WorkspaceKeyEnrollmentResult: Codable, Equatable, Sendable {
    public let operationID: String
    public let target: WorkspaceKeyEnrollmentTarget
    public let state: WorkspaceKeyEnrollmentState
    public let detail: String

    public init(operationID: String, target: WorkspaceKeyEnrollmentTarget,
                state: WorkspaceKeyEnrollmentState, detail: String) throws {
        guard operationID.utf8.count == 64, operationID.utf8.allSatisfy({
            (48...57).contains($0) || (97...102).contains($0)
        }) else { throw FTError.malformed("enrollment.operation_id") }
        guard detail.utf8.count <= 1024,
              !detail.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else {
            throw FTError.malformed("enrollment.result_detail")
        }
        self.operationID = operationID; self.target = target; self.state = state; self.detail = detail
    }

    private enum CodingKeys: String, CodingKey { case operationID, target, state, detail }

    public init(from decoder: any Decoder) throws {
        try enrollmentExactCodingFields(decoder, ["operationID", "target", "state", "detail"])
        let values = try decoder.container(keyedBy: CodingKeys.self)
        try self.init(operationID: values.decode(String.self, forKey: .operationID),
                      target: values.decode(WorkspaceKeyEnrollmentTarget.self, forKey: .target),
                      state: values.decode(WorkspaceKeyEnrollmentState.self, forKey: .state),
                      detail: values.decode(String.self, forKey: .detail))
    }
}

/// A bounded public record of the reviewed account set and every known outcome.
/// The caller must durably save `beginInstall` before allowing the remote edit.
/// Loading this record never resumes an operation or grants connection authority.
public struct WorkspaceKeyEnrollmentJournal: Equatable, Sendable {
    public static let maximumBytes = 524_288
    public static let maximumTargets = 256
    public let request: WorkspaceKeyEnrollmentRequest
    public let targets: [WorkspaceKeyEnrollmentTarget]
    public private(set) var results: [WorkspaceKeyEnrollmentResult]

    public init(request: WorkspaceKeyEnrollmentRequest, targets: [WorkspaceKeyEnrollmentTarget]) throws {
        guard (1...Self.maximumTargets).contains(targets.count),
              Set(targets.map(\.id)).count == targets.count else {
            throw FTError.malformed("enrollment.journal_targets")
        }
        self.request = request; self.targets = targets; results = []
    }

    public init(data: Data) throws {
        guard !data.isEmpty, data.count <= Self.maximumBytes else {
            throw FTError.limit("enrollment.journal_bytes")
        }
        let root = try StrictJSON.decode(data).object()
        guard Set(root.keys) == ["format", "version", "request", "targets", "results"],
              try root.required("format").string() == "ft-phone-key-enrollment-journal",
              try root.required("version").uint() == 1 else {
            throw FTError.malformed("enrollment.journal_format")
        }
        let request = try WorkspaceKeyEnrollmentRequest(data: StrictJSON.encode(root.required("request")))
        let rawTargets = try root.required("targets").array()
        guard (1...Self.maximumTargets).contains(rawTargets.count) else {
            throw FTError.limit("enrollment.journal_targets")
        }
        let targets = try rawTargets.map { raw -> WorkspaceKeyEnrollmentTarget in
            let record = try raw.object()
            guard Set(record.keys) == ["host", "port", "user"] else {
                throw FTError.malformed("enrollment.journal_target_fields")
            }
            return try .init(host: record.required("host").string(),
                port: WorkspaceDocument.port(record.required("port").uint()),
                user: record.required("user").string())
        }
        try self.init(request: request, targets: targets)
        let rawResults = try root.required("results").array()
        guard rawResults.count <= targets.count else { throw FTError.limit("enrollment.journal_results") }
        let indexed = Dictionary(uniqueKeysWithValues: targets.map { ($0.id, $0) })
        var seen = Set<String>()
        for raw in rawResults {
            let record = try raw.object()
            guard Set(record.keys) == ["operation_id", "target_id", "state", "detail"],
                  let target = try indexed[record.required("target_id").string()],
                  let state = try WorkspaceKeyEnrollmentState(rawValue: record.required("state").string()),
                  seen.insert(target.id).inserted else {
                throw FTError.malformed("enrollment.journal_result_fields")
            }
            let result = try WorkspaceKeyEnrollmentResult(
                operationID: record.required("operation_id").string(), target: target,
                state: state, detail: record.required("detail").string())
            try validate(result)
            results.append(result)
        }
        orderResults()
    }

    public func encoded() throws -> Data {
        let data = try StrictJSON.encode(.object([
            "format": .string("ft-phone-key-enrollment-journal"), "version": .number("1"),
            "request": try StrictJSON.decode(request.encoded()),
            "targets": .array(targets.map { target in .object([
                "host": .string(target.host), "port": .number(String(target.port)), "user": .string(target.user)
            ]) }),
            "results": .array(results.map { result in .object([
                "operation_id": .string(result.operationID), "target_id": .string(result.target.id),
                "state": .string(result.state.rawValue), "detail": .string(result.detail)
            ]) })
        ]))
        guard data.count <= Self.maximumBytes else { throw FTError.limit("enrollment.journal_bytes") }
        return data
    }

    public func result(for targetID: String) -> WorkspaceKeyEnrollmentResult? {
        results.first { $0.target.id == targetID }
    }

    public mutating func beginInstall(targetID: String) throws {
        guard let target = targets.first(where: { $0.id == targetID }) else {
            throw FTError.stale
        }
        let pending = try WorkspaceKeyEnrollmentResult(operationID: request.operationID(for: target),
            target: target, state: .uncertain,
            detail: "This account may have changed. Inspect its current state before another edit.")
        set(pending)
    }

    public mutating func record(_ result: WorkspaceKeyEnrollmentResult,
                                action: WorkspaceKeyEnrollmentAction) throws {
        try validate(result)
        // A successful read may reconcile uncertainty. A failed read, including
        // an install retry's preflight, cannot certify that an earlier edit failed.
        if result.state == .blocked,
           self.result(for: result.target.id)?.state == .uncertain {
            set(try .init(operationID: result.operationID, target: result.target,
                          state: .uncertain, detail: result.detail))
            return
        }
        guard action != .inspect || result.state != .installed else {
            throw FTError.malformed("enrollment.inspect_mutation_claim")
        }
        guard action != .install || result.state != .absent else {
            throw FTError.malformed("enrollment.install_absence_claim")
        }
        set(result)
    }

    private func validate(_ result: WorkspaceKeyEnrollmentResult) throws {
        guard targets.contains(result.target), result.operationID == request.operationID(for: result.target) else {
            throw FTError.stale
        }
    }

    private mutating func set(_ result: WorkspaceKeyEnrollmentResult) {
        if let index = results.firstIndex(where: { $0.target.id == result.target.id }) {
            results[index] = result
        } else { results.append(result) }
        orderResults()
    }

    private mutating func orderResults() {
        let order = Dictionary(uniqueKeysWithValues: targets.enumerated().map { ($0.element.id, $0.offset) })
        let count = targets.count
        results.sort { order[$0.target.id, default: count] < order[$1.target.id, default: count] }
    }
}

private func enrollmentDigest(_ value: String) -> String {
    SHA256.hash(data: Data(value.utf8)).map { String(format: "%02x", $0) }.joined()
}

private struct EnrollmentCodingKey: CodingKey {
    let stringValue: String
    var intValue: Int? { nil }
    init?(stringValue: String) { self.stringValue = stringValue }
    init?(intValue: Int) { return nil }
}

private func enrollmentExactCodingFields(_ decoder: any Decoder, _ expected: Set<String>) throws {
    let values = try decoder.container(keyedBy: EnrollmentCodingKey.self)
    guard Set(values.allKeys.map(\.stringValue)) == expected else {
        throw FTError.malformed("enrollment.journal_fields")
    }
}
