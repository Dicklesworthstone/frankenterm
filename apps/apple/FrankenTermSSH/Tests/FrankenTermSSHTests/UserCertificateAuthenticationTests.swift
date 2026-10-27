import Foundation
import XCTest
@testable import FrankenTermKit
@testable import FrankenTermSSH
@testable import FrankenTermSSHIntegration
#if canImport(CryptoKit)
import CryptoKit
#else
import Crypto
#endif

@MainActor
final class UserCertificateAuthenticationTests: XCTestCase {
    /// Independent OpenSSH certificates and pyca authentication signatures. The
    /// fixture contains public synthetic private keys, never user credentials.
    private struct Files: Decodable {
        struct Certificate: Decodable {
            let id, subjectAlgorithm, privatePEM, publicKey, certificate: String
            let authenticationAlgorithm, signedData, signature: String
            var publicBlob: Data { Data(base64Encoded: String(publicKey.split(separator: " ")[1]))! }
            var certificateBlob: Data { Data(base64Encoded: String(certificate.split(separator: " ")[1]))! }
        }
        let user, sessionIdentifierHex: String
        let certificates: [Certificate]
        let largeCertificate: Certificate
        static func load() throws -> Self {
            var root = URL(fileURLWithPath: #filePath)
            for _ in 0..<6 { root.deleteLastPathComponent() }
            return try JSONDecoder().decode(Self.self,
                from: Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/ssh-user-certificates.json")))
        }
    }

    private func fixture(_ certificate: Files.Certificate) async throws -> SSHWorkspaceTestFixture {
        let record = try PrivateKeyRecord.fromPEM(Data(certificate.privatePEM.utf8), credentialID: "key", using: SSHWorkspaceTestCryptography())
        return try await .make(primaryKey: record) { workspace in
            var credentials = workspace["credentials"] as! [[String: Any]]
            credentials[0]["user_certificate"] = certificate.certificateBlob.base64EncodedString()
            credentials[0]["required_capabilities"] = [WorkspaceDocument.userCertificateCapability]
            workspace["credentials"] = credentials
        }
    }

    private func verifyECDSA(_ signature: Data, challenge: SSHUserAuthenticationChallenge) throws -> Bool {
        let curve = try XCTUnwrap(SSHECDSACurve.allCases.first { $0.algorithm == challenge.algorithm })
        var envelope = ByteCursor(signature)
        XCTAssertEqual(try envelope.sshString(cap: 64), Data(challenge.algorithm.utf8))
        var payload = try ByteCursor(envelope.sshString())
        let r = try payload.sshScalar(curve: curve), s = try payload.sshScalar(curve: curve)
        XCTAssertEqual(envelope.remaining, 0); XCTAssertEqual(payload.remaining, 0)
        var publicKey = ByteCursor(challenge.subjectPublicBlob)
        _ = try publicKey.sshString(); _ = try publicKey.sshString()
        let point = try publicKey.sshString()
        switch curve {
        case .nistp256:
            return try P256.Signing.PublicKey(x963Representation: point)
                .isValidSignature(P256.Signing.ECDSASignature(rawRepresentation: r + s), for: challenge.signedData)
        case .nistp384:
            return try P384.Signing.PublicKey(x963Representation: point)
                .isValidSignature(P384.Signing.ECDSASignature(rawRepresentation: r + s), for: challenge.signedData)
        case .nistp521:
            return try P521.Signing.PublicKey(x963Representation: point)
                .isValidSignature(P521.Signing.ECDSASignature(rawRepresentation: r + s), for: challenge.signedData)
        }
    }

    func testEveryCertificateFamilyOffersExactCertificateAndSignsWithRawSubject() async throws {
        let files = try Files.load(), session = try Data(ftHex: files.sessionIdentifierHex)
        XCTAssertTrue(WorkspaceNativeSSHAuthentication.routeSupport.capabilities.contains(WorkspaceDocument.userCertificateCapability))
        for certificate in files.certificates + [files.largeCertificate] {
            let f = try await fixture(certificate), transport = UUID()
            let authority = f.authority(transport: transport), peer = f.peer(transport: transport)
            let identity = try await authority.identity(for: peer)
            XCTAssertEqual(identity.publicKey, certificate.certificateBlob, certificate.id)
            XCTAssertNotEqual(identity.publicKey, certificate.publicBlob, "a certificate reference must never become a raw-key offer")
            let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: session, user: files.user,
                publicBlob: identity.publicKey, signatureAlgorithm: certificate.authenticationAlgorithm)
            XCTAssertEqual(challenge.signedData, Data(base64Encoded: certificate.signedData), certificate.id)
            let native = try NativeSSHChallenge(bytes: challenge.signedData, transportID: transport, identity: identity)
            XCTAssertEqual(native.algorithm, certificate.authenticationAlgorithm)
            XCTAssertEqual(native.publicKeyAlgorithm, certificate.subjectAlgorithm)
            let signature = try await authority.signature(for: native, peer: peer)
            if certificate.subjectAlgorithm.hasPrefix("ecdsa-") {
                XCTAssertTrue(try verifyECDSA(signature, challenge: challenge), certificate.id)
            } else {
                // Ed25519 and PKCS#1 v1.5 RSA signatures are deterministic;
                // compare every byte with the independently produced oracle.
                XCTAssertEqual(signature, Data(base64Encoded: certificate.signature), certificate.id)
            }
            try await authority.validateForAuthentication(peer: peer, identity: identity)
            try await authority.validate(peer: peer, identity: identity)
            try await authority.validateCurrent()
            XCTAssertEqual(f.vault.counts.signatures, 1)
            let raw = try SSHPublicIdentity(blob: certificate.publicBlob)
            try await f.store.revokeLocally(fingerprint: raw.fingerprint)
            do { try await authority.validateCurrent(); XCTFail("certificate outlived revoked subject key") } catch {}
        }
    }

    func testNativeChallengeRejectsRawKeySubstitutionAndCertificateAlgorithmDowngrade() throws {
        let files = try Files.load(), session = try Data(ftHex: files.sessionIdentifierHex)
        for certificate in files.certificates + [files.largeCertificate] {
            let identity = try NativeSSHIdentity(user: files.user, publicKey: certificate.certificateBlob)
            let rawSignature = certificate.subjectAlgorithm == "ssh-rsa" ? "rsa-sha2-512" : certificate.subjectAlgorithm
            let raw = try SSHUserAuthenticationChallenge(sessionIdentifier: session, user: files.user,
                publicBlob: certificate.publicBlob, signatureAlgorithm: rawSignature)
            XCTAssertThrowsError(try NativeSSHChallenge(bytes: raw.signedData, transportID: UUID(), identity: identity))
            func bytes(algorithm: String, publicBlob: Data) -> Data {
                var value = SSHUserAuthenticationChallenge.string(session); value.append(50)
                for field in [files.user, "ssh-connection", "publickey"] {
                    value.append(SSHUserAuthenticationChallenge.string(Data(field.utf8)))
                }
                value.append(1)
                value.append(SSHUserAuthenticationChallenge.string(Data(algorithm.utf8)))
                value.append(SSHUserAuthenticationChallenge.string(publicBlob))
                return value
            }
            XCTAssertThrowsError(try NativeSSHChallenge(bytes: bytes(algorithm: rawSignature, publicBlob: certificate.certificateBlob),
                transportID: UUID(), identity: identity))
            if certificate.subjectAlgorithm == "ssh-rsa" {
                XCTAssertThrowsError(try NativeSSHChallenge(bytes: bytes(algorithm: "ssh-rsa-cert-v01@openssh.com", publicBlob: certificate.certificateBlob),
                    transportID: UUID(), identity: identity))
            }
            let proper = bytes(algorithm: certificate.authenticationAlgorithm, publicBlob: certificate.certificateBlob)
            XCTAssertNoThrow(try NativeSSHChallenge(bytes: proper, transportID: UUID(), identity: identity))
            var changed = certificate.certificateBlob; changed[changed.count - 1] ^= 1
            XCTAssertThrowsError(try NativeSSHChallenge(bytes: bytes(algorithm: certificate.authenticationAlgorithm, publicBlob: changed),
                transportID: UUID(), identity: identity))
        }
    }
}
