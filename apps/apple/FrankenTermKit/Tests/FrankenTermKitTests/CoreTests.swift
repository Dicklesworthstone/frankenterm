import Foundation
import XCTest
@testable import FrankenTermKit

func publicBlob(_ raw: Data) -> Data {
    var result = Data()
    for value in [Data("ssh-ed25519".utf8), raw] { result.appendBE(UInt64(value.count), bytes: 4); result += value }
    return result
}

func sampleManifest(fingerprint: String = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", bundleID: String = "bundle") throws -> Data {
    let palette = Array(repeating: "#123456", count: 16)
    let theme: [String: Any] = ["id": "theme", "name": "Cobalt", "foreground": "#FFFFFF", "background": "#10223C", "accent": "#3B82F6", "ansi": palette,
                                "cursor_foreground": "#000000", "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF", "selection_background": "#345D91"]
    let workspace: [String: Any] = [
        "id": "work", "name": "Machines", "groups": [["id": "group", "name": "Agents", "machine_order": ["build", "gpu"], "auto_connect": true]],
        "machines": ["build", "gpu"].map { ["id": $0, "name": $0, "theme_ref": "theme", "route_refs": ["route"], "preferred_route_ref": "route", "auto_connect": true] as [String: Any] },
        "routes": [["id": "route", "mode": "direct_ssh", "session_creation": "on_select", "host": "host.example.invalid", "port": 22, "user": "operator", "credential_refs": ["key"], "authentication_methods": ["publickey"], "host_trust_ref": "trust", "jump_routes": []]],
        "credentials": [["id": "key", "algorithm": "ssh-ed25519", "public_fingerprint": fingerprint, "requirement": "matching_private_key", "private_material": "not_included"]],
        "host_trust": [["id": "trust", "host": "host.example.invalid", "port": 22, "revoked_keys": [], "pins": [["algorithm": "ssh-ed25519", "fingerprint": "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]]]],
        "themes": [theme], "preferences": ["initial_machine_ref": "build", "initial_group_ref": "group", "connect_on_open": true]
    ]
    return try JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1, "bundle_id": bundleID, "source": ["exporter_id": "mac"], "required_capabilities": [], "workspace": workspace], options: [.sortedKeys])
}

func sampleKey(using crypto: TestCryptography) throws -> (PrivateKeyRecord, String) {
    let seed = Data(0..<32), key = try crypto.ed25519PublicKey(seed: seed), blob = publicBlob(key)
    func ssh(_ data: Data) -> Data { var value = Data(); value.appendBE(UInt64(data.count), bytes: 4); return value + data }
    var part = Data([1, 2, 3, 4, 1, 2, 3, 4]) + ssh(Data("ssh-ed25519".utf8)) + ssh(key) + ssh(seed + key) + ssh(Data("synthetic".utf8))
    var padding: UInt8 = 1
    while !part.count.isMultiple(of: 8) { part.append(padding); padding += 1 }
    let container = Data("openssh-key-v1\0".utf8) + ssh(Data("none".utf8)) + ssh(Data("none".utf8)) + ssh(Data()) + Data([0, 0, 0, 1]) + ssh(blob) + ssh(part)
    return (PrivateKeyRecord(credentialID: "key", container: container, publicBlob: blob), "SHA256:" + crypto.sha256(blob).base64EncodedString().replacingOccurrences(of: "=", with: ""))
}

final class CoreTests: XCTestCase {
    let crypto = TestCryptography()
    func testLocalHostRevocationPrecedesAlgorithmReadinessForEachJump() throws {
        let document = try WorkspaceDocument(data: changedWorkspace { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            var pins = trusts[0]["pins"] as! [[String: Any]]
            pins.append(["algorithm": "ssh-rsa", "fingerprint": "SHA256:ERERERERERERERERERERERERERERERERERERERERERE"])
            trusts[0]["pins"] = pins
            var hopTrust = trusts[0]; hopTrust["id"] = "hop-trust"; hopTrust["host"] = "jump.example.invalid"
            trusts.append(hopTrust); workspace["host_trust"] = trusts
            var routes = workspace["routes"] as! [[String: Any]], hop = routes[0]
            hop["id"] = "jump"; hop["host"] = "jump.example.invalid"; hop["host_trust_ref"] = "hop-trust"
            routes[0]["jump_routes"] = ["jump"]; routes.append(hop); workspace["routes"] = routes
        })
        let ed25519 = try XCTUnwrap(document.hostTrust[0].pins.first)
        let reviewed: Set<String> = ["trust", "hop-trust"]
        XCTAssertTrue(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: reviewed,
            support: qualifiedTestTransport).isLocallyReady)
        let targetRevoked = try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: reviewed,
            locallyRevokedHostKeys: ["trust": [ed25519]], support: qualifiedTestTransport)
        XCTAssertEqual(targetRevoked.issues, [.hostAlgorithmUnavailable], "An unrevoked unsupported pin cannot replace a revoked supported pin.")
        let hopRevocations: [String: Set<WorkspaceDocument.HostKey>] = ["hop-trust": [ed25519]]
        XCTAssertEqual(try document.readiness(forRouteID: "jump", availableCredentialIDs: ["key"], reviewedTrustIDs: reviewed,
            locallyRevokedHostKeys: hopRevocations, support: qualifiedTestTransport).issues, [.hostAlgorithmUnavailable])
        XCTAssertEqual(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: reviewed,
            locallyRevokedHostKeys: hopRevocations, support: qualifiedTestTransport).issues, [.blockedJumpRoute("jump")])
        XCTAssertEqual(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: reviewed,
            locallyRevokedHostKeys: ["trust": Set(document.hostTrust[0].pins)], support: qualifiedTestTransport).issues, [.hostIdentityRevoked])
    }

    func testEveryCoreRecordRejectsHiddenPrivateMaterial() throws {
        for collection in ["groups", "machines", "routes", "credentials", "host_trust", "themes"] {
            XCTAssertThrowsError(try WorkspaceDocument(data: changedWorkspace { workspace in
                var records = workspace[collection] as! [[String: Any]]
                records[0]["private_key"] = "synthetic-secret-must-not-persist"; workspace[collection] = records
            }), collection)
        }
        for scope in ["root", "source", "workspace", "preferences", "pin"] {
            var root = try JSONSerialization.jsonObject(with: sampleManifest()) as! [String: Any]
            var workspace = root["workspace"] as! [String: Any]
            switch scope {
            case "root": root["private_key"] = "secret"
            case "source": var source = root["source"] as! [String: Any]; source["private_key"] = "secret"; root["source"] = source
            case "workspace": workspace["private_key"] = "secret"
            case "preferences": var preferences = workspace["preferences"] as! [String: Any]; preferences["private_key"] = "secret"; workspace["preferences"] = preferences
            default:
                var trusts = workspace["host_trust"] as! [[String: Any]], pins = trusts[0]["pins"] as! [[String: Any]]
                pins[0]["private_key"] = "secret"; trusts[0]["pins"] = pins; workspace["host_trust"] = trusts
            }
            root["workspace"] = workspace
            XCTAssertThrowsError(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), scope)
        }
    }

    func testHostNormalizationBindsTrustToCanonicalEndpoint() throws {
        for (routeHost, trustHost, expected) in [("HOST.Example.Invalid.", "host.example.invalid", "host.example.invalid"),
                                                ("[2001:db8:0:0:0:0:0:1]", "2001:db8::1", "2001:db8::1"),
                                                ("bücher.example", "xn--bcher-kva.example", "xn--bcher-kva.example")] {
            let document = try WorkspaceDocument(data: changedWorkspace { workspace in
                var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
                routes[0]["host"] = routeHost; trusts[0]["host"] = trustHost; workspace["routes"] = routes; workspace["host_trust"] = trusts
            })
            XCTAssertEqual(document.routes[0].host, expected); XCTAssertEqual(document.hostTrust[0].host, expected)
        }
        for invalid in ["host/path", "host#fragment", "-bad.example", "bad..example", "fe80::1%en0", "1.2.3.999", "01.2.3.4", "2001:db8::xyz"] {
            XCTAssertThrowsError(try WorkspaceDocument.host(invalid), invalid)
        }
    }

    func testPersistedHostIdentityDecodingCannotSkipValidation() throws {
        let key = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        XCTAssertEqual(try JSONDecoder().decode(WorkspaceDocument.HostKey.self, from: JSONEncoder().encode(key)), key)
        for fields in [
            ["algorithm": "ssh-ed25519", "fingerprint": "invalid"],
            ["algorithm": "", "fingerprint": key.fingerprint],
            ["algorithm": "ssh-ed25519\n", "fingerprint": key.fingerprint],
            ["algorithm": key.algorithm, "fingerprint": key.fingerprint + "="]
        ] {
            XCTAssertThrowsError(try JSONDecoder().decode(WorkspaceDocument.HostKey.self, from: JSONSerialization.data(withJSONObject: fields)))
        }
    }

    func testMappedIPv4CannotAcquireASecondTrustIdentity() throws {
        for spelling in ["::FFFF:C000:0201", "::ffff:192.0.2.1", "[::ffff:192.0.2.1]"] {
            XCTAssertEqual(try WorkspaceDocument.host(spelling), "192.0.2.1")
            let raw = try changedWorkspace { workspace in
                var routes = workspace["routes"] as! [[String: Any]], trusts = workspace["host_trust"] as! [[String: Any]]
                routes[0]["host"] = spelling; trusts[0]["host"] = "192.0.2.1"
                workspace["routes"] = routes; workspace["host_trust"] = trusts
            }
            let document = try WorkspaceDocument(data: raw)
            XCTAssertEqual(document.routes[0].host, document.hostTrust[0].host)
            XCTAssertEqual(document.raw, raw, "Canonical trust identity must preserve the imported display/export bytes.")
        }
    }

    func testEndpointCanonicalFormIsStableAcrossIDNAAndIPv6Variants() throws {
        for (spelling, canonical) in [
            ("BÜCHER.example.", "xn--bcher-kva.example"),
            ("bu\u{0308}cher.example", "xn--bcher-kva.example"),
            ("ＢＵＩＬＤ。ＥＸＡＭＰＬＥ", "build.example"),
            ("host.example。", "host.example"),
            ("192.0.2.1。", "192.0.2.1"),
            ("2001:0:0:1:0:0:1:1", "2001::1:0:0:1:1"),
            ("1:2:3:4:5:6:0:8", "1:2:3:4:5:6:0:8"),
            ("0:0:0:0:0:0:0:0", "::")
        ] {
            XCTAssertEqual(try WorkspaceDocument.host(spelling), canonical, spelling)
            XCTAssertEqual(try WorkspaceDocument.host(canonical), canonical, "Endpoint normalization must be idempotent.")
        }
    }

    func testEndpointAdmissionRefusesResolverAliasesAndEncodedAuthorityDelimiters() throws {
        for invalid in [
            "127.1", "2130706433", "0x7f000001", "0x7f.0.0.1", "0177.0.0.1", "127.00.0.1",
            "ssh://host.example", "user@host.example", "host.example:22", "host%2eexample", "host.example?next=other",
            "host／example", "user＠host.example", "host.example：22", "host.example？next", "host.example＃other",
            "host.example＼path", "host％2eexample", "host\u{00A0}example", "host\0.example",
            "[::1]:22", "[127.0.0.1]", "[::1", "::1]",
            String(repeating: "a", count: 64) + ".example",
            Array(repeating: String(repeating: "a", count: 63), count: 4).joined(separator: ".")
        ] {
            XCTAssertThrowsError(try WorkspaceDocument.host(invalid), invalid)
        }
    }
    func testStrictJSONRejectsDuplicateEscapedKeysAndMalformedUnicode() throws {
        for text in [#"{"a":1,"\u0061":2}"#, #"{"a":1,"a":2}"#, #""\ud800""#, "{\"a\":01}", "[true,]", "{}{}", "NaN", "[1e]", "[+1]", #""\q""#] {
            XCTAssertThrowsError(try StrictJSON.decode(Data(text.utf8)), text)
        }
        XCTAssertEqual(try StrictJSON.decode(Data(#"{"s":"\ud83d\ude00","a":[-1.2e+3,true,null]}"#.utf8)).object()["s"], .string("😀"))
        XCTAssertThrowsError(try StrictJSON.decode(Data((String(repeating: "[", count: 14) + "0" + String(repeating: "]", count: 14)).utf8)))
        XCTAssertThrowsError(try StrictJSON.decode(Data(("[" + Array(repeating: "0", count: 32769).joined(separator: ",") + "]").utf8)))
    }
    func testWorkspacePreservesExactBytesAndOrder() throws {
        let raw = try sampleManifest(), workspace = try WorkspaceDocument(data: raw)
        XCTAssertEqual(workspace.raw, raw)
        XCTAssertEqual(workspace.groups[0].machineOrder, ["build", "gpu"])
        XCTAssertEqual(workspace.initialMachineID, "build")
        XCTAssertEqual(workspace.themes[0].ansi.count, 16)
        XCTAssertTrue(workspace.machines[0].autoConnect)
    }
    func testWorkspaceRejectsBadReferencesTrustAndPolicy() throws {
        let original = String(decoding: try sampleManifest(), as: UTF8.self)
        for bad in [original.replacingOccurrences(of: "\"port\":22", with: "\"port\":65536"),
                    original.replacingOccurrences(of: "\"preferred_route_ref\":\"route\"", with: "\"preferred_route_ref\":\"missing\""),
                    original.replacingOccurrences(of: "\"required_capabilities\":[]", with: "\"required_capabilities\":[\"unknown\"]"),
                    original.replacingOccurrences(of: "\"jump_routes\":[]", with: "\"jump_routes\":[\"route\"]"),
                    original.replacingOccurrences(of: "\"mode\":\"direct_ssh\"", with: "\"mode\":\"via_mac\""),
                    original.replacingOccurrences(of: "\"private_material\":\"not_included\"", with: "\"private_material\":\"secret\"")] {
            XCTAssertNotEqual(bad, original)
            XCTAssertThrowsError(try WorkspaceDocument(data: Data(bad.utf8)))
        }
    }
    func testCurrentPlanFixtureImportsBothModesAndEveryPaletteComponent() throws {
        let repository = (0..<6).reduce(URL(fileURLWithPath: #filePath)) { result, _ in result.deletingLastPathComponent() }
        let bytes = try Data(contentsOf: repository.appendingPathComponent("fixtures/ios-airgap/workspace-v1-native-plan.ftworkspace"))
        let workspace = try WorkspaceDocument(data: bytes)
        XCTAssertEqual(workspace.raw, bytes)
        XCTAssertEqual(workspace.groups[0].machineOrder, ["machine-build", "machine-gpu"])
        XCTAssertTrue(workspace.groups[0].autoConnect)
        XCTAssertTrue(workspace.connectOnOpen)
        XCTAssertEqual(workspace.exportSequence, 7)
        XCTAssertEqual(workspace.machines[1].shortLabel, "GPU")
        XCTAssertEqual(workspace.routes[0].credentialIDs, ["credential-main"])
        XCTAssertEqual(workspace.routes[0].authenticationMethods, [.publicKey])
        XCTAssertEqual(workspace.routes[0].sessionCreation, "on_select")
        XCTAssertEqual(workspace.routes[1].mode, "frankenterm_mux")
        XCTAssertEqual(workspace.routes[1].attachPolicy, "existing_only")
        XCTAssertEqual(workspace.themes[0].cursorForeground, "#10223C")
        XCTAssertEqual(workspace.themes[1].selectionBackground, "#66457D")
        XCTAssertEqual(workspace.themes[0].ansi[8], "#657B99")
        XCTAssertTrue(workspace.hostTrust.allSatisfy { $0.revokedKeys.isEmpty })
        let archive = try AirGapArchive(workspace: workspace, keys: [], using: crypto)
        let encrypted = try FTAG.seal(archive, using: crypto)
        XCTAssertEqual(try FTAG.open(encrypted.envelope, code: encrypted.code, using: crypto).workspace.raw, bytes)
    }

    func testCredentialCandidatesAndPermittedMethodsRemainOrderedAndExplicit() throws {
        var root = try manifestObject()
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        var alias = credentials[0]; alias["id"] = "second-key"; credentials.append(alias)
        workspace["credentials"] = credentials
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        routes[0]["credential_refs"] = ["second-key", "key"]
        routes[0]["authentication_methods"] = ["publickey", "keyboard-interactive"]
        workspace["routes"] = routes; root["workspace"] = workspace
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        let ready = try readiness(document, available: ["key", "second-key"])
        XCTAssertTrue(ready.canAttemptConnection)
        XCTAssertEqual(ready.usableCredentialIDs, ["second-key", "key"])
        XCTAssertEqual(ready.authenticationMethods, [.publicKey, .keyboardInteractive])
        let laterCandidate = try readiness(document, available: ["key"])
        XCTAssertEqual(laterCandidate.usableCredentialIDs, ["key"])
        let missingKeys = try readiness(document, available: [])
        XCTAssertTrue(missingKeys.canAttemptConnection, "a separately permitted interactive method remains available")
        XCTAssertEqual(missingKeys.authenticationMethods, [.keyboardInteractive])

        let original = try WorkspaceDocument(data: sampleManifest())
        let noKey = try readiness(original, available: [])
        XCTAssertFalse(noKey.canAttemptConnection)
        XCTAssertEqual(noKey.issues.map(\.code), [.credentialRequired])
        XCTAssertTrue(noKey.authenticationMethods.isEmpty, "backend password defaults cannot enlarge imported policy")
        let unsupported = try original.readiness(for: "route", availableCredentialIDs: ["key"], acceptedTrustIDs: ["trust"],
            supportedAuthenticationMethods: [.password], supportedCredentialAlgorithms: ["ssh-ed25519"], supportedHostKeyAlgorithms: ["ssh-ed25519"])
        XCTAssertEqual(unsupported.issues.map(\.code), [.authenticationMethodUnsupported])
    }

    func testCredentialAndMethodListsRejectDuplicatesDanglingAndUnboundedPolicies() throws {
        for (field, values) in [
            ("credential_refs", ["key", "key"]), ("credential_refs", ["missing"]),
            ("credential_refs", (0..<9).map { "key\($0)" }),
            ("authentication_methods", []), ("authentication_methods", ["publickey", "publickey"]),
            ("authentication_methods", ["gssapi-with-mic"])
        ] {
            var root = try manifestObject(), workspace = try workspaceObject(root)
            var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
            routes[0][field] = values; workspace["routes"] = routes; root["workspace"] = workspace
            XCTAssertThrowsError(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), field)
        }
        var root = try manifestObject(), workspace = try workspaceObject(root)
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        routes[0]["credential_ref"] = "key"
        workspace["routes"] = routes; root["workspace"] = workspace
        XCTAssertThrowsError(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), "no ambiguous singular compatibility field")
    }

    func testNegativeHostTrustWinsAndUnsupportedPolicyStaysWithItsRoute() throws {
        var root = try manifestObject(), workspace = try workspaceObject(root)
        var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        trusts[0]["revoked_keys"] = trusts[0]["pins"]
        workspace["host_trust"] = trusts; root["workspace"] = workspace
        let revokedDocument = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        let trust = revokedDocument.hostTrust[0], pin = try XCTUnwrap(trust.pins.first)
        XCTAssertEqual(trust.evaluate(pin, locallyAccepted: true), .revoked)
        XCTAssertEqual(trust.evaluate(pin, locallyAccepted: false), .revoked)
        XCTAssertEqual(try readiness(revokedDocument).issues.map(\.code), [.hostIdentityRevoked])

        let ordinary = try WorkspaceDocument(data: sampleManifest())
        XCTAssertEqual(ordinary.hostTrust[0].evaluate(pin, locallyAccepted: false), .reviewRequired)
        XCTAssertEqual(ordinary.hostTrust[0].evaluate(pin, locallyAccepted: true), .trusted)
        XCTAssertEqual(ordinary.hostTrust[0].evaluate(pin, locallyAccepted: true, additionalRevocations: [pin]), .revoked)
        let locallyRevoked = try ordinary.readiness(for: "route", availableCredentialIDs: ["key"], acceptedTrustIDs: ["trust"],
            supportedAuthenticationMethods: [.publicKey], supportedCredentialAlgorithms: ["ssh-ed25519"], supportedHostKeyAlgorithms: ["ssh-ed25519"],
            revokedHostKeysByTrustScope: [ordinary.hostTrust[0].canonicalTrustScope: [pin]])
        XCTAssertEqual(locallyRevoked.issues.map(\.code), [.hostIdentityRevoked])

        root = try manifestObject(); workspace = try workspaceObject(root)
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        var unavailable = routes[0]; unavailable["id"] = "blocked-route"; unavailable["required_capabilities"] = ["mandatory-krl"]
        routes.append(unavailable); workspace["routes"] = routes; root["workspace"] = workspace
        let partial = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        XCTAssertTrue(try readiness(partial).canAttemptConnection)
        XCTAssertEqual(try readiness(partial, route: "blocked-route").issues.map(\.code), [.requiredCapabilityUnavailable])
        routes[0]["jump_routes"] = ["blocked-route"]; workspace["routes"] = routes; root["workspace"] = workspace
        let dependent = try readiness(WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)))
        XCTAssertFalse(dependent.canAttemptConnection)
        XCTAssertEqual(dependent.issues.first?.routeID, "blocked-route")
    }

    func testTrustAliasesCannotBypassNegativeTrustOrRequiredPolicy() throws {
        var root = try manifestObject(), workspace = try workspaceObject(root)
        var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        var restriction = trusts[0]
        restriction["id"] = "same-host-restriction"
        restriction["host"] = "HOST.Example.Invalid."
        restriction["revoked_keys"] = restriction["pins"]
        trusts.append(restriction); workspace["host_trust"] = trusts; root["workspace"] = workspace
        var document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        let pin = try XCTUnwrap(document.hostTrust[0].pins.first)
        XCTAssertEqual(try document.evaluateHostKey(pin, trustID: "trust", locallyAccepted: true), .revoked)
        XCTAssertEqual(try readiness(document).issues.map(\.code), [.hostIdentityRevoked])

        trusts[1]["revoked_keys"] = []
        trusts[1]["required_capabilities"] = ["mandatory-krl"]
        workspace["host_trust"] = trusts; root["workspace"] = workspace
        document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        XCTAssertEqual(try document.evaluateHostKey(pin, trustID: "trust", locallyAccepted: true), .requiredPolicyUnavailable)
        XCTAssertEqual(try readiness(document).issues.map(\.code), [.requiredCapabilityUnavailable])
    }

    func testFingerprintAliasesRequireOneCanonicalPublicKeyAlgorithm() throws {
        var root = try manifestObject(), workspace = try workspaceObject(root)
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        var alias = credentials[0]; alias["id"] = "alias"; credentials.append(alias)
        workspace["credentials"] = credentials; root["workspace"] = workspace
        XCTAssertNoThrow(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)))
        credentials[1]["algorithm"] = "ecdsa-sha2-nistp256"
        workspace["credentials"] = credentials; root["workspace"] = workspace
        XCTAssertThrowsError(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))) { error in
            XCTAssertEqual(error as? FTError, .malformed("identity.algorithm_conflict"))
        }
    }

    func testNegativeFingerprintCannotBeWeakenedByItsAlgorithmLabel() throws {
        let document = try WorkspaceDocument(data: changedWorkspace { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["revoked_keys"] = [["algorithm": "ssh-rsa", "fingerprint": "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]]
            workspace["host_trust"] = trusts
        })
        let pin = try XCTUnwrap(document.hostTrust[0].pins.first)
        XCTAssertEqual(document.hostTrust[0].evaluate(pin, locallyAccepted: true), .revoked)
        XCTAssertEqual(try document.evaluateHostKey(pin, trustID: "trust", locallyAccepted: true), .revoked)
        XCTAssertEqual(try readiness(document).issues.map(\.code), [.hostIdentityRevoked])
        XCTAssertEqual(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"],
            reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues, [.hostIdentityRevoked])
        let selection = try WorkspaceExportSelection(workspace: document, groupIDs: ["group"])
        let choices = Dictionary(uniqueKeysWithValues: selection.identities.map { ($0.fingerprint, WorkspaceExportSelection.IdentityDecision.keepReferenceOnly) })
        XCTAssertEqual(try selection.review(decisions: choices, loadedFingerprints: []).blockedRouteIDs, ["route"])
    }

    func testTrustKeyCapsIncludeRevocationsAndMissingPinsStayUnavailable() throws {
        var root = try manifestObject(), workspace = try workspaceObject(root)
        var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        trusts[0]["pins"] = []
        workspace["host_trust"] = trusts; root["workspace"] = workspace
        let missing = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        XCTAssertEqual(try readiness(missing).issues.map(\.code), [.hostTrustMissing])
        let keys: [[String: Any]] = (0..<513).map { index in
            var bytes = Data(repeating: 0, count: 32)
            bytes[0] = UInt8(index & 255); bytes[1] = UInt8(index >> 8)
            return ["algorithm": "ssh-ed25519", "fingerprint": "SHA256:" + bytes.base64EncodedString().replacingOccurrences(of: "=", with: "")]
        }
        trusts[0]["revoked_keys"] = Array(keys.prefix(512))
        workspace["host_trust"] = trusts; root["workspace"] = workspace
        XCTAssertNoThrow(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)))
        trusts[0]["revoked_keys"] = keys
        workspace["host_trust"] = trusts; root["workspace"] = workspace
        XCTAssertThrowsError(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))) { error in
            XCTAssertEqual(error as? FTError, .limit("trust.keys"))
        }
    }

    func testPublicManifestRejectsExtraSecretFieldsAtEveryCoreBoundary() throws {
        let marker = "PRIVATE-MATERIAL-MUST-NOT-PERSIST"
        for target in ["root", "source", "workspace", "groups", "machines", "routes", "credentials", "host_trust", "themes", "preferences", "pin"] {
            var root = try manifestObject(), workspace = try workspaceObject(root)
            switch target {
            case "root": root["private_key"] = marker
            case "source":
                var source = try XCTUnwrap(root["source"] as? [String: Any]); source["private_key"] = marker; root["source"] = source
            case "workspace": workspace["private_key"] = marker
            case "preferences":
                var preferences = try XCTUnwrap(workspace["preferences"] as? [String: Any]); preferences["private_key"] = marker; workspace["preferences"] = preferences
            case "pin":
                var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
                var pins = try XCTUnwrap(trusts[0]["pins"] as? [[String: Any]]); pins[0]["private_key"] = marker
                trusts[0]["pins"] = pins; workspace["host_trust"] = trusts
            default:
                var values = try XCTUnwrap(workspace[target] as? [[String: Any]]); values[0]["private_key"] = marker; workspace[target] = values
            }
            root["workspace"] = workspace
            XCTAssertThrowsError(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), target) { error in
                XCTAssertEqual(error as? FTError, .malformed("manifest.unexpected_field"))
                XCTAssertFalse(error.localizedDescription.contains(marker))
            }
        }
    }

    func testUnknownOptionalPayloadsAreDiscardedBeforeStorageAndReexport() throws {
        var root = try manifestObject(), workspace = try workspaceObject(root)
        let marker = "UNKNOWN-PRIVATE-PAYLOAD"
        root["extensions"] = ["org.example.opaque": ["private_key": marker]]
        workspace["extensions"] = ["org.example.future": ["payload": marker]]
        root["workspace"] = workspace
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        XCTAssertNil(document.raw.range(of: Data(marker.utf8)))
        XCTAssertEqual(document.diagnostics.map(\.code), ["workspace.unknown_optional_extensions_discarded"])
        XCTAssertEqual(document.diagnostics.first?.count, 2)
        XCTAssertEqual(try WorkspaceDocument(data: document.raw).groups[0].machineOrder, ["build", "gpu"])
        let archive = try AirGapArchive(workspace: document, keys: [], using: crypto)
        XCTAssertNil(try archive.encoded().range(of: Data(marker.utf8)))
        root["required_capabilities"] = ["org.example.opaque"]
        XCTAssertThrowsError(try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root)), "discarding payload does not satisfy its required capability")
    }

    func testCanonicalEndpointAndTrustScopeAgreeWithoutRewritingDisplayValues() throws {
        for (input, expected) in [("BUILD.Example.Invalid.", "build.example.invalid"),
                                  ("[2001:0DB8:0:0:0:0:0:1]", "2001:db8::1"),
                                  ("2001:db8::1", "2001:db8::1"), ("bücher.example", "xn--bcher-kva.example")] {
            XCTAssertEqual(try WorkspaceDocument.host(input), expected)
        }
        for invalid in ["host;touch", "host:22", "host/path", "user@host", "host\n", "-option", "host%20name", "[127.0.0.1]",
                        "[2001:db8::1]:22", "fe80::1%en0", "127.1", "127.000.0.1", "bad..host", "host?query", "host#fragment"] {
            XCTAssertThrowsError(try WorkspaceDocument.host(invalid), invalid)
        }
        var root = try manifestObject(), workspace = try workspaceObject(root)
        var routes = try XCTUnwrap(workspace["routes"] as? [[String: Any]])
        var trusts = try XCTUnwrap(workspace["host_trust"] as? [[String: Any]])
        routes[0]["host"] = "HOST.Example.Invalid."
        routes[0]["host_key_alias"] = "My-Host-Identity"
        trusts[0]["host_key_alias"] = "My-Host-Identity"
        workspace["routes"] = routes; workspace["host_trust"] = trusts; root["workspace"] = workspace
        let document = try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
        XCTAssertEqual(document.routes[0].displayHost, "HOST.Example.Invalid.")
        XCTAssertEqual(document.routes[0].host, "host.example.invalid")
        XCTAssertEqual(document.hostTrust[0].hostKeyAlias, "my-host-identity")
        XCTAssertEqual(document.hostTrust[0].canonicalTrustScope, "22:16:my-host-identity")
    }

    func testReissuingBundleChangesOnlyItsIdentity() throws {
        let document = try WorkspaceDocument(data: sampleManifest())
        let issued = try document.reissuingBundle(id: "new-export")
        XCTAssertEqual(issued.bundleID, "new-export")
        XCTAssertEqual(issued.id, document.id)
        XCTAssertEqual(issued.exporterID, document.exporterID)
        var original = try StrictJSON.decode(document.raw).object(), changed = try StrictJSON.decode(issued.raw).object()
        original.removeValue(forKey: "bundle_id"); changed.removeValue(forKey: "bundle_id")
        XCTAssertEqual(original, changed)
        XCTAssertThrowsError(try document.reissuingBundle(id: "bad/path"))
    }

    private func manifestObject() throws -> [String: Any] {
        try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest()) as? [String: Any])
    }
    private func workspaceObject(_ root: [String: Any]) throws -> [String: Any] {
        try XCTUnwrap(root["workspace"] as? [String: Any])
    }
    private func readiness(_ document: WorkspaceDocument, route: String = "route", available: Set<String> = ["key"]) throws -> WorkspaceDocument.RouteReadinessDetail {
        try document.readiness(for: route, availableCredentialIDs: available, acceptedTrustIDs: ["trust"],
            supportedAuthenticationMethods: [.publicKey, .keyboardInteractive, .password],
            supportedCredentialAlgorithms: ["ssh-ed25519"], supportedHostKeyAlgorithms: ["ssh-ed25519"])
    }
    private func changedWorkspace(_ change: (inout [String: Any]) -> Void) throws -> Data {
        var root = try manifestObject(), workspace = try workspaceObject(root)
        change(&workspace); root["workspace"] = workspace
        return try JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])
    }
    private func changedRoute(_ change: (inout [String: Any]) -> Void) throws -> Data {
        try changedWorkspace { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            change(&routes[0]); workspace["routes"] = routes
        }
    }
    private var qualifiedTestTransport: WorkspaceDocument.RouteSupport {
        .init(routeModes: ["direct_ssh", "frankenterm_mux"], authenticationMethods: [.publicKey, .password, .keyboardInteractive],
              credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
    }

    func testLatestNativePlanFixtureParsesWithoutRewritingItsPolicy() throws {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        let plan = try String(contentsOf: root.appendingPathComponent("COMPREHENSIVE_PLAN_FOR_NATIVE_IOS_FRANKENTERM_APP.md"), encoding: .utf8)
        let section = try XCTUnwrap(plan.range(of: "### 9.1 Version-one format"))
        let start = try XCTUnwrap(plan.range(of: "```json\n", range: section.upperBound..<plan.endIndex))
        let end = try XCTUnwrap(plan.range(of: "\n```", range: start.upperBound..<plan.endIndex))
        let bytes = Data(plan[start.upperBound..<end.lowerBound].utf8)
        let document = try WorkspaceDocument(data: bytes)
        XCTAssertEqual(document.raw, bytes)
        XCTAssertEqual(document.routes[0].credentialIDs, ["credential-main"])
        XCTAssertEqual(document.routes[0].authenticationMethods, [.publicKey])
        XCTAssertEqual(document.routes[0].sessionCreation, "on_select")
        XCTAssertEqual(document.routes[1].attachPolicy, "existing_only")
        XCTAssertEqual(document.hostTrust.count, 2)
        XCTAssertEqual(document.groups[0].machineOrder, ["machine-build", "machine-gpu"])
        XCTAssertEqual(document.themes[1].cursorBorder, "#FFFFFF")
        XCTAssertTrue(document.groups[0].autoConnect)
    }

    func testOrderedAuthenticationTriesOnlyPermittedAvailableCandidates() throws {
        let data = try changedWorkspace { workspace in
            var keys = workspace["credentials"] as! [[String: Any]]
            var other = keys[0]; other["id"] = "other"
            other["public_fingerprint"] = "SHA256:ERERERERERERERERERERERERERERERERERERERERERE"
            keys.append(other); workspace["credentials"] = keys
            var routes = workspace["routes"] as! [[String: Any]]
            routes[0]["credential_refs"] = ["other", "key"]
            routes[0]["authentication_methods"] = ["publickey", "keyboard-interactive"]
            workspace["routes"] = routes
        }
        let document = try WorkspaceDocument(data: data)
        let ready = try document.readiness(forRouteID: "route", availableCredentialIDs: ["key", "unlisted-key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport)
        XCTAssertEqual(ready.usableCredentialIDs, ["key"]); XCTAssertTrue(ready.isLocallyReady)
        let revoked = try document.readiness(forRouteID: "route", availableCredentialIDs: ["key", "other"], reviewedTrustIDs: ["trust"],
            revokedCredentialFingerprints: ["SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"], support: qualifiedTestTransport)
        XCTAssertEqual(revoked.usableCredentialIDs, ["other"])
        let password = try WorkspaceDocument(data: changedRoute { $0["credential_refs"] = []; $0["authentication_methods"] = ["password"] })
        XCTAssertTrue(try password.readiness(forRouteID: "route", availableCredentialIDs: [], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).isLocallyReady)
        let publicKeyOnly = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey], credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
        XCTAssertEqual(try password.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: publicKeyOnly).issues, [.authenticationUnavailable])
        XCTAssertEqual(try WorkspaceDocument(data: sampleManifest()).readiness(forRouteID: "route", availableCredentialIDs: [], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues, [.credentialUnavailable])
    }

    func testAuthenticationCannotDefaultOrIgnoreUnknownAndDuplicatePolicy() throws {
        let changes: [(inout [String: Any]) -> Void] = [
            { $0.removeValue(forKey: "authentication_methods") }, { $0["authentication_methods"] = [] },
            { $0["authentication_methods"] = ["publickey", "publickey"] }, { $0["authentication_methods"] = ["publickey", "unknown"] },
            { $0["credential_refs"] = ["key", "key"] }, { $0["credential_refs"] = ["missing"] },
            { $0["credential_ref"] = "key" }, { $0["user"] = "" }, { $0["user"] = "operator\ncommand" }, { $0["user"] = "two users" }
        ]
        for change in changes { XCTAssertThrowsError(try WorkspaceDocument(data: changedRoute(change))) }
        let offline = try WorkspaceDocument(data: sampleManifest()).readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"])
        XCTAssertTrue(offline.issues.contains(.transportUnavailable)); XCTAssertFalse(offline.isLocallyReady)
        XCTAssertThrowsError(try WorkspaceDocument(data: changedWorkspace { workspace in
            var groups = workspace["groups"] as! [[String: Any]], preferences = workspace["preferences"] as! [String: Any]
            groups[0]["id"] = "build"; preferences["initial_group_ref"] = "build"
            workspace["groups"] = groups; workspace["preferences"] = preferences
        }))
    }

    func testNegativeHostTrustWinsBeforePinReviewOrUnsupportedPolicy() throws {
        let pin = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA")
        let other = try WorkspaceDocument.HostKey(algorithm: "ssh-ed25519", fingerprint: "SHA256:ERERERERERERERERERERERERERERERERERERERERERE")
        let document = try WorkspaceDocument(data: changedWorkspace { workspace in
            var trust = workspace["host_trust"] as! [[String: Any]]
            trust[0]["revoked_keys"] = trust[0]["pins"]; trust[0]["required_capabilities"] = ["required.krl"]
            workspace["host_trust"] = trust
        })
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: pin, reviewedTrustIDs: []), .revoked)
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: other, certificateAuthority: pin, reviewedTrustIDs: ["trust"], support: qualifiedTestTransport), .revoked)
        XCTAssertTrue(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues.contains(.hostIdentityRevoked))
        let ordinary = try WorkspaceDocument(data: sampleManifest())
        XCTAssertEqual(try ordinary.verifyHostKey(forRouteID: "route", presented: pin, reviewedTrustIDs: [], support: qualifiedTestTransport), .reviewRequired)
        XCTAssertEqual(try ordinary.verifyHostKey(forRouteID: "route", presented: pin, reviewedTrustIDs: ["trust"], support: qualifiedTestTransport), .trustedPin)
        XCTAssertEqual(try ordinary.verifyHostKey(forRouteID: "route", presented: other, reviewedTrustIDs: ["trust"], support: qualifiedTestTransport), .pinMismatch)
        XCTAssertEqual(try ordinary.verifyHostKey(forRouteID: "route", presented: pin, reviewedTrustIDs: ["trust"], localRevokedFingerprints: [pin.fingerprint], support: qualifiedTestTransport), .revoked)
        XCTAssertEqual(try ordinary.verifyHostKey(forRouteID: "route", presented: pin, reviewedTrustIDs: ["trust"], locallyRejectedTrustIDs: ["trust"], support: qualifiedTestTransport), .locallyRejected)
        XCTAssertEqual(try ordinary.verifyHostKey(forRouteID: "route", presented: pin, certificateAuthority: other, reviewedTrustIDs: ["trust"], support: qualifiedTestTransport), .unsupportedCertificate)
    }

    func testIncompleteTrustAndUnsupportedRouteRemainVisibleWithoutDowngrade() throws {
        let document = try WorkspaceDocument(data: changedWorkspace { workspace in
            var routes = workspace["routes"] as! [[String: Any]], blocked = routes[0]
            blocked["id"] = "blocked"; blocked["required_capabilities"] = ["ssh.revocation_krl"]
            routes.append(blocked); workspace["routes"] = routes
        })
        XCTAssertTrue(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).isLocallyReady)
        XCTAssertEqual(try document.readiness(forRouteID: "blocked", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues, [.unsupportedRequirement])
        let pin = try XCTUnwrap(document.hostTrust.first?.pins.first)
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "blocked", presented: pin, reviewedTrustIDs: ["trust"], support: qualifiedTestTransport), .unsupportedPolicy)
        let noPins = try WorkspaceDocument(data: changedWorkspace { workspace in
            var trust = workspace["host_trust"] as! [[String: Any]]; trust[0]["pins"] = []; workspace["host_trust"] = trust
        })
        XCTAssertEqual(try noPins.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues, [.hostTrustMissing])
    }

    func testJumpReadinessUsesTransportWithoutExecutingHopSessionPreference() throws {
        let document = try WorkspaceDocument(data: changedWorkspace { workspace in
            var routes = workspace["routes"] as! [[String: Any]], hop = routes[0]
            hop["id"] = "jump"; hop["mode"] = "frankenterm_mux"; hop.removeValue(forKey: "session_creation")
            routes[0]["jump_routes"] = ["jump"]; routes.append(hop); workspace["routes"] = routes
        })
        XCTAssertEqual(try document.readiness(forRouteID: "jump", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues, [.sessionPolicyMissing])
        XCTAssertTrue(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).isLocallyReady)
        XCTAssertTrue(try document.readiness(forRouteID: "route", availableCredentialIDs: [], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues.contains(.blockedJumpRoute("jump")))
    }

    func testUnknownExtensionsCannotSmuggleSecretsOrActivateRequirements() throws {
        let secret = "synthetic-private-container-not-for-catalog"
        let optional = try WorkspaceDocument(data: changedRoute { $0["extensions"] = ["org.example.optional": ["private_key": secret]] })
        XCTAssertEqual(optional.discardedExtensionCount, 1)
        XCTAssertNil(optional.raw.range(of: Data(secret.utf8)))
        XCTAssertNil(optional.raw.range(of: Data("org.example.optional".utf8)))
        XCTAssertEqual(try WorkspaceDocument(data: optional.raw).discardedExtensionCount, 0)
        let required = try WorkspaceDocument(data: changedRoute { $0["extensions"] = ["org.example.required": ["required": true, "private_key": secret]] })
        XCTAssertNil(required.raw.range(of: Data(secret.utf8)))
        XCTAssertEqual(required.routes[0].requiredExtensions, ["org.example.required"])
        XCTAssertEqual(try WorkspaceDocument(data: required.raw).routes[0].requiredExtensions, ["org.example.required"])
        XCTAssertTrue(try required.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues.contains(.unsupportedRequirement))
        XCTAssertThrowsError(try WorkspaceDocument(data: changedWorkspace { $0["extensions"] = ["org.example.required": ["required": true]] }))
        XCTAssertNil(try AirGapArchive(workspace: optional, keys: [], using: crypto).encoded().range(of: Data(secret.utf8)))
    }

    func testThemePreservesCompletePaletteAndBoundedPortableFont() throws {
        let document = try WorkspaceDocument(data: changedWorkspace { workspace in
            var themes = workspace["themes"] as! [[String: Any]]
            themes[0]["cursor_background"] = "#aabbcc80"; themes[0]["selection_background"] = "#010203"
            themes[0]["indexed"] = ["16": "#123abc", "255": "#abcdef"]
            themes[0]["font"] = ["family": "Example Mono", "face": "Regular", "point_size": 15.5, "fallbacks": ["Example CJK"], "variations": ["wght": 450]]
            workspace["themes"] = themes
        })
        let theme = document.themes[0]
        XCTAssertEqual(theme.cursorBackground, "#AABBCC80"); XCTAssertEqual(theme.selectionBackground, "#010203")
        XCTAssertEqual(theme.indexedColor(16), "#123ABC"); XCTAssertEqual(theme.indexedColor(255), "#ABCDEF")
        XCTAssertEqual(theme.indexedColor(17), "#00005F"); XCTAssertEqual(theme.indexedColor(231), "#FFFFFF")
        XCTAssertEqual(theme.indexedColor(232), "#080808"); XCTAssertEqual(theme.indexedColor(254), "#E4E4E4")
        XCTAssertEqual(theme.font?.family, "Example Mono"); XCTAssertEqual(theme.font?.pointSize, 15.5)
        XCTAssertEqual(theme.font?.variations["wght"], 450)
        for invalid in ["0", "15", "016", "256", "-1"] {
            XCTAssertThrowsError(try WorkspaceDocument(data: changedWorkspace { workspace in
                var themes = workspace["themes"] as! [[String: Any]]; themes[0]["indexed"] = [invalid: "#FFFFFF"]; workspace["themes"] = themes
            }))
        }
        XCTAssertThrowsError(try WorkspaceDocument(data: changedWorkspace { workspace in
            var themes = workspace["themes"] as! [[String: Any]]
            themes[0]["font"] = ["family": "Example Mono", "private_key": "secret"]; workspace["themes"] = themes
        }))
    }

    func testCombinedHostTrustBudgetAndStrictEncoder() throws {
        XCTAssertThrowsError(try StrictJSON.encode(.number("0,1")))
        XCTAssertThrowsError(try StrictJSON.encode(.number("null")))
        XCTAssertThrowsError(try StrictJSON.encode(.object(["text": .string(String(repeating: "x", count: FTLimit.text + 1))])))
        XCTAssertEqual(String(decoding: try StrictJSON.encode(.object(["z": .number("1e2"), "a": .array([.boolean(true), .null])])), as: UTF8.self), #"{"a":[true,null],"z":1e2}"#)
    }

    func testRetainedTrustRequirementsBlockBothReadinessAndLiveVerification() throws {
        let document = try WorkspaceDocument(data: sampleManifest())
        let trust = document.hostTrust[0], pin = try XCTUnwrap(trust.pins.first)
        let retained: [String: Set<String>] = [trust.canonicalTrustScope: ["mandatory.krl"]]
        XCTAssertEqual(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport,
            requiredHostCapabilitiesByTrustScope: retained).issues, [.unsupportedRequirement])
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: pin, reviewedTrustIDs: ["trust"], support: qualifiedTestTransport,
            requiredHostCapabilitiesByTrustScope: retained), .unsupportedPolicy)
        XCTAssertEqual(try document.evaluateHostKey(pin, trustID: "trust", locallyAccepted: true, requiredHostCapabilitiesByTrustScope: retained), .requiredPolicyUnavailable)
        let requirement = "extension:org.example.mandatory"
        let claimsExtension = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], capabilities: [requirement])
        XCTAssertEqual(try document.verifyHostKey(forRouteID: "route", presented: pin, reviewedTrustIDs: ["trust"], support: claimsExtension,
            requiredHostCapabilitiesByTrustScope: [trust.canonicalTrustScope: [requirement]]), .unsupportedPolicy)
        let marked = try WorkspaceDocument(data: changedWorkspace { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            trusts[0]["extensions"] = ["org.example.mandatory": ["required": true, "private_key": "secret"]]
            workspace["host_trust"] = trusts
        })
        XCTAssertEqual(marked.hostTrust[0].requiredPolicyRequirements, [requirement])
        XCTAssertNil(marked.raw.range(of: Data("secret".utf8)))
    }
    func testCodesMatchCommittedBoundaryVectors() throws {
        for (hex, code) in [
            ("00000000000000000000000000000000", "0000-0000-0000-0000-0000-0000-00PR"),
            ("00000000000000000000000000000001", "0000-0000-0000-0000-0000-0000-013Y"),
            ("80000000000000000000000000000000", "4000-0000-0000-0000-0000-0000-001E"),
            ("ffffffffffffffffffffffffffffffff", "7ZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZZZ-ZZH7"),
            ("000102030405060708090a0b0c0d0e0f", "0004-1061-050R-3GG2-8A1C-60T3-GFCM")] {
            let secret = try Data(ftHex: hex), value = try TransferCode(secret: secret)
            XCTAssertEqual(value.reveal(using: crypto), code)
            XCTAssertEqual(try TransferCode(entered: code.lowercased(), using: crypto).secret, secret)
            XCTAssertEqual(try TransferCode(entered: code.replacingOccurrences(of: "0", with: "O").replacingOccurrences(of: "1", with: "l"), using: crypto).secret, secret)
        }
        for invalid in ["", "8000-0000-0000-0000-0000-0000-00PR", "0000-0000-0000-0000-0000-0000-00PS", "0000\t0000-0000-0000-0000-0000-0000-00PR"] {
            XCTAssertThrowsError(try TransferCode(entered: invalid, using: crypto))
        }
    }
    func testBothPublishedEnvelopeVectorsAndAllSingleByteMutations() throws {
        // Exact constants from fixtures/ios-airgap/ftag-v1-code-vectors.json.
        let raw = try Data(ftHex: "8501427b7d808080")
        let compressedArchive = try Data(ftHex: "850159010e7b2266697874757265223a22") + Data(repeating: 65, count: 256) + Data(ftHex: "227d808080")
        for (archive, start, expected, tag, codeText) in [
            (raw, UInt8(0), "67479d4af8f3ee221300eb16adc5ad2442eadaeccc562004c733c6247072fd34", "df289a74d156fd13bd3750fcd9bc15cf", "0004-1061-050R-3GG2-8A1C-60T3-GFCM"),
            (compressedArchive, UInt8(64), "7666f3a5e35309598f07201154f70821a4c7a6be55a4104414c0218901131bf9", "e26ce37ad28972e37051c778fe015993", "2085-146H-258S-3MGJ-AA9D-64TK-JFAJ")
        ] {
            let code = try TransferCode(secret: Data(start..<start + 16))
            let envelope = try FTAG.sealArchiveBytes(archive, code: code, transferID: Data(start + 48..<start + 64),
                                                    salt: Data(start + 16..<start + 32), nonce: Data(start + 32..<start + 44), using: crypto)
            XCTAssertEqual(code.reveal(using: crypto), codeText)
            XCTAssertEqual(envelope.count, 4170)
            XCTAssertEqual(crypto.sha256(envelope).ftHex.lowercased(), expected)
            XCTAssertEqual(envelope.suffix(16).ftHex.lowercased(), tag)
            XCTAssertEqual(try FTAG.openArchiveBytes(envelope, code: code, using: crypto), archive)
            XCTAssertThrowsError(try FTAG.open(envelope, code: code, using: crypto), "fixture is intentionally not a workspace")
            for i in envelope.indices {
                var bad = envelope; bad[i] ^= 1
                XCTAssertThrowsError(try FTAG.openArchiveBytes(bad, code: code, using: crypto), "byte \(i)")
            }
            for bad in [Data(envelope.dropLast()), Data(envelope.prefix(29)), envelope + Data([0])] {
                XCTAssertThrowsError(try FTAG.openArchiveBytes(bad, code: code, using: crypto))
            }
            XCTAssertThrowsError(try FTAG.openArchiveBytes(envelope, code: TransferCode(secret: Data(repeating: 255, count: 16)), using: crypto))
        }
    }
    func testPackingRejectsTrailingStreamsDictionaryAndPadding() throws {
        let data = Data(repeating: 65, count: 1000)
        let compressed = try ZlibPacking.compress(data)
        XCTAssertEqual(try ZlibPacking.expand(compressed, expected: data.count), data)
        XCTAssertThrowsError(try ZlibPacking.expand(compressed + compressed, expected: data.count))
        XCTAssertThrowsError(try ZlibPacking.expand(compressed + Data([0]), expected: data.count))
        XCTAssertThrowsError(try ZlibPacking.expand(compressed, expected: 999))
        XCTAssertThrowsError(try ZlibPacking.expand(compressed, expected: 1001))
        var packed = try FTAG.pack(data)
        packed[packed.count - 1] = 1
        XCTAssertThrowsError(try FTAG.unpackAuthenticated(packed))
        XCTAssertThrowsError(try FTAG.unpackAuthenticated(FTAG.pack(data) + Data(repeating: 0, count: 4096)))
        XCTAssertThrowsError(try FTAG.pack(Data(repeating: 3, count: FTLimit.envelope), compress: false))
    }
    func testRestrictedCBORRejectsNoncanonicalAndTrailingObjects() throws {
        var noncanonical = CBORReader(try Data(ftHex: "1801"))
        XCTAssertThrowsError(try noncanonical.head(major: 0))
        var indefinite = CBORReader(try Data(ftHex: "9f01ff"))
        XCTAssertThrowsError(try indefinite.array(cap: 10))
        var truncated = CBORReader(try Data(ftHex: "5a7fffffff"))
        XCTAssertThrowsError(try truncated.bytes(cap: FTLimit.archive))
        let archive = try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [], using: crypto)
        XCTAssertThrowsError(try AirGapArchive(data: archive.encoded() + Data([0]), using: crypto))
    }
    func testRealSyntheticKeyAndWorkspaceRoundtrip() throws {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let workspace = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint))
        let archive = try AirGapArchive(workspace: workspace, keys: [key], using: crypto)
        let export = try FTAG.seal(archive, using: crypto)
        let decoded = try FTAG.open(export.envelope, code: export.code, using: crypto)
        XCTAssertEqual(decoded.workspace.raw, workspace.raw)
        XCTAssertEqual(decoded.keys[0].container, key.container)
        XCTAssertFalse(export.envelope.range(of: key.container) != nil)
        XCTAssertFalse(String(reflecting: export).contains(export.code.reveal(using: crypto)))
        let pem = Data(("-----BEGIN OPENSSH PRIVATE KEY-----\n" + key.container.base64EncodedString() + "\n-----END OPENSSH PRIVATE KEY-----\n").utf8)
        XCTAssertEqual(try PrivateKeyRecord.fromPEM(pem, credentialID: "key", using: crypto).publicBlob, key.publicBlob)
    }
    func testKeyBindingCannotBeForgedOrDuplicated() throws {
        let (key, fingerprint) = try sampleKey(using: crypto)
        let workspace = try WorkspaceDocument(data: sampleManifest(fingerprint: fingerprint))
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [key, key], using: crypto))
        XCTAssertThrowsError(try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest()), keys: [key], using: crypto))
        let fake = PrivateKeyRecord(credentialID: "key", container: key.container, publicBlob: Data(repeating: 0, count: 51))
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [fake], using: crypto))
        var corrupted = key.container
        corrupted[corrupted.count - 16] ^= 1
        XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(corrupted, using: crypto))
        let unknown = PrivateKeyRecord(credentialID: "another", container: key.container, publicBlob: key.publicBlob)
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace, keys: [unknown], using: crypto))
    }
}
