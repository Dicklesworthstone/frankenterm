import Foundation
import XCTest
@testable import FrankenTermKit

/// Public-only, independently generated OpenSSH fixtures. Expected decisions
/// were obtained from ssh-keygen -Q or ssh-keygen -Y verify -r, not this parser.
/// The latter calls the same flat-list revocation checker used by ssh(1), and
/// each signature/signer baseline succeeded before a revocation file was added.
private struct HostRevocationOracleFixtures: Decodable {
    struct Target: Decodable {
        let id: String, publicKey: String, subjectID: String
        let isCertificate: Bool
        let authorityID: String?, serial: UInt64?, keyID: String?

        var blob: Data {
            get throws {
                let fields = publicKey.split(separator: " ")
                return try XCTUnwrap(fields.count >= 2 ? Data(base64Encoded: String(fields[1])) : nil)
            }
        }

        func evidence() throws -> (WorkspaceDocument.HostKey, SSHPublicIdentity?, SSHHostCertificate?) {
            if isCertificate {
                let certificate = try SSHHostCertificate(blob: blob)
                return (try .init(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint), nil, certificate)
            }
            let identity = try SSHPublicIdentity(blob: blob)
            return (try .init(algorithm: identity.algorithm, fingerprint: identity.fingerprint), identity, nil)
        }
    }

    struct Source: Decodable {
        let id: String, format: String, data: String, oracle: String
        let sections: [UInt8], certificateSections: [UInt8], expectedDenied: [String]

        func parsed() throws -> WorkspaceHostRevocationSource {
            try SSHRevokedHostKeys.parse(data: XCTUnwrap(Data(base64Encoded: data)), id: id)
        }
    }

    let schemaVersion: Int
    let targets: [Target], sources: [Source]

    static func load() throws -> Self {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Self.self, from: Data(contentsOf:
            root.appendingPathComponent("fixtures/ios-airgap/ssh-host-revocations.json")))
    }

    func source(_ id: String) throws -> Source { try XCTUnwrap(sources.first { $0.id == id }) }

    func check(_ source: Source, policy: WorkspaceHostRevocationPolicy? = nil,
               file: StaticString = #filePath, line: UInt = #line) throws {
        let selected = try policy ?? source.parsed().policy
        let denied = Set(source.expectedDenied)
        for target in targets {
            let (presented, raw, certificate) = try target.evidence()
            XCTAssertEqual(selected.denies(presented: presented, rawIdentity: raw, certificate: certificate),
                denied.contains(target.id), "\(source.id): \(target.id)", file: file, line: line)
        }
    }
}

final class SSHHostRevocationOracleTests: XCTestCase {
    func testOpenSSHExplicitAndFingerprintSectionsRevokeRawSubjectsAndIssuers() throws {
        let fixtures = try HostRevocationOracleFixtures.load()
        XCTAssertEqual(fixtures.schemaVersion, 1)
        for (prefix, section) in [("key", UInt8(2)), ("sha1", UInt8(3)), ("sha256", UInt8(5))] {
            for identity in ["subject-a", "ca-a"] {
                let source = try fixtures.source(prefix + "-" + identity)
                XCTAssertEqual(source.sections, [section])
                XCTAssertTrue(source.expectedDenied.contains(identity))
                XCTAssertTrue(source.expectedDenied.contains("serial-100"))
                XCTAssertFalse(source.expectedDenied.contains("ca-b"))
                try fixtures.check(source)
            }
        }
    }

    func testOpenSSHSerialListRangeAndBitmapPreserveBoundariesWithoutExpansion() throws {
        let fixtures = try HostRevocationOracleFixtures.load()
        for (id, tag) in [("serial-list", UInt8(0x20)), ("serial-range", UInt8(0x21)), ("serial-bitmap", UInt8(0x22))] {
            let source = try fixtures.source(id)
            XCTAssertTrue(source.certificateSections.contains(tag), "OpenSSH must actually emit \(id)'s representation")
            XCTAssertFalse(source.expectedDenied.contains("serial-0"))
            XCTAssertFalse(source.expectedDenied.contains("other-ca"))
            XCTAssertFalse(source.expectedDenied.contains("subject-a"))
            try fixtures.check(source)
        }
        let range = try fixtures.source("serial-range")
        XCTAssertLessThan(try XCTUnwrap(Data(base64Encoded: range.data)).count, 256)
        XCTAssertFalse(range.expectedDenied.contains("serial-999"))
        XCTAssertTrue(range.expectedDenied.contains("serial-1000"))
        XCTAssertTrue(range.expectedDenied.contains("serial-18446744073709551615"))
        let bitmap = try fixtures.source("serial-bitmap")
        XCTAssertTrue(bitmap.expectedDenied.contains("serial-100"))
        XCTAssertFalse(bitmap.expectedDenied.contains("serial-101"))
        XCTAssertTrue(bitmap.expectedDenied.contains("serial-111"))
        XCTAssertFalse(bitmap.expectedDenied.contains("serial-112"))
    }

    func testOpenSSHKeyIDsRetainExactOpaqueBytesAndWildcardAuthority() throws {
        let fixtures = try HostRevocationOracleFixtures.load()
        for target in fixtures.targets where target.isCertificate {
            let certificate = try SSHHostCertificate(blob: target.blob)
            XCTAssertEqual(certificate.keyID, try XCTUnwrap(target.keyID.flatMap { Data(base64Encoded: $0) }), target.id)
            XCTAssertEqual(certificate.serial, try XCTUnwrap(target.serial), target.id)
        }
        for id in ["opaque-ca-id", "opaque-any-ca-id", "composed-id", "empty-id"] {
            try fixtures.check(fixtures.source(id))
        }
        let scoped = try fixtures.source("opaque-ca-id"), wildcard = try fixtures.source("opaque-any-ca-id")
        XCTAssertTrue(scoped.expectedDenied.contains("opaque-id-a"))
        XCTAssertFalse(scoped.expectedDenied.contains("opaque-id-b"))
        XCTAssertTrue(wildcard.expectedDenied.contains("opaque-id-a"))
        XCTAssertTrue(wildcard.expectedDenied.contains("opaque-id-b"))
        XCTAssertFalse(wildcard.expectedDenied.contains("different-id"))
        let composed = try fixtures.source("composed-id")
        XCTAssertTrue(composed.expectedDenied.contains("composed-id"))
        XCTAssertFalse(composed.expectedDenied.contains("decomposed-id"))
        XCTAssertEqual(try fixtures.source("empty-id").expectedDenied, ["empty-id"])
    }

    func testPlainPublicKeyListsUseSubjectAndIssuerSemanticsIncludingCertificateLines() throws {
        let fixtures = try HostRevocationOracleFixtures.load()
        for id in ["plain-subject", "plain-authority", "plain-certificate-subject"] {
            let source = try fixtures.source(id)
            XCTAssertEqual(source.format, "public-key-list")
            XCTAssertTrue(source.oracle.hasPrefix("ssh-keygen -Y verify -r"))
            XCTAssertFalse(source.expectedDenied.contains("subject-b"))
            try fixtures.check(source)
        }
        XCTAssertEqual(try fixtures.source("plain-subject").expectedDenied,
                       try fixtures.source("plain-certificate-subject").expectedDenied)
        XCTAssertTrue(try fixtures.source("plain-authority").expectedDenied.contains("other-subject"))
        XCTAssertFalse(try fixtures.source("plain-authority").expectedDenied.contains("other-ca"))
    }

    func testNormalizedManifestRoundTripAndUnionRetainIndependentOracleDenials() throws {
        let fixtures = try HostRevocationOracleFixtures.load()
        XCTAssertEqual(fixtures.targets.count, 27)
        XCTAssertEqual(fixtures.sources.count, 16)
        for source in fixtures.sources {
            let parsed = try source.parsed()
            let restored = try WorkspaceHostRevocationSource(json: parsed.json)
            XCTAssertEqual(restored.id, source.id)
            try fixtures.check(source, policy: restored.policy)
        }
        var combined = WorkspaceHostRevocationPolicy.empty
        var expected = Set<String>()
        for id in ["serial-bitmap", "sha1-ca-a", "opaque-any-ca-id"] {
            let source = try fixtures.source(id)
            combined = try combined.unioning(source.parsed().policy)
            expected.formUnion(source.expectedDenied)
        }
        for target in fixtures.targets {
            let (presented, raw, certificate) = try target.evidence()
            XCTAssertEqual(combined.denies(presented: presented, rawIdentity: raw, certificate: certificate),
                expected.contains(target.id), target.id)
        }
    }
}
