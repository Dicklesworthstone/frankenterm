import Foundation
import XCTest
@testable import FrankenTermKit
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

/// Independent ssh-keygen decisions live in SSHHostRevocationOracleTests.
/// These tests target refusal, resource bounds, immutable normalization and the
/// precise byte distinctions that an authentication caller must retain.
final class SSHRevokedHostKeysTests: XCTestCase {
    private typealias Policy = WorkspaceHostRevocationPolicy
    private func integer(_ value: UInt64, bytes: Int = 8) -> Data {
        var result = Data(); result.appendBE(value, bytes: bytes); return result
    }
    private func field(_ data: Data) -> Data { integer(UInt64(data.count), bytes: 4) + data }
    private func section(_ type: UInt8, _ data: Data) -> Data { Data([type]) + field(data) }
    private func krl(_ body: Data = Data(), format: UInt64 = 1, version: UInt64 = 7,
                     generated: UInt64 = 0, flags: UInt64 = 0, reserved: Data = Data(), comment: Data = Data()) -> Data {
        Data([0x53, 0x53, 0x48, 0x4b, 0x52, 0x4c, 0x0a, 0]) + integer(format, bytes: 4)
            + integer(version) + integer(generated) + integer(flags) + field(reserved) + field(comment) + body
    }
    private func certificateSection(_ body: Data, authority: Data = Data(), reserved: Data = Data()) -> Data {
        section(1, field(authority) + field(reserved) + body)
    }
    private func extensionData(critical: UInt8, name: Data = Data("unrecognized.example".utf8),
                               value: Data = Data([1, 2, 3])) -> Data {
        field(name) + Data([critical]) + field(value)
    }
    private func certificate(serial: UInt64 = 1, keyID: Data = Data("workspace-fixture".utf8)) throws -> SSHHostCertificate {
        try .init(blob: workspaceTestHostCertificate(serial: serial, keyID: keyID))
    }
    private func presented(_ identity: SSHPublicIdentity) throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: identity.algorithm, fingerprint: identity.fingerprint)
    }
    private func presented(_ certificate: SSHHostCertificate) throws -> WorkspaceDocument.HostKey {
        try .init(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint)
    }

    func testEmptyReadableSnapshotsRemainWitnessedWithoutClaimingFreshness() throws {
        let text = try SSHRevokedHostKeys.parse(data: Data())
        XCTAssertEqual(text.sourceKind, "openssh_key_list_v1"); XCTAssertTrue(text.policy.isEmpty)
        XCTAssertNil(text.krlVersion); XCTAssertNil(text.generatedAt)
        XCTAssertTrue(text.id.hasPrefix("revocation-")); XCTAssertEqual(text.id.count, 75)
        let empty = try SSHRevokedHostKeys.parse(data: krl(version: UInt64.max, generated: UInt64.max))
        XCTAssertEqual(empty.sourceKind, "openssh_krl_v1"); XCTAssertTrue(empty.policy.isEmpty)
        XCTAssertEqual(empty.krlVersion, UInt64.max); XCTAssertEqual(empty.generatedAt, UInt64.max)
        XCTAssertNotEqual(text.sourceSHA256, empty.sourceSHA256)
        XCTAssertEqual(try WorkspaceHostRevocationSource(json: empty.json), empty)
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceHostRevocationSource.self, from: JSONEncoder().encode(empty)), empty)
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: Data(), id: "unsafe\nresource"))
    }

    func testTruncatedMalformedAndUnknownKRLNeverPublishesAnAcceptedPrefix() throws {
        let digest = Data(repeating: 17, count: 32), valid = krl(section(5, field(digest)))
        // A header alone is valid; every cut inside its scalar/string fields or
        // the mandatory section's framed data must fail rather than disappear.
        let headerBytes = krl().count
        for length in Array(1..<headerBytes) + Array((headerBytes + 1)..<valid.count) {
            XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: Data(valid.prefix(length))), "cut at \(length)")
        }
        for invalid in [krl(format: 2), krl(flags: 1), krl(comment: Data([65, 0, 66])),
                        valid + Data([5]), valid + section(0x7f, Data()),
                        krl(section(5, field(Data(repeating: 1, count: 31)))),
                        krl(section(3, field(Data(repeating: 1, count: 21)))),
                        krl(certificateSection(section(0x24, Data()))) ] {
            XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: invalid))
        }
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: Data(repeating: 0, count: SSHRevokedHostKeys.maximumSourceBytes + 1)))
    }

    func testReservedAndOptionalExtensionsAreInertButCriticalOrSignedPolicyBlocks() throws {
        let body = section(255, extensionData(critical: 0))
            + certificateSection(section(0x39, extensionData(critical: 0)), reserved: Data([0xff, 0]))
        let accepted = try SSHRevokedHostKeys.parse(data: krl(body, reserved: Data([0, 1, 0xff]), comment: Data([0xff])))
        XCTAssertTrue(accepted.policy.isEmpty)
        XCTAssertFalse(try StrictJSON.encode(accepted.json).contains(0xff), "opaque source payloads do not survive normalization")
        for critical in [UInt8(1), 2, 255] {
            XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(section(255, extensionData(critical: critical)))))
            XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(certificateSection(section(0x39, extensionData(critical: critical))))))
        }
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(section(255, extensionData(critical: 0, name: Data([1, 0]))))))
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(section(255, extensionData(critical: 0) + Data([1])))))
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(section(4, Data()) + field(Data()))))
    }

    func testSerialRangesAndBitmapRunsStayCompactAndRefuseWrapOrZeroSerial() throws {
        let serials = section(0x20, integer(4) + integer(2) + integer(3) + integer(3))
        let span = section(0x21, integer(5) + integer(UInt64.max))
        let parsed = try SSHRevokedHostKeys.parse(data: krl(certificateSection(serials + span)))
        XCTAssertEqual(parsed.policy.certificates.count, 1)
        XCTAssertEqual(parsed.policy.certificates[0].serialRanges, [try .init(first: 2, last: UInt64.max)])
        let zero = try certificate(serial: 0), maximum = try certificate(serial: UInt64.max)
        XCTAssertFalse(parsed.policy.denies(presented: try presented(zero), certificate: zero))
        XCTAssertTrue(parsed.policy.denies(presented: try presented(maximum), certificate: maximum))
        let bitmap = section(0x22, integer(100) + field(Data([0, 0xff, 0xff])))
        let run = try SSHRevokedHostKeys.parse(data: krl(certificateSection(bitmap)))
        XCTAssertEqual(run.policy.certificates[0].serialRanges, [try .init(first: 100, last: 115)])
        let empty = try SSHRevokedHostKeys.parse(data: krl(certificateSection(section(0x22, integer(0) + field(Data())))))
        XCTAssertTrue(empty.policy.isEmpty)
        for body in [section(0x20, integer(0)), section(0x21, integer(9) + integer(8)),
                     section(0x21, integer(1) + integer(2) + Data([0])),
                     section(0x22, integer(0) + field(Data([1]))),
                     section(0x22, integer(UInt64.max) + field(Data([2]))),
                     section(0x22, integer(1) + field(Data([0x80]))),
                     section(0x22, integer(1) + field(Data([0, 1]))),
                     section(0x22, integer(1) + field(Data([0]))) ] {
            XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(certificateSection(body))))
        }
    }

    func testKeyIDBytesAreNotUnicodeNormalizedAndEmbeddedNULIsNeverAccepted() throws {
        let opaque = Data([0xff, 0xfe, 65]), composed = Data("\u{e9}".utf8), decomposed = Data("e\u{301}".utf8)
        for keyID in [Data(), opaque, composed, decomposed] {
            let host = try certificate(serial: 0, keyID: keyID)
            XCTAssertEqual(host.keyID, keyID)
            let user = try SSHUserCertificate(blob: workspaceTestCertificate(keyID: keyID))
            XCTAssertEqual(user.keyID, keyID)
            let source = try SSHRevokedHostKeys.parse(data: krl(certificateSection(section(0x23, field(keyID)))))
            XCTAssertTrue(source.policy.denies(presented: try presented(host), certificate: host))
            let other = try certificate(serial: 0, keyID: keyID == composed ? decomposed : composed)
            XCTAssertFalse(source.policy.denies(presented: try presented(other), certificate: other))
        }
        let nul = Data([65, 0, 66])
        XCTAssertThrowsError(try certificate(keyID: nul))
        XCTAssertThrowsError(try SSHUserCertificate(blob: workspaceTestCertificate(keyID: nul)))
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(certificateSection(section(0x23, field(nul))))))
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: krl(certificateSection(section(0x23,
            field(Data(repeating: 65, count: Policy.maximumKeyIDBytes + 1)))))))
    }

    func testExactExplicitBlobAndSHA1WitnessSemanticsDoNotConfuseCertificateHashes() throws {
        let host = try certificate(), key = try presented(host.subject)
        let sha1 = Data(Insecure.SHA1.hash(data: host.subject.blob))
        let source = try SSHRevokedHostKeys.parse(data: krl(section(3, field(sha1))))
        XCTAssertTrue(source.policy.requiresRawIdentity)
        XCTAssertTrue(source.policy.denies(presented: key, rawIdentity: host.subject))
        XCTAssertFalse(source.policy.denies(presented: key), "absence of a raw witness cannot prove a SHA1 match")
        XCTAssertTrue(source.policy.denies(presented: try presented(host), certificate: host))
        let raw = try SSHRevokedHostKeys.parse(data: krl(section(2, field(host.subject.blob))))
        XCTAssertFalse(raw.policy.requiresRawIdentity)
        XCTAssertTrue(raw.policy.denies(presented: key))
        XCTAssertTrue(raw.policy.denies(presented: try presented(host), certificate: host))
        // The raw key section does not interpret an arbitrarily inserted cert
        // blob as a serial rule or as its subject: OpenSSH stores it verbatim.
        for type in [UInt8(2), 5] {
            let data = type == 2 ? host.blob : Data(SHA256.hash(data: host.blob))
            let certHash = try SSHRevokedHostKeys.parse(data: krl(section(type, field(data))))
            XCTAssertFalse(certHash.policy.denies(presented: try presented(host), certificate: host))
            XCTAssertFalse(certHash.policy.denies(presented: key, rawIdentity: host.subject))
        }
    }

    func testKeyListsRejectMalformedSuffixInsteadOfKeepingOnlyEarlierDenials() throws {
        let host = try certificate(), raw = host.subject.algorithm + " " + host.subject.blob.base64EncodedString()
        let valid = Data(("# public test source\n\t" + raw + " arbitrary comment\n").utf8)
        let source = try SSHRevokedHostKeys.parse(data: valid)
        XCTAssertTrue(source.policy.denies(presented: try presented(host.subject)))
        for suffix in ["not a public key\n", "ssh-ed25519 !!!\n", "ssh-rsa " + host.subject.blob.base64EncodedString() + "\n",
                       "command=\"ignored\" " + raw + "\n"] {
            XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: valid + Data(suffix.utf8)))
        }
        var forged = host.blob; forged[forged.count - 1] ^= 1
        XCTAssertThrowsError(try SSHRevokedHostKeys.parse(data: Data((host.algorithm + " " + forged.base64EncodedString()).utf8)))
        let certificateLine = try SSHRevokedHostKeys.parse(data: Data((host.algorithm + " " + host.blob.base64EncodedString()).utf8))
        XCTAssertEqual(certificateLine.policy, source.policy)
    }

    func testMonotonicUnionNormalizesIntervalsAndPreservesAllDenialDimensions() throws {
        let host = try certificate(serial: 8), issuer = host.authority.fingerprint
        let first = try Policy(revokedSHA256: [Data(repeating: 3, count: 32)], certificates: [
            .init(authoritySHA256: issuer, serialRanges: [.init(first: 4, last: 7)], keyIDs: [Data([66])])])
        let second = try Policy(revokedSHA1: [Data(repeating: 4, count: 20)], certificates: [
            .init(authoritySHA256: issuer, serialRanges: [.init(first: 8, last: UInt64.max)], keyIDs: [Data([65])]),
            .init(keyIDs: [Data([0xff])])])
        let union = try first.unioning(second)
        XCTAssertEqual(try union.unioning(first), union)
        XCTAssertEqual(try union.unioning(.empty), union); XCTAssertEqual(try Policy.empty.unioning(union), union)
        XCTAssertEqual(try second.unioning(first), union)
        XCTAssertEqual(union.revokedSHA256, first.revokedSHA256); XCTAssertEqual(union.revokedSHA1, second.revokedSHA1)
        XCTAssertEqual(union.certificates.first?.authoritySHA256, nil)
        XCTAssertEqual(union.certificates.last?.serialRanges, [try .init(first: 4, last: UInt64.max)])
        XCTAssertEqual(union.certificates.last?.keyIDs, [Data([65]), Data([66])])
        XCTAssertTrue(union.denies(presented: try presented(host), certificate: host))
        XCTAssertEqual(try JSONDecoder().decode(Policy.self, from: JSONEncoder().encode(union)), union)
    }

    func testNormalizedResourcesRejectMalformedBoundsAndContradictoryProvenanceShape() throws {
        XCTAssertThrowsError(try Policy(revokedSHA256: [Data(repeating: 0, count: 31)]))
        XCTAssertThrowsError(try Policy(revokedSHA1: [Data(repeating: 0, count: 21)]))
        XCTAssertThrowsError(try Policy(revokedSHA256: Array(repeating: Data(repeating: 0, count: 32), count: Policy.maximumEntries + 1)))
        let source = try SSHRevokedHostKeys.parse(data: krl(section(5, field(Data(repeating: 0, count: 32)))))
        let original = try source.json.object()
        for (field, value) in [("unexpected", JSONValue.boolean(true)), ("source_kind", .string("unknown")),
                               ("source_sha256", .string("invalid")), ("krl_version", .null),
                               ("revoked_sha1", .array([.string("AA==")])),
                               ("revoked_sha256", .array([.string(String(repeating: "A", count: 43))]))] {
            var fields = original; fields[field] = value
            XCTAssertThrowsError(try WorkspaceHostRevocationSource(json: .object(fields)), field)
        }
        var flat = original; flat["source_kind"] = .string("openssh_key_list_v1")
        XCTAssertThrowsError(try WorkspaceHostRevocationSource(json: .object(flat)))
        var malformedRange = original
        malformedRange["certificates"] = .array([.object(["serial_ranges": .array([.object([
            "first": .number("0"), "last": .number("18446744073709551615")])]), "key_ids": .array([])])])
        XCTAssertThrowsError(try WorkspaceHostRevocationSource(json: .object(malformedRange)))
    }
}
