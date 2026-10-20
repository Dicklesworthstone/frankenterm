import Foundation
import XCTest
@testable import FrankenTermKit

func publicBlob(_ raw: Data) -> Data {
    var result = Data()
    for value in [Data("ssh-ed25519".utf8), raw] { result.appendBE(UInt64(value.count), bytes: 4); result += value }
    return result
}

func sampleManifest(fingerprint: String = "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA") throws -> Data {
    let palette = Array(repeating: "#123456", count: 16)
    let theme: [String: Any] = ["id": "theme", "name": "Cobalt", "foreground": "#FFFFFF", "background": "#10223C", "accent": "#3B82F6", "ansi": palette,
                                "cursor_foreground": "#000000", "cursor_background": "#FFFFFF", "cursor_border": "#FFFFFF", "selection_foreground": "#FFFFFF", "selection_background": "#345D91"]
    let workspace: [String: Any] = [
        "id": "work", "name": "Machines", "groups": [["id": "group", "name": "Agents", "machine_order": ["build", "gpu"]]],
        "machines": ["build", "gpu"].map { ["id": $0, "name": $0, "theme_ref": "theme", "route_refs": ["route"], "preferred_route_ref": "route", "auto_connect": true] as [String: Any] },
        "routes": [["id": "route", "mode": "direct_ssh", "host": "host.example.invalid", "port": 22, "user": "operator", "credential_refs": ["key"],
                    "authentication_methods": ["publickey"], "host_trust_ref": "trust", "jump_routes": [], "session_creation": "on_select"]],
        "credentials": [["id": "key", "algorithm": "ssh-ed25519", "public_fingerprint": fingerprint, "private_material": "not_included"]],
        "host_trust": [["id": "trust", "host": "host.example.invalid", "port": 22, "revoked_keys": [],
                        "pins": [["algorithm": "ssh-ed25519", "fingerprint": "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]]]],
        "themes": [theme], "preferences": ["initial_machine_ref": "build", "initial_group_ref": "group", "connect_on_open": true]
    ]
    return try JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1, "bundle_id": "bundle", "source": ["exporter_id": "mac"], "required_capabilities": [], "workspace": workspace], options: [.sortedKeys])
}

private func changedWorkspace(_ change: (inout [String: Any]) -> Void) throws -> Data {
    var root = try JSONSerialization.jsonObject(with: sampleManifest()) as! [String: Any]
    var workspace = root["workspace"] as! [String: Any]
    change(&workspace)
    root["workspace"] = workspace
    return try JSONSerialization.data(withJSONObject: root, options: [.sortedKeys])
}

private func changedRoute(_ change: (inout [String: Any]) -> Void) throws -> Data {
    try changedWorkspace { workspace in
        var routes = workspace["routes"] as! [[String: Any]]
        change(&routes[0]); workspace["routes"] = routes
    }
}

private let qualifiedTestTransport = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh", "frankenterm_mux"],
    authenticationMethods: [.publicKey, .password, .keyboardInteractive], credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])

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
    func testLatestNativePlanFixtureParsesWithoutRewritingItsPolicy() throws {
        // This fixture is independent of the encoder and was supplied by the
        // product plan. A private older singular credential_ref schema cannot
        // silently replace the actual format the Mac and phone must share.
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
        XCTAssertEqual(document.routes[0].credentialIDs, ["other", "key"])
        XCTAssertEqual(document.routes[0].authenticationMethods, [.publicKey, .keyboardInteractive])
        let readiness = try document.readiness(forRouteID: "route", availableCredentialIDs: ["key", "unlisted-key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport)
        XCTAssertEqual(readiness.usableCredentialIDs, ["key"])
        XCTAssertTrue(readiness.isLocallyReady)
        let revoked = try document.readiness(forRouteID: "route", availableCredentialIDs: ["key", "other"], reviewedTrustIDs: ["trust"],
            revokedCredentialFingerprints: ["SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"], support: qualifiedTestTransport)
        XCTAssertEqual(revoked.usableCredentialIDs, ["other"])
        let password = try WorkspaceDocument(data: changedRoute { $0["credential_refs"] = []; $0["authentication_methods"] = ["password"] })
        XCTAssertTrue(try password.readiness(forRouteID: "route", availableCredentialIDs: [], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).isLocallyReady)
        let publicKeyOnly = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey], credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"])
        XCTAssertEqual(try password.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: publicKeyOnly).issues, [.authenticationUnavailable])
        let noKeys = try WorkspaceDocument(data: sampleManifest()).readiness(forRouteID: "route", availableCredentialIDs: [], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport)
        XCTAssertEqual(noKeys.issues, [.credentialUnavailable])
    }
    func testAuthenticationCannotDefaultOrIgnoreUnknownAndDuplicatePolicy() throws {
        let changes: [(inout [String: Any]) -> Void] = [
            { $0.removeValue(forKey: "authentication_methods") }, { $0["authentication_methods"] = [] },
            { $0["authentication_methods"] = ["publickey", "publickey"] }, { $0["authentication_methods"] = ["publickey", "unknown"] },
            { $0["credential_refs"] = ["key", "key"] }, { $0["credential_refs"] = ["missing"] },
            { $0["credential_ref"] = "key" }, { $0["user"] = "" }, { $0["user"] = "operator\ncommand" }
        ]
        for change in changes { XCTAssertThrowsError(try WorkspaceDocument(data: changedRoute(change))) }
        let offline = try WorkspaceDocument(data: sampleManifest()).readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"])
        XCTAssertTrue(offline.issues.contains(.transportUnavailable))
        XCTAssertFalse(offline.isLocallyReady)
        XCTAssertThrowsError(try WorkspaceDocument(data: changedWorkspace { workspace in
            var keys = workspace["credentials"] as! [[String: Any]], alias = keys[0]
            alias["id"] = "alias"; alias["algorithm"] = "ssh-rsa"; keys.append(alias); workspace["credentials"] = keys
        }))
        XCTAssertThrowsError(try WorkspaceDocument(data: changedWorkspace { workspace in
            // A group's ID must not ambiguously refer to a machine as well.
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
            trust[0]["revoked_keys"] = trust[0]["pins"]
            trust[0]["required_capabilities"] = ["required.krl"]
            workspace["host_trust"] = trust
        })
        XCTAssertEqual(document.hostTrust[0].revokedKeys, [pin])
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
        let data = try changedWorkspace { workspace in
            var routes = workspace["routes"] as! [[String: Any]]
            var blocked = routes[0]; blocked["id"] = "blocked"; blocked["required_capabilities"] = ["ssh.revocation_krl"]
            routes.append(blocked); workspace["routes"] = routes
        }
        let document = try WorkspaceDocument(data: data)
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
            var routes = workspace["routes"] as! [[String: Any]]
            var hop = routes[0]; hop["id"] = "jump"; hop["mode"] = "frankenterm_mux"; hop.removeValue(forKey: "session_creation")
            routes[0]["jump_routes"] = ["jump"]; routes.append(hop); workspace["routes"] = routes
        })
        XCTAssertEqual(try document.readiness(forRouteID: "jump", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).issues, [.sessionPolicyMissing])
        XCTAssertTrue(try document.readiness(forRouteID: "route", availableCredentialIDs: ["key"], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport).isLocallyReady)
        let blocked = try document.readiness(forRouteID: "route", availableCredentialIDs: [], reviewedTrustIDs: ["trust"], support: qualifiedTestTransport)
        XCTAssertTrue(blocked.issues.contains(.blockedJumpRoute("jump")))
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
        let archive = try AirGapArchive(workspace: optional, keys: [], using: crypto)
        XCTAssertNil(try archive.encoded().range(of: Data(secret.utf8)))
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
    func testCombinedHostTrustBudgetAndStrictEncoder() throws {
        let data = try changedWorkspace { workspace in
            var trusts = workspace["host_trust"] as! [[String: Any]]
            let keys = (0..<513).map { index -> [String: String] in
                var bytes = Data(repeating: 0, count: 32); bytes[30] = UInt8(index / 256); bytes[31] = UInt8(index % 256)
                return ["algorithm": "ssh-ed25519", "fingerprint": "SHA256:" + bytes.base64EncodedString().replacingOccurrences(of: "=", with: "")]
            }
            trusts[0]["pins"] = Array(keys.prefix(256)); trusts[0]["revoked_keys"] = Array(keys.dropFirst(256)); workspace["host_trust"] = trusts
        }
        XCTAssertThrowsError(try WorkspaceDocument(data: data))
        XCTAssertThrowsError(try StrictJSON.encode(.number("0,1")))
        XCTAssertThrowsError(try StrictJSON.encode(.number("null")))
        XCTAssertThrowsError(try StrictJSON.encode(.object(["text": .string(String(repeating: "x", count: FTLimit.text + 1))])))
        XCTAssertEqual(String(decoding: try StrictJSON.encode(.object(["z": .number("1e2"), "a": .array([.boolean(true), .null])])), as: UTF8.self), #"{"a":[true,null],"z":1e2}"#)
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
