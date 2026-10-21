#if canImport(UIKit)
import UIKit
import UIKit.UIGestureRecognizerSubclass

/// Native machine/pane rail and canvas gestures, sharing one transactional
/// navigation coordinator with pickers and accessibility actions. The terminal
/// surface remains the existing Metal view; no text is drawn by this control.
/// The scene owns source registration, frame routing and network admission.
@MainActor
public final class NativeTerminalNavigationControls: UIView, UIGestureRecognizerDelegate {
    public var onFailure: (@MainActor (Error) -> Void)?
    /// Called only after a complete focus/identity publication. The scene can now
    /// install the focus owner's new keyboard and exact-source Metal surface.
    public var onSelection: (@MainActor (TerminalNavigationSelection) -> Void)?
    private let coordinator: TerminalNavigationCoordinator
    private let permitsCanvasNavigation: @MainActor () -> Bool
    private let machine = UIButton(type: .system), pane = UIButton(type: .system)
    private let railGesture = TerminalRingGesture(target: nil, action: nil)
    private let canvasGesture = TerminalRingGesture(target: nil, action: nil)
    private weak var canvas: UIView?
    private var observation: UUID?
    private var action: Task<Void, Never>?
    private var retired = false
    private var lastSelection: TerminalNavigationSelection?
    private var lastPhase: TerminalNavigationCoordinator.Phase?

    /// permitsCanvasNavigation must be false while selection, horizontal panning,
    /// terminal mouse interaction or another application-owned gesture has focus.
    /// The identity rail and explicit controls remain separate alternatives.
    public init(coordinator: TerminalNavigationCoordinator, terminalCanvas: UIView,
                permitsCanvasNavigation: @escaping @MainActor () -> Bool) throws {
        self.coordinator = coordinator; self.canvas = terminalCanvas
        self.permitsCanvasNavigation = permitsCanvasNavigation
        super.init(frame: .zero)
        // Reserve the only throwing resource before installing recognizers on
        // someone else's view. Failure must not leave an orphaned canvas gesture.
        observation = try coordinator.observe { [weak self] in self?.refresh() }
        backgroundColor = .secondarySystemBackground
        directionalLayoutMargins = .init(top: 0, leading: 8, bottom: 0, trailing: 8)
        let stack = UIStackView(arrangedSubviews: [machine, pane])
        stack.axis = .horizontal; stack.distribution = .fillEqually; stack.spacing = 8
        stack.translatesAutoresizingMaskIntoConstraints = false; addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: layoutMarginsGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: layoutMarginsGuide.trailingAnchor),
            stack.topAnchor.constraint(equalTo: topAnchor), stack.bottomAnchor.constraint(equalTo: bottomAnchor),
            heightAnchor.constraint(greaterThanOrEqualToConstant: 44)
        ])
        for button in [machine, pane] {
            button.showsMenuAsPrimaryAction = true
            button.titleLabel?.font = .preferredFont(forTextStyle: .subheadline)
            button.titleLabel?.adjustsFontForContentSizeCategory = true
            button.titleLabel?.lineBreakMode = .byTruncatingTail
        }
        machine.accessibilityHint = "Choose a machine. Swipe this rail with one finger to change machines."
        pane.accessibilityHint = "Choose a pane. Swipe the terminal with two fingers to change panes."
        for gesture in [railGesture, canvasGesture] {
            gesture.delegate = self
            gesture.allowedTouchTypes = [NSNumber(value: UITouch.TouchType.direct.rawValue)]
            gesture.capture = { [weak self] in self?.capturePlans() }
            gesture.accept = { [weak self] in self?.perform($0) }
        }
        railGesture.allowed = { [weak self] in
            guard let self else { return false }
            return !self.retired && !UIAccessibility.isVoiceOverRunning && !UIAccessibility.isSwitchControlRunning
        }
        canvasGesture.allowed = { [weak self] in
            guard let self else { return false }
            return !self.retired && !UIAccessibility.isVoiceOverRunning && !UIAccessibility.isSwitchControlRunning
                && self.permitsCanvasNavigation()
        }
        railGesture.machineOnly = true
        isMultipleTouchEnabled = true; terminalCanvas.isMultipleTouchEnabled = true
        addGestureRecognizer(railGesture); terminalCanvas.addGestureRecognizer(canvasGesture)
        machine.accessibilityCustomActions = [
            UIAccessibilityCustomAction(name: "Previous machine", target: self, selector: #selector(previousMachine)),
            UIAccessibilityCustomAction(name: "Next machine", target: self, selector: #selector(nextMachine))
        ]
        pane.accessibilityCustomActions = [
            UIAccessibilityCustomAction(name: "Previous pane", target: self, selector: #selector(previousPane)),
            UIAccessibilityCustomAction(name: "Next pane", target: self, selector: #selector(nextPane))
        ]
        refresh()
    }
    @available(*, unavailable)
    public required init?(coder: NSCoder) { nil }
    deinit {
        action?.cancel()
        let coordinator = coordinator, observation = observation
        Task { @MainActor in if let observation { coordinator.removeObserver(observation) } }
    }

    /// Call when removing the scene's terminal controls. This retires keyboard
    /// focus, not remote sessions; the app's connection owner controls those.
    public func retire() {
        guard !retired else { return }
        retired = true; action?.cancel(); coordinator.suspend()
        if let observation { coordinator.removeObserver(observation) }
        observation = nil
        canvas?.removeGestureRecognizer(canvasGesture); removeGestureRecognizer(railGesture)
        canvasGesture.capture = nil; canvasGesture.accept = nil; canvasGesture.allowed = nil
        railGesture.capture = nil; railGesture.accept = nil; railGesture.allowed = nil
        machine.menu = nil; pane.menu = nil
        machine.accessibilityCustomActions = nil; pane.accessibilityCustomActions = nil
    }

    public func refresh() {
        guard !retired else { return }
        let model = coordinator.navigation, selection = model.selection, phase = coordinator.phase
        let selected = model.machines.first(where: { $0.id == selection?.machineID })
        let machineIndex = selected.flatMap { chosen in model.machines.firstIndex(where: { $0.id == chosen.id }) }
        let paneIndex = selected?.panes.firstIndex(where: { $0.id == selection?.paneID })
        machine.setTitle(selected.map { String($0.name.prefix(80)) } ?? "Choose machine", for: .normal)
        let paneTitle: String
        if let selected, let paneIndex { paneTitle = "Pane \(paneIndex + 1)/\(selected.panes.count) · \(String(selected.panes[paneIndex].title.prefix(60)))" }
        else { paneTitle = "No pane selected" }
        pane.setTitle(paneTitle, for: .normal)
        machine.accessibilityLabel = "Machine: \(selected?.name ?? "none")"
        machine.accessibilityValue = machineIndex.map { "\($0 + 1) of \(model.machines.count)" }
        pane.accessibilityLabel = paneTitle
        let enabled = phase == .ready || phase == .inactive
        machine.isEnabled = enabled && !model.machines.isEmpty
        pane.isEnabled = enabled && selected?.panes.isEmpty == false
        machine.menu = UIMenu(title: "Machines", children: model.machines.map { candidate in
            UIAction(title: String(candidate.name.prefix(120)), state: candidate.id == selection?.machineID ? .on : .off) { [weak self] _ in
                guard let self else { return }
                do {
                    let plan = try self.coordinator.navigation.plan(machineID: candidate.id)
                        ?? self.coordinator.navigation.activationPlan()
                    if let plan { self.perform(plan) }
                } catch { self.onFailure?(error) }
            }
        })
        pane.menu = UIMenu(title: "Panes", children: (selected?.panes ?? []).enumerated().map { index, candidate in
            UIAction(title: "\(index + 1). \(String(candidate.title.prefix(120)))", state: candidate.id == selection?.paneID ? .on : .off) { [weak self] _ in
                guard let self else { return }
                do {
                    let plan = try self.coordinator.navigation.plan(paneID: candidate.id)
                        ?? self.coordinator.navigation.activationPlan()
                    if let plan { self.perform(plan) }
                } catch { self.onFailure?(error) }
            }
        })
        let changed = lastSelection != selection || lastPhase != phase
        lastSelection = selection; lastPhase = phase
        if changed, phase == .ready, let selection { onSelection?(selection) }
    }

    private func capturePlans() -> TerminalRingGesture.Plans? {
        guard !retired, action == nil, coordinator.phase == .ready || coordinator.phase == .inactive else { return nil }
        let model = coordinator.navigation
        return .init(machinePrevious: try? model.plan(axis: .machine, direction: .previous),
                     machineNext: try? model.plan(axis: .machine, direction: .next),
                     panePrevious: try? model.plan(axis: .pane, direction: .previous),
                     paneNext: try? model.plan(axis: .pane, direction: .next))
    }
    private func perform(_ plan: TerminalNavigationPlan) {
        guard !retired, action == nil else { return }
        action = Task { [weak self] in
            guard let self else { return }
            defer { self.action = nil }
            do { try await self.coordinator.perform(plan) }
            catch { if !self.retired, !Task.isCancelled { self.onFailure?(error) } }
        }
    }
    private func step(_ axis: TerminalNavigationAxis, _ direction: TerminalNavigationDirection) -> Bool {
        do {
            guard !retired, action == nil,
                  let plan = try coordinator.navigation.plan(axis: axis, direction: direction) else { return false }
            perform(plan); return true
        } catch { onFailure?(error); return false }
    }
    @objc private func previousMachine() -> Bool { step(.machine, .previous) }
    @objc private func nextMachine() -> Bool { step(.machine, .next) }
    @objc private func previousPane() -> Bool { step(.pane, .previous) }
    @objc private func nextPane() -> Bool { step(.pane, .next) }

    public func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer, shouldReceive touch: UITouch) -> Bool {
        guard !retired, !UIAccessibility.isVoiceOverRunning, !UIAccessibility.isSwitchControlRunning,
              touch.type == .direct else { return false }
        if gestureRecognizer === canvasGesture, !permitsCanvasNavigation() { return false }
        guard let view = gestureRecognizer.view else { return false }
        let point = touch.location(in: view)
        // Leave system screen-edge gestures alone; direct controls remain usable.
        return point.x >= 24 && point.x <= view.bounds.width - 24
    }
    // UIView declares this too; one override serves both the view's own check and
    // the delegate role for recognizers it owns.
    public override func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        !retired && action == nil && (gestureRecognizer !== canvasGesture || permitsCanvasNavigation())
    }
    public func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                                  shouldRequireFailureOf otherGestureRecognizer: UIGestureRecognizer) -> Bool {
        otherGestureRecognizer is UIScreenEdgePanGestureRecognizer
    }
}

/// One recognizer classifies finger count once. Using two competing pan
/// recognizers with mutual failure rules can defer recognition until touch-up.
/// The four immutable candidates are captured at first touch, not at release.
@MainActor
private final class TerminalRingGesture: UIGestureRecognizer {
    struct Plans {
        let machinePrevious: TerminalNavigationPlan?, machineNext: TerminalNavigationPlan?
        let panePrevious: TerminalNavigationPlan?, paneNext: TerminalNavigationPlan?
        func plan(_ axis: TerminalNavigationAxis, _ direction: TerminalNavigationDirection) -> TerminalNavigationPlan? {
            if axis == .machine { return direction == .next ? machineNext : machinePrevious }
            return direction == .next ? paneNext : panePrevious
        }
    }
    var machineOnly = false
    var capture: (@MainActor () -> Plans?)?
    var accept: (@MainActor (TerminalNavigationPlan) -> Void)?
    var allowed: (@MainActor () -> Bool)?
    private var plans: Plans?, classifier: TerminalNavigationGesture?
    private var touches = Set<UITouch>()
    private var origin = CGPoint.zero, previous = CGPoint.zero
    private var started: TimeInterval = 0, lastSample: TimeInterval = 0, velocityX: Double = 0
    private var initialSpan: CGFloat?

    override func reset() {
        super.reset()
        touches.removeAll(); plans = nil; classifier = nil; initialSpan = nil
        origin = .zero; previous = .zero; started = 0; lastSample = 0; velocityX = 0
    }
    private func cancelGesture() {
        classifier?.cancel()
        state = state == .possible ? .failed : .cancelled
    }
    private func centroid() -> CGPoint {
        let points = touches.map { $0.location(in: view) }
        guard !points.isEmpty else { return .zero }
        return .init(x: points.reduce(0) { $0 + $1.x } / CGFloat(points.count),
                     y: points.reduce(0) { $0 + $1.y } / CGFloat(points.count))
    }
    private func span() -> CGFloat? {
        guard touches.count == 2 else { return nil }
        let points = touches.map { $0.location(in: view) }
        return hypot(points[0].x - points[1].x, points[0].y - points[1].y)
    }
    override func touchesBegan(_ incoming: Set<UITouch>, with event: UIEvent) {
        guard state == .possible || state == .began || state == .changed else { return }
        guard allowed?() == true else { cancelGesture(); return }
        touches.formUnion(incoming)
        guard touches.count <= (machineOnly ? 1 : 2) else { cancelGesture(); return }
        let point = centroid(), timestamp = incoming.map(\.timestamp).max() ?? event.timestamp
        if classifier == nil {
            guard let plans = capture?() else { state = .failed; return }
            self.plans = plans
            classifier = try? TerminalNavigationGesture(touches: touches.count)
            origin = point; previous = point; started = timestamp; lastSample = timestamp
        } else {
            _ = classifier?.update(touches: touches.count, x: Double(point.x - origin.x),
                                    y: Double(point.y - origin.y), elapsed: timestamp - started)
            previous = point; lastSample = timestamp; velocityX = 0
            if classifier?.state == .cancelled { cancelGesture(); return }
        }
        initialSpan = span()
    }
    override func touchesMoved(_ incoming: Set<UITouch>, with event: UIEvent) {
        guard state == .possible || state == .began || state == .changed else { return }
        guard allowed?() == true else { cancelGesture(); return }
        if let initialSpan, let currentSpan = span(), abs(currentSpan - initialSpan) > max(12, initialSpan * 0.12) {
            cancelGesture(); return
        }
        let point = centroid(), timestamp = incoming.map(\.timestamp).max() ?? event.timestamp
        if timestamp > lastSample { velocityX = Double(point.x - previous.x) / (timestamp - lastSample) }
        previous = point; lastSample = timestamp
        let classified = classifier?.update(touches: touches.count, x: Double(point.x - origin.x),
                                            y: Double(point.y - origin.y), elapsed: timestamp - started)
        if classified == .cancelled { cancelGesture(); return }
        if case let .some(.dragging(axis)) = classified {
            guard plans?.plan(axis, .next) != nil || plans?.plan(axis, .previous) != nil else { cancelGesture(); return }
            // UIKit consults gestureRecognizerShouldBegin on this transition.
            state = state == .possible ? .began : .changed
        }
    }
    override func touchesEnded(_ incoming: Set<UITouch>, with event: UIEvent) {
        guard state == .possible || state == .began || state == .changed else { return }
        guard allowed?() == true else { cancelGesture(); return }
        let ended = incoming.map(\.timestamp).max() ?? event.timestamp
        if ended - lastSample > 0.1 { velocityX = 0 }
        // Stop on the first release; two fingers need not lift simultaneously.
        // Do not reinterpret the remaining finger as a machine gesture.
        guard let view, var classifier,
              let (axis, direction) = classifier.finish(width: Double(view.bounds.width), velocityX: velocityX),
              let plan = plans?.plan(axis, direction), state != .possible else {
            cancelGesture(); return
        }
        self.classifier = classifier
        state = .ended
        accept?(plan)
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent) { cancelGesture() }
    override func canPrevent(_ preventedGestureRecognizer: UIGestureRecognizer) -> Bool {
        if preventedGestureRecognizer is UIPinchGestureRecognizer || preventedGestureRecognizer is UILongPressGestureRecognizer
            || preventedGestureRecognizer is UIScreenEdgePanGestureRecognizer { return false }
        return super.canPrevent(preventedGestureRecognizer)
    }
}
#endif
