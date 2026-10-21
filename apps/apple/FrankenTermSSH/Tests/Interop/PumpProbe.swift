// Real native driver -> session adapter -> coordinator -> consumer integration.
// Python owns synthetic test keys and the independent libssh server. There is
// deliberately no terminal parser, Keychain substitute or production signer here.
import Foundation
import FrankenTermKit
import FrankenTermSSH
import FrankenTermSSHIntegration

private enum ProbeError: Error { case invalid, expectedRejection, consumerFailure }
private func require(_ condition: Bool) throws { if !condition { throw ProbeError.invalid } }
private func emit(_ fields: [String: Any]) {
    let bytes = try! JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys])
    FileHandle.standardOutput.write(bytes + Data([10]))
}

private final class ProbeAuthority: NativeSSHAuthentication, Sendable {
    let selected: NativeSSHIdentity, host: Data
    private let queue = DispatchQueue(label: "test.only.pump-signing")
    init(publicKey: Data, host: Data) throws {
        selected = try NativeSSHIdentity(user: "operator", publicKey: publicKey); self.host = host
    }
    func identity(for peer: NativeSSHPeer) async throws -> NativeSSHIdentity {
        try require(peer.hostKey == host && peer.endpoint?.host == "localhost")
        try require(peer.outboundCompression == "none" && peer.inboundCompression == "none")
        try require(peer.outboundMAC.hasSuffix("-etm@openssh.com") && peer.inboundMAC.hasSuffix("-etm@openssh.com"))
        return selected
    }
    func signature(for challenge: NativeSSHChallenge, peer: NativeSSHPeer) async throws -> Data {
        try require(challenge.identity == selected && challenge.transportID == peer.transportID)
        return try await withCheckedThrowingContinuation { continuation in
            queue.async {
                emit(["event": "sign", "preimage": challenge.signedData.base64EncodedString()])
                var line = Data()
                while line.count <= 2048 {
                    guard let next = try? FileHandle.standardInput.read(upToCount: 1), !next.isEmpty else {
                        continuation.resume(throwing: ProbeError.invalid); return
                    }
                    if next == Data([10]) { break }
                    line += next
                }
                guard line.count <= 2048, let signature = Data(base64Encoded: line) else {
                    continuation.resume(throwing: ProbeError.invalid); return
                }
                continuation.resume(returning: signature)
            }
        }
    }
    func validate(peer: NativeSSHPeer, identity: NativeSSHIdentity) async throws {
        try require(peer.hostKey == host && identity == selected)
    }
    func cancel(transportID: UUID) {}
}

private actor ProbeConsumer: TerminalSessionConsumer {
    let source: TerminalSource
    let failOutcomes: Bool, replyToBanner: Bool
    let coordinator: TerminalInputCoordinator
    private var replied = false
    private var received = Data()
    private var metadata: [TerminalInputOutcome] = []
    private var geometry: TerminalSessionGeometry?
    private var waits: [(Int, CheckedContinuation<Void, Never>)] = []
    private var eof = false
    init(source: TerminalSource, coordinator: TerminalInputCoordinator, failOutcomes: Bool, replyToBanner: Bool) {
        self.source = source; self.coordinator = coordinator
        self.failOutcomes = failOutcomes; self.replyToBanner = replyToBanner
    }
    func receive(_ update: TerminalSessionUpdate, source: TerminalSource) async throws {
        try require(source == self.source && update.output.count <= 16384)
        try require(received.count + update.output.count <= 1_048_576)
        received += update.output; geometry = update.resized ?? geometry; eof = eof || update.endOfOutput
        if replyToBanner && !replied && received.count >= 15 {
            replied = true
            _ = try await coordinator.reply(source, encodedBytes: Data("\u{1B}[0n".utf8))
        }
        let done = waits.filter { received.count >= $0.0 }
        waits.removeAll { received.count >= $0.0 }
        for (_, continuation) in done { continuation.resume() }
    }
    func receive(_ outcomes: [TerminalInputOutcome], source: TerminalSource) async throws {
        try require(source == self.source && outcomes.count <= 64 && outcomes.allSatisfy { $0.receipt.source == source })
        if failOutcomes { throw ProbeError.consumerFailure }
        metadata += outcomes
    }
    func reached(_ count: Int) async {
        if received.count >= count { return }
        await withCheckedContinuation { waits.append((count, $0)) }
    }
    func summary() -> (Data, [TerminalInputOutcome], TerminalSessionGeometry?, Bool) { (received, metadata, geometry, eof) }
}

@main struct PumpProbe {
    static func main() async {
        do {
            let args = CommandLine.arguments
            guard args.count == 5, let port = UInt16(args[1]), let host = Data(base64Encoded: args[2]),
                  let publicKey = Data(base64Encoded: args[3]) else { throw ProbeError.invalid }
            let mode = args[4]
            let session = try await NativeSSHConnector.connect(to: NativeSSHEndpoint(host: "localhost", port: port))
            let authority = try ProbeAuthority(publicKey: publicKey, host: host)
            _ = try await session.authenticate(using: authority)
            let coordinator = try TerminalInputCoordinator()
            func identity(transport: UUID? = nil, channel: UUID = UUID()) throws -> TerminalConnectionIdentity {
                try TerminalConnectionIdentity(workspaceID: "test", machineID: "loopback", routeID: "direct", routeRevision: 1,
                    transportID: transport ?? session.transportID, channelID: channel)
            }
            // An authenticated connection alone is not a created terminal. This
            // rejection must not consume the connection or open a shell for us.
            do { _ = try await NativeSSHTerminalChannel.attach(session: session, identity: identity(), coordinator: coordinator)
                throw ProbeError.expectedRejection }
            catch NativeSSHError.closed { }
            try await session.openShell(geometry: NativeSSHGeometry(columns: 80, rows: 24))
            if mode == "usedChannel" || mode == "pendingWrite" {
                if mode == "usedChannel" { _ = try await session.service(timeout: .seconds(3)) }
                else { _ = try await session.admit(Data("legacy".utf8)) }
                do { _ = try await NativeSSHTerminalChannel.attach(session: session, identity: identity(), coordinator: coordinator)
                    throw ProbeError.expectedRejection }
                catch NativeSSHError.busy { }
                // Refused ownership must not close somebody else's live channel.
                try require(try await session.accounting().allocatedBytes > 0)
                await session.close()
                emit(["event": "done", "mode": mode, "refusedRelabel": true]); return
            }
            let id = try identity()
            do { _ = try await NativeSSHTerminalChannel.attach(session: session, identity: identity(transport: UUID()), coordinator: coordinator)
                throw ProbeError.expectedRejection }
            catch TerminalInputError.invalidIdentity { }
            let pump = try await NativeSSHTerminalChannel.attach(session: session, identity: id, coordinator: coordinator)
            // A second owner is rejected without closing the first attachment.
            do { _ = try await NativeSSHTerminalChannel.attach(session: session, identity: identity(), coordinator: coordinator)
                throw ProbeError.expectedRejection }
            catch NativeSSHError.busy { }
            do { _ = try await session.admit(Data([1])); throw ProbeError.expectedRejection }
            catch NativeSSHError.invalidInput { }
            do { _ = try await session.service(timeout: .milliseconds(10)); throw ProbeError.expectedRejection }
            catch NativeSSHError.invalidInput { }
            do { try await session.requestResize(NativeSSHGeometry(columns: 1, rows: 1)); throw ProbeError.expectedRejection }
            catch NativeSSHError.invalidInput { }
            let blank = try await coordinator.makeStream(sceneID: UUID())
            let focus = try await coordinator.changeFocus(blank, to: pump.source)
            let sink = ProbeConsumer(source: pump.source, coordinator: coordinator, failOutcomes: mode == "outcomeFailure", replyToBanner: mode == "reply")
            if mode == "outcomeFailure" { _ = try await coordinator.submit(focus, encodedBytes: Data("pending\r".utf8)) }
            let run = Task { try await pump.run(consumer: sink) }
            // Bound the entire network trace independently from the driver's own
            // idle timeout, so a broken wakeup fails rather than hanging the suite.
            let watchdog = Task { try await Task.sleep(for: .seconds(12)); pump.requestStop() }
            defer { watchdog.cancel() }
            if mode == "outcomeFailure" {
                let end = try await run.value
                try require(end == .consumerFailed)
                let batch = try await coordinator.pendingOutcomes(pump.source)
                try require(batch.outcomes.count == 1 && batch.outcomes[0].disposition == .transportAccepted)
                try await coordinator.acknowledgeOutcomes(batch)
                emit(["event": "done", "mode": mode, "retainedOutcome": true]); return
            }
            await sink.reached(mode == "reply" ? 19 : 15)
            if mode == "cancelIdle" {
                run.cancel()
                let end = try await run.value
                try require(end == .cancelled)
                try require(try await session.accounting().allocatedBytes == 0)
                emit(["event": "done", "mode": mode, "closed": true]); return
            }
            // No input notification exists until the session is actually idle.
            // This sleep is a test stimulus, never production polling.
            try await Task.sleep(for: .milliseconds(70))
            try await session.requestResize(NativeSSHGeometry(columns: 132, rows: 42), channelID: id.channelID)
            let payload = Data(("native é界🦀\r\n" + String(repeating: "abcd", count: mode == "large" ? 24000 : 200)).utf8)
            var expected = Data("ready\r\nstderr\r\n".utf8)
            if mode == "reply" { expected += Data("\u{1B}[0n".utf8) }
            if mode == "paste" {
                let text = String(repeating: "hello 界🦀\n", count: 5000)
                _ = try await coordinator.paste(focus, text: text, bracketed: true)
                expected += Data("\u{1B}[200~\(text)\u{1B}[201~".utf8)
            } else {
                _ = try await coordinator.submit(focus, encodedBytes: payload)
                expected += payload
            }
            // The original source continues its stream even with no focused view.
            let hidden = try await coordinator.changeFocus(focus, to: nil)
            await sink.reached(expected.count)
            let returned = try await coordinator.changeFocus(hidden, to: pump.source)
            let ending = Data("END-SESSION\r".utf8)
            _ = try await coordinator.submit(returned, encodedBytes: ending)
            expected += ending
            let end = try await run.value, summary = await sink.summary()
            try require(end == .peerOutputEnded && summary.0 == expected && summary.3)
            try require(summary.2?.columns == 132 && summary.2?.rows == 42)
            let stats = try await session.accounting()
            try require(stats.locallyAcceptedBytes == expected.count - 15 && stats.channelAcceptedBytes == expected.count - 15)
            try require(stats.allocatedBytes == 0 && stats.pendingWriteBytes == 0)
            let tail = try await coordinator.pendingOutcomes(pump.source)
            if !tail.outcomes.isEmpty { try await coordinator.acknowledgeOutcomes(tail) }
            _ = try await coordinator.suspend(returned)
            try await coordinator.retire(pump.source)
            let accounting = await coordinator.accounting()
            try require(accounting.connections == 0 && accounting.retainedBytes == 0 && accounting.retainedRecords == 0)
            emit(["event": "done", "mode": mode, "bytes": expected.count - 15, "closed": true])
        } catch {
            emit(["event": "error", "reason": String(describing: error)])
            exit(1)
        }
    }
}
