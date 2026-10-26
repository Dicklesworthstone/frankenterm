import XCTest
@testable import FrankenTermKit

final class TerminalReconnectPolicyTests: XCTestCase {
    func testShortSuccessfulSessionsCannotCreateAnUnlimitedRetryLoop() {
        var policy = TerminalReconnectPolicy()
        let start = ContinuousClock.now
        policy.recordLive(at: start)
        policy.recordEnd(at: start.advanced(by: .seconds(1)))
        for (index, delay) in [Duration.seconds(1), .seconds(2), .seconds(4)].enumerated() {
            XCTAssertEqual(policy.nextDelay, delay)
            XCTAssertTrue(policy.beginRetry())
            let connected = start.advanced(by: .seconds(10 * (index + 1)))
            policy.recordLive(at: connected)
            XCTAssertNil(policy.nextDelay)
            policy.recordEnd(at: connected.advanced(by: .milliseconds(10)))
            XCTAssertEqual(policy.attemptsUsed, index + 1)
        }
        XCTAssertNil(policy.nextDelay)
        XCTAssertFalse(policy.beginRetry())
        XCTAssertEqual(policy.attemptsUsed, 3)
    }

    func testStableSessionReplenishesBudgetAtTheExactBoundary() {
        var policy = TerminalReconnectPolicy()
        for _ in 0..<3 { XCTAssertTrue(policy.beginRetry()) }
        let start = ContinuousClock.now
        policy.recordLive(at: start)
        policy.recordEnd(at: start.advanced(by: .seconds(60) - .nanoseconds(1)))
        XCTAssertNil(policy.nextDelay)
        policy.recordLive(at: start.advanced(by: .seconds(100)))
        policy.recordEnd(at: start.advanced(by: .seconds(160)))
        XCTAssertEqual(policy.attemptsUsed, 0)
        XCTAssertEqual(policy.nextDelay, .seconds(1))
    }

    func testRepeatedLiveNotificationsDoNotResetBudgetOrMoveStabilityStart() {
        var policy = TerminalReconnectPolicy()
        XCTAssertTrue(policy.beginRetry())
        let start = ContinuousClock.now
        policy.recordLive(at: start)
        for second in 1..<60 {
            policy.recordLive(at: start.advanced(by: .seconds(second)))
            XCTAssertEqual(policy.attemptsUsed, 1)
            XCTAssertFalse(policy.beginRetry())
        }
        policy.recordEnd(at: start.advanced(by: .seconds(60)))
        XCTAssertEqual(policy.attemptsUsed, 0)
        policy.recordEnd(at: start.advanced(by: .seconds(61)))
        XCTAssertEqual(policy.nextDelay, .seconds(1))
    }

    func testExplicitRetryStartsNewBudgetAndRetiresOldLiveTimestamp() {
        var policy = TerminalReconnectPolicy()
        for _ in 0..<3 { XCTAssertTrue(policy.beginRetry()) }
        let start = ContinuousClock.now
        policy.recordLive(at: start)
        policy.resetForUserRequest()
        XCTAssertEqual(policy.nextDelay, .seconds(1))
        XCTAssertTrue(policy.beginRetry())
        policy.recordEnd(at: start.advanced(by: .seconds(600)))
        XCTAssertEqual(policy.attemptsUsed, 1)
    }

    func testUnobservedBackgroundTimeDoesNotAccumulateStability() {
        var policy = TerminalReconnectPolicy()
        for _ in 0..<3 { XCTAssertTrue(policy.beginRetry()) }
        let start = ContinuousClock.now
        policy.recordLive(at: start)
        policy.recordEnd(at: start.advanced(by: .seconds(20)))
        // The owner restarts observation after the app returns. Even a long
        // gap says nothing about how long the remote connection stayed alive.
        policy.recordLive(at: start.advanced(by: .seconds(600)))
        policy.recordEnd(at: start.advanced(by: .seconds(640)))
        XCTAssertEqual(policy.attemptsUsed, 3)
        XCTAssertNil(policy.nextDelay)
    }

    func testANewNetworkPathStartsANewBudgetOnlyForADroppedTerminal() {
        var policy = TerminalReconnectPolicy()
        for _ in 0..<3 { XCTAssertTrue(policy.beginRetry()) }
        XCTAssertNil(policy.nextDelay)
        policy.networkChanged()
        XCTAssertEqual(policy.nextDelay, .seconds(1))
        XCTAssertTrue(policy.beginRetry())
        let start = ContinuousClock.now
        policy.recordLive(at: start)
        policy.networkChanged()
        XCTAssertEqual(policy.attemptsUsed, 1, "a live terminal keeps its accounting")
        policy.recordEnd(at: start.advanced(by: .seconds(1)))
        XCTAssertEqual(policy.nextDelay, .seconds(2))
    }

    func testOnlyAUsablePathThatChangedIsANewRoute() {
        let wifi = TerminalNetworkPath(usable: true, interfaces: ["en0"])
        let none = TerminalNetworkPath(usable: false, interfaces: [])
        let vpn = TerminalNetworkPath(usable: true, interfaces: ["utun4", "en0"])
        XCTAssertFalse(wifi.isNewRoute(after: nil), "the first path is a baseline")
        XCTAssertFalse(none.isNewRoute(after: wifi))
        XCTAssertTrue(wifi.isNewRoute(after: none), "the network came back")
        XCTAssertFalse(wifi.isNewRoute(after: wifi))
        XCTAssertTrue(vpn.isNewRoute(after: wifi), "a VPN came up")
        XCTAssertFalse(TerminalNetworkPath(usable: true, interfaces: ["en0", "utun4", "en0"]).isNewRoute(after: vpn),
                       "the same interfaces in another order")
        XCTAssertTrue(TerminalNetworkPath(usable: true, interfaces: ["pdp_ip0"]).isNewRoute(after: wifi), "Wi-Fi to cellular")
    }

    func testMissingOrEarlierLiveTimeCannotReplenishBudget() {
        var policy = TerminalReconnectPolicy()
        XCTAssertTrue(policy.beginRetry())
        let start = ContinuousClock.now
        policy.recordEnd(at: start)
        XCTAssertEqual(policy.attemptsUsed, 1)
        policy.recordLive(at: start)
        policy.recordEnd(at: start.advanced(by: .seconds(-1)))
        XCTAssertEqual(policy.attemptsUsed, 1)
        XCTAssertEqual(policy.nextDelay, .seconds(2))
    }
}
