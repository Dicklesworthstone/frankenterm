// Real-network integration probe, not an application target. The Python harness
// owns the independent server and test private keys. Signing IPC is test-only.
import Foundation
import FrankenTermSSH
import FrankenTermKit
import FrankenTermSSHIntegration

private enum ProbeFailure: Error { case invalid, unexpected, denied }
private func emit(_ fields: [String: Any]) {
    let data = try! JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys])
    FileHandle.standardOutput.write(data + Data([10]))
}

/// Receives the production session pump's exact byte stream and disposition
/// records. It deliberately does not parse ANSI or manufacture render frames.
private actor PumpConsumer: TerminalSessionConsumer {
    let source: TerminalSource
    let maximumBytes: Int
    private var output = Data()
    private var outcomes: [TerminalInputOutcome] = []
    private var seenReceipts: Set<TerminalInputReceipt> = []
    private var resize: TerminalSessionGeometry?
    private var ended = false

    init(source: TerminalSource, maximumBytes: Int) {
        self.source = source; self.maximumBytes = maximumBytes
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        guard source == self.source, !ended, update.output.count <= 16_384,
              update.output.count <= maximumBytes - output.count else { throw ProbeFailure.invalid }
        output += update.output
        if let resized = update.resized {
            guard resize == nil else { throw ProbeFailure.invalid }
            resize = resized
        }
        ended = update.endOfOutput
    }
    func receive(_ values: [TerminalInputOutcome], source: TerminalSource) async throws {
        guard source == self.source, values.count <= 64,
              outcomes.count + values.count <= 128 else { throw ProbeFailure.invalid }
        for value in values {
            guard value.receipt.source == source, seenReceipts.insert(value.receipt).inserted else {
                throw ProbeFailure.invalid
            }
            outcomes.append(value)
        }
    }
    func waitForGreeting() async throws {
        let deadline = ContinuousClock.now + .seconds(5)
        while output.count < 15 {
            guard ContinuousClock.now < deadline else { throw ProbeFailure.unexpected }
            try await Task.sleep(for: .milliseconds(5))
        }
        guard output == Data("ready\r\nstderr\r\n".utf8) else { throw ProbeFailure.invalid }
    }
    func verify(output expected: Data, receipts: [TerminalInputReceipt], resize expectedResize: TerminalSessionGeometry?) throws {
        guard output == expected, resize == expectedResize,
              outcomes.map(\.receipt) == receipts,
              outcomes.allSatisfy({ $0.disposition == .transportAccepted && $0.originalBytes == $0.acceptedOriginalBytes && $0.acceptedCleanupBytes == 0 }),
              expectedResize == nil || ended else { throw ProbeFailure.invalid }
    }
}

private func mustReject(_ operation: @Sendable () async throws -> Void) async throws {
    do { try await operation() } catch { return }
    throw ProbeFailure.unexpected
}

private func runPump(session: NativeSSHSession, mode: String) async throws {
    var limits = TerminalInputLimits()
    limits.bytesPerConnection = 256 * 1024
    // The real adapter must return an accepted <=16 KiB prefix of this larger
    // coordinator lease. Treating its native slot limit as a fatal error loses
    // an otherwise valid, already admitted input operation.
    limits.maximumWriteBytes = 65536
    let coordinator = try TerminalInputCoordinator(limits: limits)
    let identity = try TerminalConnectionIdentity(workspaceID: "test-workspace", machineID: "test-machine",
        routeID: "test-route", routeRevision: 7, transportID: session.transportID, channelID: session.channelID)
    let attachment = try await NativeSSHTerminalAttachment.attach(session: session, identity: identity, coordinator: coordinator)
    try await mustReject { _ = try await NativeSSHTerminalAttachment.attach(session: session, identity: identity, coordinator: coordinator) }
    try await mustReject { _ = try await session.admit(Data("must-not-send".utf8)) }
    try await mustReject { _ = try await session.service(timeout: .milliseconds(20)) }
    try await mustReject { try await session.requestResize(.init(columns: 1, rows: 1)) }
    // Knowing this public source identity must not recover the private driver
    // capability, nor permit the older UUID-bound adapter to steal ownership.
    try await mustReject { _ = try await session.admit(Data("must-not-send".utf8), channelID: session.channelID) }
    try await mustReject { _ = try await session.service(timeout: .milliseconds(20), channelID: session.channelID) }
    try await mustReject { try await session.requestResize(.init(columns: 1, rows: 1), channelID: session.channelID) }
    try await mustReject { try await session.claimTerminalChannel(session.channelID) }
    try await mustReject { _ = try await NativeSSHTerminalChannel.attach(session: session, identity: identity, coordinator: coordinator) }
    // Legacy notifications are nonthrowing. They grant no channel access and
    // must not retire the real owner's pump or alter its byte accounting.
    let beforeWake = try await session.accounting()
    await session.wakeService()
    await session.wakeService(channelID: session.channelID)
    let afterWake = try await session.accounting()
    guard beforeWake == afterWake else { throw ProbeFailure.invalid }

    let payload = Data(("naïve 界 🦀\r\n" + String(repeating: "abcd", count: 500)).utf8)
        + Data((0..<200_000).map { UInt8(truncatingIfNeeded: $0) })
    let finish = Data("END-SESSION\r".utf8)
    let greeting = Data("ready\r\nstderr\r\n".utf8)
    let consumer = PumpConsumer(source: attachment.source, maximumBytes: greeting.count + payload.count + finish.count)
    let run = Task { try await attachment.pump.run(consumer: consumer) }
    let watchdog = Task {
        do { try await Task.sleep(for: .seconds(8)); attachment.pump.requestStop() }
        catch {}
    }
    defer { watchdog.cancel() }
    do {
        try await consumer.waitForGreeting()
        // The server is now quiet. New coordinator input has to wake the actual
        // idle NativeSSHSession.service call through the production adapter.
        if mode == "pumpCancel" {
            let started = ContinuousClock.now
            attachment.pump.requestStop()
            let reason = try await run.value
            guard reason == .cancelled, started.duration(to: .now) < .seconds(2) else { throw ProbeFailure.invalid }
            try await consumer.verify(output: greeting, receipts: [], resize: nil)
        } else {
            let stream = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(stream, to: attachment.source)
            let size = try TerminalSessionGeometry(columns: 132, rows: 42, pixelWidth: 1000, pixelHeight: 700)
            try await attachment.requestResize(size, source: attachment.source)
            let first = try await coordinator.submit(focus, encodedBytes: payload)
            let last = try await coordinator.submit(focus, encodedBytes: finish)
            let reason = try await run.value
            guard reason == .peerOutputEnded else { throw ProbeFailure.invalid }
            try await consumer.verify(output: greeting + payload + finish, receipts: [first,last], resize: size)
            let remaining = try await coordinator.pendingOutcomes(attachment.source)
            guard remaining.outcomes.isEmpty else { throw ProbeFailure.invalid }
        }
        try await attachment.close()
        let stats = try await attachment.accounting()
        let accepted = mode == "pumpCancel" ? 0 : payload.count + finish.count
        guard stats.allocatedBytes == 0, stats.locallyAcceptedBytes == accepted,
              stats.channelAcceptedBytes == accepted else { throw ProbeFailure.invalid }
        emit(["event": "done", "mode": mode, "bytes": accepted, "closed": true])
    } catch {
        attachment.pump.requestStop()
        try? await attachment.close()
        _ = try? await run.value
        throw error
    }
}

private final class Authority: NativeSSHAuthentication, @unchecked Sendable {
    let identity: NativeSSHIdentity, host: Data, mode: String
    let queue = DispatchQueue(label: "test.only.ssh-signing-ipc")
    let lock = NSLock()
    var cancelled = false, validations = 0
    weak var session: NativeSSHSession?
    init(publicKey: Data, host: Data, mode: String) throws {
        identity = try .init(user: "operator", publicKey: publicKey)
        self.host = host; self.mode = mode
    }
    func identity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity {
        guard peer.hostKey == host, peer.outboundCompression == "none", peer.inboundCompression == "none",
              peer.outboundMAC.hasSuffix("-etm@openssh.com"), peer.inboundMAC.hasSuffix("-etm@openssh.com") else { throw ProbeFailure.denied }
        if mode == "connect" {
            guard peer.endpoint?.host == "localhost", peer.endpoint?.port != nil else { throw ProbeFailure.invalid }
        }
        if mode == "rejectHost" { throw ProbeFailure.denied }
        if mode == "cancelPeer" { await session?.close() }
        return identity
    }
    func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) async throws -> Data {
        guard challenge.identity == identity, challenge.transportID == peer.transportID,
              [32, 48, 64].contains(challenge.sessionIdentifier.count) else { throw ProbeFailure.invalid }
        if mode == "cancelSigner" { await session?.close() }
        return try await withCheckedThrowingContinuation { continuation in
            queue.async {
                emit(["event": "sign", "preimage": challenge.signedData.base64EncodedString()])
                var line = Data()
                while line.count <= 2048 {
                    guard let next = try? FileHandle.standardInput.read(upToCount: 1), !next.isEmpty else {
                        continuation.resume(throwing: ProbeFailure.invalid); return
                    }
                    if next == Data([10]) { break }
                    line += next
                }
                guard line.count <= 2048, let signature = Data(base64Encoded: line) else {
                    continuation.resume(throwing: ProbeFailure.invalid); return
                }
                // Intentionally deliver despite cancellation to exercise the
                // production owner's post-await generation/deadline checks.
                continuation.resume(returning: signature)
            }
        }
    }
    private func incrementValidation() -> Int {
        lock.lock(); defer { lock.unlock() }; validations += 1; return validations
    }
    func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws {
        let call = incrementValidation()
        if mode == "revokeBeforeSign" && call == 1 { throw ProbeFailure.denied }
        if mode == "revokeBeforeShell" && call == 3 { throw ProbeFailure.denied }
        if mode == "cancelFinalValidation" && call == 2 { await session?.close() }
    }
    func cancel(transportID: UUID) { lock.lock(); cancelled = true; lock.unlock() }
}

@main struct SessionProbe {
    static func main() async {
        do {
            let args = CommandLine.arguments
            guard args.count == 5, let endpoint = Int32(args[1]), let host = Data(base64Encoded: args[2]),
                  let publicKey = Data(base64Encoded: args[3]) else { throw ProbeFailure.invalid }
            let mode = args[4]
            let transfersBytes = ["io", "idle", "connect", "wake", "pump", "pumpCancel"].contains(mode)
            let session: NativeSSHSession
            if mode == "connect" {
                guard endpoint > 0, endpoint <= 65535 else { throw ProbeFailure.invalid }
                session = try await NativeSSHConnector.connect(to: NativeSSHEndpoint(host: "localhost", port: UInt16(endpoint)), timeout: .seconds(5))
            } else { session = try NativeSSHSession(connectedDescriptor: endpoint) }
            let authority = try Authority(publicKey: publicKey, host: host, mode: mode)
            authority.session = session
            do {
                _ = try await session.authenticate(using: authority, timeout: mode == "deadlineSigner" ? .seconds(1) : .seconds(5))
                try await session.openShell(geometry: .init(columns: 80, rows: 24), timeout: .seconds(5))
                guard transfersBytes else { throw ProbeFailure.unexpected }
            } catch {
                if transfersBytes { throw error }
                if case ProbeFailure.unexpected = error { throw error }
                let stats = try await session.accounting()
                guard stats.allocatedBytes == 0, stats.locallyAcceptedBytes == 0 else { throw ProbeFailure.invalid }
                emit(["event": "done", "mode": mode, "closed": true]); return
            }
            if mode == "pump" || mode == "pumpCancel" {
                try await runPump(session: session, mode: mode)
                return
            }
            var received = Data()
            if mode == "idle" || mode == "wake" {
                while received.count < 15 {
                    received += try await session.service(timeout: .seconds(5)).output
                }
                if mode == "idle" {
                    let idle = try await session.service(timeout: .milliseconds(40))
                    guard idle.waitExpired, idle.output.isEmpty, !idle.endOfOutput,
                          idle.accounting.allocatedBytes > 0 else { throw ProbeFailure.invalid }
                } else {
                    // A wake preceding service must survive; a wake during an
                    // idle wait must complete it instead of reentering that wait.
                    await session.wakeService()
                    let started = ContinuousClock.now
                    let early = try await session.service(timeout: .seconds(5))
                    guard !early.waitExpired, early.output.isEmpty, !early.endOfOutput,
                          started.duration(to: .now) < .seconds(2) else { throw ProbeFailure.invalid }
                    let waiting = Task { try await session.service(timeout: .seconds(5)) }
                    try await Task.sleep(for: .milliseconds(20))
                    await session.wakeService()
                    let interrupted = try await waiting.value
                    guard !interrupted.waitExpired, interrupted.output.isEmpty, !interrupted.endOfOutput,
                          started.duration(to: .now) < .seconds(2) else { throw ProbeFailure.invalid }
                }
            }
            let initial = try NativeSSHGeometry(columns: 132, rows: 42, pixelWidth: 1000, pixelHeight: 700)
            try await session.requestResize(initial)
            var resized = false, admitted = 0
            let payload = Data(("naïve 界 🦀\r\n" + String(repeating: "abcd", count: 500)).utf8)
                + Data((0..<200_000).map { UInt8(truncatingIfNeeded: $0) }) + Data("END-SESSION\r".utf8)
            let expected = Data("ready\r\nstderr\r\n".utf8) + payload
            // The pump can wait while another actor turn admits input. This is
            // the native integration's wake-up, not an interval-based retry loop.
            for _ in 0..<10000 {
                let waiting = Task { try await session.service(timeout: .seconds(5)) }
                if admitted < payload.count {
                    let chunk = payload.subdata(in: admitted..<min(admitted + 4096, payload.count))
                    switch try await session.admit(chunk) {
                    case .accepted(let n): guard n == chunk.count else { throw ProbeFailure.invalid }; admitted += n
                    case .wouldBlock: break
                    }
                }
                let event = try await waiting.value
                received += event.output
                if event.resized == initial { resized = true }
                guard event.output.count <= 16384, received.count <= expected.count,
                      event.accounting.pendingWriteBytes <= 16384 else { throw ProbeFailure.invalid }
                if event.endOfOutput { break }
            }
            guard resized, admitted == payload.count, received == expected else { throw ProbeFailure.invalid }
            let final = try await session.accounting()
            guard final.locallyAcceptedBytes == payload.count, final.channelAcceptedBytes == payload.count else { throw ProbeFailure.invalid }
            async let first: Void = session.close()
            async let second: Void = session.close()
            _ = await (first, second)
            let closed = try await session.accounting()
            guard closed.allocatedBytes == 0 else { throw ProbeFailure.invalid }
            emit(["event": "done", "mode": mode, "bytes": payload.count, "peakNativeBytes": final.peakAllocatedBytes])
        } catch {
            emit(["event": "error", "reason": String(describing: error)])
            exit(1)
        }
    }
}
