import Foundation
import XCTest
#if canImport(Darwin)
import Darwin
#elseif canImport(Glibc)
import Glibc
#endif
@testable import FrankenTermKit

@MainActor
final class SSHWorkspaceIdentityFilesTests: XCTestCase {
    private let crypto = TestCryptography()

    /// These deliberately public test keys were generated independently by
    /// OpenSSH ssh-keygen, including protected files for every supported curve.
    private struct Fixtures: Decodable {
        struct Pair: Decodable {
            let id, algorithm, fingerprint, encryptedPEM, clearPEM: String
        }
        let passphrase: String
        let pairs: [Pair]
        static func load() throws -> Self {
            var root = URL(fileURLWithPath: #filePath)
            for _ in 0..<6 { root.deleteLastPathComponent() }
            return try JSONDecoder().decode(Self.self,
                from: Data(contentsOf: root.appendingPathComponent("fixtures/ios-airgap/openssh-source-keys.json")))
        }
    }

    private func files(_ entries: [String: Data]) throws -> URL {
        let selected = FileManager.default.temporaryDirectory.appendingPathComponent("ft-ssh-identities-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: selected.appendingPathComponent(".ssh"), withIntermediateDirectories: true)
        let physical = try XCTUnwrap(realpath(selected.path, nil))
        defer { free(physical) }
        let home = URL(fileURLWithPath: String(cString: physical), isDirectory: true)
        for (name, bytes) in entries {
            let url = home.appendingPathComponent(".ssh/" + name)
            try FileManager.default.createDirectory(at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
            try bytes.write(to: url)
        }
        return home
    }

    private func source(_ configuration: String) throws -> SSHWorkspaceSource {
        try SSHWorkspaceSource(configuration: Data(configuration.utf8))
    }

    private func publicFile(_ fixture: Fixtures.Pair) throws -> Data {
        let key = try PrivateKeyRecord.fromPEM(Data(fixture.clearPEM.utf8), credentialID: "fixture", using: crypto)
        return Data((fixture.algorithm + " " + key.publicBlob.base64EncodedString() + " public-test-fixture\n").utf8)
    }

    private func export(_ source: SSHWorkspaceSource, identities: SSHWorkspaceSource.PublicIdentitySnapshot,
                        aliases: [String]? = nil) throws -> SSHWorkspaceExport {
        var registry = SSHWorkspaceRegistry()
        return try source.export(aliases: aliases ?? source.aliases, workspaceName: "Selected SSH keys", defaultUser: "operator",
            publicIdentityFiles: identities, registry: &registry, using: crypto)
    }

    private func publicBlob(_ reference: String, from snapshot: SSHWorkspaceSource.PublicIdentitySnapshot) throws -> Data {
        guard case .present(let blob)? = snapshot.files[reference] else {
            XCTFail("The selected reference has no public identity.")
            throw FTError.stale
        }
        return blob
    }

    func testAllSupportedClearAndEncryptedKeysSupplyPublicIdentityWithoutSidecars() async throws {
        let fixtures = try Fixtures.load()
        XCTAssertEqual(fixtures.pairs.count, 6)
        XCTAssertEqual(Set(fixtures.pairs.map(\.algorithm)).count, 4)
        var configuration = "", entries: [String: Data] = [:]
        for (index, fixture) in fixtures.pairs.enumerated() {
            for (suffix, pem) in [("clear", fixture.clearPEM), ("encrypted", fixture.encryptedPEM)] {
                let name = "key-\(index)-\(suffix)"
                entries[name] = Data(pem.utf8)
                configuration += "Host \(name)\nIdentityFile ~/.ssh/\(name)\n"
            }
        }
        let home = try files(entries), input = try source(configuration)
        let publicKeys = try input.loadPublicIdentityFiles(for: input.aliases, homeDirectory: home)
        XCTAssertEqual(publicKeys.files.count, fixtures.pairs.count * 2)
        for (index, fixture) in fixtures.pairs.enumerated() {
            let independent = try PrivateKeyRecord.fromPEM(Data(fixture.clearPEM.utf8), credentialID: "fixture", using: crypto)
            for suffix in ["clear", "encrypted"] {
                let blob = try publicBlob("~/.ssh/key-\(index)-\(suffix)", from: publicKeys)
                XCTAssertEqual(blob, independent.publicBlob, fixture.id)
                XCTAssertLessThan(blob.count, 2048)
                XCTAssertNil(blob.range(of: Data("PRIVATE KEY".utf8)))
            }
        }
        let result = try export(input, identities: publicKeys)
        XCTAssertEqual(Set(result.workspace.credentials.map(\.fingerprint)), Set(fixtures.pairs.map(\.fingerprint)))
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.credentialIDs.count == 1 })
        XCTAssertFalse(String(decoding: result.workspace.raw, as: UTF8.self).contains("~/.ssh/"))
        let linkedHome = home.deletingLastPathComponent().appendingPathComponent("ft-identity-home-" + UUID().uuidString)
        try FileManager.default.createSymbolicLink(at: linkedHome, withDestinationURL: home)
        XCTAssertEqual(try input.loadPublicIdentityFiles(for: input.aliases, homeDirectory: linkedHome).files, publicKeys.files)
    }

    func testDiscoveredIdentityRequiresExplicitKeyInclusionAndSurvivesEncryptedArchive() async throws {
        let fixtures = try Fixtures.load(), fixture = try XCTUnwrap(fixtures.pairs.first)
        for encrypted in [false, true] {
            let pem = Data((encrypted ? fixture.encryptedPEM : fixture.clearPEM).utf8)
            let home = try files(["selected": pem])
            let input = try source("Host build\nIdentityFile ~/.ssh/selected\nPreferredAuthentications publickey\n")
            let result = try export(input, identities: input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home))
            let reference = try XCTUnwrap(result.workspace.credentials.first)
            XCTAssertEqual(reference.fingerprint, fixture.fingerprint)
            XCTAssertEqual(result.identityReferences[reference.id], "~/.ssh/selected")
            let selection = try WorkspaceExportSelection(workspace: result.workspace,
                groupIDs: [result.workspace.initialGroupID], findings: result.portability)
            XCTAssertFalse(try selection.review(decisions: [:], loadedFingerprints: []).canPrepare)
            let approved: [String: WorkspaceExportSelection.IdentityDecision] = [fixture.fingerprint: .includePrivateKey]
            XCTAssertFalse(try selection.review(decisions: approved, loadedFingerprints: []).canPrepare)
            let publicOnly = try selection.archive(decisions: [fixture.fingerprint: .keepReferenceOnly], keys: [],
                bundleID: UUID().uuidString, using: crypto)
            XCTAssertTrue(publicOnly.keys.isEmpty)

            // This is the separate, explicit Choose Matching Key action used by
            // the native exporter; discovery itself never executes this unlock.
            var unlocker = OpenSSHSourceKeyUnlocker(using: crypto)
            let key: PrivateKeyRecord
            switch try unlocker.select(pem: Data(contentsOf: home.appendingPathComponent(".ssh/selected")), reference: reference) {
            case let .ready(value):
                XCTAssertFalse(encrypted); key = value
            case let .passphraseRequired(request):
                XCTAssertTrue(encrypted)
                XCTAssertEqual(request.fingerprint, fixture.fingerprint)
                XCTAssertThrowsError(try unlocker.unlock(requestID: request.id, passphrase: Data("incorrect-fixture-passphrase".utf8))) {
                    XCTAssertEqual($0 as? OpenSSHSourceKeyError, .incorrectPassphraseOrDamagedKey)
                }
                key = try unlocker.unlock(requestID: request.id, passphrase: Data(fixtures.passphrase.utf8))
            }
            XCTAssertThrowsError(try selection.archive(decisions: [:], keys: [key], bundleID: UUID().uuidString, using: crypto))
            let archive = try selection.archive(decisions: approved, keys: [key], bundleID: UUID().uuidString, using: crypto)
            let sealed = try FTAG.seal(archive, using: crypto)
            let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
            XCTAssertEqual(restored.keys.count, 1)
            XCTAssertEqual(restored.keys[0].publicBlob, key.publicBlob)
            XCTAssertEqual(try PrivateKeyRecord.validateContainer(restored.keys[0].container, using: crypto), key.publicBlob)
            XCTAssertEqual(restored.workspace.credentials[0].fingerprint, fixture.fingerprint)
            XCTAssertEqual(try Data(contentsOf: home.appendingPathComponent(".ssh/selected")), pem)
            let manifest = String(decoding: restored.workspace.raw, as: UTF8.self)
            for secret in ["~/.ssh/", "PRIVATE KEY", fixtures.passphrase, key.container.base64EncodedString()] {
                XCTAssertFalse(manifest.contains(secret))
            }
        }
    }

    func testPublicSidecarWinsAndThePrivateFileIsNotInspected() async throws {
        let fixtures = try Fixtures.load(), publicBytes = try publicFile(fixtures.pairs[1])
        let home = try files(["key.pub": publicBytes, "key": Data("not an OpenSSH container".utf8)])
        try FileManager.default.setAttributes([.posixPermissions: 0o666], ofItemAtPath: home.appendingPathComponent(".ssh/key").path)
        let input = try source("Host build\nIdentityFile ~/.ssh/key\n")
        let snapshot = try input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home)
        let independent = try PrivateKeyRecord.fromPEM(Data(fixtures.pairs[1].clearPEM.utf8), credentialID: "fixture", using: crypto)
        XCTAssertEqual(try publicBlob("~/.ssh/key", from: snapshot), independent.publicBlob)
        let reference = try XCTUnwrap(export(input, identities: snapshot).workspace.credentials.first)
        XCTAssertEqual(reference.fingerprint, fixtures.pairs[1].fingerprint)
        var unlocker = OpenSSHSourceKeyUnlocker(using: crypto)
        XCTAssertThrowsError(try unlocker.select(pem: Data(fixtures.pairs[0].clearPEM.utf8), reference: reference)) {
            XCTAssertEqual($0 as? OpenSSHSourceKeyError, .identityMismatch)
        }
    }

    func testMalformedPresentSidecarNeverFallsBackToValidPrivateIdentity() async throws {
        let fixture = try XCTUnwrap(Fixtures.load().pairs.first)
        let malformed = Data("not a public key\n".utf8)
        let home = try files(["key.pub": malformed, "key": Data(fixture.clearPEM.utf8)])
        let input = try source("Host build\nIdentityFile ~/.ssh/key\n")
        let snapshot = try input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home)
        XCTAssertEqual(snapshot.files, ["~/.ssh/key": .unavailable])
        let result = try export(input, identities: snapshot)
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.identityfile" && $0.disposition == 1 })
        XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty)
    }

    func testExplicitPublicReferenceAndMissingFilesDoNotAuthorizeOtherPrivateFiles() async throws {
        let fixture = try XCTUnwrap(Fixtures.load().pairs.first)
        let home = try files(["key": Data(fixture.clearPEM.utf8), "key.pub.pub": try publicFile(fixture)])
        let input = try source("Host build\nIdentityFile ~/.ssh/key.pub\nIdentityFile ~/.ssh/absent\n")
        let snapshot = try input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home)
        XCTAssertEqual(snapshot.files, ["~/.ssh/key.pub": .missing, "~/.ssh/absent": .missing])
        let result = try export(input, identities: snapshot)
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.credentials" })
    }

    func testOnlySelectedDestinationsAndTheirJumpIdentitiesAreRead() async throws {
        let fixtures = try Fixtures.load()
        let home = try files(["target": Data(fixtures.pairs[0].encryptedPEM.utf8),
                              "gate": Data(fixtures.pairs[1].clearPEM.utf8),
                              "unselected.pub": Data("must not be read".utf8)])
        try FileManager.default.setAttributes([.posixPermissions: 0o666], ofItemAtPath: home.appendingPathComponent(".ssh/unselected.pub").path)
        let input = try source("""
        Host target
          ProxyJump gate
          IdentityFile ~/.ssh/target
        Host gate
          IdentityFile ~/.ssh/gate
        Host unselected
          IdentityFile ~/.ssh/unselected
        Host disabled
          PubkeyAuthentication no
          IdentityFile ~/.ssh/unselected
        Host none
          IdentityFile none
        """)
        let snapshot = try input.loadPublicIdentityFiles(for: ["target", "disabled", "none"], homeDirectory: home)
        XCTAssertEqual(Set(snapshot.files.keys), ["~/.ssh/target", "~/.ssh/gate"])
        XCTAssertThrowsError(try input.loadPublicIdentityFiles(for: ["unselected"], homeDirectory: home))
    }

    func testSymbolicLinksUnsafePermissionsAndSpecialFilesNeverTriggerFallback() async throws {
        let fixture = try XCTUnwrap(Fixtures.load().pairs.first)
        for variant in ["sidecar-link", "dangling-sidecar", "private-link", "parent-link", "sidecar-permissions",
                        "private-permissions", "sidecar-directory", "private-directory", "sidecar-fifo", "private-fifo"] {
            var entries = ["real": Data(fixture.clearPEM.utf8), "real.pub": try publicFile(fixture)]
            if variant.hasPrefix("sidecar") || variant == "dangling-sidecar" { entries["key"] = Data(fixture.clearPEM.utf8) }
            if variant == "sidecar-permissions" { entries["key.pub"] = try publicFile(fixture) }
            if variant == "private-permissions" { entries["key"] = Data(fixture.clearPEM.utf8) }
            let home = try files(entries), key = home.appendingPathComponent(".ssh/key")
            switch variant {
            case "sidecar-link", "dangling-sidecar":
                try FileManager.default.createSymbolicLink(at: key.appendingPathExtension("pub"),
                    withDestinationURL: home.appendingPathComponent(variant == "sidecar-link" ? ".ssh/real.pub" : ".ssh/absent"))
            case "private-link":
                try FileManager.default.createSymbolicLink(at: key, withDestinationURL: home.appendingPathComponent(".ssh/real"))
            case "parent-link":
                try FileManager.default.createSymbolicLink(at: home.appendingPathComponent("link"), withDestinationURL: home.appendingPathComponent(".ssh"))
            case "sidecar-permissions", "private-permissions":
                try FileManager.default.setAttributes([.posixPermissions: 0o666],
                    ofItemAtPath: (variant == "sidecar-permissions" ? key.appendingPathExtension("pub") : key).path)
            case "sidecar-directory", "private-directory":
                try FileManager.default.createDirectory(at: variant == "sidecar-directory" ? key.appendingPathExtension("pub") : key,
                    withIntermediateDirectories: false)
            default:
                let pipe = variant == "sidecar-fifo" ? key.appendingPathExtension("pub") : key
                XCTAssertEqual(mkfifo(pipe.path, 0o600), 0)
            }
            let input = try source("Host build\nIdentityFile \(variant == "parent-link" ? "~/link/real" : "~/.ssh/key")\n")
            XCTAssertThrowsError(try input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home), variant)
        }
    }

    func testPrivateContainerPreflightReportsUnavailableAndParentTraversalFailsClosed() async throws {
        let home = try files(["key": Data("not an OpenSSH container".utf8)])
        let input = try source("Host build\nIdentityFile ~/.ssh/key\n")
        XCTAssertEqual(try input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home).files,
            ["~/.ssh/key": .unavailable])
        let traversal = try source("Host build\nIdentityFile ~/.ssh/../key\n")
        XCTAssertThrowsError(try traversal.loadPublicIdentityFiles(for: ["build"], homeDirectory: home)) {
            XCTAssertEqual($0 as? FTError, .unsupported("ssh.identity.reference"))
        }
    }

    func testUnavailableDefaultContainerDoesNotDiscardOtherSelectedMachinesOrKeys() async throws {
        let fixture = try XCTUnwrap(Fixtures.load().pairs.first)
        let home = try files(["id_rsa": Data("-----BEGIN RSA PRIVATE KEY-----\nunsupported fixture\n-----END RSA PRIVATE KEY-----\n".utf8),
                              "id_ed25519": Data(fixture.encryptedPEM.utf8)])
        let input = try source("Host defaults\nHost supported\nIdentityFile ~/.ssh/id_ed25519\n")
        let inventory = try input.loadPublicIdentityFiles(for: input.aliases, homeDirectory: home)
        XCTAssertEqual(inventory.files["~/.ssh/id_rsa"], .unavailable)
        XCTAssertNotNil(try publicBlob("~/.ssh/id_ed25519", from: inventory))
        let result = try export(input, identities: inventory)
        XCTAssertEqual(result.workspace.machines.map(\.name), ["defaults", "supported"])
        XCTAssertEqual(result.workspace.credentials.map(\.fingerprint), [fixture.fingerprint])
        let supportedID = try XCTUnwrap(result.workspace.machines.first { $0.name == "supported" }?.preferredRouteID)
        let defaultsID = try XCTUnwrap(result.workspace.machines.first { $0.name == "defaults" }?.preferredRouteID)
        XCTAssertTrue(try XCTUnwrap(result.workspace.routes.first { $0.id == supportedID }).requiredCapabilities.isEmpty)
        XCTAssertFalse(try XCTUnwrap(result.workspace.routes.first { $0.id == defaultsID }).requiredCapabilities.isEmpty)
        XCTAssertTrue(result.portability.contains { $0.entityID == defaultsID && $0.setting == "ssh_config.identityfile" && $0.disposition == 1 })
        let selection = try WorkspaceExportSelection(workspace: result.workspace,
            groupIDs: [result.workspace.initialGroupID], findings: result.portability)
        let publicOnly = try selection.archive(decisions: [fixture.fingerprint: .keepReferenceOnly], keys: [],
            bundleID: UUID().uuidString, using: crypto)
        XCTAssertEqual(publicOnly.workspace.machines.count, 2)
        XCTAssertTrue(publicOnly.keys.isEmpty)
    }

    func testPublicSnapshotRejectsPrivateBytesAmbiguousAndUnselectedSourcesAndBindsFailureState() async throws {
        let fixture = try XCTUnwrap(Fixtures.load().pairs.first), key = try publicFile(fixture)
        XCTAssertThrowsError(try SSHWorkspaceSource.PublicIdentitySnapshot(files: ["~/.ssh/key": .present(Data(fixture.clearPEM.utf8))]))
        let input = try source("Host build\nIdentityFile ~/.ssh/key\n")
        let selected = try SSHWorkspaceSource.PublicIdentitySnapshot(files: ["~/.ssh/key": .present(key)])
        var registry = SSHWorkspaceRegistry()
        XCTAssertThrowsError(try input.export(aliases: ["build"], workspaceName: "Test", defaultUser: "operator",
            publicKeys: ["~/.ssh/key": key], publicIdentityFiles: selected, registry: &registry, using: crypto)) {
            XCTAssertEqual($0 as? FTError, .malformed("ssh.identity.ambiguous_sources"))
        }
        let unselected = try SSHWorkspaceSource.PublicIdentitySnapshot(files: ["~/.ssh/other": .present(key)])
        XCTAssertThrowsError(try export(input, identities: unselected)) {
            XCTAssertEqual($0 as? FTError, .malformed("ssh.identity.unselected_source"))
        }
        var revisions = Set<String>()
        for state: SSHWorkspaceSource.PublicIdentitySnapshot.File in [.present(key), .missing, .unavailable] {
            let snapshot = try SSHWorkspaceSource.PublicIdentitySnapshot(files: ["~/.ssh/key": state])
            let result = try export(input, identities: snapshot)
            let revision = try StrictJSON.decode(result.workspace.raw).object().required("source").object().text("configuration_revision")
            revisions.insert(revision)
        }
        XCTAssertEqual(revisions.count, 3)
    }

    func testPerFileAndAggregateIdentityReadBudgetsAreEnforced() async throws {
        for name in ["key", "key.pub"] {
            let home = try files([name: Data(repeating: 0x61, count: FTLimit.key + 1)])
            let input = try source("Host build\nIdentityFile ~/.ssh/key\n")
            XCTAssertThrowsError(try input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home)) {
                XCTAssertEqual($0 as? FTError, .limit("ssh.identity.bytes"))
            }
        }
        let fixture = try XCTUnwrap(Fixtures.load().pairs.first)
        var padded = try publicFile(fixture)
        padded.append(Data(repeating: 0x20, count: FTLimit.key - padded.count))
        let count = SSHWorkspaceSource.maximumSourceBytes / FTLimit.key
        var entries: [String: Data] = [:], configuration = ""
        for index in 0...count {
            entries["key-\(index).pub"] = padded
            configuration += "Host host-\(index)\nIdentityFile ~/.ssh/key-\(index)\n"
        }
        let home = try files(entries), input = try source(configuration)
        XCTAssertEqual(try input.loadPublicIdentityFiles(for: Array(input.aliases.prefix(count)), homeDirectory: home).files.count, count)
        XCTAssertThrowsError(try input.loadPublicIdentityFiles(for: input.aliases, homeDirectory: home)) {
            XCTAssertEqual($0 as? FTError, .limit("ssh.identity.bytes"))
        }
    }

    func testCancelledInventoryDoesNotReadOrPublishIdentities() async throws {
        let fixture = try XCTUnwrap(Fixtures.load().pairs.first)
        let home = try files(["key": Data(fixture.encryptedPEM.utf8)])
        let input = try source("Host build\nIdentityFile ~/.ssh/key\n")
        let cancelled = Task {
            withUnsafeCurrentTask { $0?.cancel() }
            return try input.loadPublicIdentityFiles(for: ["build"], homeDirectory: home)
        }
        do { _ = try await cancelled.value; XCTFail("A cancelled source read must not publish a snapshot.") }
        catch is CancellationError { }
    }
}
