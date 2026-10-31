import Foundation
import XCTest
@testable import FrankenTermKit

/// Independently signs exact host-certificate boundaries using the existing
/// test wire encoder. No production parser or certificate encoder is involved.
func workspaceTestHostCertificate(
    serial: UInt64 = 1, principals: [String] = ["host.example.invalid"],
    validAfter: UInt64 = 946_684_800, validBefore: UInt64 = 4_102_444_800,
    subjectSeed: Data = Data(0..<32), authoritySeed: Data = Data(32..<64),
    certificateType: UInt32 = 2, criticalOptions: [String: Data] = [:],
    extensionOptions: [String: Data] = [:], reserved: Data = Data()
) throws -> Data {
    try workspaceTestCertificate(serial: serial, principals: principals, validAfter: validAfter,
        validBefore: validBefore, extensions: [], subjectSeed: subjectSeed, authoritySeed: authoritySeed,
        criticalOptions: criticalOptions, extensionValues: extensionOptions,
        certificateType: certificateType, reserved: reserved)
}

/// Public OpenSSH-generated certificates exercise every admitted subject and
/// issuer family, including cross-family signing and both RSA SHA2 envelopes.
private struct HostCertificateFixtures: Decodable {
    struct Certificate: Decodable {
        let id: String, subjectAlgorithm: String, authorityAlgorithm: String, signatureAlgorithm: String
        let serial: UInt64, publicKey: String, authorityPublicKey: String, certificate: String
        var certificateBlob: Data { HostCertificateFixtures.blob(certificate) }
    }
    let validAfter: UInt64, validBefore: UInt64, principals: [String]
    let certificates: [Certificate], sha1Certificate: Certificate

    static func blob(_ line: String) -> Data { Data(base64Encoded: line.split(separator: " ")[1].description)! }
    static func load() throws -> Self {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Self.self,
            from: Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/ssh-host-certificates.json")))
    }
}

final class SSHHostCertificateTests: XCTestCase {
    private let now = Date(timeIntervalSince1970: 1_793_059_200)

    func testIndependentOpenSSHSubjectsAndIssuerSignaturesAcrossAllSupportedFamilies() throws {
        let fixtures = try HostCertificateFixtures.load()
        XCTAssertEqual(fixtures.certificates.count, 6)
        XCTAssertEqual(Set(fixtures.certificates.map(\.subjectAlgorithm)), SSHHostCertificate.supportedAuthorityAlgorithms)
        XCTAssertEqual(Set(fixtures.certificates.map(\.authorityAlgorithm)), SSHHostCertificate.supportedAuthorityAlgorithms)
        var admittedAlgorithms = Set<String>()
        for fixture in fixtures.certificates {
            let certificate = try SSHHostCertificate(blob: fixture.certificateBlob)
            admittedAlgorithms.insert(certificate.algorithm)
            XCTAssertEqual(certificate.blob, fixture.certificateBlob, fixture.id)
            XCTAssertEqual(certificate.subject, try SSHPublicIdentity(blob: HostCertificateFixtures.blob(fixture.publicKey)), fixture.id)
            XCTAssertEqual(certificate.authority, try SSHPublicIdentity(blob: HostCertificateFixtures.blob(fixture.authorityPublicKey)), fixture.id)
            XCTAssertEqual(certificate.subject.algorithm, fixture.subjectAlgorithm)
            XCTAssertEqual(certificate.authority.algorithm, fixture.authorityAlgorithm)
            XCTAssertEqual(certificate.signatureAlgorithm, fixture.signatureAlgorithm)
            XCTAssertEqual(certificate.serial, fixture.serial)
            XCTAssertEqual(certificate.principals, fixtures.principals)
            XCTAssertEqual(certificate.validAfter, fixtures.validAfter)
            XCTAssertEqual(certificate.validBefore, fixtures.validBefore)
            XCTAssertNoThrow(try certificate.validate(host: "host.example.invalid", now: now), fixture.id)
            XCTAssertNoThrow(try certificate.validate(host: "child.fleet.invalid", now: now), fixture.id)
            XCTAssertThrowsError(try certificate.validate(host: "unapproved.invalid", now: now), fixture.id)
        }
        XCTAssertEqual(admittedAlgorithms, SSHHostCertificate.supportedAlgorithms)
    }

    func testHostSignatureNegotiationBindsCertificateSubjectAndPermitsOnlyRSASHA2Aliases() throws {
        let fixtures = try HostCertificateFixtures.load()
        let possible = SSHHostCertificate.supportedAlgorithms.union(SSHHostCertificate.supportedAuthorityAlgorithms)
            .union(["rsa-sha2-256", "rsa-sha2-512", "rsa-sha2-256-cert-v01@openssh.com", "rsa-sha2-512-cert-v01@openssh.com"])
        for fixture in fixtures.certificates {
            let certificate = try SSHHostCertificate(blob: fixture.certificateBlob)
            let expected: Set<String> = certificate.subject.algorithm == "ssh-rsa"
                ? ["rsa-sha2-256-cert-v01@openssh.com", "rsa-sha2-512-cert-v01@openssh.com"]
                : [certificate.algorithm]
            for algorithm in possible {
                XCTAssertEqual(certificate.permitsHostSignature(algorithm), expected.contains(algorithm), "\(fixture.id): \(algorithm)")
            }
        }
    }

    func testIssuerSignaturesRejectSHA1WrongAlgorithmsAndNoncanonicalECDSAIntegers() throws {
        let fixtures = try HostCertificateFixtures.load()
        XCTAssertThrowsError(try SSHHostCertificate(blob: fixtures.sha1Certificate.certificateBlob))
        for fixture in fixtures.certificates {
            let envelope = try signatureEnvelope(fixture.certificateBlob)
            let wrongName = fixture.authorityAlgorithm == "ssh-ed25519" ? "ecdsa-sha2-nistp256" : "ssh-ed25519"
            for body in [sshString(Data(wrongName.utf8)) + sshString(envelope.payload),
                         sshString(Data(envelope.algorithm.utf8)) + sshString(envelope.payload) + Data([0]),
                         sshString(Data(envelope.algorithm.utf8)) + sshString(Data())] {
                XCTAssertThrowsError(try SSHHostCertificate(blob: envelope.prefix + sshString(body)), fixture.id)
            }
            if fixture.authorityAlgorithm.hasPrefix("ecdsa-") {
                var components = ByteCursor(envelope.payload)
                let r = try components.sshString(), s = try components.sshString()
                for invalidR in [Data([0]) + r, Data([0x80]), Data([0]), Data()] {
                    let invalidPayload = sshString(invalidR) + sshString(s)
                    let body = sshString(Data(envelope.algorithm.utf8)) + sshString(invalidPayload)
                    XCTAssertThrowsError(try SSHHostCertificate(blob: envelope.prefix + sshString(body)), fixture.id)
                }
            }
        }
    }

    func testEveryIndependentIssuerRejectsChangedSignedBytesAndSignatures() throws {
        for fixture in try HostCertificateFixtures.load().certificates {
            let blob = fixture.certificateBlob
            var reader = ByteCursor(blob)
            _ = try reader.sshString()
            var nonce = blob; nonce[reader.offset + 4] ^= 1
            var signature = blob; signature[signature.count - 1] ^= 1
            for invalid in [nonce, signature, Data(blob.dropLast()), blob + Data([0])] {
                XCTAssertThrowsError(try SSHHostCertificate(blob: invalid), fixture.id)
            }
        }
    }

    func testSignedSubjectAuthorityAndOriginalCertificateRemainDistinct() throws {
        let blob = try workspaceTestHostCertificate(serial: 42)
        let certificate = try SSHHostCertificate(blob: blob)
        XCTAssertEqual(certificate.blob, blob)
        XCTAssertEqual(certificate.algorithm, "ssh-ed25519-cert-v01@openssh.com")
        XCTAssertEqual(certificate.subject.algorithm, "ssh-ed25519")
        XCTAssertEqual(certificate.authority.algorithm, "ssh-ed25519")
        XCTAssertEqual(certificate.signatureAlgorithm, "ssh-ed25519")
        XCTAssertNotEqual(certificate.subject.fingerprint, certificate.authority.fingerprint)
        XCTAssertNotEqual(certificate.fingerprint, certificate.subject.fingerprint)
        XCTAssertNotEqual(certificate.fingerprint, certificate.authority.fingerprint)
        XCTAssertEqual(certificate.serial, 42)
        XCTAssertEqual(certificate.debugDescription, "SSHHostCertificate([redacted])")
        XCTAssertNoThrow(try certificate.validate(host: "host.example.invalid", now: now))
        // Data.SubSequence can retain a nonzero start index. Signature coverage
        // must still include exactly the original certificate prefix.
        let framed = Data([0, 1, 2]) + blob
        let slice = framed.dropFirst(3)
        XCTAssertNotEqual(slice.startIndex, 0)
        XCTAssertEqual(try SSHHostCertificate(blob: slice), certificate)
    }

    func testCertificateRolesCannotBeConfusedEvenWithAnAuthenticIssuerSignature() throws {
        for kind: UInt32 in [0, 1, 3, .max] {
            XCTAssertThrowsError(try SSHHostCertificate(blob: workspaceTestHostCertificate(certificateType: kind)))
        }
        XCTAssertThrowsError(try SSHUserCertificate(blob: workspaceTestHostCertificate()))
        XCTAssertNoThrow(try SSHUserCertificate(blob: workspaceTestCertificate()))
    }

    func testHostPrincipalsMatchOpenSSHCaseSensitiveByteWildcards() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(
            principals: ["host.example.invalid", "*.fleet.invalid", "node-?.site.invalid"]))
        for host in ["host.example.invalid", "one.fleet.invalid", "many.labels.fleet.invalid", ".fleet.invalid", "node-7.site.invalid"] {
            XCTAssertNoThrow(try certificate.validate(host: host, now: now), host)
        }
        for host in ["HOST.example.invalid", "fleet.invalid", "node-77.site.invalid", "prefixhost.example.invalid", "host.example.invalid.suffix"] {
            XCTAssertThrowsError(try certificate.validate(host: host, now: now), host)
        }
        let alias = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: ["approved-alias"]))
        XCTAssertNoThrow(try alias.validate(host: "approved-alias", now: now))
        XCTAssertThrowsError(try alias.validate(host: "host.example.invalid", now: now))
        XCTAssertThrowsError(try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: [])))
    }

    func testPrincipalPatternSyntaxDoesNotInheritKnownHostsListsOrUnicodeEquivalence() throws {
        for literal in ["[ab].invalid", "!host.invalid", "first.invalid,second.invalid", "slash\\name.invalid"] {
            let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: [literal]))
            XCTAssertNoThrow(try certificate.validate(host: literal, now: now))
            XCTAssertThrowsError(try certificate.validate(host: "host.invalid", now: now))
        }
        let decomposed = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: ["e\u{301}.invalid"]))
        XCTAssertNoThrow(try decomposed.validate(host: "e\u{301}.invalid", now: now))
        XCTAssertThrowsError(try decomposed.validate(host: "\u{e9}.invalid", now: now))
        let oneByte = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: ["?.invalid"]))
        let twoBytes = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: ["??.invalid"]))
        XCTAssertThrowsError(try oneByte.validate(host: "\u{e9}.invalid", now: now))
        XCTAssertNoThrow(try twoBytes.validate(host: "\u{e9}.invalid", now: now))
    }

    func testRepeatedStarPatternsAreBoundedAndHostnameLimitsCannotBeSkipped() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(
            principals: [String(repeating: "*a", count: 127) + "b"]))
        XCTAssertNoThrow(try certificate.validate(host: String(repeating: "a", count: 127) + "b", now: now))
        XCTAssertThrowsError(try certificate.validate(host: String(repeating: "a", count: 255) + "c", now: now))
        let wildcard = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: ["*"]))
        XCTAssertNoThrow(try wildcard.validate(host: String(repeating: "a", count: 256), now: nil))
        for invalid in ["", "line\nbreak", "nul\0byte", String(repeating: "a", count: 257)] {
            XCTAssertThrowsError(try wildcard.validate(host: invalid, now: nil))
        }
        for principals in [[""], ["bad\0principal"], [String(repeating: "a", count: 257)], ["duplicate", "duplicate"],
                           (0..<129).map { "host-\($0).invalid" }] {
            XCTAssertThrowsError(try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: principals)))
        }
        XCTAssertNoThrow(try SSHHostCertificate(blob: workspaceTestHostCertificate(
            principals: (0..<128).map { "host-\($0).invalid" })))
    }

    func testHalfOpenValidityAndEstablishedSessionClockExemptionRetainPrincipalChecks() throws {
        let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(validAfter: 100, validBefore: 200))
        XCTAssertThrowsError(try certificate.validateValidity(now: Date(timeIntervalSince1970: 99)))
        XCTAssertNoThrow(try certificate.validateValidity(now: Date(timeIntervalSince1970: 100)))
        XCTAssertNoThrow(try certificate.validateValidity(now: Date(timeIntervalSince1970: 199.9)))
        XCTAssertThrowsError(try certificate.validateValidity(now: Date(timeIntervalSince1970: 200)))
        XCTAssertThrowsError(try certificate.validate(host: "host.example.invalid", now: now))
        XCTAssertNoThrow(try certificate.validate(host: "host.example.invalid", now: nil))
        XCTAssertThrowsError(try certificate.validate(host: "wrong.invalid", now: nil))
        for seconds in [Double.infinity, -Double.infinity, Double.nan, -1, Double(UInt64.max)] {
            XCTAssertThrowsError(try certificate.validateValidity(now: Date(timeIntervalSince1970: seconds)))
        }
        XCTAssertThrowsError(try SSHHostCertificate(blob: workspaceTestHostCertificate(validAfter: 200, validBefore: 200)))
    }

    func testCriticalOptionsFailClosedAndSignedOptionalFieldsArePreserved() throws {
        for name in ["force-command", "source-address", "future-critical@example.invalid"] {
            XCTAssertThrowsError(try SSHHostCertificate(blob: workspaceTestHostCertificate(criticalOptions: [name: Data()])))
        }
        let options = ["future-optional@example.invalid": Data([0, 255, 10]), "permit-pty": Data([1])]
        let blob = try workspaceTestHostCertificate(extensionOptions: options, reserved: Data([0, 255, 42]))
        let certificate = try SSHHostCertificate(blob: blob)
        XCTAssertEqual(certificate.blob, blob)
        XCTAssertEqual(certificate.extensionOptions, options)
        XCTAssertTrue(certificate.criticalOptions.isEmpty)
        XCTAssertNoThrow(try certificate.validate(host: "host.example.invalid", now: now))
        // The user profile retains its existing stricter flag/reserved rules.
        XCTAssertThrowsError(try SSHUserCertificate(blob: workspaceTestCertificate(extensionValues: ["permit-pty": Data([1])])))
        XCTAssertThrowsError(try SSHUserCertificate(blob: workspaceTestCertificate(reserved: Data([1]))))
    }

    func testTamperedSignedPrefixSignaturesAndFramingAreRejected() throws {
        let blob = try workspaceTestHostCertificate()
        var reader = ByteCursor(blob)
        _ = try reader.sshString()
        var changedNonce = blob
        changedNonce[reader.offset + 4] ^= 1
        var changedSignature = blob
        changedSignature[changedSignature.count - 1] ^= 1
        for invalid in [changedNonce, changedSignature, Data(blob.dropLast()), blob + Data([0]), Data(),
                        Data(repeating: 0, count: SSHHostCertificate.maximumBlobBytes + 1)] {
            XCTAssertThrowsError(try SSHHostCertificate(blob: invalid))
        }
        let certificate = try SSHHostCertificate(blob: blob)
        XCTAssertThrowsError(try SSHHostCertificate(blob: certificate.subject.blob))
        XCTAssertTrue(certificate.permitsHostSignature(certificate.algorithm))
        for algorithm in ["ssh-ed25519", "ssh-rsa-cert-v01@openssh.com", "rsa-sha2-256-cert-v01@openssh.com"] {
            XCTAssertFalse(certificate.permitsHostSignature(algorithm))
        }
    }

    private func sshString(_ bytes: Data) -> Data {
        var value = Data(); value.appendBE(UInt64(bytes.count), bytes: 4); value.append(bytes); return value
    }

    /// Locate only the issuer envelope for adversarial mutations. Test fixtures
    /// remain independently signed by OpenSSH; no product validator is reused.
    private func signatureEnvelope(_ blob: Data) throws -> (prefix: Data, algorithm: String, payload: Data) {
        var reader = ByteCursor(blob)
        let algorithm = String(decoding: try reader.sshString(), as: UTF8.self)
        _ = try reader.sshString() // nonce
        for _ in 0..<(algorithm.hasPrefix("ssh-ed25519-") ? 1 : 2) { _ = try reader.sshString() }
        _ = try reader.uint(8); _ = try reader.uint(4)
        _ = try reader.sshString(); _ = try reader.sshString()
        _ = try reader.uint(8); _ = try reader.uint(8)
        for _ in 0..<4 { _ = try reader.sshString() } // options, extensions, reserved, CA
        let prefix = Data(blob.prefix(reader.offset))
        var signature = ByteCursor(try reader.sshString())
        return (prefix, String(decoding: try signature.sshString(), as: UTF8.self), try signature.sshString())
    }
}
