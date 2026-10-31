#if os(macOS) || os(Linux)
import Foundation
import XCTest
@testable import FrankenTermKit

final class WorkspaceKeyEnrollmentSSHTests: XCTestCase {
    private func request() throws -> WorkspaceKeyEnrollmentRequest {
        var generated = try WorkspaceDeviceKey.generate()
        defer { generated.container.resetBytes(in: 0..<generated.container.count) }
        return try WorkspaceKeyEnrollmentRequest(key: generated.key)
    }

    func testInvocationRequiresExistingTrustAndDisablesUnexpectedRemoteAndLocalEffects() throws {
        let request = try request()
        let target = try WorkspaceKeyEnrollmentTarget(host: "Example.COM", port: 2202, user: "operator")
        let config = URL(fileURLWithPath: "/fixture-owned/config with spaces")
        let invocation = try WorkspaceKeyEnrollmentSSH.invocation(request: request, target: target,
            action: .install, sshConfiguration: config)
        XCTAssertEqual(invocation.executable.path, "/usr/bin/ssh")
        XCTAssertEqual(Array(invocation.arguments.prefix(3)), ["-T", "-F", config.path])
        for option in ["StrictHostKeyChecking=yes", "BatchMode=yes", "ForwardAgent=no",
                       "ClearAllForwardings=yes", "PermitLocalCommand=no", "RemoteCommand=none",
                       "RequestTTY=no", "ControlMaster=no", "ControlPath=none", "ProxyCommand=none",
                       "ProxyJump=none", "CanonicalizeHostname=no", "KnownHostsCommand=none",
                       "PreferredAuthentications=publickey", "PasswordAuthentication=no",
                       "KbdInteractiveAuthentication=no", "HostName=example.com", "HostKeyAlias=[example.com]:2202"] {
            XCTAssertEqual(invocation.arguments.filter { $0 == option }.count, 1, option)
            let index = try XCTUnwrap(invocation.arguments.firstIndex(of: option))
            XCTAssertEqual(invocation.arguments[index - 1], "-o")
        }
        XCTAssertEqual(Array(invocation.arguments.suffix(7).prefix(6)), ["-p", "2202", "-l", "operator", "--", "example.com"])
        XCTAssertEqual(invocation.arguments.last, try WorkspaceKeyEnrollmentSSH.remoteCommand(
            request: request, target: target, action: .install))
        let defaultTarget = try WorkspaceKeyEnrollmentTarget(host: "example.com", user: "operator")
        let defaultCall = try WorkspaceKeyEnrollmentSSH.invocation(request: request, target: defaultTarget, action: .inspect)
        XCTAssertTrue(defaultCall.arguments.contains("HostKeyAlias=example.com"))
        XCTAssertFalse(defaultCall.arguments.contains("-F"))
    }

    func testResponseMustBindExactRequestAccountFingerprintAndPermittedObservation() throws {
        let request = try request()
        let target = try WorkspaceKeyEnrollmentTarget(host: "example.com", user: "operator")
        let valid: [String: JSONValue] = ["version": .number("1"),
            "operation_id": .string(request.operationID(for: target)), "target_id": .string(target.id),
            "fingerprint": .string(request.key.fingerprint), "state": .string("alreadyPresent"),
            "detail_code": .string("owned_entry")]
        let response = try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(valid)),
            request: request, target: target, action: .inspect)
        XCTAssertEqual(response.state, .alreadyPresent)
        XCTAssertEqual(response.operationID, request.operationID(for: target))
        for field in ["operation_id", "target_id", "fingerprint", "detail_code", "state"] {
            var changed = valid
            changed[field] = .string("unrelated")
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(changed)),
                request: request, target: target, action: .inspect), field)
        }
        var effect = valid
        effect["state"] = .string("installed"); effect["detail_code"] = .string("added")
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(effect)),
            request: request, target: target, action: .inspect))
        XCTAssertEqual(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(effect)),
            request: request, target: target, action: .install).state, .installed)
        var absent = valid
        absent["state"] = .string("absent"); absent["detail_code"] = .string("not_present")
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(absent)),
            request: request, target: target, action: .install))
        XCTAssertEqual(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(absent)),
            request: request, target: target, action: .inspect).state, .absent)
        var unexpected = valid
        unexpected["remote_diagnostic"] = .string("untrusted terminal output")
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(unexpected)),
            request: request, target: target, action: .inspect))
        let encoded = try StrictJSON.encode(.object(valid))
        let duplicate = Data("{\"version\":1,".utf8) + encoded.dropFirst()
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(duplicate,
            request: request, target: target, action: .inspect))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(Data(repeating: 32, count: 4097),
            request: request, target: target, action: .inspect))
    }

    func testCancelledOperationDoesNotLaunchAndCannotClaimAnInstallation() throws {
        let request = try request()
        let target = try WorkspaceKeyEnrollmentTarget(host: "no-network.invalid", user: "operator")
        let config = URL(fileURLWithPath: "/nonexistent/fixture-only-config")
        for action in [WorkspaceKeyEnrollmentAction.inspect, .install, .remove] {
            let result = try WorkspaceKeyEnrollmentSSH.perform(request: request, target: target,
                action: action, sshConfiguration: config, isCancelled: { true })
            XCTAssertEqual(result.state, action == .inspect ? .blocked : .uncertain)
            XCTAssertEqual(result.operationID, request.operationID(for: target))
            XCTAssertFalse(result.detail.contains(config.path))
        }
    }

    func testRemovalReceiptsDistinguishOwnedRemovalFromIndependentAuthorization() throws {
        let request = try request()
        let target = try WorkspaceKeyEnrollmentTarget(host: "example.com", user: "operator")
        func reply(_ state: String, _ code: String, action: WorkspaceKeyEnrollmentAction) throws -> WorkspaceKeyEnrollmentResult {
            try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object([
                "version": .number("1"), "operation_id": .string(request.operationID(for: target)),
                "target_id": .string(target.id), "fingerprint": .string(request.key.fingerprint),
                "state": .string(state), "detail_code": .string(code)
            ])), request: request, target: target, action: action)
        }
        XCTAssertEqual(try reply("removed", "owned_entry_disabled", action: .remove).state, .removed)
        XCTAssertThrowsError(try reply("removed", "owned_entry_disabled", action: .inspect))
        XCTAssertThrowsError(try reply("removed", "owned_entry_disabled", action: .install))
        XCTAssertEqual(try reply("stillAuthorized", "owned_removed_other_remains", action: .remove).state, .stillAuthorized)
        XCTAssertThrowsError(try reply("stillAuthorized", "owned_removed_other_remains", action: .inspect))
        for action in [WorkspaceKeyEnrollmentAction.remove, .inspect] {
            XCTAssertEqual(try reply("stillAuthorized", "independent_entry", action: action).state, .stillAuthorized)
            XCTAssertEqual(try reply("absent", "not_present", action: action).state, .absent)
        }
        XCTAssertThrowsError(try reply("stillAuthorized", "independent_entry", action: .install))
        XCTAssertEqual(try reply("uncertain", "write_uncertain", action: .remove).state, .uncertain)
        XCTAssertThrowsError(try reply("uncertain", "write_uncertain", action: .inspect))
        XCTAssertEqual(try reply("alreadyPresent", "owned_and_other", action: .inspect).state, .alreadyPresent)
    }

    func testRemovalRejectsInstallationClaimsAndMismatchedOperationReceipts() throws {
        let request = try request()
        let target = try WorkspaceKeyEnrollmentTarget(host: "example.com", user: "operator")
        var record: [String: JSONValue] = ["version": .number("1"),
            "operation_id": .string(request.operationID(for: target)), "target_id": .string(target.id),
            "fingerprint": .string(request.key.fingerprint), "state": .string("removed"),
            "detail_code": .string("owned_entry_disabled")]
        for (state, code) in [("installed", "added"), ("alreadyPresent", "owned_entry"),
                              ("alreadyPresent", "owned_and_other"), ("restricted", "restricted_entry")] {
            var changed = record
            changed["state"] = .string(state); changed["detail_code"] = .string(code)
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(changed)),
                request: request, target: target, action: .remove), state)
        }
        let otherRequest = try WorkspaceKeyEnrollmentRequest(key: request.key)
        record["operation_id"] = .string(otherRequest.operationID(for: target))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.response(StrictJSON.encode(.object(record)),
            request: request, target: target, action: .remove))
    }

    func testLiveEnrollmentUsesTrustedSSHAndAccountsForRemoteEffects() throws {
        struct FixtureHop: Decodable {
            let host: String
            let port: UInt16
            let user: String
            let hostKeyAlias: String?
        }
        let environment = ProcessInfo.processInfo.environment
        guard let config = environment["FT_ENROLLMENT_CONFIG"],
              let requestFile = environment["FT_ENROLLMENT_REQUEST"],
              let portText = environment["FT_ENROLLMENT_PORT"], let port = UInt16(portText),
              let account = environment["FT_ENROLLMENT_ACCOUNT"],
              let actionText = environment["FT_ENROLLMENT_ACTION"],
              let action = WorkspaceKeyEnrollmentAction(rawValue: actionText),
              let expected = environment["FT_ENROLLMENT_EXPECTED"] else {
            throw XCTSkip("Run Tests/Interop/test_device_key_enrollment.py for the owned live SSH fixture.")
        }
        let request = try WorkspaceKeyEnrollmentRequest(data: Data(contentsOf: URL(fileURLWithPath: requestFile)))
        let target = try WorkspaceKeyEnrollmentTarget(host: environment["FT_ENROLLMENT_HOST"] ?? "127.0.0.1",
            port: port, user: account)
        let fixtureHops = try JSONDecoder().decode([FixtureHop].self,
            from: Data((environment["FT_ENROLLMENT_JUMPS"] ?? "[]").utf8))
        let jumps = try fixtureHops.map { hop in
            try WorkspaceKeyEnrollmentSSHHost(target: WorkspaceKeyEnrollmentTarget(
                host: hop.host, port: hop.port, user: hop.user), hostKeyAlias: hop.hostKeyAlias)
        }
        let plan = try WorkspaceKeyEnrollmentSSHPlan(target: target,
            hostKeyAlias: environment["FT_ENROLLMENT_HOST_ALIAS"], jumpHosts: jumps)
        let cancellationFile = environment["FT_ENROLLMENT_CANCEL_FILE"]
        let result = try WorkspaceKeyEnrollmentSSH.perform(request: request, plan: plan,
            action: action, sshConfiguration: URL(fileURLWithPath: config), isCancelled: {
                cancellationFile.map { FileManager.default.fileExists(atPath: $0) } ?? false
            })
        XCTAssertEqual(result.state.rawValue, expected, result.detail)
        XCTAssertEqual(result.target, plan.target)
        XCTAssertEqual(result.operationID, request.operationID(for: plan.target))
        if !jumps.isEmpty {
            XCTAssertNotEqual(plan.target, target, "The receipt must retain its complete bastion path.")
            XCTAssertNotEqual(result.operationID, request.operationID(for: target))
        }
        XCTAssertFalse(result.detail.contains(config))
    }
}
#endif
