import Foundation
import XCTest
@testable import FrankenTermKit

final class SSHWorkspaceSourceTests: XCTestCase {
    private let crypto = TestCryptography()
    private func source(_ text: String) throws -> SSHWorkspaceSource {
        try SSHWorkspaceSource(configuration: Data(text.utf8))
    }
    private func publicFile(_ seed: UInt8 = 0) throws -> Data {
        let raw = try crypto.ed25519PublicKey(seed: Data((0..<32).map { seed &+ UInt8($0) }))
        return Data(("ssh-ed25519 " + publicBlob(raw).base64EncodedString() + " synthetic@example.invalid\n").utf8)
    }
    private func hostLine(_ names: String, seed: UInt8 = 0, marker: String = "") throws -> String {
        (marker.isEmpty ? "" : marker + " ") + names + " " + String(decoding: try publicFile(seed), as: UTF8.self)
    }
    private func hashedName(_ lookup: String) throws -> String {
        let salt = Data(0..<20)
        let digest = try crypto.hmacSHA1(key: salt, message: Data(lookup.utf8))
        return "|1|" + salt.base64EncodedString() + "|" + digest.base64EncodedString()
    }
    private func hashedLine(_ lookup: String, seed: UInt8 = 0, marker: String = "") throws -> String {
        try hostLine(hashedName(lookup), seed: seed, marker: marker)
    }
    private func records(_ result: SSHWorkspaceExport, _ name: String) throws -> [[String: JSONValue]] {
        try StrictJSON.decode(result.workspace.raw).object().required("workspace").object().required(name).array().map { try $0.object() }
    }
    private func exported(_ text: String, aliases: [String]? = nil, keys: [String: Data] = [:],
                          certificates: SSHWorkspaceSource.PublicCertificateSnapshot? = nil,
                          known: Data? = nil) throws -> SSHWorkspaceExport {
        let input = try source(text)
        var registry = SSHWorkspaceRegistry()
        return try input.export(aliases: aliases ?? input.aliases, workspaceName: "Selected machines", defaultUser: "local-user",
                                publicKeys: keys, publicCertificateFiles: certificates, knownHosts: known, registry: &registry, using: crypto)
    }
    private func destination(_ alias: String, in document: WorkspaceDocument) throws -> WorkspaceDocument.Route {
        let machine = try XCTUnwrap(document.machines.first { $0.name == alias })
        return try XCTUnwrap(document.routes.first { $0.id == machine.preferredRouteID })
    }
    private func hops(_ route: WorkspaceDocument.Route, in document: WorkspaceDocument) throws -> [WorkspaceDocument.Route] {
        try route.jumpRoutes.map { id in try XCTUnwrap(document.routes.first { $0.id == id }) }
    }

    private func configurationFiles(_ files: [String: String]) throws -> URL {
        let literal = FileManager.default.temporaryDirectory.appendingPathComponent("ft-ssh-includes-" + UUID().uuidString)
        try FileManager.default.createDirectory(at: literal.appendingPathComponent(".ssh"), withIntermediateDirectories: true)
        // Physical path (macOS temp lives under the /var -> /private/var link), so
        // expected source paths match what the loader records.
        let resolved = try XCTUnwrap(realpath(literal.path, nil))
        defer { free(resolved) }
        let home = URL(fileURLWithPath: String(cString: resolved), isDirectory: true)
        for (path, text) in files {
            let destination = home.appendingPathComponent(".ssh").appendingPathComponent(path)
            try FileManager.default.createDirectory(at: destination.deletingLastPathComponent(), withIntermediateDirectories: true)
            try Data(text.utf8).write(to: destination)
        }
        return home
    }

    private func loadFiles(_ home: URL, root: String = "config") throws -> SSHWorkspaceSource {
        try SSHWorkspaceSource.loadUserConfiguration(home.appendingPathComponent(".ssh").appendingPathComponent(root), homeDirectory: home)
    }

    private func exportSource(_ source: SSHWorkspaceSource) throws -> SSHWorkspaceExport {
        var registry = SSHWorkspaceRegistry()
        return try source.export(aliases: source.aliases, workspaceName: "Included hosts", defaultUser: "fallback",
                                 registry: &registry, using: crypto)
    }

    private func exportFiles(_ source: SSHWorkspaceSource, _ files: SSHWorkspaceSource.KnownHostsSnapshot,
                             aliases: [String]? = nil, keys: [String: Data] = [:]) throws -> SSHWorkspaceExport {
        var registry = SSHWorkspaceRegistry()
        return try source.export(aliases: aliases ?? source.aliases, workspaceName: "File-scoped trust", defaultUser: "operator",
            publicKeys: keys, knownHostFiles: files, registry: &registry, using: crypto)
    }

    private func trust(_ alias: String, in document: WorkspaceDocument) throws -> WorkspaceDocument.HostTrust {
        let route = try destination(alias, in: document)
        return try XCTUnwrap(document.hostTrust.first { $0.id == route.trustID })
    }

    private func sourceRevision(_ document: WorkspaceDocument) throws -> JSONValue? {
        try StrictJSON.decode(document.raw).object().required("source").object()["configuration_revision"]
    }

    func testLocalIncludeInventoryLexicalOrderAndFrozenSource() throws {
        let home = try configurationFiles([
            "config": "Include conf.d/*.conf\nHost *\nIdentityFile none\n",
            "conf.d/20-beta.conf": "Host beta\nHostName beta.example.invalid\nUser beta-user\n",
            "conf.d/10-alpha.conf": "Host alpha\nHostName alpha.example.invalid\nUser alpha-user\n",
            "conf.d/.hidden.conf": "Host hidden\n"
        ])
        let loaded = try loadFiles(home)
        XCTAssertEqual(loaded.aliases, ["alpha", "beta"])
        let first = try exportSource(loaded)
        XCTAssertEqual(first.workspace.routes.map(\.host), ["alpha.example.invalid", "beta.example.invalid"])
        XCTAssertTrue(first.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        XCTAssertFalse(first.portability.contains { $0.setting == "ssh_config.include" })
        try Data("Host alpha\nHostName changed.example.invalid\n".utf8).write(to: home.appendingPathComponent(".ssh/conf.d/10-alpha.conf"))
        XCTAssertEqual(try exportSource(loaded).workspace.routes[0].host, "alpha.example.invalid", "A source selection is frozen before the host picker opens.")
        let changed = try exportSource(loadFiles(home))
        let firstSource = try StrictJSON.decode(first.workspace.raw).object().required("source").object()
        let changedSource = try StrictJSON.decode(changed.workspace.raw).object().required("source").object()
        XCTAssertNotEqual(firstSource["configuration_revision"], changedSource["configuration_revision"], "Included bytes participate in the source revision.")
    }

    func testIncludedHostConditionsCannotEscapeAndParentResumesForEveryFile() throws {
        let home = try configurationFiles([
            "config": "Host alpha beta\nInclude conf.d/*.conf\nUser outer\nHost gamma\nUser gamma-user\nHost *\nIdentityFile none\n",
            "conf.d/10-hosts.conf": "Host alpha\nUser alpha-user\nHost *\nPort 2200\nHost unreachable\nHostName must-not-appear.invalid\n",
            "conf.d/20-defaults.conf": "User included-default\n"
        ])
        let loaded = try loadFiles(home)
        XCTAssertEqual(loaded.aliases, ["alpha", "beta", "gamma"])
        let result = try exportSource(loaded)
        XCTAssertEqual(result.workspace.routes.map(\.user), ["alpha-user", "included-default", "gamma-user"])
        XCTAssertEqual(result.workspace.routes.map(\.port), [2200, 2200, 22])
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
    }

    func testNestedIncludeUsesUserSSHDirectoryAndRestoresOuterHost() throws {
        let home = try configurationFiles([
            "config": "Include conf.d/hosts.conf\nHost gamma\nIdentityFile none\n",
            "conf.d/hosts.conf": "Host alpha\nInclude shared.conf\nUser outer\nHost beta\nUser beta-user\n",
            "shared.conf": "User shared-user\nHost *\nPort 2222\n",
            "conf.d/shared.conf": "User wrong-directory\nPort 9999\n"
        ])
        let result = try exportSource(loadFiles(home))
        XCTAssertEqual(result.workspace.machines.map(\.name), ["alpha", "beta", "gamma"])
        XCTAssertEqual(result.workspace.routes.map(\.user), ["shared-user", "beta-user", "fallback"])
        XCTAssertEqual(result.workspace.routes.map(\.port), [2222, 22, 22])
    }

    /// macOS hands out home/config locations under `/var` and `/tmp`, which are
    /// themselves symbolic links into `/private`. Selecting the configuration and
    /// home through such a linked directory must still load its includes (the
    /// selected locations are canonicalized physically), while links *inside*
    /// the include graph remain rejected by the O_NOFOLLOW walk.
    func testSelectionThroughALinkedDirectoryStillLoadsIncludes() throws {
        let home = try configurationFiles([
            "config": "Include conf.d/*.conf\nHost *\nIdentityFile none\n",
            "conf.d/hosts.conf": "Host alpha\nUser linked-user\n"
        ])
        let link = home.deletingLastPathComponent().appendingPathComponent("ft-ssh-link-" + UUID().uuidString)
        try FileManager.default.createSymbolicLink(at: link, withDestinationURL: home)
        defer { try? FileManager.default.removeItem(at: link) }
        let result = try exportSource(SSHWorkspaceSource.loadUserConfiguration(
            link.appendingPathComponent(".ssh").appendingPathComponent("config"), homeDirectory: link))
        XCTAssertEqual(result.workspace.machines.map(\.name), ["alpha"])
        XCTAssertEqual(result.workspace.routes.map(\.user), ["linked-user"])
    }

    func testIncludeMultiplePathsTildeQuestionMarkDotAndMissingMatches() throws {
        let home = try configurationFiles([
            "config": "Include missing/*.conf missing-file ~/../unsupported\nInclude \"with space.conf\" ~/.ssh/team?.conf .hidden/*.conf\nHost *\nIdentityFile none\n",
            "with space.conf": "Host first\nUser first-user\n",
            "team1.conf": "Host second\n",
            "team10.conf": "Host must-not-match\n",
            ".hidden/host.conf": "Host third\n"
        ])
        let result = try exportSource(loadFiles(home))
        XCTAssertEqual(result.workspace.machines.map(\.name), ["first", "second", "third"])
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.include" && $0.disposition == 1 })
        XCTAssertTrue(result.workspace.routes.allSatisfy { !$0.requiredCapabilities.isEmpty })
    }

    func testUnmatchedIncludeIsNoOpButMatchAndDynamicPathsRemainBlocked() throws {
        let empty = try configurationFiles(["config": "Include absent*.conf missing/subdir/*.conf missing.conf\nHost build\nIdentityFile none\n"])
        let unblocked = try exportSource(loadFiles(empty))
        XCTAssertTrue(unblocked.workspace.routes[0].requiredCapabilities.isEmpty)
        for include in ["%h.conf", "${HOME}/secret.conf", "~someone/config", "[ab].conf", "escaped\\*.conf"] {
            let home = try configurationFiles(["config": "Include \(include)\nHost build\nIdentityFile none\n"])
            let result = try exportSource(loadFiles(home))
            XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.include" && $0.disposition == 1 }, include)
        }
        let marker = FileManager.default.temporaryDirectory.appendingPathComponent("ft-include-no-exec-" + UUID().uuidString)
        let home = try configurationFiles([
            "config": "Include hosts.conf\nHost build\nIdentityFile none\n",
            "hosts.conf": "Match exec \"touch \(marker.path)\"\nInclude never-read.conf\nHost build\nUser included-user\n",
            "never-read.conf": "This deliberately is not a valid SSH file\n"
        ])
        let result = try exportSource(loadFiles(home))
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.match" && $0.disposition == 1 })
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.include" && $0.disposition == 1 })
        XCTAssertFalse(FileManager.default.fileExists(atPath: marker.path))
    }

    func testIncludeRejectsCyclesSymlinksAndUnsafePermissions() throws {
        let cycle = try configurationFiles(["config": "Include second.conf\nHost build\n", "second.conf": "Include config\n"])
        XCTAssertThrowsError(try loadFiles(cycle)) { XCTAssertEqual($0 as? FTError, .malformed("ssh.include_cycle")) }
        let hardLink = try configurationFiles(["config": "Include again.conf\nHost build\n"])
        try FileManager.default.linkItem(at: hardLink.appendingPathComponent(".ssh/config"), to: hardLink.appendingPathComponent(".ssh/again.conf"))
        XCTAssertThrowsError(try loadFiles(hardLink)) { XCTAssertEqual($0 as? FTError, .malformed("ssh.include_cycle")) }
        for directory in [false, true] {
            let home = try configurationFiles(["config": "Include \(directory ? "linked/*.conf" : "linked.conf")\nHost build\n", "actual/host.conf": "Host other\n"])
            let target = home.appendingPathComponent(directory ? ".ssh/actual" : ".ssh/actual/host.conf")
            try FileManager.default.createSymbolicLink(at: home.appendingPathComponent(directory ? ".ssh/linked" : ".ssh/linked.conf"), withDestinationURL: target)
            XCTAssertThrowsError(try loadFiles(home)) { XCTAssertEqual($0 as? FTError, .unsupported("ssh.include_file")) }
        }
        let writable = try configurationFiles(["config": "Include writable.conf\nHost build\n", "writable.conf": "User bad-permissions\n"])
        try FileManager.default.setAttributes([.posixPermissions: 0o666], ofItemAtPath: writable.appendingPathComponent(".ssh/writable.conf").path)
        XCTAssertThrowsError(try loadFiles(writable)) { XCTAssertEqual($0 as? FTError, .unsupported("ssh.include_permissions")) }
    }

    func testIncludeDepthAndAggregateFileAdmissionAreBounded() throws {
        var depth: [String: String] = ["config": "Include depth1.conf\nHost build\n"]
        for index in 1...9 { depth["depth\(index).conf"] = index < 9 ? "Include depth\(index + 1).conf\n" : "User deep\n" }
        XCTAssertThrowsError(try loadFiles(configurationFiles(depth))) {
            XCTAssertEqual($0 as? FTError, .limit("ssh.include_files_or_depth"))
        }
        var files = ["config": "Include nodes/*.conf\nHost build\n"]
        for index in 0..<64 { files["nodes/\(index).conf"] = "IdentityFile none\n" }
        XCTAssertThrowsError(try loadFiles(configurationFiles(files))) {
            XCTAssertEqual($0 as? FTError, .limit("ssh.include_files_or_depth"))
        }
        let line = "#" + String(repeating: "x", count: 4094) + "\n"
        let extent = String(repeating: line, count: 260)
        let large = try configurationFiles(["config": "Include first.conf second.conf\nHost build\n", "first.conf": extent, "second.conf": extent])
        XCTAssertThrowsError(try loadFiles(large)) { XCTAssertEqual($0 as? FTError, .limit("ssh.include_file")) }
    }

    func testRepeatedIncludeIsAllowedAndConditionalIdentityOrderSurvivesArchive() throws {
        let home = try configurationFiles([
            "config": "Host build gpu\nInclude identity.conf\nHost build\nInclude identity.conf\nHost *\nIdentityFile ~/.ssh/second\n",
            "identity.conf": "IdentityFile ~/.ssh/first\nPreferredAuthentications publickey\nPasswordAuthentication no\nKbdInteractiveAuthentication no\n"
        ])
        let source = try loadFiles(home)
        XCTAssertEqual(try source.identityFileReferences(for: ["gpu", "build"]), ["~/.ssh/first", "~/.ssh/second"])
        var registry = SSHWorkspaceRegistry()
        let result = try source.export(aliases: source.aliases, workspaceName: "Split configuration", defaultUser: "operator",
            publicKeys: ["~/.ssh/first": try publicFile(), "~/.ssh/second": try publicFile(42)],
            knownHosts: Data(try hostLine("build,gpu", seed: 64).utf8), registry: &registry, using: crypto)
        let original = try sampleKey(using: crypto).0
        let selected = try XCTUnwrap(result.workspace.credentials.first { $0.algorithm == "ssh-ed25519" })
        let key = PrivateKeyRecord(credentialID: selected.id, container: original.container, publicBlob: original.publicBlob)
        let archive = try AirGapArchive(workspace: result.workspace, keys: [key], portability: result.portability, using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(restored.workspace.raw, result.workspace.raw)
        XCTAssertEqual(restored.keys.count, 1)
        XCTAssertTrue(restored.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
    }

    func testKnownHostsDefaultListsFirstAssignmentAndNoneAreResolvedPerHost() throws {
        let input = try source("""
        Host build
          UserKnownHostsFile /selected/first "/selected/with space"
          UserKnownHostsFile /must-not-replace
          GlobalKnownHostsFile NONE
        Host no-user
          UserKnownHostsFile none
        Host no-trust
          UserKnownHostsFile none
          GlobalKnownHostsFile none
        Host defaults
          IdentityFile none
        """)
        XCTAssertEqual(try input.knownHostsFileReferences(for: ["build"]), ["/selected/first", "/selected/with space"])
        XCTAssertEqual(try input.knownHostsFileReferences(for: ["no-user"]), ["/etc/ssh/ssh_known_hosts", "/etc/ssh/ssh_known_hosts2"])
        XCTAssertEqual(try input.knownHostsFileReferences(for: ["no-trust"]), [])
        XCTAssertEqual(try input.knownHostsFileReferences(for: ["defaults"]),
            ["~/.ssh/known_hosts", "~/.ssh/known_hosts2", "/etc/ssh/ssh_known_hosts", "/etc/ssh/ssh_known_hosts2"])
        let result = try exportFiles(input, .init(files: [:]), aliases: ["no-trust"])
        XCTAssertTrue(result.workspace.hostTrust[0].pins.isEmpty)
        XCTAssertTrue(result.workspace.routes[0].requiredCapabilities.isEmpty)
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.host_trust" && $0.disposition == 0 })
    }

    func testLocalTrustFilesPreserveRouteScopeAndGlobalRevocations() throws {
        let home = try configurationFiles([
            "config": """
            IdentityFile none
            Host build
              HostName shared.example.invalid
              UserKnownHostsFile ~/.ssh/build_hosts
              GlobalKnownHostsFile ~/.ssh/system_hosts
            Host gpu
              HostName shared.example.invalid
              UserKnownHostsFile ~/.ssh/gpu_hosts
              GlobalKnownHostsFile none
            """,
            "build_hosts": try hostLine("*", seed: 8),
            "gpu_hosts": try hostLine("*", seed: 16),
            "system_hosts": try hashedLine("shared.example.invalid", seed: 8, marker: "@revoked")
        ])
        let input = try loadFiles(home), files = try input.loadKnownHostFiles(for: input.aliases, homeDirectory: home)
        let result = try exportFiles(input, files)
        let build = try trust("build", in: result.workspace), gpu = try trust("gpu", in: result.workspace)
        XCTAssertEqual(build.pins.count, 1); XCTAssertEqual(gpu.pins.count, 1)
        XCTAssertNotEqual(build.pins, gpu.pins, "A wildcard in one alias's file cannot add trust to another alias's route.")
        XCTAssertEqual(build.revokedKeys, build.pins)
        XCTAssertTrue(gpu.revokedKeys.isEmpty)
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        let selection = try WorkspaceExportSelection(workspace: result.workspace,
            groupIDs: [result.workspace.initialGroupID], findings: result.portability)
        let review = try selection.review(decisions: [:], loadedFingerprints: [])
        XCTAssertEqual(review.blockedRouteIDs, [try destination("build", in: result.workspace).id])
    }

    func testTrustFileInventoryIncludesOnlySelectedRoutesAndTheirJumps() throws {
        let home = try configurationFiles([
            "config": """
            IdentityFile none
            GlobalKnownHostsFile none
            Host target
              ProxyJump gate
              UserKnownHostsFile ~/.ssh/target_hosts
            Host gate
              HostName gateway.example.invalid
              HostKeyAlias pinned-gateway
              Port 2200
              UserKnownHostsFile ~/.ssh/gate_hosts
            Host unrelated
              UserKnownHostsFile ~/.ssh/unreadable_hosts
            """,
            "target_hosts": try hostLine("target", seed: 8),
            "gate_hosts": try hashedLine("pinned-gateway", seed: 16) + hostLine("[pinned-gateway]:2200", seed: 24),
            "unreadable_hosts": "This malformed unrelated trust source must never be read.\n"
        ])
        let input = try loadFiles(home)
        XCTAssertEqual(try input.knownHostsFileReferences(for: ["target"]), ["~/.ssh/target_hosts", "~/.ssh/gate_hosts"])
        let files = try input.loadKnownHostFiles(for: ["target"], homeDirectory: home)
        XCTAssertNil(files.files["~/.ssh/unreadable_hosts"])
        let result = try exportFiles(input, files, aliases: ["target"])
        let target = try destination("target", in: result.workspace), gate = try XCTUnwrap(hops(target, in: result.workspace).first)
        let gateTrust = try XCTUnwrap(result.workspace.hostTrust.first { $0.id == gate.trustID })
        XCTAssertEqual(gateTrust.pins.count, 1, "HostKeyAlias remains a verbatim lookup independent of the port.")
        XCTAssertEqual(result.workspace.machines.map(\.name), ["target"])
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
    }

    func testTrustSnapshotIsFrozenAndEverySourceStateChangesRevision() throws {
        let home = try configurationFiles([
            "config": "Host build\nIdentityFile none\nUserKnownHostsFile ~/.ssh/pins\nGlobalKnownHostsFile none\n",
            "pins": try hostLine("build", seed: 8)
        ])
        let input = try loadFiles(home), frozen = try input.loadKnownHostFiles(for: ["build"], homeDirectory: home)
        let linkedHome = home.deletingLastPathComponent().appendingPathComponent("ft-trust-home-link-" + UUID().uuidString)
        try FileManager.default.createSymbolicLink(at: linkedHome, withDestinationURL: home)
        XCTAssertEqual(try input.loadKnownHostFiles(for: ["build"], homeDirectory: linkedHome).files, frozen.files)
        let first = try exportFiles(input, frozen)
        try Data(hostLine("build", seed: 16).utf8).write(to: home.appendingPathComponent(".ssh/pins"))
        XCTAssertEqual(try exportFiles(input, frozen).workspace.hostTrust[0].pins, first.workspace.hostTrust[0].pins)
        let changed = try exportFiles(input, input.loadKnownHostFiles(for: ["build"], homeDirectory: home))
        XCTAssertNotEqual(try sourceRevision(first.workspace), try sourceRevision(changed.workspace))
        var revisions = Set<String>()
        for state: SSHWorkspaceSource.KnownHostsSnapshot.File in [.missing, .unavailable, .present(Data())] {
            let result = try exportFiles(input, .init(files: ["~/.ssh/pins": state]))
            revisions.insert(try XCTUnwrap(sourceRevision(result.workspace)).string())
        }
        XCTAssertEqual(revisions.count, 3, "Missing, unreadable and an empty verified file are different frozen observations.")
    }

    func testUnreadableTrustFilesBlockOnlyAffectedRoutesAndMissingFilesRemainMissing() throws {
        let home = try configurationFiles([
            "config": """
            IdentityFile none
            GlobalKnownHostsFile none
            Host good
              UserKnownHostsFile ~/.ssh/good_hosts
            Host unsafe
              UserKnownHostsFile ~/.ssh/unsafe_hosts
            Host linked
              UserKnownHostsFile ~/.ssh/link_hosts
            Host absent
              UserKnownHostsFile ~/.ssh/absent_hosts
            """,
            "good_hosts": try hostLine("*", seed: 8),
            "unsafe_hosts": try hostLine("*", seed: 16, marker: "@revoked")
        ])
        try FileManager.default.setAttributes([.posixPermissions: 0o666], ofItemAtPath: home.appendingPathComponent(".ssh/unsafe_hosts").path)
        try FileManager.default.createSymbolicLink(at: home.appendingPathComponent(".ssh/link_hosts"),
            withDestinationURL: home.appendingPathComponent(".ssh/good_hosts"))
        let input = try loadFiles(home), files = try input.loadKnownHostFiles(for: input.aliases, homeDirectory: home)
        XCTAssertEqual(files.files["~/.ssh/unsafe_hosts"], .unavailable)
        XCTAssertEqual(files.files["~/.ssh/link_hosts"], .unavailable)
        XCTAssertEqual(files.files["~/.ssh/absent_hosts"], .missing)
        let result = try exportFiles(input, files)
        for name in ["unsafe", "linked"] {
            let route = try destination(name, in: result.workspace)
            XCTAssertFalse(route.requiredCapabilities.isEmpty)
            XCTAssertTrue(result.portability.contains { $0.entityID == route.id && $0.setting == "ssh_config.known_hosts" && $0.disposition == 1 })
        }
        XCTAssertTrue(try destination("good", in: result.workspace).requiredCapabilities.isEmpty)
        XCTAssertTrue(try destination("absent", in: result.workspace).requiredCapabilities.isEmpty)
        XCTAssertTrue(try trust("absent", in: result.workspace).pins.isEmpty)
    }

    func testUnsupportedTrustListsNeverUseAPartialPrefixOrPooledTrust() throws {
        for value in ["none /selected/pins", "/selected/pins none", "/selected/pins %d/other",
                      "/selected/pins ${HOME}/other", "/selected/pins relative", "/selected/pins ~someone/other",
                      "/selected/pins ~/../other", Array(repeating: "/selected/pins", count: 17).joined(separator: " ")] {
            let input = try source("Host build\nIdentityFile none\nUserKnownHostsFile \(value)\nGlobalKnownHostsFile none\n")
            XCTAssertEqual(try input.knownHostsFileReferences(for: ["build"]), [], value)
            let result = try exportFiles(input, .init(files: [:]))
            XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty, value)
            XCTAssertTrue(result.workspace.hostTrust[0].pins.isEmpty, value)
        }
        let input = try source("Host build\nIdentityFile none\nUserKnownHostsFile /selected/pins\nGlobalKnownHostsFile none\n")
        let omitted = try exportFiles(input, .init(files: [:]))
        XCTAssertFalse(omitted.workspace.routes[0].requiredCapabilities.isEmpty, "Not supplying a file is not proof it is absent.")
        var registry = SSHWorkspaceRegistry()
        let legacy = try input.export(aliases: ["build"], workspaceName: "Partial", defaultUser: "operator",
            knownHosts: Data(hostLine("build").utf8), registry: &registry, using: crypto)
        XCTAssertFalse(legacy.workspace.routes[0].requiredCapabilities.isEmpty)
        XCTAssertTrue(legacy.workspace.hostTrust[0].pins.isEmpty)
        XCTAssertThrowsError(try exportFiles(input, .init(files: ["/unselected/pins": .missing]))) {
            XCTAssertEqual($0 as? FTError, .malformed("ssh.known_hosts.unselected_source"))
        }
    }

    func testTrustSnapshotBoundsApplyAcrossFilesWithoutTruncation() throws {
        let tooMany = Dictionary(uniqueKeysWithValues: (0...64).map { ("/selected/\($0)", SSHWorkspaceSource.KnownHostsSnapshot.File.missing) })
        XCTAssertThrowsError(try SSHWorkspaceSource.KnownHostsSnapshot(files: tooMany)) {
            XCTAssertEqual($0 as? FTError, .limit("ssh.known_hosts.files"))
        }
        XCTAssertThrowsError(try SSHWorkspaceSource.KnownHostsSnapshot(files: [
            "/selected/one": .present(Data(repeating: 32, count: SSHWorkspaceSource.maximumSourceBytes)),
            "/selected/two": .present(Data([32]))
        ])) { XCTAssertEqual($0 as? FTError, .limit("ssh.known_hosts.bytes")) }
        XCTAssertThrowsError(try SSHWorkspaceSource.KnownHostsSnapshot(files: [
            "/selected/one": .present(Data(repeating: 10, count: 8192)),
            "/selected/two": .present(Data(repeating: 10, count: 8192))
        ])) { XCTAssertEqual($0 as? FTError, .limit("ssh.known_hosts.lines")) }
        let home = try configurationFiles([
            "config": "Host build\nUserKnownHostsFile ~/.ssh/large\nGlobalKnownHostsFile none\n",
            "large": String(repeating: " ", count: SSHWorkspaceSource.maximumSourceBytes + 1)
        ])
        XCTAssertThrowsError(try loadFiles(home).loadKnownHostFiles(for: ["build"], homeDirectory: home)) {
            XCTAssertEqual($0 as? FTError, .limit("ssh.known_hosts.bytes"))
        }
    }

    func testFileScopedTrustAndSelectedIdentityRoundTripThroughEncryptedArchive() throws {
        let input = try source("Host build\nIdentityFile ~/.ssh/key\nUserKnownHostsFile /selected/pins /selected/revoked\nGlobalKnownHostsFile none\n")
        let files = try SSHWorkspaceSource.KnownHostsSnapshot(files: [
            "/selected/pins": .present(Data(hostLine("build", seed: 8).utf8)),
            "/selected/revoked": .present(Data(hashedLine("build", seed: 16, marker: "@revoked").utf8))
        ])
        let result = try exportFiles(input, files, keys: ["~/.ssh/key": try publicFile()])
        let original = try sampleKey(using: crypto).0, reference = try XCTUnwrap(result.workspace.credentials.first)
        let key = PrivateKeyRecord(credentialID: reference.id, container: original.container, publicBlob: original.publicBlob)
        let selection = try WorkspaceExportSelection(workspace: result.workspace,
            groupIDs: [result.workspace.initialGroupID], findings: result.portability)
        let archive = try selection.archive(including: [key], using: crypto), sealed = try FTAG.seal(archive, using: crypto)
        let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(restored.workspace.hostTrust[0].pins, result.workspace.hostTrust[0].pins)
        XCTAssertEqual(restored.workspace.hostTrust[0].revokedKeys, result.workspace.hostTrust[0].revokedKeys)
        XCTAssertEqual(restored.keys[0].publicBlob, original.publicBlob)
        XCTAssertTrue(restored.workspace.routes[0].requiredCapabilities.isEmpty)
        let manifest = String(decoding: restored.workspace.raw, as: UTF8.self)
        XCTAssertFalse(manifest.contains("/selected/"))
        XCTAssertFalse(manifest.contains("~/.ssh/"))
    }

    func testConcreteInventoryFirstValueWildcardNegationAndIdentityOrder() throws {
        let input = try source("""
        User global-first
        Host build gpu build *.invalid !excluded
          User ignored-later
          IdentityFile ~/.ssh/team
        Host * !gpu
          Port=2200
          IdentityFile "/opt/keys/shared key"
        Host build
          HostName build.example.invalid # ordinary comment
          Port 9999
        Host gpu
          HostName=gpu.example.invalid
          Port 2222
        """)
        XCTAssertEqual(input.aliases, ["build", "gpu"])
        XCTAssertEqual(try input.identityFileReferences(for: ["gpu", "build"]), ["~/.ssh/team", "/opt/keys/shared key"])
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: ["build", "gpu"], workspaceName: "Fleet", defaultUser: "fallback",
                                      registry: &registry, using: crypto)
        XCTAssertEqual(result.workspace.routes.map(\.host), ["build.example.invalid", "gpu.example.invalid"])
        XCTAssertEqual(result.workspace.routes.map(\.port), [2200, 2222])
        XCTAssertEqual(result.workspace.routes.map(\.user), ["global-first", "global-first"])
        XCTAssertEqual(result.workspace.groups[0].machineOrder, result.workspace.machines.map(\.id))
        XCTAssertTrue(result.workspace.machines.allSatisfy(\.autoConnect), "every machine signs in in the background")
        XCTAssertTrue(result.workspace.groups[0].autoConnect)
        XCTAssertThrowsError(try input.identityFileReferences(for: ["*.invalid"]))
        XCTAssertThrowsError(try input.identityFileReferences(for: ["build", "build"]))
    }

    func testStableRegistrySurvivesSerializationReorderAndEndpointChanges() throws {
        var registry = SSHWorkspaceRegistry()
        let input = try source("Host build gpu\nUser operator\nIdentityFile none\n")
        let first = try input.export(aliases: ["build", "gpu"], workspaceName: "Fleet", defaultUser: "fallback", registry: &registry, using: crypto)
        registry = try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: JSONEncoder().encode(registry))
        let changed = try source("Host gpu\nHostName new-gpu.example.invalid\nHost build\nHostName new-build.example.invalid\n")
        let second = try changed.export(aliases: ["gpu", "build"], workspaceName: "New display name", defaultUser: "fallback", registry: &registry, using: crypto)
        XCTAssertEqual(first.workspace.id, second.workspace.id)
        XCTAssertEqual(first.workspace.exporterID, second.workspace.exporterID)
        XCTAssertEqual(Array(first.workspace.machines.map(\.id).reversed()), second.workspace.machines.map(\.id))
        XCTAssertEqual(Array(first.workspace.routes.map(\.id).reversed()), second.workspace.routes.map(\.id))
        XCTAssertNotEqual(first.workspace.bundleID, second.workspace.bundleID)
        let a = try StrictJSON.decode(first.workspace.raw).object().required("source").object()
        let b = try StrictJSON.decode(second.workspace.raw).object().required("source").object()
        XCTAssertEqual(try a.text("kind"), "ssh_config")
        XCTAssertNotEqual(a["configuration_revision"], b["configuration_revision"])
    }

    func testActualPublicIdentityDedupAndEncryptedOfflineRoundtrip() throws {
        let input = try source("""
        Host build gpu
          User operator
          IdentityFile ~/.ssh/first
          IdentityFile ~/.ssh/same-material
          IdentitiesOnly yes
          PreferredAuthentications publickey
          PasswordAuthentication no
          KbdInteractiveAuthentication no
        """)
        let known = try hostLine("build,gpu", seed: 64)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: input.aliases, workspaceName: "Fleet", defaultUser: "fallback",
            publicKeys: ["~/.ssh/first": try publicFile(), "~/.ssh/same-material": try publicFile()],
            knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        XCTAssertEqual(result.workspace.credentials.count, 1)
        let credential = try XCTUnwrap(result.workspace.credentials.first)
        let routes = try records(result, "routes")
        for route in routes {
            XCTAssertEqual(route["credential_refs"], .array([.string(credential.id)]))
            XCTAssertEqual(route["authentication_methods"], .array([.string("publickey")]))
            XCTAssertNil(route["required_capabilities"])
        }
        let (original, fingerprint) = try sampleKey(using: crypto)
        XCTAssertEqual(credential.fingerprint, fingerprint)
        let key = PrivateKeyRecord(credentialID: credential.id, container: original.container, publicBlob: original.publicBlob)
        let archive = try AirGapArchive(workspace: result.workspace, keys: [key], portability: result.portability, using: crypto)
        let encrypted = try FTAG.seal(archive, using: crypto)
        let recovered = try FTAG.open(encrypted.envelope, code: encrypted.code, using: crypto)
        XCTAssertEqual(recovered.workspace.raw, result.workspace.raw)
        XCTAssertEqual(recovered.keys.count, 1)
        XCTAssertEqual(recovered.keys[0].publicBlob, original.publicBlob)
        XCTAssertEqual(result.identityReferences[credential.id], "~/.ssh/first")
        let json = String(decoding: result.workspace.raw, as: UTF8.self)
        XCTAssertFalse(json.contains("~/.ssh"))
        XCTAssertFalse(json.contains("synthetic@example.invalid"))
        XCTAssertFalse(json.contains(original.container.base64EncodedString()))
    }

    func testMissingIdentityAndTrustRemainValidWithoutInventedReferences() throws {
        let result = try exported("Host build\nHostName build.example.invalid\nIdentityFile ~/.ssh/not-supplied\nPreferredAuthentications publickey\n")
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        let route = try XCTUnwrap(records(result, "routes").first)
        let trust = try XCTUnwrap(records(result, "host_trust").first)
        XCTAssertEqual(route["credential_refs"], .array([]))
        XCTAssertEqual(trust["pins"], .array([]))
        XCTAssertEqual(trust["revoked_keys"], .array([]))
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.credentials" })
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.host_trust" })
        // A key added on the phone later lets it sign in like the others.
        XCTAssertTrue(result.workspace.machines[0].autoConnect)
    }

    func testDefaultIdentityPolicyIsExplicitAndNoneOrDisabledPublicKeyPreventsReads() throws {
        let ordinary = try source("Host build\n")
        XCTAssertEqual(try ordinary.identityFileReferences(for: ["build"]), ["~/.ssh/id_rsa", "~/.ssh/id_ecdsa", "~/.ssh/id_ecdsa_sk", "~/.ssh/id_ed25519", "~/.ssh/id_ed25519_sk", "~/.ssh/id_mldsa44_ed25519"])
        let none = try source("Host build\nIdentityFile none\n")
        XCTAssertTrue(try none.identityFileReferences(for: ["build"]).isEmpty)
        let disabled = try source("Host build\nPubkeyAuthentication no\nIdentityFile ~/.ssh/explicit\n")
        XCTAssertTrue(try disabled.identityFileReferences(for: ["build"]).isEmpty)
        let result = try exported("Host build\n", keys: ["~/.ssh/id_ed25519": try publicFile()])
        XCTAssertEqual(result.workspace.credentials.count, 1)
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.default_identities" && $0.disposition == 0 })
    }

    func testExplicitCertificateInventoryFollowsSelectedJumpGraphWithoutImplicitDiscovery() throws {
        let input = try source("""
        Host target
          ProxyJump operator@gate:2200
          CertificateFile ~/.ssh/target-cert.pub
          CertificateFile ~/.ssh/target-cert.pub
        Host gate
          CertificateFile /selected/gate-cert.pub
        Host unrelated
          CertificateFile /unrelated/cert.pub
        Host *
          IdentityFile ~/.ssh/key
          CertificateFile ~/.ssh/shared-cert.pub
        """)
        XCTAssertEqual(try input.certificateFileReferences(for: ["target"]),
                       ["~/.ssh/target-cert.pub", "~/.ssh/shared-cert.pub", "/selected/gate-cert.pub"])
        XCTAssertEqual(try input.identityFileReferences(for: ["target"]), ["~/.ssh/key"])
        XCTAssertTrue(try source("Host target\nIdentityFile ~/.ssh/key\n").certificateFileReferences(for: ["target"]).isEmpty)
        let disabled = try source("Host target\nPubkeyAuthentication no\nCertificateFile /must/not/read\nIdentityFile ~/.ssh/key\n")
        XCTAssertTrue(try disabled.certificateFileReferences(for: ["target"]).isEmpty)
        XCTAssertTrue(try disabled.identityFileReferences(for: ["target"]).isEmpty)
        XCTAssertThrowsError(try input.certificateFileReferences(for: ["unknown"]))
    }

    func testCertificateFileLoaderFreezesOnlyExactPublicFilesAndKeepsUnsafeInputsUnavailable() throws {
        let blob = try workspaceTestCertificate()
        let certificate = try SSHUserCertificate(blob: blob)
        let text = certificate.algorithm + " " + blob.base64EncodedString() + " public-certificate\n"
        let home = try configurationFiles([
            "config": "Host selected\nCertificateFile ~/.ssh/selected-cert.pub\nIdentityFile none\nHost absent\nCertificateFile ~/.ssh/missing.pub\nIdentityFile none\nHost unsafe\nCertificateFile ~/.ssh/linked.pub\nIdentityFile none\n",
            "selected-cert.pub": text,
            "selected-cert": "PRIVATE_SIBLING_MUST_NOT_BE_READ",
            "missing": "PRIVATE_FALLBACK_MUST_NOT_BE_READ"
        ])
        try FileManager.default.createSymbolicLink(at: home.appendingPathComponent(".ssh/linked.pub"),
                                                  withDestinationURL: home.appendingPathComponent(".ssh/selected-cert.pub"))
        let input = try loadFiles(home)
        let frozen = try input.loadPublicCertificateFiles(for: ["selected"], homeDirectory: home)
        XCTAssertEqual(frozen.files, ["~/.ssh/selected-cert.pub": .present(blob)])
        XCTAssertEqual(try input.loadPublicCertificateFiles(for: ["absent"], homeDirectory: home).files,
                       ["~/.ssh/missing.pub": .missing], "No private sibling may substitute for a public certificate.")
        XCTAssertEqual(try input.loadPublicCertificateFiles(for: ["unsafe"], homeDirectory: home).files,
                       ["~/.ssh/linked.pub": .unavailable])
        try Data("invalid certificate\n".utf8).write(to: home.appendingPathComponent(".ssh/selected-cert.pub"))
        let changed = try input.loadPublicCertificateFiles(for: ["selected"], homeDirectory: home)
        XCTAssertEqual(changed.files, ["~/.ssh/selected-cert.pub": .unavailable])
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: ["selected"], workspaceName: "Frozen certificate", defaultUser: "operator",
            publicCertificateFiles: frozen, registry: &registry, using: crypto)
        XCTAssertEqual(result.workspace.credentials.first?.userCertificate?.blob, blob)
        XCTAssertTrue(try destination("selected", in: result.workspace).requiredCapabilities.isEmpty)
        let manifest = String(decoding: result.workspace.raw, as: UTF8.self)
        XCTAssertFalse(manifest.contains("PRIVATE_")); XCTAssertFalse(manifest.contains("~/.ssh/"))
    }

    func testCertificateSnapshotsRejectUnselectedMalformedAndUnboundedInputs() throws {
        let blob = try workspaceTestCertificate(), certificate = try SSHUserCertificate(blob: blob)
        var corrupted = blob; corrupted[corrupted.count - 1] ^= 1
        let wrongType = Data(("ssh-ed25519 " + blob.base64EncodedString()).utf8)
        let text = certificate.algorithm + " " + blob.base64EncodedString()
        for invalid in [corrupted, try publicFile(), wrongType, Data((text + "\n" + text).utf8),
                        Data(repeating: 0, count: SSHUserCertificate.maximumBlobBytes + 1)] {
            XCTAssertThrowsError(try SSHWorkspaceSource.PublicCertificateSnapshot(files: ["/selected/cert.pub": .present(invalid)]))
        }
        for path in ["relative.pub", "~other/cert.pub", "/selected/../cert.pub", "/selected/%h.pub", "/selected/${KEY}.pub"] {
            XCTAssertThrowsError(try SSHWorkspaceSource.PublicCertificateSnapshot(files: [path: .missing]))
        }
        let files = Dictionary(uniqueKeysWithValues: (0...256).map { ("/selected/\($0).pub", SSHWorkspaceSource.PublicCertificateSnapshot.File.missing) })
        XCTAssertThrowsError(try SSHWorkspaceSource.PublicCertificateSnapshot(files: files)) {
            XCTAssertEqual($0 as? FTError, .limit("ssh.certificate_references"))
        }
        let unselected = try SSHWorkspaceSource.PublicCertificateSnapshot(files: ["/unselected/cert.pub": .present(blob)])
        XCTAssertThrowsError(try exported("Host build\nCertificateFile /selected/cert.pub\nIdentityFile none\n", certificates: unselected)) {
            XCTAssertEqual($0 as? FTError, .malformed("ssh.certificate.unselected_source"))
        }
    }

    func testCertificateCandidatesPrecedeRawKeysWithoutInventingFallbackAndKeepStableIDs() throws {
        let firstBlob = try workspaceTestCertificate(serial: 1), secondBlob = try workspaceTestCertificate(serial: 2)
        let files = try SSHWorkspaceSource.PublicCertificateSnapshot(files: [
            "/selected/first.pub": .present(firstBlob), "/selected/duplicate.pub": .present(firstBlob),
            "/selected/second.pub": .present(secondBlob)
        ])
        let input = try source("""
        Host build
          User operator
          CertificateFile /selected/first.pub
          CertificateFile /selected/duplicate.pub
          CertificateFile /selected/second.pub
          IdentityFile ~/.ssh/key
        """)
        var registry = SSHWorkspaceRegistry()
        let first = try input.export(aliases: ["build"], workspaceName: "Certificate candidates", defaultUser: "operator",
            publicKeys: ["~/.ssh/key": try publicFile()], publicCertificateFiles: files, registry: &registry, using: crypto)
        let route = try destination("build", in: first.workspace)
        let credentials = try route.credentialIDs.map { id in try XCTUnwrap(first.workspace.credentials.first { $0.id == id }) }
        XCTAssertEqual(credentials.count, 3)
        XCTAssertEqual(credentials[0].userCertificate?.blob, firstBlob)
        XCTAssertEqual(credentials[1].userCertificate?.blob, secondBlob)
        XCTAssertNil(credentials[2].userCertificate)
        XCTAssertEqual(Set(credentials.map(\.fingerprint)).count, 1)
        XCTAssertEqual(Set(credentials.map(\.id)).count, 3)
        XCTAssertEqual(credentials[0].requiredCapabilities, [WorkspaceDocument.userCertificateCapability])
        XCTAssertEqual(credentials[1].requiredCapabilities, [WorkspaceDocument.userCertificateCapability])
        XCTAssertTrue(credentials[2].requiredCapabilities.isEmpty)
        XCTAssertTrue(credentials.allSatisfy { first.identityReferences[$0.id] == "~/.ssh/key" })
        registry = try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: JSONEncoder().encode(registry))
        let again = try input.export(aliases: ["build"], workspaceName: "Renamed", defaultUser: "operator",
            publicKeys: ["~/.ssh/key": try publicFile()], publicCertificateFiles: files, registry: &registry, using: crypto)
        XCTAssertEqual(first.workspace.credentials.map(\.id), again.workspace.credentials.map(\.id))
        let certOnly = try exported("Host build\nUser operator\nIdentityFile none\nCertificateFile /selected/first.pub\n",
            certificates: SSHWorkspaceSource.PublicCertificateSnapshot(files: ["/selected/first.pub": .present(firstBlob)]))
        XCTAssertEqual(certOnly.workspace.credentials.count, 1)
        XCTAssertNotNil(certOnly.workspace.credentials[0].userCertificate)
        XCTAssertTrue(certOnly.identityReferences.isEmpty)
        let selection = try WorkspaceExportSelection(workspace: certOnly.workspace, groupIDs: [certOnly.workspace.initialGroupID])
        let publicDocument = try selection.publicDocument(bundleID: "public-certificates")
        XCTAssertEqual(publicDocument.credentials[0].userCertificate?.blob, firstBlob)
        XCTAssertEqual(publicDocument.credentials[0].requiredCapabilities, [WorkspaceDocument.userCertificateCapability])
    }

    func testEveryConfiguredCertificateMustResolveAndSnapshotStatesChangeRevision() throws {
        let blob = try workspaceTestCertificate(), renewed = try workspaceTestCertificate(serial: 2)
        let input = try source("Host restricted\nCertificateFile /selected/cert.pub\nHost ordinary\nHost *\nIdentityFile ~/.ssh/key\n")
        var registry = SSHWorkspaceRegistry(), revisions = Set<String>()
        let states: [SSHWorkspaceSource.PublicCertificateSnapshot.File] = [.present(blob), .present(renewed), .missing, .unavailable]
        for file in states {
            let files = try SSHWorkspaceSource.PublicCertificateSnapshot(files: ["/selected/cert.pub": file])
            let result = try input.export(aliases: ["restricted", "ordinary"], workspaceName: "Certificates", defaultUser: "operator",
                publicKeys: ["~/.ssh/key": try publicFile()], publicCertificateFiles: files, registry: &registry, using: crypto)
            revisions.insert(try XCTUnwrap(sourceRevision(result.workspace)).string())
            let restricted = try destination("restricted", in: result.workspace)
            XCTAssertTrue(try destination("ordinary", in: result.workspace).requiredCapabilities.isEmpty)
            if case .present = file {
                XCTAssertTrue(restricted.requiredCapabilities.isEmpty)
                XCTAssertEqual(restricted.credentialIDs.count, 2)
            } else {
                XCTAssertEqual(restricted.requiredCapabilities, ["ssh_config.unresolved_policy"])
                XCTAssertEqual(restricted.credentialIDs.count, 1, "A raw candidate can be retained, but unresolved certificate policy keeps it inert.")
                XCTAssertTrue(result.portability.contains {
                    $0.entityID == restricted.id && $0.setting == "ssh_config.certificatefile" && $0.disposition == 1
                })
            }
        }
        XCTAssertEqual(revisions.count, 4)
        let missingSnapshot = try exported("Host build\nIdentityFile ~/.ssh/key\nCertificateFile /selected/cert.pub\n", keys: ["~/.ssh/key": try publicFile()])
        XCTAssertEqual(missingSnapshot.workspace.routes[0].requiredCapabilities, ["ssh_config.unresolved_policy"])
        for reference in ["relative.pub", "none", "/selected/%h.pub", "/selected/${KEY}.pub"] {
            let result = try exported("Host build\nIdentityFile none\nCertificateFile \(reference)\n")
            XCTAssertEqual(result.workspace.routes[0].requiredCapabilities, ["ssh_config.unresolved_policy"])
        }
    }

    func testCombinedCertificateAndRawCandidateLimitNeverPublishesAPartialPrefix() throws {
        let files = try SSHWorkspaceSource.PublicCertificateSnapshot(files: ["/selected/cert.pub": .present(workspaceTestCertificate())])
        for count in [7, 8] {
            let identities = (0..<count).map { "IdentityFile /selected/key-\($0)\n" }.joined()
            let keys = try Dictionary(uniqueKeysWithValues: (0..<count).map { ("/selected/key-\($0)", try publicFile(UInt8($0))) })
            let result = try exported("Host build\nCertificateFile /selected/cert.pub\n" + identities, keys: keys, certificates: files)
            if count == 7 {
                XCTAssertEqual(result.workspace.routes[0].credentialIDs.count, 8)
                XCTAssertTrue(result.workspace.routes[0].requiredCapabilities.isEmpty)
            } else {
                XCTAssertTrue(result.workspace.routes[0].credentialIDs.isEmpty)
                XCTAssertTrue(result.workspace.credentials.isEmpty)
                XCTAssertTrue(result.identityReferences.isEmpty)
                XCTAssertEqual(result.workspace.routes[0].requiredCapabilities, ["ssh_config.unresolved_policy"])
                XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.authentication_candidates" && $0.disposition == 1 })
            }
        }
        let tooMany = try source("Host build\nIdentityFile none\n" +
            (0..<9).map { "CertificateFile /selected/cert-\($0).pub\n" }.joined())
        XCTAssertTrue(try tooMany.certificateFileReferences(for: ["build"]).isEmpty)
        let result = try exportSource(tooMany)
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        XCTAssertEqual(result.workspace.routes[0].requiredCapabilities, ["ssh_config.unresolved_policy"])
    }

    func testOpenSSHCertificateFilesAndMatchingPrivatePickersExportEverySupportedSubjectFamily() throws {
        let fixtures = try UserCertificateFixtures.load()
        XCTAssertGreaterThan(fixtures.largeCertificate.certificateBlob.base64EncodedString().utf8.count, FTLimit.text)
        for fixture in fixtures.certificates + [fixtures.largeCertificate] {
            let certificates = try SSHWorkspaceSource.PublicCertificateSnapshot(files: [
                "/selected/cert.pub": .present(Data(fixture.certificate.utf8))
            ])
            let identities = try SSHWorkspaceSource.PublicIdentitySnapshot(files: [
                "/selected/key": .present(Data(fixture.publicKey.utf8))
            ])
            let input = try source("Host build\nUser operator\nCertificateFile /selected/cert.pub\nIdentityFile /selected/key\n")
            var registry = SSHWorkspaceRegistry()
            let result = try input.export(aliases: ["build"], workspaceName: "Certified source", defaultUser: "operator",
                publicIdentityFiles: identities, publicCertificateFiles: certificates, registry: &registry, using: crypto)
            let selection = try WorkspaceExportSelection(workspace: result.workspace, groupIDs: [result.workspace.initialGroupID],
                                                          findings: result.portability)
            let identity = try XCTUnwrap(selection.identities.first)
            XCTAssertEqual(selection.identities.count, 1, fixture.id)
            XCTAssertEqual(identity.credentialIDs.count, 2, fixture.id)
            XCTAssertEqual(identity.algorithm, fixture.subjectAlgorithm)
            let reference = try XCTUnwrap(result.workspace.credentials.first { $0.id == identity.credentialIDs[0] })
            let key = try PrivateKeyRecord.fromPEM(Data(fixture.privatePEM.utf8), credentialID: reference.id, using: crypto)
            XCTAssertEqual(key.publicBlob, fixture.publicBlob)
            XCTAssertTrue(key.certificates.isEmpty, "The private file contains only the raw key; the selected manifest supplies its certificate.")
            let archive = try selection.archive(including: [key], using: crypto)
            let decoded = try AirGapArchive(data: archive.encoded(), using: crypto)
            XCTAssertEqual(decoded.keys.count, 1)
            XCTAssertEqual(decoded.keys[0].certificates, [fixture.certificateBlob])
            XCTAssertEqual(decoded.workspace.credentials.compactMap(\.userCertificate).map(\.blob), [fixture.certificateBlob])
            XCTAssertEqual(decoded.workspace.credentials.map(\.algorithm), [fixture.subjectAlgorithm, fixture.subjectAlgorithm])
        }
    }

    func testCertificateImportPreservesValidityAndRestrictionsWithoutBypassingRequiredSourcePolicy() throws {
        let fixtures = try UserCertificateFixtures.load()
        let restricted = UserCertificateFixtures.blob(fixtures.restrictedCertificate)
        let noPTY = UserCertificateFixtures.blob(fixtures.withoutPTYCertificate)
        let expired = try workspaceTestCertificate(validAfter: 100, validBefore: 200)
        let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"],
            capabilities: [WorkspaceDocument.userCertificateCapability])
        for blob in [restricted, noPTY, expired] {
            let files = try SSHWorkspaceSource.PublicCertificateSnapshot(files: ["/selected/cert.pub": .present(blob)])
            let result = try exported("Host build\nUser operator\nIdentityFile none\nCertificateFile /selected/cert.pub\nPreferredAuthentications publickey\n",
                certificates: files, known: Data(hostLine("build", seed: 8).utf8))
            XCTAssertEqual(result.workspace.credentials[0].userCertificate?.blob, blob)
            let route = result.workspace.routes[0]
            let readiness = try result.workspace.readiness(forRouteID: route.id,
                availableCredentialIDs: Set(route.credentialIDs), reviewedTrustIDs: Set(result.workspace.hostTrust.map(\.id)),
                support: support, certificateTime: UserCertificateFixtures.now)
            XCTAssertFalse(readiness.isLocallyReady)
            XCTAssertTrue(readiness.usableCredentialIDs.isEmpty)
            XCTAssertTrue(readiness.issues.contains(.credentialCertificateUnavailable))
        }
        let files = try SSHWorkspaceSource.PublicCertificateSnapshot(files: ["/selected/cert.pub": .present(workspaceTestCertificate())])
        let policy = try exported("Host build\nIdentityFile none\nCertificateFile /selected/cert.pub\nRevokedHostKeys /required/revocations\n",
                                  certificates: files)
        XCTAssertEqual(policy.workspace.routes[0].requiredCapabilities, ["ssh_config.unresolved_policy"])
        XCTAssertTrue(policy.portability.contains { $0.setting == "ssh_config.revokedhostkeys" && $0.disposition == 1 })
    }

    @MainActor
    func testSelectedCertificatesAndJumpIdentityRestoreThroughFTAGIntoOneProtectedAccount() async throws {
        let fixture = try XCTUnwrap(UserCertificateFixtures.load().certificates.first { $0.id == "ed25519" })
        let input = try source("""
        User operator
        IdentityFile ~/.ssh/key
        CertificateFile ~/.ssh/key-cert.pub
        PreferredAuthentications publickey
        Host target
          ProxyJump operator@gate:2200
        Host gate
        """)
        let files = try SSHWorkspaceSource.PublicCertificateSnapshot(files: ["~/.ssh/key-cert.pub": .present(Data(fixture.certificate.utf8))])
        var registry = SSHWorkspaceRegistry()
        let known = try hostLine("target", seed: 8) + hostLine("[gate]:2200", seed: 16)
        let result = try input.export(aliases: ["target"], workspaceName: "Certified jump", defaultUser: "operator",
            publicKeys: ["~/.ssh/key": Data(fixture.publicKey.utf8)], publicCertificateFiles: files,
            knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        let reference = try XCTUnwrap(result.workspace.credentials.first { $0.userCertificate != nil })
        let key = try PrivateKeyRecord.fromPEM(Data(fixture.privatePEM.utf8), credentialID: reference.id, using: crypto)
        let selection = try WorkspaceExportSelection(workspace: result.workspace,
            groupIDs: [result.workspace.initialGroupID], findings: result.portability)
        let sealed = try FTAG.seal(selection.archive(including: [key], using: crypto), using: crypto)
        let decoded = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(decoded.keys.count, 1)
        XCTAssertEqual(decoded.keys[0].certificates, [fixture.certificateBlob])
        let disk = StoreTestPersistence(), vault = StoreTestVault(), store = WorkspaceStore(persistence: disk, vault: vault)
        let receipt = try await store.install(decoded, envelopeDigest: crypto.sha256(sealed.envelope),
            expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertTrue(receipt.workspace.connectLater)
        XCTAssertTrue(receipt.missingCredentialIDs.isEmpty)
        XCTAssertEqual(vault.creates, 1)
        XCTAssertEqual(receipt.workspace.bindings.count, 2)
        XCTAssertEqual(Set(receipt.workspace.bindings.map(\.account)).count, 1)
        let reopened = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await reopened.load(), document = try XCTUnwrap(saved.first).document()
        XCTAssertEqual(document.credentials.first { $0.id == reference.id }?.userCertificate?.blob, fixture.certificateBlob)
        let target = try destination("target", in: document), gate = try XCTUnwrap(hops(target, in: document).first)
        XCTAssertEqual(target.credentialIDs, gate.credentialIDs)
        XCTAssertEqual(target.credentialIDs.first, reference.id)
        let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"],
            capabilities: [WorkspaceDocument.userCertificateCapability], maximumJumpHops: 3)
        let readiness = try await reopened.routeReadiness(workspaceID: receipt.workspace.id, support: support)
        XCTAssertTrue(try XCTUnwrap(readiness.first { $0.routeID == target.id }).isLocallyReady)
        let pin = try XCTUnwrap(document.hostTrust.first { $0.id == target.trustID }?.pins.first)
        let authorization = try await reopened.authorizeSSHAuthentication(workspaceID: receipt.workspace.id,
            expectedRevision: receipt.workspace.revision, routeID: target.id, credentialID: reference.id,
            presentedHostKey: pin, support: support)
        XCTAssertEqual(authorization.userCertificate?.blob, fixture.certificateBlob)
        XCTAssertTrue(try XCTUnwrap(saved.first).connectLater)
    }

    func testKnownHostsPinsRevocationsNegationAndPortBinding() throws {
        let known = try hostLine("*,!gpu", seed: 8) + hostLine("gpu", seed: 16) + hostLine("*", seed: 8, marker: "@revoked")
            + hostLine("[2001:db8::1]:2222", seed: 24) + hostLine("[2001:db8::1]:22", seed: 32)
        let result = try exported("Host build gpu\nIdentityFile none\nHost ipv6\nHostName 2001:db8::1\nPort 2222\nIdentityFile none\n", known: Data(known.utf8))
        let trusts = try records(result, "host_trust")
        XCTAssertEqual(try trusts[0].required("pins").array().count, 1)
        XCTAssertEqual(try trusts[1].required("pins").array().count, 1)
        XCTAssertEqual(try trusts[2].required("pins").array().count, 2)
        for trust in trusts { XCTAssertEqual(try trust.required("revoked_keys").array().count, 1) }
        XCTAssertEqual(try trusts[0].required("pins").array()[0], try trusts[0].required("revoked_keys").array()[0], "A revoked pin remains explicitly denied; export must not drop the tombstone.")
        XCTAssertNotEqual(trusts[0]["pins"], trusts[1]["pins"])
    }

    func testHostKeyAliasUsesItsOwnLookupNameAndCanonicalEndpointKeepsDisplayAlias() throws {
        let known = try hostLine("verified-alias", seed: 8) + hostLine("[verified-alias]:2200", seed: 16)
            + hostLine("build.example.invalid.", seed: 24)
        let result = try exported("""
        Host build_node
          HostName BUILD.Example.Invalid.
          HostKeyAlias verified-alias
          Port 2200
          IdentityFile none
        Host normal
          HostName BUILD.Example.Invalid.
          IdentityFile none
        """, known: Data(known.utf8))
        XCTAssertEqual(result.workspace.machines[0].name, "build_node")
        XCTAssertEqual(result.workspace.routes.map(\.host), ["build.example.invalid", "build.example.invalid"])
        let routes = try records(result, "routes"), trusts = try records(result, "host_trust")
        XCTAssertEqual(routes[0]["host_key_alias"], .string("verified-alias"))
        XCTAssertEqual(trusts[0]["host_key_alias"], .string("verified-alias"))
        XCTAssertNil(routes[0]["required_capabilities"])
        XCTAssertEqual(try trusts[0].required("pins").array().count, 1)
        XCTAssertEqual(try trusts[1].required("pins").array().count, 1)
        XCTAssertNotEqual(trusts[0]["pins"], trusts[1]["pins"])
    }

    func testNormalizationNeverDropsOriginalOrCanonicalRevocationOrBroadensPositiveTrust() throws {
        let known = try hostLine("verified-alias.", seed: 8, marker: "@revoked")
            + hostLine("verified-alias", seed: 16, marker: "@revoked")
            + hostLine("verified-alias.", seed: 24) + hostLine("verified-alias", seed: 32)
        let result = try exported("Host build\nHostKeyAlias Verified-Alias.\nPort 2200\nIdentityFile none\n", known: Data(known.utf8))
        let trust = try XCTUnwrap(records(result, "host_trust").first)
        XCTAssertEqual(trust["host_key_alias"], .string("verified-alias."))
        XCTAssertEqual(try trust.required("pins").array().count, 1, "Only the source lookup may supply positive trust.")
        XCTAssertEqual(try trust.required("revoked_keys").array().count, 2, "Neither spelling's denial may disappear during normalization.")
    }

    func testUnevaluatedCodeIncludesAndSecurityConstraintsStayBlockedAndInert() throws {
        let marker = "/tmp/frankenterm-should-never-execute-" + UUID().uuidString
        let input = try source("""
        Include /unreadable/config-fragment
        Host build
          IdentityFile none
          Ciphers aes256-gcm@openssh.com
          RevokedHostKeys /required/host-revocations
          ProxyCommand sh -c "echo SENSITIVE_VALUE > \(marker)"
        Match exec "touch \(marker)"
          HostName never-evaluated.invalid
        Host gpu
          HostName gpu.example.invalid
          IdentityFile none
        """)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: input.aliases, workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)
        XCTAssertFalse(FileManager.default.fileExists(atPath: marker))
        for route in try records(result, "routes") {
            XCTAssertEqual(route["required_capabilities"], .array([.string("ssh_config.unresolved_policy")]))
        }
        for setting in ["include", "match", "ciphers", "revokedhostkeys", "proxycommand"] {
            XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config." + setting && $0.disposition == 1 }, setting)
        }
        XCTAssertEqual(result.workspace.routes[0].host, "build")
        let json = String(decoding: result.workspace.raw, as: UTF8.self)
        XCTAssertFalse(json.contains("SENSITIVE_VALUE"))
        XCTAssertFalse(json.contains(marker))
        XCTAssertFalse(result.portability.contains { $0.reason.contains("SENSITIVE_VALUE") || $0.reason.contains(marker) })
    }

    func testFirstProxyWinsAndUnknownPolicyDoesNotBlockUnrelatedRoute() throws {
        let result = try exported("""
        Host ordinary
          ProxyCommand none
          ProxyJump unused-bastion
          IdentityFile none
        Host restricted
          HostKeyAlgorithms ssh-ed25519
          KexAlgorithms curve25519-sha256
          IdentityFile none
        """)
        let routes = try records(result, "routes")
        XCTAssertNil(routes[0]["required_capabilities"])
        XCTAssertNotNil(routes[1]["required_capabilities"])
        XCTAssertFalse(result.portability.contains { $0.entityID == result.workspace.routes[0].id && $0.disposition == 1 })
        XCTAssertTrue(result.portability.contains { $0.entityID == result.workspace.routes[1].id && $0.setting == "ssh_config.hostkeyalgorithms" })
    }

    func testMalformedHashedTrustAndHostCAsNeverBecomePositivePins() throws {
        let known = try hostLine("|1|c2FsdA==|aGFzaA==", marker: "@revoked")
            + hostLine("build", seed: 4, marker: "@cert-authority")
            + hostLine("build", seed: 8)
        let result = try exported("Host build gpu\nIdentityFile none\n", known: Data(known.utf8))
        XCTAssertFalse(result.portability.contains { $0.setting == "ssh_config.host_ca" && $0.disposition == 1 })
        XCTAssertEqual(result.workspace.hostTrust[0].certificateAuthorities.count, 1)
        XCTAssertTrue(result.workspace.hostTrust[1].certificateAuthorities.isEmpty)
        XCTAssertTrue(result.workspace.routes.allSatisfy { !$0.requiredCapabilities.isEmpty },
            "a supported CA cannot remove an unresolved malformed revocation source")
        XCTAssertEqual(result.portability.filter { $0.setting == "ssh_config.hashed_known_hosts" && $0.disposition == 1 }.count, 2)
        let trusts = try records(result, "host_trust")
        XCTAssertEqual(try trusts[0].required("pins").array().count, 1)
        XCTAssertTrue(try trusts[1].required("pins").array().isEmpty)
        let malformed = try exported("Host build\nIdentityFile none\n", known: Data("@revoked incomplete\n".utf8))
        XCTAssertNotNil(try records(malformed, "routes")[0]["required_capabilities"])
    }

    func testKnownHostHMACMatchesIndependentKnownAnswerAndBoundsInputs() throws {
        // RFC 2202 test case 1, independent of both native implementations.
        let result = try crypto.hmacSHA1(key: Data(repeating: 0x0b, count: 20), message: Data("Hi There".utf8))
        XCTAssertEqual(result.ftHex.lowercased(), "b617318655057264e28bc0b6fb378c8ef146be00")
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 19), message: Data([1])))
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 21), message: Data([1])))
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 20), message: Data()))
        XCTAssertThrowsError(try crypto.hmacSHA1(key: Data(repeating: 0, count: 20), message: Data(repeating: 1, count: FTLimit.text + 1)))
    }

    func testIndependentOpenSSHHashedFixturesMatchExactSourceLookupNames() throws {
        // Generated with `ssh-keygen -H -f <synthetic-known-hosts>` and each
        // independently located with `ssh-keygen -F <lookup> -f <file>`.
        // The key is RFC 8032's public test vector; it cannot convey a secret.
        let fixtures = [
            "|1|aDeDAUKmHFuLWoGhGWF42FpIfik=|eWeKZpxS/fL0nuUZsSEzazPtB74=", // build.example.invalid
            "|1|s8qDi2B3JjeqdlcGU8FEnJK+0xI=|Uf6sewTNMZrOm+o1IbYL6eSQGlk=", // [port.example.invalid]:2200
            "|1|YfWZDMVb2Cq66apUKfjfhqOxOwo=|6PW29LS887CTgKV+QBDMrRcmY9k=", // [2001:db8::1]:2222
            "|1|398GT5aJEJgtUuUoSid98Ynu3MQ=|MekHkSHl8j6umpJvS6zjrp/4cPI=", // verified-alias.
            "|1|E+aW6MMGRqNIei+xpbk+mTqZ1Ac=|/jJzSlOu3MNdKq9tc3+4qgBYqIA=", // [::ffff:192.0.2.1]:2222
            "|1|KkQ0QueSKOljC6ONfi094X2qZdk=|1T3UVv/Bbh8zQ+xdMoTyASilYUE=" // [::192.0.2.1]:2222
        ]
        let publicKey = " ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAINdamAGCsQq31Uv+08lkBzoO4XLz2qYjJa8CGmj3B1Ea\n"
        let unrelated = "|1|0rxKPTlV0RN+uyV1ePRHg/HycR0=|r8dv/qGevaDsAhPp9dExqPvvJ6E="
        let known = fixtures.map { $0 + publicKey }.joined() + "@revoked " + unrelated + publicKey
        let result = try exported("""
        IdentityFile none
        Host dns
          HostName BUILD.Example.Invalid
        Host port
          HostName Port.Example.Invalid
          Port 2200
        Host ipv6
          HostName 2001:0DB8:0:0::1
          Port 2222
        Host alias
          HostName endpoint.example.invalid
          HostKeyAlias Verified-Alias.
          Port 65535
        Host mapped
          HostName ::FFFF:C000:201
          Port 2222
        Host compatible
          HostName ::C000:201
          Port 2222
        Host unmatched
          HostName not-in-this-file.example.invalid
        """, known: Data(known.utf8))
        for trust in result.workspace.hostTrust.prefix(6) {
            XCTAssertEqual(trust.pins.count, 1)
            XCTAssertTrue(trust.revokedKeys.isEmpty)
            XCTAssertEqual(trust.evaluate(try XCTUnwrap(trust.pins.first), locallyAccepted: false), .reviewRequired)
        }
        XCTAssertTrue(result.workspace.hostTrust[6].pins.isEmpty)
        XCTAssertTrue(result.workspace.hostTrust[6].revokedKeys.isEmpty)
        XCTAssertEqual(result.workspace.routes[4].host, "192.0.2.1")
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        XCTAssertFalse(result.portability.contains { $0.setting == "ssh_config.hashed_known_hosts" })
        let json = String(decoding: result.workspace.raw, as: UTF8.self)
        XCTAssertFalse(json.contains("|1|"))
        XCTAssertFalse(json.contains("aDeDAUKmHFuLWoGhGWF42FpIfik="))
    }

    func testHashedLookupDoesNotBroadenTrustAcrossPortsCaseOrTrailingDots() throws {
        let known = try hashedLine("[build.example.invalid]:2200")
            + hashedLine("verified-alias.") + hashedLine("UPPER.example.invalid")
            + hashedLine("build.example.invalid")
        let result = try exported("""
        IdentityFile none
        Host wrong-port
          HostName build.example.invalid
          Port 2201
        Host alias-without-dot
          HostKeyAlias verified-alias
          Port 2200
        Host lowercased
          HostName UPPER.example.invalid
        Host dotted
          HostName build.example.invalid.
        """, known: Data(known.utf8))
        XCTAssertTrue(result.workspace.hostTrust.allSatisfy { $0.pins.isEmpty })
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
    }

    func testHashedRevocationsSurviveNormalizationAndOverridePositivePins() throws {
        let known = try hashedLine("verified-alias.", seed: 8, marker: "@revoked")
            + hashedLine("verified-alias", seed: 16, marker: "@revoked")
            + hashedLine("verified-alias.", seed: 16) + hashedLine("verified-alias", seed: 32)
            + hashedLine("unrelated-alias", seed: 48, marker: "@revoked")
        let result = try exported("Host build\nHostKeyAlias Verified-Alias.\nPort 2200\nIdentityFile none\n", known: Data(known.utf8))
        let trust = try XCTUnwrap(result.workspace.hostTrust.first)
        XCTAssertEqual(trust.pins.count, 1)
        XCTAssertEqual(trust.revokedKeys.count, 2)
        let pin = try XCTUnwrap(trust.pins.first)
        XCTAssertEqual(trust.evaluate(pin, locallyAccepted: true), .revoked)
        XCTAssertEqual(try result.workspace.verifyHostKey(forRouteID: result.workspace.routes[0].id, presented: pin,
                                                         reviewedTrustIDs: [trust.id]), .revoked)
    }

    func testHashedUnknownMarkerConstraintsStayInactiveOnlyWhenApplicable() throws {
        for (marker, setting) in [("@future-policy", "known_hosts_marker")] {
            // A canonical-spelling constraint must survive source normalization,
            // even though a canonical-spelling positive pin is not broadened.
            let known = try hashedLine("verified-alias", seed: 8, marker: marker)
                + hashedLine("verified-alias.", seed: 16)
            let result = try exported("IdentityFile none\nHost matched\nHostKeyAlias Verified-Alias.\nHost unrelated\n", known: Data(known.utf8))
            XCTAssertEqual(result.workspace.hostTrust[0].pins.count, 1)
            XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty)
            XCTAssertTrue(result.workspace.routes[1].requiredCapabilities.isEmpty)
            XCTAssertTrue(result.portability.contains {
                $0.entityID == result.workspace.routes[0].id && $0.setting == "ssh_config." + setting && $0.disposition == 1
            })
        }
        let invalidKey = try hashedName("build") + " ssh-ed25519 not-base64\n"
        let result = try exported("Host build unrelated\nIdentityFile none\n", known: Data(invalidKey.utf8))
        XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty)
        XCTAssertTrue(result.workspace.routes[1].requiredCapabilities.isEmpty)
        XCTAssertTrue(result.workspace.hostTrust.allSatisfy { $0.pins.isEmpty })
    }

    func testCertificateAuthorityTrustAndJumpScopeSurviveEncryptedTransfer() throws {
        let known = try hostLine("*.corp,!raw.corp", seed: 8, marker: "@cert-authority")
            + hostLine("gate-verified", seed: 16, marker: "@cert-authority")
            + hostLine("[gate-verified]:2222", seed: 24, marker: "@cert-authority")
            + hostLine("raw.corp", seed: 32)
        let result = try exported("""
        IdentityFile none
        PreferredAuthentications password
        Host app
          HostName app.corp
          ProxyJump gate
        Host raw
          HostName raw.corp
        Host gate
          HostName gate.internal
          HostKeyAlias gate-verified
          Port 2222
        """, aliases: ["app", "raw"], known: Data(known.utf8))
        let app = try destination("app", in: result.workspace), raw = try destination("raw", in: result.workspace)
        let hop = try XCTUnwrap(hops(app, in: result.workspace).first)
        XCTAssertEqual(hop.hostKeyAlias, "gate-verified"); XCTAssertEqual(hop.port, 2222)
        func trust(_ route: WorkspaceDocument.Route) throws -> WorkspaceDocument.HostTrust {
            try XCTUnwrap(result.workspace.hostTrust.first { $0.id == route.trustID })
        }
        let appTrust = try trust(app), hopTrust = try trust(hop), rawTrust = try trust(raw)
        XCTAssertTrue(appTrust.pins.isEmpty); XCTAssertTrue(hopTrust.pins.isEmpty)
        XCTAssertEqual(appTrust.certificateAuthorities.count, 1)
        XCTAssertEqual(hopTrust.certificateAuthorities.count, 1)
        XCTAssertNotEqual(appTrust.certificateAuthorities, hopTrust.certificateAuthorities)
        XCTAssertEqual(appTrust.requiredCapabilities, [WorkspaceDocument.hostCertificateCapability])
        XCTAssertEqual(hopTrust.requiredCapabilities, [WorkspaceDocument.hostCertificateCapability])
        XCTAssertEqual(rawTrust.pins.count, 1); XCTAssertTrue(rawTrust.certificateAuthorities.isEmpty)
        XCTAssertTrue(rawTrust.requiredCapabilities.isEmpty)
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        let selection = try WorkspaceExportSelection(workspace: result.workspace,
            groupIDs: [result.workspace.initialGroupID], findings: result.portability)
        XCTAssertTrue(try selection.review(decisions: [:], loadedFingerprints: []).blockedRouteIDs.isEmpty)
        let sealed = try FTAG.seal(selection.archive(including: [], using: crypto), using: crypto)
        let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(restored.workspace.hostTrust.map(\.certificateAuthorities),
            selection.workspace.hostTrust.map(\.certificateAuthorities))
        XCTAssertEqual(restored.workspace.routes.map(\.jumpRoutes), selection.workspace.routes.map(\.jumpRoutes))
        XCTAssertFalse(restored.portability.contains { $0.disposition == 1 })
    }

    func testCertificateAuthoritiesDoNotSatisfyUnsupportedIssuerOrRevocationPolicy() throws {
        let known = try hostLine("build", seed: 8, marker: "@cert-authority") + hostLine("build", seed: 16)
        for policy in ["CASignatureAlgorithms ssh-ed25519", "CASignatureAlgorithms -ssh-rsa",
                       "HostKeyAlgorithms ssh-ed25519-cert-v01@openssh.com", "RevokedHostKeys /selected/revocations.krl"] {
            let result = try exported("Host build\nIdentityFile none\n\(policy)\n", known: Data(known.utf8))
            XCTAssertEqual(result.workspace.hostTrust[0].certificateAuthorities.count, 1, policy)
            XCTAssertEqual(result.workspace.hostTrust[0].pins.count, 1)
            XCTAssertEqual(result.workspace.hostTrust[0].requiredCapabilities, [WorkspaceDocument.hostCertificateCapability])
            XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty, policy)
            XCTAssertTrue(result.portability.contains { $0.disposition == 1 }, policy)
        }
    }

    func testHashedCertificateAuthoritiesNeedOriginalLookupAndRetainCanonicalRevocations() throws {
        let known = try hashedLine("verified-alias", seed: 8, marker: "@cert-authority")
            + hashedLine("verified-alias.", seed: 16, marker: "@cert-authority")
            + hashedLine("verified-alias.", seed: 16, marker: "@cert-authority")
            + hashedLine("verified-alias", seed: 16, marker: "@revoked")
            + hashedLine("verified-alias.", seed: 32)
        let result = try exported("Host build\nHostKeyAlias Verified-Alias.\nPort 2200\nIdentityFile none\n", known: Data(known.utf8))
        let trust = try XCTUnwrap(result.workspace.hostTrust.first)
        XCTAssertEqual(trust.certificateAuthorities.count, 1, "normalization cannot grant another authority")
        XCTAssertEqual(trust.certificateAuthorities, trust.revokedKeys, "canonical denials still bind the source authority")
        XCTAssertEqual(trust.pins.count, 1)
        XCTAssertFalse(trust.certificateAuthorities.contains(try XCTUnwrap(trust.pins.first)))
        XCTAssertEqual(trust.requiredCapabilities, [WorkspaceDocument.hostCertificateCapability])
        XCTAssertTrue(result.workspace.routes[0].requiredCapabilities.isEmpty)
        let wrongPort = try exported("Host build\nHostName box\nPort 2223\nIdentityFile none\n",
            known: Data(try hashedLine("[box]:2222", marker: "@cert-authority").utf8))
        XCTAssertTrue(wrongPort.workspace.hostTrust[0].certificateAuthorities.isEmpty)
        XCTAssertTrue(wrongPort.workspace.hostTrust[0].requiredCapabilities.isEmpty)
    }

    func testCertificatePrincipalRetainsEffectiveHostnameAndAliasSpelling() throws {
        // OpenSSH ssh.c and ssh -G agree: effective names are lowercase and
        // retain trailing dots, unlike the canonical mobile denial scope.
        let known = try hostLine("reviewed-alias.", seed: 32, marker: "@cert-authority")
            + hostLine("target.example.invalid.", seed: 32, marker: "@cert-authority")
        let result = try exported("""
        IdentityFile none
        Host alias
          HostName TARGET.Example.Invalid.
          HostKeyAlias Reviewed-Alias.
        Host hostname
          HostName TARGET.Example.Invalid.
        """, known: Data(known.utf8))
        let routes = try records(result, "routes"), trusts = try records(result, "host_trust")
        XCTAssertEqual(routes[0]["host_key_alias"], .string("reviewed-alias."))
        XCTAssertEqual(trusts[0]["host_key_alias"], .string("reviewed-alias."))
        XCTAssertEqual(routes[1]["host"], .string("target.example.invalid."))
        XCTAssertEqual(trusts[1]["host"], .string("target.example.invalid."))
        XCTAssertEqual(result.workspace.routes.map(\.host), ["target.example.invalid", "target.example.invalid"])
        XCTAssertEqual(result.workspace.hostTrust.map(\.certificatePrincipal), ["reviewed-alias.", "target.example.invalid."])
        XCTAssertTrue(result.workspace.hostTrust.allSatisfy { $0.certificateAuthorities.count == 1 })
        let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519-cert-v01@openssh.com"],
            capabilities: [WorkspaceDocument.hostCertificateCapability])
        for (index, principal) in ["reviewed-alias.", "target.example.invalid."].enumerated() {
            for offered in [principal, String(principal.dropLast()), principal.uppercased()] {
                let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: [offered]))
                let decision = try result.workspace.verifyHostKey(forRouteID: result.workspace.routes[index].id,
                    presented: .init(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint),
                    hostCertificate: certificate, reviewedTrustIDs: Set(result.workspace.hostTrust.map(\.id)), support: support)
                XCTAssertEqual(decision, offered == principal ? .trustedCertificate : .unsupportedCertificate, offered)
            }
        }
    }

    func testEffectiveCertificateNamesUseASCIIBytesWithoutUnicodeCaseFolding() throws {
        // Both installed ssh -G and upstream misc.c lowercase() leave the
        // UTF-8 bytes of Ü, İ and É unchanged while folding ASCII letters.
        let cases = [
            ("HostName BÜCHER.Example.Invalid.", "bÜcher.example.invalid.", "bücher.example.invalid."),
            ("HostKeyAlias ÜPPER-ALIAS.", "Üpper-alias.", "üpper-alias."),
            ("HostKeyAlias İCAFÉ.", "İcafÉ.", "i\u{0307}café.")
        ]
        let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519-cert-v01@openssh.com"],
            capabilities: [WorkspaceDocument.hostCertificateCapability])
        for (option, expected, folded) in cases {
            let canonical = try WorkspaceDocument.host(expected)
            let known = try hostLine(expected, seed: 32, marker: "@cert-authority")
                + hashedLine(expected, seed: 32, marker: "@cert-authority")
                + hostLine(folded, seed: 16, marker: "@cert-authority")
                + hostLine(canonical, seed: 24, marker: "@cert-authority")
                + hostLine(canonical, seed: 8, marker: "@revoked")
            let result = try exported("Host build\nIdentityFile none\n\(option)\n", known: Data(known.utf8))
            let trust = try XCTUnwrap(result.workspace.hostTrust.first)
            XCTAssertTrue(trust.certificatePrincipal.utf8.elementsEqual(expected.utf8), option)
            XCTAssertEqual(trust.certificateAuthorities.count, 1,
                "neither Unicode case folding nor IDNA may grant another CA")
            XCTAssertEqual(trust.revokedKeys.count, 1, "canonical denials remain effective")
            for principal in [expected, folded, canonical] {
                let certificate = try SSHHostCertificate(blob: workspaceTestHostCertificate(principals: [principal]))
                let decision = try result.workspace.verifyHostKey(forRouteID: result.workspace.routes[0].id,
                    presented: .init(algorithm: certificate.algorithm, fingerprint: certificate.fingerprint),
                    hostCertificate: certificate, reviewedTrustIDs: [trust.id], support: support)
                XCTAssertEqual(decision, principal.utf8.elementsEqual(expected.utf8) ? .trustedCertificate : .unsupportedCertificate)
            }
        }
    }

    func testMalformedOrUnsupportedHashesCannotHideRevocations() throws {
        let valid = "|1|aDeDAUKmHFuLWoGhGWF42FpIfik=|eWeKZpxS/fL0nuUZsSEzazPtB74="
        let invalid = [
            valid.replacingOccurrences(of: "|1|", with: "|2|"),
            "|1|c2FsdA==|eWeKZpxS/fL0nuUZsSEzazPtB74=",
            "|1|aDeDAUKmHFuLWoGhGWF42FpIfik=|aGFzaA==",
            String(valid.dropFirst()), String(valid.dropLast()), valid + "|extra",
            String(valid.dropLast(2)) + "5=", "!" + valid,
            valid + ",build", "build," + valid, "*" + valid
        ]
        for value in invalid {
            let known = try hostLine(value, marker: "@revoked")
            let result = try exported("Host build unrelated\nIdentityFile none\n", known: Data(known.utf8))
            XCTAssertTrue(result.workspace.routes.allSatisfy { !$0.requiredCapabilities.isEmpty }, value)
            XCTAssertTrue(result.workspace.hostTrust.allSatisfy { $0.pins.isEmpty && $0.revokedKeys.isEmpty }, value)
            XCTAssertEqual(result.portability.filter { $0.setting == "ssh_config.hashed_known_hosts" && $0.disposition == 1 }.count, 2)
        }
        let conflictingMarkers = "@revoked @cert-authority " + valid + " " + String(decoding: try publicFile(), as: UTF8.self)
        let result = try exported("Host build\nIdentityFile none\n", known: Data(conflictingMarkers.utf8))
        XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty)
    }

    func testHashedTrustComparisonLimitCannotPublishPartialRegistryOrSelection() throws {
        let names = (0..<128).map { "alias-\($0)" }
        let input = try source("Host " + names.joined(separator: " ") + "\nIdentityFile none\n")
        let repeated = try hashedLine("not-selected.example.invalid")
        let count = SSHWorkspaceSource.maximumKnownHostHashComparisons / names.count + 1
        let known = Data(String(repeating: repeated, count: count).utf8)
        var registry = SSHWorkspaceRegistry()
        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
        let before = try encoder.encode(registry)
        XCTAssertThrowsError(try input.export(aliases: names, workspaceName: "Fleet", defaultUser: "operator",
                                              knownHosts: known, registry: &registry, using: crypto)) { error in
            XCTAssertEqual(error as? FTError, .limit("ssh.known_hosts_hash_work"))
        }
        XCTAssertEqual(try encoder.encode(registry), before)
    }

    func testAllSupportedECDSACurvesAreOfferedAsExportableIdentityFamilies() throws {
        func ssh(_ data: Data) -> Data {
            var prefix = Data(); prefix.appendBE(UInt64(data.count), bytes: 4)
            return prefix + data
        }
        var configuration = "", keys: [String: Data] = [:]
        for curve in SSHECDSACurve.allCases {
            let reference = "/selected/" + curve.rawValue
            var scalar = Data(repeating: 0, count: curve.scalarBytes); scalar[scalar.count - 1] = 1
            let point = try crypto.ecdsaPublicKey(scalar: scalar, curve: curve)
            let blob = ssh(Data(curve.algorithm.utf8)) + ssh(Data(curve.rawValue.utf8)) + ssh(point)
            keys[reference] = Data((curve.algorithm + " " + blob.base64EncodedString()).utf8)
            configuration += "Host " + curve.rawValue + "\nIdentityFile " + reference + "\n"
        }
        let result = try exported(configuration, keys: keys)
        XCTAssertEqual(result.workspace.credentials.map(\.algorithm), SSHECDSACurve.allCases.map(\.algorithm))
        XCTAssertFalse(result.portability.contains { $0.setting == "ssh_config.private_key_format" })
    }

    func testOrderedAuthenticationDisallowsDisabledMethodsAndFailsClosedWhenEmpty() throws {
        let result = try exported("""
        Host build
          PreferredAuthentications password,publickey,keyboard-interactive
          PasswordAuthentication no
          ChallengeResponseAuthentication yes
          KbdInteractiveAuthentication no
          IdentityFile none
        Host blocked
          PubkeyAuthentication no
          PasswordAuthentication no
          KbdInteractiveAuthentication no
        """)
        let routes = try records(result, "routes")
        XCTAssertEqual(routes[0]["authentication_methods"], .array([.string("publickey"), .string("keyboard-interactive")]))
        XCTAssertNotNil(routes[1]["required_capabilities"])
        XCTAssertTrue(result.portability.contains { $0.entityID == result.workspace.routes[1].id && $0.setting == "ssh_config.authentication" && $0.disposition == 1 })
    }

    func testInvalidPublicKeyAndUnresolvedTokensCannotPublishUsableIdentity() throws {
        let invalid = Data("ssh-rsa AAAAC3NzaC1lZDI1NTE5AAAAIAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA\n".utf8)
        let result = try exported("""
        Host build
          HostName %h.internal
          IdentityFile ~/.ssh/invalid
          IdentityFile ~/.ssh/%r-key
        """, keys: ["~/.ssh/invalid": invalid])
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        XCTAssertNotNil(try records(result, "routes")[0]["required_capabilities"])
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.hostname" && $0.disposition == 1 })
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.identityfile" && $0.disposition == 1 })
        XCTAssertEqual(try source("Host build\nIdentityFile ~/.ssh/%r-key\n").identityFileReferences(for: ["build"]), [])
    }

    func testNoPartialIdentityPrefixAndInvalidRegistryHasNoPublication() throws {
        let identities = (0..<9).map { "IdentityFile ~/.ssh/key-\($0)" }.joined(separator: "\n")
        let input = try source("Host build\n" + identities)
        XCTAssertTrue(try input.identityFileReferences(for: ["build"]).isEmpty)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: ["build"], workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)
        XCTAssertTrue(result.workspace.credentials.isEmpty)
        XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.identityfile" && $0.disposition == 1 })
        let corrupt = Data(#"{"exporterID":"same","workspaceID":"same","groupID":"group","themeID":"theme","entities":{}}"#.utf8)
        registry = try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: corrupt)
        XCTAssertThrowsError(try input.export(aliases: ["build"], workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto))
    }

    func testSharedProxyJumpIncludesHopIdentityAndTrustWithoutAddingAMachine() throws {
        let input = try source("""
        Host build gpu
          User target-user
          Port 2222
          IdentityFile ~/.ssh/fleet
          PreferredAuthentications publickey
          ProxyJump gate
        Host gate
          HostName gate.example.invalid
          User bastion-user
          Port 2200
          IdentityFile ~/.ssh/gate
          PreferredAuthentications publickey
        """)
        XCTAssertEqual(try input.identityFileReferences(for: ["build", "gpu"]), ["~/.ssh/fleet", "~/.ssh/gate"])
        let known = try hashedLine("[build]:2222", seed: 8) + hashedLine("[gpu]:2222", seed: 16)
            + hashedLine("[gate.example.invalid]:2200", seed: 24)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: ["build", "gpu"], workspaceName: "Fleet", defaultUser: "local-user",
            publicKeys: ["~/.ssh/fleet": try publicFile(), "~/.ssh/gate": try publicFile(32)],
            knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        let document = result.workspace
        XCTAssertEqual(document.machines.map(\.name), ["build", "gpu"])
        XCTAssertEqual(document.routes.count, 3)
        XCTAssertEqual(document.hostTrust.count, 3)
        XCTAssertEqual(document.credentials.count, 2)
        let build = try destination("build", in: document), gpu = try destination("gpu", in: document)
        XCTAssertEqual(build.jumpRoutes, gpu.jumpRoutes)
        let gate = try XCTUnwrap(hops(build, in: document).first)
        XCTAssertEqual(gate.host, "gate.example.invalid")
        XCTAssertEqual(gate.user, "bastion-user")
        XCTAssertEqual(gate.port, 2200)
        XCTAssertNil(gate.sessionCreation)
        XCTAssertTrue(gate.jumpRoutes.isEmpty)
        XCTAssertNotEqual(build.credentialIDs, gate.credentialIDs)
        XCTAssertTrue(document.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        XCTAssertTrue(document.hostTrust.allSatisfy { $0.pins.count == 1 })

        let original = try sampleKey(using: crypto).0
        let key = PrivateKeyRecord(credentialID: try XCTUnwrap(build.credentialIDs.first), container: original.container, publicBlob: original.publicBlob)
        let selection = try WorkspaceExportSelection(workspace: document, groupIDs: [document.initialGroupID], findings: result.portability)
        let archive = try selection.archive(including: [key], using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(restored.workspace.routes.map(\.id), document.routes.map(\.id))
        XCTAssertEqual(try destination("build", in: restored.workspace).jumpRoutes, build.jumpRoutes)
        XCTAssertEqual(restored.keys.count, 1)
        XCTAssertEqual(restored.keys[0].container, original.container)
        XCTAssertEqual(restored.workspace.machines.map(\.name), ["build", "gpu"])
    }

    func testProxyJumpOrderExpandsFirstHopAndOverridesLaterConfiguredChains() throws {
        // OpenSSH ssh.c passes earlier comma entries via -J to the last entry.
        // Thus edge inherits outer, while inner's configured decoy is overridden:
        // outer -> edge -> inner -> target. Destination options do not leak to hops.
        let input = try source("""
        IdentityFile none
        Host target
          HostName target.example.invalid
          User target-user
          Port 443
          ProxyJump edge,inner
        Host edge
          HostName edge.example.invalid
          User edge-user
          ProxyJump outer
        Host outer
          HostName outer.example.invalid
          Port 10022
        Host inner
          HostName inner.example.invalid
          User inner-user
          Port 2200
          ProxyJump decoy
        Host decoy
          IdentityFile /not-a-dependency/decoy
        """)
        XCTAssertTrue(try input.identityFileReferences(for: ["target"]).isEmpty)
        var registry = SSHWorkspaceRegistry()
        let result = try input.export(aliases: ["target"], workspaceName: "Fleet", defaultUser: "local-user", registry: &registry, using: crypto)
        let route = try destination("target", in: result.workspace)
        let chain = try hops(route, in: result.workspace)
        XCTAssertEqual(chain.map(\.host), ["outer.example.invalid", "edge.example.invalid", "inner.example.invalid"])
        XCTAssertEqual(chain.map(\.user), ["local-user", "edge-user", "inner-user"])
        XCTAssertEqual(chain.map(\.port), [10022, 22, 2200])
        XCTAssertTrue(chain.allSatisfy { $0.jumpRoutes.isEmpty && $0.sessionCreation == nil })
        XCTAssertEqual(route.sessionCreation, "on_select")
        XCTAssertEqual(result.workspace.routes.count, 4)
        XCTAssertEqual(result.workspace.machines.map(\.name), ["target"])
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        XCTAssertFalse(String(decoding: result.workspace.raw, as: UTF8.self).contains("decoy"))
    }

    func testQualifiedProxyJumpOverridesOnlyItsOwnUserAndPortAndKeepsHostPolicy() throws {
        // Independent OpenSSH -G resolution of -l jump-user -p 2200 gate uses
        // gate's HostName, IdentityFile and HostKeyAlias, not target's options.
        let input = try source("""
        IdentityFile none
        Host target
          HostName target.example.invalid
          User target-user
          Port 443
          ProxyJump jump-user@gate:2200
          IdentityFile ~/.ssh/target
        Host gate
          HostName gate.example.invalid
          User configured-user
          Port 2022
          HostKeyAlias verified-gateway
          IdentityFile ~/.ssh/gate
          PreferredAuthentications publickey
        """)
        XCTAssertEqual(try input.identityFileReferences(for: ["target"]), ["~/.ssh/target", "~/.ssh/gate"])
        var registry = SSHWorkspaceRegistry()
        let known = try hostLine("verified-gateway", seed: 8) + hostLine("[gate.example.invalid]:2200", seed: 16)
        let result = try input.export(aliases: ["target"], workspaceName: "Qualified jumps", defaultUser: "local-user",
            publicKeys: ["~/.ssh/target": try publicFile(), "~/.ssh/gate": try publicFile(32)],
            knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        let target = try destination("target", in: result.workspace)
        let gate = try XCTUnwrap(hops(target, in: result.workspace).first)
        XCTAssertEqual(target.host, "target.example.invalid")
        XCTAssertEqual(target.user, "target-user"); XCTAssertEqual(target.port, 443)
        XCTAssertEqual(gate.host, "gate.example.invalid")
        XCTAssertEqual(gate.user, "jump-user"); XCTAssertEqual(gate.port, 2200)
        XCTAssertEqual(gate.hostKeyAlias, "verified-gateway")
        XCTAssertEqual(gate.authenticationMethods, [.publicKey])
        XCTAssertNotEqual(gate.credentialIDs, target.credentialIDs)
        XCTAssertTrue(gate.requiredCapabilities.isEmpty)
        let pin = try XCTUnwrap(result.workspace.hostTrust.first { $0.id == gate.trustID })
        XCTAssertEqual(pin.host, gate.host); XCTAssertEqual(pin.port, gate.port)
        let expectedPoint = try crypto.ed25519PublicKey(seed: Data((0..<32).map { 8 + UInt8($0) }))
        let expectedPin = try SSHPublicIdentity(blob: publicBlob(expectedPoint))
        XCTAssertEqual(pin.pins.map(\.fingerprint), [expectedPin.fingerprint])
        XCTAssertNil(gate.sessionCreation)
        XCTAssertEqual(result.workspace.machines.map(\.name), ["target"])
    }

    func testLiteralAndBracketedIPv6JumpsResolveWildcardIdentityAndTrustSources() throws {
        let input = try source("""
        IdentityFile none
        Host target
          User target-user
          ProxyJump edge-user@edge.example.invalid:2222,[2001:db8::5]:2205
        Host *.example.invalid
          User wildcard-user
          IdentityFile ~/.ssh/edge
          UserKnownHostsFile /selected/edge-pins
          GlobalKnownHostsFile none
        Host 2001:db8::*
          User ipv6-user
          IdentityFile ~/.ssh/ipv6
          UserKnownHostsFile /selected/ipv6-pins
          GlobalKnownHostsFile none
        """)
        XCTAssertEqual(input.aliases, ["target"])
        XCTAssertEqual(try input.identityFileReferences(for: ["target"]), ["~/.ssh/edge", "~/.ssh/ipv6"])
        let files = try SSHWorkspaceSource.KnownHostsSnapshot(files: [
            "~/.ssh/known_hosts": .missing, "~/.ssh/known_hosts2": .missing,
            "/etc/ssh/ssh_known_hosts": .missing, "/etc/ssh/ssh_known_hosts2": .missing,
            "/selected/edge-pins": .present(Data(try hostLine("[edge.example.invalid]:2222", seed: 8).utf8)),
            "/selected/ipv6-pins": .present(Data(try hostLine("[2001:db8::5]:2205", seed: 16).utf8)),
        ])
        XCTAssertEqual(Set(try input.knownHostsFileReferences(for: ["target"])), Set(files.files.keys))
        let result = try exportFiles(input, files, aliases: ["target"],
                                     keys: ["~/.ssh/edge": publicFile(), "~/.ssh/ipv6": publicFile(32)])
        let target = try destination("target", in: result.workspace)
        let chain = try hops(target, in: result.workspace)
        XCTAssertEqual(chain.map(\.host), ["edge.example.invalid", "2001:db8::5"])
        XCTAssertEqual(chain.map(\.user), ["edge-user", "ipv6-user"])
        XCTAssertEqual(chain.map(\.port), [2222, 2205])
        XCTAssertTrue(chain.allSatisfy { $0.requiredCapabilities.isEmpty && $0.jumpRoutes.isEmpty })
        for hop in chain {
            let trust = try XCTUnwrap(result.workspace.hostTrust.first { $0.id == hop.trustID })
            XCTAssertEqual(trust.host, hop.host); XCTAssertEqual(trust.port, hop.port)
            XCTAssertEqual(trust.pins.count, 1)
            XCTAssertEqual(hop.credentialIDs.count, 1)
        }
    }

    func testQualifiedJumpAndTrustIdentitiesStayDistinctStableAndDeduplicated() throws {
        let input = try source("""
        IdentityFile none
        Host alpha same
          ProxyJump alice@gate:2200
        Host beta
          ProxyJump bob@gate:2200
        Host gamma
          ProxyJump gate:2201
        Host gate
          HostName gate.example.invalid
          Port 2202
        """)
        let known = try hostLine("[gate.example.invalid]:2200", seed: 8)
            + hostLine("[gate.example.invalid]:2201", seed: 16)
            + hostLine("[gate.example.invalid]:2202", seed: 24)
        var registry = SSHWorkspaceRegistry()
        let first = try input.export(aliases: ["alpha", "same", "beta", "gamma", "gate"], workspaceName: "Ports", defaultUser: "local-user",
                                     knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        func jump(_ name: String, _ document: WorkspaceDocument) throws -> WorkspaceDocument.Route {
            try XCTUnwrap(hops(destination(name, in: document), in: document).first)
        }
        let alice = try jump("alpha", first.workspace), bob = try jump("beta", first.workspace)
        let gamma = try jump("gamma", first.workspace), direct = try destination("gate", in: first.workspace)
        XCTAssertEqual(alice.id, try jump("same", first.workspace).id)
        XCTAssertNotEqual(alice.id, bob.id)
        XCTAssertEqual(alice.trustID, bob.trustID, "The account cannot alter trust in the same host and port.")
        XCTAssertNotEqual(alice.trustID, gamma.trustID)
        XCTAssertNotEqual(gamma.trustID, direct.trustID)
        let pins = try [alice, gamma, direct].map { route in
            try XCTUnwrap(first.workspace.hostTrust.first { $0.id == route.trustID })
        }
        XCTAssertTrue(pins.allSatisfy { $0.pins.count == 1 })
        XCTAssertEqual(Set(pins.flatMap { $0.pins.map(\.fingerprint) }).count, 3)
        registry = try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: JSONEncoder().encode(registry))
        let second = try input.export(aliases: ["gamma", "beta", "alpha"], workspaceName: "Ports", defaultUser: "local-user",
                                      knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        for name in ["alpha", "beta", "gamma"] {
            let old = try jump(name, first.workspace), new = try jump(name, second.workspace)
            XCTAssertEqual(old.id, new.id); XCTAssertEqual(old.trustID, new.trustID)
            XCTAssertEqual(old.host, new.host); XCTAssertEqual(old.port, new.port); XCTAssertEqual(old.user, new.user)
        }
    }

    func testQualifiedFirstHopInheritsItsPrefixWhileLaterProxyCommandIsOverridden() throws {
        let marker = "/tmp/frankenterm-qualified-proxy-must-not-run-" + UUID().uuidString
        let result = try exported("""
        IdentityFile none
        Host target
          ProxyJump edge-user@edge:2200,inner-user@inner:2300
        Host edge
          User configured-edge
          Port 22
          ProxyJump outer-user@outer:2100
        Host outer
          HostName outer.example.invalid
        Host inner
          User configured-inner
          Port 24
          ProxyCommand sh -c "echo PRIVATE_QUALIFIED_COMMAND > \(marker)"
        """, aliases: ["target"])
        let chain = try hops(destination("target", in: result.workspace), in: result.workspace)
        XCTAssertEqual(chain.map(\.host), ["outer.example.invalid", "edge", "inner"])
        XCTAssertEqual(chain.map(\.user), ["outer-user", "edge-user", "inner-user"])
        XCTAssertEqual(chain.map(\.port), [2100, 2200, 2300])
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.requiredCapabilities.isEmpty })
        XCTAssertFalse(FileManager.default.fileExists(atPath: marker))
        XCTAssertFalse(String(decoding: result.workspace.raw, as: UTF8.self).contains("PRIVATE_QUALIFIED_COMMAND"))
        XCTAssertFalse(result.portability.contains { $0.reason.contains(marker) })
    }

    @MainActor
    func testQualifiedJumpCredentialsAndTrustRestoreThroughFTAGAndRelaunch() async throws {
        let input = try source("""
        IdentityFile ~/.ssh/shared
        PreferredAuthentications publickey
        Host target
          HostName target.example.invalid
          User target-user
          ProxyJump jump-user@gate.example.invalid:2200
        """)
        let known = try hostLine("target.example.invalid", seed: 8) + hostLine("[gate.example.invalid]:2200", seed: 16)
        var registry = SSHWorkspaceRegistry()
        let exported = try input.export(aliases: ["target"], workspaceName: "Restored jumps", defaultUser: "local-user",
            publicKeys: ["~/.ssh/shared": try publicFile()], knownHosts: Data(known.utf8), registry: &registry, using: crypto)
        let original = try sampleKey(using: crypto).0
        let key = PrivateKeyRecord(credentialID: try XCTUnwrap(exported.workspace.credentials.first?.id),
                                   container: original.container, publicBlob: original.publicBlob)
        let selection = try WorkspaceExportSelection(workspace: exported.workspace,
            groupIDs: [exported.workspace.initialGroupID], findings: exported.portability)
        let sealed = try FTAG.seal(selection.archive(including: [key], using: crypto), using: crypto)
        let decoded = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        let disk = StoreTestPersistence(), vault = StoreTestVault()
        let store = WorkspaceStore(persistence: disk, vault: vault)
        let receipt = try await store.install(decoded, envelopeDigest: crypto.sha256(sealed.envelope),
                                              expectedRevision: nil, sourceAndContentsConfirmed: true)
        XCTAssertTrue(receipt.workspace.connectLater)
        XCTAssertTrue(receipt.missingCredentialIDs.isEmpty)
        XCTAssertEqual(vault.creates, 1, "Target and jump share one validated private identity.")
        let reopened = WorkspaceStore(persistence: disk, vault: vault)
        let saved = try await reopened.load()
        let document = try XCTUnwrap(saved.first).document()
        let target = try destination("target", in: document)
        let gate = try XCTUnwrap(hops(target, in: document).first)
        XCTAssertEqual(gate.host, "gate.example.invalid")
        XCTAssertEqual(gate.port, 2200); XCTAssertEqual(gate.user, "jump-user")
        XCTAssertEqual(target.credentialIDs, gate.credentialIDs)
        XCTAssertEqual(target.jumpRoutes, try destination("target", in: exported.workspace).jumpRoutes)
        let support = WorkspaceDocument.RouteSupport(routeModes: ["direct_ssh"], authenticationMethods: [.publicKey],
            credentialAlgorithms: ["ssh-ed25519"], hostKeyAlgorithms: ["ssh-ed25519"], maximumJumpHops: 3)
        let readiness = try await reopened.routeReadiness(workspaceID: receipt.workspace.id, support: support)
        XCTAssertTrue(try XCTUnwrap(readiness.first { $0.routeID == target.id }).isLocallyReady)
        XCTAssertTrue(try XCTUnwrap(saved.first).connectLater, "Local readiness never bypasses offline restore.")
    }

    func testProxyCommandOverrideIsBoundToTransitContextAndNeverExecuted() throws {
        let marker = "/tmp/frankenterm-proxy-must-not-run-" + UUID().uuidString
        let result = try exported("""
        IdentityFile none
        Host target
          ProxyJump outer,inner
        Host direct-through-inner
          ProxyJump inner
        Host outer
        Host inner
          ProxyCommand sh -c "echo PRIVATE_COMMAND_TEXT > \(marker)"
        """, aliases: ["target", "direct-through-inner", "inner"])
        let target = try destination("target", in: result.workspace)
        let viaOuter = try XCTUnwrap(hops(target, in: result.workspace).last)
        let ordinaryInner = try destination("inner", in: result.workspace)
        let direct = try destination("direct-through-inner", in: result.workspace)
        let firstHop = try XCTUnwrap(hops(direct, in: result.workspace).first)
        XCTAssertNotEqual(viaOuter.id, ordinaryInner.id)
        XCTAssertNotEqual(viaOuter.id, firstHop.id)
        XCTAssertEqual(viaOuter.trustID, firstHop.trustID)
        XCTAssertEqual(viaOuter.trustID, ordinaryInner.trustID)
        XCTAssertTrue(viaOuter.requiredCapabilities.isEmpty)
        XCTAssertFalse(firstHop.requiredCapabilities.isEmpty)
        XCTAssertFalse(ordinaryInner.requiredCapabilities.isEmpty)
        let selection = try WorkspaceExportSelection(workspace: result.workspace, groupIDs: [result.workspace.initialGroupID], findings: result.portability)
        let review = try selection.review(decisions: [:], loadedFingerprints: [])
        XCTAssertFalse(review.blockedRouteIDs.contains(target.id))
        XCTAssertTrue(review.blockedRouteIDs.contains(direct.id))
        XCTAssertTrue(review.blockedRouteIDs.contains(ordinaryInner.id))
        XCTAssertFalse(FileManager.default.fileExists(atPath: marker))
        XCTAssertFalse(String(decoding: result.workspace.raw, as: UTF8.self).contains("PRIVATE_COMMAND_TEXT"))
        XCTAssertFalse(result.portability.contains { $0.reason.contains(marker) || $0.reason.contains("PRIVATE_COMMAND_TEXT") })
    }

    func testHopRestrictionsAndRevocationsSurviveProjectionAndBothExportFormats() throws {
        let known = try hashedLine("gate.example.invalid", seed: 8)
            + hashedLine("gate.example.invalid", seed: 8, marker: "@revoked") + hashedLine("target", seed: 16)
        let result = try exported("""
        IdentityFile none
        Host target
          ProxyJump gate
        Host gate
          HostName gate.example.invalid
          Ciphers aes256-gcm@openssh.com
          RevokedHostKeys /unread/required-revocations
        """, aliases: ["target"], known: Data(known.utf8))
        let route = try destination("target", in: result.workspace)
        let gate = try XCTUnwrap(hops(route, in: result.workspace).first)
        let trust = try XCTUnwrap(result.workspace.hostTrust.first { $0.id == gate.trustID })
        XCTAssertEqual(trust.pins, trust.revokedKeys)
        XCTAssertEqual(trust.evaluate(try XCTUnwrap(trust.pins.first), locallyAccepted: true), .revoked)
        for setting in ["ciphers", "revokedhostkeys"] {
            XCTAssertTrue(result.portability.contains { $0.entityID == gate.id && $0.setting == "ssh_config." + setting && $0.disposition == 1 })
        }
        let selection = try WorkspaceExportSelection(workspace: result.workspace, groupIDs: [result.workspace.initialGroupID], findings: result.portability)
        XCTAssertEqual(Set(try selection.review(decisions: [:], loadedFingerprints: []).blockedRouteIDs), Set([route.id, gate.id]))
        let archive = try selection.archive(including: [], using: crypto)
        let sealed = try FTAG.seal(archive, using: crypto)
        let restored = try FTAG.open(sealed.envelope, code: sealed.code, using: crypto)
        XCTAssertEqual(restored.workspace.hostTrust.first { $0.id == gate.trustID }?.revokedKeys, trust.revokedKeys)
        XCTAssertTrue(restored.portability.contains { $0.entityID == gate.id && $0.disposition == 1 })
        let publicDocument = try selection.publicDocument(bundleID: "proxyjump-public")
        for id in [route.id, gate.id] {
            XCTAssertTrue(try XCTUnwrap(publicDocument.routes.first { $0.id == id }).requiredCapabilities.contains("source.unresolved_required_policy"))
        }
        XCTAssertEqual(publicDocument.machines.map(\.name), ["target"])
    }

    func testProxyJumpCyclesRepeatedAliasesAndExpandedDepthNeverPublishAUsablePrefix() throws {
        let invalid = [
            "ProxyJump target\n",
            "ProxyJump other-user@target:2200\n",
            "ProxyJump a\nHost a\nProxyJump target\n",
            "ProxyJump user@a:2200\nHost a\nProxyJump other@target:2201\n",
            "ProxyJump a,a\nHost a\n",
            "ProxyJump first@a:2200,second@a:2201\nHost a\n",
            "ProxyJump [2001:db8::1]:2200,[2001:0db8::1]:2201\n",
            "ProxyJump a,b\nHost a\nProxyJump b\nHost b\n",
            "ProxyJump a,b,c,d\nHost a b c d\n",
            "ProxyJump user@a:1,user@b:2,user@c:3,user@d:4\n",
            "ProxyJump a,b,c\nHost a\nProxyJump outer\nHost b c outer\n"
        ]
        for suffix in invalid {
            let input = try source("Host target\nIdentityFile /selected/target\n" + suffix)
            XCTAssertEqual(try input.identityFileReferences(for: ["target"]), ["/selected/target"])
            var registry = SSHWorkspaceRegistry()
            let result = try input.export(aliases: ["target"], workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)
            XCTAssertEqual(result.workspace.routes.count, 1, suffix)
            XCTAssertTrue(result.workspace.routes[0].jumpRoutes.isEmpty, suffix)
            XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty, suffix)
            XCTAssertTrue(result.portability.contains { $0.setting == "ssh_config.proxyjump" && $0.disposition == 1 }, suffix)
        }
    }

    func testUnsupportedProxyJumpFormsRemainExplicitBlockersAndNonePreservesPrecedence() throws {
        for value in ["ssh://hop", "%h", "${GATEWAY}", "@hop", "user@", "hop:0", "hop:65536", "hop:-1", "hop:ssh",
                      "hop:22:33", "[2001:db8::1", "[2001:db8::1]suffix", "[2001:db8::1]:0", "[not-ipv6]:22",
                      "2001:db8::1", "hop,,other", "\"hop, other\"", "none,hop", "-option", "user;command@hop"] {
            let result = try exported("Host target\nIdentityFile none\nProxyJump " + value + "\nHost hop other\nIdentityFile none\n", aliases: ["target"])
            XCTAssertEqual(result.workspace.routes.count, 1, value)
            XCTAssertTrue(result.workspace.routes[0].jumpRoutes.isEmpty, value)
            XCTAssertFalse(result.workspace.routes[0].requiredCapabilities.isEmpty, value)
        }
        let result = try exported("""
        IdentityFile none
        Host disabled
          ProxyJump NONE
          ProxyCommand must-not-execute
        Host command-disabled
          ProxyCommand none
          ProxyJump unknown
        """)
        XCTAssertTrue(result.workspace.routes.allSatisfy { $0.jumpRoutes.isEmpty && $0.requiredCapabilities.isEmpty })
    }

    func testTransitIdentitiesStayStableWhenAHopBecomesASelectedMachine() throws {
        let input = try source("IdentityFile none\nHost target\nProxyJump outer,inner\nHost outer inner\n")
        var registry = SSHWorkspaceRegistry()
        let first = try input.export(aliases: ["target"], workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)
        registry = try JSONDecoder().decode(SSHWorkspaceRegistry.self, from: JSONEncoder().encode(registry))
        let second = try input.export(aliases: ["inner", "target"], workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)
        let a = try destination("target", in: first.workspace), b = try destination("target", in: second.workspace)
        XCTAssertEqual(a.id, b.id)
        XCTAssertEqual(a.jumpRoutes, b.jumpRoutes)
        XCTAssertEqual(try hops(a, in: first.workspace).map(\.trustID), try hops(b, in: second.workspace).map(\.trustID))
        let inner = try destination("inner", in: second.workspace)
        XCTAssertFalse(b.jumpRoutes.contains(inner.id))
        XCTAssertEqual(inner.sessionCreation, "on_select")
        XCTAssertEqual(second.workspace.hostTrust.count, first.workspace.hostTrust.count)
        XCTAssertEqual(second.workspace.machines.map(\.name), ["inner", "target"])
    }

    func testExpandedDependencyAdmissionCannotPublishAnOversizedPartialWorkspace() throws {
        var text = "IdentityFile none\n"
        let selected = (0..<128).map { "target-\($0)" }
        for name in selected {
            text += "Host " + name + "\nProxyJump " + name + "-a," + name + "-b\nHost " + name + "-a " + name + "-b\n"
        }
        let input = try source(text)
        XCTAssertThrowsError(try input.identityFileReferences(for: selected)) { error in
            XCTAssertEqual(error as? FTError, .limit("ssh.routes"))
        }
        var registry = SSHWorkspaceRegistry()
        let encoder = JSONEncoder(); encoder.outputFormatting = [.sortedKeys]
        let original = try encoder.encode(registry)
        XCTAssertThrowsError(try input.export(aliases: selected, workspaceName: "Fleet", defaultUser: "operator", registry: &registry, using: crypto)) { error in
            XCTAssertEqual(error as? FTError, .limit("ssh.routes"))
        }
        XCTAssertEqual(try encoder.encode(registry), original)
    }

    func testSourceAdmissionBoundsMalformedSyntaxAndSelection() throws {
        XCTAssertThrowsError(try SSHWorkspaceSource(configuration: Data(repeating: 65, count: SSHWorkspaceSource.maximumSourceBytes + 1)))
        XCTAssertThrowsError(try source(String(repeating: "\n", count: SSHWorkspaceSource.maximumLines)))
        XCTAssertThrowsError(try source("Host \"unterminated\n"))
        XCTAssertThrowsError(try source("Host build\nHostName a\0b\n"))
        XCTAssertThrowsError(try source("Host " + String(repeating: "x", count: 255)))
        let tooMany = (0...SSHWorkspaceSource.maximumAliases).map { "Host alias-\($0)" }.joined(separator: "\n")
        XCTAssertThrowsError(try source(tooMany))
        XCTAssertThrowsError(try exported("Host *\nUser operator\n"))
        XCTAssertThrowsError(try exported("Host build\n", aliases: ["not-listed"]))
        XCTAssertThrowsError(try exported("Host build\n", known: Data(repeating: 65, count: SSHWorkspaceSource.maximumSourceBytes + 1)))
    }
}
