#if os(macOS) || os(Linux)
import Foundation
import XCTest
@testable import FrankenTermKit

final class WorkspaceKeyEnrollmentSSHPlanTests: XCTestCase {
    private func account(_ host: String = "10.0.0.8", port: UInt16 = 22,
                         user: String = "operator") throws -> WorkspaceKeyEnrollmentTarget {
        try .init(host: host, port: port, user: user)
    }

    private func request() throws -> WorkspaceKeyEnrollmentRequest {
        var generated = try WorkspaceDeviceKey.generate()
        defer { generated.container.resetBytes(in: 0..<generated.container.count) }
        return try .init(key: generated.key)
    }

    private func document(_ change: (inout [String: Any]) throws -> Void = { _ in }) throws -> WorkspaceDocument {
        let fingerprint = "SHA256:" + String(repeating: "A", count: 43)
        let definitions: [(String, String, Int, String, [String])] = [
            ("destination", "10.0.0.8", 2202, "operator", ["hop-middle", "hop-inner"]),
            ("hop-inner", "inner.example", 22, "inside", []),
            ("hop-first", "outside.example", 2222, "gateway", []),
            ("hop-middle", "middle.example", 22, "bridge", ["hop-first"])
        ]
        var workspace: [String: Any] = [
            "id": "workspace", "name": "Reviewed fleet",
            "groups": [["id": "group", "name": "Fleet", "machine_order": ["machine"]]],
            "machines": [["id": "machine", "name": "Private host", "theme_ref": "theme",
                          "route_refs": ["destination"], "preferred_route_ref": "destination"]],
            "routes": definitions.map { id, host, port, user, hops in
                ["id": id, "mode": "direct_ssh", "host": host, "port": port, "user": user,
                 "credential_refs": ["key-" + id], "authentication_methods": ["publickey"],
                 "host_trust_ref": "trust-" + id, "jump_routes": hops,
                 "session_creation": "manual"] as [String: Any]
            },
            "credentials": definitions.map { id, _, _, _, _ in
                ["id": "key-" + id, "algorithm": "ssh-ed25519", "public_fingerprint": fingerprint,
                 "private_material": "not_included"] as [String: Any]
            },
            "host_trust": definitions.map { id, host, port, _, _ in
                ["id": "trust-" + id, "host": host, "port": port,
                 "pins": [["algorithm": "ssh-ed25519", "fingerprint": fingerprint]],
                 "revoked_keys": []] as [String: Any]
            },
            "themes": [["id": "theme", "name": "Plain", "foreground": "#FFFFFF", "background": "#000000",
                        "accent": "#123456", "cursor_foreground": "#000000", "cursor_background": "#FFFFFF",
                        "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF", "selection_background": "#123456",
                        "ansi": Array(repeating: "#123456", count: 16)]],
            "preferences": ["initial_machine_ref": "machine", "initial_group_ref": "group", "connect_on_open": false]
        ]
        try change(&workspace)
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: [
            "format": "ftworkspace", "version": 1, "bundle_id": "bundle", "source": ["exporter_id": "mac"],
            "required_capabilities": [], "workspace": workspace
        ], options: [.sortedKeys]))
    }

    private func edit(_ workspace: inout [String: Any], section: String, id: String,
                      _ change: (inout [String: Any]) -> Void) throws {
        var rows = try XCTUnwrap(workspace[section] as? [[String: Any]])
        let index = try XCTUnwrap(rows.firstIndex { $0["id"] as? String == id })
        change(&rows[index]); workspace[section] = rows
    }

    private func plan(_ document: WorkspaceDocument) throws -> WorkspaceKeyEnrollmentSSHPlan {
        try WorkspaceKeyEnrollmentSSH.plan(for: XCTUnwrap(document.routes.first { $0.id == "destination" }), in: document)
    }

    func testNestedJumpOrderAndSeparatelyReviewedTransitPrefixes() throws {
        let original = try document(), plan = try plan(original)
        XCTAssertEqual(plan.jumpHosts.map { $0.target.host }, ["outside.example", "middle.example", "inner.example"])
        XCTAssertEqual(plan.jumpHosts.map { $0.target.user }, ["gateway", "bridge", "inside"])
        XCTAssertEqual(plan.destination.target.host, "10.0.0.8")
        XCTAssertEqual(plan.target.host, "10.0.0.8")
        XCTAssertNotNil(plan.target.routeScope)
        let transit = try plan.transitPlans
        XCTAssertEqual(transit.map { $0.jumpHosts.count }, [0, 1, 2])
        XCTAssertEqual(transit.map { $0.target.host }, ["outside.example", "middle.example", "inner.example"])
        XCTAssertNil(transit[0].target.routeScope)
        XCTAssertEqual(transit[1].jumpHosts, Array(plan.jumpHosts.prefix(1)))
        XCTAssertEqual(transit[2].jumpHosts, Array(plan.jumpHosts.prefix(2)))
        XCTAssertEqual(Set((transit + [plan]).map { $0.target.id }).count, 4)
        let route = try XCTUnwrap(original.routes.first { $0.id == "destination" })
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.target(for: route, in: original),
                             "The legacy target API must not silently drop transit accounts.")
    }

    func testSamePrivateEndpointOnDifferentRoutesHasDistinctDurableIdentity() throws {
        let destination = try account(), request = try request()
        let alpha = try WorkspaceKeyEnrollmentSSHHost(target: account("alpha.example"))
        let beta = try WorkspaceKeyEnrollmentSSHHost(target: account("beta.example"))
        let first = try WorkspaceKeyEnrollmentSSHPlan(target: destination, jumpHosts: [alpha])
        let second = try WorkspaceKeyEnrollmentSSHPlan(target: destination, jumpHosts: [beta])
        let two = try WorkspaceKeyEnrollmentSSHPlan(target: destination, jumpHosts: [alpha, beta])
        let reversed = try WorkspaceKeyEnrollmentSSHPlan(target: destination, jumpHosts: [beta, alpha])
        let aliased = try WorkspaceKeyEnrollmentSSHPlan(target: destination, hostKeyAlias: "private-server", jumpHosts: [alpha])
        let hopAlias = try WorkspaceKeyEnrollmentSSHHost(target: alpha.target, hostKeyAlias: "gateway-identity")
        let differentHopTrust = try WorkspaceKeyEnrollmentSSHPlan(target: destination, jumpHosts: [hopAlias])
        let plans = [first, second, two, reversed, aliased, differentHopTrust]
        XCTAssertEqual(Set(plans.map(\.id)).count, plans.count)
        XCTAssertEqual(Set(plans.map { $0.target.id }).count, plans.count)
        XCTAssertEqual(Set(plans.map { request.operationID(for: $0.target) }).count, plans.count)
        XCTAssertTrue(plans.allSatisfy { $0.target.host == destination.host && $0.target.user == destination.user })
        XCTAssertEqual(first, try WorkspaceKeyEnrollmentSSHPlan(target: account("10.0.0.8"), jumpHosts: [alpha]))
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceKeyEnrollmentTarget.self,
            from: JSONEncoder().encode(first.target)), first.target)
    }

    func testUnaliasedDirectPlanKeepsLegacyIdentityAndCannotReconstructScopedPath() throws {
        let target = try account("EXAMPLE.COM."), plan = try WorkspaceKeyEnrollmentSSHPlan(target: target)
        XCTAssertEqual(plan.target, target)
        XCTAssertNil(plan.target.routeScope)
        XCTAssertTrue(try plan.transitPlans.isEmpty)
        let scoped = try WorkspaceKeyEnrollmentSSHPlan(target: target, hostKeyAlias: "server-trust")
        XCTAssertNotEqual(scoped.target.id, target.id)
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSHHost(target: scoped.target))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSHPlan(target: scoped.target))
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.invocation(request: request(), target: scoped.target, action: .inspect))
        let directDocument = try document { workspace in
            try self.edit(&workspace, section: "routes", id: "destination") { $0["jump_routes"] = [] as [String] }
        }
        let directRoute = try XCTUnwrap(directDocument.routes.first { $0.id == "destination" })
        XCTAssertEqual(try WorkspaceKeyEnrollmentSSH.target(for: directRoute, in: directDocument),
                       try self.plan(directDocument).target)
    }

    func testEveryHopMustPreserveAuthenticationAndRequiredPolicy() throws {
        for id in ["destination", "hop-first", "hop-middle", "hop-inner"] {
            let passwordOnly = try document { workspace in
                try self.edit(&workspace, section: "routes", id: id) { $0["authentication_methods"] = ["password"] }
            }
            XCTAssertThrowsError(try plan(passwordOnly), id)
            let routePolicy = try document { workspace in
                try self.edit(&workspace, section: "routes", id: id) { $0["required_capabilities"] = ["unresolved-hop-policy"] }
            }
            XCTAssertThrowsError(try plan(routePolicy), id)
            let credentialPolicy = try document { workspace in
                try self.edit(&workspace, section: "credentials", id: "key-" + id) {
                    $0["required_capabilities"] = ["external-signer-policy"]
                }
            }
            XCTAssertThrowsError(try plan(credentialPolicy), id)
        }
    }

    func testRelatedRevocationAndTrustRequirementsBlockEveryAffectedHop() throws {
        for id in ["destination", "hop-first", "hop-middle", "hop-inner"] {
            let revoked = try document { workspace in
                var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
                var related = try XCTUnwrap(trusts.first { $0["id"] as? String == "trust-" + id })
                related["id"] = "related-trust"; related["host_key_alias"] = "other-lookup"
                related["revoked_keys"] = related["pins"]
                trusts.append(related); workspace["host_trust"] = trusts
            }
            XCTAssertThrowsError(try plan(revoked), id)
            let required = try document { workspace in
                try self.edit(&workspace, section: "host_trust", id: "trust-" + id) {
                    $0["required_capabilities"] = ["unsupported-revocation-source"]
                }
            }
            XCTAssertThrowsError(try plan(required), id)
        }
    }

    func testStaleRouteCannotChooseChangedJumpChainOrAccountPolicy() throws {
        let original = try document()
        let selected = try XCTUnwrap(original.routes.first { $0.id == "destination" })
        let changedChain = try document { workspace in
            try self.edit(&workspace, section: "routes", id: "destination") { $0["jump_routes"] = ["hop-inner"] }
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.plan(for: selected, in: changedChain))
        let changedAuth = try document { workspace in
            try self.edit(&workspace, section: "routes", id: "destination") { $0["authentication_methods"] = ["publickey", "password"] }
        }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSH.plan(for: selected, in: changedAuth))
        XCTAssertEqual(try plan(original).jumpHosts.count, 3)
    }

    func testDisplayNamesAndMuxCommandsDoNotChangeOrEnterTransportPlan() throws {
        let original = try document(), expected = try plan(original)
        let renamed = try document { workspace in
            try self.edit(&workspace, section: "machines", id: "machine") { $0["name"] = "A different label" }
            try self.edit(&workspace, section: "routes", id: "hop-inner") {
                $0["mode"] = "frankenterm_mux"; $0["session_creation"] = nil
                $0["attach_policy"] = "existing_only"; $0["remote_command"] = "echo IMPORTED_COMMAND_MUST_NOT_RUN"
            }
        }
        let actual = try plan(renamed)
        XCTAssertEqual(actual, expected)
        let call = try WorkspaceKeyEnrollmentSSH.invocation(request: request(), plan: actual, action: .inspect)
        XCTAssertFalse(call.arguments.joined(separator: " ").contains("IMPORTED_COMMAND_MUST_NOT_RUN"))
        XCTAssertFalse(call.arguments.contains("-J"))
    }

    func testPlanBoundsAndAliasValidationRejectUnsafeOrAmbiguousPaths() throws {
        let destination = try account()
        let hops = try (0..<4).map { try WorkspaceKeyEnrollmentSSHHost(target: account("hop-\($0).example")) }
        XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSHPlan(target: destination, jumpHosts: hops))
        let repeated = try WorkspaceKeyEnrollmentSSHHost(target: destination)
        let repeatedPlan = try WorkspaceKeyEnrollmentSSHPlan(target: destination, jumpHosts: [repeated, repeated])
        let prefixes = try repeatedPlan.transitPlans + [repeatedPlan]
        XCTAssertEqual(prefixes.map { $0.target.host }, [destination.host, destination.host, destination.host])
        XCTAssertEqual(prefixes.map { $0.jumpHosts.count }, [0, 1, 2])
        XCTAssertEqual(Set(prefixes.map { $0.target.id }).count, 3,
                       "The same private endpoint in successive namespaces must not collapse into one account.")
        for alias in ["", "-oProxyCommand=bad", "alias with space", "alias%h", "$(id)", "x;id", "a\nb", "*.example"] {
            XCTAssertThrowsError(try WorkspaceKeyEnrollmentSSHHost(target: destination, hostKeyAlias: alias), alias)
        }
        XCTAssertEqual(try WorkspaceKeyEnrollmentSSHHost(target: destination, hostKeyAlias: "ALIAS.EXAMPLE.").hostKeyAlias,
                       "alias.example")
    }

    /// OpenSSH first consumes doubled percent literals, then invokes the shell.
    /// Exercise that shell's actual argv parsing without starting any SSH child.
    private func proxyArguments(_ value: String) throws -> [String] {
        let bytes = Array(value.utf8)
        var expanded = [UInt8](), index = 0
        while index < bytes.count {
            if bytes[index] == 37 {
                XCTAssertLessThan(index + 1, bytes.count)
                guard index + 1 < bytes.count, bytes[index + 1] == 37 else {
                    throw FTError.malformed("test.unescaped_proxy_token")
                }
                expanded.append(37); index += 2
            } else { expanded.append(bytes[index]); index += 1 }
        }
        let command = String(decoding: expanded, as: UTF8.self)
        let output = try LocalHelperProcess.run(executable: URL(fileURLWithPath: "/bin/sh"),
            arguments: ["-c", "set -- " + command + "; printf '%s\\0' \"$@\""],
            maximumOutputBytes: 131_072, timeout: 3)
        return output.split(separator: 0, omittingEmptySubsequences: false).dropLast().map { String(decoding: $0, as: UTF8.self) }
    }

    private func option(_ name: String, in arguments: [String]) throws -> String {
        let entries = arguments.enumerated().compactMap { index, value -> String? in
            guard index > 0, arguments[index - 1] == "-o", value.hasPrefix(name + "=") else { return nil }
            return String(value.dropFirst(name.count + 1))
        }
        XCTAssertEqual(entries.count, 1, name)
        return try XCTUnwrap(entries.first)
    }

    func testEveryGeneratedHopKeepsStrictOptionsAndLiteralConfigurationPath() throws {
        let plan = try plan(document()), request = try request()
        let configuration = URL(fileURLWithPath: "/fixture/quoted ' file %h $(printf injected) `printf injected`.conf")
        let call = try WorkspaceKeyEnrollmentSSH.invocation(request: request, plan: plan, action: .inspect,
                                                             sshConfiguration: configuration)
        var arguments = call.arguments
        let hosts = plan.jumpHosts + [plan.destination]
        for index in stride(from: hosts.count - 1, through: 0, by: -1) {
            let host = hosts[index]
            let fileIndex = try XCTUnwrap(arguments.firstIndex(of: "-F"))
            XCTAssertEqual(arguments[fileIndex + 1], configuration.path)
            for (name, expected) in [("StrictHostKeyChecking", "yes"), ("BatchMode", "yes"),
                ("ForwardAgent", "no"), ("ClearAllForwardings", "yes"), ("PermitLocalCommand", "no"),
                ("RemoteCommand", "none"), ("ControlMaster", "no"), ("ControlPath", "none"),
                ("ProxyJump", "none"), ("ProxyUseFdpass", "no"), ("CanonicalizeHostname", "no"),
                ("KnownHostsCommand", "none"), ("PasswordAuthentication", "no"),
                ("KbdInteractiveAuthentication", "no"), ("PreferredAuthentications", "publickey"),
                ("ForkAfterAuthentication", "no"), ("StdinNull", "no"), ("ExitOnForwardFailure", "yes")] {
                XCTAssertEqual(try option(name, in: arguments), expected, "\(host.target.host): \(name)")
            }
            XCTAssertEqual(try option("HostName", in: arguments), host.target.host)
            XCTAssertEqual(try option("HostKeyAlias", in: arguments), host.hostKeyAlias ??
                (host.target.port == 22 ? host.target.host : "[\(host.target.host)]:\(host.target.port)"))
            XCTAssertFalse(arguments.contains("-J"))
            if index == 0 { XCTAssertEqual(try option("ProxyCommand", in: arguments), "none") }
            else {
                arguments = try proxyArguments(option("ProxyCommand", in: arguments))
                XCTAssertEqual(arguments.first, "/usr/bin/ssh")
                let forwarding = try XCTUnwrap(arguments.firstIndex(of: "-W"))
                XCTAssertEqual(arguments[forwarding + 1], "[\(host.target.host)]:\(host.target.port)")
                XCTAssertEqual(arguments.last, hosts[index - 1].target.host)
            }
        }
    }

    func testExplicitAliasAtNonstandardPortIsUsedVerbatim() throws {
        let document = try document { workspace in
            try self.edit(&workspace, section: "routes", id: "destination") { $0["host_key_alias"] = "private-identity" }
            try self.edit(&workspace, section: "host_trust", id: "trust-destination") { $0["host_key_alias"] = "private-identity" }
            try self.edit(&workspace, section: "routes", id: "hop-first") { $0["host_key_alias"] = "gateway-identity" }
            try self.edit(&workspace, section: "host_trust", id: "trust-hop-first") { $0["host_key_alias"] = "gateway-identity" }
        }
        let plan = try plan(document)
        var arguments = try WorkspaceKeyEnrollmentSSH.invocation(request: request(), plan: plan, action: .inspect).arguments
        XCTAssertEqual(try option("HostKeyAlias", in: arguments), "private-identity")
        for _ in 0..<3 { arguments = try proxyArguments(option("ProxyCommand", in: arguments)) }
        XCTAssertEqual(try option("HostKeyAlias", in: arguments), "gateway-identity")
        XCTAssertNotNil(try plan.transitPlans[0].target.routeScope)
    }
}
#endif
