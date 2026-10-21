import Foundation
import XCTest
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration
#if canImport(Darwin)
import Darwin
#else
import Glibc
#endif

private func attachmentSocketPair() throws -> [Int32] {
    var pair = [Int32](repeating: -1, count: 2)
#if os(Linux)
    let kind = Int32(SOCK_STREAM.rawValue)
#else
    let kind = SOCK_STREAM
#endif
    guard socketpair(AF_UNIX, kind, 0, &pair) == 0 else { throw NativeSSHError.invalidInput }
    return pair
}
private func closeAttachmentPeer(_ descriptor: Int32) {
#if canImport(Darwin)
    _ = Darwin.close(descriptor)
#else
    _ = Glibc.close(descriptor)
#endif
}
private func attachmentIdentity(_ session: NativeSSHSession,
                                transport: UUID? = nil, channel: UUID? = nil) throws -> TerminalConnectionIdentity {
    try .init(workspaceID: "test-workspace", machineID: "test-machine", routeID: "test-route", routeRevision: 1,
              transportID: transport ?? session.transportID, channelID: channel ?? session.channelID)
}

final class AttachmentTests: XCTestCase, @unchecked Sendable {
    func testUnauthenticatedSocketCannotBecomeATerminalSource() async throws {
        let sockets = try attachmentSocketPair(); defer { closeAttachmentPeer(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let coordinator = try TerminalInputCoordinator()
        do {
            _ = try await NativeSSHTerminalAttachment.attach(session: session, identity: attachmentIdentity(session), coordinator: coordinator)
            XCTFail("A connected socket is not an authenticated shell")
        } catch { XCTAssertEqual(error as? NativeSSHError, .busy) }
        let beforeClose = try await session.accounting()
        XCTAssertGreaterThan(beforeClose.allocatedBytes, 0, "Failed preflight leaves the caller's native owner intact")
        XCTAssertEqual(beforeClose.locallyAcceptedBytes, 0)
        let inputs = await coordinator.accounting()
        XCTAssertEqual(inputs.connections, 0)
        await session.close()
    }

    func testBothNativeIdentitiesMustMatchBeforeOwnershipCanTransfer() async throws {
        let sockets = try attachmentSocketPair(); defer { closeAttachmentPeer(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let coordinator = try TerminalInputCoordinator()
        for identity in [try attachmentIdentity(session, transport: UUID()),
                         try attachmentIdentity(session, channel: UUID())] {
            do {
                _ = try await NativeSSHTerminalAttachment.attach(session: session, identity: identity, coordinator: coordinator)
                XCTFail("A foreign transport or channel became this terminal")
            } catch { XCTAssertEqual(error as? TerminalInputError, .invalidIdentity) }
        }
        let stats = try await session.accounting()
        XCTAssertGreaterThan(stats.allocatedBytes, 0)
        XCTAssertEqual(stats.locallyAcceptedBytes, 0)
        let inputs = await coordinator.accounting()
        XCTAssertEqual(inputs.connections, 0)
        await session.close()
    }

    func testCancelledAttachmentDoesNotPublishOrConsumeCallerOwnership() async throws {
        let sockets = try attachmentSocketPair(); defer { closeAttachmentPeer(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let coordinator = try TerminalInputCoordinator(), identity = try attachmentIdentity(session)
        let attempt = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try await NativeSSHTerminalAttachment.attach(session: session, identity: identity, coordinator: coordinator)
        }
        do { _ = try await attempt.value; XCTFail("Cancelled source published") }
        catch { XCTAssertTrue(error is CancellationError) }
        let stats = try await session.accounting()
        XCTAssertGreaterThan(stats.allocatedBytes, 0)
        let inputs = await coordinator.accounting()
        XCTAssertEqual(inputs.connections, 0)
        await session.close()
    }

    func testClosedChannelCannotAttachOrLatchAnotherServiceWake() async throws {
        let sockets = try attachmentSocketPair(); defer { closeAttachmentPeer(sockets[1]) }
        let session = try NativeSSHSession(connectedDescriptor: sockets[0])
        let coordinator = try TerminalInputCoordinator()
        await session.wakeService()
        await session.wakeService(channelID: session.channelID)
        await session.close()
        do {
            _ = try await NativeSSHTerminalAttachment.attach(session: session, identity: attachmentIdentity(session), coordinator: coordinator)
            XCTFail("Closed source revived")
        } catch { XCTAssertEqual(error as? NativeSSHError, .closed) }
        await session.wakeService()
        await session.wakeService(channelID: session.channelID)
        let stats = try await session.accounting()
        XCTAssertEqual(stats.allocatedBytes, 0)
        XCTAssertEqual(stats.locallyAcceptedBytes, 0)
        let inputs = await coordinator.accounting()
        XCTAssertEqual(inputs.connections, 0)
    }
}
