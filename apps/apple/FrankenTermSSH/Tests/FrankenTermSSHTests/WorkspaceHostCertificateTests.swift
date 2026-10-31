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

private actor CertificateWorkspacePrompt: WorkspaceSSHPasswordPrompting, WorkspaceSSHKeyboardInteractivePrompting {
    private(set) var certificates: [SSHHostCertificate?] = []
    func password(for request: WorkspaceSSHPasswordRequest) async throws -> Data {
        certificates.append(request.authorization.hostCertificate)
        return Data("synthetic password".utf8)
    }
    func responses(for request: WorkspaceSSHKeyboardInteractiveRequest) async throws -> [Data] {
        certificates.append(request.authorization.hostCertificate)
        return [Data("synthetic code".utf8)]
    }
    nonisolated func cancel(requestID: UUID) {}
}

@MainActor
final class WorkspaceHostCertificateTests: XCTestCase {
    private func integer<T: FixedWidthInteger>(_ value: T) -> Data {
        var value = value.bigEndian
        return withUnsafeBytes(of: &value) { Data($0) }
    }
    private func field(_ bytes: Data) -> Data { integer(UInt32(bytes.count)) + bytes }
    private func text(_ value: String) -> Data { field(Data(value.utf8)) }

    /// The signing seed belongs only to this deterministic synthetic issuer.
    /// The complete certificate is signed by Crypto, then parsed and verified
    /// by production certificate code before it enters the broker fixture.
    private func certificate(principal: String = "localhost", authoritySeed: UInt8 = 91,
                             validity: ClosedRange<UInt64>? = nil) throws -> SSHHostCertificate {
        let issuer = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(repeating: authoritySeed, count: 32))
        let subject = try Curve25519.Signing.PrivateKey(rawRepresentation: Data(repeating: 92, count: 32))
        let issuerBlob = text("ssh-ed25519") + field(issuer.publicKey.rawRepresentation)
        let now = UInt64(Date().timeIntervalSince1970)
        let validity = validity ?? ((now - 60)...(now + 3600))
        var bytes = text("ssh-ed25519-cert-v01@openssh.com") + field(Data(repeating: 3, count: 16))
        bytes += field(subject.publicKey.rawRepresentation)
        bytes += integer(UInt64(7)) + integer(UInt32(2)) + text("synthetic host")
        bytes += field(text(principal)) + integer(validity.lowerBound) + integer(validity.upperBound)
        bytes += field(Data()) + field(Data()) + field(Data()) + field(issuerBlob)
        bytes += field(text("ssh-ed25519") + field(try issuer.signature(for: bytes)))
        return try SSHHostCertificate(blob: bytes)
    }

    private func fixture(_ certificate: SSHHostCertificate, method: String = "publickey", mixed: Bool = false,
                         trustedAuthority: SSHPublicIdentity? = nil) async throws -> SSHWorkspaceTestFixture {
        guard NativeSSHSession.supportsHostCertificates else {
            throw XCTSkip("Requires the versioned host-certificate KEX backend")
        }
        let authority = trustedAuthority ?? certificate.authority
        return try await SSHWorkspaceTestFixture.make(hostBlob: certificate.subject.blob,
            requiredCapabilities: [WorkspaceDocument.hostCertificateCapability]) { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            if !mixed { trusts[0]["pins"] = [] as [[String: Any]] }
            trusts[0]["certificate_authorities"] = [["algorithm": authority.algorithm, "fingerprint": authority.fingerprint]]
            workspace["host_trust"] = trusts
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["authentication_methods"] = [method]
            if method != "publickey" { routes[0]["credential_refs"] = [] as [String] }
            workspace["routes"] = routes
        }
    }

    private func peer(_ fixture: SSHWorkspaceTestFixture, certificate: SSHHostCertificate,
                      transport: UUID) -> NativeSSHPeer {
        fixture.peer(transport: transport, hostKey: certificate.blob, signature: certificate.algorithm)
    }

    private func refuse(_ body: () async throws -> Void, file: StaticString = #filePath, line: UInt = #line) async {
        do { try await body(); XCTFail("Untrusted certificate authorized authentication", file: file, line: line) }
        catch {}
    }

    private func assertWrongIssuerEvidence(_ error: any Error, fixture: SSHWorkspaceTestFixture,
                                          certificate: SSHHostCertificate, issuer: SSHPublicIdentity) throws {
        let mismatch = try XCTUnwrap(error as? WorkspaceSSHHostKeyMismatch)
        XCTAssertEqual(mismatch.workspaceID, fixture.installed.id)
        XCTAssertEqual(mismatch.expectedRevision, fixture.installed.revision)
        XCTAssertEqual(mismatch.routeID, "route")
        XCTAssertNil(mismatch.transitForRouteID)
        XCTAssertEqual(mismatch.host, fixture.endpoint.host)
        XCTAssertEqual(mismatch.port, fixture.endpoint.port)
        XCTAssertEqual(mismatch.certificatePrincipal, "localhost")
        XCTAssertEqual(mismatch.hostCertificate, certificate)
        XCTAssertEqual(mismatch.presentedKey,
            try .init(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint))
        XCTAssertEqual(mismatch.expectedCertificateAuthorities,
            [try .init(algorithm: issuer.algorithm, fingerprint: issuer.fingerprint)])
        XCTAssertEqual(mismatch.expectedKeys, fixture.archive.workspace.hostTrust[0].pins)
        XCTAssertFalse(mismatch.description.contains(certificate.fingerprint))
    }

    func testRoutePolicyDistinguishesRawCAOnlyAndMixedTrust() async throws {
        let plain = try await SSHWorkspaceTestFixture.make()
        XCTAssertEqual(try WorkspaceNativeSSHAuthentication.hostKeyPolicy(for: plain.archive.workspace.routes[0],
            document: plain.archive.workspace), .rawKeys)
        let certificate = try certificate()
        for mixed in [false, true] {
            let fixture = try await fixture(certificate, mixed: mixed)
            XCTAssertEqual(try WorkspaceNativeSSHAuthentication.hostKeyPolicy(for: fixture.archive.workspace.routes[0],
                document: fixture.archive.workspace), mixed ? .certificatesAndRawKeys : .certificates)
        }
    }

    func testCertificateRemainsExactThroughProtectedSigningAndCARevocation() async throws {
        let certificate = try certificate(), fixture = try await fixture(certificate), transport = UUID()
        let peer = peer(fixture, certificate: certificate, transport: transport)
        let authority = fixture.authority(transport: transport)
        let identity = try await authority.identity(for: peer)
        let host = try WorkspaceNativeSSHAuthentication.hostIdentity(for: peer)
        XCTAssertEqual(host.certificate, certificate)
        XCTAssertNil(host.rawIdentity)
        XCTAssertEqual(host.key.fingerprint, certificate.fingerprint)
        XCTAssertNotEqual(host.key.fingerprint, certificate.subject.fingerprint)
        let grant = try await fixture.broker.authorization(workspaceID: fixture.installed.id,
            expectedRevision: fixture.installed.revision, routeID: "route", credentialID: "key",
            presentedHostKey: host.key, hostCertificate: host.certificate)
        XCTAssertEqual(grant.hostCertificate, certificate)
        let typed = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 7, count: 32),
            user: identity.user, publicBlob: identity.publicKey)
        let challenge = try NativeSSHChallenge(bytes: typed.signedData, transportID: transport, identity: identity)
        let signature = try await authority.signature(for: challenge, peer: peer)
        let publicKey = try Curve25519.Signing.PublicKey(rawRepresentation: fixture.publicBlob.suffix(32))
        XCTAssertTrue(publicKey.isValidSignature(signature.suffix(64), for: typed.signedData))
        try await authority.validateCurrent()
        XCTAssertEqual(fixture.vault.counts.reads, 1)
        XCTAssertEqual(fixture.vault.counts.signatures, 1)

        let connections = WorkspaceSSHConnections(store: fixture.store, broker: fixture.broker,
            journal: try TerminalSessionJournal(persistence: SSHWorkspaceTestPersistence()),
            coordinator: try TerminalInputCoordinator())
        _ = try await connections.revokeHostLocally(workspaceID: fixture.installed.id, trustID: "trust",
            key: .init(algorithm: certificate.authority.algorithm, fingerprint: certificate.authority.fingerprint),
            confirmedByUser: true)
        await refuse { try await authority.validateCurrent() }
    }

    func testPasswordAndInteractivePromptsRetainCertificateAndRevalidateCA() async throws {
        let certificate = try certificate()
        for method in ["password", "keyboard-interactive"] {
            let fixture = try await fixture(certificate, method: method), transport = UUID()
            let peer = peer(fixture, certificate: certificate, transport: transport), prompt = CertificateWorkspacePrompt()
            if method == "password" {
                let authority = WorkspaceNativeSSHPasswordAuthentication(broker: fixture.broker, prompter: prompt,
                    workspaceID: fixture.installed.id, revision: fixture.installed.revision, routeID: "route",
                    transportID: transport, endpoint: fixture.endpoint)
                let account = try await authority.account(for: peer)
                _ = try await authority.password(for: account, peer: peer)
                try await authority.authenticationCompleted()
                try await authority.validateCurrent()
                try await fixture.store.revokeHostLocally(workspaceID: fixture.installed.id, trustID: "trust",
                    key: .init(algorithm: certificate.authority.algorithm, fingerprint: certificate.authority.fingerprint))
                await refuse { try await authority.validateCurrent() }
            } else {
                let authority = WorkspaceNativeSSHInteractiveAuthentication(broker: fixture.broker, prompter: prompt,
                    workspaceID: fixture.installed.id, revision: fixture.installed.revision, routeID: "route",
                    transportID: transport, endpoint: fixture.endpoint)
                let account = try await authority.account(for: peer)
                let bytes = text("Sign in") + text("Confirm access") + integer(UInt32(1)) + Data([0]) + text("Code:")
                let challenge = try NativeSSHInteractiveChallenge(bytes: bytes, round: 1, transportID: transport, account: account)
                _ = try await authority.responses(for: challenge, peer: peer)
                try await authority.authenticationCompleted()
                try await authority.validateCurrent()
                try await fixture.store.revokeHostLocally(workspaceID: fixture.installed.id, trustID: "trust",
                    key: .init(algorithm: certificate.authority.algorithm, fingerprint: certificate.authority.fingerprint))
                await refuse { try await authority.validateCurrent() }
            }
            let seen = await prompt.certificates
            XCTAssertEqual(seen, [certificate])
            XCTAssertEqual(fixture.vault.counts.reads, 0)
            XCTAssertEqual(fixture.vault.counts.signatures, 0)
        }
    }

    func testWrongPrincipalExpiredAndWrongIssuerFailBeforeProtectedKeyRead() async throws {
        let now = UInt64(Date().timeIntervalSince1970), issuer = try certificate().authority
        let invalid = [try certificate(principal: "other.invalid"), try certificate(validity: 1...(now - 60)),
                       try certificate(authoritySeed: 93)]
        for certificate in invalid {
            // The raw subject remains pinned in this mixed profile. A bad
            // certificate cannot be retried as that otherwise trusted raw key.
            let fixture = try await fixture(certificate, mixed: true, trustedAuthority: issuer), transport = UUID()
            do {
                _ = try await fixture.authority(transport: transport).identity(
                    for: peer(fixture, certificate: certificate, transport: transport))
                XCTFail("Untrusted certificate authorized a protected identity read")
            } catch {
                if certificate.authority != issuer {
                    try assertWrongIssuerEvidence(error, fixture: fixture, certificate: certificate, issuer: issuer)
                } else {
                    XCTAssertFalse(error is WorkspaceSSHHostKeyMismatch,
                        "Invalid names and validity must not become a certificate replacement review")
                }
            }
            XCTAssertEqual(fixture.vault.counts.reads, 0)
            XCTAssertEqual(fixture.vault.counts.signatures, 0)
        }
    }

    func testInvalidCertificateNeverPromptsPasswordOrInteractiveUser() async throws {
        let issuer = try certificate().authority
        for certificate in [try certificate(principal: "other.invalid"), try certificate(authoritySeed: 93)] {
            for method in ["password", "keyboard-interactive"] {
                let fixture = try await fixture(certificate, method: method, trustedAuthority: issuer), transport = UUID()
                let peer = peer(fixture, certificate: certificate, transport: transport), prompt = CertificateWorkspacePrompt()
                do {
                    if method == "password" {
                        let authority = WorkspaceNativeSSHPasswordAuthentication(broker: fixture.broker, prompter: prompt,
                            workspaceID: fixture.installed.id, revision: fixture.installed.revision, routeID: "route",
                            transportID: transport, endpoint: fixture.endpoint)
                        _ = try await authority.account(for: peer)
                    } else {
                        let authority = WorkspaceNativeSSHInteractiveAuthentication(broker: fixture.broker, prompter: prompt,
                            workspaceID: fixture.installed.id, revision: fixture.installed.revision, routeID: "route",
                            transportID: transport, endpoint: fixture.endpoint)
                        _ = try await authority.account(for: peer)
                    }
                    XCTFail("Untrusted certificate authorized a credential prompt")
                } catch {
                    if certificate.authority != issuer {
                        try assertWrongIssuerEvidence(error, fixture: fixture, certificate: certificate, issuer: issuer)
                    } else {
                        XCTAssertFalse(error is WorkspaceSSHHostKeyMismatch)
                    }
                }
                let seen = await prompt.certificates
                XCTAssertTrue(seen.isEmpty)
                XCTAssertEqual(fixture.vault.counts.reads, 0)
                XCTAssertEqual(fixture.vault.counts.signatures, 0)
            }
        }
    }

    func testCertificateAndNegotiatedAlgorithmCannotBeSubstituted() async throws {
        let certificate = try certificate(), fixture = try await fixture(certificate), transport = UUID()
        XCTAssertThrowsError(try WorkspaceNativeSSHAuthentication.hostIdentity(for: fixture.peer(transport: transport,
            hostKey: certificate.blob, signature: "ssh-ed25519")))
        XCTAssertThrowsError(try WorkspaceNativeSSHAuthentication.hostIdentity(for: fixture.peer(transport: transport,
            hostKey: certificate.subject.blob, signature: certificate.algorithm)))
        var corrupted = certificate.blob
        corrupted[corrupted.count - 1] ^= 1
        let authority = fixture.authority(transport: transport)
        await refuse { _ = try await authority.identity(for: fixture.peer(transport: transport,
            hostKey: corrupted, signature: certificate.algorithm)) }
        XCTAssertEqual(fixture.vault.counts.reads, 0)
        XCTAssertEqual(fixture.vault.counts.signatures, 0)
    }
}
