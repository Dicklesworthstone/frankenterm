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
    private func render(_ output: String, name: String,
                        configuration: NativeTerminalRenderConfiguration = .init()) async throws -> (Snapshot, NativeTerminalCaret?) {
        let coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(InertTransport())
        let model = try NativeTerminalModel(source: source, geometry: .init(columns: 40, rows: 12), scrollbackRows: 16)
        self.model = model
        try model.feed(Data(output.utf8))
        let frame = try model.captureFrame(viewportTop: nil)

        view?.removeFromSuperview()
        let view = try await NativeTerminalRenderView(source: source, bundle: .main, configuration: configuration)
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

    /// Summed glyph intensity per pixel column of row 0, cell 0 (cell geometry
    /// from the caret, cursor at row 1 column 0).
    private func firstCellColumnProfile(_ shot: Snapshot, _ caret: NativeTerminalCaret?) throws -> [Int] {
        let cell = try XCTUnwrap(caret?.rect, "the drawn frame must publish its caret")
        let height = Int(cell.height.rounded()), top = Int(cell.minY.rounded()) - height
        let width = Int(cell.width.rounded())
        XCTAssertGreaterThanOrEqual(top, 0)
        return (0..<width).map { x in
            (top..<(top + height)).reduce(0) { total, y in
                let (r, g, b) = shot.pixel(x, y)
                return total + max(0, (r + g + b) / 3 - 40)
            }
        }
    }

    /// U+E0B0 (powerline solid right-pointing triangle) is in the bundled Symbols
    /// Nerd Font Mono, not in Menlo. A filled triangle inks its left edge far more
    /// than its right; the missing-glyph box Menlo would draw is a symmetric
    /// outline. The same render with fallbacks disabled is the negative control.
    func testNerdFontSymbolsComeFromTheBundledFallback() async throws {
        let (shot, caret) = try await render("\u{e0b0}\r\n", name: "nerd-font-fallback")
        let profile = try firstCellColumnProfile(shot, caret)
        let inked = profile.indices.filter { profile[$0] > 100 }
        let left = try XCTUnwrap(inked.first), right = try XCTUnwrap(inked.last)
        XCTAssertGreaterThan(profile[left] + profile[left + 1], 3 * (profile[right] + profile[right - 1]),
                             "U+E0B0 must be the filled powerline triangle (profile \(profile))")

        var noFallback = NativeTerminalRenderConfiguration(); noFallback.fallbackFontNames = []
        let (control, controlCaret) = try await render("\u{e0b0}\r\n", name: "nerd-font-no-fallback", configuration: noFallback)
        let boxed = try firstCellColumnProfile(control, controlCaret)
        let boxInk = boxed.indices.filter { boxed[$0] > 100 }
        if let l = boxInk.first, let r = boxInk.last, r > l {
            XCTAssertLessThan(boxed[l] + boxed[l + 1], 3 * (boxed[r] + boxed[r - 1]),
                              "without the fallback the glyph must not look like the triangle (profile \(boxed))")
        }
    }

    /// A glyph's atlas bitmap must keep its own top at the top. "T" has a wide
    /// bar at the top of its ink and a narrow stem below, so an inverted atlas
    /// upload (the first iOS render drew every glyph upside down) puts the wide
    /// rows at the bottom. A symmetric-ink glyph such as a solid half block
    /// cannot detect this: glyph placement comes from the font's ink bounds,
    /// so flipping a solid rectangle changes nothing (verified with a planted
    /// flip, which a U+2580 check let pass).
    func testGlyphsAreDrawnUprightInTheirCells() async throws {
        let (shot, caret) = try await render(String(repeating: "T", count: 8) + "\r\n", name: "glyph-orientation")
        let cell = try XCTUnwrap(caret?.rect, "the drawn frame must publish its caret (cursor at row 1, column 0)")
        let cellHeight = Int(cell.height.rounded()), rowTop = Int(cell.minY.rounded()) - cellHeight
        XCTAssertGreaterThanOrEqual(rowTop, 0)
        XCTAssertGreaterThan(cellHeight, 4)
        let columns = min(shot.width, Int((cell.width * 8).rounded()))
        // Summed glyph intensity per scanline across the eight glyphs of row 0.
        // Intensity, not a light/dark threshold: an antialiased bar can straddle
        // two scanlines at half intensity each.
        let profile = (rowTop..<(rowTop + cellHeight)).map { y in
            (0..<columns).reduce(0) { total, x in
                let (r, g, b) = shot.pixel(x, y)
                return total + max(0, (r + g + b) / 3 - 40)
            }
        }
        let inked = profile.indices.filter { profile[$0] > 300 }
        let first = try XCTUnwrap(inked.first), last = try XCTUnwrap(inked.last)
        XCTAssertGreaterThan(last - first, 3, "the glyphs must span several scanlines")
        let top = profile[first] + profile[first + 1], bottom = profile[last] + profile[last - 1]
        XCTAssertGreaterThan(top, 2 * bottom, "a T's bar must be at the top of its ink, not the bottom (profile \(profile))")
    }
}
