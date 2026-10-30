import Foundation
import XCTest
import FrankenTermKit
@testable import FrankenTermTerminal

// These drive the real Rust mux client (frankenterm-mobile-mux) through
// ft_mux_v1_* with the bytes a v0.13 (codec v46) mux server sends.

private func leb(_ value: UInt64) -> [UInt8] {
    var value = value, out: [UInt8] = []
    repeat {
        var byte = UInt8(value & 0x7f); value >>= 7
        if value != 0 { byte |= 0x80 }
        out.append(byte)
    } while value != 0
    return out
}
private func text(_ string: String) -> [UInt8] { leb(UInt64(string.utf8.count)) + Array(string.utf8) }
private func frame(ident: UInt64, serial: UInt64, body: [UInt8]) -> Data {
    let ids = leb(serial) + leb(ident)
    return Data(leb(UInt64(ids.count + body.count)) + ids + body)
}

private final class MuxTestClock: @unchecked Sendable {
    private let lock = NSLock()
    private var value: UInt64 = 0
    func read() -> UInt64 { lock.withLock { value } }
    func set(_ value: UInt64) { lock.withLock { self.value = value } }
}

final class NativeMuxClientTests: XCTestCase {
    /// Complete codec-v71 PDU25, including the line envelope and sequence
    /// fence required by GetPaneRenderStateV1 (114). A live PDU30 is only
    /// the legacy PDU24 reply and cannot establish this observation.
    private func renderObservation(serial: UInt64) -> Data {
        var body: [UInt8] = [7, 0, 0, 0, 0, 0, 1] // pane, modes, cursor
        body += [1, 1, 1, 0, 0, 96, 8, 16, 0] // dimensions, pixels, reverse video
        body += [0, 0] + text("") + [0] // tiering, dirty ranges, title, working directory
        body += [0, 0, 0, 0, 1] // lines, hyperlinks, images, input serial, sequence
        return frame(ident: 25, serial: serial, body: body)
    }

    private func events(_ client: NativeMuxClient) throws -> [TerminalMuxEvent] {
        var all: [TerminalMuxEvent] = []
        while let event = try client.nextEvent() { all.append(event) }
        return all
    }

    private func settleRender(_ client: NativeMuxClient, serial: UInt8) throws {
        try client.receive(renderObservation(serial: UInt64(serial)))
        let layout: [UInt8] = [1, 1, 1, 1, 0, 0, 96, 8, 16, 0]
        XCTAssertEqual(try request(client, ident: 102, serial: serial + 1),
                       [7] + layout + [1, 0, 1], "read row 0..<1 at exactly this layout")
        // No retained line: the protocol renders the requested row as blank.
        try client.receive(frame(ident: 103, serial: UInt64(serial + 1), body: [7] + layout + [0, 0, 0]))
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty)
    }

    /// These small fixture requests have one-byte lengths/serials/IDs; the
    /// native codec agreement suite covers the general variable-length form.
    private func request(_ client: NativeMuxClient, ident: UInt8, serial: UInt8) throws -> [UInt8] {
        let sent = Array(try client.outgoing(limit: 4096))
        guard sent.count >= 3, sent[0] < 128 else { throw TerminalModelError.invalid }
        XCTAssertEqual(Int(sent[0]), sent.count - 1)
        XCTAssertEqual(sent[1], serial)
        XCTAssertEqual(sent[2], ident)
        return Array(sent.dropFirst(3))
    }

    private func currentClient(clock: MuxTestClock, consumeEvents: Bool = true) throws -> NativeMuxClient {
        let client = try NativeMuxClient(monotonicNanoseconds: { clock.read() })
        _ = try request(client, ident: 26, serial: 1)
        try client.receive(frame(ident: 27, serial: 1, body: leb(71) + text("current")))
        _ = try request(client, ident: 40, serial: 2)
        try client.receive(frame(ident: 10, serial: 2, body: []))
        _ = try request(client, ident: 81, serial: 3)
        let run = [UInt8](repeating: 0x1c, count: 16), stream = [UInt8](repeating: 0x5a, count: 16)
        // A coherent empty inventory followed by an explicitly shown pane's
        // authoritative identity, independent of display/render traffic.
        try client.receive(frame(ident: 82, serial: 3, body: [1] + stream + [0] + run + [0, 0, 0, 0, 0]))
        try client.show(pane: 7)
        _ = try request(client, ident: 117, serial: 4)
        let identity = run + [UInt8](repeating: 0xd1, count: 16) + [UInt8](repeating: 0x27, count: 16)
        try client.receive(frame(ident: 118, serial: 4, body: [7, 0] + identity + [0]))
        XCTAssertEqual(try request(client, ident: 114, serial: 5), [7])
        try settleRender(client, serial: 5)
        if consumeEvents { _ = try events(client) }
        return client
    }

    func testInitialResetIsPreparatoryUntilTheCompletePaneOutputArrives() throws {
        let client = try currentClient(clock: MuxTestClock(), consumeEvents: false)
        let output = try events(client).filter {
            switch $0 {
            case .preparingOutput, .output: true
            default: false
            }
        }
        XCTAssertEqual(output.count, 2, "the reset keeps its own native parser work budget")
        guard output.count == 2,
              case let .preparingOutput(resetPane, resetRows, resetColumns, reset) = output[0],
              case let .output(pane, rows, columns, complete) = output[1] else {
            return XCTFail("The native bridge lost the distinction between a reset and a complete pane")
        }
        XCTAssertEqual(resetPane, 7); XCTAssertEqual(pane, 7)
        XCTAssertEqual(resetRows, 1); XCTAssertEqual(resetColumns, 1)
        XCTAssertEqual(rows, 1); XCTAssertEqual(columns, 1)
        XCTAssertEqual(reset, Data("\u{1b}c\u{1b}[?25l\u{1b}[?2004h".utf8))
        XCTAssertFalse(complete.isEmpty)
        XCTAssertFalse(complete.starts(with: Data("\u{1b}c".utf8)),
                       "the complete paint must not repeat the separately charged reset")
    }

    func testACurrentServerNamesItsRunBeforeItsPanesAndPushesTheirChanges() throws {
        let client = try NativeMuxClient()
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [2, 1, 26])
        try client.receive(frame(ident: 27, serial: 1, body: leb(71) + text("current")))
        XCTAssertEqual(try events(client), [], "client registration must be acknowledged first")
        _ = try request(client, ident: 40, serial: 2)
        try client.receive(frame(ident: 10, serial: 2, body: []))
        XCTAssertEqual(try events(client), [.ready(serverVersion: "current")])
        // ListPanesCoherent(supported and required: a fenced snapshot), serial 3.
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [4, 3, 81, 1, 1])

        var entry: [UInt8] = leb(0) + leb(3) + leb(7) + text("agent")
        entry += leb(24) + leb(80) + leb(640) + leb(384) + leb(96)
        entry += [0, 0, 1, 0] + text("default")                  // no working_dir; alt, active, zoomed
        entry += [0, 0, 0, 1] + [0] + leb(0) + leb(0) + [0]
        let panes = leb(1) + leb(2) + entry + leb(1) + text("tab one") + leb(0) + leb(0)
        let run = [UInt8](repeating: 0x1c, count: 16), stream = [UInt8](repeating: 0x5a, count: 16)
        // negotiated, stream id, Snapshot(incarnation, revision, panes).
        try client.receive(frame(ident: 82, serial: 3, body: leb(1) + stream + leb(0) + run + leb(0) + panes))
        let pane = TerminalMuxPane(paneId: 7, tabId: 3, windowId: 0, tabTitle: "tab one", title: "agent",
            workspace: "default", workingDir: nil, size: .init(rows: 24, cols: 80), isActive: true, isZoomed: false)
        XCTAssertEqual(try events(client), [.incarnation(Data(run)), .panes([pane])])
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty, "that was the first list")

        // A topology change, PaneRemoved(7): the pane is gone; ListPanes, serial 4.
        try client.receive(frame(ident: 83, serial: 0, body: stream + leb(1) + leb(2) + leb(7)))
        XCTAssertEqual(try events(client), [.paneGone(paneID: 7)])
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [2, 4, 3])
    }

    func testAV46ServerIsGreetedAndItsPanesArriveAsTypedPanes() throws {
        let client = try NativeMuxClient()
        // GetCodecVersion, serial 1.
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [2, 1, 26])
        try client.receive(frame(ident: 27, serial: 1, body: leb(46) + text("v0.13")))
        XCTAssertEqual(try events(client), [.ready(serverVersion: "v0.13")])
        // Ready asks for the pane list: ListPanes, serial 2.
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [2, 2, 3])

        // The exact v46 PDU 4: one tab holding one leaf pane.
        var entry: [UInt8] = leb(0) + leb(3) + leb(7) + text("agent")
        entry += leb(24) + leb(80) + leb(640) + leb(384) + leb(96)  // size
        entry += [1] + text("file:///home/u/project")              // working_dir
        entry += [0, 1, 0] + text("default")                       // alt, active, zoomed, workspace
        entry += [0, 0, 0, 1]                                      // cursor x, y, shape, visibility
        entry += [0] + leb(0) + leb(0) + [0]                       // physical_top, top_row, left_col, tty
        let body = leb(1) + leb(2) + entry + leb(1) + text("tab one") + leb(0)
        // Delivered a byte at a time, as a slow channel would.
        for byte in frame(ident: 4, serial: 2, body: body) { try client.receive(Data([byte])) }
        let pane = TerminalMuxPane(paneId: 7, tabId: 3, windowId: 0, tabTitle: "tab one", title: "agent",
            workspace: "default", workingDir: "file:///home/u/project", size: .init(rows: 24, cols: 80),
            isActive: true, isZoomed: false)
        XCTAssertEqual(try events(client), [.panes([pane])])
        XCTAssertEqual(pane.label, "agent")

        // Showing it subscribes to its changes: GetPaneRenderChanges(7), serial 3.
        try client.show(pane: 7)
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [3, 3, 24, 7])
        try client.write(pane: 7, bytes: Data("ls\r".utf8))
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [7, 4, 9, 7, 3] + Array("ls\r".utf8))

        // Sizing the pane says what to give back. Queueing the restore must
        // not say it is done before the server's exact successful answer.
        try client.resize(pane: 7, rows: 40, columns: 54)
        let desktop = TerminalMuxSize(rows: 24, cols: 80, pixelWidth: 640, pixelHeight: 384, dpi: 96)
        XCTAssertEqual(try events(client), [.phoneSize(paneID: 7, desktop: desktop, rows: 40, columns: 54)])
        try client.restoreSize(pane: 7)
        XCTAssertEqual(try events(client), [], "a queued request is not a restoration receipt")
        // Until the server answers the restore (the last Resize, 14), it may
        // not have applied it. These frames are short: one-byte lengths.
        XCTAssertTrue(try client.restoring())
        func takeRestoreSerial() throws -> UInt64 {
            var sent = Array(try client.outgoing(limit: 4096)), restore: UInt8?
            while let length = sent.first.map(Int.init), sent.count > length {
                if sent[2] == 14 { restore = sent[1] }
                sent.removeFirst(length + 1)
            }
            return UInt64(try XCTUnwrap(restore))
        }
        try client.receive(frame(ident: 10, serial: try takeRestoreSerial(), body: []))
        XCTAssertFalse(try client.restoring())
        XCTAssertEqual(try events(client), [.phoneSizeRestored(paneID: 7)])
        // The listed pane is 24 x 80, not a phone size left behind.
        XCTAssertFalse(try client.adoptPhoneSize(pane: 7, desktop: desktop, rows: 40, columns: 54))

        try client.resize(pane: 7, rows: 41, columns: 55)
        try client.restoreSize(pane: 7)
        let older = try takeRestoreSerial()
        try client.resize(pane: 7, rows: 42, columns: 56)
        try client.restoreSize(pane: 7)
        let newer = try takeRestoreSerial()
        XCTAssertEqual(try events(client), [
            .phoneSize(paneID: 7, desktop: desktop, rows: 41, columns: 55),
            .phoneSize(paneID: 7, desktop: desktop, rows: 42, columns: 56)
        ])
        try client.receive(frame(ident: 10, serial: older, body: []))
        XCTAssertTrue(try client.restoring())
        XCTAssertEqual(try events(client), [], "an old ACK cannot retire the newer resize")
        try client.receive(frame(ident: 0, serial: newer, body: text("Error: resize refused")))
        XCTAssertFalse(try client.restoring())
        XCTAssertEqual(try events(client), [], "rejection is not a successful restoration")
        try client.restoreSize(pane: 7)
        try client.receive(frame(ident: 10, serial: try takeRestoreSerial(), body: []))
        XCTAssertEqual(try events(client), [.phoneSizeRestored(paneID: 7)])

        // A click is a press and a release: two SendMouseEvent (12) frames.
        _ = try client.outgoing(limit: 4096)
        try client.click(pane: 7, column: 3, row: 2)
        let click = Array(try client.outgoing(limit: 4096))
        XCTAssertEqual(click.count % 2, 0)
        let half = click.count / 2
        XCTAssertEqual(click[2], 12); XCTAssertEqual(click[half + 2], 12)
        XCTAssertThrowsError(try client.click(pane: 7, column: 5000, row: 0))
    }

    func testAServerOnAnotherProtocolEndsTheSessionWithItsReason() throws {
        let client = try NativeMuxClient()
        _ = try client.outgoing(limit: 4096)
        // Newer than the current window this client speaks (61..72), and no
        // longer accepting its newest: version, name, executable, no config
        // file, oldest version accepted.
        let body = leb(73) + text("v0.17") + text("/usr/local/bin/frankenterm-mux-server") + [0] + leb(73)
        try client.receive(frame(ident: 27, serial: 1, body: body))
        let all = try events(client)
        guard case let .failed(reason)? = all.first, all.count == 1 else { return XCTFail("\(all)") }
        XCTAssertTrue(reason.contains("73") && reason.contains("46") && reason.contains("72"), reason)
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty, "nothing more is sent")
    }

    func testANewerServerThatStillAcceptsThisClientIsSpokenToInTheCurrentLayout() throws {
        let client = try NativeMuxClient()
        _ = try client.outgoing(limit: 4096)
        let body = leb(73) + text("v0.17") + text("/usr/local/bin/frankenterm-mux-server") + [0] + leb(61)
        try client.receive(frame(ident: 27, serial: 1, body: body))
        XCTAssertEqual(try events(client), [])
        _ = try request(client, ident: 40, serial: 2)
        try client.receive(frame(ident: 10, serial: 2, body: []))
        XCTAssertEqual(try events(client), [.ready(serverVersion: "v0.17")])
        // ListPanesCoherent(supported and required: a fenced snapshot), serial 3.
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [4, 3, 81, 1, 1])
    }

    func testNoEffectRetryUsesTheSameOperationOnlyWhenItsClockWakeIsDue() throws {
        let clock = MuxTestClock(), client = try currentClient(clock: clock)
        try client.write(pane: 7, bytes: Data("abc".utf8))
        let original = try request(client, ident: 100, serial: 7)
        // ReliableWrite Retry(DefinitelyNotApplied), input serial 1.
        try client.receive(frame(ident: 101, serial: 7, body: [7, 1, 2, 1] + leb(5_000_000)))
        try client.write(pane: 7, bytes: Data([0x1b, 0x5b, 0x41]))
        XCTAssertEqual(try client.pollInputTimer(), Duration.milliseconds(5))
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty)
        clock.set(4_999_999)
        XCTAssertEqual(try client.pollInputTimer(), Duration.nanoseconds(1))
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty, "neither retry nor later key may escape early")
        clock.set(5_000_000)
        _ = try client.pollInputTimer()
        XCTAssertEqual(try request(client, ident: 100, serial: 8), original,
                       "new RPC serial, identical pane registration/input serial/payload")
        XCTAssertTrue(try client.unansweredInput(pane: 7))
        try client.receive(frame(ident: 101, serial: 8, body: [7, 1, 0, 3]))
        _ = try request(client, ident: 96, serial: 9)
        try client.receive(frame(ident: 97, serial: 9, body: [7, 4, 0]))
        XCTAssertNil(try client.pollInputTimer())
        XCTAssertFalse(try client.unansweredInput(pane: 7))
    }

    func testMissingReceiptExpiresWithoutReplayAndRetainsUnansweredState() throws {
        let clock = MuxTestClock(), client = try currentClient(clock: clock)
        try client.write(pane: 7, bytes: Data("possibly applied".utf8))
        _ = try request(client, ident: 100, serial: 7)
        clock.set(5_000_000_000)
        XCTAssertThrowsError(try client.pollInputTimer()) { XCTAssertEqual($0 as? TerminalModelError, .closed) }
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty)
        XCTAssertTrue(try client.unansweredInput(pane: 7), "read-only settlement remains available after timer failure")
        let all = try events(client)
        guard case let .failed(reason)? = all.first else { return XCTFail("missing deadline failure") }
        XCTAssertTrue(reason.contains("deadline"))
    }

    func testAClockRegressionCannotSendTheQueuedRetry() throws {
        let clock = MuxTestClock(), client = try currentClient(clock: clock)
        try client.write(pane: 7, bytes: Data("pending".utf8))
        _ = try request(client, ident: 100, serial: 7)
        try client.receive(frame(ident: 101, serial: 7, body: [7, 1, 2, 1] + leb(5_000_000)))
        clock.set(100)
        _ = try client.pollInputTimer()
        clock.set(99)
        XCTAssertThrowsError(try client.pollInputTimer()) { XCTAssertEqual($0 as? TerminalModelError, .closed) }
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty)
        XCTAssertTrue(try client.unansweredInput(pane: 7))
    }

    func testAPaneWhoseBusyServerKeepsRefusingIsReportedWaitingUntilARepeatIsAnswered() throws {
        let clock = MuxTestClock(), client = try NativeMuxClient(monotonicNanoseconds: { clock.read() })
        _ = try request(client, ident: 26, serial: 1)
        try client.receive(frame(ident: 27, serial: 1, body: leb(71) + text("current")))
        _ = try request(client, ident: 40, serial: 2)
        try client.receive(frame(ident: 10, serial: 2, body: []))
        _ = try request(client, ident: 81, serial: 3)
        let run = [UInt8](repeating: 0x1c, count: 16), stream = [UInt8](repeating: 0x5a, count: 16)
        try client.receive(frame(ident: 82, serial: 3, body: [1] + stream + [0] + run + [0, 0, 0, 0, 0]))
        try client.show(pane: 7)
        _ = try request(client, ident: 117, serial: 4)
        let identity = run + [UInt8](repeating: 0xd1, count: 16) + [UInt8](repeating: 0x27, count: 16)
        try client.receive(frame(ident: 118, serial: 4, body: [7, 0] + identity + [0]))
        _ = try events(client)
        // ErrorResponse v1 for GetPaneRenderStateV1 (114): RESOURCE_BUSY (12),
        // no object, NOT_APPLIED (1), SAFE_AFTER_BACKOFF (2).
        let busy: [UInt8] = [1, 114, 12, 0, 1, 2]
        var serial: UInt8 = 5
        for refusal in 1...3 {
            XCTAssertEqual(try request(client, ident: 114, serial: serial), [7])
            try client.receive(frame(ident: 0, serial: UInt64(serial), body: busy))
            XCTAssertEqual(try events(client), refusal < 3 ? [] : [.paneWaiting(paneID: 7, waiting: true)],
                           "after \(refusal) refusal(s)")
            guard let wait = try client.pollInputTimer() else { return XCTFail("no repeat is due") }
            let (seconds, attoseconds) = wait.components
            clock.set(clock.read() + UInt64(seconds) * 1_000_000_000 + UInt64(attoseconds / 1_000_000_000))
            _ = try client.pollInputTimer()
            serial += 1
        }
        try client.receive(renderObservation(serial: 0))
        XCTAssertEqual(try events(client), [.paneTitle(paneID: 7, title: ""),
                                            .mouseGrab(paneID: 7, grabbed: false)],
                       "unsolicited output may update metadata but cannot settle the pending read")
        XCTAssertEqual(try request(client, ident: 114, serial: serial), [7])
        try settleRender(client, serial: serial)
        let settled = try events(client)
        XCTAssertEqual(settled.filter { if case .paneWaiting = $0 { return true }; return false },
                       [.paneWaiting(paneID: 7, waiting: false)])
        XCTAssertFalse(settled.contains { if case .failed = $0 { return true }; return false })
        XCTAssertTrue(settled.contains {
            if case let .output(paneID, rows, columns, bytes) = $0 {
                return paneID == 7 && rows == 1 && columns == 1 && !bytes.isEmpty
            }
            return false
        }, "the recovered pane must actually draw")
        XCTAssertNil(try client.pollInputTimer(), "nothing is left to repeat")
    }

    func testAClosedChannelIsReportedOnce() throws {
        let client = try NativeMuxClient()
        try client.channelClosed()
        guard case .failed? = try client.nextEvent() else { return XCTFail("no failure") }
        XCTAssertNil(try client.nextEvent())
        XCTAssertThrowsError(try client.resize(pane: 1, rows: 0, columns: 80))
    }
}
