import Foundation
import FrankenTermKit
import FrankenTermSSH

/// The actual libssh2 session bound to the existing input/streaming owner. This
/// product is opt-in; the offline workspace application does not link or invoke
/// it merely by restoring a workspace. It adds no parser, signer, or queue.
/// This compatibility driver uses public UUID-bound raw I/O. New owners should
/// use NativeSSHTerminalAttachment, whose driver capability never escapes.
public final class NativeSSHTerminalChannel: TerminalSessionTransport, Sendable {
    public let identity: TerminalConnectionIdentity
    public let inputSignal = TerminalSessionSignal()
    private let session: NativeSSHSession

    private init(session: NativeSSHSession, identity: TerminalConnectionIdentity) {
        self.session = session; self.identity = identity
    }

    /// Call after explicit authentication and PTY creation, and after the owning
    /// app has prepared its durable uncertainty marker. A wrong transport or a
    /// competing claim leaves the existing owner untouched. After a successful
    /// claim, failed coordinator attachment closes the now-owned native session.
    public static func attach(session: NativeSSHSession, identity: TerminalConnectionIdentity,
                              coordinator: TerminalInputCoordinator) async throws -> TerminalSessionPump {
        try Task.checkCancellation()
        guard identity.transportID == session.transportID else { throw TerminalInputError.invalidIdentity }
        try await session.claimTerminalChannel(identity.channelID)
        let transport = NativeSSHTerminalChannel(session: session, identity: identity)
        var attached: TerminalSessionPump?
        do {
            let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
            attached = pump
            try Task.checkCancellation()
            return pump
        } catch {
            if let attached {
                try? await attached.disconnect()
                try? await coordinator.retire(attached.source)
            }
            await transport.close()
            throw error
        }
    }

    public func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        try Task.checkCancellation()
        guard !bytes.isEmpty else { throw TerminalInputError.emptyInput }
        // The coordinator may configure a larger chunk than the native slot.
        // Report only this bounded prefix; its suffix remains in the coordinator.
        switch try await session.admit(Data(bytes.prefix(16_384)), channelID: identity.channelID) {
        case .accepted(let count): return .accepted(count)
        case .wouldBlock: return .wouldBlock
        }
    }
    public func service() async throws -> TerminalSessionUpdate {
        let update = try await session.service(channelID: identity.channelID)
        let resized = try update.resized.map {
            try TerminalSessionGeometry(columns: $0.columns, rows: $0.rows,
                                        pixelWidth: $0.pixelWidth, pixelHeight: $0.pixelHeight)
        }
        // Empty idle-expiry/local-work events are not EOF or transport failures.
        return TerminalSessionUpdate(output: update.output, endOfOutput: update.endOfOutput, resized: resized)
    }
    public func wakeService() async { await session.wakeService(channelID: identity.channelID) }
    public func close() async { inputSignal.finish(); await session.close() }
}
