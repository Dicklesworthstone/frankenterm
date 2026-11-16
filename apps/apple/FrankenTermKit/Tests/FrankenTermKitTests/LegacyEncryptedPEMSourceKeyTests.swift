import Foundation
import XCTest
@testable import FrankenTermKit

/// Positive containers, malformed encrypted plaintexts and KDF vectors come
/// from independent OpenSSL operations over existing public synthetic keys.
/// The tested unlocker always executes the production Swift MD5 derivation;
/// Linux AES and key import use the existing independent OpenSSL test oracle.
@MainActor
final class LegacyEncryptedPEMSourceKeyTests: XCTestCase {
    private let crypto = LegacyPEMTestCryptography()

    func testOpenSSLMatrixPreservesPrivateIdentityThroughThePublicUnlocker() async throws {
        let fixtures = try LegacyPEMFixtures.load()
        XCTAssertEqual(fixtures.keys.count, 18)
        XCTAssertEqual(Set(fixtures.keys.map(\.aesKeyBits)), [128, 192, 256])
        XCTAssertEqual(Set(fixtures.keys.map(\.algorithm)), Set(["ssh-rsa"] + SSHECDSACurve.allCases.map(\.algorithm)))
        for fixture in fixtures.keys {
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            let reference = try workspace(fixture).credentials[0]
            let selected = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
            XCTAssertEqual(selected.protection, .legacyPEM(aesKeyBits: fixture.aesKeyBits))
            XCTAssertTrue(selected.protection.hidesPublicIdentity)
            XCTAssertEqual(selected.fingerprint, fixture.fingerprint)
            XCTAssertEqual(selected.algorithm, fixture.algorithm)
            XCTAssertNil(selected.bcryptRounds)
            let record = try picker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))
            XCTAssertNil(picker.request)
            XCTAssertEqual(record.publicBlob, fixture.publicBlob, fixture.id)
            XCTAssertEqual(try PrivateKeyRecord.validateContainer(record.container, using: crypto), fixture.publicBlob, fixture.id)
            let original = try PrivateKeyRecord.fromPEM(fixture.clearPEM(), credentialID: reference.id, using: crypto)
            XCTAssertEqual(try privateIdentityDigest(record), try privateIdentityDigest(original), fixture.id)
        }
    }

    func testUnlockedKeysSurviveAuthenticatedTransferWithoutChangingTheirIdentity() async throws {
        let fixtures = try LegacyPEMFixtures.load()
        for id in ["rsa-2048-aes256", "nistp384-1-aes128"] {
            let fixture = try XCTUnwrap(fixtures.keys.first { $0.id == id })
            let document = try workspace(fixture)
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            let selected = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: document.credentials[0]))
            let record = try picker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))
            let archive = try AirGapArchive(workspace: document, keys: [record], using: crypto)
            let sealed = try FTAG.seal(archive, using: crypto)
            let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
            XCTAssertEqual(restored.workspace.raw, document.raw)
            XCTAssertEqual(restored.keys.count, 1)
            XCTAssertEqual(restored.keys[0].publicBlob, fixture.publicBlob)
            XCTAssertEqual(try privateIdentityDigest(restored.keys[0]), try privateIdentityDigest(record))
            XCTAssertEqual(try SSHUserAuthenticationSigner.publicIdentity(privateContainer: restored.keys[0].container).fingerprint,
                           fixture.fingerprint)
        }
    }

    func testProductionDerivationMatchesOpenSSLIncludingBinaryAndMaximumPasswords() async throws {
        let fixtures = try LegacyPEMFixtures.load()
        XCTAssertEqual(fixtures.vectors.count, 9)
        for vector in fixtures.vectors {
            XCTAssertEqual(try LegacyPEMSourcePrimitives.key(passphrase: Data(ftHex: vector.passphraseHex),
                salt: Data(ftHex: vector.saltHex), outputByteCount: vector.outputByteCount),
                try Data(ftHex: vector.expectedHex))
        }
    }

    func testDerivationAdmitsOnlyBoundedExistingFileParameters() async throws {
        let password = Data([1]), salt = Data(repeating: 2, count: 8)
        for size in [-1, 0, 15, 17, 23, 25, 31, 33, Int.max] {
            XCTAssertThrowsError(try LegacyPEMSourcePrimitives.key(passphrase: password, salt: salt, outputByteCount: size))
        }
        for size in [0, 7, 9, 16, 64] {
            XCTAssertThrowsError(try LegacyPEMSourcePrimitives.key(passphrase: password,
                salt: Data(repeating: 2, count: size), outputByteCount: 32))
        }
        for password in [Data(), Data(repeating: 1, count: OpenSSHSourceKey.maximumPassphraseBytes + 1)] {
            XCTAssertThrowsError(try LegacyPEMSourcePrimitives.key(passphrase: password, salt: salt, outputByteCount: 32))
        }
    }

    func testEncryptedIdentityMustMatchTheSeparatelyApprovedReferenceAfterUnlock() async throws {
        let fixtures = try LegacyPEMFixtures.load()
        for id in ["rsa-2048-aes192", "nistp256-1-aes256"] {
            let fixture = try XCTUnwrap(fixtures.keys.first { $0.id == id })
            let other = try XCTUnwrap(fixtures.keys.first { $0.algorithm == fixture.algorithm && $0.sourceID != fixture.sourceID }
                ?? fixtures.keys.first { $0.algorithm != fixture.algorithm })
            let reference = try workspace(fixture, fingerprint: other.fingerprint).credentials[0]
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            let selected = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
            XCTAssertEqual(selected.fingerprint, other.fingerprint)
            XCTAssertThrowsError(try picker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, .identityMismatch)
            }
        }
        let ec = try XCTUnwrap(fixtures.keys.first { $0.id == "nistp256-1-aes128" })
        let otherCurve = try XCTUnwrap(fixtures.keys.first { $0.id == "nistp384-1-aes128" })
        var picker = OpenSSHSourceKeyUnlocker(using: crypto)
        let selected = try request(picker.select(pem: Data(ec.encryptedPEM.utf8), reference: workspace(otherCurve).credentials[0]))
        XCTAssertThrowsError(try picker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))) {
            XCTAssertEqual($0 as? OpenSSHSourceKeyError, .identityMismatch)
        }
    }

    func testWrongPasswordAndCiphertextDamageNeverReleaseAKey() async throws {
        let fixtures = try LegacyPEMFixtures.load()
        for fixture in [fixtures.keys[0], fixtures.keys[9]] {
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            let reference = try workspace(fixture).credentials[0]
            let selected = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
            XCTAssertThrowsError(try picker.unlock(requestID: selected.id, passphrase: Data("wrong public password".utf8))) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey)
                XCTAssertFalse($0.localizedDescription.contains(fixtures.passphrase))
            }
            XCTAssertEqual(picker.request?.attemptsRemaining, 4)
            for index in [0, -1] {
                var lines = fixture.encryptedPEM.trimmingCharacters(in: .newlines).components(separatedBy: "\n")
                var ciphertext = try XCTUnwrap(Data(base64Encoded: lines.dropFirst(4).dropLast().joined()))
                ciphertext[index == -1 ? ciphertext.count - 1 : index] ^= 0x80
                lines.replaceSubrange(4..<(lines.count - 1), with: [ciphertext.base64EncodedString()])
                let request = try self.request(picker.select(pem: Data(lines.joined(separator: "\n").utf8), reference: reference))
                XCTAssertThrowsError(try picker.unlock(requestID: request.id, passphrase: Data(fixtures.passphrase.utf8))) {
                    XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey)
                }
            }
        }
    }

    func testOpenSSLEncryptedNoncanonicalDERAndInvalidPrivateComponentsAreRejected() async throws {
        let fixtures = try LegacyPEMFixtures.load()
        XCTAssertEqual(fixtures.invalid.count, 12)
        for invalid in fixtures.invalid {
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            let reference = try workspace(algorithm: invalid.algorithm, fingerprint: invalid.fingerprint).credentials[0]
            let selected = try request(picker.select(pem: Data(invalid.encryptedPEM.utf8), reference: reference))
            XCTAssertThrowsError(try picker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8)), invalid.id) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey, invalid.id)
            }
        }
    }

    func testHeaderGeometryCipherAndPayloadAreCheckedBeforePromptOrCrypto() async throws {
        let fixture = try LegacyPEMFixtures.load().keys[0]
        let lines = fixture.encryptedPEM.trimmingCharacters(in: .newlines).components(separatedBy: "\n")
        let original = fixture.encryptedPEM
        let badHeader = [
            original.replacingOccurrences(of: "Proc-Type: 4,ENCRYPTED", with: "Proc-Type: 5,ENCRYPTED"),
            original.replacingOccurrences(of: "Proc-Type: 4,ENCRYPTED", with: "Proc-Type: 4,CLEAR"),
            original.replacingOccurrences(of: "Proc-Type: 4,ENCRYPTED\n", with: ""),
            original.replacingOccurrences(of: "Proc-Type: 4,ENCRYPTED", with: "Proc-Type: 4,ENCRYPTED\nProc-Type: 4,ENCRYPTED"),
            original.replacingOccurrences(of: "DEK-Info: ", with: "DEK-Info: \nDEK-Info: "),
            original.replacingOccurrences(of: "\n\n", with: "\n"),
            original.replacingOccurrences(of: "AES-128-CBC", with: "DES-EDE3-CBC"),
            original.replacingOccurrences(of: "AES-128-CBC", with: "AES-128-CTR"),
            original.replacingOccurrences(of: lines[2], with: "DEK-Info: AES-128-CBC,0000"),
            original.replacingOccurrences(of: lines[2], with: "DEK-Info: AES-128-CBC," + String(repeating: "G", count: 32)),
            original.replacingOccurrences(of: lines[2], with: lines[2] + ",ignored"),
            original.replacingOccurrences(of: "-----END RSA PRIVATE KEY-----", with: "-----END EC PRIVATE KEY-----"),
            original + original,
            original.replacingOccurrences(of: lines[4], with: ":" + lines[4]),
            original.replacingOccurrences(of: "\n", with: "\r"),
            original.replacingOccurrences(of: "RSA PRIVATE KEY", with: "PRIVATE KEY")
        ]
        var badPayloads: [String] = []
        for size in [0, 1, 15, 17, LegacyRSASourceKey.maximumDERBytes + 32] {
            badPayloads.append((Array(lines.prefix(4)) + [Data(repeating: 1, count: size).base64EncodedString(), lines.last!])
                .joined(separator: "\n"))
        }
        let denied = LegacyPEMTestCryptography(behavior: .denied)
        var picker = OpenSSHSourceKeyUnlocker(using: denied)
        for bad in badHeader + badPayloads {
            XCTAssertThrowsError(try picker.select(pem: Data(bad.utf8), reference: workspace(fixture).credentials[0]))
            XCTAssertNil(picker.request)
        }
        XCTAssertEqual(denied.calls.snapshot.kdf, 0)
        XCTAssertEqual(denied.calls.snapshot.aes, 0)
    }

    func testCRLFAndLowercaseIVRemainCompatibleWithOpenSSL() async throws {
        let fixtures = try LegacyPEMFixtures.load(), fixture = fixtures.keys[9]
        var lines = fixture.encryptedPEM.components(separatedBy: "\n")
        let parts = lines[2].components(separatedBy: ",")
        lines[2] = parts[0] + "," + parts[1].lowercased()
        var picker = OpenSSHSourceKeyUnlocker(using: crypto)
        let selected = try request(picker.select(pem: Data(lines.joined(separator: "\r\n").utf8), reference: workspace(fixture).credentials[0]))
        XCTAssertEqual(try picker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8)).publicBlob, fixture.publicBlob)
    }

    func testSelectedFamilyIsCheckedBeforeAPromptAndPassiveKeyConsumersStillRefuseEncryption() async throws {
        let fixtures = try LegacyPEMFixtures.load(), rsa = fixtures.keys[0], ec = fixtures.keys[9]
        for (fixture, reference) in [(rsa, try workspace(ec).credentials[0]), (ec, try workspace(rsa).credentials[0])] {
            var picker = OpenSSHSourceKeyUnlocker(using: crypto)
            XCTAssertThrowsError(try picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference)) {
                XCTAssertEqual($0 as? FTError, .unsupported("source_key.family"))
            }
            XCTAssertNil(picker.request)
            XCTAssertThrowsError(try PrivateKeyRecord.fromPEM(Data(fixture.encryptedPEM.utf8), credentialID: "key", using: crypto))
        }
    }

    func testAttemptsAndCancelledOrReplacedRequestsStayBoundToTheSelectedFile() async throws {
        let fixtures = try LegacyPEMFixtures.load(), fixture = fixtures.keys[0]
        let reference = try workspace(fixture).credentials[0]
        var picker = OpenSSHSourceKeyUnlocker(using: crypto)
        let old = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
        let selected = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
        XCTAssertThrowsError(try picker.unlock(requestID: old.id, passphrase: Data(fixtures.passphrase.utf8))) {
            XCTAssertEqual($0 as? FTError, .stale)
        }
        XCTAssertThrowsError(try picker.cancel(requestID: old.id))
        XCTAssertEqual(picker.request?.id, selected.id)
        for attempt in 1...5 {
            XCTAssertThrowsError(try picker.unlock(requestID: selected.id, passphrase: Data([1]))) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, attempt == 5 ? .attemptsExhausted : .incorrectPassphraseOrDamagedKey)
            }
        }
        XCTAssertNil(picker.request)
        let cancelled = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
        try picker.cancel(requestID: cancelled.id)
        XCTAssertThrowsError(try picker.unlock(requestID: cancelled.id, passphrase: Data(fixtures.passphrase.utf8))) {
            XCTAssertEqual($0 as? FTError, .stale)
        }
    }

    func testCancellationAroundNativeWorkRetiresTheExactRequest() async throws {
        let fixtures = try LegacyPEMFixtures.load(), fixture = fixtures.keys[9]
        let reference = try workspace(fixture).credentials[0]
        for behavior in [LegacyPEMTestCryptography.Behavior.denied, .cancelAfterKDF, .cancelAfterAES] {
            let audit = LegacyPEMTestCryptography(behavior: behavior)
            let operation = Task { () throws -> Bool in
                var picker = OpenSSHSourceKeyUnlocker(using: audit)
                let selected = try self.request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
                if behavior == .denied { withUnsafeCurrentTask { $0?.cancel() } }
                do {
                    _ = try picker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))
                    return false
                } catch is CancellationError { return picker.request == nil }
            }
            let retired = try await operation.value
            XCTAssertTrue(retired)
            XCTAssertEqual(audit.calls.snapshot.kdf, behavior == .denied ? 0 : 1)
            XCTAssertEqual(audit.calls.snapshot.aes, behavior == .cancelAfterAES ? 1 : 0)
        }
    }

    func testShortKDFOutputAndUnboundedPasswordsCannotReachAES() async throws {
        let fixture = try LegacyPEMFixtures.load().keys[0]
        let audit = LegacyPEMTestCryptography(behavior: .shortKDF)
        var picker = OpenSSHSourceKeyUnlocker(using: audit)
        let selected = try request(picker.select(pem: Data(fixture.encryptedPEM.utf8), reference: workspace(fixture).credentials[0]))
        for password in [Data(), Data(repeating: 1, count: 1025)] {
            XCTAssertThrowsError(try picker.unlock(requestID: selected.id, passphrase: password)) {
                XCTAssertEqual($0 as? FTError, .limit("source_key.passphrase"))
            }
        }
        XCTAssertEqual(audit.calls.snapshot.kdf, 0)
        XCTAssertThrowsError(try picker.unlock(requestID: selected.id, passphrase: Data([1]))) {
            XCTAssertEqual($0 as? FTError, .malformed("source_key.kdf_output"))
        }
        XCTAssertEqual(audit.calls.snapshot.kdf, 1)
        XCTAssertEqual(audit.calls.snapshot.aes, 0)
    }

    private func request<C: AirGapCryptography>(_ selection: OpenSSHSourceKeyUnlocker<C>.Selection) throws -> OpenSSHSourceKeyUnlocker<C>.Request {
        guard case .passphraseRequired(let value) = selection else { throw FTError.malformed("test.expected_prompt") }
        return value
    }
    private func workspace(_ fixture: LegacyPEMFixtures.Key, fingerprint: String? = nil) throws -> WorkspaceDocument {
        try workspace(algorithm: fixture.algorithm, fingerprint: fingerprint ?? fixture.fingerprint)
    }
    private func workspace(algorithm: String, fingerprint: String) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fingerprint)) as? [String: Any])
        var value = try XCTUnwrap(root["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(value["credentials"] as? [[String: Any]])
        credentials[0]["algorithm"] = algorithm; value["credentials"] = credentials; root["workspace"] = value
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
    }
    private func privateIdentityDigest(_ record: PrivateKeyRecord) throws -> Data {
        var outer = ByteCursor(record.container)
        _ = try outer.take(15)
        for _ in 0..<3 { _ = try outer.sshString() }
        _ = try outer.uint(4); _ = try outer.sshString()
        var inner = ByteCursor(try outer.sshString())
        _ = try inner.take(8)
        let algorithm = try inner.sshString()
        for _ in 0..<(algorithm == Data("ssh-rsa".utf8) ? 7 : 4) { _ = try inner.sshString() }
        return crypto.sha256(inner.data.subdata(in: 8..<inner.offset))
    }
}

private struct LegacyPEMFixtures: Decodable, Sendable {
    struct Key: Decodable, Sendable {
        let id, sourceFile, sourceID, clearField, algorithm, fingerprint, publicKey, encryptedPEM: String
        let aesKeyBits: Int
        var publicBlob: Data { Data(base64Encoded: publicKey.split(separator: " ")[1].description)! }
        func clearPEM() throws -> Data {
            guard ["rsa-source-keys.json", "legacy-ecdsa-source-keys.json"].contains(sourceFile),
                  ["pkcs1PEM", "sec1PEM"].contains(clearField),
                  let source = try JSONSerialization.jsonObject(with: Data(contentsOf: LegacyPEMFixtures.directory.appendingPathComponent(sourceFile))) as? [String: Any],
                  let keys = source["keys"] as? [[String: Any]],
                  let key = keys.first(where: { $0["id"] as? String == sourceID }), let pem = key[clearField] as? String else {
                throw FTError.malformed("test.fixture")
            }
            return Data(pem.utf8)
        }
    }
    struct Vector: Decodable, Sendable {
        let passphraseHex, saltHex, expectedHex: String
        let outputByteCount: Int
    }
    struct Invalid: Decodable, Sendable { let id, algorithm, fingerprint, encryptedPEM: String }
    let passphrase: String, keys: [Key], vectors: [Vector], invalid: [Invalid]
    static var directory: URL {
        (0..<6).reduce(URL(fileURLWithPath: #filePath)) { value, _ in value.deletingLastPathComponent() }
            .appendingPathComponent("fixtures/ios-airgap")
    }
    static func load() throws -> Self {
        try JSONDecoder().decode(Self.self, from: Data(contentsOf: directory.appendingPathComponent("legacy-encrypted-pem-source-keys.json")))
    }
}

private final class LegacyPEMCalls: @unchecked Sendable {
    private let lock = NSLock()
    private var kdf = 0, aes = 0
    var snapshot: (kdf: Int, aes: Int) { lock.withLock { (kdf, aes) } }
    func derived() { lock.withLock { kdf += 1 } }
    func decrypted() { lock.withLock { aes += 1 } }
}

private struct LegacyPEMTestCryptography: AirGapCryptography {
    enum Behavior: Equatable, Sendable { case normal, denied, shortKDF, cancelAfterKDF, cancelAfterAES }
    let base = TestCryptography(), calls = LegacyPEMCalls()
    let behavior: Behavior
    init(behavior: Behavior = .normal) { self.behavior = behavior }
    func sha256(_ bytes: Data) -> Data { base.sha256(bytes) }
    func hmacSHA1(key: Data, message: Data) throws -> Data { try base.hmacSHA1(key: key, message: message) }
    func randomBytes(count: Int) throws -> Data { try base.randomBytes(count: count) }
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { try base.seal(plaintext, secret: secret, salt: salt, nonce: nonce, aad: aad) }
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { try base.open(ciphertextAndTag, secret: secret, salt: salt, nonce: nonce, aad: aad) }
    func ed25519PublicKey(seed: Data) throws -> Data { try base.ed25519PublicKey(seed: seed) }
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data { try base.ecdsaPublicKey(scalar: scalar, curve: curve) }
    func importECDSAPrivateScalar(_ der: Data, curve: SSHECDSACurve) throws -> Data { try base.importECDSAPrivateScalar(der, curve: curve) }
    func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data { try base.bcryptPBKDF(passphrase: passphrase, salt: salt, rounds: rounds, outputByteCount: outputByteCount) }
    func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data { try base.decryptAESCTR(ciphertext, key: key, iv: iv) }
    func legacyPEMKey(passphrase: Data, salt: Data, outputByteCount: Int) throws -> Data {
        calls.derived()
        if behavior == .denied { throw FTError.storage }
        if behavior == .shortKDF { return Data(repeating: 0, count: outputByteCount - 1) }
        let result = try LegacyPEMSourcePrimitives.key(passphrase: passphrase, salt: salt, outputByteCount: outputByteCount)
        if behavior == .cancelAfterKDF { withUnsafeCurrentTask { $0?.cancel() } }
        return result
    }
    func decryptAESCBC(_ ciphertext: Data, key: Data, iv: Data) throws -> Data {
        calls.decrypted()
        let result = try base.decryptAESCBC(ciphertext, key: key, iv: iv)
        if behavior == .cancelAfterAES { withUnsafeCurrentTask { $0?.cancel() } }
        return result
    }
}
