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

final class NativeMuxClientTests: XCTestCase {
    private func events(_ client: NativeMuxClient) throws -> [TerminalMuxEvent] {
        var all: [TerminalMuxEvent] = []
        while let event = try client.nextEvent() { all.append(event) }
        return all
    }

    func testACurrentServerNamesItsRunBeforeItsPanesAndPushesTheirChanges() throws {
        let client = try NativeMuxClient()
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [2, 1, 26])
        try client.receive(frame(ident: 27, serial: 1, body: leb(70) + text("current")))
        XCTAssertEqual(try events(client), [.ready(serverVersion: "current")])
        // ListPanesCoherent(supported and required: a fenced snapshot), serial 2.
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [4, 2, 81, 1, 1])

        var entry: [UInt8] = leb(0) + leb(3) + leb(7) + text("agent")
        entry += leb(24) + leb(80) + leb(640) + leb(384) + leb(96)
        entry += [0, 0, 1, 0] + text("default")                  // no working_dir; alt, active, zoomed
        entry += [0, 0, 0, 1] + [0] + leb(0) + leb(0) + [0]
        let panes = leb(1) + leb(2) + entry + leb(1) + text("tab one") + leb(0) + leb(0)
        let run = [UInt8](repeating: 0x1c, count: 16), stream = [UInt8](repeating: 0x5a, count: 16)
        // negotiated, stream id, Snapshot(incarnation, revision, panes).
        try client.receive(frame(ident: 82, serial: 2, body: leb(1) + stream + leb(0) + run + leb(0) + panes))
        let pane = TerminalMuxPane(paneId: 7, tabId: 3, windowId: 0, tabTitle: "tab one", title: "agent",
            workspace: "default", workingDir: nil, size: .init(rows: 24, cols: 80), isActive: true, isZoomed: false)
        XCTAssertEqual(try events(client), [.incarnation(Data(run)), .panes([pane])])
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty, "that was the first list")

        // A topology change, PaneRemoved(7): the pane is gone; ListPanes, serial 3.
        try client.receive(frame(ident: 83, serial: 0, body: stream + leb(1) + leb(2) + leb(7)))
        XCTAssertEqual(try events(client), [.paneGone(paneID: 7)])
        XCTAssertEqual(Array(try client.outgoing(limit: 4096)), [2, 3, 3])
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

        // Sizing the pane to the phone says what to give back, and giving it
        // back says it is done.
        try client.resize(pane: 7, rows: 40, columns: 54)
        let desktop = TerminalMuxSize(rows: 24, cols: 80, pixelWidth: 640, pixelHeight: 384, dpi: 96)
        XCTAssertEqual(try events(client), [.phoneSize(paneID: 7, desktop: desktop, rows: 40, columns: 54)])
        try client.restoreSize(pane: 7)
        XCTAssertEqual(try events(client), [.phoneSize(paneID: 7, desktop: nil, rows: 0, columns: 0)])
        // Until the server answers the restore (the last Resize, 14), it may
        // not have applied it. These frames are short: one-byte lengths.
        XCTAssertTrue(try client.restoring())
        var sent = Array(try client.outgoing(limit: 4096)), restore: UInt8?
        while let length = sent.first.map(Int.init), sent.count > length {
            if sent[2] == 14 { restore = sent[1] }
            sent.removeFirst(length + 1)
        }
        try client.receive(frame(ident: 10, serial: UInt64(try XCTUnwrap(restore)), body: []))
        XCTAssertFalse(try client.restoring())
        // The listed pane is 24 x 80, not a phone size left behind.
        XCTAssertFalse(try client.adoptPhoneSize(pane: 7, desktop: desktop, rows: 40, columns: 54))

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
        // Newer than the current window this client speaks (61..70).
        try client.receive(frame(ident: 27, serial: 1, body: leb(71) + text("v0.16")))
        let all = try events(client)
        guard case let .failed(reason)? = all.first, all.count == 1 else { return XCTFail("\(all)") }
        XCTAssertTrue(reason.contains("71") && reason.contains("46") && reason.contains("70"), reason)
        XCTAssertTrue(try client.outgoing(limit: 4096).isEmpty, "nothing more is sent")
    }

    func testAClosedChannelIsReportedOnce() throws {
        let client = try NativeMuxClient()
        try client.channelClosed()
        guard case .failed? = try client.nextEvent() else { return XCTFail("no failure") }
        XCTAssertNil(try client.nextEvent())
        XCTAssertThrowsError(try client.resize(pane: 1, rows: 0, columns: 80))
    }
}
