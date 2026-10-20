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
        "routes": [["id": "route", "mode": "direct_ssh", "host": "host.example.invalid", "port": 22, "user": "operator", "credential_ref": "key", "host_trust_ref": "trust", "jump_routes": []]],
        "credentials": [["id": "key", "algorithm": "ssh-ed25519", "public_fingerprint": fingerprint, "private_material": "not_included"]],
        "host_trust": [["id": "trust", "host": "host.example.invalid", "port": 22, "pins": [["algorithm": "ssh-ed25519", "fingerprint": "SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"]]]],
        "themes": [theme], "preferences": ["initial_machine_ref": "build", "initial_group_ref": "group", "connect_on_open": true]
    ]
    return try JSONSerialization.data(withJSONObject: ["format": "ftworkspace", "version": 1, "bundle_id": "bundle", "source": ["exporter_id": "mac"], "required_capabilities": [], "workspace": workspace], options: [.sortedKeys])
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
