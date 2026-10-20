import Foundation
import XCTest
@testable import FrankenTermKit

/// The fixture keys and passphrase are deliberately public and must never be
/// used for authentication. Both protected and clear files were independently
/// produced by OpenSSH ssh-keygen 9.6p1; no production encoder made the vectors.
@MainActor
final class OpenSSHSourceKeyTests: XCTestCase {
    let crypto = TestCryptography()

    func testIndependentEncryptedAndClearKeysAgreeForEverySupportedFamily() async throws {
        let fixtures = try SourceKeyFixtures.load()
        XCTAssertEqual(fixtures.pairs.count, 6)
        XCTAssertEqual(Set(fixtures.pairs.map(\.algorithm)).count, 4)
        for fixture in fixtures.pairs {
            let source = try OpenSSHSourceKey(pem: Data(fixture.encryptedPEM.utf8))
            XCTAssertEqual(source.algorithm, fixture.algorithm, fixture.id)
            XCTAssertTrue(source.isEncrypted, fixture.id)
            XCTAssertEqual(source.bcryptRounds, fixture.rounds, fixture.id)
            XCTAssertEqual(source.fingerprint(using: crypto), fixture.fingerprint, fixture.id)
            let independent = try PrivateKeyRecord.fromPEM(Data(fixture.clearPEM.utf8), credentialID: "key", using: crypto)
            let unlocked = try source.unlock(passphrase: Data(fixtures.passphrase.utf8), credentialID: "key", using: crypto)
            XCTAssertEqual(unlocked.publicBlob, independent.publicBlob, fixture.id)
            XCTAssertEqual(unlocked.credentialID, "key")
            XCTAssertEqual(try identityDigest(unlocked.container), try identityDigest(independent.container), fixture.id)
            XCTAssertEqual(try PrivateKeyRecord.validateContainer(unlocked.container, using: crypto), independent.publicBlob)
            let clear = try OpenSSHSourceKey(pem: Data(fixture.clearPEM.utf8))
            XCTAssertFalse(clear.isEncrypted)
            XCTAssertNil(clear.bcryptRounds)
            XCTAssertEqual(try clear.unlock(credentialID: "key", using: crypto).container, independent.container)
        }
    }

    func testEveryEncryptedPaddingLengthRepackagesToCanonicalClearContainer() async throws {
        let fixtures = try SourceKeyFixtures.load()
        XCTAssertEqual(fixtures.paddingCases.count, 64)
        for algorithm in Set(fixtures.paddingCases.map(\.algorithm)) {
            XCTAssertEqual(Set(fixtures.paddingCases.filter { $0.algorithm == algorithm }.map(\.paddingBytes)), Set(0..<16))
        }
        for fixture in fixtures.paddingCases {
            let source = try OpenSSHSourceKey(pem: Data(fixture.encryptedPEM.utf8))
            let key = try source.unlock(passphrase: Data(fixtures.passphrase.utf8), credentialID: "key", using: crypto)
            XCTAssertEqual(source.fingerprint(using: crypto), fixture.fingerprint, fixture.id)
            let wire = try SourceKeyWire(container: key.container)
            XCTAssertEqual(wire.cipher, Data("none".utf8))
            XCTAssertEqual(wire.kdf, Data("none".utf8))
            XCTAssertTrue(wire.options.isEmpty)
            XCTAssertTrue(wire.privatePart.count.isMultiple(of: 8))
            XCTAssertEqual(try PrivateKeyRecord.validateContainer(key.container, using: crypto), key.publicBlob, fixture.id)
        }
    }

    func testUnlockedKeysSurviveAuthenticatedArchiveAndStillBindTheirIdentity() async throws {
        let fixtures = try SourceKeyFixtures.load()
        for fixture in fixtures.pairs.prefix(4) {
            let key = try OpenSSHSourceKey(pem: Data(fixture.encryptedPEM.utf8))
                .unlock(passphrase: Data(fixtures.passphrase.utf8), credentialID: "key", using: crypto)
            let document = try workspace(for: fixture)
            let archive = try AirGapArchive(workspace: document, keys: [key], using: crypto)
            let sealed = try FTAG.seal(archive, using: crypto)
            let opened = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
            XCTAssertEqual(opened.keys.count, 1)
            XCTAssertEqual(opened.keys[0].publicBlob, key.publicBlob)
            XCTAssertEqual(crypto.sha256(opened.keys[0].container), crypto.sha256(key.container))
        }
    }

    func testPassiveArchiveParserStillRejectsEncryptedInnerContainers() async throws {
        let fixture = try SourceKeyFixtures.load().pairs[0]
        let pem = Data(fixture.encryptedPEM.utf8)
        XCTAssertThrowsError(try PrivateKeyRecord.fromPEM(pem, credentialID: "key", using: crypto)) { error in
            XCTAssertEqual(error as? FTError, .unsupported("key.encrypted_container"))
        }
        let wire = try SourceKeyWire(pem: pem)
        let key = PrivateKeyRecord(credentialID: "key", container: wire.encoded(), publicBlob: wire.publicBlob)
        XCTAssertThrowsError(try AirGapArchive(workspace: workspace(for: fixture), keys: [key], using: crypto))
    }

    func testWrongPassphraseAndCorruptedPrivateBytesHaveContentFreeErrors() async throws {
        let fixtures = try SourceKeyFixtures.load(), fixture = fixtures.pairs[0]
        let original = try SourceKeyWire(pem: Data(fixture.encryptedPEM.utf8))
        let source = try OpenSSHSourceKey(pem: original.pem())
        XCTAssertThrowsError(try source.unlock(passphrase: Data("incorrect-public-test-passphrase".utf8), credentialID: "key", using: crypto)) {
            XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey)
        }
        var check = original; check.privatePart[0] ^= 1
        var padding = original; padding.privatePart[padding.privatePart.count - 1] ^= 1
        var identity = original; identity.publicBlob[identity.publicBlob.count - 1] ^= 1
        for changed in [check, padding, identity] {
            XCTAssertThrowsError(try OpenSSHSourceKey(pem: changed.pem()).unlock(
                passphrase: Data(fixtures.passphrase.utf8), credentialID: "key", using: crypto)) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey)
            }
        }
        XCTAssertEqual(String(describing: source), "OpenSSHSourceKey([redacted])")
        XCTAssertEqual(String(reflecting: source), "OpenSSHSourceKey([redacted])")
        XCTAssertFalse(OpenSSHSourceKeyError.incorrectPassphraseOrDamagedKey.localizedDescription.contains(fixtures.passphrase))
    }

    func testMissingAndOutOfBoundsPassphrasesNeverReachNativeCrypto() async throws {
        let fixture = try SourceKeyFixtures.load().pairs[0]
        let source = try OpenSSHSourceKey(pem: Data(fixture.encryptedPEM.utf8)), denied = DeniedSourceCryptography()
        XCTAssertThrowsError(try source.unlock(credentialID: "key", using: denied)) {
            XCTAssertEqual($0 as? OpenSSHSourceKeyError, .passphraseRequired)
        }
        for passphrase in [Data(), Data(repeating: 1, count: 1025)] {
            XCTAssertThrowsError(try source.unlock(passphrase: passphrase, credentialID: "key", using: denied)) {
                XCTAssertEqual($0 as? FTError, .limit("source_key.passphrase"))
            }
        }
        XCTAssertThrowsError(try source.unlock(passphrase: Data([1]), credentialID: "", using: denied)) {
            XCTAssertNotNil($0 as? FTError)
        }
        let clear = try OpenSSHSourceKey(pem: Data(fixture.clearPEM.utf8))
        XCTAssertThrowsError(try clear.unlock(passphrase: Data([1]), credentialID: "key", using: denied)) {
            XCTAssertEqual($0 as? FTError, .malformed("source_key.unexpected_passphrase"))
        }
    }

    func testSourcePreflightRejectsUnsupportedOrAmbiguousHeaders() async throws {
        let fixture = try SourceKeyFixtures.load().pairs[0]
        let original = try SourceKeyWire(pem: Data(fixture.encryptedPEM.utf8))
        var variants: [SourceKeyWire] = []
        var value = original; value.cipher = Data("aes256-cbc".utf8); variants.append(value)
        value = original; value.kdf = Data("argon2".utf8); variants.append(value)
        value = original; value.cipher = Data("none".utf8); variants.append(value)
        value = original; value.keyCount = 0; variants.append(value)
        value = original; value.keyCount = 2; variants.append(value)
        value = original; value.publicBlob.append(0); variants.append(value)
        value = original; value.privatePart = Data(); variants.append(value)
        value = original; value.privatePart.removeLast(); variants.append(value)
        for saltLength in [0, 15, 65] {
            value = original; value.options = SourceKeyWire.ssh(Data(repeating: 1, count: saltLength))
            value.options.appendBE(4, bytes: 4); variants.append(value)
        }
        for rounds in [UInt32(0), 65, UInt32.max] {
            value = original; value.options = SourceKeyWire.ssh(Data(repeating: 1, count: 16))
            value.options.appendBE(UInt64(rounds), bytes: 4); variants.append(value)
        }
        value = original; value.options.append(0); variants.append(value)
        value = original; value.options.removeLast(); variants.append(value)
        value = original; value.publicBlob = SourceKeyWire.ssh(Data("ssh-rsa".utf8)); variants.append(value)
        value = original; value.publicBlob = SourceKeyWire.ssh(Data("ssh-ed25519".utf8)) + SourceKeyWire.ssh(Data(repeating: 0, count: 31)); variants.append(value)
        for (index, invalid) in variants.enumerated() {
            XCTAssertThrowsError(try OpenSSHSourceKey(pem: invalid.pem()), "header variant \(index)") { XCTAssertNotNil($0 as? FTError) }
        }
        var trailing = original.encoded(); trailing.append(0)
        XCTAssertThrowsError(try OpenSSHSourceKey(pem: SourceKeyWire.pem(trailing)))
        for invalid in [Data(), Data(repeating: 65, count: FTLimit.key + 1), Data("not a PEM key".utf8),
                        Data((fixture.encryptedPEM + "trailing data").utf8)] {
            XCTAssertThrowsError(try OpenSSHSourceKey(pem: invalid))
        }
    }

    func testSourcePreflightAdmitsBothSaltBoundariesAndMaximumRounds() async throws {
        let fixture = try SourceKeyFixtures.load().pairs[0]
        let original = try SourceKeyWire(pem: Data(fixture.encryptedPEM.utf8))
        for count in [16, 64] {
            var changed = original
            changed.options = SourceKeyWire.ssh(Data(repeating: 7, count: count)); changed.options.appendBE(64, bytes: 4)
            let source = try OpenSSHSourceKey(pem: changed.pem())
            XCTAssertEqual(source.bcryptRounds, 64)
            XCTAssertEqual(source.algorithm, fixture.algorithm)
        }
    }

    func testUnlockerRetiresInvalidSelectionsAndBindsEachRequestToOneIdentity() async throws {
        let fixtures = try SourceKeyFixtures.load(), first = fixtures.pairs[0], second = fixtures.pairs[4]
        let reference = try workspace(for: first).credentials[0]
        var unlocker = OpenSSHSourceKeyUnlocker(using: DeniedSourceCryptography())
        let old = try request(unlocker.select(pem: Data(first.encryptedPEM.utf8), reference: reference))
        XCTAssertEqual(old.attemptsRemaining, 5)
        XCTAssertThrowsError(try unlocker.select(pem: Data(second.encryptedPEM.utf8), reference: reference)) {
            XCTAssertEqual($0 as? OpenSSHSourceKeyError, .identityMismatch)
        }
        XCTAssertNil(unlocker.request)
        XCTAssertThrowsError(try unlocker.unlock(requestID: old.id, passphrase: Data(fixtures.passphrase.utf8))) {
            XCTAssertEqual($0 as? FTError, .stale)
        }
        _ = try unlocker.select(pem: Data(first.encryptedPEM.utf8), reference: reference)
        XCTAssertThrowsError(try unlocker.select(pem: Data("invalid".utf8), reference: reference))
        XCTAssertNil(unlocker.request)
        let secondReference = try workspace(for: second).credentials[0]
        let latest = try request(unlocker.select(pem: Data(second.encryptedPEM.utf8), reference: secondReference))
        XCTAssertNotEqual(latest.id, old.id)
        XCTAssertThrowsError(try unlocker.cancel(requestID: old.id)) { XCTAssertEqual($0 as? FTError, .stale) }
        XCTAssertEqual(unlocker.request?.id, latest.id)
        XCTAssertThrowsError(try unlocker.unlock(requestID: old.id, passphrase: Data([1]))) { XCTAssertEqual($0 as? FTError, .stale) }
        XCTAssertEqual(unlocker.request?.attemptsRemaining, 5)
        try unlocker.cancel(requestID: latest.id)
        XCTAssertNil(unlocker.request)
    }

    func testUnlockerBoundsAttemptsWithoutChargingInvalidPassphraseLengths() async throws {
        let fixtures = try SourceKeyFixtures.load(), fixture = fixtures.pairs[0]
        var unlocker = OpenSSHSourceKeyUnlocker(using: crypto)
        let selected = try request(unlocker.select(pem: Data(fixture.encryptedPEM.utf8), reference: workspace(for: fixture).credentials[0]))
        for passphrase in [Data(), Data(repeating: 1, count: 1025)] {
            XCTAssertThrowsError(try unlocker.unlock(requestID: selected.id, passphrase: passphrase)) {
                XCTAssertEqual($0 as? FTError, .limit("source_key.passphrase"))
            }
        }
        XCTAssertEqual(unlocker.request?.attemptsRemaining, 5)
        for attempt in 1...5 {
            XCTAssertThrowsError(try unlocker.unlock(requestID: selected.id, passphrase: Data("wrong".utf8))) {
                XCTAssertEqual($0 as? OpenSSHSourceKeyError, attempt == 5 ? .attemptsExhausted : .incorrectPassphraseOrDamagedKey)
            }
            if attempt < 5 { XCTAssertEqual(unlocker.request?.attemptsRemaining, 5 - attempt) }
        }
        XCTAssertNil(unlocker.request)
        XCTAssertThrowsError(try unlocker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))) {
            XCTAssertEqual($0 as? FTError, .stale)
        }
    }

    func testSuccessfulUnlockRetiresRequestAndASelectedClearKeyNeedsNoPrompt() async throws {
        let fixtures = try SourceKeyFixtures.load(), fixture = fixtures.pairs[0]
        let reference = try workspace(for: fixture).credentials[0]
        var unlocker = OpenSSHSourceKeyUnlocker(using: crypto)
        let selected = try request(unlocker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference))
        let key = try unlocker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))
        XCTAssertEqual(key.publicBlob, try OpenSSHSourceKey(pem: Data(fixture.clearPEM.utf8)).publicBlob)
        XCTAssertNil(unlocker.request)
        XCTAssertThrowsError(try unlocker.unlock(requestID: selected.id, passphrase: Data(fixtures.passphrase.utf8))) {
            XCTAssertEqual($0 as? FTError, .stale)
        }
        guard case .ready(let clear) = try unlocker.select(pem: Data(fixture.clearPEM.utf8), reference: reference) else {
            return XCTFail("clear source unexpectedly requested a passphrase")
        }
        XCTAssertEqual(clear.publicBlob, key.publicBlob)
        XCTAssertNil(unlocker.request)
    }

    func testCancelledWorkNeverReachesNativeCryptoAndRetiresPendingRequest() async throws {
        let fixture = try SourceKeyFixtures.load().pairs[0]
        let source = try OpenSSHSourceKey(pem: Data(fixture.encryptedPEM.utf8))
        let reference = try workspace(for: fixture).credentials[0]
        let direct = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            do { _ = try source.unlock(passphrase: Data([1]), credentialID: "key", using: DeniedSourceCryptography()); return false }
            catch is CancellationError { return true }
            catch { return false }
        }
        let directCancelled = await direct.value
        XCTAssertTrue(directCancelled)
        let coordinated = Task {
            var unlocker = OpenSSHSourceKeyUnlocker(using: DeniedSourceCryptography())
            _ = try unlocker.select(pem: Data(fixture.encryptedPEM.utf8), reference: reference)
            let selected = try XCTUnwrap(unlocker.request)
            withUnsafeCurrentTask { $0?.cancel() }
            do { _ = try unlocker.unlock(requestID: selected.id, passphrase: Data([1])); return false }
            catch is CancellationError { return unlocker.request == nil }
        }
        let coordinatedCancelled = try await coordinated.value
        XCTAssertTrue(coordinatedCancelled)
    }

    func testAESCTRMatchesIndependentNISTVectorsForBothAdmittedKeySizes() async throws {
        // NIST SP 800-38A Appendix F.5, first two blocks of CTR-AES128/256.
        let clear = try Data(ftHex: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51")
        let iv = try Data(ftHex: "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff")
        for (key, cipher) in [
            ("2b7e151628aed2a6abf7158809cf4f3c", "874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff"),
            ("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4", "601ec313775789a5b7a7f504bbf3d228f443e3ca4d62b59aca84e990cacaf5c5")
        ] {
            XCTAssertEqual(try crypto.decryptAESCTR(Data(ftHex: cipher), key: Data(ftHex: key), iv: iv), clear)
        }
    }

    func testBcryptLayoutsMatchIndependentPublishedOpenBSDVector() async throws {
        // OpenBSD regress/lib/libutil/bcrypt_pbkdf/bcrypt_pbkdf_test.c, blob
        // ad3330bd9560358b952316df97f93d0f5184ceec. The published 256-byte result
        // interleaves eight blocks. Column 0 gives output32; the first 24 rows
        // of columns 0/1 give output48. These are not production-generated goldens.
        let pass = try Data(ftHex: "0db3ac94b3ee53284f4a22893b3c24ae")
        let salt = try Data(ftHex: "3a62f0f0dbcef823cfcc854856ea1028")
        for (count, expected) in [
            (32, "204438175eee7ce136c91b49a67923ff9988c989613180333f97684bc78093e8"),
            (48, "2054440338de170a5ea4ee237c86e1dc368fc9e71bbf490ea678792e238eff4499708873c9bf89a7616731a1800133b4")
        ] {
            XCTAssertEqual(try crypto.bcryptPBKDF(passphrase: pass, salt: salt, rounds: 8, outputByteCount: count), try Data(ftHex: expected))
        }
    }

    func testPrimitiveBoundsRejectExpensiveOrUnsupportedWork() async throws {
        let salt = Data(repeating: 1, count: 16), pass = Data([1])
        for rounds in [UInt32(0), 65, UInt32.max] {
            XCTAssertThrowsError(try crypto.bcryptPBKDF(passphrase: pass, salt: salt, rounds: rounds, outputByteCount: 48)) {
                XCTAssertEqual($0 as? FTError, .limit("key.source_kdf"))
            }
        }
        for count in [-1, 0, 16, 33, 64, Int.max] {
            XCTAssertThrowsError(try crypto.bcryptPBKDF(passphrase: pass, salt: salt, rounds: 1, outputByteCount: count))
        }
        for count in [0, 15, 65] {
            XCTAssertThrowsError(try crypto.bcryptPBKDF(passphrase: pass, salt: Data(repeating: 0, count: count), rounds: 1, outputByteCount: 32))
        }
        for password in [Data(), Data(repeating: 0, count: 1025)] {
            XCTAssertThrowsError(try crypto.bcryptPBKDF(passphrase: password, salt: salt, rounds: 1, outputByteCount: 32))
        }
        for count in [0, 15, 17, FTLimit.key + 16] {
            XCTAssertThrowsError(try crypto.decryptAESCTR(Data(repeating: 0, count: count), key: Data(repeating: 0, count: 32), iv: salt))
        }
        XCTAssertThrowsError(try crypto.decryptAESCTR(salt, key: Data(repeating: 0, count: 24), iv: salt))
        XCTAssertThrowsError(try crypto.decryptAESCTR(salt, key: salt, iv: Data(repeating: 0, count: 15)))
    }

    private func request<C: AirGapCryptography>(_ selection: OpenSSHSourceKeyUnlocker<C>.Selection) throws -> OpenSSHSourceKeyUnlocker<C>.Request {
        guard case .passphraseRequired(let request) = selection else { throw FTError.malformed("test.expected_prompt") }
        return request
    }

    private func workspace(for fixture: SourceKeyFixtures.Pair) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fixture.fingerprint)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        credentials[0]["algorithm"] = fixture.algorithm; workspace["credentials"] = credentials; root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root))
    }

    private func identityDigest(_ container: Data) throws -> Data {
        var reader = ByteCursor(try SourceKeyWire(container: container).privatePart)
        _ = try reader.take(8)
        let algorithm = try reader.sshString()
        for _ in 0..<(algorithm == Data("ssh-ed25519".utf8) ? 2 : 3) { _ = try reader.sshString() }
        _ = try reader.sshString() // preserve the source comment without logging it
        return crypto.sha256(reader.data.subdata(in: 8..<reader.offset))
    }
}

private struct SourceKeyFixtures: Decodable, Sendable {
    struct Pair: Decodable, Sendable {
        let id: String, algorithm: String, cipher: String, fingerprint: String, encryptedPEM: String, clearPEM: String
        let rounds: UInt32
    }
    struct Padding: Decodable, Sendable {
        let id: String, algorithm: String, fingerprint: String, encryptedPEM: String
        let paddingBytes: Int
    }
    let passphrase: String
    let pairs: [Pair]
    let paddingCases: [Padding]
    static func load() throws -> Self {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Self.self, from: Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/openssh-source-keys.json")))
    }
}

private struct SourceKeyWire {
    var cipher: Data, kdf: Data, options: Data, publicBlob: Data, privatePart: Data
    var keyCount: UInt64
    init(pem: Data) throws {
        let lines = String(decoding: pem, as: UTF8.self).split(whereSeparator: \.isNewline)
        guard let data = Data(base64Encoded: lines.dropFirst().dropLast().joined()) else { throw FTError.malformed("test.pem") }
        try self.init(container: data)
    }
    init(container: Data) throws {
        var reader = ByteCursor(container)
        _ = try reader.take(15)
        cipher = try reader.sshString(); kdf = try reader.sshString(); options = try reader.sshString()
        keyCount = try reader.uint(4); publicBlob = try reader.sshString(); privatePart = try reader.sshString()
        guard reader.remaining == 0 else { throw FTError.malformed("test.extent") }
    }
    static func ssh(_ bytes: Data) -> Data {
        var result = Data(); result.appendBE(UInt64(bytes.count), bytes: 4); result += bytes; return result
    }
    func encoded() -> Data {
        var result = Data("openssh-key-v1\0".utf8)
        for field in [cipher, kdf, options] { result += Self.ssh(field) }
        result.appendBE(keyCount, bytes: 4)
        result += Self.ssh(publicBlob); result += Self.ssh(privatePart)
        return result
    }
    func pem() -> Data { Self.pem(encoded()) }
    static func pem(_ container: Data) -> Data {
        Data(("-----BEGIN OPENSSH PRIVATE KEY-----\n" + container.base64EncodedString() + "\n-----END OPENSSH PRIVATE KEY-----\n").utf8)
    }
}

private enum SourceProbeError: Error { case unexpectedNativeWork }
private struct DeniedSourceCryptography: AirGapCryptography {
    let base = TestCryptography()
    func sha256(_ bytes: Data) -> Data { base.sha256(bytes) }
    func randomBytes(count: Int) throws -> Data { try base.randomBytes(count: count) }
    func seal(_ plaintext: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { try base.seal(plaintext, secret: secret, salt: salt, nonce: nonce, aad: aad) }
    func open(_ ciphertextAndTag: Data, secret: Data, salt: Data, nonce: Data, aad: Data) throws -> Data { try base.open(ciphertextAndTag, secret: secret, salt: salt, nonce: nonce, aad: aad) }
    func ed25519PublicKey(seed: Data) throws -> Data { try base.ed25519PublicKey(seed: seed) }
    func ecdsaPublicKey(scalar: Data, curve: SSHECDSACurve) throws -> Data { try base.ecdsaPublicKey(scalar: scalar, curve: curve) }
    func bcryptPBKDF(passphrase: Data, salt: Data, rounds: UInt32, outputByteCount: Int) throws -> Data { throw SourceProbeError.unexpectedNativeWork }
    func decryptAESCTR(_ ciphertext: Data, key: Data, iv: Data) throws -> Data { throw SourceProbeError.unexpectedNativeWork }
}
