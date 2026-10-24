import Foundation
import XCTest
@testable import FrankenTermKit

/// Public synthetic fixtures generated independently by OpenSSH_9.6p1:
/// ssh-keygen -q -t ecdsa -b <256|384|521> -N '' -C '<test-only comment>' -f <path>
/// Fingerprints are the independent `ssh-keygen -lf <path>.pub` results.
/// These private values are published test data and MUST NEVER authenticate anything.
private struct ECDSAFixture {
    let curve: SSHECDSACurve, fingerprint: String, pem: String
    static let all: [ECDSAFixture] = [
        .init(curve: .nistp256, fingerprint: "SHA256:TIH5q7eiU7YB2c8Zy+FfY67DS06JBKkXKNb03xGCRU4", pem: """
        -----BEGIN OPENSSH PRIVATE KEY-----
        b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
        1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQTEIVH697ImUbT9f8JMOelSQTkIrW+b
        TiZ7JaRYMyb3Oqss/zQ3Erl3eQGyfebU7YK2Hx1a9tcXkAlmyaKTCsfkAAAA0PsJYCP7CW
        AjAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBMQhUfr3siZRtP1/
        wkw56VJBOQitb5tOJnslpFgzJvc6qyz/NDcSuXd5AbJ95tTtgrYfHVr21xeQCWbJopMKx+
        QAAAAhANfYxqHc0dZNiwDYXpRPO60euaK5NsBmD0CfrpWCLS5DAAAAMkZyYW5rZW5UZXJt
        IHB1YmxpYyB0ZXN0IHZlY3RvciBuaXN0cDI1NjsgTkVWRVIgVVNFAQIDBAU=
        -----END OPENSSH PRIVATE KEY-----
        """),
        .init(curve: .nistp384, fingerprint: "SHA256:2BMoRDLy2BcFZAEHHwIVcmaudPpkj2b3sn5jSluRJcU", pem: """
        -----BEGIN OPENSSH PRIVATE KEY-----
        b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAiAAAABNlY2RzYS
        1zaGEyLW5pc3RwMzg0AAAACG5pc3RwMzg0AAAAYQR7AbesmWyuc1moNvhYEVNuH/RkxWdh
        JrLHpb4EYAYi1+tboI+vxZO/N+z1+Tt7LepAm0c1HkY1oXiKS5M9/goi0awT/8ljPfFi6v
        NlatmTorkBPdm3RMxPspBzn15+TSkAAAEAj+7WOI/u1jgAAAATZWNkc2Etc2hhMi1uaXN0
        cDM4NAAAAAhuaXN0cDM4NAAAAGEEewG3rJlsrnNZqDb4WBFTbh/0ZMVnYSayx6W+BGAGIt
        frW6CPr8WTvzfs9fk7ey3qQJtHNR5GNaF4ikuTPf4KItGsE//JYz3xYurzZWrZk6K5AT3Z
        t0TMT7KQc59efk0pAAAAMCUAgyQKPyh3daOKGq2LqLJ252FDUvwaZioaVE+x7N+X4d7aFJ
        CTEwjj3QFHCRbS9gAAADJGcmFua2VuVGVybSBwdWJsaWMgdGVzdCB2ZWN0b3IgbmlzdHAz
        ODQ7IE5FVkVSIFVTRQECAwQFBg==
        -----END OPENSSH PRIVATE KEY-----
        """),
        .init(curve: .nistp521, fingerprint: "SHA256:SQ5R5gcdFEg4R0/e3JwKN1/mEuUbWyYcT3xD7opr6qU", pem: """
        -----BEGIN OPENSSH PRIVATE KEY-----
        b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAArAAAABNlY2RzYS
        1zaGEyLW5pc3RwNTIxAAAACG5pc3RwNTIxAAAAhQQBLA8BOM2uW0QqjBq+E+w5VACNAoY1
        VPdTvpdlFeqCnJa2vZYi5ouA61ohgS+i5PKu5P7gduYNGGPjj6g9j2pNrYYAwBEdgDdA9o
        PsPX51q4IscUf+R0xA/ecrTUBzUjwYam0I3fqoA1AEWH9XLmcgFKsTGW0fs6KOCfTYsAAf
        mpbA8VAAAAEwQfsSJUH7EiUAAAATZWNkc2Etc2hhMi1uaXN0cDUyMQAAAAhuaXN0cDUyMQ
        AAAIUEASwPATjNrltEKowavhPsOVQAjQKGNVT3U76XZRXqgpyWtr2WIuaLgOtaIYEvouTy
        ruT+4HbmDRhj44+oPY9qTa2GAMARHYA3QPaD7D1+dauCLHFH/kdMQP3nK01Ac1I8GGptCN
        36qANQBFh/Vy5nIBSrExltH7Oijgn02LAAH5qWwPFQAAAAQgCA0eSAk7Aphl3ZL8Z2a5v/
        dB7sjlna6qVytY2WHm7EiI2MDJABauYqNbslihC3cZPYuuLorbgm8SH0gYvSYtM0IgAAAD
        JGcmFua2VuVGVybSBwdWJsaWMgdGVzdCB2ZWN0b3IgbmlzdHA1MjE7IE5FVkVSIFVTRQ==
        -----END OPENSSH PRIVATE KEY-----
        """)
    ]
}

final class ECDSAKeyTests: XCTestCase {
    let crypto = TestCryptography()
    private func workspace(for fixture: ECDSAFixture) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fixture.fingerprint)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        credentials[0]["algorithm"] = fixture.curve.algorithm
        workspace["credentials"] = credentials; root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }
    private func ssh(_ bytes: Data) -> Data {
        var result = Data(); result.appendBE(UInt64(bytes.count), bytes: 4)
        return result + bytes
    }
    private struct Components {
        var algorithm: Data, curve: Data, point: Data, scalar: Data, comment: Data
    }
    private func components(of key: PrivateKeyRecord) throws -> Components {
        var reader = ByteCursor(key.container)
        _ = try reader.take(15)
        for _ in 0..<3 { _ = try reader.sshString() }
        _ = try reader.uint(4); _ = try reader.sshString()
        var inner = ByteCursor(try reader.sshString())
        _ = try inner.take(8)
        return Components(algorithm: try inner.sshString(), curve: try inner.sshString(), point: try inner.sshString(),
                          scalar: try inner.sshString(), comment: try inner.sshString())
    }
    private func container(_ fields: Components, publicPoint: Data? = nil, publicCurve: Data? = nil, publicAlgorithm: Data? = nil) -> Data {
        let blob = ssh(publicAlgorithm ?? fields.algorithm) + ssh(publicCurve ?? fields.curve) + ssh(publicPoint ?? fields.point)
        var inner = Data([1, 2, 3, 4, 1, 2, 3, 4]) + ssh(fields.algorithm) + ssh(fields.curve)
            + ssh(fields.point) + ssh(fields.scalar) + ssh(fields.comment)
        var next: UInt8 = 1
        while !inner.count.isMultiple(of: 8) { inner.append(next); next += 1 }
        return Data("openssh-key-v1\0".utf8) + ssh(Data("none".utf8)) + ssh(Data("none".utf8)) + ssh(Data())
            + Data([0, 0, 0, 1]) + ssh(blob) + ssh(inner)
    }
    func testOpenSSHFixturesDeriveTheActualPublicIdentityAndRoundTripThroughFTAG() throws {
        for fixture in ECDSAFixture.all {
            let key = try PrivateKeyRecord.fromPEM(Data(fixture.pem.utf8), credentialID: "key", using: crypto)
            XCTAssertEqual("SHA256:" + crypto.sha256(key.publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: ""), fixture.fingerprint)
            let components = try components(of: key)
            var scalarReader = ByteCursor(ssh(components.scalar))
            let scalar = try scalarReader.sshScalar(curve: fixture.curve)
            XCTAssertEqual(try crypto.ecdsaPublicKey(scalar: scalar, curve: fixture.curve), components.point)
            let archive = try AirGapArchive(workspace: workspace(for: fixture), keys: [key], using: crypto)
            let exported = try FTAG.seal(archive, using: crypto)
            let restored = try FTAG.open(exported.envelope, code: exported.code, using: crypto)
            XCTAssertEqual(restored.keys.count, 1)
            XCTAssertEqual(restored.keys[0].container, key.container)
            XCTAssertEqual(restored.workspace.credentials[0].algorithm, fixture.curve.algorithm)
            XCTAssertNil(exported.envelope.range(of: key.container))
        }
    }
    func testCanonicalPositiveScalarBoundariesRejectAmbiguousOrUnboundedIntegers() throws {
        for curve in SSHECDSACurve.allCases {
            let width = curve.scalarBytes
            for invalid in [Data(), Data([0]), Data([0, 1]), Data([0, 0x7f]), Data([0x80]), Data([0xff]),
                            Data([0, 0, 0x80]), Data(repeating: 1, count: width + 1)] {
                var cursor = ByteCursor(ssh(invalid))
                XCTAssertThrowsError(try cursor.sshScalar(curve: curve))
            }
            for (encoded, lastByte) in [(Data([1]), UInt8(1)), (Data([0, 0x80]), UInt8(0x80)), (Data([0, 0xff]), UInt8(0xff))] {
                var cursor = ByteCursor(ssh(encoded))
                let scalar = try cursor.sshScalar(curve: curve)
                XCTAssertEqual(scalar, Data(repeating: 0, count: width - 1) + Data([lastByte]))
            }
        }
    }
    func testEveryCurveRejectsForgedPublicComponentsAndInvalidScalars() throws {
        for fixture in ECDSAFixture.all {
            let key = try PrivateKeyRecord.fromPEM(Data(fixture.pem.utf8), credentialID: "key", using: crypto)
            let good = try components(of: key)
            var wrongPoint = good.point; wrongPoint[wrongPoint.count - 1] ^= 1
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(good, publicPoint: wrongPoint), using: crypto))
            var forged = good; forged.point = wrongPoint
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(forged), using: crypto), "matching public copies do not prove the private scalar")
            forged = good; forged.scalar[forged.scalar.count - 1] ^= 1
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(forged), using: crypto))
            for bad in [Data(), Data([0]), Data([0, 1]), Data([0x80]), Data([0]) + Data(repeating: 0xff, count: fixture.curve.scalarBytes)] {
                forged = good; forged.scalar = bad
                XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(forged), using: crypto))
            }
            XCTAssertThrowsError(try crypto.ecdsaPublicKey(scalar: Data(repeating: 0, count: fixture.curve.scalarBytes), curve: fixture.curve))
            XCTAssertThrowsError(try crypto.ecdsaPublicKey(scalar: Data(repeating: 0xff, count: fixture.curve.scalarBytes), curve: fixture.curve))
            XCTAssertThrowsError(try crypto.ecdsaPublicKey(scalar: Data([1]), curve: fixture.curve))
            forged = good; forged.point[0] = 6
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(forged), using: crypto), "hybrid/compressed encodings are not the canonical profile")
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(good, publicCurve: Data("nistp999".utf8)), using: crypto))
            forged = good; forged.curve = Data("nistp999".utf8)
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(forged), using: crypto))
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(container(good, publicAlgorithm: Data("ssh-ed25519".utf8)), using: crypto))
        }
    }
    func testManifestAlgorithmAndFingerprintMustBothMatchPrivateIdentity() throws {
        for fixture in ECDSAFixture.all {
            let key = try PrivateKeyRecord.fromPEM(Data(fixture.pem.utf8), credentialID: "key", using: crypto)
            // An Ed25519 declaration with the correct ECDSA fingerprint is still a lie.
            let wrongAlgorithm = try WorkspaceDocument(data: sampleManifest(fingerprint: fixture.fingerprint))
            XCTAssertThrowsError(try AirGapArchive(workspace: wrongAlgorithm, keys: [key], using: crypto))
            let wrongFingerprint = try WorkspaceDocument(data: sampleManifest())
            XCTAssertThrowsError(try AirGapArchive(workspace: wrongFingerprint, keys: [key], using: crypto))
            let forged = PrivateKeyRecord(credentialID: "key", container: key.container, publicBlob: key.publicBlob + Data([0]))
            XCTAssertThrowsError(try AirGapArchive(workspace: workspace(for: fixture), keys: [forged], using: crypto))
        }
    }
}

// Preserve the independently generated fixtures and five regressions published
// concurrently on main. Both OpenSSH fixture sets exercise the merged parser.
private struct PublishedECDSAFixture {
    let curve: SSHECDSACurve
    let fingerprint: String
    let publicBase64: String
    let pem: String
}

// Public synthetic identities, generated independently with OpenSSH ssh-keygen
// -t ecdsa -b <256|384|521> -N '' -C PUBLIC_SYNTHETIC_FIXTURE_NEVER_USE.
// Fingerprints/public blobs are the corresponding ssh-keygen -lf / .pub output.
// These keys are test data only and MUST NOT be authorized on any real host.
private let ecdsaFixtures: [PublishedECDSAFixture] = [
    .init(curve: .nistp256,
          fingerprint: "SHA256:tmPrFNXD0Q2kVuoDTv7NIWb5V1NPZaZTCI3I0RmUu8M",
          publicBase64: "AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFdpmH3hh4BcviMteVqPZyuUCsZe7Oh4ooq67jD0jPzVuawq4i5Yl2Qa1hFrMBjg/8jPLhXND7GlAjx6XFqdTqY=",
          pem: """
          -----BEGIN OPENSSH PRIVATE KEY-----
          b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
          1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQRXaZh94YeAXL4jLXlaj2crlArGXuzo
          eKKKuu4w9Iz81bmsKuIuWJdkGtYRazAY4P/Izy4VzQ+xpQI8elxanU6mAAAAwJIHSRaSB0
          kWAAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBFdpmH3hh4BcviMt
          eVqPZyuUCsZe7Oh4ooq67jD0jPzVuawq4i5Yl2Qa1hFrMBjg/8jPLhXND7GlAjx6XFqdTq
          YAAAAhANoLuJOJ11CniSwr795gwIT+RaE3TjFUDUpsTttL53TxAAAAIlBVQkxJQ19TWU5U
          SEVUSUNfRklYVFVSRV9ORVZFUl9VU0UBAgMEBQ==
          -----END OPENSSH PRIVATE KEY-----
          """),
    .init(curve: .nistp384,
          fingerprint: "SHA256:L4LjtUFMP+4JTZNioMQ97UN2aVVqjFbI7o59E2L0RT4",
          publicBase64: "AAAAE2VjZHNhLXNoYTItbmlzdHAzODQAAAAIbmlzdHAzODQAAABhBGjjMXwPyRFHeidJKU2OIat9qh96DbqCaBHaMNkgbTeLhLq6V+EktjcO6sWMS+/9VQx5AR1gSSqdRKCOmvxXSMMxLLM8WMIR3kkWYCoMya1JvpMK81qE3LUx4r5j80BlyQ==",
          pem: """
          -----BEGIN OPENSSH PRIVATE KEY-----
          b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAiAAAABNlY2RzYS
          1zaGEyLW5pc3RwMzg0AAAACG5pc3RwMzg0AAAAYQRo4zF8D8kRR3onSSlNjiGrfaofeg26
          gmgR2jDZIG03i4S6ulfhJLY3DurFjEvv/VUMeQEdYEkqnUSgjpr8V0jDMSyzPFjCEd5JFm
          AqDMmtSb6TCvNahNy1MeK+Y/NAZckAAADw5TioAuU4qAIAAAATZWNkc2Etc2hhMi1uaXN0
          cDM4NAAAAAhuaXN0cDM4NAAAAGEEaOMxfA/JEUd6J0kpTY4hq32qH3oNuoJoEdow2SBtN4
          uEurpX4SS2Nw7qxYxL7/1VDHkBHWBJKp1EoI6a/FdIwzEsszxYwhHeSRZgKgzJrUm+kwrz
          WoTctTHivmPzQGXJAAAAMAaMAqsZt1r0HqSaXrLhX/EaAkF6Plg4RFpo7PgIWFixqzUX6/
          7gvJ2MXUQG1a63fwAAACJQVUJMSUNfU1lOVEhFVElDX0ZJWFRVUkVfTkVWRVJfVVNFAQID
          BAUG
          -----END OPENSSH PRIVATE KEY-----
          """),
    .init(curve: .nistp521,
          fingerprint: "SHA256:+or1F5gHxgrrryOapQtTZmDwFQvUNQ7g6SCihhoFy5U",
          publicBase64: "AAAAE2VjZHNhLXNoYTItbmlzdHA1MjEAAAAIbmlzdHA1MjEAAACFBAFlKbOzh3OE/Mln0e+raBDLZgFd80LrOYoNnmyipNTVmgB+ed5xbMDQTIn8pjTGwUWGkyWe2BZE/1j3kdEHcf1P6QDA6AeeJ1chYt2aWXekN4m6KB/gwE+IychEmdp2ewxn0dhMdmR0VKQfqAI2fpHWh8PGRLigNx627qgZ7IzwImRLdw==",
          pem: """
          -----BEGIN OPENSSH PRIVATE KEY-----
          b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAArAAAABNlY2RzYS
          1zaGEyLW5pc3RwNTIxAAAACG5pc3RwNTIxAAAAhQQBZSmzs4dzhPzJZ9Hvq2gQy2YBXfNC
          6zmKDZ5soqTU1ZoAfnnecWzA0EyJ/KY0xsFFhpMlntgWRP9Y95HRB3H9T+kAwOgHnidXIW
          Ldmll3pDeJuigf4MBPiMnIRJnadnsMZ9HYTHZkdFSkH6gCNn6R1ofDxkS4oDcetu6oGeyM
          8CJkS3cAAAEg1zHqn9cx6p8AAAATZWNkc2Etc2hhMi1uaXN0cDUyMQAAAAhuaXN0cDUyMQ
          AAAIUEAWUps7OHc4T8yWfR76toEMtmAV3zQus5ig2ebKKk1NWaAH553nFswNBMifymNMbB
          RYaTJZ7YFkT/WPeR0Qdx/U/pAMDoB54nVyFi3ZpZd6Q3ibooH+DAT4jJyESZ2nZ7DGfR2E
          x2ZHRUpB+oAjZ+kdaHw8ZEuKA3HrbuqBnsjPAiZEt3AAAAQgFUcuNpAmwCx0Wwp3pZcTTg
          BcbXVKptTat7RHkT8FXHob7f8rdbQUTk9M3kp4sc+QpJ4mMPSeeoQ+ZJu3aa92p2lAAAAC
          JQVUJMSUNfU1lOVEhFVElDX0ZJWFRVUkVfTkVWRVJfVVNF
          -----END OPENSSH PRIVATE KEY-----
          """)
]

private func ecdsaSSHString(_ value: Data) -> Data {
    var encoded = Data()
    encoded.appendBE(UInt64(value.count), bytes: 4)
    return encoded + value
}

/// Structural mutation helper. The accepted source of truth remains the
/// independent OpenSSH fixture; this helper is used only for rejection cases.
private struct ECDSAContainer {
    var algorithm, curveName, publicPoint: Data
    var privateAlgorithm, privateCurveName, privatePoint, scalar, comment: Data
    let check: Data

    init(_ data: Data) throws {
        var outer = ByteCursor(data)
        _ = try outer.take(15)
        for _ in 0..<3 { _ = try outer.sshString() }
        _ = try outer.uint(4)
        var publicPart = ByteCursor(try outer.sshString())
        var privatePart = ByteCursor(try outer.sshString())
        algorithm = try publicPart.sshString(); curveName = try publicPart.sshString(); publicPoint = try publicPart.sshString()
        check = try privatePart.take(8)
        privateAlgorithm = try privatePart.sshString(); privateCurveName = try privatePart.sshString()
        privatePoint = try privatePart.sshString(); scalar = try privatePart.sshString(); comment = try privatePart.sshString()
    }
    func encoded() -> Data {
        let publicBlob = ecdsaSSHString(algorithm) + ecdsaSSHString(curveName) + ecdsaSSHString(publicPoint)
        var privatePart = check + ecdsaSSHString(privateAlgorithm) + ecdsaSSHString(privateCurveName)
            + ecdsaSSHString(privatePoint) + ecdsaSSHString(scalar) + ecdsaSSHString(comment)
        var padding: UInt8 = 1
        while !privatePart.count.isMultiple(of: 8) { privatePart.append(padding); padding += 1 }
        return Data("openssh-key-v1\0".utf8) + ecdsaSSHString(Data("none".utf8)) + ecdsaSSHString(Data("none".utf8))
            + ecdsaSSHString(Data()) + Data([0, 0, 0, 1]) + ecdsaSSHString(publicBlob) + ecdsaSSHString(privatePart)
    }
}

@MainActor
final class ECDSAPublishedKeyTests: XCTestCase {
    private let crypto = TestCryptography()

    private func key(_ fixture: PublishedECDSAFixture) throws -> PrivateKeyRecord {
        try PrivateKeyRecord.fromPEM(Data(fixture.pem.utf8), credentialID: "key", using: crypto)
    }
    private func workspace(_ fixture: PublishedECDSAFixture) throws -> WorkspaceDocument {
        var root = try XCTUnwrap(JSONSerialization.jsonObject(with: sampleManifest(fingerprint: fixture.fingerprint)) as? [String: Any])
        var workspace = try XCTUnwrap(root["workspace"] as? [String: Any])
        var credentials = try XCTUnwrap(workspace["credentials"] as? [[String: Any]])
        credentials[0]["algorithm"] = fixture.curve.algorithm
        workspace["credentials"] = credentials; root["workspace"] = workspace
        return try WorkspaceDocument(data: JSONSerialization.data(withJSONObject: root, options: [.sortedKeys]))
    }

    func testIndependentOpenSSHFixturesRoundTripThroughAuthenticatedArchive() async throws {
        for fixture in ecdsaFixtures {
            let key = try key(fixture)
            XCTAssertEqual(key.publicBlob, try XCTUnwrap(Data(base64Encoded: fixture.publicBase64)))
            XCTAssertEqual("SHA256:" + crypto.sha256(key.publicBlob).base64EncodedString().replacingOccurrences(of: "=", with: ""), fixture.fingerprint)
            let archive = try AirGapArchive(workspace: workspace(fixture), keys: [key], using: crypto)
            let export = try FTAG.seal(archive, using: crypto)
            let restored = try FTAG.open(export.envelope, code: export.code, using: crypto)
            XCTAssertEqual(restored.keys.count, 1)
            XCTAssertEqual(restored.keys[0].container, key.container)
            XCTAssertEqual(restored.keys[0].publicBlob, key.publicBlob)
            XCTAssertEqual(restored.workspace.credentials[0].algorithm, fixture.curve.algorithm)
            XCTAssertNil(export.envelope.range(of: key.container))
        }
    }

    func testCurveAlgorithmAndPublicPrivateBindingsRejectSubstitution() async throws {
        for fixture in ecdsaFixtures {
            let original = try ECDSAContainer(key(fixture).container)
            var variants: [ECDSAContainer] = []
            var altered = original; altered.curveName = Data("nistp999".utf8); variants.append(altered)
            altered = original; altered.privateCurveName = Data("nistp999".utf8); variants.append(altered)
            altered = original; altered.privateAlgorithm = Data("ssh-ed25519".utf8); variants.append(altered)
            altered = original; altered.publicPoint[1] ^= 1; variants.append(altered)
            altered = original; altered.privatePoint[1] ^= 1; variants.append(altered)
            altered = original; altered.publicPoint[1] ^= 1; altered.privatePoint = altered.publicPoint; variants.append(altered)
            altered = original; altered.scalar = Data([1]); variants.append(altered)
            altered = original; altered.publicPoint[0] = 6; altered.privatePoint = altered.publicPoint; variants.append(altered)
            altered = original; altered.publicPoint = Data([2]) + original.publicPoint[1..<1 + fixture.curve.scalarBytes]
            altered.privatePoint = altered.publicPoint; variants.append(altered)
            for invalid in variants {
                XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(invalid.encoded(), using: crypto))
            }
            XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(try key(fixture).container + Data([0]), using: crypto))
        }
    }

    func testPrivateScalarsRejectNoncanonicalNegativeZeroAndOutOfRangeValues() async throws {
        for fixture in ecdsaFixtures {
            let original = try ECDSAContainer(key(fixture).container)
            let order = try fixture.curve.order
            let encodedOrder = order[0] & 0x80 == 0 ? order : Data([0]) + order
            let invalids = [Data(), Data([0]), Data([0, 0]), Data([0x80]), Data([0, 1]),
                            Data(repeating: 1, count: fixture.curve.scalarBytes + 1), encodedOrder,
                            Data([0]) + original.scalar]
            for scalar in invalids {
                var reader = ByteCursor(ecdsaSSHString(scalar))
                XCTAssertThrowsError(try reader.sshScalar(curve: fixture.curve))
                var invalid = original; invalid.scalar = scalar
                XCTAssertThrowsError(try PrivateKeyRecord.validateContainer(invalid.encoded(), using: crypto))
            }
            XCTAssertThrowsError(try crypto.ecdsaPublicKey(scalar: Data(repeating: 0, count: fixture.curve.scalarBytes), curve: fixture.curve))
            XCTAssertThrowsError(try crypto.ecdsaPublicKey(scalar: order, curve: fixture.curve))
            // A short positive mpint is canonical and is left-padded for the
            // platform API; it is not mistaken for an invalid short raw key.
            var oneReader = ByteCursor(ecdsaSSHString(Data([1])))
            let one = try oneReader.sshScalar(curve: fixture.curve)
            XCTAssertEqual(one, Data(repeating: 0, count: fixture.curve.scalarBytes - 1) + Data([1]))
            XCTAssertEqual(try crypto.ecdsaPublicKey(scalar: one, curve: fixture.curve).count, 1 + 2 * fixture.curve.scalarBytes)
            var last = order; last[last.count - 1] -= 1
            XCTAssertNoThrow(try fixture.curve.validateScalar(last))
            XCTAssertNoThrow(try crypto.ecdsaPublicKey(scalar: last, curve: fixture.curve))
        }
    }

    func testManifestAlgorithmAndCertificateRestrictionsRemainBoundToPrivateIdentity() async throws {
        for fixture in ecdsaFixtures {
            let key = try key(fixture)
            XCTAssertThrowsError(try AirGapArchive(workspace: WorkspaceDocument(data: sampleManifest(fingerprint: fixture.fingerprint)), keys: [key], using: crypto))
            let forged = PrivateKeyRecord(credentialID: "key", container: key.container, publicBlob: key.publicBlob + Data([0]))
            XCTAssertThrowsError(try AirGapArchive(workspace: workspace(fixture), keys: [forged], using: crypto))
            let uncertified = PrivateKeyRecord(credentialID: "key", container: key.container, publicBlob: key.publicBlob, certificates: [Data([1])])
            XCTAssertThrowsError(try AirGapArchive(workspace: workspace(fixture), keys: [uncertified], using: crypto))
        }
    }

    func testECDSARestoreUsesOwnedCredentialTransactionAndStaysOffline() async throws {
        for fixture in ecdsaFixtures {
            let vault = StoreTestVault(), persistence = StoreTestPersistence()
            let store = WorkspaceStore(persistence: persistence, vault: vault)
            let archive = try AirGapArchive(workspace: workspace(fixture), keys: [key(fixture)], using: crypto)
            let export = try FTAG.seal(archive, using: crypto)
            let decoded = try FTAG.open(export.envelope, code: export.code, using: crypto)
            let receipt = try await store.install(decoded, envelopeDigest: crypto.sha256(export.envelope),
                                                  transferID: FTAGHeader(envelope: export.envelope).transferID,
                                                  expectedRevision: nil, sourceAndContentsConfirmed: true)
            XCTAssertTrue(receipt.missingCredentialIDs.isEmpty)
            XCTAssertEqual(receipt.workspace.bindings.first?.fingerprint, fixture.fingerprint)
            XCTAssertTrue(receipt.workspace.connectLater)
            XCTAssertEqual(vault.count, 1)
            let reloaded = try await WorkspaceStore(persistence: persistence, vault: vault).load()
            XCTAssertEqual(reloaded.first?.bindings, receipt.workspace.bindings)
            XCTAssertEqual(reloaded.first?.connectLater, true)
        }
    }
}
