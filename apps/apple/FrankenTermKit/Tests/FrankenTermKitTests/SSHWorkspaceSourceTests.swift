import Foundation
import XCTest
@testable import FrankenTermKit

final class SSHWorkspaceSourceTests: XCTestCase {
    private let crypto = TestCryptography()
    private func source(_ text: String) throws -> SSHWorkspaceSource {
        try SSHWorkspaceSource(configuration: Data(text.utf8))
    }
    private func publicFile(_ seed: UInt8 = 0) throws -> Data {
        let raw = try crypto.ed25519PublicKey(seed: Data((0..<32).map { seed &+ UInt8($0) }))
        return Data(("ssh-ed25519 " + publicBlob(raw).base64EncodedString() + " synthetic@example.invalid\n").utf8)
    }
    private func hostLine(_ names: String, seed: UInt8 = 0, marker: String = "") throws -> String {
        (marker.isEmpty ? "" : marker + " ") + names + " " + String(decoding: try publicFile(seed), as: UTF8.self)
    }
    private func hashedName(_ lookup: String) throws -> String {
        let salt = Data(0..<20)
        let digest = try crypto.hmacSHA1(key: salt, message: Data(lookup.utf8))
        return "|1|" + salt.base64EncodedString() + "|" + digest.base64EncodedString()
    }
    private func hashedLine(_ lookup: String, seed: UInt8 = 0, marker: String = "") throws -> String {
        try hostLine(hashedName(lookup), seed: seed, marker: marker)
    }
    private func records(_ result: SSHWorkspaceExport, _ name: String) throws -> [[String: JSONValue]] {
        try StrictJSON.decode(result.workspace.raw).object().required("workspace").object().required(name).array().map { try $0.object() }
    }
    private func exported(_ text: String, aliases: [String]? = nil, keys: [String: Data] = [:], known: Data? = nil) throws -> SSHWorkspaceExport {
        let input = try source(text)
        var registry = SSHWorkspaceRegistry()
        return try input.export(aliases: aliases ?? input.aliases, workspaceName: "Selected machines", defaultUser: "local-user",
                                publicKeys: keys, knownHosts: known, registry: &registry, using: crypto)
    }

    func testConcreteInventoryFirstValueWildcardNegationAndIdentityOrder() throws {
        let input = try source("""
        User global-first
        Host build gpu build *.invalid !excluded
          User ignored-later
          IdentityFile ~/.ssh/team
        Host * !gpu
          Port=2200
          IdentityFile "/opt/keys/shared key"
        Host build
          HostName build.example.invalid # ordinary comment
          Port 9999
        Host gpu
          HostName=gpu.example.invalid
          Port 2222
        """)
        XCTAssertEqual(input.aliases, ["build", "gpu"])
        XCTAssertEqual(try input.identityFileReferences(for: ["gpu", "build"]), ["~/.ssh/team", "/opt/keys/shared key"])
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: ["build", "gpu"], workspaceName: "Fleet", defaultUser: "fallback",
                                      registry: &registry, using: crypto)
        XCTAssertEqual(result.workspace.routes.map(\.host), ["build.example.invalid", "gpu.example.invalid"])
        XCTAssertEqual(result.workspace.routes.map(\.port), [2200, 2222])
        XCTAssertEqual(result.workspace.routes.map(\.user), ["global-first", "global-first"])
        XCTAssertEqual(result.workspace.groups[0].machineOrder, result.workspace.machines.map(\.id))
        XCTAssertFalse(result.workspace.machines.contains(where: \.autoConnect))
        XCTAssertThrowsError(try input.identityFileReferences(for: ["*.invalid"]))
        XCTAssertThrowsError(try input.identityFileReferences(for: ["build", "build"]))
    }

    func testStableRegistrySurvivesSerializationReorderAndEndpointChanges() throws {
        var registry = SSHWorkspaceRegistry()
        let input = try source("Host build gpu\nUser operator\nIdentityFile none\n")
        let first = try input.export(aliases: ["build", "gpu"], workspaceName: "Fleet", defaultUser: "fallback", registry: &registry, using: crypto)
        registry = try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: JSONEncoder().encode(registry))
        let changed = try source("Host gpu\nHostName new-gpu.example.invalid\nHost build\nHostName new-build.example.invalid\n")
        let second = try changed.export(aliases: ["gpu", "build"], workspaceName: "New display name", defaultUser: "fallback", registry: &registry, using: crypto)
        XCTAssertEqual(first.workspace.id, second.workspace.id)
        XCTAssertEqual(first.workspace.exporterID, second.workspace.exporterID)
        XCTAssertEqual(Array(first.workspace.machines.map(\.id).reversed()), second.workspace.machines.map(\.id))
        XCTAssertEqual(Array(first.workspace.routes.map(\.id).reversed()), second.workspace.routes.map(\.id))
        XCTAssertNotEqual(first.workspace.bundleID, second.workspace.bundleID)
        let a = try StrictJSON.decode(first.workspace.raw).object().required("source").object()
        let b = try StrictJSON.decode(second.workspace.raw).object().required("source").object()
        XCTAssertEqual(try a.text("kind"), "ssh_config")
        XCTAssertNotEqual(a["configuration_revision"], b["configuration_revision"])
    }

    func testActualPublicIdentityDedupAndEncryptedOfflineRoundtrip() throws {
        let input = try source("""
        Host build gpu
          User operator
          IdentityFile ~/.ssh/first
          IdentityFile ~/.ssh/same-material
          IdentitiesOnly yes
          PreferredAuthentications publickey
          PasswordAuthentication no
          KbdInteractiveAuthentication no
        """)
        let known = try hostLine("build,gpu", seed: 64)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: input.aliases, workspaceName: "Fleet", defaultUser: "fallback",
            publicKeys: ["~/.ssh/first": try publicFile(), "~/.ssh/same-material": try publicFile()],
            knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        XCTAssertEqual(result.workspace.credentials.count, 1)
        let credential = try XCTUnwrap(result.workspace.credentials.first)
        let routes = try records(result, "routes")
        for route in routes {
            XCTAssertEqual(route["credential_refs"], .array([.string(credential.id)]))
            XCTAssertEqual(route["authentication_methods"], .array([.string("publickey")]))
            XCTAssertNil(route["required_capabilities"])
        }
        let (original, fingerprint) = try sampleKey(using: crypto)
        XCTAssertEqual(credential.fingerprint, fingerprint)
        let key = PrivateKeyRecord(credentialID: credential.id, container: original.container, publicBlob: original.publicBlob)
        let archive = try AirGapArchive(workspace: result.workspace, keys: [key], portability: result.portability, using: crypto)
        let encrypted = try FTAG.seal(archive, using: crypto)
        let recovered = try FTAG.open(encrypted.envelope, code: encrypted.code, using: crypto)
        XCTAssertEqual(recovered.workspace.raw, result.workspace.raw)
        XCTAssertEqual(recovered.keys.count, 1)
        XCTAssertEqual(recovered.keys[0].publicBlob, original.publicBlob)
        XCTAssertEqual(result.identityReferences[credential.id], "~/.ssh/first")
        let json = String(decoding: result.workspace.raw, as: UTF8.self)
        XCTAssertFalse(json.contains("~/.ssh"))
        XCTAssertFalse(json.contains("synthetic@example.invalid"))
        XCTAssertFalse(json.contains(original.container.base64EncodedString()))
    }

    func testMissingIdentityAndTrustRemainValidWithoutInventedReferences() throws {
        let result = try exported("Host build\nHostName build.example.invalid\nIdentityFile ~/.ssh/not-supplied\nPreferredAuthentications publickey\n")
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        let route = try XCTUnwrap(records(result, "routes").first)
        let trust = try XCTUnwrap(records(result, "host_trust").first)
        XCTAssertEqual(route["credential_refs"], .array([]))
        XCTAssertEqual(trust["pins"], .array([]))
        XCTAssertEqual(trust["revoked_keys"], .array([]))
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.credentials" })
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.host_trust" })
        XCTAssertFalse(result.workspace.machines[0].autoConnect)
    }

    func testDefaultIdentityPolicyIsExplicitAndNoneOrDisabledPublicKeyPreventsReads() throws {
        let ordinary = try source("Host build\n")
        XCTAssertEqual(try ordinary.identityFileReferences(for: ["build"]), ["~/.ssh/id_rsa", "~/.ssh/id_ecdsa", "~/.ssh/id_ecdsa_sk", "~/.ssh/id_ed25519", "~/.ssh/id_ed25519_sk", "~/.ssh/id_mldsa44_ed25519"])
        let none = try source("Host build\nIdentityFile none\n")
        XCTAssertTrue(try none.identityFileReferences(for: ["build"]).isEmpty)
        let disabled = try source("Host build\nPubkeyAuthentication no\nIdentityFile ~/.ssh/explicit\n")
        XCTAssertTrue(try disabled.identityFileReferences(for: ["build"]).isEmpty)
        let result = try exported("Host build\n", keys: ["~/.ssh/id_ed25519": try publicFile()])
        XCTAssertEqual(result.workspace.credentials.count, 1)
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.default_identities" && $0.disposition == 0 })
    }

    func testKnownHostsPinsRevocationsNegationAndPortBinding() throws {
        let known = try hostLine("*,!gpu", seed: 8) + hostLine("gpu", seed: 16) + hostLine("*", seed: 8, marker: "@revoked")
            + hostLine("[2001:db8::1]:2222", seed: 24) + hostLine("[2001:db8::1]:22", seed: 32)
        let result = try exported("Host build gpu\nIdentityFile none\nHost ipv6\nHostName 2001:db8::1\nPort 2222\nIdentityFile none\n", known: Data(known.utf8))
        let trusts = try records(result, "host_trust")
        XCTAssertEqual(try trusts[0].required("pins").array().count, 1)
        XCTAssertEqual(try trusts[1].required("pins").array().count, 1)
        XCTAssertEqual(try trusts[2].required("pins").array().count, 2)
        for trust in trusts { XCTAssertEqual(try trust.required("revoked_keys").array().count, 1) }
        XCTAssertEqual(try trusts[0].required("pins").array()[0], try trusts[0].required("revoked_keys").array()[0], "A revoked pin remains explicitly denied; export must not drop the tombstone.")
        XCTAssertNotEqual(trusts[0]["pins"], trusts[1]["pins"])
    }

    func testHostKeyAliasUsesItsOwnLookupNameAndCanonicalEndpointKeepsDisplayAlias() throws {
        let known = try hostLine("verified-alias", seed: 8) + hostLine("[verified-alias]:2200", seed: 16)
            + hostLine("build.example.invalid.", seed: 24)
        let result = try exported("""
        Host build_node
          HostName BUILD.Example.Invalid.
          HostKeyAlias verified-alias
          Port 2200
          IdentityFile none
        Host normal
          HostName BUILD.Example.Invalid.
          IdentityFile none
        """, known: Data(known.utf8))
        XCTAssertEqual(result.workspace.machines[0].name, "build_node")
        XCTAssertEqual(result.workspace.routes.map(\.host), ["build.example.invalid", "build.example.invalid"])
        let routes = try records(result, "routes"), trusts = try records(result, "host_trust")
        XCTAssertEqual(routes[0]["host_key_alias"], .string("verified-alias"))
        XCTAssertEqual(trusts[0]["host_key_alias"], .string("verified-alias"))
        XCTAssertNil(routes[0]["required_capabilities"])
        XCTAssertEqual(try trusts[0].required("pins").array().count, 1)
        XCTAssertEqual(try trusts[1].required("pins").array().count, 1)
        XCTAssertNotEqual(trusts[0]["pins"], trusts[1]["pins"])
    }

    func testNormalizationNeverDropsOriginalOrCanonicalRevocationOrBroadensPositiveTrust() throws {
        let known = try hostLine("verified-alias.", seed: 8, marker: "@revoked")
            + hostLine("verified-alias", seed: 16, marker: "@revoked")
            + hostLine("verified-alias.", seed: 24) + hostLine("verified-alias", seed: 32)
        let result = try exported("Host build\nHostKeyAlias Verified-Alias.\nPort 2200\nIdentityFile none\n", known: Data(known.utf8))
        let trust = try XCTUnwrap(records(result, "host_trust").first)
        XCTAssertEqual(trust["host_key_alias"], .string("verified-alias"))
        XCTAssertEqual(try trust.required("pins").array().count, 1, "Only the source lookup may supply positive trust.")
        XCTAssertEqual(try trust.required("revoked_keys").array().count, 2, "Neither spelling's denial may disappear during normalization.")
    }

    func testUnevaluatedCodeIncludesAndSecurityConstraintsStayBlockedAndInert() throws {
        let marker = "/tmp/frankenterm-should-never-execute-" + UUID().uuidString
        let input = try source("""
        Include /unreadable/config-fragment
        Host build
          IdentityFile none
          Ciphers aes256-gcm@openssh.com
          RevokedHostKeys /required/host-revocations
          ProxyCommand sh -c "echo SENSITIVE_VALUE > \(marker)"
        Match exec "touch \(marker)"
          HostName never-evaluated.invalid
        Host gpu
          HostName gpu.example.invalid
          IdentityFile none
        """)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: input.aliases, workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)
        XCTAssertFalse(FileManager.default.fileExists(atPath: marker))
        for route in try records(result, "routes") {
            XCTAssertEqual(route["required_capabilities"], .array([.string("ssh_config.unresolved_policy")]))
        }
        for setting in ["include", "match", "ciphers", "revokedhostkeys", "proxycommand"] {
            XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config." + setting && $0.disposition == 1 }, setting)
        }
        XCTAssertEqual(result.workspace.routes[0].host, "build")
        let json = String(decoding: result.workspace.raw, as: UTF8.self)
        XCTAssertFalse(json.contains("SENSITIVE_VALUE"))
        XCTAssertFalse(json.contains(marker))
        XCTAssertFalse(result.portability.contains { $0.reason.contains("SENSITIVE_VALUE") || $0.reason.contains(marker) })
    }

    func testFirstProxyWinsAndUnknownPolicyDoesNotBlockUnrelatedRoute() throws {
        let result = try exported("""
        Host ordinary
          ProxyCommand none
          ProxyJump unused-bastion
          IdentityFile none
        Host restricted
          HostKeyAlgorithms ssh-ed25519
          KexAlgorithms curve25519-sha256
          IdentityFile none
        """)
        let routes = try records(result, "routes")
        XCTAssertNil(routes[0]["required_capabilities"])
        XCTAssertNotNil(routes[1]["required_capabilities"])
        XCTAssertFalse(result.portability.contains { $0.entityID == result.workspace.routes[0].id && $0.disposition == 1 })
        XCTAssertTrue(result.portability.contains { $0.entityID == result.workspace.routes[1].id && $0.setting == "ssh_config.hostkeyalgorithms" })
    }

    func testMalformedHashedTrustAndHostCAsNeverBecomePositivePins() throws {
        let known = try hostLine("|1|c2FsdA==|aGFzaA==", marker: "@revoked")
            + hostLine("build", seed: 4, marker: "@cert-authority")
            + hostLine("build", seed: 8)
        let result = try exported("Host build gpu\nIdentityFile none\n", known: Data(known.utf8))
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.host_ca" && $0.disposition == 1 })
        XCTAssertEqual(result.portability.filter { $0.setting == "ssh_config.hashed_known_hosts" && $0.disposition == 1 }.count, 2)
        let trusts = try records(result, "host_trust")
        XCTAssertEqual(try trusts[0].required("pins").array().count, 1)
        XCTAssertTrue(try trusts[1].required("pins").array().isEmpty)
        let malformed = try exported("Host build\nIdentityFile none\n", known: Data("@revoked incomplete\n".utf8))
        XCTAssertNotNil(try records(malformed, "routes")[0]["required_capabilities"])
    }

    func testKnownHostHMACMatchesIndependentKnownAnswerAndBoundsInputs() throws {
        // RFC 2202 test case 1, independent of both native implementations.
        let result = try crypto.hmacSHA1(key: Data(repeating: 0x0b, count: 20), message: Data("Hi There".utf8))
        XCTAssertEqual(result.ftHex.lowercased(), "b617318655057264e28bc0b6fb378c8ef146be00")
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 19), message: Data([1])))
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 21), message: Data([1])))
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 20), message: Data()))
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 20), message: Data(repeating: 1, count: FTLimit.text + 1)))
    }

    func testIndependentOpenSSHHashedFixturesMatchExactSourceLookupNames() throws {
        // Generated with `ssh-keygen -H -f <synthetic-known-hosts>` and each
        // independently located with `ssh-keygen -F <lookup> -f <file>`.
        // The key is RFC 8032's public test vector; it cannot convey a secret.
        let fixtures = [
            "|1|aDeDAUKmHFuLWoGhGWF42FpIfik=|eWeKZpxS/fL0nuUZsSEzazPtB74=", // build.example.invalid
            "|1|s8qDi2B3JjeqdlcGU8FEnJK+0xI=|Uf6sewTNMZrOm+o1IbYL6eSQGlk=", // [port.example.invalid]:2200
            "|1|YfWZDMVb2Cq66apUKfjfhqOxOwo=|6PW29LS887CTgKV+QBDMrRcmY9k=", // [2001:db8::1]:2222
            "|1|398GT5aJEJgtUuUoSid98Ynu3MQ=|MekHkSHl8j6umpJvS6zjrp/4cPI=", // verified-alias.
            "|1|E+aW6MMGRqNIei+xpbk+mTqZ1Ac=|/jJzSlOu3MNdKq9tc3+4qgBYqIA=", // [::ffff:192.0.2.1]:2222
            "|1|KkQ0QueSKOljC6ONfi094X2qZdk=|1T3UVv/Bbh8zQ+xdMoTyASilYUE=" // [::192.0.2.1]:2222
        ]
        let publicKey = " ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea\n"
        let unrelated = "|1|0rxKPTlV0RN+uyV1ePRHg/HycR0=|r8dv/qGevaDsAhPp9dExqPvvJ6E="
        let known = fixtures.map { $0 + publicKey }.joined() + "@revoked " + unrelated + publicKey
        let result = try exported("""
        IdentityFile none
        Host dns
          HostName BUILD.Example.Invalid
        Host port
          HostName Port.Example.Invalid
          Port 2200
        Host ipv6
          HostName 2001:0DB8:0:0::1
          Port 2222
        Host alias
          HostName endpoint.example.invalid
          HostKeyAlias Verified-Alias.
          Port 65535
        Host mapped
          HostName ::FFFF:C000:201
          Port 2222
        Host compatible
          HostName ::C000:201
          Port 2222
        Host unmatched
          HostName not-in-this-file.example.invalid
        """, known: Data(known.utf8))
        for trust in result.workspace.hostTrust.prefix(6) {
            XCTAssertEqual(trust.pins.count, 1)
            XCTAssertTrue(trust.revokedKeys.isEmpty)
            XCTAssertEqual(trust.evaluate(try XCTUnwrap(trust.pins.first), locallyAccepted: false), .reviewRequired)
        }
        XCTAssertTrue(result.workspace.hostTrust[6].pins.isEmpty)
        XCTAssertTrue(result.workspace.hostTrust[6].revokedKeys.isEmpty)
        XCTAssertEqual(result.workspace.routes[4].host, "192.0.2.1")
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        XCTAssertFalse(result.portability.contains { $0.setting == "ssh_config.hashed_known_hosts" })
        let json = String(decoding: result.workspace.raw, as: UTF8.self)
        XCTAssertFalse(json.contains("|1|"))
        XCTAssertFalse(json.contains("aDeDAUKmHFuLWoGhGWF42FpIfik="))
    }

    func testHashedLookupDoesNotBroadenTrustAcrossPortsCaseOrTrailingDots() throws {
        let known = try hashedLine("[build.example.invalid]:2200")
            + hashedLine("verified-alias.") + hashedLine("UPPER.example.invalid")
            + hashedLine("build.example.invalid")
        let result = try exported("""
        IdentityFile none
        Host wrong-port
          HostName build.example.invalid
          Port 2201
        Host alias-without-dot
          HostKeyAlias verified-alias
          Port 2200
        Host lowercased
          HostName UPPER.example.invalid
        Host dotted
          HostName build.example.invalid.
        """, known: Data(known.utf8))
        XCTAssertTrue(result.workspace.hostTrust.allSatisfy { $0.pins.isEmpty })
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
    }

    func testHashedRevocationsSurviveNormalizationAndOverridePositivePins() throws {
        let known = try hashedLine("verified-alias.", seed: 8, marker: "@revoked")
            + hashedLine("verified-alias", seed: 16, marker: "@revoked")
            + hashedLine("verified-alias.", seed: 16) + hashedLine("verified-alias", seed: 32)
            + hashedLine("unrelated-alias", seed: 48, marker: "@revoked")
        let result = try exported("Host build\nHostKeyAlias Verified-Alias.\nPort 2200\nIdentityFile none\n", known: Data(known.utf8))
        let trust = try XCTUnwrap(result.workspace.hostTrust.first)
        XCTAssertEqual(trust.pins.count, 1)
        XCTAssertEqual(trust.revokedKeys.count, 2)
        let pin = try XCTUnwrap(trust.pins.first)
        XCTAssertEqual(trust.evaluate(pin, locallyAccepted: true), .revoked)
        XCTAssertEqual(try result.workspace.verifyHostKey(forRouteID: result.workspace.routes[0].id, presented: pin,
                                                         reviewedTrustIDs: [trust.id]), .revoked)
    }

    func testHashedCertificateAndUnknownMarkerConstraintsStayInactiveOnlyWhenApplicable() throws {
        for (marker, setting) in [("@cert-authority", "host_ca"), ("@future-policy", "known_hosts_marker")] {
            // A canonical-spelling constraint must survive source normalization,
            // even though a canonical-spelling positive pin is not broadened.
            let known = try hashedLine("verified-alias", seed: 8, marker: marker)
                + hashedLine("verified-alias.", seed: 16)
            let result = try exported("IdentityFile none\nHost matched\nHostKeyAlias Verified-Alias.\nHost unrelated\n", known: Data(known.utf8))
            XCTAssertEqual(result.workspace.hostTrust[0].pins.count, 1)
            XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty)
            XCTAssertTrue(result.workspace.routes[1].requiredCapabilities.isEmpty)
            XCTAssertTrue(result.portability.contains {
                $0.entityID == result.workspace.routes[0].id && $0.setting == "ssh_config." + setting && $0.disposition == 1
            })
        }
        let invalidKey = try hashedName("build") + " ssh-ed25519 not-base64\n"
        let result = try exported("Host build unrelated\nIdentityFile none\n", known: Data(invalidKey.utf8))
        XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty)
        XCTAssertTrue(result.workspace.routes[1].requiredCapabilities.isEmpty)
        XCTAssertTrue(result.workspace.hostTrust.allSatisfy { $0.pins.isEmpty })
    }

    func testMalformedOrUnsupportedHashesCannotHideRevocations() throws {
        let valid = "|1|aDeDAUKmHFuLWoGhGWF42FpIfik=|eWeKZpxS/fL0nuUZsSEzazPtB74="
        let invalid = [
            valid.replacingOccurrences(of: "|1|", with: "|2|"),
            "|1|c2FsdA==|eWeKZpxS/fL0nuUZsSEzazPtB74=",
            "|1|aDeDAUKmHFuLWoGhGWF42FpIfik=|aGFzaA==",
            String(valid.dropFirst()), String(valid.dropLast()), valid + "|extra",
            String(valid.dropLast(2)) + "5=", "!" + valid,
            valid + ",build", "build," + valid, "*" + valid
        ]
        for value in invalid {
            let known = try hostLine(value, marker: "@revoked")
            let result = try exported("Host build unrelated\nIdentityFile none\n", known: Data(known.utf8))
            XCTAssertTrue(result.workspace.routes.allSatisfy { !$0.requiredCapabilities.isEmpty }, value)
            XCTAssertTrue(result.workspace.hostTrust.allSatisfy { $0.pins.isEmpty && $0.revokedKeys.isEmpty }, value)
            XCTAssertEqual(result.portability.filter { $0.setting == "ssh_config.hashed_known_hosts" && $0.disposition == 1 }.count, 2)
        }
        let conflictingMarkers = "@revoked @cert-authority " + valid + " " + String(decoding: try publicFile(), as: UTF8.self)
        let result = try exported("Host build\nIdentityFile none\n", known: Data(conflictingMarkers.utf8))
        XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty)
    }

    func testHashedTrustComparisonLimitCannotPublishPartialRegistryOrSelection() throws {
        let names = (0..<128).map { "alias-\($0)" }
        let input = try source("Host " + names.joined(separator: " ") + "\nIdentityFile none\n")
        let repeated = try hashedLine("not-selected.example.invalid")
        let count = SSHWorkspaceSource.maximumKnownHostHashComparisons / names.count + 1
        let known = Data(String(repeating: repeated, count: count).utf8)
        var registry = SSHWorkspaceRegistry()
        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
        let before = try encoder.encode(registry)
        XCTAssertThrowsError(try input.export(aliases: names, workspaceName: "Fleet", defaultUser: "operator",
                                              knownHosts: known, registry: &registry, using: crypto)) { error in
            XCTAssertEqual(error as? FTError, .limit("ssh.known_hosts_hash_work"))
        }
        XCTAssertEqual(try encoder.encode(registry), before)
    }

    func testAllSupportedECDSACurvesAreOfferedAsExportableIdentityFamilies() throws {
        func ssh(_ data: Data) -> Data {
            var prefix = Data(); prefix.appendBE(UInt64(data.count), bytes: 4)
            return prefix + data
        }
        var configuration = "", keys: [String: Data] = [:]
        for curve in SSHECDSACurve.allCases {
            let reference = "/selected/" + curve.rawValue
            var scalar = Data(repeating: 0, count: curve.scalarBytes); scalar[scalar.count - 1] = 1
            let point = try crypto.ecdsaPublicKey(scalar: scalar, curve: curve)
            let blob = ssh(Data(curve.algorithm.utf8)) + ssh(Data(curve.rawValue.utf8)) + ssh(point)
            keys[reference] = Data((curve.algorithm + " " + blob.base64EncodedString()).utf8)
            configuration += "Host " + curve.rawValue + "\nIdentityFile " + reference + "\n"
        }
        let result = try exported(configuration, keys: keys)
        XCTAssertEqual(result.workspace.credentials.map(\.algorithm), SSHECDSACurve.allCases.map(\.algorithm))
        XCTAssertFalse(result.portability.contains { $0.setting == "ssh_config.private_key_format" })
    }

    func testOrderedAuthenticationDisallowsDisabledMethodsAndFailsClosedWhenEmpty() throws {
        let result = try exported("""
        Host build
          PreferredAuthentications password,publickey,keyboard-interactive
          PasswordAuthentication no
          ChallengeResponseAuthentication yes
          KbdInteractiveAuthentication no
          IdentityFile none
        Host blocked
          PubkeyAuthentication no
          PasswordAuthentication no
          KbdInteractiveAuthentication no
        """)
        let routes = try records(result, "routes")
        XCTAssertEqual(routes[0]["authentication_methods"], .array([.string("publickey"), .string("keyboard-interactive")]))
        XCTAssertNotNil(routes[1]["required_capabilities"])
        XCTAssertTrue(result.portability.contains { $0.entityID == result.workspace.routes[1].id && $0.setting == "ssh_config.authentication" && $0.disposition == 1 })
    }

    func testInvalidPublicKeyAndUnresolvedTokensCannotPublishUsableIdentity() throws {
        let invalid = Data("ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n".utf8)
        let result = try exported("""
        Host build
          HostName %h.internal
          IdentityFile ~/.ssh/invalid
          IdentityFile ~/.ssh/%r-key
        """, keys: ["~/.ssh/invalid": invalid])
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        XCTAssertNotNil(try records(result, "routes")[0]["required_capabilities"])
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.hostname" && $0.disposition == 1 })
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.identityfile" && $0.disposition == 1 })
        XCTAssertEqual(try source("Host build\nIdentityFile ~/.ssh/%r-key\n").identityFileReferences(for: ["build"]), [])
    }

    func testNoPartialIdentityPrefixAndInvalidRegistryHasNoPublication() throws {
        let identities = (0..<9).map { "IdentityFile ~/.ssh/key-\($0)" }.joined(separator: "\n")
        let input = try source("Host build\n" + identities)
        XCTAssertTrue(try input.identityFileReferences(for: ["build"]).isEmpty)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: ["build"], workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.identityfile" && $0.disposition == 1 })
        let corrupt = Data(#"{"exporterID":"same","workspaceID":"same","groupID":"group","themeID":"theme","entities":{}}"#.utf8)
        registry = try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: corrupt)
        XCTAssertThrowsError(try input.export(aliases: ["build"], workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto))
    }

    func testSourceAdmissionBoundsMalformedSyntaxAndSelection() throws {
        XCTAssertThrowsError(try SSHWorkspaceSource(configuration: Data(repeating: 65, count: SSHWorkspaceSource.maximumSourceBytes + 1)))
        XCTAssertThrowsError(try source(String(repeating: "\n", count: SSHWorkspaceSource.maximumLines)))
        XCTAssertThrowsError(try source("Host \"unterminated\n"))
        XCTAssertThrowsError(try source("Host build\nHostName a\0b\n"))
        XCTAssertThrowsError(try source("Host " + String(repeating: "x", count: 255)))
        let tooMany = (0...SSHWorkspaceSource.maximumAliases).map { "Host alias-\($0)" }.joined(separator: "\n")
        XCTAssertThrowsError(try source(tooMany))
        XCTAssertThrowsError(try exported("Host *\nUser operator\n"))
        XCTAssertThrowsError(try exported("Host build\n", aliases: ["not-listed"]))
        XCTAssertThrowsError(try exported("Host build\n", known: Data(repeating: 65, count: SSHWorkspaceSource.maximumSourceBytes + 1)))
    }
}
