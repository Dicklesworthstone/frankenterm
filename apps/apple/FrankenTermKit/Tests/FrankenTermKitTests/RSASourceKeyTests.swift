import Foundation
import XCTest
@testable import FrankenTermKit

/// These keys are PUBLIC TEST MATERIAL. OpenSSH generated the private keys and
/// encrypted containers; OpenSSL independently generated the SHA-2 signatures.
/// Shared with signer tests so no production encoder becomes a test oracle.
struct RSAFixtures: Decodable, Sendable {
    struct Key: Decodable, Sendable {
        let id: String, bits: Int, algorithm: String, cipher: String, rounds: UInt32
        let publicKey: String, fingerprint: String
        let clearPEM: String, encryptedPEM: String, pkcs1PEM: String, pkcs8PEM: String
        let signatures: [String: String], signedData: [String: String]
        var publicBlob: Data { Data(base64Encoded: publicKey.split(separator: " ")[1].description)! }
        var clearForms: [String] { [clearPEM, pkcs1PEM, pkcs8PEM] }
    }
    let passphrase: String, sessionIdentifierHex: String, user: String, keys: [Key]

    static func load() throws -> Self {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Self.self,
            from: Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/rsa-source-keys.json")))
    }
}

@MainActor
final class RSASourceKeyTests: XCTestCase {
    private let crypto = TestCryptography()

    func testIndependentOpenSSHAndLegacyFormsPreserveCompleteRSAIdentityAtEverySize() async throws {
        let fixtures = try RSAFixtures.load()
        XCTAssertEqual(fixtures.keys.map(\.bits), [2048, 4096, 8192])
        for fixture in fixtures.keys {
            let independent = try PrivateKeyRecord.fromPEM(Data(fixture.clearPEM.utf8), credentialID: "key", using: crypto)
            let expectedFields = try privateFields(independent.container)
            XCTAssertEqual(expectedFields.count, 6)
            for pem in fixture.clearForms {
                let source = try OpenSSHSourceKey(pem: Data(pem.utf8), using: crypto)
                XCTAssertEqual(source.algorithm, "ssh-rsa", fixture.id)
                XCTAssertEqual(source.publicBlob, fixture.publicBlob, fixture.id)
                XCTAssertEqual(source.fingerprint(using: crypto), fixture.fingerprint, fixture.id)
                XCTAssertFalse(source.isEncrypted)
                XCTAssertNil(source.bcryptRounds)
                let record = try source.unlock(credentialID: "key", using: crypto)
                XCTAssertEqual(try privateFields(record.container), expectedFields, fixture.id)
                XCTAssertEqual(try PrivateKeyRecord.validateContainer(record.container, using: crypto), fixture.publicBlob)
                let direct = try PrivateKeyRecord.fromPEM(Data(pem.utf8), credentialID: "key", using: crypto)
                XCTAssertEqual(try privateFields(direct.container), expectedFields, fixture.id)
            }
            let encrypted = try OpenSSHSourceKey(pem: Data(fixture.encryptedPEM.utf8))
            XCTAssertTrue(encrypted.isEncrypted)
            XCTAssertEqual(encrypted.bcryptRounds, fixture.rounds)
            XCTAssertEqual(encrypted.publicBlob, fixture.publicBlob)
            let unlocked = try encrypted.unlock(passphrase: Data(fixtures.passphrase.utf8), credentialID: "key", using: crypto)
            XCTAssertEqual(try privateFields(unlocked.container), expectedFields, fixture.id)
            XCTAssertEqual(try PrivateKeyRecord.validateContainer(unlocked.container, using: crypto), fixture.publicBlob)
        }
    }

    func testEncryptedRSAIdentityFlowsThroughPickerAndAuthenticatedArchive() async throws {
        let fixtures = try RSAFixtures.load()
        for fixture in fixtures.keys {
            let document = try workspace(fixture)
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            guard case .passphraseRequired(let request) = try picker.select(
                pem: Data(fixture.encryptedPEM.utf8), reference: document.credentials[0]) else {
                return XCTFail("An encrypted source must require its own passphrase.")
            }
            XCTAssertEqual(request.fingerprint, fixture.fingerprint)
            let record = try picker.unlock(requestID: request.id, passphrase: Data(fixtures.passphrase.utf8))
            XCTAssertNil(picker.request)
            let archive = try AirGapArchive(workspace: document, keys: [record], using: crypto)
            let sealed = try FTAG.seal(archive, using: crypto)
            let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
            XCTAssertEqual(restored.workspace.raw, document.raw)
            XCTAssertEqual(restored.keys.count, 1)
            XCTAssertEqual(restored.keys[0].container, record.container)
            XCTAssertEqual(restored.keys[0].publicBlob, fixture.publicBlob)
            XCTAssertEqual(try SSHUserAuthenticationSigner.publicIdentity(privateContainer: restored.keys[0].container).fingerprint,
                           fixture.fingerprint)
        }
    }

    func testWrongPassphraseAndChangedEncryptedIdentityFailWithoutImportingAKey() async throws {
        let fixtures = try RSAFixtures.load()
        for fixture in fixtures.keys {
            let source = try OpenSSHSourceKey(pem: Data(fixture.encryptedPEM.utf8))
            XCTAssertThrowsError(try source.unlock(passphrase: Data("wrong public test passphrase".utf8), credentialID: "key", using: crypto)) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey)
            }
            var wire = try RSAWire(pem: fixture.encryptedPEM)
            wire.publicBlob[wire.publicBlob.count - 2] ^= 1
            let changed = try OpenSSHSourceKey(pem: wire.pem())
            XCTAssertThrowsError(try changed.unlock(passphrase: Data(fixtures.passphrase.utf8), credentialID: "key", using: crypto)) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey)
            }
            let other = try XCTUnwrap(fixtures.keys.first { $0.bits != fixture.bits })
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            XCTAssertThrowsError(try picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: workspace(other).credentials[0])) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, .identityMismatch)
            }
            XCTAssertNil(picker.request)
        }
    }

    func testEveryOpenSSHPrivateComponentIsValidatedIncludingTheStoredCRTCoefficient() async throws {
        let fixture = try RSAFixtures.load().keys[0]
        let original = try RSAWire(pem: fixture.clearPEM)
        let fields = try privateFields(original.container())
        for index in fields.indices {
            var changed = fields
            changed[index][changed[index].count - 1] ^= 2
            var wire = original
            wire.privatePart = privatePart(fields: changed)
            XCTAssertThrowsError(try PrivateKeyRecord.fromPEM(wire.pem(), credentialID: "key", using: crypto), "private component \(index)")
        }
        for index in fields.indices {
            for invalid in [Data(), Data([0]), Data([0]) + fields[index], Data([0x80]), Data(repeating: 1, count: 1026)] {
                var changed = fields; changed[index] = invalid
                var wire = original; wire.privatePart = privatePart(fields: changed)
                XCTAssertThrowsError(try PrivateKeyRecord.fromPEM(wire.pem(), credentialID: "key", using: crypto), "canonical component \(index)")
            }
        }
        // A private key from another size must never inherit the public header.
        var wrongHeader = try RSAWire(pem: RSAFixtures.load().keys[1].clearPEM)
        wrongHeader.publicBlob = original.publicBlob
        XCTAssertThrowsError(try PrivateKeyRecord.fromPEM(wrongHeader.pem(), credentialID: "key", using: crypto))
    }

    func testRSAProfileAndCanonicalPublicIntegersAreRejectedBeforeAPassphrasePrompt() async throws {
        let fixture = try RSAFixtures.load().keys[0]
        let original = try RSAWire(pem: fixture.encryptedPEM)
        var reader = ByteCursor(original.publicBlob)
        _ = try reader.sshString()
        let exponent = try reader.sshString(), modulus = try reader.sshString()
        var invalidBlobs: [Data] = []
        for invalid in [Data(), Data([0]), Data([1]), Data([2]), Data([0]) + exponent,
                        Data([0x80]), Data([1]) + Data(repeating: 1, count: 8)] {
            invalidBlobs.append(publicBlob(e: invalid, n: modulus))
        }
        for invalid in [Data(), Data([0]), Data(modulus.dropFirst()), Data([0]) + modulus,
                        Data([0x7f]) + Data(repeating: 0xff, count: 254),
                        Data([1]) + Data(repeating: 0xff, count: 1024)] {
            invalidBlobs.append(publicBlob(e: exponent, n: invalid))
        }
        var even = modulus; even[even.count - 1] &= 0xfe
        invalidBlobs.append(publicBlob(e: exponent, n: even))
        invalidBlobs.append(original.publicBlob + Data([0]))
        for invalid in invalidBlobs {
            var wire = original; wire.publicBlob = invalid
            XCTAssertThrowsError(try OpenSSHSourceKey(pem: wire.pem()))
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            XCTAssertThrowsError(try picker.select(pem: wire.pem(), reference: workspace(fixture).credentials[0]))
            XCTAssertNil(picker.request)
        }
    }

    func testLegacyRSARejectsMalformedIntegersCRTValuesVersionsAndDERExtents() async throws {
        let fixture = try RSAFixtures.load().keys[0]
        let original = try pkcs1Fields(fixture.pkcs1PEM)
        XCTAssertEqual(original.count, 9)
        for index in 1..<9 {
            var changed = original
            changed[index][changed[index].count - 1] ^= 2
            XCTAssertThrowsError(try OpenSSHSourceKey(pem: pkcs1PEM(changed), using: crypto), "DER component \(index)")
        }
        for index in 1..<9 {
            for invalid in [Data(), Data([0]), Data([0]) + original[index], Data([0x80])] {
                var changed = original; changed[index] = invalid
                XCTAssertThrowsError(try OpenSSHSourceKey(pem: pkcs1PEM(changed), using: crypto), "DER integer \(index)")
            }
        }
        var multiprime = original; multiprime[0] = Data([1])
        XCTAssertThrowsError(try OpenSSHSourceKey(pem: pkcs1PEM(multiprime), using: crypto))
        let valid = der(0x30, original.reduce(Data()) { $0 + der(0x02, $1) })
        for malformed in [valid + Data([0]), der(0x30, original.reduce(Data()) { $0 + der(0x02, $1) } + Data([0x05, 0x00])),
                          Data([0x30, 0x80]) + valid + Data([0, 0]), Data([0x30, 0x83, 0]) + valid.dropFirst(2)] {
            XCTAssertThrowsError(try OpenSSHSourceKey(pem: pem(malformed, label: "RSA PRIVATE KEY"), using: crypto))
        }
    }

    func testPKCS8RestrictionsAndEncryptedLegacyPEMAreNeverSilentlyDiscarded() async throws {
        let fixture = try RSAFixtures.load().keys[0]
        let rsaOID = Data([0x06, 0x09, 0x2a, 0x86, 0x48, 0x86, 0xf7, 0x0d, 0x01, 0x01, 0x01])
        var restricted = try decodePEM(fixture.pkcs8PEM)
        let oid = try XCTUnwrap(restricted.range(of: rsaOID))
        restricted[oid.upperBound - 1] = 0x0a // id-RSASSA-PSS, not rsaEncryption
        XCTAssertThrowsError(try OpenSSHSourceKey(pem: pem(restricted, label: "PRIVATE KEY"), using: crypto))
        let pkcs1 = try decodePEM(fixture.pkcs1PEM)
        let algorithm = der(0x30, rsaOID + Data([0x05, 0]))
        for content in [der(0x02, Data([1])) + algorithm + der(0x04, pkcs1),
                        der(0x02, Data([0])) + algorithm + der(0x04, pkcs1) + der(0xa0, Data()),
                        der(0x02, Data([0])) + der(0x30, rsaOID + Data([0x05, 1, 0])) + der(0x04, pkcs1)] {
            XCTAssertThrowsError(try OpenSSHSourceKey(pem: pem(der(0x30, content), label: "PRIVATE KEY"), using: crypto))
        }
        let encryptedLegacy = fixture.pkcs1PEM.replacingOccurrences(of: "-----BEGIN RSA PRIVATE KEY-----",
            with: "-----BEGIN RSA PRIVATE KEY-----\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-256-CBC,00112233445566778899AABBCCDDEEFF")
        XCTAssertThrowsError(try OpenSSHSourceKey(pem: Data(encryptedLegacy.utf8), using: crypto)) {
            XCTAssertEqual($0 as? FTError, .unsupported("source_key.encrypted_pem"))
        }
    }

    func testPassiveDiscoveryPreservesLegacyRSAIdentitiesWithoutPrivateKeyOperations() async throws {
        let fixtures = try RSAFixtures.load()
        var entries: [String: Data] = [:], configuration = ""
        for fixture in fixtures.keys {
            for (suffix, pem) in [("pkcs1", fixture.pkcs1PEM), ("pkcs8", fixture.pkcs8PEM)] {
                let name = fixture.id + "-" + suffix
                entries[name] = Data(pem.utf8)
                configuration += "Host \(name)\nIdentityFile ~/.ssh/\(name)\n"
                XCTAssertEqual(try LegacyRSASourceKey.embeddedPublicBlob(pem: Data(pem.utf8)), fixture.publicBlob)
            }
        }
        let home = try sourceFiles(entries), source = try SSHWorkspaceSource(configuration: Data(configuration.utf8))
        let snapshot = try source.loadPublicIdentityFiles(for: source.aliases, homeDirectory: home)
        XCTAssertEqual(snapshot.files.count, 6)
        for fixture in fixtures.keys {
            for suffix in ["pkcs1", "pkcs8"] {
                XCTAssertEqual(snapshot.files["~/.ssh/" + fixture.id + "-" + suffix], .present(fixture.publicBlob))
            }
        }
        var registry = SSHWorkspaceRegistry()
        let exported = try source.export(aliases: source.aliases, workspaceName: "Public RSA metadata", defaultUser: "operator",
            publicIdentityFiles: snapshot, registry: &registry, using: crypto)
        XCTAssertEqual(Set(exported.workspace.credentials.map(\.fingerprint)), Set(fixtures.keys.map(\.fingerprint)))
        XCTAssertTrue(exported.workspace.credentials.allSatisfy { $0.algorithm == "ssh-rsa" })
        XCTAssertFalse(exported.portability.contains { $0.setting == "ssh_config.private_key_format" })
        let manifest = String(decoding: exported.workspace.raw, as: UTF8.self)
        XCTAssertFalse(manifest.contains("PRIVATE KEY"))
        XCTAssertFalse(manifest.contains("~/.ssh/"))
        for (name, original) in entries {
            XCTAssertEqual(try Data(contentsOf: home.appendingPathComponent(".ssh/" + name)), original)
        }
    }

    func testPassiveRSAMetadataHonorsSidecarAndExplicitPublicFileBoundaries() async throws {
        let fixtures = try RSAFixtures.load(), fixture = fixtures.keys[0], other = fixtures.keys[1]
        let entries = ["bad-sidecar": Data(fixture.pkcs1PEM.utf8), "bad-sidecar.pub": Data("malformed public identity\n".utf8),
                       "explicit": Data(fixture.pkcs8PEM.utf8), "good-sidecar": Data(fixture.pkcs1PEM.utf8),
                       "good-sidecar.pub": Data((other.publicKey + "\n").utf8)]
        let home = try sourceFiles(entries)
        let source = try SSHWorkspaceSource(configuration: Data("""
        Host bad
          IdentityFile ~/.ssh/bad-sidecar
        Host explicit
          IdentityFile ~/.ssh/explicit.pub
        Host good
          IdentityFile ~/.ssh/good-sidecar
        """.utf8))
        let snapshot = try source.loadPublicIdentityFiles(for: source.aliases, homeDirectory: home)
        XCTAssertEqual(snapshot.files["~/.ssh/bad-sidecar"], .unavailable)
        XCTAssertEqual(snapshot.files["~/.ssh/explicit.pub"], .missing)
        XCTAssertEqual(snapshot.files["~/.ssh/good-sidecar"], .present(other.publicBlob))
        // A valid sidecar describes an approved identity; selecting a different
        // private key must still fail rather than inheriting that identity.
        var picker = OpenSSHSourceKeyUnlocker(using: crypto)
        XCTAssertThrowsError(try picker.select(pem: entries["good-sidecar"]!, reference: workspace(other).credentials[0])) {
            XCTAssertEqual($0 as? OpenSSHSourceKeyError, .identityMismatch)
        }
        for (name, original) in entries {
            XCTAssertEqual(try Data(contentsOf: home.appendingPathComponent(".ssh/" + name)), original)
        }
    }

    func testPassivePublicMetadataNeverGrantsApprovalToInvalidPrivateRSAComponents() async throws {
        let fixture = try RSAFixtures.load().keys[0]
        var fields = try pkcs1Fields(fixture.pkcs1PEM)
        // This is structurally bounded and retains genuine public metadata,
        // but p=1 cannot initialize the maintained RSA private primitive.
        fields[4] = Data([1])
        let malformedPrivate = pkcs1PEM(fields)
        XCTAssertEqual(try LegacyRSASourceKey.embeddedPublicBlob(pem: malformedPrivate), fixture.publicBlob)
        let home = try sourceFiles(["public-only": malformedPrivate])
        let source = try SSHWorkspaceSource(configuration: Data("Host selected\nIdentityFile ~/.ssh/public-only\n".utf8))
        XCTAssertEqual(try source.loadPublicIdentityFiles(for: ["selected"], homeDirectory: home).files["~/.ssh/public-only"],
                       .present(fixture.publicBlob))
        XCTAssertThrowsError(try OpenSSHSourceKey(pem: malformedPrivate, using: crypto))
        XCTAssertThrowsError(try PrivateKeyRecord.fromPEM(malformedPrivate, credentialID: "key", using: crypto))
        fields[0] = Data([1])
        XCTAssertThrowsError(try LegacyRSASourceKey.embeddedPublicBlob(pem: pkcs1PEM(fields)))
        fields[0] = Data([0]); fields[2] = Data([0, 1])
        XCTAssertThrowsError(try LegacyRSASourceKey.embeddedPublicBlob(pem: pkcs1PEM(fields)))
    }

    func testOutOfProfilePublicRSAIsPreservedWithoutAdvertisingPrivateExportSupport() async throws {
        let source = try SSHWorkspaceSource(configuration: Data("Host legacy\nIdentityFile ~/.ssh/legacy\n".utf8))
        // Passive public metadata may describe an older key, while the private
        // importer and native signer deliberately require at least 2048 bits.
        let blob = publicBlob(e: Data([1, 0, 1]), n: Data([0]) + Data(repeating: 0xff, count: 128))
        let publicFile = Data(("ssh-rsa " + blob.base64EncodedString() + "\n").utf8)
        var registry = SSHWorkspaceRegistry()
        let exported = try source.export(aliases: ["legacy"], workspaceName: "Legacy metadata", defaultUser: "operator",
            publicKeys: ["~/.ssh/legacy": publicFile], registry: &registry, using: crypto)
        XCTAssertEqual(exported.workspace.credentials.count, 1)
        XCTAssertEqual(exported.workspace.credentials[0].algorithm, "ssh-rsa")
        XCTAssertTrue(exported.portability.contains { $0.setting == "ssh_config.private_key_format" })
        XCTAssertThrowsError(try SSHPublicIdentity(blob: blob))
    }

    private func sourceFiles(_ entries: [String: Data]) throws -> URL {
        let home = FileManager.default.temporaryDirectory.appendingPathComponent("ft-rsa-identities-" + UUID().uuidString)
        let directory = home.appendingPathComponent(".ssh")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        for (name, bytes) in entries { try bytes.write(to: directory.appendingPathComponent(name)) }
        return home
    }

    private func workspace(_ fixture: RSAFixtures.Key) throws -> WorkspaceDocument {
        var root = try JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fixture.fingerprint)) as! [String: Any]
        var workspace = root["workspace"] as! [String: Any]
        var credentials = workspace["credentials"] as! [[String: Any]]
        credentials[0]["algorithm"] = "ssh-rsa"
        workspace["credentials"] = credentials; root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    private func privateFields(_ container: Data) throws -> [Data] {
        var outer = ByteCursor(container)
        _ = try outer.take(15)
        for _ in 0..<3 { _ = try outer.sshString() }
        _ = try outer.uint(4); _ = try outer.sshString()
        var inner = ByteCursor(try outer.sshString())
        _ = try inner.take(8)
        XCTAssertEqual(try inner.sshString(), Data("ssh-rsa".utf8))
        return try (0..<6).map { _ in try inner.sshString() }
    }

    private func privatePart(fields: [Data]) -> Data {
        var body = Data(repeating: 0x42, count: 8) + RSAWire.ssh(Data("ssh-rsa".utf8))
        for field in fields { body += RSAWire.ssh(field) }
        body += RSAWire.ssh(Data())
        var padding: UInt8 = 1
        while !body.count.isMultiple(of: 8) { body.append(padding); padding += 1 }
        return body
    }

    private func publicBlob(e: Data, n: Data) -> Data {
        RSAWire.ssh(Data("ssh-rsa".utf8)) + RSAWire.ssh(e) + RSAWire.ssh(n)
    }

    private func decodePEM(_ text: String) throws -> Data {
        let lines = text.split(whereSeparator: \.isNewline)
        return try XCTUnwrap(Data(base64Encoded: lines.dropFirst().dropLast().joined()))
    }

    private func pkcs1Fields(_ text: String) throws -> [Data] {
        var cursor = ByteCursor(try decodePEM(text))
        func read(_ input: inout ByteCursor, tag: UInt64) throws -> Data {
            XCTAssertEqual(try input.uint(1), tag)
            let first = try input.uint(1)
            let count = first < 128 ? first : try input.uint(Int(first & 0x7f))
            return try input.take(Int(count))
        }
        var body = ByteCursor(try read(&cursor, tag: 0x30))
        XCTAssertEqual(cursor.remaining, 0)
        var fields: [Data] = []
        while body.remaining > 0 { fields.append(try read(&body, tag: 0x02)) }
        return fields
    }

    private func der(_ tag: UInt8, _ content: Data) -> Data {
        var result = Data([tag])
        if content.count < 128 { result.append(UInt8(content.count)) }
        else if content.count < 256 { result += Data([0x81, UInt8(content.count)]) }
        else { result.append(0x82); result.appendBE(UInt64(content.count), bytes: 2) }
        result += content; return result
    }
    private func pkcs1PEM(_ fields: [Data]) -> Data {
        pem(der(0x30, fields.reduce(Data()) { $0 + der(0x02, $1) }), label: "RSA PRIVATE KEY")
    }
    private func pem(_ bytes: Data, label: String) -> Data {
        Data(("-----BEGIN \(label)-----\n" + bytes.base64EncodedString() + "\n-----END \(label)-----\n").utf8)
    }
}

private struct RSAWire {
    var cipher: Data, kdf: Data, options: Data, publicBlob: Data, privatePart: Data
    let keyCount: UInt64
    init(pem: String) throws {
        let lines = pem.split(whereSeparator: \.isNewline)
        var cursor = ByteCursor(try XCTUnwrap(Data(base64Encoded: lines.dropFirst().dropLast().joined())))
        XCTAssertEqual(try cursor.take(15), Data("openssh-key-v1\0".utf8))
        cipher = try cursor.sshString(); kdf = try cursor.sshString(); options = try cursor.sshString()
        keyCount = try cursor.uint(4); publicBlob = try cursor.sshString(); privatePart = try cursor.sshString()
        XCTAssertEqual(cursor.remaining, 0)
    }
    static func ssh(_ data: Data) -> Data {
        var result = Data(); result.appendBE(UInt64(data.count), bytes: 4); result += data; return result
    }
    func container() -> Data {
        var result = Data("openssh-key-v1\0".utf8)
        for field in [cipher, kdf, options] { result += Self.ssh(field) }
        result.appendBE(keyCount, bytes: 4)
        result += Self.ssh(publicBlob); result += Self.ssh(privatePart); return result
    }
    func pem() -> Data {
        Data(("-----BEGIN OPENSSH PRIVATE KEY-----\n" + container().base64EncodedString() + "\n-----END OPENSSH PRIVATE KEY-----\n").utf8)
    }
}
