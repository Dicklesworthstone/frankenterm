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
        if view.localViewportEnabled {
            status += " Readable size. Actions can pan the local view, reveal the cursor, or fit the entire pane."
        }
        header?.accessibilityValue = status
        // A local transform does not change the captured text or its elements.
        // Refresh the available view actions without moving reading focus.
        header?.accessibilityCustomActions = actions()
        for element in lineElements { element.accessibilityCustomActions = actions() }
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
        var result = [UIAccessibilityCustomAction(name: "Refresh terminal output") { [weak self] _ in self?.move(to: .refresh) ?? false },
         UIAccessibilityCustomAction(name: "Read cursor line") { [weak self] _ in self?.move(to: .cursor) ?? false },
         UIAccessibilityCustomAction(name: "Read newest visible output") { [weak self] _ in self?.move(to: .newest) ?? false }]
        if view?.localViewportEnabled == true {
            result += [
                UIAccessibilityCustomAction(name: "Pan left") { [weak self] _ in self?.pan(horizontal: -1, vertical: 0) ?? false },
                UIAccessibilityCustomAction(name: "Pan right") { [weak self] _ in self?.pan(horizontal: 1, vertical: 0) ?? false },
                UIAccessibilityCustomAction(name: "Pan up") { [weak self] _ in self?.pan(horizontal: 0, vertical: -1) ?? false },
                UIAccessibilityCustomAction(name: "Pan down") { [weak self] _ in self?.pan(horizontal: 0, vertical: 1) ?? false },
                UIAccessibilityCustomAction(name: "Reveal cursor") { [weak self] _ in self?.transform(revealCursor: true) ?? false },
                UIAccessibilityCustomAction(name: "Fit entire pane") { [weak self] _ in self?.transform(revealCursor: false) ?? false }
            ]
        }
        return result
    }

    private func pan(horizontal: Double, vertical: Double) -> Bool {
        guard let view, view.permitsAccessibilityReading, let projection = view.pointerProjection else { return false }
        return view.panLocalViewport(
            columns: horizontal * max(1, projection.viewport.width / projection.viewport.cellWidth / 2),
            rows: vertical * max(1, projection.viewport.height / projection.viewport.cellHeight / 2))
    }

    private func transform(revealCursor: Bool) -> Bool {
        guard let view, view.permitsAccessibilityReading, view.localViewportEnabled else { return false }
        if revealCursor {
            // The owning terminal can return a history viewport to live
            // output before revealing its cursor. A stale rotor never looks
            // up a different scene or acquires its keyboard authority.
            if let request = view.onCursorRevealRequest { return request() }
            guard view.admittedFrame?.isLive == true else { return false }
        }
        do {
            if revealCursor { try view.revealLocalCursor() }
            else { try view.setLocalViewportEnabled(false) }
            return true
        } catch { view.onFailure?((error as? TerminalRenderError) ?? .unavailable); return false }
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

    private func updateFrames(_ snapshot: TerminalAccessibilitySnapshot, in view: NativeTerminalRenderView) {
        let height = view.bounds.height / CGFloat(max(1, snapshot.viewportRows))
        header?.accessibilityFrameInContainerSpace = CGRect(x: 0, y: 0,
            width: view.bounds.width, height: max(1, min(height, view.bounds.height)))
        if let projection = view.pointerProjection,
           snapshot.source == projection.frame.source, snapshot.revision == projection.frame.revision,
           snapshot.geometryRevision == projection.frame.geometryRevision {
            for (index, line) in snapshot.lines.enumerated() {
                let first = line.firstRow.subtractingReportingOverflow(projection.frame.viewportFirst)
                let last = line.lastRow.subtractingReportingOverflow(projection.frame.viewportFirst)
                guard !first.overflow, !last.overflow, first.partialValue >= 0,
                      last.partialValue >= first.partialValue, last.partialValue < Int64(projection.frame.rows),
                      let row = projection.viewport.rectangle(column: 0, row: Int(first.partialValue),
                          columns: projection.frame.columns, rows: Int(last.partialValue - first.partialValue + 1)),
                      let visible = projection.viewport.clipped(row) else {
                    // The bounded transcript remains readable with the rotor,
                    // but offscreen text must not claim a visible touch target.
                    lineElements[index].accessibilityFrameInContainerSpace = .zero
                    continue
                }
                lineElements[index].accessibilityFrameInContainerSpace = CGRect(x: visible.x, y: visible.y,
                    width: visible.width, height: visible.height).intersection(view.bounds)
            }
            return
        }
        // Frozen older output is a separate reading snapshot. Its bounded
        // frames are not selection coordinates for the newer visual terminal.
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
