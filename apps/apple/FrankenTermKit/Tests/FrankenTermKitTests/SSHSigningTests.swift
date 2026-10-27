import Foundation
import XCTest
@testable import FrankenTermKit
#if os(Linux)
import CTestOpenSSL
#else
import CryptoKit
#endif

/// All identities here are public synthetic test data. The Linux verifier is
/// independent OpenSSL, while production signing uses maintained Swift Crypto.
final class SSHSigningTests: XCTestCase, @unchecked Sendable {
    private struct Fixture {
        let key: PrivateKeyRecord
        let curve: SSHECDSACurve?
        var algorithm: String { curve?.algorithm ?? "ssh-ed25519" }
    }

    private func string(_ bytes: Data) -> Data {
        let count = UInt32(bytes.count)
        return Data([UInt8(truncatingIfNeeded: count >> 24), UInt8(truncatingIfNeeded: count >> 16),
                     UInt8(truncatingIfNeeded: count >> 8), UInt8(truncatingIfNeeded: count)]) + bytes
    }

    private func ecdsaFixture(_ curve: SSHECDSACurve, scalarByte: UInt8 = 1) throws -> Fixture {
        let scalar = Data(repeating: 0, count: curve.scalarBytes - 1) + Data([scalarByte])
        let point = try TestCryptography().ecdsaPublicKey(scalar: scalar, curve: curve)
        let algorithm = string(Data(curve.algorithm.utf8)), name = string(Data(curve.rawValue.utf8))
        let blob = algorithm + name + string(point)
        var part = Data([1, 2, 3, 4, 1, 2, 3, 4]) + algorithm + name + string(point)
            + string(Data([scalarByte])) + string(Data("PUBLIC SYNTHETIC TEST KEY; NEVER AUTHORIZE".utf8))
        var padding: UInt8 = 1
        while !part.count.isMultiple(of: 8) { part.append(padding); padding += 1 }
        let container = Data("openssh-key-v1\0".utf8) + string(Data("none".utf8)) + string(Data("none".utf8))
            + string(Data()) + Data([0, 0, 0, 1]) + string(blob) + string(part)
        _ = try PrivateKeyRecord.validateContainer(container, using: TestCryptography())
        return Fixture(key: .init(credentialID: "synthetic", container: container, publicBlob: blob), curve: curve)
    }

    private func fixtures() throws -> [Fixture] {
        [Fixture(key: try sampleKey(using: TestCryptography()).0, curve: nil)]
            + (try SSHECDSACurve.allCases.map { try ecdsaFixture($0) })
    }

    /// Generated independently by pyca/cryptography 50.0.2 from the public
    /// synthetic scalar 2^254; OpenSSH ssh-keygen -y accepts this exact record.
    /// Its aligned private body has a complete 01...08 padding block. Never
    /// authorize this published fixture on a real machine.
    private func independentlyPaddedFixture() throws -> Fixture {
        let pem = Data("""
        -----BEGIN OPENSSH PRIVATE KEY-----
        b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNl
        Y2RzYS1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQQB7n/CAnCM/rDCv5ML
        8zporQhtTOmaEeONk8ppjrmYBZZVzvAbAkiCEkvgLvNFVxGBGDbqNb55mwn9X04Q
        7syvAAAAoCMHJpsjByabAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAy
        NTYAAABBBAHuf8ICcIz+sMK/kwvzOmitCG1M6ZoR442TymmOuZgFllXO8BsCSIIS
        S+Au80VXEYEYNuo1vnmbCf1fThDuzK8AAAAgQAAAAAAAAAAAAAAAAAAAAAAAAAAA
        AAAAAAAAAAAAAAAAAAAAAQIDBAUGBwg=
        -----END OPENSSH PRIVATE KEY-----
        """.utf8)
        return Fixture(key: try PrivateKeyRecord.fromPEM(pem, credentialID: "synthetic", using: TestCryptography()), curve: .nistp256)
    }

    func testIndependentOpenSSHFullBlockPaddingImportsAndSigns() throws {
        let fixture = try independentlyPaddedFixture()
        XCTAssertEqual(fixture.key.container.suffix(8), Data(1...8))
        let scalar = Data([0x40]) + Data(repeating: 0, count: 31)
        let point = try TestCryptography().ecdsaPublicKey(scalar: scalar, curve: .nistp256)
        let expected = string(Data("ecdsa-sha2-nistp256".utf8)) + string(Data("nistp256".utf8)) + string(point)
        XCTAssertEqual(fixture.key.publicBlob, expected)
        let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(0..<32), user: "operator", publicBlob: expected)
        let signature = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: fixture.key.container)
        XCTAssertTrue(try verify(signature, message: challenge.signedData, fixture: fixture))
    }

    func testFullBlockPaddingStillRejectsMalformedAndExcessPadding() throws {
        let fixture = try independentlyPaddedFixture()
        var reader = Reader(Data(fixture.key.container.dropFirst(15)))
        let cipher = try reader.string(), kdf = try reader.string(), options = try reader.string()
        reader.offset += 4
        let publicKey = try reader.string(), privatePart = try reader.string()
        XCTAssertTrue(reader.finished)
        func container(_ part: Data) -> Data {
            Data("openssh-key-v1\0".utf8) + string(cipher) + string(kdf) + string(options)
                + Data([0, 0, 0, 1]) + string(publicKey) + string(part)
        }
        for part in [privatePart + Data(9...16), Data(privatePart.dropLast()) + Data([0])] {
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(part), using: TestCryptography()))
        }
        // OpenSSH's own writer can omit padding for an already aligned body.
        XCTAssertEqual(try PrivateKeyRecord.validateContainer(container(Data(privatePart.dropLast(8))), using: TestCryptography()), publicKey)
    }

    /// Deliberately separate from the production ByteCursor/SSH serializers.
    private struct Reader {
        let bytes: [UInt8]
        var offset = 0
        init(_ value: Data) { bytes = Array(value) }
        mutating func string() throws -> Data {
            guard offset <= bytes.count, bytes.count - offset >= 4 else { throw FTError.malformed("test.ssh_extent") }
            let count = Int(bytes[offset]) << 24 | Int(bytes[offset + 1]) << 16
                | Int(bytes[offset + 2]) << 8 | Int(bytes[offset + 3])
            offset += 4
            guard count <= bytes.count - offset else { throw FTError.malformed("test.ssh_extent") }
            defer { offset += count }
            return Data(bytes[offset..<offset + count])
        }
        var finished: Bool { offset == bytes.count }
    }

    private func positiveInteger(_ encoded: Data, width: Int) throws -> Data {
        let first = try XCTUnwrap(encoded.first)
        XCTAssertEqual(first & 0x80, 0, "RFC 4251 mpint must not encode a negative signature component")
        let magnitude: Data
        if first == 0 {
            XCTAssertGreaterThan(encoded.count, 1)
            guard encoded.count > 1 else { throw FTError.malformed("test.zero_mpint") }
            XCTAssertNotEqual(encoded[encoded.startIndex + 1] & 0x80, 0, "Only a necessary sign byte is permitted")
            magnitude = Data(encoded.dropFirst())
        } else { magnitude = encoded }
        XCTAssertLessThanOrEqual(magnitude.count, width)
        XCTAssertTrue(magnitude.contains { $0 != 0 })
        return magnitude
    }

    private func verify(_ blob: Data, message: Data, fixture: Fixture, wrongHash: Bool = false) throws -> Bool {
        var signature = Reader(blob), publicKey = Reader(fixture.key.publicBlob)
        XCTAssertEqual(try signature.string(), Data(fixture.algorithm.utf8))
        let payload = try signature.string()
        XCTAssertTrue(signature.finished, "Signature blob has exactly two SSH strings")
        XCTAssertEqual(try publicKey.string(), Data(fixture.algorithm.utf8))
        if let curve = fixture.curve {
            XCTAssertEqual(try publicKey.string(), Data(curve.rawValue.utf8))
            let point = try publicKey.string()
            XCTAssertTrue(publicKey.finished)
            var components = Reader(payload)
            let r = try positiveInteger(components.string(), width: curve.scalarBytes)
            let s = try positiveInteger(components.string(), width: curve.scalarBytes)
            XCTAssertTrue(components.finished, "ECDSA SSH payload is exactly two canonical mpints")
            #if os(Linux)
            let nid: Int32, hash: Int32
            switch curve {
            case .nistp256: nid = NID_X9_62_prime256v1; hash = wrongHash ? 512 : 256
            case .nistp384: nid = NID_secp384r1; hash = wrongHash ? 256 : 384
            case .nistp521: nid = NID_secp521r1; hash = wrongHash ? 384 : 512
            }
            let status = point.withUnsafeBytes { point in
                message.withUnsafeBytes { messageBytes in
                    r.withUnsafeBytes { rBytes in
                        s.withUnsafeBytes { sBytes in
                            ft_test_verify_ecdsa(nid, hash,
                                point.bindMemory(to: UInt8.self).baseAddress, point.count,
                                messageBytes.bindMemory(to: UInt8.self).baseAddress, messageBytes.count,
                                rBytes.bindMemory(to: UInt8.self).baseAddress, rBytes.count,
                                sBytes.bindMemory(to: UInt8.self).baseAddress, sBytes.count)
                        }
                    }
                }
            }
            XCTAssertGreaterThanOrEqual(status, 0, "Independent OpenSSL verification setup must succeed")
            return status == 1
            #else
            let raw = Data(repeating: 0, count: curve.scalarBytes - r.count) + r
                + Data(repeating: 0, count: curve.scalarBytes - s.count) + s
            switch curve {
            case .nistp256:
                let key = try P256.Signing.PublicKey(x963Representation: point)
                let signature = try P256.Signing.ECDSASignature(rawRepresentation: raw)
                return wrongHash ? key.isValidSignature(signature, for: SHA512.hash(data: message))
                    : key.isValidSignature(signature, for: message)
            case .nistp384:
                let key = try P384.Signing.PublicKey(x963Representation: point)
                let signature = try P384.Signing.ECDSASignature(rawRepresentation: raw)
                return wrongHash ? key.isValidSignature(signature, for: SHA256.hash(data: message))
                    : key.isValidSignature(signature, for: message)
            case .nistp521:
                let key = try P521.Signing.PublicKey(x963Representation: point)
                let signature = try P521.Signing.ECDSASignature(rawRepresentation: raw)
                return wrongHash ? key.isValidSignature(signature, for: SHA384.hash(data: message))
                    : key.isValidSignature(signature, for: message)
            }
            #endif
        }
        let rawKey = try publicKey.string()
        XCTAssertTrue(publicKey.finished)
        XCTAssertEqual(payload.count, 64)
        #if os(Linux)
        let status = rawKey.withUnsafeBytes { key in
            message.withUnsafeBytes { messageBytes in
                payload.withUnsafeBytes { signature in
                    ft_test_verify_ed25519(key.bindMemory(to: UInt8.self).baseAddress, key.count,
                        messageBytes.bindMemory(to: UInt8.self).baseAddress, messageBytes.count,
                        signature.bindMemory(to: UInt8.self).baseAddress, signature.count)
                }
            }
        }
        XCTAssertGreaterThanOrEqual(status, 0, "Independent OpenSSL verification setup must succeed")
        return status == 1
        #else
        return try Curve25519.Signing.PublicKey(rawRepresentation: rawKey).isValidSignature(payload, for: message)
        #endif
    }

    func testAuthenticationPayloadUsesTheExactRFC4252FieldsAndBinarySessionIdentifier() throws {
        let fixture = try fixtures()[0], identifier = Data(0..<32)
        let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: identifier, user: "operator",
                                                           publicBlob: fixture.key.publicBlob)
        // Built incrementally: the single nine-term `+` chain exceeds the Apple
        // SDK type checker's time limit (Linux's overload set happens to fit).
        var expected = string(identifier)
        expected.append(Data([50]))
        expected.append(string(Data("operator".utf8)))
        expected.append(string(Data("ssh-connection".utf8)))
        expected.append(string(Data("publickey".utf8)))
        expected.append(Data([1]))
        expected.append(string(Data("ssh-ed25519".utf8)))
        expected.append(string(fixture.key.publicBlob))
        XCTAssertEqual(challenge.signedData, expected)
        XCTAssertEqual(challenge.sessionIdentifier, identifier)
        XCTAssertEqual(challenge.user, "operator")
        XCTAssertEqual(challenge.algorithm, "ssh-ed25519")
        XCTAssertEqual(challenge.publicBlob, fixture.key.publicBlob)
        XCTAssertEqual(challenge.signedData.first, 0, "Session ID is an SSH string, not raw bytes or text")
    }

    func testEverySupportedFamilyProducesAnIndependentlyVerifiedSSHSignature() throws {
        for fixture in try fixtures() {
            let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(0..<32), user: "operator",
                                                               publicBlob: fixture.key.publicBlob)
            let signature = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: fixture.key.container)
            XCTAssertTrue(try verify(signature, message: challenge.signedData, fixture: fixture), fixture.algorithm)
            var tampered = challenge.signedData; tampered[8] ^= 1
            XCTAssertFalse(try verify(signature, message: tampered, fixture: fixture), fixture.algorithm)
            if fixture.curve != nil {
                XCTAssertFalse(try verify(signature, message: challenge.signedData, fixture: fixture, wrongHash: true),
                               "RFC 5656 selects SHA-256/384/512 from the curve")
            }
        }
    }

    func testSignaturesCannotMoveToAnotherUserOrTransportSession() throws {
        for fixture in try fixtures() {
            let identifier = Data(repeating: 0xA5, count: 48)
            let original = try SSHUserAuthenticationChallenge(sessionIdentifier: identifier, user: "operator",
                                                              publicBlob: fixture.key.publicBlob)
            let signature = try SSHUserAuthenticationSigner.sign(original, privateContainer: fixture.key.container)
            let anotherUser = try SSHUserAuthenticationChallenge(sessionIdentifier: identifier, user: "root",
                                                                 publicBlob: fixture.key.publicBlob)
            let anotherSession = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 0x5A, count: 48),
                                                                    user: "operator", publicBlob: fixture.key.publicBlob)
            XCTAssertFalse(try verify(signature, message: anotherUser.signedData, fixture: fixture))
            XCTAssertFalse(try verify(signature, message: anotherSession.signedData, fixture: fixture))
        }
    }

    func testSignatureCoversServiceMethodBooleanAlgorithmAndPublicKeyFields() throws {
        let fixture = try fixtures()[0]
        let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 0x11, count: 64),
                                                           user: "operator", publicBlob: fixture.key.publicBlob)
        let signature = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: fixture.key.container)
        for text in ["ssh-connection", "publickey", "ssh-ed25519"] {
            var modified = challenge.signedData
            let range = try XCTUnwrap(modified.range(of: Data(text.utf8)))
            modified[range.lowerBound] ^= 1
            XCTAssertFalse(try verify(signature, message: modified, fixture: fixture))
        }
        var query = challenge.signedData
        let methodRange = try XCTUnwrap(query.range(of: Data("publickey".utf8)))
        XCTAssertEqual(query[methodRange.upperBound], 1)
        query[methodRange.upperBound] = 0
        XCTAssertFalse(try verify(signature, message: query, fixture: fixture), "A signature cannot become a key query")
        var wrongKey = challenge.signedData; wrongKey[wrongKey.count - 1] ^= 1
        XCTAssertFalse(try verify(signature, message: wrongKey, fixture: fixture))
    }

    func testAContainerMustBelongToTheExactRequestedPublicIdentity() throws {
        let keys = try fixtures()
        for (index, fixture) in keys.enumerated() {
            let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 7, count: 32),
                                                               user: "operator", publicBlob: fixture.key.publicBlob)
            XCTAssertThrowsError(try SSHUserAuthenticationSigner.sign(challenge,
                privateContainer: keys[(index + 1) % keys.count].key.container))
            if let curve = fixture.curve {
                XCTAssertThrowsError(try SSHUserAuthenticationSigner.sign(challenge,
                    privateContainer: ecdsaFixture(curve, scalarByte: 2).key.container))
            }
        }
    }

    func testSigningValidatesTheWholePrivateContainerBeforeUsingItsScalar() throws {
        for fixture in try fixtures() {
            let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 8, count: 32),
                                                               user: "operator", publicBlob: fixture.key.publicBlob)
            XCTAssertThrowsError(try SSHUserAuthenticationSigner.sign(challenge,
                privateContainer: fixture.key.container + Data([0])))
            var outer = ByteCursor(fixture.key.container)
            _ = try outer.take(15)
            for _ in 0..<3 { _ = try outer.sshString() }
            _ = try outer.uint(4); _ = try outer.sshString()
            var mismatchedCheckints = fixture.key.container
            mismatchedCheckints[outer.offset + 4] ^= 1
            XCTAssertThrowsError(try SSHUserAuthenticationSigner.sign(challenge, privateContainer: mismatchedCheckints))
        }
    }

    func testMalformedAuthenticationChallengesCannotReachSigning() throws {
        let key = try sampleKey(using: TestCryptography()).0
        for identifier in [Data(), Data(repeating: 0, count: 65_536)] {
            XCTAssertThrowsError(try SSHUserAuthenticationChallenge(sessionIdentifier: identifier,
                user: "operator", publicBlob: key.publicBlob))
        }
        for user in ["", "operator\0root", "operator\nroot", String(repeating: "x", count: 65_536)] {
            XCTAssertThrowsError(try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 0, count: 32),
                user: user, publicBlob: key.publicBlob))
        }
        for blob in [Data(), key.publicBlob + Data([0]), string(Data("ssh-rsa".utf8)) + string(Data([1])),
                     string(Data("ssh-ed25519".utf8)) + string(Data(repeating: 0, count: 31))] {
            XCTAssertThrowsError(try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 0, count: 32),
                user: "operator", publicBlob: blob))
        }
    }

    /// Structural parser fixtures, not generated private keys. Actual RSA host
    /// signatures are exercised by the independent libssh loopback harness.
    private func rsaPublicBlob(exponent: Data = Data([1, 0, 1]), modulus: Data) -> Data {
        string(Data("ssh-rsa".utf8)) + string(exponent) + string(modulus)
    }

    func testRSAHostIdentityPreservesItsCanonicalFingerprintAndRequiresSHA2() throws {
        let blob = rsaPublicBlob(modulus: Data([0]) + Data(repeating: 0xA5, count: 256))
        let host = try SSHPublicIdentity(blob: blob)
        XCTAssertEqual(host.blob, blob)
        XCTAssertEqual(host.algorithm, "ssh-rsa")
        // Independently computed from this exact SSH blob with Python hashlib.
        XCTAssertEqual(host.fingerprint, "SHA256:ecOadYuKd3sZt88BcsfofO2iQzd1t1iQSPFiPNMWCn4")
        XCTAssertTrue(host.permitsHostSignature("rsa-sha2-256"))
        XCTAssertTrue(host.permitsHostSignature("rsa-sha2-512"))
        for refused in ["ssh-rsa", "rsa-sha2-384", "ssh-ed25519", "ecdsa-sha2-nistp256",
                        "rsa-sha2-256-cert-v01@openssh.com", "", "RSA-SHA2-512"] {
            XCTAssertFalse(host.permitsHostSignature(refused))
        }
        for fixture in try fixtures() {
            let identity = try SSHPublicIdentity(blob: fixture.key.publicBlob)
            XCTAssertTrue(identity.permitsHostSignature(fixture.algorithm))
            XCTAssertFalse(identity.permitsHostSignature("rsa-sha2-256"))
            XCTAssertFalse(identity.permitsHostSignature("rsa-sha2-512"))
        }
    }

    func testRSAHostKeyBoundsUseActualModulusBitsAndCanonicalPositiveIntegers() throws {
        for bits in [2048, 2049, 3072, 4096, 8191, 8192] {
            var magnitude = Data(repeating: 0xA5, count: (bits + 7) / 8)
            magnitude[0] = UInt8(1 << ((bits - 1) % 8))
            let encoded = magnitude[0] & 0x80 == 0 ? magnitude : Data([0]) + magnitude
            XCTAssertEqual(try SSHPublicIdentity(blob: rsaPublicBlob(modulus: encoded)).algorithm, "ssh-rsa")
        }
        let modulus = Data([0]) + Data(repeating: 0xA5, count: 256)
        // Both small and larger canonical exponents fit the bounded profile.
        for exponent in [Data([3]), Data([1, 0, 1]), Data([0]) + Data(repeating: 0xFF, count: 8)] {
            XCTAssertNoThrow(try SSHPublicIdentity(blob: rsaPublicBlob(exponent: exponent, modulus: modulus)))
        }
        for exponent in [Data(), Data([0]), Data([1]), Data([2]), Data([4]), Data([0x81]),
                         Data([0, 3]), Data([0, 0x7F]), Data([0, 0, 0x81]), Data(repeating: 1, count: 9)] {
            XCTAssertThrowsError(try SSHPublicIdentity(blob: rsaPublicBlob(exponent: exponent, modulus: modulus)))
        }
        // Typed up front: Swift 6.4 cannot infer this literal's `+` overloads in time.
        let invalidModuli: [Data] = [Data(), Data([0]), Data(repeating: 0xA5, count: 256),
                                     Data([0, 0]) + Data(repeating: 0xA5, count: 256),
                                     Data([0x7F]) + Data(repeating: 0xA5, count: 255),
                                     Data([0]) + Data(repeating: 0xA5, count: 255),
                                     Data([0]) + Data(repeating: 0xA4, count: 256),
                                     Data([1]) + Data(repeating: 0xA5, count: 1024)]
        for invalid in invalidModuli {
            XCTAssertThrowsError(try SSHPublicIdentity(blob: rsaPublicBlob(modulus: invalid)))
        }
        let valid = rsaPublicBlob(modulus: modulus)
        XCTAssertThrowsError(try SSHPublicIdentity(blob: valid + Data([0])))
        XCTAssertThrowsError(try SSHPublicIdentity(blob: Data(valid.dropLast())))
        for invalidName in ["rsa-sha2-256", "rsa-sha2-512", "ssh-rsa-cert-v01@openssh.com"] {
            XCTAssertThrowsError(try SSHPublicIdentity(blob:
                string(Data(invalidName.utf8)) + string(Data([1, 0, 1])) + string(modulus)))
        }
    }

    func testRSAUserAuthenticationRequiresAnExplicitSHA2AlgorithmAndCompletePrivateKey() throws {
        let blob = rsaPublicBlob(modulus: Data([0]) + Data(repeating: 0xA5, count: 256))
        _ = try SSHPublicIdentity(blob: blob)
        XCTAssertThrowsError(try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 9, count: 32),
            user: "operator", publicBlob: blob)) { error in
            XCTAssertEqual(error as? SSHAuthenticationError, .unsupportedAlgorithm)
        }
        // An accepted public blob is not proof that a corresponding private
        // key exists. A bounded but incomplete private record cannot be staged.
        let container = Data("openssh-key-v1\0".utf8) + string(Data("none".utf8)) + string(Data("none".utf8))
            + string(Data()) + Data([0, 0, 0, 1]) + string(blob) + string(Data(repeating: 0, count: 24))
        XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container, using: TestCryptography()))
    }

    func testRSAChallengeBindsNegotiatedSHA2ToTheOriginalPublicIdentity() throws {
        for byteCount in [256, 384, 512, 1024] {
            let blob = rsaPublicBlob(modulus: Data([0]) + Data(repeating: 0xA5, count: byteCount))
            let identity = try SSHPublicIdentity(blob: blob)
            for algorithm in ["rsa-sha2-256", "rsa-sha2-512"] {
                let user = String(repeating: "u", count: 256), session = Data(0..<64)
                let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: session, user: user,
                    publicBlob: blob, signatureAlgorithm: algorithm)
                XCTAssertEqual(challenge.publicKeyAlgorithm, "ssh-rsa")
                XCTAssertEqual(challenge.algorithm, algorithm)
                XCTAssertEqual(challenge.fingerprint, identity.fingerprint)
                var expected = string(session)
                expected.append(50)
                expected += string(Data(user.utf8))
                expected += string(Data("ssh-connection".utf8))
                expected += string(Data("publickey".utf8))
                expected.append(1)
                expected += string(Data(algorithm.utf8))
                expected += string(blob)
                XCTAssertEqual(challenge.signedData, expected)
                XCTAssertLessThanOrEqual(challenge.signedData.count, 1536)
            }
            for refused in ["ssh-rsa", "rsa-sha2-384", "rsa-sha2-512-cert-v01@openssh.com", "ssh-ed25519", ""] {
                XCTAssertThrowsError(try SSHUserAuthenticationChallenge(sessionIdentifier: Data(0..<32), user: "operator",
                    publicBlob: blob, signatureAlgorithm: refused))
            }
        }
        for fixture in try fixtures() {
            XCTAssertThrowsError(try SSHUserAuthenticationChallenge(sessionIdentifier: Data(0..<32), user: "operator",
                publicBlob: fixture.key.publicBlob, signatureAlgorithm: "rsa-sha2-512"))
            let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(0..<32), user: "operator",
                publicBlob: fixture.key.publicBlob, signatureAlgorithm: fixture.algorithm)
            XCTAssertEqual(challenge.algorithm, fixture.algorithm)
        }
    }

    func testRSAProducesExactOpenSSLSignaturesFromImportedOpenSSHKeys() throws {
        let fixtures = try RSAFixtures.load()
        let session = try Data(ftHex: fixtures.sessionIdentifierHex)
        for fixture in fixtures.keys {
            let key = try PrivateKeyRecord.fromPEM(Data(fixture.clearPEM.utf8), credentialID: "synthetic",
                                                   using: TestCryptography())
            for algorithm in ["rsa-sha2-256", "rsa-sha2-512"] {
                let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: session, user: fixtures.user,
                    publicBlob: fixture.publicBlob, signatureAlgorithm: algorithm)
                XCTAssertEqual(challenge.signedData, try Data(ftHex: XCTUnwrap(fixture.signedData[algorithm])))
                let expectedPayload = try Data(ftHex: XCTUnwrap(fixture.signatures[algorithm]))
                let signature = try SSHUserAuthenticationSigner.sign(challenge, privateContainer: key.container)
                XCTAssertEqual(signature, string(Data(algorithm.utf8)) + string(expectedPayload))
                XCTAssertEqual(signature.count, fixture.bits / 8 + 20)
                XCTAssertLessThanOrEqual(signature.count, 1044)
                // The stable public identity is unchanged by the negotiated
                // hash, but the signed session and username are not fungible.
                let other = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 99, count: 32),
                    user: "other", publicBlob: fixture.publicBlob, signatureAlgorithm: algorithm)
                XCTAssertEqual(other.fingerprint, challenge.fingerprint)
                XCTAssertNotEqual(try SSHUserAuthenticationSigner.sign(other, privateContainer: key.container), signature)
            }
        }
    }

    func testCancellationNeverReturnsASignature() async throws {
        let key = try sampleKey(using: TestCryptography()).0
        let challenge = try SSHUserAuthenticationChallenge(sessionIdentifier: Data(repeating: 1, count: 32),
                                                           user: "operator", publicBlob: key.publicBlob)
        let task = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            XCTAssertThrowsError(try SSHUserAuthenticationSigner.sign(challenge, privateContainer: key.container)) { error in
                XCTAssertTrue(error is CancellationError)
            }
        }
        try await task.value
    }
}
