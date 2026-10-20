// Real-network integration probe, not an application target. The Python harness
// owns the independent server and test private keys. Signing IPC is test-only.
import Foundation
import FrankenTermSSH

private enum ProbeFailure: Error { case invalid, unexpected, denied }
private func emit(_ fields: [String: Any]) {
    let data = try! JSONSerialization.data(withJSONObject: fields, options: [.sortedKeys])
    FileHandle.standardOutput.write(data + Data([10]))
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
            guard args.count == 5, let fd = Int32(args[1]), let host = Data(base64Encoded: args[2]),
                  let publicKey = Data(base64Encoded: args[3]) else { throw ProbeFailure.invalid }
            let mode = args[4]
            let session = try NativeSSHSession(connectedDescriptor: fd)
            let authority = try Authority(publicKey: publicKey, host: host, mode: mode)
            authority.session = session
            do {
                _ = try await session.authenticate(using: authority, timeout: mode == "deadlineSigner" ? .seconds(1) : .seconds(5))
                try await session.openShell(geometry: .init(columns: 80, rows: 24), timeout: .seconds(5))
                guard mode == "io" || mode == "idle" else { throw ProbeFailure.unexpected }
            } catch {
                if mode == "io" || mode == "idle" { throw error }
                if case ProbeFailure.unexpected = error { throw error }
                let stats = try await session.accounting()
                guard stats.allocatedBytes == 0, stats.locallyAcceptedBytes == 0 else { throw ProbeFailure.invalid }
                emit(["event": "done", "mode": mode, "closed": true]); return
            }
            var received = Data()
            if mode == "idle" {
                while received.count < 15 {
                    received += try await session.service(timeout: .seconds(5)).output
                }
                let idle = try await session.service(timeout: .milliseconds(40))
                guard idle.waitExpired, idle.output.isEmpty, !idle.endOfOutput,
                      idle.accounting.allocatedBytes > 0 else { throw ProbeFailure.invalid }
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
