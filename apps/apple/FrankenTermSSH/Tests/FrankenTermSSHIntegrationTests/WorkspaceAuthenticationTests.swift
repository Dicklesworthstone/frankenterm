import Foundation
import XCTest
import FrankenTermKit
@testable import FrankenTermSSH
import FrankenTermSSHIntegration
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

private enum AuthenticationFixtureError: Error { case unexpectedStorageAccess }
private final class UnusedAuthenticationPersistence: WorkspacePersistence, @unchecked Sendable {
    private let lock = NSLock()
    private var value = 0
    var accesses: Int { lock.withLock { value } }
    func withExclusiveAccess<T>(_ body: () throws -> T) throws -> T {
        lock.withLock { value += 1 }
        throw AuthenticationFixtureError.unexpectedStorageAccess
    }
    func read(_ slot: WorkspaceStorageSlot, limit: Int) throws -> Data? { throw AuthenticationFixtureError.unexpectedStorageAccess }
    func write(_ data: Data, to slot: WorkspaceStorageSlot) throws { throw AuthenticationFixtureError.unexpectedStorageAccess }
    func removeJournal() throws { throw AuthenticationFixtureError.unexpectedStorageAccess }
}
private struct UnusedAuthenticationVault: WorkspaceCredentialVault, WorkspaceSSHSigning {
    func create(account: String, privateContainer: Data, fingerprint: String) throws { throw AuthenticationFixtureError.unexpectedStorageAccess }
    func fingerprint(account: String) throws -> String? { throw AuthenticationFixtureError.unexpectedStorageAccess }
    func remove(account: String) throws { throw AuthenticationFixtureError.unexpectedStorageAccess }
    func publicIdentity(requestID: UUID, authorization: WorkspaceSSHAuthorization) async throws -> Data {
        throw AuthenticationFixtureError.unexpectedStorageAccess
    }
    func sign(requestID: UUID, authorization: WorkspaceSSHAuthorization, challenge: SSHUserAuthenticationChallenge) async throws -> Data {
        throw AuthenticationFixtureError.unexpectedStorageAccess
    }
    func cancel(requestID: UUID) {}
}

final class WorkspaceAuthenticationTests: XCTestCase, @unchecked Sendable {
    private let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
        credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])

    private func sockets() throws -> [Int32] {
        var descriptors = [Int32](repeating: -1, count: 2)
#if os(Linux)
        let kind = Int32(SOCK_STREAM.rawValue)
#else
        let kind = SOCK_STREAM
#endif
        guard socketpair(AF_UNIX, kind, 0, &descriptors) == 0 else { throw NativeSSHError.invalidInput }
        return descriptors
    }
    private func closePeer(_ descriptor: Int32) {
#if canImport(Darwin)
        _ = Darwin.close(descriptor)
#else
        _ = Glibc.close(descriptor)
#endif
    }
    private func authority(_ session: NativeSSHSession, persistence: UnusedAuthenticationPersistence) throws -> WorkspaceNativeSSHAuthentication {
        let vault = UnusedAuthenticationVault(), store = WorkspaceStore(persistence: persistence, vault: vault)
        let broker = SSHAuthenticationBroker(store: store, signer: vault, support: support)
        return try .init(broker: broker, workspaceID: UUID(), revision: UUID(),
                         routeID: "route", credentialID: "key", transportID: session.transportID,
                         endpoint: .init(host: "host.invalid"))
    }
    private func blob() -> Data {
        Data([0, 0, 0, 11]) + Data("ssh-ed25519".utf8) + Data([0, 0, 0, 32]) + Data(repeating: 7, count: 32)
    }
    private func peer(_ session: NativeSSHSession, variant: Int) throws -> NativeSSHPeer {
        try .init(transportID: variant == 0 ? UUID() : session.transportID,
              endpoint: variant == 1 ? .init(host: "other.invalid", port: 22) :
                  variant == 2 ? .init(host: "host.invalid", port: 2222) : session.endpoint,
              hostKey: variant == 8 ? blob() + Data([0]) : blob(),
              keyExchange: variant == 3 ? "diffie-hellman-group14-sha1" : "curve25519-sha256",
              hostSignature: variant == 9 ? "ecdsa-sha2-nistp256" : "ssh-ed25519",
              outboundCipher: variant == 4 ? "aes256-cbc" : "aes256-ctr", inboundCipher: "aes256-ctr",
              outboundMAC: variant == 5 ? "hmac-sha2-256" : "hmac-sha2-256-etm@openssh.com",
              inboundMAC: "hmac-sha2-256-etm@openssh.com",
              outboundCompression: variant == 6 ? "zlib@openssh.com" : "none",
              inboundCompression: variant == 7 ? "zlib" : "none")
    }

    func testPeerWithoutImmutableEndpointCannotAcquireWorkspaceAuthority() async throws {
        let descriptors = try sockets(); defer { closePeer(descriptors[1]) }
        let session = try NativeSSHSession(connectedDescriptor: descriptors[0]), persistence = UnusedAuthenticationPersistence()
        let authority = try authority(session, persistence: persistence)
        do { _ = try await authority.identity(for: peer(session, variant: -1)); XCTFail("Endpoint-less peer acquired workspace authority") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .denied) }
        XCTAssertEqual(persistence.accesses, 0)
        await session.close()
    }

    func testForeignPeerOrUnqualifiedNegotiationFailsBeforeWorkspaceOrProtectedKeyAccess() async throws {
        for variant in 0..<10 {
            let descriptors = try sockets(); defer { closePeer(descriptors[1]) }
            let session = try NativeSSHSession(connectedDescriptor: descriptors[0], endpoint: .init(host: "host.invalid"))
            let persistence = UnusedAuthenticationPersistence(), authority = try authority(session, persistence: persistence)
            do {
                _ = try await authority.identity(for: peer(session, variant: variant))
                XCTFail("Unbound peer context reached identity selection: \(variant)")
            } catch {
                XCTAssertFalse(error is AuthenticationFixtureError, "Peer refusal occurred after persistent access: \(variant)")
            }
            XCTAssertEqual(persistence.accesses, 0)
            await session.close()
        }
    }

    func testExactTransportCancellationLatchesBeforeSelectionAndCannotExposeAuthorization() async throws {
        let descriptors = try sockets(); defer { closePeer(descriptors[1]) }
        let session = try NativeSSHSession(connectedDescriptor: descriptors[0], endpoint: .init(host: "host.invalid"))
        let persistence = UnusedAuthenticationPersistence(), authority = try authority(session, persistence: persistence)
        authority.cancel(transportID: session.transportID)
        do { _ = try await authority.identity(for: peer(session, variant: -1)); XCTFail("Cancelled authority selected a key") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        do { try await authority.validateCurrent(); XCTFail("Unselected authority exposed a grant") }
        catch { XCTAssertEqual(error as? SSHAuthenticationError, .stale) }
        XCTAssertEqual(persistence.accesses, 0)
        await session.close()
    }
}
