#if canImport(UIKit)
import UIKit
import XCTest
@testable import FrankenTermKit

@MainActor
private final class NativeNavigationTestFocus: TerminalNavigationFocusOwner {
    var currentSelection: TerminalNavigationSelection?
    func prepare(_ destination: TerminalNavigationSelection) async throws -> TerminalNavigationPreparation {
        .init(destination: destination)
    }
    func publish(_ preparation: TerminalNavigationPreparation) throws { currentSelection = preparation.destination }
    func abandon(_ preparation: TerminalNavigationPreparation) async { }
    func seal() { currentSelection = nil }
    func joinSealed() async { }
}

/// These tests require an actual UIKit SDK/test host. A Linux conditional build
/// or parser-only pass does not execute or type-check these native branches.
@MainActor
final class NativeTerminalNavigationTests: XCTestCase {
    private func coordinator() throws -> TerminalNavigationCoordinator {
        let machines = try ["A", "B"].map { name in
            try TerminalNavigationMachine(id: name, name: name, panes: (0..<2).map {
                try TerminalNavigationPane(id: .init(id: UUID(), incarnation: UUID()), title: "pane-\($0)")
            })
        }
        return TerminalNavigationCoordinator(navigation: try .init(machines: machines), focusOwner: NativeNavigationTestFocus())
    }

    func testNativeRailMenusAccessibilityAndPublishedSelection() async throws {
        let coordinator = try coordinator(); coordinator.resume()
        let canvas = UIView(frame: CGRect(x: 0, y: 0, width: 320, height: 600))
        let controls = try NativeTerminalNavigationControls(coordinator: coordinator, terminalCanvas: canvas,
                                                             permitsCanvasNavigation: { true })
        let stack = try XCTUnwrap(controls.subviews.first as? UIStackView)
        let machine = try XCTUnwrap(stack.arrangedSubviews.first as? UIButton)
        let pane = try XCTUnwrap(stack.arrangedSubviews.last as? UIButton)
        XCTAssertEqual(machine.menu?.children.count, 2)
        XCTAssertEqual(pane.menu?.children.count, 2)
        XCTAssertEqual(machine.accessibilityCustomActions?.map(\.name), ["Previous machine", "Next machine"])
        XCTAssertEqual(pane.accessibilityCustomActions?.map(\.name), ["Previous pane", "Next pane"])
        XCTAssertEqual(canvas.gestureRecognizers?.count, 1)
        var publications: [TerminalNavigationSelection] = []
        controls.onSelection = { publications.append($0) }
        try await coordinator.activateSelection()
        XCTAssertEqual(publications.count, 1)
        XCTAssertEqual(publications[0].machineID, "A")
        controls.refresh()
        XCTAssertEqual(publications.count, 1)
        try await coordinator.perform(XCTUnwrap(coordinator.navigation.plan(machineID: "B")))
        XCTAssertEqual(publications.last?.machineID, "B")
        XCTAssertEqual(machine.accessibilityLabel, "Machine: B")
        controls.retire(); await coordinator.joinSuspension()
        XCTAssertTrue(canvas.gestureRecognizers?.isEmpty ?? true)
        XCTAssertNil(machine.menu); XCTAssertNil(machine.accessibilityCustomActions)
        XCTAssertEqual(coordinator.phase, .suspended)
    }

    func testFailedObserverReservationDoesNotInstallCanvasRecognizer() throws {
        let coordinator = try coordinator(), canvas = UIView()
        var tokens: [UUID] = []
        for _ in 0..<16 { tokens.append(try coordinator.observe {}) }
        XCTAssertThrowsError(try NativeTerminalNavigationControls(coordinator: coordinator, terminalCanvas: canvas,
                                                                  permitsCanvasNavigation: { true }))
        XCTAssertTrue(canvas.gestureRecognizers?.isEmpty ?? true)
        for token in tokens { coordinator.removeObserver(token) }
        let controls = try NativeTerminalNavigationControls(coordinator: coordinator, terminalCanvas: canvas,
                                                             permitsCanvasNavigation: { true })
        controls.retire()
    }

    func testCanvasInteractionCanDeclineNavigationWithoutRemovingRail() throws {
        let coordinator = try coordinator(); coordinator.resume()
        let canvas = UIView(frame: CGRect(x: 0, y: 0, width: 320, height: 600))
        var allowed = false
        let controls = try NativeTerminalNavigationControls(coordinator: coordinator, terminalCanvas: canvas,
                                                             permitsCanvasNavigation: { allowed })
        let gesture = try XCTUnwrap(canvas.gestureRecognizers?.first)
        XCTAssertFalse(controls.gestureRecognizerShouldBegin(gesture))
        allowed = true
        XCTAssertTrue(controls.gestureRecognizerShouldBegin(gesture))
        XCTAssertTrue(controls.gestureRecognizer(gesture, shouldRequireFailureOf: UIScreenEdgePanGestureRecognizer()))
        controls.retire()
        XCTAssertFalse(controls.gestureRecognizerShouldBegin(gesture))
    }
}
#endif
