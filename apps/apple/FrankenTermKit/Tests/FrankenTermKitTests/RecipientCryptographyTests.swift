import Foundation
import XCTest
@testable import FrankenTermKit

/// Published synthetic material from RFC 9180 Appendix A.2.1, Base mode with
/// KEM 0x0020 / KDF 0x0001 / AEAD 0x0003. Never use these keys outside tests.
/// https://www.rfc-editor.org/rfc/rfc9180.html#appendix-A.2.1
/// Machine-readable source pinned by the RFC:
/// https://github.com/cfrg/draft-irtf-cfrg-hpke/blob/5f503c564da00b0687b3de75f1dfbdfc4079ad31/test-vectors.json
private struct RFC9180BaseVector {
    let info = try! Data(ftHex: "4f6465206f6e2061204772656369616e2055726e")
    let recipientPrivate = try! Data(ftHex: "8057991eef8f1f1af18f4a9491d16a1ce333f695d4db8e38da75975c4478e0fb")
    let recipientPublic = try! Data(ftHex: "4310ee97d88cc1f088a5576c77ab0cf5c3ac797f3d95139c6c84b5429c59662a")
    let ephemeralPrivate = try! Data(ftHex: "f4ec9b33b792c372c1d2c2063507b684ef925b8c75a42dbcbf57d63ccd381600")
    let enc = try! Data(ftHex: "1afa08d3dec047a643885163f1180476fa7ddb54c6a8029ea33f95796bf2ac4a")
    let plaintext = try! Data(ftHex: "4265617574792069732074727574682c20747275746820626561757479")
    let aad = try! Data(ftHex: "436f756e742d30")
    let ciphertext = try! Data(ftHex: "1c5250d8034ec2b784ba2cfd69dbdb8af406cfe3ff938e131f0def8c8b60b4db21993c62ce81883d2dd1b51a28")
}

final class RecipientCryptographyTests: XCTestCase {
    let crypto = TestCryptography()

    func testRFC9180PublishedRecipientVector() throws {
        let vector = RFC9180BaseVector()
        XCTAssertEqual(try crypto.x25519PublicKey(privateKey: vector.recipientPrivate), vector.recipientPublic)
        XCTAssertEqual(try crypto.x25519PublicKey(privateKey: vector.ephemeralPrivate), vector.enc)
        XCTAssertEqual(try crypto.hpkeOpen(vector.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                           encapsulatedKey: vector.enc, info: vector.info, aad: vector.aad), vector.plaintext)
    }

    func testIndependentPythonFTAGRecipientEnvelopeFixture() throws {
        let vector = RFC9180BaseVector()
        let requestID = Data(0..<16), transferID = Data((240..<256).map(UInt8.init))
        let session = try RecipientSession(privateKey: vector.recipientPrivate, requestID: requestID, using: crypto)
        let envelope = try XCTUnwrap(Data(base64Encoded: FTAGRecipientVector.envelope, options: .ignoreUnknownCharacters))
        let archive = Data("FrankenTerm independent HPKE FTAG v1 fixture".utf8)
        XCTAssertEqual(session.request.encoded, try Data(ftHex: "840150000102030405060708090a0b0c0d0e0f58204310ee97d88cc1f088a5576c77ab0cf5c3ac797f3d95139c6c84b5429c59662a1a00400000"))
        XCTAssertEqual(session.request.digest(using: crypto), try Data(ftHex: "627055e3e6f0802da069361cc0612ad4801158f1598b8bee6ac2c8dcf4476e91"))
        XCTAssertEqual(envelope.count, 4222)
        XCTAssertEqual(crypto.sha256(envelope), try Data(ftHex: "d02c7daa782749e277a2367811f2dc9979fb895b332e719c746bb34f6e8defd9"))
        let header = try FTAGHeader(envelope: envelope)
        XCTAssertEqual(header.mode, .recipient)
        XCTAssertEqual(header.transferID, transferID)
        XCTAssertEqual(header.header.count, 110)
        XCTAssertEqual(header.ciphertextLength, 4112)
        XCTAssertEqual(header.modeHeader, requestID + session.request.digest(using: crypto) + vector.enc)
        XCTAssertEqual(try FTAG.openArchiveBytes(envelope, recipient: session, using: crypto), archive)
        #if !canImport(CryptoKit)
        let sealed = try crypto.hpkeSealDeterministically(FTAG.pack(archive, compress: false, mode: .recipient),
                                                        recipientPublicKey: vector.recipientPublic,
                                                        ephemeralPrivateKey: vector.ephemeralPrivate,
                                                        info: session.request.info(using: crypto), aad: header.aad)
        XCTAssertEqual(sealed.encapsulatedKey, vector.enc)
        XCTAssertEqual(header.header + sealed.ciphertext, envelope)
        #endif
    }

    #if !canImport(CryptoKit)
    func testIndependentOpenSSLKeyScheduleAndDeterministicSenderMatchRFC9180() throws {
        let vector = RFC9180BaseVector()
        let schedule = try crypto.hpkeKeySchedule(privateKey: vector.ephemeralPrivate, peerPublicKey: vector.recipientPublic,
                                                  encapsulatedKey: vector.enc, recipientPublicKey: vector.recipientPublic, info: vector.info)
        XCTAssertEqual(schedule.sharedSecret, try Data(ftHex: "0bbe78490412b4bbea4812666f7916932b828bba79942424abb65244930d69a7"))
        XCTAssertEqual(schedule.context, try Data(ftHex: "00431df6cd95e11ff49d7013563baf7f11588c75a6611ee2a4404a49306ae4cfc5b69c5718a60cc5876c358d3f7fc31ddb598503f67be58ea1e798c0bb19eb9796"))
        XCTAssertEqual(schedule.secret, try Data(ftHex: "5b9cd775e64b437a2335cf499361b2e0d5e444d5cb41a8a53336d8fe402282c6"))
        XCTAssertEqual(schedule.key, try Data(ftHex: "ad2744de8e17f4ebba575b3f5f5a8fa1f69c2a07f6e7500bc60ca6e3e3ec1c91"))
        XCTAssertEqual(schedule.baseNonce, try Data(ftHex: "5c4d98150661b848853b547f"))
        XCTAssertEqual(schedule.exporterSecret, try Data(ftHex: "a3b010d4994890e2c6968a36f64470d3c824c8f5029942feb11e7a74b2921922"))
        let sealed = try crypto.hpkeSealDeterministically(vector.plaintext, recipientPublicKey: vector.recipientPublic,
                                                        ephemeralPrivateKey: vector.ephemeralPrivate, info: vector.info, aad: vector.aad)
        XCTAssertEqual(sealed.encapsulatedKey, vector.enc)
        XCTAssertEqual(sealed.ciphertext, vector.ciphertext)
    }
    #endif

    func testFreshSendersBindEncapsulatedKeyAndCompleteAAD() throws {
        let vector = RFC9180BaseVector(), prefix = Data("FrankenTerm/FTAG/v1/envelope\0header".utf8)
        let first = try crypto.hpkeSeal(vector.plaintext, recipientPublicKey: vector.recipientPublic, info: vector.info, aadPrefix: prefix)
        let second = try crypto.hpkeSeal(vector.plaintext, recipientPublicKey: vector.recipientPublic, info: vector.info, aadPrefix: prefix)
        XCTAssertEqual(first.encapsulatedKey.count, 32)
        XCTAssertEqual(first.ciphertext.count, vector.plaintext.count + 16)
        XCTAssertNotEqual(first.encapsulatedKey, second.encapsulatedKey)
        XCTAssertNotEqual(first.ciphertext, second.ciphertext)
        XCTAssertEqual(try crypto.hpkeOpen(first.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                           encapsulatedKey: first.encapsulatedKey, info: vector.info,
                                           aad: prefix + first.encapsulatedKey), vector.plaintext)
        XCTAssertThrowsError(try crypto.hpkeOpen(first.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                                 encapsulatedKey: first.encapsulatedKey, info: vector.info, aad: prefix))
        XCTAssertThrowsError(try crypto.hpkeOpen(first.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                                 encapsulatedKey: second.encapsulatedKey, info: vector.info,
                                                 aad: prefix + first.encapsulatedKey))
    }

    func testPublishedCiphertextRejectsChangedAuthenticationInputs() throws {
        let vector = RFC9180BaseVector()
        for position in [0, vector.ciphertext.count - 1] {
            var tampered = vector.ciphertext; tampered[position] ^= 1
            XCTAssertThrowsError(try crypto.hpkeOpen(tampered, recipientPrivateKey: vector.recipientPrivate,
                                                     encapsulatedKey: vector.enc, info: vector.info, aad: vector.aad))
        }
        XCTAssertThrowsError(try crypto.hpkeOpen(vector.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                                 encapsulatedKey: vector.enc, info: vector.info + Data([0]), aad: vector.aad))
        XCTAssertThrowsError(try crypto.hpkeOpen(vector.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                                 encapsulatedKey: vector.enc, info: vector.info, aad: vector.aad + Data([0])))
        XCTAssertThrowsError(try crypto.hpkeOpen(vector.ciphertext, recipientPrivateKey: vector.ephemeralPrivate,
                                                 encapsulatedKey: vector.enc, info: vector.info, aad: vector.aad))
        XCTAssertThrowsError(try crypto.hpkeOpen(Data(vector.ciphertext.dropLast()), recipientPrivateKey: vector.recipientPrivate,
                                                 encapsulatedKey: vector.enc, info: vector.info, aad: vector.aad))
    }

    func testInvalidKeyLengthsAndLowOrderPublicKeysFailClosed() throws {
        let vector = RFC9180BaseVector()
        for invalid in [Data(), Data(repeating: 1, count: 31), Data(repeating: 1, count: 33)] {
            XCTAssertThrowsError(try crypto.x25519PublicKey(privateKey: invalid))
            XCTAssertThrowsError(try crypto.hpkeSeal(vector.plaintext, recipientPublicKey: invalid,
                                                     info: vector.info, aadPrefix: vector.aad))
            XCTAssertThrowsError(try crypto.hpkeOpen(vector.ciphertext, recipientPrivateKey: invalid,
                                                     encapsulatedKey: vector.enc, info: vector.info, aad: vector.aad))
            XCTAssertThrowsError(try crypto.hpkeOpen(vector.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                                     encapsulatedKey: invalid, info: vector.info, aad: vector.aad))
        }
        for lowOrder in [Data(repeating: 0, count: 32), Data([1]) + Data(repeating: 0, count: 31)] {
            XCTAssertThrowsError(try crypto.hpkeSeal(vector.plaintext, recipientPublicKey: lowOrder,
                                                     info: vector.info, aadPrefix: vector.aad))
            XCTAssertThrowsError(try crypto.hpkeOpen(vector.ciphertext, recipientPrivateKey: vector.recipientPrivate,
                                                     encapsulatedKey: lowOrder, info: vector.info, aad: vector.aad))
        }
        XCTAssertThrowsError(try crypto.hpkeOpen(Data(repeating: 0, count: 15), recipientPrivateKey: vector.recipientPrivate,
                                                 encapsulatedKey: vector.enc, info: vector.info, aad: vector.aad))
    }
}

/// Generated independently with Python cryptography 50.0.2 (X25519, HMAC,
/// HKDFExpand, ChaCha20Poly1305), after verifying its schedule and seal against
/// RFC 9180 A.2.1. Uses that public vector's recipient/ephemeral private keys,
/// request ID 00...0f, transfer ID f0...ff, a 4-MiB advertised limit, and the raw
/// archive asserted above, padded to 4096 bytes. This is the complete FTAG file,
/// including the independently constructed header, request digest, and AEAD tag.
private enum FTAGRecipientVector {
    static let envelope = """
    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
    u2o2XKp3oi0dZodyU79Y3Y14auMXrg==
    """
}
