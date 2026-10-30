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
        for action in [WorkspaceKeyEnrollmentAction.inspect, .install] {
            let result = try WorkspaceKeyEnrollmentSSH.perform(request: request, target: target,
                action: action, sshConfiguration: config, isCancelled: { true })
            XCTAssertEqual(result.state, action == .install ? .uncertain : .blocked)
            XCTAssertEqual(result.operationID, request.operationID(for: target))
            XCTAssertFalse(result.detail.contains(config.path))
        }
    }

    func testLiveEnrollmentUsesTrustedSSHAndAccountsForRemoteEffects() throws {
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
        let target = try WorkspaceKeyEnrollmentTarget(host: "127.0.0.1", port: port, user: account)
        let result = try WorkspaceKeyEnrollmentSSH.perform(request: request, target: target,
            action: action, sshConfiguration: URL(fileURLWithPath: config))
        XCTAssertEqual(result.state.rawValue, expected, result.detail)
        XCTAssertEqual(result.target, target)
        XCTAssertEqual(result.operationID, request.operationID(for: target))
        XCTAssertFalse(result.detail.contains(config))
    }
}
#endif
