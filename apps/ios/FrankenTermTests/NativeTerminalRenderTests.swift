import XCTest
import UIKit
import FrankenTermKit
import FrankenTermTerminal

// Hosted in the FrankenTerm app so the renderer loads the app's bundled
// background.metal/text.metal. Frames come from the real Rust engine
// (FrankenTermTerminal's XCFramework); nothing here is a fixture frame.

private actor InertTransport: TerminalByteTransport {
    nonisolated let identity: TerminalConnectionIdentity
    init() throws {
        identity = try .init(workspaceID: "render-test", machineID: "machine", routeID: "direct",
                             routeRevision: 1, transportID: UUID(), channelID: UUID())
    }
    func writeSome(_ bytes: Data) -> TerminalTransportWrite { .accepted(bytes.count) }
    func close() {}
}

private struct Snapshot {
    let image: UIImage, bytes: [UInt8], width: Int, height: Int
    func pixel(_ x: Int, _ y: Int) -> (Int, Int, Int) {
        let index = (y * width + x) * 4
        return (Int(bytes[index]), Int(bytes[index + 1]), Int(bytes[index + 2]))
    }
    func isLight(_ x: Int, _ y: Int) -> Bool {
        let (r, g, b) = pixel(x, y)
        return r > 150 && g > 150 && b > 150
    }
}

@MainActor
final class NativeTerminalRenderTests: XCTestCase {
    private var model: NativeTerminalModel?
    private var view: NativeTerminalRenderView?
    override func tearDown() async throws {
        view?.removeFromSuperview(); view = nil
        try? model?.close(); model = nil
    }

    /// Feeds `output` to the real engine, draws its frame through Metal in the
    /// host app's window, and returns a 1x snapshot plus the drawn caret.
    private func render(_ output: String, name: String) async throws -> (Snapshot, NativeTerminalCaret?) {
        let coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(InertTransport())
        let model = try NativeTerminalModel(source: source, geometry: .init(columns: 40, rows: 12), scrollbackRows: 16)
        self.model = model
        try model.feed(Data(output.utf8))
        let frame = try model.captureFrame(viewportTop: nil)

        let view = try await NativeTerminalRenderView(source: source, bundle: .main)
        self.view = view
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let window = try XCTUnwrap(scenes.flatMap(\.windows).first { $0.isKeyWindow } ?? scenes.first?.windows.first)
        view.frame = CGRect(x: 0, y: 0, width: min(window.bounds.width, 402), height: 260)
        window.addSubview(view)
        let drawn = expectation(description: "a submitted Metal command buffer completed")
        drawn.assertForOverFulfill = false
        var failure: TerminalRenderError?, caret: NativeTerminalCaret?
        view.onAppearance = { _, _ in drawn.fulfill() }
        view.onCaret = { caret = $0 }
        view.onFailure = { failure = $0 }
        try view.submit(frame, source: source)
        await fulfillment(of: [drawn], timeout: 20)
        XCTAssertNil(failure)

        let format = UIGraphicsImageRendererFormat(); format.scale = 1
        let image = UIGraphicsImageRenderer(bounds: view.bounds, format: format).image { _ in
            _ = view.drawHierarchy(in: view.bounds, afterScreenUpdates: true)
        }
        let cgImage = try XCTUnwrap(image.cgImage)
        let width = cgImage.width, height = cgImage.height
        var bytes = [UInt8](repeating: 0, count: width * height * 4)
        let context = try XCTUnwrap(CGContext(data: &bytes, width: width, height: height, bitsPerComponent: 8,
            bytesPerRow: width * 4, space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue))
        context.draw(cgImage, in: CGRect(x: 0, y: 0, width: width, height: height))
        let attachment = XCTAttachment(image: image)
        attachment.name = name; attachment.lifetime = .keepAlways
        add(attachment)
        return (Snapshot(image: image, bytes: bytes, width: width, height: height), caret)
    }

    func testCanonicalTerminalFrameIsDrawnThroughMetal() async throws {
        let (shot, _) = try await render(
            "\u{1b}[1;32mFrankenTerm\u{1b}[0m on iOS: Rust parser -> Metal\r\n"
            + "\u{1b}[30;41m  red  \u{1b}[0m \u{1b}[30;42m green \u{1b}[0m \u{1b}[30;44m blue \u{1b}[0m\r\n"
            + "unicode: \u{00e9}\u{00e8} \u{4f60}\u{597d} \u{2603}\r\n$ ", name: "canonical-terminal-metal-frame")
        // The SGR 41/42/44 runs paint saturated red, green and blue cell
        // backgrounds; the default background is dark. Count each.
        var red = 0, green = 0, blue = 0, light = 0
        for y in 0..<shot.height {
            for x in 0..<shot.width {
                let (r, g, b) = shot.pixel(x, y)
                if r > 150 && g < 110 && b < 110 { red += 1 }
                if g > 150 && r < 110 && b < 110 { green += 1 }
                if b > 150 && r < 110 && g < 130 { blue += 1 }
                if shot.isLight(x, y) { light += 1 }
            }
        }
        XCTAssertGreaterThan(red, 200, "SGR 41 background cells were not drawn")
        XCTAssertGreaterThan(green, 200, "SGR 42 background cells were not drawn")
        XCTAssertGreaterThan(blue, 200, "SGR 44 background cells were not drawn")
        XCTAssertGreaterThan(light, 50, "default-foreground glyphs were not rasterized")
    }

    /// U+2580 UPPER HALF BLOCK fills the top half of its cell. An inverted atlas
    /// upload (the first iOS render drew every glyph upside down) lights the
    /// bottom half instead; colour/coverage counts alone cannot see that.
    func testGlyphsAreDrawnUprightInTheirCells() async throws {
        let (shot, caret) = try await render(String(repeating: "\u{2580}", count: 8) + "\r\n",
                                             name: "upper-half-block-orientation")
        let cell = try XCTUnwrap(caret?.rect, "the drawn frame must publish its caret (cursor at row 1, column 0)")
        let cellHeight = Int(cell.height.rounded()), rowTop = Int(cell.minY.rounded()) - cellHeight
        XCTAssertGreaterThanOrEqual(rowTop, 0)
        XCTAssertGreaterThan(cellHeight, 4)
        let half = cellHeight / 2, columns = min(shot.width, Int((cell.width * 8).rounded()))
        var upper = 0, lower = 0
        for x in 0..<columns {
            for y in rowTop..<(rowTop + half) where shot.isLight(x, y) { upper += 1 }
            for y in (rowTop + cellHeight - half)..<(rowTop + cellHeight) where shot.isLight(x, y) { lower += 1 }
        }
        XCTAssertGreaterThan(upper, columns * half / 2, "the upper half of the cells must be filled")
        XCTAssertLessThan(lower, upper / 5, "the lower half of the cells must stay background")
    }
}
