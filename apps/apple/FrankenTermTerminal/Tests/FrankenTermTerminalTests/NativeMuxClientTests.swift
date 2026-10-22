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
    }

    func testAServerOnAnotherProtocolEndsTheSessionWithItsReason() throws {
        let client = try NativeMuxClient()
        _ = try client.outgoing(limit: 4096)
        try client.receive(frame(ident: 27, serial: 1, body: leb(70) + text("v0.15")))
        let all = try events(client)
        guard case let .failed(reason)? = all.first, all.count == 1 else { return XCTFail("\(all)") }
        XCTAssertTrue(reason.contains("70") && reason.contains("46"), reason)
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
