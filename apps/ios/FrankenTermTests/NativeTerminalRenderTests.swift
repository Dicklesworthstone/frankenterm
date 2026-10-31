import XCTest
import UIKit
@testable import FrankenTermKit
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
    private var renderedFontName: String?
    override func tearDown() async throws {
        view?.removeFromSuperview(); view = nil
        try? model?.close(); model = nil
    }

    /// Feeds `output` to the real engine, draws its frame through Metal in the
    /// host app's window, and returns a 1x snapshot plus the drawn caret.
    private func render(_ output: String, name: String,
                        configuration: NativeTerminalRenderConfiguration = .init(),
                        powerPolicy: TerminalRenderPowerPolicy? = nil,
                        columns: UInt32 = 40, rows: UInt32 = 12) async throws -> (Snapshot, NativeTerminalCaret?) {
        let coordinator = try TerminalInputCoordinator()
        let source = try await coordinator.attach(InertTransport())
        let model = try NativeTerminalModel(source: source, geometry: .init(columns: columns, rows: rows), scrollbackRows: 16)
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
        renderedFontName = nil
        view.onAppearance = { [weak self] font, _ in
            self?.renderedFontName = font.fontName
            drawn.fulfill()
        }
        view.onCaret = { caret = $0 }
        view.onFailure = { failure = $0 }
        // Apply after window attachment refreshes system policy and before the
        // first frame; no older GPU completion can satisfy this baseline draw.
        if let powerPolicy { view.applyPowerPolicy(powerPolicy) }
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

    func testExportedFamilyAndFaceReachTheActualMetalGlyphFont() async throws {
        let installed = try XCTUnwrap(UIFont(name: "PragmasevkaNF-Italic", size: 12))
        let request = WorkspaceDocument.Font(family: installed.familyName, face: installed.fontName,
                                            pointSize: 30, fallbacks: [], variations: [:])
        let resolved = NativeTerminalFonts.resolve(request)
        XCTAssertEqual(resolved.primary.postScriptName, installed.fontName)
        XCTAssertTrue(resolved.notices.isEmpty)
        var configuration = NativeTerminalRenderConfiguration()
        configuration.fontName = resolved.primary.postScriptName
        configuration.fallbackFontNames = resolved.fallbackNames
        configuration.fontPointSize = 14
        let (shot, _) = try await render("Machine-specific font: MWi 0123", name: "exported-machine-font",
                                         configuration: configuration)
        XCTAssertEqual(renderedFontName, installed.fontName, "the completed draw must use the requested face")
        XCTAssertEqual(view?.configuration.fontPointSize, 14, "desktop point size does not replace phone zoom")
        XCTAssertTrue((0..<shot.height).contains { y in (0..<shot.width).contains { shot.isLight($0, y) } })
    }

    func testUnavailableExportedFontUsesTheDisclosedBundledSubstitution() async throws {
        let request = WorkspaceDocument.Font(family: "FrankenTerm-Missing-Font-7FC27D", face: nil,
                                            pointSize: nil, fallbacks: [], variations: [:])
        let resolved = NativeTerminalFonts.resolve(request)
        XCTAssertEqual(resolved.primary.postScriptName, "PragmasevkaNF")
        XCTAssertTrue(resolved.notices.contains { $0.contains(request.family) && $0.contains(resolved.primary.familyName) })
        var configuration = NativeTerminalRenderConfiguration()
        configuration.fontName = resolved.primary.postScriptName
        configuration.fallbackFontNames = resolved.fallbackNames
        _ = try await render("Readable installed fallback", name: "missing-machine-font", configuration: configuration)
        XCTAssertEqual(renderedFontName, resolved.primary.postScriptName)
    }

    func testOrdinaryCellsKeepAnExplicitItalicBaseFace() async throws {
        var configuration = NativeTerminalRenderConfiguration()
        configuration.fontName = "PragmasevkaNF-Italic"
        configuration.cursorBlink = false
        let text = "\u{1b}[?25lMWi ff 0123"
        let (plain, _) = try await render(text, name: "italic-base-ordinary-cells", configuration: configuration)
        let (emphasized, _) = try await render("\u{1b}[3m" + text,
            name: "italic-base-emphasized-cells", configuration: configuration)
        XCTAssertEqual(plain.width, emphasized.width)
        XCTAssertEqual(plain.height, emphasized.height)
        XCTAssertEqual(plain.bytes, emphasized.bytes,
            "ordinary cells in an italic base face must not silently become the regular face")
    }

    func testMemoryWarningReclaimsIdleCachesAndPreservesPendingTerminalOutput() async throws {
        var configuration = NativeTerminalRenderConfiguration()
        configuration.cursorBlink = false
        _ = try await render("Before memory pressure", name: "before-memory-pressure", configuration: configuration)
        let view = try XCTUnwrap(view), model = try XCTUnwrap(model)
        let original = try XCTUnwrap(view.admittedFrame)
        let cachedBefore = await view.cachedGPUResourceCount()
        XCTAssertGreaterThan(cachedBefore, 0)

        for _ in 0..<32 {
            NotificationCenter.default.post(name: UIApplication.didReceiveMemoryWarningNotification, object: nil)
        }
        let cachedAfter = await view.cachedGPUResourceCount()
        XCTAssertEqual(cachedAfter, 0)
        XCTAssertEqual(view.admittedFrame?.revision, original.revision)
        XCTAssertEqual(view.admittedFrame?.readableText, original.readableText)

        let drawn = expectation(description: "new output survives pressure during preparation")
        drawn.assertForOverFulfill = false
        var failure: TerminalRenderError?
        view.onAppearance = { _, _ in drawn.fulfill() }
        view.onFailure = { failure = $0 }
        try model.feed(Data("\r\nOutput after pressure".utf8))
        try view.submit(model.captureFrame(viewportTop: nil), source: view.source)
        // This can race the worker's preparation or the GPU submission. Each
        // owns its exact resource set independently from the cache reference.
        NotificationCenter.default.post(name: UIApplication.didReceiveMemoryWarningNotification, object: nil)
        await fulfillment(of: [drawn], timeout: 20)
        XCTAssertNil(failure)
        XCTAssertTrue(view.admittedFrame?.readableText.contains("Output after pressure") == true)
        XCTAssertEqual(view.admittedFrame?.source, original.source)
        XCTAssertEqual(view.admittedFrame?.columns, original.columns)
        XCTAssertEqual(view.admittedFrame?.rows, original.rows)
    }

    func testThermalPolicyStopsIdleBlinkWorkAndResumesWithoutNewOutput() async throws {
        _ = try await render("\u{1b}[5mBlinking output\u{1b}[0m", name: "before-thermal-pressure",
            powerPolicy: .init(thermalState: .nominal, lowPower: false))
        let view = try XCTUnwrap(view), original = try XCTUnwrap(view.admittedFrame)
        let steady = expectation(description: "retained blinking screen is presented visibly")
        steady.assertForOverFulfill = false
        var failure: TerminalRenderError?
        view.onAppearance = { _, _ in steady.fulfill() }
        view.onFailure = { failure = $0 }
        view.applyPowerPolicy(.init(thermalState: .critical, lowPower: false))
        await fulfillment(of: [steady], timeout: 20)
        XCTAssertNil(failure)
        XCTAssertEqual(view.admittedFrame?.revision, original.revision)

        let idle = expectation(description: "no decorative redraw under critical thermal pressure")
        idle.isInverted = true
        view.onAppearance = { _, _ in idle.fulfill() }
        await fulfillment(of: [idle], timeout: 0.8)

        let resumed = expectation(description: "cooling re-presents and then blinks retained output")
        resumed.expectedFulfillmentCount = 2
        resumed.assertForOverFulfill = false
        view.onAppearance = { _, _ in resumed.fulfill() }
        view.applyPowerPolicy(.init(thermalState: .nominal, lowPower: false))
        await fulfillment(of: [resumed], timeout: 20)
        XCTAssertNil(failure)
        XCTAssertEqual(view.admittedFrame?.revision, original.revision)
        XCTAssertEqual(view.admittedFrame?.source, original.source)
    }

    func testKeepDesktopSizeFitsTheWholeRemoteGridWithCaretAndPointerInTheSameCells() async throws {
        var configuration = NativeTerminalRenderConfiguration()
        configuration.fitsRemoteGrid = true
        let (shot, caret) = try await render("\u{1b}[1;31mDesktop grid\u{1b}[0m\u{1b}[60;180H",
            name: "observer-full-desktop-grid", configuration: configuration, columns: 180, rows: 60)
        let view = try XCTUnwrap(view), projection = try XCTUnwrap(view.pointerProjection)
        let rect = try XCTUnwrap(caret?.rect)
        XCTAssertEqual(projection.frame.columns, 180); XCTAssertEqual(projection.frame.rows, 60)
        XCTAssertEqual(view.configuration.fontPointSize, configuration.fontPointSize,
            "Fitting must preserve the person's preferred font for a later interactive session")
        XCTAssertGreaterThan(projection.cellSize.width, 0); XCTAssertGreaterThan(projection.cellSize.height, 0)
        XCTAssertLessThanOrEqual(projection.cellSize.width * 180, view.bounds.width + 0.01)
        XCTAssertLessThanOrEqual(projection.cellSize.height * 60, view.bounds.height + 0.01)
        XCTAssertGreaterThan(rect.minX, 0); XCTAssertGreaterThan(rect.minY, 0)
        XCTAssertLessThanOrEqual(rect.maxX, CGFloat(shot.width) + 0.01)
        XCTAssertLessThanOrEqual(rect.maxY, CGFloat(shot.height) + 0.01)
        XCTAssertEqual(rect.width, projection.cellSize.width, accuracy: 0.01)
        XCTAssertEqual(rect.height, projection.cellSize.height, accuracy: 0.01)
        let cell = try XCTUnwrap(projection.frame.columns == 180 ? caret : nil)
        XCTAssertEqual(cell.rect.minX, projection.cellSize.width * 179, accuracy: 0.01)
        XCTAssertEqual(cell.rect.minY, projection.cellSize.height * 59, accuracy: 0.01)
    }

    func testSplitFitChangesOnlyLocalProjectionAndCanReturnToThePreferredSize() async throws {
        var configuration = NativeTerminalRenderConfiguration()
        configuration.cursorBlink = false
        _ = try await render("Shared grid\u{1b}[60;180H", name: "before-split-fit",
            configuration: configuration, columns: 180, rows: 60)
        let view = try XCTUnwrap(view), original = try XCTUnwrap(view.admittedFrame)
        let size = try XCTUnwrap(view.cellSize)
        let fitted = expectation(description: "the retained frame completed at the companion's fitted scale")
        fitted.assertForOverFulfill = false
        view.onAppearance = { _, _ in fitted.fulfill() }
        try view.setFitsRemoteGrid(true)
        await fulfillment(of: [fitted], timeout: 20)
        let projection = try XCTUnwrap(view.pointerProjection)
        XCTAssertLessThan(projection.cellSize.width, size.width)
        XCTAssertLessThanOrEqual(projection.cellSize.width * 180, view.bounds.width + 0.01)
        XCTAssertEqual(view.admittedFrame?.revision, original.revision)
        XCTAssertEqual(view.configuration.fontRevision, configuration.fontRevision)
        let restored = expectation(description: "focusing again restores the original local scale")
        restored.assertForOverFulfill = false
        view.onAppearance = { _, _ in restored.fulfill() }
        try view.setFitsRemoteGrid(false)
        await fulfillment(of: [restored], timeout: 20)
        XCTAssertEqual(try XCTUnwrap(view.cellSize), size)
        XCTAssertEqual(view.admittedFrame?.source, original.source)
        XCTAssertEqual(view.admittedFrame?.columns, 180); XCTAssertEqual(view.admittedFrame?.rows, 60)
    }

    func testAccessibleOutputHasSeparateLinesAndDoesNotExposeConcealedText() async throws {
        _ = try await render("first line\r\nwide: 界 👩‍💻\r\n\u{1b}[8mprivate\u{1b}[0m\r\n$ ",
                             name: "accessible-terminal-lines")
        let view = try XCTUnwrap(view)
        let reading = NativeTerminalAccessibility(view: view)
        let elements = reading.elements().compactMap { $0 as? UIAccessibilityElement }
        XCTAssertEqual(elements.first?.accessibilityLabel, "Terminal output")
        XCTAssertGreaterThan(elements.count, 3)
        let labels = elements.dropFirst().compactMap(\.accessibilityLabel)
        XCTAssertTrue(labels.contains("first line"))
        XCTAssertTrue(labels.contains("wide: 界 👩‍💻"))
        XCTAssertFalse(labels.joined().contains("private"))
        for element in elements.dropFirst() {
            XCTAssertTrue(element.accessibilityTraits.contains(.staticText))
            XCTAssertFalse(element.accessibilityTraits.contains(.updatesFrequently))
            XCTAssertTrue(view.bounds.contains(element.accessibilityFrameInContainerSpace))
            XCTAssertEqual(element.accessibilityCustomActions?.count, 3)
        }
        // UIKit can retain the old rotor/actions. Their capture path must honor
        // the renderer's admission gate, even when invoked outside its getter.
        view.setSceneActive(false)
        XCTAssertTrue(reading.elements(refresh: true).isEmpty)
        XCTAssertNil(view.accessibilityValue)
        view.setSceneActive(true)
        XCTAssertFalse(reading.elements().isEmpty)
        // A paused reading snapshot owns only text. It can be discarded on
        // scene deactivation without closing the terminal or feeding input.
        reading.discard()
        XCTAssertFalse(try XCTUnwrap(model).captureFrame(viewportTop: nil).isEmpty)
    }

    func testPointerProjectionRequiresCompletedCurrentGeometryAndActiveScene() async throws {
        _ = try await render("\u{1b}[?1000h\u{1b}[?1006hmouse target\r\n$ ", name: "pointer-terminal-geometry")
        let view = try XCTUnwrap(view), model = try XCTUnwrap(model)
        let original = try XCTUnwrap(view.pointerProjection)
        XCTAssertTrue(original.frame.mouseGrabbed)
        XCTAssertGreaterThan(original.cellSize.width, 0); XCTAssertGreaterThan(original.cellSize.height, 0)
        view.setSceneActive(false)
        XCTAssertNil(view.pointerProjection)
        let resumed = expectation(description: "a new completed draw after scene activation")
        var resumedOnce = false
        view.onCaret = { _ in if !resumedOnce { resumedOnce = true; resumed.fulfill() } }
        view.setSceneActive(true)
        XCTAssertNil(view.pointerProjection, "an old completed draw cannot authorize a resumed gesture")
        await fulfillment(of: [resumed], timeout: 20)
        XCTAssertNotNil(view.pointerProjection)

        try model.resize(.init(columns: UInt32(original.frame.columns + 1), rows: UInt32(original.frame.rows)))
        let admitted = expectation(description: "the replacement grid is admitted before it is drawn")
        let resized = expectation(description: "a completed draw of the replacement grid")
        var admittedOnce = false, resizedOnce = false
        view.onFrameAdmitted = { frame in
            if !admittedOnce, frame.geometryRevision > original.frame.geometryRevision {
                admittedOnce = true
                XCTAssertNil(view.pointerProjection, "admission cannot reuse the old grid's completed draw")
                admitted.fulfill()
            }
        }
        view.onCaret = { _ in
            if !resizedOnce, let projection = view.pointerProjection,
               projection.frame.geometryRevision > original.frame.geometryRevision {
                resizedOnce = true; resized.fulfill()
            }
        }
        try view.submit(model.captureFrame(viewportTop: nil), source: model.source)
        await fulfillment(of: [admitted, resized], timeout: 20)
        let current = try XCTUnwrap(view.pointerProjection)
        XCTAssertGreaterThan(current.frame.geometryRevision, original.frame.geometryRevision)
        XCTAssertEqual(current.frame.columns, original.frame.columns + 1)
        view.retire(); view.setSceneActive(true)
        XCTAssertNil(view.pointerProjection, "retirement cannot reactivate pointer authority")
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
