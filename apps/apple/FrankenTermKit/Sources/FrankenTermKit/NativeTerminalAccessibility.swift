#if canImport(UIKit) && canImport(MetalKit) && canImport(CoreText)
import UIKit

/// UIKit's semantic reading surface for the Metal terminal. It is created only
/// when assistive technology asks for output. No work or announcements occur
/// on the render loop, and reading never acquires input or resize authority.
@MainActor
final class NativeTerminalAccessibility: NSObject {
    private weak var view: NativeTerminalRenderView?
    private var reading: TerminalAccessibilityReading
    private var renderedSnapshot: TerminalAccessibilitySnapshot?
    private var lineElements: [UIAccessibilityElement] = []
    private var header: UIAccessibilityElement?

    init(view: NativeTerminalRenderView) {
        self.view = view; reading = .init(source: view.source)
        super.init()
        NotificationCenter.default.addObserver(self, selector: #selector(focusChanged(_:)),
            name: UIAccessibility.elementFocusedNotification, object: nil)
    }

    deinit { NotificationCenter.default.removeObserver(self) }

    func discard() {
        reading.discard(); renderedSnapshot = nil; lineElements = []; header = nil
    }

    func elements(refresh: Bool = false) -> [Any] {
        guard let view, view.permitsAccessibilityReading, let frame = view.admittedFrame,
              let snapshot = try? reading.capture(frame, refresh: refresh) else {
            discard(); return []
        }
        if snapshot != renderedSnapshot {
            renderedSnapshot = snapshot
            let header = UIAccessibilityElement(accessibilityContainer: view)
            header.accessibilityLabel = "Terminal output"
            header.accessibilityTraits = .header
            header.accessibilityHint = "Swipe through captured lines. Use Actions to refresh, read the cursor line, or read the newest visible output."
            header.accessibilityCustomActions = actions()
            self.header = header
            lineElements = snapshot.lines.enumerated().map { index, line in
                let element = UIAccessibilityElement(accessibilityContainer: view)
                element.accessibilityIdentifier = "terminal.output.line.\(index)"
                element.accessibilityLabel = line.spokenText
                element.accessibilityTraits = .staticText
                element.accessibilityCustomActions = actions()
                return element
            }
        }
        let state = snapshot.isRetained ? "Saved output" : snapshot.isLive ? "Live screen" : "Scrollback"
        var status = "\(state). \(snapshot.lines.count) captured lines."
        if reading.isReading { status += " Reading stays in place until you refresh or leave terminal output." }
        if frame.revision != snapshot.revision { status += " Newer output is available." }
        if snapshot.omittedLines > 0 { status += " \(snapshot.omittedLines) earlier visible lines are outside the reading limit." }
        if snapshot.lines.isEmpty { status += " No visible text." }
        header?.accessibilityValue = status
        updateFrames(snapshot, in: view)
        return (header.map { [$0] } ?? []) + lineElements
    }

    func rotor() -> UIAccessibilityCustomRotor {
        UIAccessibilityCustomRotor(name: "Terminal lines") { [weak self] predicate in
            guard let self else { return nil }
            _ = self.elements()
            guard let snapshot = self.renderedSnapshot else { return nil }
            let target = predicate.currentItem.targetElement as AnyObject?
            let current = self.lineElements.firstIndex { $0 === target }
            guard let next = snapshot.nextLine(after: current, forward: predicate.searchDirection == .next),
                  self.lineElements.indices.contains(next) else { return nil }
            self.reading.setReading(true)
            return UIAccessibilityCustomRotorItemResult(targetElement: self.lineElements[next], targetRange: nil)
        }
    }

    private func actions() -> [UIAccessibilityCustomAction] {
        [UIAccessibilityCustomAction(name: "Refresh terminal output") { [weak self] _ in self?.move(to: .refresh) ?? false },
         UIAccessibilityCustomAction(name: "Read cursor line") { [weak self] _ in self?.move(to: .cursor) ?? false },
         UIAccessibilityCustomAction(name: "Read newest visible output") { [weak self] _ in self?.move(to: .newest) ?? false }]
    }

    private enum Destination { case refresh, cursor, newest }

    private func move(to destination: Destination) -> Bool {
        _ = elements(refresh: true)
        guard let snapshot = renderedSnapshot else { return false }
        reading.setReading(true)
        let index: Int?
        switch destination {
        case .refresh: index = snapshot.lines.indices.first
        case .cursor: index = snapshot.cursorLineIndex
        case .newest: index = snapshot.newestOutputIndex
        }
        guard let index, lineElements.indices.contains(index) else {
            // These announcements are explicit user requests, never output
            // notifications. A history viewport has no live cursor to read.
            UIAccessibility.post(notification: .announcement,
                argument: destination == .cursor ? "The cursor is not in the captured output." : "No visible text.")
            return true
        }
        UIAccessibility.post(notification: .layoutChanged, argument: lineElements[index])
        return true
    }

    private func updateFrames(_ snapshot: TerminalAccessibilitySnapshot, in view: UIView) {
        // The frozen text remains a reading snapshot if the visual terminal
        // scrolls underneath it. Frames stay within the current container even
        // after rotation; they never become remote selection coordinates.
        let height = view.bounds.height / CGFloat(max(1, snapshot.viewportRows))
        header?.accessibilityFrameInContainerSpace = CGRect(x: 0, y: 0,
            width: view.bounds.width, height: max(1, min(height, view.bounds.height)))
        for (index, line) in snapshot.lines.enumerated() {
            lineElements[index].accessibilityFrameInContainerSpace = CGRect(x: 0,
                y: CGFloat(line.viewportRows.lowerBound) * height, width: view.bounds.width,
                height: CGFloat(line.viewportRows.count) * height).intersection(view.bounds)
        }
    }

    @objc private func focusChanged(_ note: Notification) {
        guard let focused = note.userInfo?[UIAccessibility.focusedElementUserInfoKey] as AnyObject? else {
            reading.setReading(false); return
        }
        reading.setReading(header === focused || lineElements.contains { $0 === focused })
    }
}
#endif
