// ft-yccm0.7 defect 2 probe: which opacity flag of a covering window makes
// AppKit report the window beneath it occluded? NSWindow.isOpaque, the content
// CAMetalLayer's opaque flag, or both. Small windows, a few seconds, no audio,
// never activates (accessory app; windows ordered front regardless).
// usage: open -g -n Occl.app --args OUT_FILE
import AppKit
import Metal
import QuartzCore

let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "/tmp/occl-result.txt"
var lines: [String] = []
func log(_ s: String) { lines.append(s) }

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let device = MTLCreateSystemDefaultDevice()!
let queue = device.makeCommandQueue()!

final class MetalView: NSView {
    override func makeBackingLayer() -> CALayer {
        let l = CAMetalLayer()
        l.isOpaque = false
        return l
    }
    override var isOpaque: Bool { false }
    override var isFlipped: Bool { true }
}

struct Spec { let windowOpaque: Bool; let layerOpaque: Bool; let alpha: Double; var square: Bool = false; var grow: CGFloat = 0; var extendTop: CGFloat = 0; var solidTitlebar: Bool = false; var bg: NSColor = .clear; var plain: Bool = false; var fullSize: Bool = true }

func makeWindow(_ frame: NSRect, _ s: Spec, red: Double) -> NSWindow {
    var frame = frame.insetBy(dx: -s.grow, dy: -s.grow)
    frame.size.height += s.extendTop
    var style: NSWindow.StyleMask = s.square
        ? [.closable, .miniaturizable, .resizable, .fullSizeContentView]
        : [.titled, .closable, .miniaturizable, .resizable, .fullSizeContentView]
    if !s.fullSize { style.remove(.fullSizeContentView) }
    if s.plain {
        let w = NSWindow(contentRect: frame, styleMask: style, backing: .buffered, defer: false)
        w.isReleasedWhenClosed = false
        w.hasShadow = false
        w.orderFrontRegardless()
        return w
    }
    let w = NSWindow(contentRect: frame, styleMask: style, backing: .buffered, defer: false)
    w.titleVisibility = s.solidTitlebar ? .visible : .hidden
    w.titlebarAppearsTransparent = !s.solidTitlebar
    for b in [NSWindow.ButtonType.closeButton, .miniaturizeButton, .zoomButton] {
        w.standardWindowButton(b)?.isHidden = true
    }
    w.isReleasedWhenClosed = false
    w.backgroundColor = s.bg
    w.isOpaque = s.windowOpaque
    w.hasShadow = false
    let v = MetalView(frame: NSRect(origin: .zero, size: frame.size))
    v.wantsLayer = true
    w.contentView = v
    let layer = v.layer as! CAMetalLayer
    layer.device = device
    layer.pixelFormat = .bgra8Unorm
    layer.isOpaque = s.layerOpaque
    let scale = w.backingScaleFactor
    layer.contentsScale = scale
    layer.drawableSize = CGSize(width: frame.width * scale, height: frame.height * scale)
    w.orderFrontRegardless()
    if let d = layer.nextDrawable() {
        let rp = MTLRenderPassDescriptor()
        rp.colorAttachments[0].texture = d.texture
        rp.colorAttachments[0].loadAction = .clear
        rp.colorAttachments[0].storeAction = .store
        let a = s.alpha
        rp.colorAttachments[0].clearColor = MTLClearColor(red: red * a, green: 0.1 * a, blue: 0.2 * a, alpha: a)
        let cb = queue.makeCommandBuffer()!
        cb.makeRenderCommandEncoder(descriptor: rp)!.endEncoding()
        cb.present(d)
        cb.commit()
    } else {
        log("  (no drawable)")
    }
    return w
}

func visible(_ w: NSWindow) -> String { w.occlusionState.contains(.visible) ? "VISIBLE" : "occluded" }

// name, covering windows from bottom-most to top-most (the probed window is below all of them)
let glass = Spec(windowOpaque: false, layerOpaque: false, alpha: 0.87)
let opq = Spec(windowOpaque: true, layerOpaque: true, alpha: 1)
let cases: [(String, [Spec])] = [
    ("control: nothing above", []),
    ("titled PLAIN default window (no metal), +20", [Spec(windowOpaque: true, layerOpaque: true, alpha: 1, grow: 20, plain: true)]),
    ("titled PLAIN default window, not full-size content, +20", [Spec(windowOpaque: true, layerOpaque: true, alpha: 1, grow: 20, plain: true, fullSize: false)]),
    ("titled metal, solid titlebar, black bg, not full-size, +20", [Spec(windowOpaque: true, layerOpaque: true, alpha: 1, grow: 20, solidTitlebar: true, bg: .black, fullSize: false)]),
    ("titled metal, solid titlebar, black bg, full-size, +20", [Spec(windowOpaque: true, layerOpaque: true, alpha: 1, grow: 20, solidTitlebar: true, bg: .black)]),
    ("square PLAIN default window, same frame", [Spec(windowOpaque: true, layerOpaque: true, alpha: 1, square: true, plain: true)]),
    ("square metal opaque, same frame (repeat)", [Spec(windowOpaque: true, layerOpaque: true, alpha: 1, square: true)]),
]

let vf = NSScreen.main!.visibleFrame
let frame = NSRect(x: vf.minX + 40, y: vf.minY + 40, width: 260, height: 170)
log("macOS \(ProcessInfo.processInfo.operatingSystemVersionString), frame \(frame)")

var idx = 0
func runNext() {
    guard idx < cases.count else {
        try? (lines.joined(separator: "\n") + "\n").write(toFile: out, atomically: true, encoding: .utf8)
        app.terminate(nil)
        return
    }
    let (name, covers) = cases[idx]
    idx += 1
    let probe = makeWindow(frame, Spec(windowOpaque: true, layerOpaque: true, alpha: 1), red: 0.9)
    var above: [NSWindow] = []
    for (i, s) in covers.enumerated() { above.append(makeWindow(frame, s, red: 0.2 + 0.2 * Double(i))) }
    DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
        let top = above.map { visible($0) }.joined(separator: ",")
        let mine = (CGWindowListCopyWindowInfo([.optionOnScreenOnly], kCGNullWindowID) as? [[String: Any]] ?? [])
            .filter { ($0[kCGWindowOwnerPID as String] as? Int32) == ProcessInfo.processInfo.processIdentifier }
            .map { w -> String in
                let n = w[kCGWindowNumber as String] as? Int ?? -1
                let tag = n == probe.windowNumber ? "probe" : (above.firstIndex { $0.windowNumber == n }.map { "cover\($0)" } ?? "other")
                let b = w[kCGWindowBounds as String] as? [String: Any] ?? [:]
                return "\(tag)@L\(w[kCGWindowLayer as String] ?? "?")(\(b["Width"] ?? "?")x\(b["Height"] ?? "?"))"
            }.joined(separator: " > ")
        log("\(name): probe \(visible(probe)) [covers bottom->top: \(top)] z front->back: \(mine)")
        for w in above.reversed() { w.orderOut(nil); w.close() }
        probe.orderOut(nil); probe.close()
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { runNext() }
    }
}
DispatchQueue.main.asyncAfter(deadline: .now() + 30) {
    log("timeout")
    try? (lines.joined(separator: "\n") + "\n").write(toFile: out, atomically: true, encoding: .utf8)
    exit(3)
}
DispatchQueue.main.async { runNext() }
app.run()
