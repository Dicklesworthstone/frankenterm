#if canImport(UIKit)
import UIKit

/// The colors of the terminal's own theme, for the chrome drawn around it
/// (the key row above the keyboard, the selection), taken from the palette
/// of the frame on screen so they follow the machine's theme.
public struct TerminalChromeTheme: Equatable {
    public let background: UIColor, foreground: UIColor, accent: UIColor

    public init(background: UIColor, foreground: UIColor, accent: UIColor) {
        self.background = background; self.foreground = foreground; self.accent = accent
    }

    /// Default foreground/background are palette entries 256/257 (swapped in
    /// reverse video); the accent is the cursor color, or ANSI blue when the
    /// cursor is drawn in the text color.
    public init?(frame: TerminalRenderFrame) {
        let palette = frame.palette
        guard palette.count > 259 else { return nil }
        func color(_ value: TerminalRenderRGBA) -> UIColor {
            UIColor(red: CGFloat(value.red) / 255, green: CGFloat(value.green) / 255,
                    blue: CGFloat(value.blue) / 255, alpha: 1)
        }
        let foreground = palette[frame.reverseVideo ? 257 : 256]
        let background = palette[frame.reverseVideo ? 256 : 257]
        let cursor = palette[259]
        let distinct = cursor != foreground && cursor != background
        self.init(background: color(background), foreground: color(foreground),
                  accent: color(distinct ? cursor : palette[12]))
    }

    /// `color` mixed with the background: keycaps and highlights sit on the
    /// terminal's colors instead of the system's.
    public func tint(_ amount: CGFloat, of color: UIColor? = nil) -> UIColor {
        let top = color ?? foreground
        var (r1, g1, b1, a1) = (CGFloat(0), CGFloat(0), CGFloat(0), CGFloat(0))
        var (r2, g2, b2, a2) = (CGFloat(0), CGFloat(0), CGFloat(0), CGFloat(0))
        background.getRed(&r1, green: &g1, blue: &b1, alpha: &a1)
        top.getRed(&r2, green: &g2, blue: &b2, alpha: &a2)
        return UIColor(red: r1 + (r2 - r1) * amount, green: g1 + (g2 - g1) * amount,
                       blue: b1 + (b2 - b1) * amount, alpha: 1)
    }

    /// Black or white, whichever reads better on `color`.
    public static func label(on color: UIColor) -> UIColor {
        var (r, g, b, a) = (CGFloat(0), CGFloat(0), CGFloat(0), CGFloat(0))
        color.getRed(&r, green: &g, blue: &b, alpha: &a)
        return 0.2126 * r + 0.7152 * g + 0.0722 * b > 0.55 ? .black : .white
    }
}

/// Draws the selected cells over the terminal and the two handles that move
/// its ends. Only the handles take touches; everything else passes through.
@MainActor
final class NativeTerminalSelectionOverlay: UIView {
    let startHandle = SelectionHandle(atStart: true)
    let endHandle = SelectionHandle(atStart: false)
    private let highlight = CAShapeLayer()

    override init(frame: CGRect) {
        super.init(frame: frame)
        isOpaque = false; backgroundColor = .clear
        layer.addSublayer(highlight)
        addSubview(startHandle); addSubview(endHandle)
        startHandle.isHidden = true; endHandle.isHidden = true
    }
    @available(*, unavailable)
    required init?(coder: NSCoder) { nil }

    override func hitTest(_ point: CGPoint, with event: UIEvent?) -> UIView? {
        for handle in [startHandle, endHandle] where !handle.isHidden {
            if handle.frame.insetBy(dx: -12, dy: -12).contains(point) { return handle }
        }
        return nil
    }

    /// Lays the highlight over `frame`'s rows; ends off screen hide their handle.
    func show(_ selection: TerminalTextSelection?, frame: TerminalRenderFrame?, cell: CGSize?, tint: UIColor) {
        guard let selection, let frame, let cell, cell.width > 0, cell.height > 0 else {
            highlight.path = nil; startHandle.isHidden = true; endHandle.isHidden = true; return
        }
        let path = UIBezierPath()
        var startRect: CGRect?, endRect: CGRect?
        for (index, row) in frame.lines.enumerated() {
            guard let columns = selection.columns(onRow: row.stableIndex, width: frame.columns) else { continue }
            let rect = CGRect(x: CGFloat(columns.lowerBound) * cell.width, y: CGFloat(index) * cell.height,
                              width: CGFloat(columns.count) * cell.width, height: cell.height)
            path.append(UIBezierPath(rect: rect))
            if row.stableIndex == selection.start.row { startRect = rect }
            if row.stableIndex == selection.end.row { endRect = rect }
        }
        highlight.path = path.cgPath
        highlight.fillColor = tint.withAlphaComponent(0.35).cgColor
        for handle in [startHandle, endHandle] { handle.tint = tint }
        if let startRect {
            startHandle.isHidden = false
            startHandle.place(at: CGPoint(x: startRect.minX, y: startRect.minY), lineHeight: cell.height)
        } else { startHandle.isHidden = true }
        if let endRect {
            endHandle.isHidden = false
            endHandle.place(at: CGPoint(x: endRect.maxX, y: endRect.maxY), lineHeight: cell.height)
        } else { endHandle.isHidden = true }
    }

    /// The selection's bounds on screen, for placing the edit menu.
    var selectionBounds: CGRect? { highlight.path.map { $0.boundingBoxOfPath } }

    /// The system's selection grabber: a stem along the line and a knob, above
    /// the first character or below the last.
    final class SelectionHandle: UIView {
        let atStart: Bool
        var tint: UIColor = .tintColor { didSet { setNeedsDisplay() } }
        private var lineHeight: CGFloat = 16
        init(atStart: Bool) {
            self.atStart = atStart
            super.init(frame: .zero)
            isOpaque = false; backgroundColor = .clear
            isAccessibilityElement = true
            accessibilityLabel = atStart ? "Selection start" : "Selection end"
        }
        @available(*, unavailable)
        required init?(coder: NSCoder) { nil }

        /// `edge` is the selection's top-left corner (start) or bottom-right (end).
        func place(at edge: CGPoint, lineHeight: CGFloat) {
            self.lineHeight = lineHeight
            let knob: CGFloat = 10, width: CGFloat = 20
            let height = lineHeight + knob
            frame = CGRect(x: edge.x - width / 2, y: atStart ? edge.y - knob : edge.y - lineHeight,
                           width: width, height: height)
            setNeedsDisplay()
        }

        override func draw(_ rect: CGRect) {
            tint.setFill()
            let knob: CGFloat = 10
            let stem = CGRect(x: bounds.midX - 1, y: atStart ? knob : 0, width: 2, height: lineHeight)
            UIBezierPath(rect: stem).fill()
            let circle = CGRect(x: bounds.midX - knob / 2, y: atStart ? 0 : bounds.height - knob, width: knob, height: knob)
            UIBezierPath(ovalIn: circle).fill()
        }
    }
}
#endif
