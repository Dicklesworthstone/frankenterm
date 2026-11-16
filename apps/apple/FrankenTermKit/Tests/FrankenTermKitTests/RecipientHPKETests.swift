import Foundation
import XCTest
@testable import FrankenTermKit

@MainActor
final class RecipientHPKETests: XCTestCase {
    func testExactRFC9180BaseSuiteVectorOpensWithThePublishedPrivateKey() async throws {
        let vector = try RecipientHPKEFixtures.load().rfc9180
        let privateKey = try Data(ftHex: vector.privateKey)
        XCTAssertEqual(try RecipientHPKE.publicKey(forPrivateKey: privateKey), try Data(ftHex: vector.publicKey))
        let clear = try RecipientHPKE.open(Data(ftHex: vector.ciphertext), privateKey: privateKey,
            encapsulatedKey: Data(ftHex: vector.encapsulatedKey), info: Data(ftHex: vector.info), authenticating: Data(ftHex: vector.aad))
        XCTAssertEqual(clear, try Data(ftHex: vector.plaintext))
    }

    func testEachSealGetsAFreshContextAndAuthenticatesTheEncapsulationInItsHeader() async throws {
        let privateKey = try RecipientHPKE.generatePrivateKey()
        let publicKey = try RecipientHPKE.publicKey(forPrivateKey: privateKey)
        let clear = Data("public synthetic recipient-mode payload".utf8), info = Data("test recipient context".utf8)
        var encapsulations = Set<Data>(), ciphertexts = Set<Data>()
        for _ in 0..<4 {
            var closureCalls = 0
            let sealed = try RecipientHPKE.seal(clear, recipientPublicKey: publicKey, info: info) { enc in
                closureCalls += 1
                XCTAssertEqual(enc.count, 32)
                return Data("complete-header".utf8) + enc
            }
            XCTAssertEqual(closureCalls, 1)
            XCTAssertEqual(sealed.ciphertext.count, clear.count + 16)
            XCTAssertEqual(try RecipientHPKE.open(sealed.ciphertext, privateKey: privateKey,
                encapsulatedKey: sealed.encapsulatedKey, info: info,
                authenticating: Data("complete-header".utf8) + sealed.encapsulatedKey), clear)
            encapsulations.insert(sealed.encapsulatedKey); ciphertexts.insert(sealed.ciphertext)
            XCTAssertEqual(String(reflecting: sealed), "RecipientHPKE.Sealed([encrypted])")
        }
        XCTAssertEqual(encapsulations.count, 4)
        XCTAssertEqual(ciphertexts.count, 4)
    }

    func testRecipientPrivateKeyInfoAADAndCiphertextAreAllRequired() async throws {
        let vector = try RecipientHPKEFixtures.load().rfc9180
        let ciphertext = try Data(ftHex: vector.ciphertext), privateKey = try Data(ftHex: vector.privateKey)
        let encapsulated = try Data(ftHex: vector.encapsulatedKey), info = try Data(ftHex: vector.info), aad = try Data(ftHex: vector.aad)
        var wrongPrivate = privateKey; wrongPrivate[16] ^= 1 // avoid X25519's clamped bits
        var wrongEncapsulation = encapsulated; wrongEncapsulation[16] ^= 1
        var wrongCiphertext = ciphertext; wrongCiphertext[wrongCiphertext.count - 1] ^= 1
        for (ct, key, enc, context, authentication) in [
            (ciphertext, wrongPrivate, encapsulated, info, aad),
            (ciphertext, privateKey, wrongEncapsulation, info, aad),
            (ciphertext, privateKey, encapsulated, info + Data([1]), aad),
            (ciphertext, privateKey, encapsulated, info, aad + Data([1])),
            (wrongCiphertext, privateKey, encapsulated, info, aad),
            (ciphertext + Data([0]), privateKey, encapsulated, info, aad)
        ] {
            XCTAssertThrowsError(try RecipientHPKE.open(ct, privateKey: key, encapsulatedKey: enc,
                info: context, authenticating: authentication)) { XCTAssertEqual($0 as? FTError, .authentication) }
        }
        // A recording of public request bytes is not a recipient private key.
        XCTAssertThrowsError(try RecipientHPKE.open(ciphertext, privateKey: Data(ftHex: vector.publicKey),
            encapsulatedKey: encapsulated, info: info, authenticating: aad))
    }

    func testLowOrderPublicKeysAndInvalidExtentsRejectBeforeAnAADCallback() async throws {
        let privateKey = try RecipientHPKE.generatePrivateKey()
        let publicKey = try RecipientHPKE.publicKey(forPrivateKey: privateKey)
        var callbackCalled = false
        // RFC 7748 masks the top bit and accepts noncanonical u-coordinates.
        // Test zero/one and the equivalent p-1/p/p+1 encodings on both paths;
        // production validation uses native agreement, not this fixture list.
        var lowOrder = [Data(repeating: 0, count: 32), Data([1]) + Data(repeating: 0, count: 31)]
        for first in [UInt8(0xEC), 0xED, 0xEE] {
            lowOrder.append(Data([first]) + Data(repeating: 0xFF, count: 30) + Data([0x7F]))
        }
        lowOrder += lowOrder.map { key in var alias = key; alias[31] |= 0x80; return alias }
        for invalid in [Data(), Data(repeating: 1, count: 31), Data(repeating: 1, count: 33)] + lowOrder {
            XCTAssertThrowsError(try RecipientHPKE.seal(Data([1]), recipientPublicKey: invalid, info: Data()) { _ in
                callbackCalled = true; return Data()
            })
        }
        XCTAssertFalse(callbackCalled)
        for invalid in lowOrder {
            XCTAssertThrowsError(try RecipientHPKE.open(Data(repeating: 0, count: 16), privateKey: privateKey,
                encapsulatedKey: invalid, info: Data(), authenticating: Data())) {
                XCTAssertEqual($0 as? FTError, .authentication)
            }
        }
        for invalid in [Data(), Data(repeating: 0, count: 31), Data(repeating: 0, count: 33)] {
            XCTAssertThrowsError(try RecipientHPKE.publicKey(forPrivateKey: invalid))
            XCTAssertThrowsError(try RecipientHPKE.open(Data(repeating: 0, count: 16), privateKey: privateKey,
                encapsulatedKey: invalid, info: Data(), authenticating: Data()))
        }
        XCTAssertThrowsError(try RecipientHPKE.seal(Data(repeating: 0, count: FTLimit.envelope), recipientPublicKey: publicKey, info: Data()) { _ in
            callbackCalled = true; return Data()
        })
        XCTAssertThrowsError(try RecipientHPKE.seal(Data(), recipientPublicKey: publicKey, info: Data(repeating: 0, count: 4097)) { _ in
            callbackCalled = true; return Data()
        })
        XCTAssertFalse(callbackCalled)
        XCTAssertThrowsError(try RecipientHPKE.seal(Data(), recipientPublicKey: publicKey, info: Data()) { _ in Data(repeating: 0, count: 4097) })
        for count in [0, 15, FTLimit.envelope + 1] {
            XCTAssertThrowsError(try RecipientHPKE.open(Data(repeating: 0, count: count), privateKey: privateKey,
                encapsulatedKey: publicKey, info: Data(), authenticating: Data()))
        }
    }

    func testAADClosureFailureAndCancellationCannotProduceASealedValue() async throws {
        let privateKey = try RecipientHPKE.generatePrivateKey(), publicKey = try RecipientHPKE.publicKey(forPrivateKey: privateKey)
        XCTAssertThrowsError(try RecipientHPKE.seal(Data([1]), recipientPublicKey: publicKey, info: Data()) { _ in
            throw RecipientTestError.cancelledHeaderConstruction
        }) { XCTAssertEqual($0 as? RecipientTestError, .cancelledHeaderConstruction) }
        let cancelled = Task.detached {
            withUnsafeCurrentTask { $0?.cancel() }
            do {
                _ = try RecipientHPKE.seal(Data([1]), recipientPublicKey: publicKey, info: Data()) { _ in
                    throw RecipientTestError.nativeWorkReached
                }
                return false
            } catch is CancellationError { return true }
            catch { return false }
        }
        let wasCancelled = await cancelled.value
        XCTAssertTrue(wasCancelled)
        let afterHeader = Task.detached {
            do {
                _ = try RecipientHPKE.seal(Data([1]), recipientPublicKey: publicKey, info: Data()) { _ in
                    withUnsafeCurrentTask { $0?.cancel() }
                    return Data()
                }
                return false
            } catch is CancellationError { return true }
            catch { return false }
        }
        let cancelledAfterHeader = await afterHeader.value
        XCTAssertTrue(cancelledAfterHeader)
    }

    func testEmptyMessageIsStillAuthenticatedAndIndependentContextsRunConcurrently() async throws {
        let privateKey = try RecipientHPKE.generatePrivateKey(), publicKey = try RecipientHPKE.publicKey(forPrivateKey: privateKey)
        try await withThrowingTaskGroup(of: Data.self) { group in
            for index in 0..<8 {
                group.addTask {
                    let info = Data([UInt8(index)])
                    let sealed = try RecipientHPKE.seal(Data(), recipientPublicKey: publicKey, info: info) { $0 }
                    guard sealed.ciphertext.count == 16 else { throw RecipientTestError.invalidExtent }
                    let clear = try RecipientHPKE.open(sealed.ciphertext, privateKey: privateKey,
                        encapsulatedKey: sealed.encapsulatedKey, info: info, authenticating: sealed.encapsulatedKey)
                    guard clear.isEmpty else { throw RecipientTestError.invalidExtent }
                    return sealed.encapsulatedKey
                }
            }
            var encapsulations = Set<Data>()
            for try await enc in group { encapsulations.insert(enc) }
            XCTAssertEqual(encapsulations.count, 8)
        }
    }

    func testSwiftSenderEmitsAnIndependentOracleFixture() async throws {
        let vector = try RecipientHPKEFixtures.load().rfc9180
        let privateKey = try Data(ftHex: vector.privateKey), publicKey = try Data(ftHex: vector.publicKey)
        let plaintext = Data("FrankenTerm Swift HPKE sender cross-platform fixture".utf8)
        let info = Data("FrankenTerm independent HPKE oracle".utf8)
        let sealed = try RecipientHPKE.seal(plaintext, recipientPublicKey: publicKey, info: info) { Data("full-header\0".utf8) + $0 }
        let aad = Data("full-header\0".utf8) + sealed.encapsulatedKey
        XCTAssertEqual(try RecipientHPKE.open(sealed.ciphertext, privateKey: privateKey,
            encapsulatedKey: sealed.encapsulatedKey, info: info, authenticating: aad), plaintext)
        if let path = ProcessInfo.processInfo.environment["FT_HPKE_VALIDATION_DIRECTORY"] {
            let output = URL(fileURLWithPath: path, isDirectory: true)
            try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
            let values: [String: String] = ["privateKey": privateKey.ftHex, "publicKey": publicKey.ftHex,
                "encapsulatedKey": sealed.encapsulatedKey.ftHex, "info": info.ftHex, "aad": aad.ftHex,
                "ciphertext": sealed.ciphertext.ftHex, "plaintext": plaintext.ftHex]
            try JSONSerialization.data(withJSONObject: values, options: [.sortedKeys, .prettyPrinted])
                .write(to: output.appendingPathComponent("swift-hpke-sender.json"), options: .withoutOverwriting)
        }
    }
}

private enum RecipientTestError: Error, Equatable {
    case cancelledHeaderConstruction, nativeWorkReached, invalidExtent
}

struct RecipientHPKEFixtures: Decodable, Sendable {
    struct RFCVector: Decodable, Sendable {
        let privateKey: String, publicKey: String, encapsulatedKey: String, info: String, aad: String, plaintext: String, ciphertext: String
    }
    struct Envelope: Decodable, Sendable {
        let id: String, requestCBOR: String, requestDigest: String, privateKey: String, publicKey: String
        let archive: String, manifest: String, envelope: String, envelopeSHA256: String
    }
    let rfc9180: RFCVector
    let envelopes: [Envelope]
    static func load() throws -> Self {
        var root = URL(fileURLWithPath: #filePath)
        for _ in 0..<6 { root.deleteLastPathComponent() }
        return try JSONDecoder().decode(Self.self, from: Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/recipient-hpke-vectors.json")))
    }
}
