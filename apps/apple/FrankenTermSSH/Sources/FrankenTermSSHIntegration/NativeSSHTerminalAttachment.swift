import Foundation
import FrankenTermKit
import FrankenTermSSH

/// Transfers one already authenticated and explicitly created SSH shell into
/// the canonical input/output pump. This owner does not authorize network use,
/// verify host policy, create shells, or supply a terminal parser. The caller
/// completes those steps and its durable uncertainty marker before attaching.
///
/// The actual transport never escapes: it cannot be attached to another
/// coordinator or pump. The source is immutable, and every resize requires that
/// exact coordinator-issued witness. Ordinary SSH gives no remote geometry ACK.
public final class NativeSSHTerminalAttachment: Sendable {
    public let source: TerminalSource
    public let pump: TerminalSessionPump
    public var identity: TerminalConnectionIdentity { source.identity }
    private let transport: NativeSSHTerminalTransport

    private init(pump: TerminalSessionPump, transport: NativeSSHTerminalTransport) {
        self.pump = pump; source = pump.source; self.transport = transport
    }

    /// Failure before the native claim leaves the session with its caller.
    /// Failure after claiming closes and settles it. An attachment never takes
    /// over an already driven channel or retries through a replacement session.
    public static func attach(session: NativeSSHSession, identity: TerminalConnectionIdentity,
                              coordinator: TerminalInputCoordinator) async throws -> NativeSSHTerminalAttachment {
        try Task.checkCancellation()
        guard identity.transportID == session.transportID, identity.channelID == session.channelID else {
            throw TerminalInputError.invalidIdentity
        }
        let driver = try await session.claimTerminalDriver(transportID: identity.transportID, channelID: identity.channelID)
        let transport = NativeSSHTerminalTransport(session: session, identity: identity, driver: driver)
        var attached: TerminalSessionPump?
        do {
            let pump = try await TerminalSessionPump.attach(transport, coordinator: coordinator)
            attached = pump
            try Task.checkCancellation()
            return NativeSSHTerminalAttachment(pump: pump, transport: transport)
        } catch {
            if let attached {
                try? await attached.disconnect()
                // This unpublished source has never been given to a responder,
                // consumer or writer. Its empty ownership record can retire.
                try? await coordinator.retire(attached.source)
            }
            await transport.close()
            throw error
        }
    }

    /// At most one immutable native resize is pending. Busy means the caller
    /// must wait for its source-bound service completion, not overwrite it.
    public func requestResize(_ geometry: TerminalSessionGeometry, source: TerminalSource) async throws {
        guard source == self.source else { throw TerminalInputError.stale }
        try Task.checkCancellation()
        try await transport.requestResize(geometry)
    }

    /// Retires admission and joins native socket teardown. The caller retains
    /// and joins its pump.run task separately, including any consumer callback.
    /// This method is safe inside a consumer callback because it does not wait
    /// for that same callback to return. Input outcomes remain in the coordinator.
    public func close() async throws {
        do { try await pump.disconnect() }
        catch { await transport.close(); throw error }
    }

    /// Available after close. Locally accepted and channel-accepted bytes remain
    /// distinct: either count can overstate what a remote program processed, and
    /// their difference is unresolved local buffering, never replay authority.
    public func accounting() async throws -> NativeSSHAccounting {
        try await transport.accounting()
    }
}

/// A single native slot is the only downstream byte buffer. No extra queue,
/// task per write, polling timer, or second SSH driver is introduced here.
private final class NativeSSHTerminalTransport: TerminalSessionTransport, Sendable {
    let identity: TerminalConnectionIdentity
    let inputSignal = TerminalSessionSignal()
    private let session: NativeSSHSession
    private let driver: NativeSSHDriver

    init(session: NativeSSHSession, identity: TerminalConnectionIdentity, driver: NativeSSHDriver) {
        self.session = session; self.identity = identity; self.driver = driver
    }

    func writeSome(_ bytes: Data) async throws -> TerminalTransportWrite {
        // A coordinator may lease up to 64 KiB. Accept only the bounded prefix
        // that fits this native slot and leave its exact tail with that owner.
        guard !bytes.isEmpty, bytes.count <= 65536 else { throw TerminalInputError.invalidWriteSize }
        let prefix = Data(bytes.prefix(NativeSSHSession.maximumWriteBytes))
        switch try await session.admit(prefix, driver: driver) {
        case .accepted(let count): return .accepted(count)
        case .wouldBlock: return .wouldBlock
        }
    }
    func service() async throws -> TerminalSessionUpdate {
        let update = try await session.service(driver: driver)
        let resized = try update.resized.map {
            try TerminalSessionGeometry(columns: $0.columns, rows: $0.rows,
                                        pixelWidth: $0.pixelWidth, pixelHeight: $0.pixelHeight)
        }
        return .init(output: update.output, endOfOutput: update.endOfOutput, resized: resized)
    }
    func wakeService() async {
        do { try await session.wakeService(driver: driver) }
        catch { inputSignal.finish(); await session.close() }
    }
    func requestResize(_ geometry: TerminalSessionGeometry) async throws {
        try await session.requestResize(NativeSSHGeometry(columns: geometry.columns, rows: geometry.rows,
            pixelWidth: geometry.pixelWidth, pixelHeight: geometry.pixelHeight), driver: driver)
    }
    func close() async {
        inputSignal.finish()
        await session.close()
    }
    func accounting() async throws -> NativeSSHAccounting { try await session.accounting() }
}
