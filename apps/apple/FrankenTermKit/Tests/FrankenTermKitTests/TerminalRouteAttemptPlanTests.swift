import XCTest
@testable import FrankenTermKit

final class TerminalRouteAttemptPlanTests: XCTestCase {
    private let routes: [TerminalRouteAttemptPlan.Route] = [
        .init(routeID: "local", credentialIDs: ["work", "personal", "recovery"]),
        .init(routeID: "tailnet", credentialIDs: ["alternate", "backup"])
    ]

    func testOnlyPositiveRefusalAdvancesKeysInConfiguredOrder() throws {
        var plan = try TerminalRouteAttemptPlan(routes: routes)
        var tried: [String] = []
        while let attempt = plan.current {
            XCTAssertEqual(attempt.selection.routeID, "local")
            tried.append(attempt.selection.credentialID)
            try plan.failed(.credentialRefused, attempt: attempt)
        }
        XCTAssertEqual(tried, ["work", "personal", "recovery"])
    }

    func testUnreachableRouteSkipsItsRemainingCredentials() throws {
        var plan = try TerminalRouteAttemptPlan(routes: routes)
        let initial = try XCTUnwrap(plan.current)
        let next = try XCTUnwrap(plan.failed(.unreachable, attempt: initial))
        XCTAssertEqual(next.selection, .init(routeID: "tailnet", credentialID: "alternate"))
        XCTAssertNil(try plan.failed(.unreachable, attempt: next))
    }

    func testTransportFailureAfterRefusalCannotSwitchTrustScopes() throws {
        var plan = try TerminalRouteAttemptPlan(routes: routes)
        let first = try XCTUnwrap(plan.current)
        let second = try XCTUnwrap(plan.failed(.credentialRefused, attempt: first))
        XCTAssertNil(try plan.failed(.unreachable, attempt: second))
        XCTAssertNil(plan.current)
    }

    func testTerminalFailureAndSuccessfulAuthenticationEndKeySelection() throws {
        for finishSuccessfully in [false, true] {
            var plan = try TerminalRouteAttemptPlan(routes: routes)
            let first = try XCTUnwrap(plan.current)
            if finishSuccessfully {
                XCTAssertEqual(try plan.connected(first), first.selection)
            } else {
                XCTAssertNil(try plan.failed(.terminal, attempt: first))
            }
            XCTAssertNil(plan.current)
            XCTAssertThrowsError(try plan.failed(.credentialRefused, attempt: first)) {
                XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .staleAttempt)
            }
        }
    }

    func testLateDuplicateAndDifferentPlanResultsDoNotConsumeCurrentKey() throws {
        var plan = try TerminalRouteAttemptPlan(routes: routes)
        let first = try XCTUnwrap(plan.current)
        let second = try XCTUnwrap(plan.failed(.credentialRefused, attempt: first))
        XCTAssertThrowsError(try plan.failed(.credentialRefused, attempt: first)) {
            XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .staleAttempt)
        }
        XCTAssertEqual(plan.current, second)
        let other = try TerminalRouteAttemptPlan(routes: routes)
        XCTAssertThrowsError(try plan.connected(try XCTUnwrap(other.current))) {
            XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .staleAttempt)
        }
        XCTAssertEqual(plan.current, second)
        plan.cancel()
        XCTAssertThrowsError(try plan.failed(.credentialRefused, attempt: second)) {
            XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .staleAttempt)
        }
        XCTAssertNil(plan.current)
    }

    func testReconnectPrefersOnlyTheListedWinnerAndPreservesOtherOrder() throws {
        let preferred = TerminalRouteAttemptPlan.Selection(routeID: "tailnet", credentialID: "backup")
        var plan = try TerminalRouteAttemptPlan(routes: routes, preferring: preferred)
        let first = try XCTUnwrap(plan.current)
        XCTAssertEqual(first.selection, preferred)
        let second = try XCTUnwrap(plan.failed(.credentialRefused, attempt: first))
        XCTAssertEqual(second.selection, .init(routeID: "tailnet", credentialID: "alternate"))
        XCTAssertNil(try plan.failed(.credentialRefused, attempt: second))

        var unreachable = try TerminalRouteAttemptPlan(routes: routes, preferring: preferred)
        let next = try XCTUnwrap(unreachable.failed(.unreachable, attempt: try XCTUnwrap(unreachable.current)))
        XCTAssertEqual(next.selection, .init(routeID: "local", credentialID: "work"))
        // A deliberate new attempt does not inherit the reconnect preference.
        XCTAssertEqual(try TerminalRouteAttemptPlan(routes: routes).current?.selection,
                       .init(routeID: "local", credentialID: "work"))
        XCTAssertThrowsError(try TerminalRouteAttemptPlan(routes: routes,
            preferring: .init(routeID: "unknown", credentialID: "work"))) {
                XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .invalidPreference)
            }
        XCTAssertThrowsError(try TerminalRouteAttemptPlan(routes: routes,
            preferring: .init(routeID: "local", credentialID: "unrelated-key"))) {
                XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .invalidPreference)
            }
    }

    func testBoundsAndDuplicatesRejectAtomicallyWithoutTruncatingChoices() throws {
        for invalid in [[], [routes[0], routes[0]], [.init(routeID: "", credentialIDs: ["key"])],
                        (0..<257).map { .init(routeID: "route-\($0)", credentialIDs: ["key"]) }] {
            XCTAssertThrowsError(try TerminalRouteAttemptPlan(routes: invalid)) {
                XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .invalidRoutes)
            }
        }
        for keys in [[], [""], ["duplicate", "duplicate"], (0..<9).map { "key-\($0)" }] {
            XCTAssertThrowsError(try TerminalRouteAttemptPlan(routes: [.init(routeID: "route", credentialIDs: keys)])) {
                XCTAssertEqual($0 as? TerminalRouteAttemptPlanError, .invalidCredentials)
            }
        }
        var maximum = try TerminalRouteAttemptPlan(routes: (0..<256).map {
            .init(routeID: "route-\($0)", credentialIDs: (0..<8).map { "key-\($0)" })
        })
        var count = 0
        while let attempt = maximum.current {
            count += 1
            try maximum.failed(attempt.selection.routeID == "route-255" ? .credentialRefused : .unreachable,
                               attempt: attempt)
        }
        XCTAssertEqual(count, 255 + 8)
    }
}
