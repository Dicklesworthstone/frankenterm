#if canImport(CryptoKit) && canImport(Darwin)
import Foundation
import XCTest
@testable import FrankenTermKit

/// These tests execute CryptoKit SHA-512/key derivation and CommonCrypto AES on
/// Apple platforms. A Linux run excludes this file and does not qualify them.
/// The portable source tests separately cover malformed files and retry policy.
@MainActor
final class AppleOpenSSHSourceKeyTests: XCTestCase {
    private struct FixtureFile: Decodable, Sendable {
        let passphrase: String
        let pairs: [Pair]
    }
    private struct Pair: Decodable, Sendable {
        let id: String, algorithm: String, cipher: String, fingerprint: String
        let rounds: UInt32
        let encryptedPEM: String, clearPEM: String
    }

    private func fixtures() throws -> FixtureFile {
        let repository = (0..<6).reduce(URL(fileURLWithPath: #filePath)) { result, _ in
            result.deletingLastPathComponent()
        }
        return try JSONDecoder().decode(FixtureFile.self, from: Data(contentsOf:
            repository.appendingPathComponent("fixtures/ios-airgap/openssh-source-keys.json")))
    }

    /// ssh-keygen changes random checkints when it saves the matching clear key.
    /// Compare every private identity field and comment, excluding only those
    /// checkints and the container's already validated alignment padding.
    private func privateFields(_ key: PrivateKeyRecord) throws -> [Data] {
        var outer = ByteCursor(key.container)
        _ = try outer.take(15)
        for _ in 0..<3 { _ = try outer.sshString() }
        _ = try outer.uint(4)
        _ = try outer.sshString()
        var inner = ByteCursor(try outer.sshString())
        _ = try inner.take(8)
        let algorithm = try inner.sshString(cap: 64)
        let additionalFields: Int
        if algorithm == Data("ssh-ed25519".utf8) {
            additionalFields = 3
        } else if SSHECDSACurve.allCases.contains(where: { algorithm == Data($0.algorithm.utf8) }) {
            additionalFields = 4
        } else { throw FTError.unsupported("test.source_key_family") }
        var result = [algorithm]
        for _ in 0..<additionalFields { result.append(try inner.sshString()) }
        return result
    }

    func testNativeBcryptPublishedVectorsRemainIndependentAcrossConcurrentCalls() async throws {
        // OpenBSD bcrypt_pbkdf_test.c, blob ad3330bd9560358b952316df97f93d0f5184ceec:
        // the published 256-byte vector has stride eight. Column zero is its
        // 32-byte result; interleaved columns zero/one (24 bytes each) give 48.
        // This preserves production admission bounds without regenerating a
        // golden value using the implementation under test.
        let passphrase = try Data(ftHex: "0db3ac94b3ee53284f4a22893b3c24ae")
        let salt = try Data(ftHex: "3a62f0f0dbcef823cfcc854856ea1028")
        let expected = [
            32: try Data(ftHex: "204438175eee7ce136c91b49a67923ff9988c989613180333f97684bc78093e8"),
            48: try Data(ftHex: "2054440338de170a5ea4ee237c86e1dc368fc9e71bbf490ea678792e238eff4499708873c9bf89a7616731a1800133b4")
        ]
        try await withThrowingTaskGroup(of: (Int, Data).self) { group in
            for count in [32, 48, 48, 32] {
                group.addTask {
                    let result = try AppleCryptography().bcryptPBKDF(passphrase: passphrase, salt: salt,
                                                                   rounds: 8, outputByteCount: count)
                    return (count, result)
                }
            }
            var completed = 0
            for try await (count, result) in group {
                XCTAssertEqual(result, expected[count])
                completed += 1
            }
            XCTAssertEqual(completed, 4)
        }
    }

    func testCommonCryptoUsesNISTBigEndianCounterModeForBothKeySizes() async throws {
        // NIST SP 800-38A, F.5.1 and F.5.5. The second block crosses the initial
        // IV's low-byte boundary, detecting the wrong counter increment order.
        let iv = try Data(ftHex: "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff")
        let clear = try Data(ftHex: "6bc1bee22e409f96e93d7e117393172aae2d8a571e03ac9c9eb76fac45af8e51")
        let vectors = [
            ("2b7e151628aed2a6abf7158809cf4f3c", "874d6191b620e3261bef6864990db6ce9806f66b7970fdff8617187bb9fffdff"),
            ("603deb1015ca71be2b73aef0857d77811f352c073b6108d72d9810a30914dff4", "601ec313775789a5b7a7f504bbf3d228f443e3ca4d62b59aca84e990cacaf5c5")
        ]
        for (key, ciphertext) in vectors {
            XCTAssertEqual(try AppleCryptography().decryptAESCTR(Data(ftHex: ciphertext), key: Data(ftHex: key), iv: iv), clear)
        }
    }

    func testConcurrentNativeUnlocksPreserveEveryIndependentOpenSSHPrivateIdentity() async throws {
        let fixture = try fixtures(), passphrase = Data(fixture.passphrase.utf8)
        XCTAssertEqual(Set(fixture.pairs.map(\.algorithm)), Set(["ssh-ed25519"] + SSHECDSACurve.allCases.map(\.algorithm)))
        XCTAssertEqual(Set(fixture.pairs.map(\.cipher)), Set(["aes128-ctr", "aes256-ctr"]))
        XCTAssertTrue(fixture.pairs.contains(where: { $0.rounds == OpenSSHSourceKey.maximumBcryptRounds }))
        try await withThrowingTaskGroup(of: (Int, PrivateKeyRecord).self) { group in
            for (index, pair) in fixture.pairs.enumerated() {
                group.addTask {
                    let source = try OpenSSHSourceKey(pem: Data(pair.encryptedPEM.utf8))
                    let key = try source.unlock(passphrase: passphrase, credentialID: "key", using: AppleCryptography())
                    return (index, key)
                }
            }
            var completed = 0
            for try await (index, key) in group {
                let pair = fixture.pairs[index]
                let expected = try PrivateKeyRecord.fromPEM(Data(pair.clearPEM.utf8), credentialID: "key", using: AppleCryptography())
                XCTAssertEqual(key.publicBlob, expected.publicBlob, pair.id)
                XCTAssertEqual(try privateFields(key), try privateFields(expected), pair.id)
                let fingerprint = "SHA256:" + AppleCryptography().sha256(key.publicBlob)
                    .base64EncodedString().replacingOccurrences(of: "=", with: "")
                XCTAssertEqual(fingerprint, pair.fingerprint, pair.id)
                XCTAssertEqual(try PrivateKeyRecord.validateContainer(key.container, using: AppleCryptography()), key.publicBlob)
                completed += 1
            }
            XCTAssertEqual(completed, fixture.pairs.count)
        }
    }

    func testCancelledNativeUnlockDoesNotPoisonAnIndependentActiveOperation() async throws {
        let fixture = try fixtures(), passphrase = Data(fixture.passphrase.utf8)
        let pair = try XCTUnwrap(fixture.pairs.first(where: { $0.id == "ed25519-aes128" }))
        let source = try OpenSSHSourceKey(pem: Data(pair.encryptedPEM.utf8))
        let cancelled = Task.detached {
            // Deterministic cancellation before bounded synchronous native work;
            // this does not claim that a bcrypt round can be interrupted.
            withUnsafeCurrentTask { $0?.cancel() }
            do {
                _ = try source.unlock(passphrase: passphrase, credentialID: "key", using: AppleCryptography())
                return false
            } catch is CancellationError { return true }
            catch { return false }
        }
        let active = Task.detached {
            try source.unlock(passphrase: passphrase, credentialID: "key", using: AppleCryptography())
        }
        defer { cancelled.cancel(); active.cancel() }
        let cancelledWithoutResult = await cancelled.value
        XCTAssertTrue(cancelledWithoutResult)
        let key = try await active.value
        let expected = try PrivateKeyRecord.fromPEM(Data(pair.clearPEM.utf8), credentialID: "key", using: AppleCryptography())
        XCTAssertEqual(key.publicBlob, expected.publicBlob)
        XCTAssertEqual(try privateFields(key), try privateFields(expected))
    }
}
#endif
